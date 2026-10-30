// SPDX-License-Identifier: MIT

#![cfg_attr(target_os = "none", no_std)]
#![allow(unexpected_cfgs)]

pub mod protocol;
pub mod remote;

use core::ffi::c_int;
#[cfg(not(target_os = "none"))]
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::once_lock::OnceLock;
#[cfg(target_os = "none")]
use log::info;
use log::warn;
#[cfg(target_os = "none")]
use static_cell::StaticCell;
#[cfg(target_os = "none")]
use zephyr::device::gpio::GpioPin;
#[cfg(target_os = "none")]
use zephyr::embassy::Executor;
#[cfg(all(target_os = "none", dt = "aliases::switch0"))]
use zephyr::raw::ZR_GPIO_OUTPUT_INACTIVE;
#[cfg(target_os = "none")]
use zephyr::sync::atomic::{AtomicU32, AtomicU8, Ordering};
#[cfg(target_os = "none")]
use zephyr_panic as _;

use protocol::Key;
use remote::{ReplayGuard, SwitchPort, SwitchState};

/// Capacity of the lock-free command queue shared between C callbacks and the
/// async `switch_controller` task.
pub const COMMAND_QUEUE_CAPACITY: usize = 8;

#[cfg(target_os = "none")]
extern "C" {
    fn board_status_led_set(on: bool);
    fn board_confirm_image();
    fn board_counter_load(value: *mut u64) -> bool;
    fn board_counter_store(value: u64) -> c_int;
    fn cellular_start() -> c_int;
}

/// Commands accepted by the switch state machine (from local button or authenticated remote radio).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchCommand {
    /// Invert the current switch state (`OFF -> ON` or `ON -> OFF`).
    Toggle,
    /// Explicitly drive the switch to `true` (`ON`) or `false` (`OFF`).
    Set(bool),
}

impl SwitchCommand {
    /// Computes the target switch state from `current_is_on`.
    #[must_use]
    pub const fn resolve(self, current_is_on: bool) -> bool {
        match self {
            Self::Toggle => !current_is_on,
            Self::Set(target) => target,
        }
    }
}

/// Outcome of applying a [`SwitchCommand`] to the switch state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateTransition {
    /// Switch state changed to the wrapped value (`true` = `ON`, `false` = `OFF`).
    Changed(bool),
    /// Switch was already in the requested state; no hardware write needed.
    Unchanged(bool),
    /// Hardware `switch0` output is unavailable; state remains forced `OFF`.
    Unavailable,
}

/// Evaluates a single [`SwitchCommand`] against `*is_on` and hardware availability.
///
/// When `output_available` is `false` (degraded mode), `*is_on` is forced to `false`
/// and [`StateTransition::Unavailable`] is returned so the firmware never illuminates
/// the status LED or logs a false `Switch state -> ON` transition.
pub fn evaluate_command(
    is_on: &mut bool,
    output_available: bool,
    cmd: SwitchCommand,
) -> StateTransition {
    if !output_available {
        *is_on = false;
        return StateTransition::Unavailable;
    }

    let next_state = cmd.resolve(*is_on);
    if next_state != *is_on {
        *is_on = next_state;
        StateTransition::Changed(next_state)
    } else {
        StateTransition::Unchanged(*is_on)
    }
}

/// Determines whether the running firmware should mark its MCUboot slot as confirmed.
///
/// Confirms the image when `switch0` initialized cleanly (`output_available`), or when
/// the target board intentionally omits `aliases::switch0` in devicetree
/// (`!switch_alias_configured`, such as generic evaluation targets). Withholds
/// confirmation if `aliases::switch0` was configured in devicetree yet failed runtime
/// initialization, allowing MCUboot test-swap upgrades to roll back on next reset.
#[must_use]
pub const fn should_confirm_boot_image(
    output_available: bool,
    switch_alias_configured: bool,
) -> bool {
    output_available || !switch_alias_configured
}

