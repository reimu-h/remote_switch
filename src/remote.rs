// SPDX-License-Identifier: MIT

//! Transport-independent handling of authenticated remote messages.
//!
//! [`handle_message`] verifies a request, enforces monotonic-counter replay protection,
//! applies the command through a [`SwitchPort`], and returns the status to report.
//! Transports (UDP, debug shell, ...) only move bytes in and out.

use log::{info, warn};

use crate::protocol::{verify_request, Key, RemoteCommand, ReplyStatus, RequestError};

struct ThreadMutex<T> {
    #[cfg(target_os = "none")]
    inner: zephyr::sync::Mutex<T>,
    #[cfg(not(target_os = "none"))]
    inner: std::sync::Mutex<T>,
}

impl<T> ThreadMutex<T> {
    const fn new(value: T) -> Self {
        Self {
            #[cfg(target_os = "none")]
            inner: zephyr::sync::Mutex::new(value),
            #[cfg(not(target_os = "none"))]
            inner: std::sync::Mutex::new(value),
        }
    }

    fn with_lock<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        #[cfg(target_os = "none")]
        let mut guard = self.inner.lock().unwrap();
        #[cfg(not(target_os = "none"))]
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }
}

/// Switch state as reported to remote clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchState {
    /// Output is driven inactive.
    Off,
    /// Output is driven active.
    On,
    /// Output hardware is missing or failed to initialize; the switch is forced off.
    Unavailable,
}

/// Connects the handler to the switch controller and persistent storage.
pub trait SwitchPort {
    /// Returns the current switch state.
    fn state(&self) -> SwitchState;

    /// Queues a request to drive the switch. Returns `false` if the queue is full.
    fn request(&self, on: bool) -> bool;

    /// Persists the last accepted counter so replay protection survives reboots.
    /// Returns `false` on storage failure.
    fn persist_counter(&self, counter: u64) -> bool;
}

/// Maximum number of non-fresh (`Duplicate` or `Stale`) replies emitted per accepted
/// counter advance before further replays are silently dropped.
///
/// Covers `tools/rsctl.py`'s default retry count (`--retries 3`) plus clock-skew
/// resynchronization, while preventing a captured packet from eliciting unbounded
/// outbound cellular traffic.
pub const MAX_NON_FRESH_REPLIES: u8 = 4;

/// Result of checking a request counter and command against the [`ReplayGuard`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// Newer than every accepted counter.
    Fresh,
    /// Equal to the last accepted counter and command, i.e. a client retry after a lost reply.
    Duplicate,
    /// Not newer than the last accepted counter (or same counter for a different/unknown command),
    /// carrying the last accepted counter for client resynchronization.
    Stale {
        /// Last accepted counter.
        last: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReplayState {
    last: u64,
    last_command: Option<RemoteCommand>,
    non_fresh_replies: u8,
}

impl ReplayState {
    fn classify(self, command: RemoteCommand, counter: u64) -> Freshness {
        if counter > self.last {
            Freshness::Fresh
        } else if counter == self.last && matches!(self.last_command, Some(prev) if prev == command)
        {
            Freshness::Duplicate
        } else {
            Freshness::Stale { last: self.last }
        }
    }
}

/// Monotonic counter and cross-transport serialization lock shared by all transports.
pub struct ReplayGuard {
    state: ThreadMutex<ReplayState>,
}

impl ReplayGuard {
    /// Creates a guard that accepts any counter greater than zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: ThreadMutex::new(ReplayState {
                last: 0,
                last_command: None,
                non_fresh_replies: 0,
            }),
        }
    }

    /// Raises the last accepted counter to at least `last` (for example after loading
    /// it from flash at boot).
    pub fn restore(&self, last: u64) {
        self.state.with_lock(|state| {
            if last > state.last {
                state.last = last;
                state.last_command = None;
                state.non_fresh_replies = 0;
            }
        });
    }

    /// Returns the last accepted counter.
    #[must_use]
    pub fn last(&self) -> u64 {
        self.state.with_lock(|state| state.last)
    }

    /// Classifies `(command, counter)` without modifying state unless `counter` is fresh,
    /// in which case `(command, counter)` is recorded.
    pub fn check(&self, command: RemoteCommand, counter: u64) -> Freshness {
        self.state.with_lock(|state| {
            let freshness = state.classify(command, counter);
            if freshness == Freshness::Fresh {
                state.last = counter;
                state.last_command = Some(command);
                state.non_fresh_replies = 0;
            }
            freshness
        })
    }
}

