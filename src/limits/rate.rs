// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-IP rate limiter.
//!
//! One limiter serves one use (client connections, host registration
//! attempts, presence queries); the relay keeps one instance per use. The
//! current [`Instant`] always comes from the caller.
//!
//! # Algorithm
//!
//! GCRA (generic cell rate algorithm), chosen over a sliding window of
//! timestamps because it keeps one [`Instant`] per key: the memory of an entry
//! does not grow with the configured limit. For a limit of `N` per minute the
//! emission interval is `60 s / N` and the burst is `N`: a quiet IP may open
//! `N` connections at once, the `N + 1`-th is refused, and one more is allowed
//! every `60 s / N` after that. Sustained, that is `N` per minute; a burst
//! right after a quiet minute can reach just under `2 N` in one minute, the
//! usual trade of the algorithm.
//!
//! # Keys
//!
//! An IPv4 address is the key as it is. An IPv6 address is keyed by its /64
//! prefix, so a client that owns a whole /64 cannot dodge the limit by rotating
//! the interface identifier. An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`)
//! counts as the IPv4 address.
//!
//! # Memory
//!
//! The map never holds more than `max_entries` keys. A new key that finds it
//! full evicts the entry that was updated longest ago, so a flood of
//! spoofed or pulverized addresses costs bounded memory (at the price of
//! forgetting the oldest keys).

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

/// Length of the window the limit is expressed in.
const WINDOW: Duration = Duration::from_secs(60);

/// What a rate-limit entry is keyed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    V4(Ipv4Addr),
    /// The upper 64 bits of an IPv6 address.
    V6Prefix64(u64),
}

impl Key {
    fn of(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V4(v4) => Self::V4(v4),
            IpAddr::V6(v6) => {
                let [a, b, c, d, ..] = v6.segments();
                Self::V6Prefix64(
                    (u64::from(a) << 48)
                        | (u64::from(b) << 32)
                        | (u64::from(c) << 16)
                        | u64::from(d),
                )
            }
        }
    }
}

#[derive(Debug)]
struct Entry {
    /// Theoretical arrival time of the next request, per GCRA.
    tat: Instant,
    /// Position in the recency order; the smallest one is evicted first.
    seq: u64,
}

/// Rate limiter keyed by client IP (IPv6 grouped by /64).
#[derive(Debug)]
pub struct RateLimiter {
    /// Requests allowed per minute; `0` refuses everything.
    limit_per_minute: u32,
    /// `WINDOW / limit_per_minute`, zero when the limit is zero.
    interval: Duration,
    max_entries: usize,
    entries: HashMap<Key, Entry>,
    /// Keys by `seq`, oldest first.
    by_age: BTreeMap<u64, Key>,
    next_seq: u64,
}

impl RateLimiter {
    /// Creates a limiter that allows `limit_per_minute` requests per minute
    /// per key and remembers at most `max_entries` keys.
    ///
    /// A `limit_per_minute` of zero refuses every request. A `max_entries` of
    /// zero is raised to one, so the limiter still works.
    #[must_use]
    pub fn new(limit_per_minute: u32, max_entries: usize) -> Self {
        let interval = WINDOW
            .checked_div(limit_per_minute)
            .unwrap_or(Duration::ZERO);
        Self {
            limit_per_minute,
            interval,
            max_entries: max_entries.max(1),
            entries: HashMap::new(),
            by_age: BTreeMap::new(),
            next_seq: 0,
        }
    }

    /// Number of keys currently remembered; never above `max_entries`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no key is currently remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Records one request from `ip` at `now` and tells whether it is within
    /// the limit.
    ///
    /// A refused request changes nothing: hammering a limited address does not
    /// push its release further away.
    pub fn check(&mut self, ip: IpAddr, now: Instant) -> bool {
        if self.limit_per_minute == 0 {
            return false;
        }
        let key = Key::of(ip);
        let earliest = self.entries.get(&key).map_or(now, |e| e.tat.max(now));
        let new_tat = earliest + self.interval;
        if new_tat.duration_since(now) > WINDOW {
            return false;
        }
        self.store(key, new_tat);
        true
    }

    /// Writes the new theoretical arrival time of `key`, making it the most
    /// recently updated entry and evicting the oldest one if the map is full.
    fn store(&mut self, key: Key, tat: Instant) {
        let seq = self.next_seq;
        self.next_seq += 1;
        if let Some(entry) = self.entries.get_mut(&key) {
            self.by_age.remove(&entry.seq);
            entry.tat = tat;
            entry.seq = seq;
        } else {
            if self.entries.len() >= self.max_entries
                && let Some((_, oldest)) = self.by_age.pop_first()
            {
                self.entries.remove(&oldest);
            }
            self.entries.insert(key, Entry { tat, seq });
        }
        self.by_age.insert(seq, key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    fn v4(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, last))
    }

