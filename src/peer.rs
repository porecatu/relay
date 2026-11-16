// SPDX-License-Identifier: GPL-3.0-or-later

//! Who the peer is: the address used for logs and for rate limits.
//!
//! The address is the socket's, unless the socket belongs to a trusted proxy
//! (`[server] trusted_proxies`), in which case it comes from the
//! `X-Forwarded-For` header (PRD-001 RF-1.13). The same resolved address feeds
//! the logs and the per-IP limits; only the logs go through [`ip_for_log`].

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use ipnet::{IpNet, Ipv4Net, Ipv6Net};

/// Prefix length of the IPv6 group used as the key of per-IP limits.
const LIMIT_V6_PREFIX: u8 = 64;
/// Prefix length kept when an IPv4 address is truncated for the logs.
const LOG_V4_PREFIX: u8 = 24;
/// Prefix length kept when an IPv6 address is truncated for the logs.
const LOG_V6_PREFIX: u8 = 48;

/// How an IP address may appear in the logs (`[log] ip_addresses`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IpLogPolicy {
    /// The whole address.
    Full,
    /// The network only: `/24` for IPv4, `/48` for IPv6.
    #[default]
    Truncated,
    /// No address at all.
    None,
}

/// Error of parsing an [`IpLogPolicy`] from text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseIpLogPolicyError(String);

impl fmt::Display for ParseIpLogPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown IP log policy {:?}: expected \"full\", \"truncated\" or \"none\"",
            self.0
        )
    }
}

impl std::error::Error for ParseIpLogPolicyError {}

impl FromStr for IpLogPolicy {
    type Err = ParseIpLogPolicyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "full" => Ok(Self::Full),
            "truncated" => Ok(Self::Truncated),
            "none" => Ok(Self::None),
            other => Err(ParseIpLogPolicyError(other.to_owned())),
        }
    }
}

/// Resolves the address of the peer behind `socket`.
///
/// - A socket that is not in `trusted` is the peer; `xff` is ignored, because
///   anybody can send that header.
/// - A socket in `trusted` is a proxy: the peer is the rightmost entry of
///   `xff` that is not in `trusted` (each proxy appends the address it saw on
///   the right, so what lies to the left of it is not vouched for by anyone
///   we trust).
/// - A missing or malformed header (an empty entry, or one that is not a bare
///   IP address) resolves to the socket.
/// - A chain made only of trusted addresses also resolves to the socket. The
///   PRD does not say what to do then; the socket is the choice that cannot be
///   forged and cannot be used to dodge a per-IP limit by rotating entries.
///
/// IPv4-mapped IPv6 addresses (what a dual-stack listener reports) are
/// reduced to plain IPv4 before any comparison.
#[must_use]
pub fn resolve_peer(socket: SocketAddr, xff: Option<&str>, trusted: &[IpNet]) -> IpAddr {
    let socket_ip = socket.ip().to_canonical();
    if !is_trusted(socket_ip, trusted) {
        return socket_ip;
    }
    let Some(entries) = xff.and_then(parse_xff) else {
        return socket_ip;
    };
    entries
        .into_iter()
        .rev()
        .find(|ip| !is_trusted(*ip, trusted))
        .unwrap_or(socket_ip)
}

/// Parses a whole `X-Forwarded-For` value; `None` if any entry is malformed.
fn parse_xff(header: &str) -> Option<Vec<IpAddr>> {
    header
        .split(',')
        .map(|entry| {
            entry
                .trim()
                .parse::<IpAddr>()
                .ok()
                .map(|ip| ip.to_canonical())
        })
        .collect()
}

fn is_trusted(ip: IpAddr, trusted: &[IpNet]) -> bool {
    trusted.iter().any(|net| net.contains(&ip))
}

/// The address as it may appear in a log line under `policy`, or `None` when
/// the policy is to log no address.
///
/// Truncated, an IPv4 address becomes its `/24` network (`203.0.113.0/24`) and
/// an IPv6 one its `/48` (`2001:db8:1::/48`).
#[must_use]
pub fn ip_for_log(ip: IpAddr, policy: IpLogPolicy) -> Option<String> {
    let ip = ip.to_canonical();
    match policy {
        IpLogPolicy::Full => Some(ip.to_string()),
        IpLogPolicy::None => None,
        IpLogPolicy::Truncated => Some(match ip {
            IpAddr::V4(v4) => Ipv4Net::new(v4, LOG_V4_PREFIX)
                .expect("a /24 prefix is valid")
                .trunc()
                .to_string(),
            IpAddr::V6(v6) => Ipv6Net::new(v6, LOG_V6_PREFIX)
                .expect("a /48 prefix is valid")
                .trunc()
                .to_string(),
        }),
    }
}

