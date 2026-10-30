// SPDX-License-Identifier: MIT

use std::sync::{Mutex, PoisonError};

use super::*;

static FFI_TEST_LOCK: Mutex<()> = Mutex::new(());

fn with_isolated_queue<F: FnOnce()>(f: F) {
    let _guard = FFI_TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    while COMMAND_CHANNEL.try_receive().is_ok() {}
    let _ = take_dropped_commands();
    f();
    while COMMAND_CHANNEL.try_receive().is_ok() {}
    let _ = take_dropped_commands();
}

#[test]
fn resolve_covers_all_command_variants() {
    assert!(SwitchCommand::Toggle.resolve(false));
    assert!(!SwitchCommand::Toggle.resolve(true));
    assert!(!SwitchCommand::Set(false).resolve(false));
    assert!(!SwitchCommand::Set(false).resolve(true));
    assert!(SwitchCommand::Set(true).resolve(false));
    assert!(SwitchCommand::Set(true).resolve(true));
}

#[test]
fn toggle_transitions_between_off_and_on() {
    let mut is_on = false;
    assert_eq!(
        evaluate_command(&mut is_on, true, SwitchCommand::Toggle),
        StateTransition::Changed(true)
    );
    assert!(is_on);

    assert_eq!(
        evaluate_command(&mut is_on, true, SwitchCommand::Toggle),
        StateTransition::Changed(false)
    );
    assert!(!is_on);
}

#[test]
fn set_is_idempotent_when_state_already_matches() {
    let mut is_on = false;
    assert_eq!(
        evaluate_command(&mut is_on, true, SwitchCommand::Set(false)),
        StateTransition::Unchanged(false)
    );
    assert!(!is_on);

    assert_eq!(
        evaluate_command(&mut is_on, true, SwitchCommand::Set(true)),
        StateTransition::Changed(true)
    );
    assert!(is_on);

    assert_eq!(
        evaluate_command(&mut is_on, true, SwitchCommand::Set(true)),
        StateTransition::Unchanged(true)
    );
    assert!(is_on);

    assert_eq!(
        evaluate_command(&mut is_on, true, SwitchCommand::Set(false)),
        StateTransition::Changed(false)
    );
    assert!(!is_on);
}

#[test]
fn degraded_unavailable_output_never_reports_on() {
    let mut is_on = false;
    assert_eq!(
        evaluate_command(&mut is_on, false, SwitchCommand::Set(true)),
        StateTransition::Unavailable
    );
    assert!(!is_on);

    assert_eq!(
        evaluate_command(&mut is_on, false, SwitchCommand::Set(false)),
        StateTransition::Unavailable
    );
    assert!(!is_on);

    let mut forced_off = true;
    assert_eq!(
        evaluate_command(&mut forced_off, false, SwitchCommand::Toggle),
        StateTransition::Unavailable
    );
    assert!(!forced_off);
}

#[test]
fn boot_confirmation_withheld_only_when_configured_switch0_fails_init() {
    assert!(should_confirm_boot_image(true, true));
    assert!(should_confirm_boot_image(true, false));
    assert!(should_confirm_boot_image(false, false));
    assert!(!should_confirm_boot_image(false, true));
}

#[test]
fn switch_state_publication_round_trips() {
    let _guard = FFI_TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    for state in [SwitchState::On, SwitchState::Unavailable, SwitchState::Off] {
        publish_switch_state(state);
        assert_eq!(published_switch_state(), state);
        assert_eq!(FirmwarePort.state(), state);
    }
}

fn call_ffi(message: &[u8]) -> Option<String> {
    let mut reply = [0u8; protocol::MAX_MESSAGE_LEN];
    // SAFETY: Both buffers are valid for their full lengths and do not overlap.
    let len = unsafe {
        rust_handle_remote_message(
            message.as_ptr(),
            message.len(),
            reply.as_mut_ptr(),
            reply.len(),
        )
    };
    let len = usize::try_from(len).unwrap();
    (len > 0).then(|| String::from_utf8(reply[..len].to_vec()).unwrap())
}

