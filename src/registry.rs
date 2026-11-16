// SPDX-License-Identifier: GPL-3.0-or-later

//! The host registry and the channel-number allocator (architecture §3, §8;
//! specification 01 §3 and §4).
//!
//! [`Registry`] is the only state the connection tasks share: a map from
//! `host_id` to a handle the server chooses (typically the sender of the host
//! task's command queue). It sits behind a [`std::sync::Mutex`] and every
//! method is synchronous, so the lock can never be held across an `.await`.
//!
//! Each registration has a [`Generation`]. Registering over an existing
//! `host_id` replaces the handle in one step (there is no instant without an
//! owner) and hands the old handle back so the caller can tell the old task it
//! was replaced, outside the lock. The old task later calls
//! [`Registry::unregister`] with its own generation, which removes nothing if
//! a newer registration already took the place.
//!
//! [`ChannelAllocator`] belongs to a host task and is not shared. It hands out
//! channel numbers that are monotonic and never reused during the life of the
//! host connection, and counts the live channels against the per-host limit.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use porecatu_protocol::envelope::ChannelId;
use porecatu_protocol::host_id::HostId;

/// Identifies one registration of a `host_id`. Monotonic over the life of the
/// registry; a replacement always gets a larger value than what it replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(u64);

impl Generation {
    /// The number, for logs and tests.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// The result of [`Registry::register`].
#[derive(Debug, PartialEq, Eq)]
pub enum RegisterOutcome<H> {
    /// The `host_id` was not registered; it is now.
    New {
        /// The generation of the new registration.
        generation: Generation,
    },
    /// The `host_id` was registered; the new handle took its place. The caller
    /// must tell the old one it was replaced (close `4002`).
    Replaced {
        /// The generation of the new registration.
        generation: Generation,
        /// The handle that was replaced.
        old: H,
    },
    /// `max_hosts` is reached and the `host_id` is not among the registered
    /// ones (a replacement never counts). The caller closes with `4003`.
    Full,
}

struct Inner<H> {
    hosts: HashMap<HostId, (Generation, H)>,
    next_generation: u64,
}

/// The registered hosts.
pub struct Registry<H> {
    max_hosts: usize,
    inner: Mutex<Inner<H>>,
}

impl<H> Registry<H> {
    /// An empty registry that holds at most `max_hosts` hosts.
    #[must_use]
    pub fn new(max_hosts: usize) -> Self {
        Self {
            max_hosts,
            inner: Mutex::new(Inner {
                hosts: HashMap::new(),
                next_generation: 0,
            }),
        }
    }

    /// The map holds plain data and no invariant spans a panic, so a poisoned
    /// lock is taken as it is rather than turned into a second panic.
    fn lock(&self) -> MutexGuard<'_, Inner<H>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers `handle` under `host_id`; the last registration wins.
    pub fn register(&self, host_id: HostId, handle: H) -> RegisterOutcome<H> {
        let mut inner = self.lock();
        let replacing = inner.hosts.contains_key(&host_id);
        if !replacing && inner.hosts.len() >= self.max_hosts {
            return RegisterOutcome::Full;
        }
        let generation = Generation(inner.next_generation);
        inner.next_generation += 1;
        match inner.hosts.insert(host_id, (generation, handle)) {
            Some((_, old)) => RegisterOutcome::Replaced { generation, old },
            None => RegisterOutcome::New { generation },
        }
    }

    /// Removes the registration of `host_id` **only if** it is still the one
    /// of `generation`. Returns whether something was removed: a connection
    /// that was already replaced removes nothing.
    pub fn unregister(&self, host_id: &HostId, generation: Generation) -> bool {
        let mut inner = self.lock();
        match inner.hosts.get(host_id) {
            Some((current, _)) if *current == generation => {
                inner.hosts.remove(host_id);
                true
            }
            _ => false,
        }
    }

    /// Whether a host is registered under `host_id` (the presence query).
    #[must_use]
    pub fn is_present(&self, host_id: &HostId) -> bool {
        self.lock().hosts.contains_key(host_id)
    }

    /// Number of registered hosts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().hosts.len()
    }

    /// Whether no host is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().hosts.is_empty()
    }
}

impl<H: Clone> Registry<H> {
    /// The handle of the host under `host_id`, cloned out of the lock.
    #[must_use]
    pub fn lookup(&self, host_id: &HostId) -> Option<H> {
        self.lock()
            .hosts
            .get(host_id)
            .map(|(_, handle)| handle.clone())
    }
}

/// Largest channel number the wire can carry (`2^53 - 1`, specification 03
/// §1.1).
const MAX_CHANNEL: u64 = (1 << 53) - 1;

