// SPDX-License-Identifier: GPL-3.0-or-later

//! Pure limit primitives: a payload-byte token bucket and a per-IP rate
//! limiter. Neither reads a clock; the caller passes the current `Instant`.

mod bucket;
mod rate;

pub use bucket::TokenBucket;
pub use rate::RateLimiter;