#[cfg(target_os = "none")]
static EXECUTOR: StaticCell<Executor> = StaticCell::new();

static COMMAND_CHANNEL: Channel<CriticalSectionRawMutex, SwitchCommand, COMMAND_QUEUE_CAPACITY> =
    Channel::new();

static DROPPED_COMMANDS: AtomicU32 = AtomicU32::new(0);

/// Enqueues a [`SwitchCommand`] without blocking or allocating.
///
/// Safe to invoke from any context, including Zephyr ISRs and small-stack threads.
/// Returns `true` if queued, or `false` if full (atomically incrementing the dropped counter).
fn enqueue_command(cmd: SwitchCommand) -> bool {
    if COMMAND_CHANNEL.try_send(cmd).is_ok() {
        true
    } else {
        DROPPED_COMMANDS.fetch_add(1, Ordering::Relaxed);
        false
    }
}

/// Atomically drains and returns the number of commands dropped due to queue overflow
/// since the previous call.
#[must_use]
pub fn take_dropped_commands() -> u32 {
    DROPPED_COMMANDS.swap(0, Ordering::Relaxed)
}

/// Called from `src/board_io.c` when a debounced local button press occurs.
///
/// Lock-free, allocation-free, and ISR-safe. Returns `true` if queued, `false` if full.
#[no_mangle]
pub extern "C" fn rust_on_local_button_press() -> bool {
    enqueue_command(SwitchCommand::Toggle)
}

const STATE_OFF: u8 = 0;
const STATE_ON: u8 = 1;
const STATE_UNAVAILABLE: u8 = 2;

/// Switch state published by `switch_controller` for remote status queries. Starts as
/// unavailable so remote commands are refused until the output is initialized.
static SWITCH_STATE: AtomicU8 = AtomicU8::new(STATE_UNAVAILABLE);

fn publish_switch_state(state: SwitchState) {
    let raw = match state {
        SwitchState::Off => STATE_OFF,
        SwitchState::On => STATE_ON,
        SwitchState::Unavailable => STATE_UNAVAILABLE,
    };
    SWITCH_STATE.store(raw, Ordering::Relaxed);
}

fn published_switch_state() -> SwitchState {
    match SWITCH_STATE.load(Ordering::Relaxed) {
        STATE_OFF => SwitchState::Off,
        STATE_ON => SwitchState::On,
        _ => SwitchState::Unavailable,
    }
}

/// Pre-shared key for remote commands. Remote control stays disabled while unset.
static REMOTE_KEY: OnceLock<Key> = OnceLock::new();

/// Replay protection shared by every remote transport.
static REPLAY_GUARD: ReplayGuard = ReplayGuard::new();

/// [`SwitchPort`] backed by the firmware's command queue, published state, and flash.
struct FirmwarePort;

impl SwitchPort for FirmwarePort {
    fn state(&self) -> SwitchState {
        published_switch_state()
    }

    fn request(&self, on: bool) -> bool {
        enqueue_command(SwitchCommand::Set(on))
    }

    #[cfg(target_os = "none")]
    fn persist_counter(&self, counter: u64) -> bool {
        // SAFETY: `board_counter_store` takes its argument by value, is only reached from
        // thread context (the cellular and shell threads), and serializes flash writes with
        // its own mutex.
        unsafe { board_counter_store(counter) == 0 }
    }

    #[cfg(not(target_os = "none"))]
    fn persist_counter(&self, _counter: u64) -> bool {
        true
    }
}

/// Verifies and applies one remote message, writing any reply into `reply`.
///
/// Returns the reply length, or 0 when the message must be dropped silently.
fn handle_remote_message(message: &[u8], reply: &mut [u8]) -> usize {
    let Some(key) = REMOTE_KEY.try_get() else {
        warn!("Dropped remote message: remote control is disabled (no key configured)");
        return 0;
    };
    match remote::handle_message(key, &REPLAY_GUARD, &FirmwarePort, message) {
        Ok((status, counter)) => protocol::encode_reply(key, status, counter, reply)
            .unwrap_or_else(|| {
                warn!("Reply buffer too small ({} bytes)", reply.len());
                0
            }),
        Err(err) => {
            warn!("Dropped remote message ({} bytes): {err:?}", message.len());
            0
        }
    }
}