/// Why [`ChannelAllocator::allocate`] gave no channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    /// `max_channels_per_host` channels are already live.
    TooManyLive,
    /// Every number up to the wire maximum was used on this connection.
    Exhausted,
}

/// Hands out the channel numbers of one host connection.
///
/// Numbers start at `0`, only grow, and are never reused: releasing a channel
/// frees a slot against the limit, not its number.
#[derive(Debug)]
pub struct ChannelAllocator {
    max_live: usize,
    next: u64,
    live: HashSet<u64>,
}

impl ChannelAllocator {
    /// An allocator that allows at most `max_live` live channels.
    #[must_use]
    pub fn new(max_live: usize) -> Self {
        Self {
            max_live,
            next: 0,
            live: HashSet::new(),
        }
    }

    /// Opens a channel: the next number, if the limit allows.
    ///
    /// # Errors
    ///
    /// [`AllocError::TooManyLive`] at the limit (the client gets
    /// `closed { limit-exceeded }`), [`AllocError::Exhausted`] if the numbers
    /// ran out. A failed call consumes no number.
    pub fn allocate(&mut self) -> Result<ChannelId, AllocError> {
        if self.live.len() >= self.max_live {
            return Err(AllocError::TooManyLive);
        }
        if self.next > MAX_CHANNEL {
            return Err(AllocError::Exhausted);
        }
        let channel = self.next;
        self.next += 1;
        self.live.insert(channel);
        Ok(ChannelId(channel))
    }

    /// Frees the slot of a live channel. Returns `false`, and changes
    /// nothing, if the channel was not live.
    pub fn release(&mut self, channel: ChannelId) -> bool {
        self.live.remove(&channel.0)
    }

    /// Whether the channel is open now.
    #[must_use]
    pub fn is_live(&self, channel: ChannelId) -> bool {
        self.live.contains(&channel.0)
    }

    /// Whether the relay ever gave out this number on this connection. A
    /// number for which this is `false` was never opened: the host naming it
    /// sent an invalid envelope (`4004`), unlike one that merely closed.
    #[must_use]
    pub fn was_issued(&self, channel: ChannelId) -> bool {
        channel.0 < self.next
    }

    /// Number of live channels.
    #[must_use]
    pub fn live(&self) -> usize {
        self.live.len()
    }
}

#[cfg(test)]
mod tests {
    use porecatu_protocol::host_id::host_id;

    use super::*;

    fn id(n: u8) -> HostId {
        host_id(&[n; 32])
    }