impl Default for ReplayGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Verifies and applies one remote message.
///
/// Returns the status and counter to put in the authenticated reply, or the reason the
/// message must be dropped without a reply. Silently dropping unauthenticated input and
/// rate-limiting repeated non-fresh replies keeps scanners and packet replays from
/// making the device spend unbounded data on replies.
///
/// Counter check, command queueing, and counter persistence execute atomically under
/// `guard`'s thread mutex so concurrent transports cannot reorder commands relative to
/// their counters. A `Busy` outcome (full command queue) does not advance the counter,
/// allowing transport retries to apply the command once space frees up. A retry that
/// reuses both the last accepted counter and command gets the current state without
/// re-applying, so a replayed request can never override a newer local button press.
pub fn handle_message(
    key: &Key,
    guard: &ReplayGuard,
    port: &impl SwitchPort,
    message: &[u8],
) -> Result<(ReplyStatus, u64), RequestError> {
    let request = verify_request(key, message)?;

    guard.state.with_lock(
        |state| match state.classify(request.command, request.counter) {
            Freshness::Fresh => {
                let status = apply(port, request.command);
                if status == ReplyStatus::Busy {
                    warn!(
                        "Remote {} (counter {}) -> busy; counter not consumed",
                        request.command.as_str(),
                        request.counter
                    );
                    return Ok((ReplyStatus::Busy, request.counter));
                }
                state.last = request.counter;
                state.last_command = Some(request.command);
                state.non_fresh_replies = 0;
                // Actuation intentionally proceeds even if flash persistence fails (for
                // example after NVS wear) so an `off` command can still de-energize the
                // load under RAM-backed replay protection until reboot.
                if !port.persist_counter(request.counter) {
                    warn!(
                        "Failed to persist counter {}; replay protection is RAM-only until reboot",
                        request.counter
                    );
                }
                info!(
                    "Remote {} (counter {}) -> {}",
                    request.command.as_str(),
                    request.counter,
                    status.as_str()
                );
                Ok((status, request.counter))
            }
            Freshness::Duplicate => {
                if state.non_fresh_replies >= MAX_NON_FRESH_REPLIES {
                    return Err(RequestError::RateLimited);
                }
                state.non_fresh_replies += 1;
                info!(
                    "Duplicate remote {} (counter {}); reporting state only",
                    request.command.as_str(),
                    request.counter
                );
                Ok((current_status(port), request.counter))
            }
            Freshness::Stale { last } => {
                if state.non_fresh_replies >= MAX_NON_FRESH_REPLIES {
                    return Err(RequestError::RateLimited);
                }
                state.non_fresh_replies += 1;
                warn!(
                    "Stale remote {} (counter {} <= {last})",
                    request.command.as_str(),
                    request.counter
                );
                Ok((ReplyStatus::Stale, last))
            }
        },
    )
}

fn apply(port: &impl SwitchPort, command: RemoteCommand) -> ReplyStatus {
    let on = match command {
        RemoteCommand::Status => return current_status(port),
        RemoteCommand::On => true,
        RemoteCommand::Off => false,
    };
    if port.state() == SwitchState::Unavailable {
        ReplyStatus::Unavailable
    } else if !port.request(on) {
        ReplyStatus::Busy
    } else if on {
        ReplyStatus::On
    } else {
        ReplyStatus::Off
    }
}