#[test]
fn ffi_remote_message_authenticates_applies_and_replies() {
    let key = Key::new([0x42; protocol::KEY_LEN]);
    let request = |command, counter| {
        let mut buf = [0u8; protocol::MAX_MESSAGE_LEN];
        let len = protocol::encode_request(&key, command, counter, &mut buf).unwrap();
        buf[..len].to_vec()
    };
    let reply = |status, counter| {
        let mut buf = [0u8; protocol::MAX_MESSAGE_LEN];
        let len = protocol::encode_reply(&key, status, counter, &mut buf).unwrap();
        String::from_utf8(buf[..len].to_vec()).unwrap()
    };

    with_isolated_queue(|| {
        let _ = REMOTE_KEY.init(key.clone());
        publish_switch_state(SwitchState::Off);
        let older = REPLAY_GUARD.last() + 1;
        let fresh = older + 1;
        use protocol::{RemoteCommand, ReplyStatus};

        assert_eq!(
            call_ffi(&request(RemoteCommand::On, fresh)),
            Some(reply(ReplyStatus::On, fresh))
        );
        assert_eq!(
            COMMAND_CHANNEL.try_receive().unwrap(),
            SwitchCommand::Set(true)
        );

        // Same counter with a different command must report `stale`, not `duplicate`.
        assert_eq!(
            call_ffi(&request(RemoteCommand::Off, fresh)),
            Some(reply(ReplyStatus::Stale, fresh))
        );

        // Forged, stale, and null inputs never reach the switch.
        let mut forged = request(RemoteCommand::Off, fresh + 1);
        forged[0] = b'x';
        assert_eq!(call_ffi(&forged), None);
        assert_eq!(
            call_ffi(&request(RemoteCommand::Off, older)),
            Some(reply(ReplyStatus::Stale, fresh))
        );
        let mut buf = [0u8; 8];
        assert_eq!(
            // SAFETY: Null pointers are rejected before any dereference.
            unsafe { rust_handle_remote_message(core::ptr::null(), 4, buf.as_mut_ptr(), 8) },
            0
        );
        assert!(COMMAND_CHANNEL.try_receive().is_err());

        // A `busy` rejection when the queue is full must not burn the counter.
        for _ in 0..COMMAND_QUEUE_CAPACITY {
            assert!(rust_on_local_button_press());
        }
        let busy_counter = fresh + 1;
        assert_eq!(
            call_ffi(&request(RemoteCommand::Off, busy_counter)),
            Some(reply(ReplyStatus::Busy, busy_counter))
        );
        assert_eq!(REPLAY_GUARD.last(), fresh);
        let _ = take_dropped_commands();
        assert_eq!(
            COMMAND_CHANNEL.try_receive().unwrap(),
            SwitchCommand::Toggle
        );
        assert_eq!(
            call_ffi(&request(RemoteCommand::Off, busy_counter)),
            Some(reply(ReplyStatus::Off, busy_counter))
        );
        assert_eq!(REPLAY_GUARD.last(), busy_counter);

        // Excess replays are dropped silently (0 bytes) until a fresh counter arrives.
        let replayed = request(RemoteCommand::Off, busy_counter);
        for _ in 0..remote::MAX_NON_FRESH_REPLIES {
            assert_eq!(
                call_ffi(&replayed),
                Some(reply(ReplyStatus::Off, busy_counter))
            );
        }
        assert_eq!(call_ffi(&replayed), None);

        let next_counter = busy_counter + 1;
        assert_eq!(
            call_ffi(&request(RemoteCommand::Status, next_counter)),
            Some(reply(ReplyStatus::Off, next_counter))
        );
        publish_switch_state(SwitchState::Off);
    });
}

#[test]
fn watchdog_tick_coalesces_without_touching_command_queue() {
    with_isolated_queue(|| {
        WATCHDOG_SIGNAL.reset();
        for _ in 0..COMMAND_QUEUE_CAPACITY {
            assert!(rust_on_local_button_press());
        }
        rust_on_watchdog_tick();
        rust_on_watchdog_tick();
        assert_eq!(take_dropped_commands(), 0);
        assert_eq!(WATCHDOG_SIGNAL.try_take(), Some(()));
        assert_eq!(WATCHDOG_SIGNAL.try_take(), None);
    });
}

#[test]
fn queue_overflow_tracks_drops_preserves_fifo_and_recovers() {
    with_isolated_queue(|| {
        for i in 0..COMMAND_QUEUE_CAPACITY {
            if i % 2 == 0 {
                assert!(rust_on_local_button_press());
            } else {
                assert!(FirmwarePort.request(true));
            }
        }
        assert_eq!(take_dropped_commands(), 0);

        assert!(!rust_on_local_button_press());
        assert!(!FirmwarePort.request(false));
        assert!(!FirmwarePort.request(true));
        assert_eq!(take_dropped_commands(), 3);
        assert_eq!(take_dropped_commands(), 0);

        // Drain one slot and confirm the queue immediately accepts a new command.
        assert_eq!(
            COMMAND_CHANNEL.try_receive().unwrap(),
            SwitchCommand::Toggle
        );
        assert!(FirmwarePort.request(false));
        assert_eq!(take_dropped_commands(), 0);

        for i in 1..COMMAND_QUEUE_CAPACITY {
            let expected = if i % 2 == 0 {
                SwitchCommand::Toggle
            } else {
                SwitchCommand::Set(true)
            };
            assert_eq!(COMMAND_CHANNEL.try_receive().unwrap(), expected);
        }
        assert_eq!(
            COMMAND_CHANNEL.try_receive().unwrap(),
            SwitchCommand::Set(false)
        );
        assert!(COMMAND_CHANNEL.try_receive().is_err());
    });
}
