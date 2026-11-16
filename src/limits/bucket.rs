// SPDX-License-Identifier: GPL-3.0-or-later

//! Token bucket counting payload bytes, one per channel and direction.
//!
//! The bucket is a pure value: the current [`Instant`] always comes from the
//! caller, so tests drive time by hand and the type never reads a clock.
//!
//! A frame that needs more than the bucket's capacity never fits, however long
//! the caller waits. The caller must close the channel with `limit-exceeded`
//! in that case, exactly as it does when [`TokenBucket::try_take`] refuses a
//! frame that would have fit in a full bucket: a refusal is never a reason to
//! drop the frame silently.

use std::time::Instant;

/// Nanoseconds in one second.
const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Bytes per second that one kbit/s of configured bandwidth stands for.
const BYTES_PER_SEC_PER_KBPS: u64 = 125;

/// Bytes in one KiB.
const BYTES_PER_KIB: u64 = 1024;

/// Token bucket measured in payload bytes.
///
/// The capacity is the burst; the refill rate is the sustained bandwidth. The
/// bucket starts full. Internally the tokens are kept in byte-nanoseconds, so
/// the refill is exact: no fraction of a byte is ever lost between calls.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity_bytes: u64,
    refill_bytes_per_sec: u64,
    /// Available tokens, in bytes times [`NANOS_PER_SEC`].
    scaled_tokens: u128,
    /// Instant up to which the refill has already been accounted for.
    last_refill: Instant,
}

impl TokenBucket {
    /// Creates a full bucket.
    ///
    /// `capacity_bytes` is the burst, `refill_bytes_per_sec` the sustained
    /// rate and `now` the instant the bucket starts counting from.
    #[must_use]
    pub fn new(capacity_bytes: u64, refill_bytes_per_sec: u64, now: Instant) -> Self {
        Self {
            capacity_bytes,
            refill_bytes_per_sec,
            scaled_tokens: u128::from(capacity_bytes) * NANOS_PER_SEC,
            last_refill: now,
        }
    }

    /// Creates a full bucket from the configuration units: sustained
    /// bandwidth in kbit/s (`channel_bandwidth_kbps`) and burst in KiB
    /// (`channel_burst_kib`).
    #[must_use]
    pub fn from_kbps(bandwidth_kbps: u64, burst_kib: u64, now: Instant) -> Self {
        Self::new(
            burst_kib.saturating_mul(BYTES_PER_KIB),
            bandwidth_kbps.saturating_mul(BYTES_PER_SEC_PER_KBPS),
            now,
        )
    }

    /// Burst size, in bytes.
    #[must_use]
    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    /// Sustained rate, in bytes per second.
    #[must_use]
    pub fn refill_bytes_per_sec(&self) -> u64 {
        self.refill_bytes_per_sec
    }

    /// Takes `bytes` of payload from the bucket.
    ///
    /// Returns `false`, leaving the available tokens untouched, when the
    /// bucket does not hold that many bytes. A frame larger than
    /// [`capacity_bytes`](Self::capacity_bytes) is therefore refused forever.
    /// A refusal means the caller closes the channel with `limit-exceeded`.
    pub fn try_take(&mut self, bytes: usize, now: Instant) -> bool {
        self.refill(now);
        let needed = (bytes as u128) * NANOS_PER_SEC;
        if needed > self.scaled_tokens {
            return false;
        }
        self.scaled_tokens -= needed;
        true
    }

    /// Adds the tokens earned since the last call, up to the capacity.
    ///
    /// An instant earlier than the last one seen adds nothing and does not
    /// move the clock back.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        if elapsed.is_zero() {
            return;
        }
        self.last_refill = now;
        let earned = elapsed.as_nanos() * u128::from(self.refill_bytes_per_sec);
        let ceiling = u128::from(self.capacity_bytes) * NANOS_PER_SEC;
        self.scaled_tokens = self.scaled_tokens.saturating_add(earned).min(ceiling);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn burst_up_to_capacity_then_refuses() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(1000, 100, t0);
        assert!(bucket.try_take(400, t0));
        assert!(bucket.try_take(600, t0));
        assert!(!bucket.try_take(1, t0), "empty bucket must refuse");
    }

    #[test]
    fn refusal_does_not_consume_tokens() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(1000, 100, t0);
        assert!(bucket.try_take(900, t0));
        assert!(!bucket.try_take(200, t0));
        assert!(bucket.try_take(100, t0), "the 100 left must still be there");
    }

    #[test]
    fn refills_with_time() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(1000, 100, t0);
        assert!(bucket.try_take(1000, t0));
        assert!(!bucket.try_take(100, t0 + Duration::from_millis(999)));
        assert!(bucket.try_take(100, t0 + secs(1)));
        assert!(!bucket.try_take(1, t0 + secs(1)));
        assert!(bucket.try_take(500, t0 + secs(6)));
    }

    #[test]
    fn refill_is_capped_at_capacity() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(1000, 100, t0);
        assert!(bucket.try_take(1000, t0));
        let much_later = t0 + secs(3600);
        assert!(!bucket.try_take(1001, much_later));
        assert!(bucket.try_take(1000, much_later));
        assert!(!bucket.try_take(1, much_later));
    }

    #[test]
    fn frame_larger_than_capacity_is_always_refused() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(1000, 1_000_000, t0);
        assert!(!bucket.try_take(1001, t0));
        assert!(!bucket.try_take(1001, t0 + secs(3600)));
        assert!(bucket.try_take(1000, t0 + secs(3600)));
    }

    #[test]
    fn fractions_of_a_byte_are_not_lost() {
        let t0 = Instant::now();
        // 3 bytes/s: 1/3 s per byte, never a whole number of nanoseconds.
        let mut bucket = TokenBucket::new(10, 3, t0);
        assert!(bucket.try_take(10, t0));
        let mut now = t0;
        let mut taken = 0u32;
        for _ in 0..300 {
            now += Duration::from_millis(10);
            while bucket.try_take(1, now) {
                taken += 1;
            }
        }
        // 3 s elapsed at 3 bytes/s: exactly 9 bytes.
        assert_eq!(taken, 9);
    }

    #[test]
    fn time_going_backwards_adds_nothing() {
        let t0 = Instant::now() + secs(10);
        let mut bucket = TokenBucket::new(100, 100, t0);
        assert!(bucket.try_take(100, t0));
        assert!(!bucket.try_take(1, t0 - secs(5)));
        assert!(!bucket.try_take(1, t0));
        assert!(bucket.try_take(100, t0 + secs(1)));
    }

    #[test]
    fn zero_length_frame_always_passes() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(0, 0, t0);
        assert!(bucket.try_take(0, t0));
        assert!(!bucket.try_take(1, t0 + secs(60)));
    }

    #[test]
    fn from_kbps_uses_configuration_units() {
        let t0 = Instant::now();
        // 8000 kbit/s is 1_000_000 bytes/s; 1024 KiB burst is 1_048_576 bytes.
        let mut bucket = TokenBucket::from_kbps(8000, 1024, t0);
        assert_eq!(bucket.capacity_bytes(), 1_048_576);
        assert_eq!(bucket.refill_bytes_per_sec(), 1_000_000);
        assert!(bucket.try_take(1_048_576, t0));
        assert!(!bucket.try_take(1, t0));
        assert!(bucket.try_take(500_000, t0 + Duration::from_millis(500)));
        assert!(!bucket.try_take(1, t0 + Duration::from_millis(500)));
    }
}
