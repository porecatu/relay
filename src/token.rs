// SPDX-License-Identifier: GPL-3.0-or-later

//! Operator tokens (ADR-0003).
//!
//! Only the BLAKE2s-256 digest of each configured token is kept; the clear
//! value leaves memory as soon as the [`TokenSet`] is built. A presented token
//! is hashed and compared with **every** configured digest in constant time,
//! accumulating the result without short-circuiting, so neither the token
//! length nor the position of the match leaks through timing.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use blake2::{Blake2s256, Digest};
use subtle::{Choice, ConditionallySelectable, ConstantTimeEq};

use crate::config::TokenDigest;

/// Number of random bytes in a generated token (43 base64url characters).
const GENERATED_TOKEN_BYTES: usize = 32;

/// Length of a BLAKE2s-256 digest.
const DIGEST_LEN: usize = 32;

type Digest32 = [u8; DIGEST_LEN];

fn digest_of(value: &str) -> Digest32 {
    Blake2s256::digest(value.as_bytes()).into()
}

/// The operator tokens the relay accepts: a name and the digest of each.
pub struct TokenSet {
    entries: Vec<(String, Digest32)>,
}

impl TokenSet {
    /// Builds the set from `(name, value)` pairs, keeping only the digest of
    /// each value.
    pub fn new<N, V>(tokens: impl IntoIterator<Item = (N, V)>) -> Self
    where
        N: Into<String>,
        V: AsRef<str>,
    {
        let entries = tokens
            .into_iter()
            .map(|(name, value)| (name.into(), digest_of(value.as_ref())))
            .collect();
        Self { entries }
    }

    /// Builds the set from the digests the configuration holds (the clear
    /// value never reached this process' state past loading). The digests are
    /// copied in and never handed out again.
    #[must_use]
    pub fn from_digests(digests: &[TokenDigest]) -> Self {
        let entries = digests
            .iter()
            .map(|token| (token.name.clone(), *token.digest()))
            .collect();
        Self { entries }
    }

    /// Number of configured tokens.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no token is configured (nothing can authenticate).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the name of the token matching `presented`, if any.
    ///
    /// The digest of `presented` is compared with every configured digest by
    /// [`ConstantTimeEq`], with no early exit and no branch on a comparison
    /// result: the index of the first hit is accumulated by a constant-time
    /// select and only looked up after the loop.
    pub fn check(&self, presented: &str) -> Option<&str> {
        let presented = digest_of(presented);
        let mut found = Choice::from(0);
        // Index of the first matching entry; meaningful only when `found`.
        let mut index = 0u64;
        for (i, (_, digest)) in self.entries.iter().enumerate() {
            let hit = presented.ct_eq(digest);
            index.conditional_assign(&(i as u64), hit & !found);
            found |= hit;
        }
        if bool::from(found) {
            self.entries
                .get(index as usize)
                .map(|(name, _)| name.as_str())
        } else {
            None
        }
    }
}