fn current_status(port: &impl SwitchPort) -> ReplyStatus {
    match port.state() {
        SwitchState::Off => ReplyStatus::Off,
        SwitchState::On => ReplyStatus::On,
        SwitchState::Unavailable => ReplyStatus::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{encode_request, MAX_MESSAGE_LEN};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Barrier, Mutex};

    struct MockPort {
        state: Mutex<SwitchState>,
        accept_requests: AtomicBool,
        persist_ok: AtomicBool,
        requests: Mutex<Vec<bool>>,
        persisted: Mutex<Vec<u64>>,
    }

    impl MockPort {
        fn new(state: SwitchState) -> Self {
            Self {
                state: Mutex::new(state),
                accept_requests: AtomicBool::new(true),
                persist_ok: AtomicBool::new(true),
                requests: Mutex::new(Vec::new()),
                persisted: Mutex::new(Vec::new()),
            }
        }

        fn set_state(&self, state: SwitchState) {
            *self.state.lock().unwrap() = state;
        }

        fn requests(&self) -> Vec<bool> {
            self.requests.lock().unwrap().clone()
        }

        fn persisted(&self) -> Vec<u64> {
            self.persisted.lock().unwrap().clone()
        }
    }

    impl SwitchPort for MockPort {
        fn state(&self) -> SwitchState {
            *self.state.lock().unwrap()
        }

        fn request(&self, on: bool) -> bool {
            let accept = self.accept_requests.load(Ordering::Relaxed);
            if accept {
                self.requests.lock().unwrap().push(on);
                // Model the controller applying the command in FIFO order.
                *self.state.lock().unwrap() = if on {
                    SwitchState::On
                } else {
                    SwitchState::Off
                };
            }
            accept
        }

        fn persist_counter(&self, counter: u64) -> bool {
            self.persisted.lock().unwrap().push(counter);
            self.persist_ok.load(Ordering::Relaxed)
        }
    }

    fn key() -> Key {
        Key::new([7; 32])
    }

    fn msg(command: RemoteCommand, counter: u64) -> Vec<u8> {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_request(&key(), command, counter, &mut buf).unwrap();
        buf[..len].to_vec()
    }

    fn handle(
        guard: &ReplayGuard,
        port: &MockPort,
        command: RemoteCommand,
        counter: u64,
    ) -> (ReplyStatus, u64) {
        handle_message(&key(), guard, port, &msg(command, counter)).unwrap()
    }

    #[test]
    fn replay_guard_classifies_and_advances_monotonically() {
        let guard = ReplayGuard::new();
        assert_eq!(guard.check(RemoteCommand::On, 5), Freshness::Fresh);
        assert_eq!(guard.check(RemoteCommand::On, 5), Freshness::Duplicate);
        assert_eq!(
            guard.check(RemoteCommand::Off, 5),
            Freshness::Stale { last: 5 }
        );
        assert_eq!(
            guard.check(RemoteCommand::On, 4),
            Freshness::Stale { last: 5 }
        );
        assert_eq!(guard.check(RemoteCommand::Off, 6), Freshness::Fresh);
        assert_eq!(guard.last(), 6);

        guard.restore(3);
        assert_eq!(guard.last(), 6, "restore never lowers the counter");
        assert_eq!(guard.check(RemoteCommand::Off, 6), Freshness::Duplicate);

        guard.restore(100);
        assert_eq!(
            guard.check(RemoteCommand::Off, 100),
            Freshness::Stale { last: 100 },
            "restored counter has unknown command and must force resynchronization"
        );
        assert_eq!(guard.check(RemoteCommand::On, u64::MAX), Freshness::Fresh);
    }

    #[test]
    fn on_off_apply_and_persist_counters() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::Off);

        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 10),
            (ReplyStatus::On, 10)
        );
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Off, 11),
            (ReplyStatus::Off, 11)
        );
        assert_eq!(port.requests(), vec![true, false]);
        assert_eq!(port.persisted(), vec![10, 11]);
    }

    #[test]
    fn status_reports_without_requesting_changes() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::On);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Status, 1),
            (ReplyStatus::On, 1)
        );
        assert!(port.requests().is_empty());
        assert_eq!(port.persisted(), vec![1]);
    }

    #[test]
    fn duplicate_reports_current_state_without_reapplying() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::Off);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Off, 20),
            (ReplyStatus::Off, 20)
        );

        // A local button press turns the switch on; a replay of the last remote "off"
        // must not turn it back off.
        port.set_state(SwitchState::On);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Off, 20),
            (ReplyStatus::On, 20)
        );
        assert_eq!(port.requests(), vec![false]);
        assert_eq!(port.persisted(), vec![20]);
    }

    #[test]
    fn same_counter_with_different_command_or_after_restore_is_stale() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::Off);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 20),
            (ReplyStatus::On, 20)
        );
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Off, 20),
            (ReplyStatus::Stale, 20)
        );
        assert_eq!(port.state(), SwitchState::On);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Off, 21),
            (ReplyStatus::Off, 21)
        );
        assert_eq!(port.requests(), vec![true, false]);

        guard.restore(50);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 50),
            (ReplyStatus::Stale, 50)
        );
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 51),
            (ReplyStatus::On, 51)
        );
    }

    #[test]
    fn stale_counters_are_rejected_with_last_accepted_counter() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::Off);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 50),
            (ReplyStatus::On, 50)
        );
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Off, 49),
            (ReplyStatus::Stale, 50)
        );
        assert_eq!(port.requests(), vec![true]);
        assert_eq!(port.state(), SwitchState::On);
    }

    #[test]
    fn non_fresh_replies_are_bounded_per_counter_advance() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::Off);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 50),
            (ReplyStatus::On, 50)
        );

        let dup = msg(RemoteCommand::On, 50);
        let stale = msg(RemoteCommand::Off, 49);
        for i in 0..MAX_NON_FRESH_REPLIES {
            let reply = if i % 2 == 0 {
                handle_message(&key(), &guard, &port, &dup)
            } else {
                handle_message(&key(), &guard, &port, &stale)
            };
            assert!(reply.is_ok(), "non-fresh attempt {i} should still reply");
        }

        // Once the retry/resync budget is exhausted, replays are silently dropped.
        assert_eq!(
            handle_message(&key(), &guard, &port, &dup),
            Err(RequestError::RateLimited)
        );
        assert_eq!(
            handle_message(&key(), &guard, &port, &stale),
            Err(RequestError::RateLimited)
        );
        assert_eq!(port.requests(), vec![true]);
        assert_eq!(port.persisted(), vec![50]);

        // Advancing to a fresh counter resets the retry budget for subsequent commands.
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Off, 51),
            (ReplyStatus::Off, 51)
        );
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Off, 51),
            (ReplyStatus::Off, 51)
        );
    }

    #[test]
    fn unauthenticated_messages_are_dropped_without_touching_state() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::Off);
        let mut forged = msg(RemoteCommand::On, 5);
        let last = forged.len() - 1;
        forged[last] = if forged[last] == b'0' { b'1' } else { b'0' };

        assert_eq!(
            handle_message(&key(), &guard, &port, &forged),
            Err(RequestError::BadTag)
        );
        assert_eq!(
            handle_message(&key(), &guard, &port, b"on 5"),
            Err(RequestError::Malformed)
        );
        assert_eq!(
            guard.last(),
            0,
            "rejected messages must not consume counters"
        );
        assert!(port.requests().is_empty());
        assert!(port.persisted().is_empty());
    }

    #[test]
    fn unavailable_output_refuses_changes() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::Unavailable);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 1),
            (ReplyStatus::Unavailable, 1)
        );
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Status, 2),
            (ReplyStatus::Unavailable, 2)
        );
        assert!(port.requests().is_empty());
    }

    #[test]
    fn full_queue_reports_busy_without_consuming_counter() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::Off);
        port.accept_requests.store(false, Ordering::Relaxed);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 1),
            (ReplyStatus::Busy, 1)
        );
        assert_eq!(guard.last(), 0, "busy rejection must not burn the counter");
        assert!(port.persisted().is_empty());
        assert_eq!(port.state(), SwitchState::Off);

        // Once queue space frees up, a retry of the same request must apply cleanly.
        port.accept_requests.store(true, Ordering::Relaxed);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 1),
            (ReplyStatus::On, 1)
        );
        assert_eq!(guard.last(), 1);
        assert_eq!(port.requests(), vec![true]);
        assert_eq!(port.persisted(), vec![1]);
        assert_eq!(port.state(), SwitchState::On);
    }

    #[test]
    fn persist_failure_still_applies_command() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::Off);
        port.persist_ok.store(false, Ordering::Relaxed);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 1),
            (ReplyStatus::On, 1)
        );
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 1).0,
            ReplyStatus::On
        );
        assert_eq!(port.requests(), vec![true]);
    }

    #[test]
    fn concurrent_transports_never_apply_older_counter_after_newer() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::Off);
        let barrier = Barrier::new(8);

        std::thread::scope(|scope| {
            for idx in 0..8u64 {
                let counter = idx + 1;
                // Odd counters turn ON, even counters (including max = 8) turn OFF.
                let command = if counter % 2 == 1 {
                    RemoteCommand::On
                } else {
                    RemoteCommand::Off
                };
                let guard = &guard;
                let port = &port;
                let barrier = &barrier;
                scope.spawn(move || {
                    let encoded = msg(command, counter);
                    barrier.wait();
                    let _ = handle_message(&key(), guard, port, &encoded).unwrap();
                });
            }
        });

        assert_eq!(guard.last(), 8);
        assert_eq!(
            port.state(),
            SwitchState::Off,
            "highest counter (8 -> Off) must always determine final state"
        );
        let persisted = port.persisted();
        assert_eq!(persisted.last().copied(), Some(8));
        for pair in persisted.windows(2) {
            assert!(
                pair[0] < pair[1],
                "persisted counters must strictly increase: {persisted:?}"
            );
        }
    }
}