    #[test]
    fn new_host_is_registered_and_present() {
        let registry = Registry::new(4);
        assert!(registry.is_empty());
        assert!(!registry.is_present(&id(1)));
        assert_eq!(registry.lookup(&id(1)), None);

        let outcome = registry.register(id(1), "a");
        assert!(matches!(outcome, RegisterOutcome::New { .. }));
        assert!(registry.is_present(&id(1)));
        assert!(!registry.is_present(&id(2)));
        assert_eq!(registry.lookup(&id(1)), Some("a"));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn replacement_returns_the_old_handle_and_keeps_the_count() {
        let registry = Registry::new(4);
        let RegisterOutcome::New { generation: first } = registry.register(id(1), "old") else {
            panic!("expected New");
        };
        let outcome = registry.register(id(1), "new");
        let RegisterOutcome::Replaced {
            generation: second,
            old,
        } = outcome
        else {
            panic!("expected Replaced, got {outcome:?}");
        };
        assert_eq!(old, "old");
        assert!(second > first);
        assert_eq!(registry.lookup(&id(1)), Some("new"));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn unregister_with_a_stale_generation_removes_nothing() {
        let registry = Registry::new(4);
        let RegisterOutcome::New { generation: first } = registry.register(id(1), "old") else {
            panic!("expected New");
        };
        let RegisterOutcome::Replaced {
            generation: second, ..
        } = registry.register(id(1), "new")
        else {
            panic!("expected Replaced");
        };

        // The old task, already replaced, cleans up after itself.
        assert!(!registry.unregister(&id(1), first));
        assert!(registry.is_present(&id(1)));
        assert_eq!(registry.lookup(&id(1)), Some("new"));

        // The current one does remove it, and only once.
        assert!(registry.unregister(&id(1), second));
        assert!(!registry.is_present(&id(1)));
        assert!(!registry.unregister(&id(1), second));
        assert!(registry.is_empty());
    }

    #[test]
    fn unregister_of_an_absent_host_is_harmless() {
        let registry: Registry<&str> = Registry::new(4);
        assert!(!registry.unregister(&id(9), Generation(0)));
    }

    #[test]
    fn full_registry_refuses_a_new_host_but_accepts_a_replacement() {
        let registry = Registry::new(2);
        assert!(matches!(
            registry.register(id(1), "a"),
            RegisterOutcome::New { .. }
        ));
        assert!(matches!(
            registry.register(id(2), "b"),
            RegisterOutcome::New { .. }
        ));
        assert_eq!(registry.register(id(3), "c"), RegisterOutcome::Full);
        assert!(!registry.is_present(&id(3)));
        assert_eq!(registry.len(), 2);

        // A replacement does not count against the limit.
        let outcome = registry.register(id(2), "b2");
        assert!(matches!(
            outcome,
            RegisterOutcome::Replaced { old: "b", .. }
        ));
        assert_eq!(registry.len(), 2);

        // Leaving frees a slot.
        let RegisterOutcome::Replaced { generation, .. } = registry.register(id(1), "a2") else {
            panic!("expected Replaced");
        };
        assert!(registry.unregister(&id(1), generation));
        assert!(matches!(
            registry.register(id(3), "c"),
            RegisterOutcome::New { .. }
        ));
    }

    #[test]
    fn generations_grow_across_hosts() {
        let registry = Registry::new(4);
        let mut last = None;
        for n in 1..=3 {
            let RegisterOutcome::New { generation } = registry.register(id(n), n) else {
                panic!("expected New");
            };
            if let Some(last) = last {
                assert!(generation > last);
            }
            last = Some(generation);
        }
    }

    #[test]
    fn lookup_clones_the_handle_out_of_the_lock() {
        let registry = Registry::new(1);
        registry.register(id(1), std::sync::Arc::new(5));
        let handle = registry.lookup(&id(1)).unwrap();
        // The registry still holds its own; the lock is free again.
        assert_eq!(std::sync::Arc::strong_count(&handle), 2);
        assert!(registry.is_present(&id(1)));
    }

    #[test]
    fn channels_are_monotonic_from_zero() {
        let mut channels = ChannelAllocator::new(8);
        for expected in 0..5 {
            assert_eq!(channels.allocate(), Ok(ChannelId(expected)));
        }
        assert_eq!(channels.live(), 5);
    }

    #[test]
    fn live_channels_are_limited() {
        let mut channels = ChannelAllocator::new(2);
        assert_eq!(channels.allocate(), Ok(ChannelId(0)));
        assert_eq!(channels.allocate(), Ok(ChannelId(1)));
        assert_eq!(channels.allocate(), Err(AllocError::TooManyLive));
        // A refusal consumes no number.
        assert!(channels.release(ChannelId(0)));
        assert_eq!(channels.allocate(), Ok(ChannelId(2)));
    }

    #[test]
    fn a_released_number_is_never_reused() {
        let mut channels = ChannelAllocator::new(1);
        let first = channels.allocate().unwrap();
        assert!(channels.release(first));
        let second = channels.allocate().unwrap();
        assert_ne!(first, second);
        assert_eq!(second, ChannelId(1));
        assert!(channels.release(second));
        assert_eq!(channels.allocate(), Ok(ChannelId(2)));
    }

    #[test]
    fn releasing_twice_does_not_open_a_slot() {
        let mut channels = ChannelAllocator::new(1);
        let first = channels.allocate().unwrap();
        assert!(channels.release(first));
        assert!(!channels.release(first));
        assert!(!channels.release(ChannelId(99)));
        let _second = channels.allocate().unwrap();
        // Only one slot exists, however many times the first was released.
        assert_eq!(channels.allocate(), Err(AllocError::TooManyLive));
    }

    #[test]
    fn closed_and_never_opened_channels_are_told_apart() {
        let mut channels = ChannelAllocator::new(8);
        let a = channels.allocate().unwrap();
        let b = channels.allocate().unwrap();
        channels.release(a);

        assert!(!channels.is_live(a));
        assert!(channels.was_issued(a));
        assert!(channels.is_live(b));
        assert!(channels.was_issued(b));
        // The next number the relay would give, and beyond: never opened.
        assert!(!channels.was_issued(ChannelId(2)));
        assert!(!channels.was_issued(ChannelId(1000)));
    }

    #[test]
    fn numbers_run_out_at_the_wire_maximum() {
        let mut channels = ChannelAllocator::new(8);
        channels.next = MAX_CHANNEL;
        assert_eq!(channels.allocate(), Ok(ChannelId(MAX_CHANNEL)));
        assert!(channels.release(ChannelId(MAX_CHANNEL)));
        assert_eq!(channels.allocate(), Err(AllocError::Exhausted));
    }
}