impl fmt::Debug for TokenSet {
    /// Shows only the names: never a digest, never a value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSet")
            .field(
                "names",
                &self
                    .entries
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Extracts the token from an `Authorization` header value.
///
/// Accepts the `Bearer` scheme (case-insensitive), exactly one space and a
/// non-empty token; anything else is `None`.
pub fn parse_bearer(header_value: &str) -> Option<&str> {
    const SCHEME: &str = "bearer";
    let bytes = header_value.as_bytes();
    if bytes.len() <= SCHEME.len() + 1 {
        return None;
    }
    if !bytes[..SCHEME.len()].eq_ignore_ascii_case(SCHEME.as_bytes()) {
        return None;
    }
    if bytes[SCHEME.len()] != b' ' {
        return None;
    }
    // The first SCHEME.len() + 1 bytes are ASCII, so this is a char boundary.
    let token = &header_value[SCHEME.len() + 1..];
    if token.is_empty() || token.starts_with(' ') {
        return None;
    }
    Some(token)
}

/// Generates a new operator token: 32 bytes from the operating system's
/// random source, encoded as base64url without padding (43 characters).
///
/// # Panics
///
/// Panics if the operating system cannot provide randomness.
pub fn generate_token() -> String {
    let mut bytes = [0u8; GENERATED_TOKEN_BYTES];
    getrandom::fill(&mut bytes).expect("the operating system random source is unavailable");
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> TokenSet {
        TokenSet::new([
            ("alpha", "a".repeat(32)),
            ("beta", "b".repeat(64)),
            ("gamma", "c".repeat(128)),
        ])
    }

    #[test]
    fn matching_token_returns_its_name() {
        let set = set();
        assert_eq!(set.check(&"a".repeat(32)), Some("alpha"));
        assert_eq!(set.check(&"b".repeat(64)), Some("beta"));
        assert_eq!(set.check(&"c".repeat(128)), Some("gamma"));
    }

    #[test]
    fn duplicate_values_return_the_first_name() {
        let set = TokenSet::new([("first", "x".repeat(40)), ("second", "x".repeat(40))]);
        assert_eq!(set.check(&"x".repeat(40)), Some("first"));
    }

    #[test]
    fn wrong_token_returns_none() {
        let set = set();
        assert_eq!(set.check(&"d".repeat(32)), None);
        assert_eq!(set.check(""), None);
    }

    #[test]
    fn length_does_not_change_the_result() {
        let set = set();
        // Prefixes and extensions of a valid token must not match.
        assert_eq!(set.check(&"a".repeat(31)), None);
        assert_eq!(set.check(&"a".repeat(33)), None);
        assert_eq!(set.check(&"b".repeat(63)), None);
        assert_eq!(set.check(&"c".repeat(129)), None);
        assert_eq!(set.check(&"c".repeat(1000)), None);
    }

    #[test]
    fn empty_set_matches_nothing() {
        let set = TokenSet::new(Vec::<(String, String)>::new());
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert_eq!(set.check("anything"), None);
    }

    #[test]
    fn debug_shows_only_names() {
        let secret = "s3cr3t-value-that-must-not-leak-0123456789";
        let set = TokenSet::new([("ops", secret)]);
        let shown = format!("{set:?}");
        assert!(shown.contains("ops"));
        assert!(!shown.contains(secret));
        assert!(!shown.contains(&format!("{:?}", digest_of(secret))));
    }

    #[test]
    fn digests_from_the_configuration_authenticate_like_values() {
        let digests = [
            TokenDigest::new("alpha", &"a".repeat(32)),
            TokenDigest::new("beta", &"b".repeat(64)),
        ];
        let set = TokenSet::from_digests(&digests);
        assert_eq!(set.len(), 2);
        assert_eq!(set.check(&"a".repeat(32)), Some("alpha"));
        assert_eq!(set.check(&"b".repeat(64)), Some("beta"));
        assert_eq!(set.check(&"c".repeat(32)), None);
        assert!(!format!("{set:?}").contains(&format!("{:?}", digest_of(&"a".repeat(32)))));
    }

    #[test]
    fn generated_token_has_43_base64url_characters() {
        let token = generate_token();
        assert_eq!(token.len(), 43);
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn generated_tokens_differ() {
        assert_ne!(generate_token(), generate_token());
    }

    #[test]
    fn generated_token_authenticates() {
        let token = generate_token();
        let set = TokenSet::new([("fresh", token.as_str())]);
        assert_eq!(set.check(&token), Some("fresh"));
    }

    #[test]
    fn bearer_scheme_is_parsed() {
        assert_eq!(parse_bearer("Bearer x"), Some("x"));
        assert_eq!(parse_bearer("bearer x"), Some("x"));
        assert_eq!(parse_bearer("BEARER abc.def"), Some("abc.def"));
    }

    #[test]
    fn other_schemes_and_empty_tokens_are_rejected() {
        assert_eq!(parse_bearer("Basic x"), None);
        assert_eq!(parse_bearer("Bearer"), None);
        assert_eq!(parse_bearer("Bearer "), None);
        assert_eq!(parse_bearer("Bearer  x"), None);
        assert_eq!(parse_bearer("Bearerx"), None);
        assert_eq!(parse_bearer("Bearer\tx"), None);
        assert_eq!(parse_bearer(""), None);
        assert_eq!(parse_bearer("x"), None);
    }

    #[test]
    fn non_ascii_header_is_rejected_without_panicking() {
        assert_eq!(parse_bearer("Béarer x"), None);
        assert_eq!(parse_bearer("é"), None);
    }
}