/// Entry point for every remote transport (UDP datagrams, debug shell, ...).
///
/// Authenticates `message` (see [`protocol`]), enforces replay protection, applies the
/// command, and writes the authenticated reply into `reply`. Returns the number of reply
/// bytes to send back to the sender, or 0 if the message was rejected and must be
/// dropped without a reply.
///
/// # Safety
/// `message` must be valid for reads of `message_len` bytes and `reply` must be valid
/// for writes of `reply_capacity` bytes for the duration of the call, and the two
/// buffers must not overlap. Must be called from thread context (it may write flash).
#[no_mangle]
pub unsafe extern "C" fn rust_handle_remote_message(
    message: *const u8,
    message_len: usize,
    reply: *mut u8,
    reply_capacity: usize,
) -> c_int {
    if message.is_null() || reply.is_null() {
        return 0;
    }
    // SAFETY: The caller guarantees `message` is valid for `message_len` bytes of reads and
    // `reply` for `reply_capacity` bytes of writes, and that they do not overlap.
    let (message, reply) = unsafe {
        (
            core::slice::from_raw_parts(message, message_len),
            core::slice::from_raw_parts_mut(reply, reply_capacity),
        )
    };
    c_int::try_from(handle_remote_message(message, reply)).unwrap_or(0)
}

#[cfg(target_os = "none")]
fn set_status_led(on: bool) {
    // SAFETY: `set_status_led` is only invoked from `switch_controller`, which runs inside
    // the Zephyr thread-backed Embassy executor (never in ISR context). `board_status_led_set`
    // in `src/board_io.c` also checks `k_is_in_isr()` and `device_is_ready()` before performing
    // blocking I2C transfers to the LED controller.
    unsafe {
        board_status_led_set(on);
    }
}

#[cfg(target_os = "none")]
fn confirm_boot_image() {
    // SAFETY: `confirm_boot_image` is only invoked once from `switch_controller`, which runs
    // inside the Zephyr thread-backed Embassy executor (never in ISR context) after placing
    // the switch output into its safe OFF state. `board_confirm_image` in `src/board_io.c`
    // also checks `k_is_in_isr()` and guards flash trailer updates with `CONFIG_MCUBOOT_IMG_MANAGER`.
    unsafe {
        board_confirm_image();
    }
}

#[cfg(all(target_os = "none", dt = "aliases::switch0"))]
fn init_switch_pin() -> Option<GpioPin> {
    let Some(mut pin) = zephyr::devicetree::aliases::switch0::get_instance() else {
        warn!("Devicetree alias `switch0` GPIO instance already taken or unavailable");
        return None;
    };
    if !pin.is_ready() {
        warn!("Devicetree alias `switch0` GPIO controller is not ready");
        return None;
    }
    pin.configure(ZR_GPIO_OUTPUT_INACTIVE);
    Some(pin)
}

#[cfg(all(target_os = "none", not(dt = "aliases::switch0")))]
fn init_switch_pin() -> Option<GpioPin> {
    warn!("Devicetree alias `switch0` is not defined");
    None
}

#[cfg(target_os = "none")]
fn log_dropped_commands_if_any() {
    let dropped = take_dropped_commands();
    if dropped > 0 {
        warn!("Command queue full; dropped {dropped} command(s)");
    }
}

