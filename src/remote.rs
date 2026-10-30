// SPDX-License-Identifier: MIT

//! Transport-independent handling of authenticated remote messages.
//!
//! [`handle_message`] verifies a request, enforces monotonic-counter replay protection,
//! applies the command through a [`SwitchPort`], and returns the status to report.
//! Transports (UDP, debug shell, ...) only move bytes in and out.

use core::cell::Cell;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use log::{info, warn};

use crate::protocol::{verify_request, Key, RemoteCommand, ReplyStatus, RequestError};

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

/// Result of checking a request counter against the [`ReplayGuard`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// Newer than every accepted counter; now recorded as the last accepted one.
    Fresh,
    /// Equal to the last accepted counter, i.e. a client retry after a lost reply.
    Duplicate,
    /// Older than the last accepted counter, which is carried for resynchronization.
    Stale {
        /// Last accepted counter.
        last: u64,
    },
}

/// Monotonic counter shared by all transports.
pub struct ReplayGuard {
    last: Mutex<CriticalSectionRawMutex, Cell<u64>>,
}

impl ReplayGuard {
    /// Creates a guard that accepts any counter greater than zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last: Mutex::new(Cell::new(0)),
        }
    }

    /// Raises the last accepted counter to at least `last` (for example after loading
    /// it from flash at boot).
    pub fn restore(&self, last: u64) {
        self.last.lock(|cell| cell.set(cell.get().max(last)));
    }

    /// Returns the last accepted counter.
    #[must_use]
    pub fn last(&self) -> u64 {
        self.last.lock(Cell::get)
    }

    /// Atomically classifies `counter` and records it if it is fresh.
    pub fn check(&self, counter: u64) -> Freshness {
        self.last.lock(|cell| {
            let last = cell.get();
            if counter > last {
                cell.set(counter);
                Freshness::Fresh
            } else if counter == last {
                Freshness::Duplicate
            } else {
                Freshness::Stale { last }
            }
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
/// message must be dropped without a reply. Silently dropping unauthenticated input
/// keeps scanners and garbage from making the device spend data on replies.
///
/// A retry that reuses the last accepted counter gets the current state but is not
/// re-applied, so a replayed request can never override a newer local button press.
pub fn handle_message(
    key: &Key,
    guard: &ReplayGuard,
    port: &impl SwitchPort,
    message: &[u8],
) -> Result<(ReplyStatus, u64), RequestError> {
    let request = verify_request(key, message)?;

    match guard.check(request.counter) {
        Freshness::Fresh => {
            if !port.persist_counter(request.counter) {
                warn!(
                    "Failed to persist counter {}; replay protection is RAM-only until reboot",
                    request.counter
                );
            }
            let status = apply(port, request.command);
            info!(
                "Remote {} (counter {}) -> {}",
                request.command.as_str(),
                request.counter,
                status.as_str()
            );
            Ok((status, request.counter))
        }
        Freshness::Duplicate => {
            info!(
                "Duplicate remote {} (counter {}); reporting state only",
                request.command.as_str(),
                request.counter
            );
            Ok((current_status(port), request.counter))
        }
        Freshness::Stale { last } => {
            warn!(
                "Stale remote {} (counter {} <= {last})",
                request.command.as_str(),
                request.counter
            );
            Ok((ReplyStatus::Stale, last))
        }
    }
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
    use std::cell::RefCell;

    struct MockPort {
        state: Cell<SwitchState>,
        accept_requests: bool,
        persist_ok: bool,
        requests: RefCell<Vec<bool>>,
        persisted: RefCell<Vec<u64>>,
    }

    impl MockPort {
        fn new(state: SwitchState) -> Self {
            Self {
                state: Cell::new(state),
                accept_requests: true,
                persist_ok: true,
                requests: RefCell::new(Vec::new()),
                persisted: RefCell::new(Vec::new()),
            }
        }
    }

    impl SwitchPort for MockPort {
        fn state(&self) -> SwitchState {
            self.state.get()
        }

        fn request(&self, on: bool) -> bool {
            if self.accept_requests {
                self.requests.borrow_mut().push(on);
                // Model the controller applying the command immediately.
                self.state.set(if on {
                    SwitchState::On
                } else {
                    SwitchState::Off
                });
            }
            self.accept_requests
        }

        fn persist_counter(&self, counter: u64) -> bool {
            self.persisted.borrow_mut().push(counter);
            self.persist_ok
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
        assert_eq!(guard.check(5), Freshness::Fresh);
        assert_eq!(guard.check(5), Freshness::Duplicate);
        assert_eq!(guard.check(4), Freshness::Stale { last: 5 });
        assert_eq!(guard.check(6), Freshness::Fresh);
        assert_eq!(guard.last(), 6);

        guard.restore(3);
        assert_eq!(guard.last(), 6, "restore never lowers the counter");
        guard.restore(100);
        assert_eq!(guard.check(100), Freshness::Duplicate);
        assert_eq!(guard.check(u64::MAX), Freshness::Fresh);
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
        assert_eq!(*port.requests.borrow(), vec![true, false]);
        assert_eq!(*port.persisted.borrow(), vec![10, 11]);
    }

    #[test]
    fn status_reports_without_requesting_changes() {
        let guard = ReplayGuard::new();
        let port = MockPort::new(SwitchState::On);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Status, 1),
            (ReplyStatus::On, 1)
        );
        assert!(port.requests.borrow().is_empty());
        assert_eq!(*port.persisted.borrow(), vec![1]);
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
        port.state.set(SwitchState::On);
        assert_eq!(
            handle(&guard, &port, RemoteCommand::Off, 20),
            (ReplyStatus::On, 20)
        );
        assert_eq!(*port.requests.borrow(), vec![false]);
        assert_eq!(*port.persisted.borrow(), vec![20]);
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
        assert_eq!(*port.requests.borrow(), vec![true]);
        assert_eq!(port.state(), SwitchState::On);
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
        assert!(port.requests.borrow().is_empty());
        assert!(port.persisted.borrow().is_empty());
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
        assert!(port.requests.borrow().is_empty());
    }

    #[test]
    fn full_queue_reports_busy() {
        let guard = ReplayGuard::new();
        let mut port = MockPort::new(SwitchState::Off);
        port.accept_requests = false;
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 1),
            (ReplyStatus::Busy, 1)
        );
        assert_eq!(port.state(), SwitchState::Off);
    }

    #[test]
    fn persist_failure_still_applies_command() {
        let guard = ReplayGuard::new();
        let mut port = MockPort::new(SwitchState::Off);
        port.persist_ok = false;
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 1),
            (ReplyStatus::On, 1)
        );
        assert_eq!(
            handle(&guard, &port, RemoteCommand::On, 1).0,
            ReplyStatus::On
        );
        assert_eq!(*port.requests.borrow(), vec![true]);
    }
}
