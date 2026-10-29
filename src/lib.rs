// SPDX-License-Identifier: MIT

#![cfg_attr(target_os = "none", no_std)]
#![allow(unexpected_cfgs)]

use core::ffi::c_int;
#[cfg(not(target_os = "none"))]
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
#[cfg(target_os = "none")]
use log::{info, warn};
#[cfg(target_os = "none")]
use static_cell::StaticCell;
#[cfg(target_os = "none")]
use zephyr::device::gpio::GpioPin;
#[cfg(target_os = "none")]
use zephyr::embassy::Executor;
#[cfg(all(target_os = "none", dt = "aliases::switch0"))]
use zephyr::raw::ZR_GPIO_OUTPUT_INACTIVE;
#[cfg(target_os = "none")]
use zephyr::sync::atomic::{AtomicU32, Ordering};
#[cfg(target_os = "none")]
use zephyr_panic as _;

/// Capacity of the lock-free command queue shared between C callbacks and the
/// async `switch_controller` task.
pub const COMMAND_QUEUE_CAPACITY: usize = 8;

#[cfg(target_os = "none")]
extern "C" {
    fn board_status_led_set(on: bool);
    fn board_confirm_image();
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

/// Entry point for authenticated remote radio transports (MQTTS / HTTPS / CoAP-DTLS / DECT NR+).
///
/// # Security Preconditions
/// Callers **must** authenticate the remote peer, authorize the operation, and enforce
/// replay protection before calling this function to actuate physical switch hardware.
/// Never wire unauthenticated or cleartext transports (such as SMS) to this hook.
///
/// # ABI Safety & Concurrency
/// Accepts `c_int` (`0` = `OFF`, non-zero = `ON`) rather than raw Rust `bool` across
/// `extern "C"` so no integer bit-pattern from C can cause Undefined Behavior.
/// Lock-free, allocation-free, and ISR-safe. Returns `true` if queued, `false` if full.
#[no_mangle]
pub extern "C" fn rust_on_remote_command(turn_on: c_int) -> bool {
    enqueue_command(SwitchCommand::Set(turn_on != 0))
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
        info!("Switch state -> OFF");
        info!("Ready: press button to toggle `switch0` output");
    } else {
        set_status_led(false);
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
                info!("Switch state -> {}", if new_state { "ON" } else { "OFF" });
            }
            StateTransition::Unchanged(_) => {}
            StateTransition::Unavailable => {
                warn!("Ignoring {cmd:?}: `switch0` output is unavailable");
            }
        }
    }
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
    fn ffi_button_and_remote_commands_normalize_all_c_int_values() {
        with_isolated_queue(|| {
            assert!(rust_on_local_button_press());
            assert!(rust_on_remote_command(0));
            assert!(rust_on_remote_command(1));
            assert!(rust_on_remote_command(2));
            assert!(rust_on_remote_command(0x100));
            assert!(rust_on_remote_command(-1));
            assert!(rust_on_remote_command(c_int::MAX));
            assert!(rust_on_remote_command(c_int::MIN));

            let expected = [
                SwitchCommand::Toggle,
                SwitchCommand::Set(false),
                SwitchCommand::Set(true),
                SwitchCommand::Set(true),
                SwitchCommand::Set(true),
                SwitchCommand::Set(true),
                SwitchCommand::Set(true),
                SwitchCommand::Set(true),
            ];
            for cmd in expected {
                assert_eq!(COMMAND_CHANNEL.try_receive().unwrap(), cmd);
            }
            assert!(COMMAND_CHANNEL.try_receive().is_err());
            assert_eq!(take_dropped_commands(), 0);
        });
    }

    #[test]
    fn queue_overflow_tracks_drops_preserves_fifo_and_recovers() {
        with_isolated_queue(|| {
            for i in 0..COMMAND_QUEUE_CAPACITY {
                if i % 2 == 0 {
                    assert!(rust_on_local_button_press());
                } else {
                    assert!(rust_on_remote_command(1));
                }
            }
            assert_eq!(take_dropped_commands(), 0);

            assert!(!rust_on_local_button_press());
            assert!(!rust_on_remote_command(0));
            assert!(!rust_on_remote_command(42));
            assert_eq!(take_dropped_commands(), 3);
            assert_eq!(take_dropped_commands(), 0);

            // Drain one slot and confirm the queue immediately accepts a new command.
            assert_eq!(
                COMMAND_CHANNEL.try_receive().unwrap(),
                SwitchCommand::Toggle
            );
            assert!(rust_on_remote_command(0));
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