#[cfg(target_os = "none")]
#[embassy_executor::task]
async fn switch_controller(mut switch_pin: Option<GpioPin>) -> ! {
    let mut is_on = false;
    if let Some(pin) = switch_pin.as_mut() {
        pin.set(false);
        set_status_led(false);
        publish_switch_state(SwitchState::Off);
        info!("Switch state -> OFF");
        info!("Ready: press button to toggle `switch0` output");
    } else {
        set_status_led(false);
        publish_switch_state(SwitchState::Unavailable);
        warn!("Degraded mode: `switch0` output unavailable; switch forced OFF");
    }
    if should_confirm_boot_image(switch_pin.is_some(), cfg!(dt = "aliases::switch0")) {
        confirm_boot_image();
    } else {
        warn!("Skipping MCUboot image confirmation due to unavailable `switch0` output");
    }

    let receiver = COMMAND_CHANNEL.receiver();
    loop {
        log_dropped_commands_if_any();
        let cmd = receiver.receive().await;
        log_dropped_commands_if_any();

        match evaluate_command(&mut is_on, switch_pin.is_some(), cmd) {
            StateTransition::Changed(new_state) => {
                if let Some(pin) = switch_pin.as_mut() {
                    pin.set(new_state);
                }
                set_status_led(new_state);
                publish_switch_state(if new_state {
                    SwitchState::On
                } else {
                    SwitchState::Off
                });
                info!("Switch state -> {}", if new_state { "ON" } else { "OFF" });
            }
            StateTransition::Unchanged(_) => {}
            StateTransition::Unavailable => {
                warn!("Ignoring {cmd:?}: `switch0` output is unavailable");
            }
        }
    }
}

/// Loads the remote key and the persisted replay counter. Remote commands are dropped
/// until this succeeds.
#[cfg(target_os = "none")]
fn init_remote_control() {
    let Some(key) = Key::from_hex(zephyr::kconfig::CONFIG_REMOTE_SWITCH_KEY) else {
        warn!("CONFIG_REMOTE_SWITCH_KEY is not 64 hex digits; remote control disabled");
        return;
    };
    let mut last_counter = 0u64;
    // SAFETY: `last_counter` is a valid, exclusively borrowed `u64` for the duration of
    // the call, and `rust_main` runs in thread context before any remote transport starts.
    if unsafe { board_counter_load(&mut last_counter) } {
        REPLAY_GUARD.restore(last_counter);
    }
    if REMOTE_KEY.init(key).is_err() {
        warn!("Remote key already initialized");
    }
    info!(
        "Remote control enabled (last counter {})",
        REPLAY_GUARD.last()
    );
}

#[cfg(target_os = "none")]
#[no_mangle]
extern "C" fn rust_main() {
    // SAFETY: `rust_main` runs once on the main Zephyr thread at boot before any tasks
    // or callbacks are spawned.
    unsafe {
        let _ = zephyr::set_logger();
    }

    info!("Booting remote_switch (Rust on Zephyr)");
    let switch_pin = init_switch_pin();
    init_remote_control();

    // SAFETY: Called once from thread context after the remote key and replay counter are
    // initialized, so the cellular thread never observes a half-initialized handler.
    let err = unsafe { cellular_start() };
    if err != 0 {
        warn!("Cellular link not started ({err}); remote control unavailable");
    }

    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.spawn(switch_controller(switch_pin).unwrap());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, PoisonError};

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

            // Forged, stale, and null inputs never reach the switch.
            let mut forged = request(RemoteCommand::Off, fresh + 1);
            forged[0] = b'x';
            assert_eq!(call_ffi(&forged), None);
            assert_eq!(
                call_ffi(&request(RemoteCommand::Off, older)),
                Some(reply(ReplyStatus::Stale, fresh))
            );
            let mut buf = [0u8; 8];
            // SAFETY: Null pointers are rejected before any dereference.
            assert_eq!(
                unsafe { rust_handle_remote_message(core::ptr::null(), 4, buf.as_mut_ptr(), 8) },
                0
            );
            assert!(COMMAND_CHANNEL.try_receive().is_err());
            publish_switch_state(SwitchState::Off);
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
}