/// The key under which per-IP limits count a peer: the address itself for
/// IPv4, the `/64` network for IPv6 (one subscriber usually owns a whole
/// `/64`, so counting single addresses would make the limit meaningless).
#[must_use]
pub fn limit_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(
            Ipv6Net::new(v6, LIMIT_V6_PREFIX)
                .expect("a /64 prefix is valid")
                .trunc()
                .addr(),
        ),
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> IpNet {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn sock(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn without_trusted_proxy_the_header_is_ignored() {
        let got = resolve_peer(sock("198.51.100.7:5000"), Some("203.0.113.9"), &[]);
        assert_eq!(got, ip("198.51.100.7"));
    }

    #[test]
    fn untrusted_socket_ignores_the_header_even_if_proxies_are_configured() {
        let trusted = [net("10.0.0.0/8")];
        let got = resolve_peer(sock("198.51.100.7:5000"), Some("203.0.113.9"), &trusted);
        assert_eq!(got, ip("198.51.100.7"));
    }

    #[test]
    fn trusted_socket_takes_the_rightmost_untrusted_entry() {
        let trusted = [net("10.0.0.0/8")];
        let got = resolve_peer(
            sock("10.0.0.1:443"),
            Some("1.2.3.4, 203.0.113.9, 10.0.0.5"),
            &trusted,
        );
        assert_eq!(got, ip("203.0.113.9"));
    }

    #[test]
    fn forged_leading_entries_do_not_win() {
        let trusted = [net("10.0.0.0/8")];
        // The client forged "192.0.2.1"; the proxy appended the real peer.
        let got = resolve_peer(
            sock("10.0.0.1:443"),
            Some("192.0.2.1, 203.0.113.9"),
            &trusted,
        );
        assert_eq!(got, ip("203.0.113.9"));
    }

    #[test]
    fn trusted_socket_without_header_is_the_socket() {
        let trusted = [net("10.0.0.0/8")];
        assert_eq!(
            resolve_peer(sock("10.0.0.1:443"), None, &trusted),
            ip("10.0.0.1")
        );
    }

    #[test]
    fn chain_made_only_of_trusted_addresses_is_the_socket() {
        let trusted = [net("10.0.0.0/8")];
        let got = resolve_peer(sock("10.0.0.1:443"), Some("10.0.0.7, 10.0.0.5"), &trusted);
        assert_eq!(got, ip("10.0.0.1"));
    }

    #[test]
    fn garbage_header_is_the_socket() {
        let trusted = [net("10.0.0.0/8")];
        for header in [
            "",
            "garbage",
            "203.0.113.9, garbage",
            "203.0.113.9,,10.0.0.5",
            "203.0.113.9:8080",
            "[2001:db8::1]:443",
            "999.1.1.1",
        ] {
            let got = resolve_peer(sock("10.0.0.1:443"), Some(header), &trusted);
            assert_eq!(got, ip("10.0.0.1"), "header {header:?}");
        }
    }

    #[test]
    fn ipv6_entries_and_trusted_networks_work() {
        let trusted = [net("10.0.0.0/8"), net("fd00::/8")];
        let got = resolve_peer(
            sock("[fd00::1]:443"),
            Some("2001:db8::7, fd00::2"),
            &trusted,
        );
        assert_eq!(got, ip("2001:db8::7"));
    }

    #[test]
    fn ipv4_mapped_addresses_are_reduced_to_ipv4() {
        let trusted = [net("10.0.0.0/8")];
        let got = resolve_peer(
            sock("[::ffff:10.0.0.1]:443"),
            Some("::ffff:203.0.113.9"),
            &trusted,
        );
        assert_eq!(got, ip("203.0.113.9"));
        assert_eq!(
            resolve_peer(sock("[::ffff:198.51.100.7]:1"), None, &[]),
            ip("198.51.100.7")
        );
    }

    #[test]
    fn policy_parses_from_the_config_names() {
        assert_eq!("full".parse(), Ok(IpLogPolicy::Full));
        assert_eq!("truncated".parse(), Ok(IpLogPolicy::Truncated));
        assert_eq!("none".parse(), Ok(IpLogPolicy::None));
        let err = "FULL".parse::<IpLogPolicy>().unwrap_err();
        assert!(err.to_string().contains("\"FULL\""));
        assert_eq!(IpLogPolicy::default(), IpLogPolicy::Truncated);
    }

    #[test]
    fn log_policy_full() {
        assert_eq!(
            ip_for_log(ip("203.0.113.77"), IpLogPolicy::Full).as_deref(),
            Some("203.0.113.77")
        );
        assert_eq!(
            ip_for_log(ip("2001:db8:1:2:3:4:5:6"), IpLogPolicy::Full).as_deref(),
            Some("2001:db8:1:2:3:4:5:6")
        );
    }

    #[test]
    fn log_policy_truncated() {
        assert_eq!(
            ip_for_log(ip("203.0.113.77"), IpLogPolicy::Truncated).as_deref(),
            Some("203.0.113.0/24")
        );
        assert_eq!(
            ip_for_log(ip("2001:db8:1:2:3:4:5:6"), IpLogPolicy::Truncated).as_deref(),
            Some("2001:db8:1::/48")
        );
        assert_eq!(
            ip_for_log(ip("::ffff:203.0.113.77"), IpLogPolicy::Truncated).as_deref(),
            Some("203.0.113.0/24")
        );
    }

    #[test]
    fn log_policy_none() {
        assert_eq!(ip_for_log(ip("203.0.113.77"), IpLogPolicy::None), None);
        assert_eq!(ip_for_log(ip("2001:db8::1"), IpLogPolicy::None), None);
    }

    #[test]
    fn limit_key_groups_ipv6_by_64_and_keeps_ipv4() {
        assert_eq!(limit_key(ip("203.0.113.77")), ip("203.0.113.77"));
        assert_eq!(
            limit_key(ip("2001:db8:1:2:aaaa:bbbb:cccc:dddd")),
            ip("2001:db8:1:2::")
        );
        assert_eq!(
            limit_key(ip("2001:db8:1:2:1::1")),
            limit_key(ip("2001:db8:1:2:2::2"))
        );
        assert_ne!(
            limit_key(ip("2001:db8:1:2::1")),
            limit_key(ip("2001:db8:1:3::1"))
        );
    }
}