    fn v6(addr: &str) -> IpAddr {
        IpAddr::V6(addr.parse::<Ipv6Addr>().unwrap())
    }

    #[test]
    fn the_n_plus_one_th_in_the_same_minute_is_refused() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(5, 100);
        for i in 0..5 {
            assert!(limiter.check(v4(1), t0), "request {i} is within the limit");
        }
        assert!(!limiter.check(v4(1), t0));
        assert!(!limiter.check(v4(1), t0 + Duration::from_secs(5)));
    }

    #[test]
    fn releases_with_time() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(6, 100);
        for _ in 0..6 {
            assert!(limiter.check(v4(1), t0));
        }
        assert!(!limiter.check(v4(1), t0));
        // One slot every 10 s.
        assert!(!limiter.check(v4(1), t0 + Duration::from_secs(9)));
        assert!(limiter.check(v4(1), t0 + Duration::from_secs(10)));
        assert!(!limiter.check(v4(1), t0 + Duration::from_secs(10)));
        // A whole minute later the full burst is back.
        let later = t0 + Duration::from_secs(70);
        for i in 0..6 {
            assert!(limiter.check(v4(1), later), "request {i} after the minute");
        }
        assert!(!limiter.check(v4(1), later));
    }

    #[test]
    fn a_refusal_does_not_push_the_release_away() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(6, 100);
        for _ in 0..6 {
            assert!(limiter.check(v4(1), t0));
        }
        for s in 0..10 {
            assert!(!limiter.check(v4(1), t0 + Duration::from_millis(s * 900)));
        }
        assert!(limiter.check(v4(1), t0 + Duration::from_secs(10)));
    }

    #[test]
    fn different_ipv4_addresses_do_not_share_a_limit() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(1, 100);
        assert!(limiter.check(v4(1), t0));
        assert!(!limiter.check(v4(1), t0));
        assert!(limiter.check(v4(2), t0));
    }

    #[test]
    fn ipv6_in_the_same_64_shares_the_limit() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(2, 100);
        assert!(limiter.check(v6("2001:db8:1:2::1"), t0));
        assert!(limiter.check(v6("2001:db8:1:2:ffff:ffff:ffff:ffff"), t0));
        assert!(!limiter.check(v6("2001:db8:1:2:abcd::9"), t0));
        assert_eq!(limiter.len(), 1);
    }

    #[test]
    fn ipv6_in_different_64s_does_not_share_the_limit() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(1, 100);
        assert!(limiter.check(v6("2001:db8:1:2::1"), t0));
        assert!(!limiter.check(v6("2001:db8:1:2::2"), t0));
        assert!(limiter.check(v6("2001:db8:1:3::1"), t0));
        assert!(limiter.check(v6("2001:db8:2:2::1"), t0));
        assert_eq!(limiter.len(), 3);
    }

    #[test]
    fn ipv4_mapped_ipv6_counts_as_the_ipv4_address() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(1, 100);
        assert!(limiter.check(v4(7), t0));
        assert!(!limiter.check(v6("::ffff:203.0.113.7"), t0));
    }

    #[test]
    fn the_map_never_exceeds_max_entries() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(10, 8);
        for i in 0..1000u32 {
            let ip = IpAddr::V4(Ipv4Addr::from(i));
            assert!(limiter.check(ip, t0));
            assert!(limiter.len() <= 8);
        }
        assert_eq!(limiter.len(), 8);
        assert_eq!(limiter.by_age.len(), 8, "age index stays in step");
        // Repeated hits on one key do not grow the age index either.
        for _ in 0..5 {
            limiter.check(IpAddr::V4(Ipv4Addr::from(999u32)), t0);
        }
        assert_eq!(limiter.by_age.len(), 8);
    }

    #[test]
    fn the_oldest_entry_is_evicted_first() {
        let t0 = Instant::now();
        let mut limiter = RateLimiter::new(1, 2);
        assert!(limiter.check(v4(1), t0));
        assert!(limiter.check(v4(2), t0));
        // Touching key 1 again is refused, so it stays the oldest update.
        assert!(!limiter.check(v4(1), t0));
        // A third key evicts key 1 (the oldest update); key 2 survives.
        assert!(limiter.check(v4(3), t0));
        assert!(
            !limiter.check(v4(2), t0),
            "key 2 was kept and is still limited"
        );
        assert!(
            limiter.check(v4(1), t0),
            "key 1 was forgotten, so it starts fresh"
        );
    }

    #[test]
    fn zero_limit_refuses_everything_and_zero_entries_still_works() {
        let t0 = Instant::now();
        let mut none = RateLimiter::new(0, 10);
        assert!(!none.check(v4(1), t0));
        assert!(none.is_empty());

        let mut tiny = RateLimiter::new(1, 0);
        assert!(tiny.check(v4(1), t0));
        assert!(!tiny.check(v4(1), t0));
        assert_eq!(tiny.len(), 1);
    }
}
