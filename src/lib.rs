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
use embassy_sync::signal::Signal;
#[cfg(target_os = "none")]
use log::info;
use log::warn;
#[cfg(target_os = "none")]
use static_cell::StaticCell;
#[cfg(target_os = "none")]
use zephyr::embassy::Executor;
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
    fn board_switch_init() -> bool;
    fn board_switch_set(on: bool) -> c_int;
    fn board_status_led_set(on: bool);
    fn board_confirm_image();
    fn board_watchdog_start() -> c_int;
    fn board_watchdog_feed();
    fn board_counter_load(value: *mut u64) -> bool;
    fn board_counter_store(value: u64) -> c_int;
    fn cellular_start() -> c_int;
    fn cellular_is_healthy() -> bool;
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

static WATCHDOG_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

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
    DROPPED_COMMANDS.swap(0, Ordering::AcqRel)
}

/// Called from `src/board_io.c` when a debounced local button press occurs.
///
/// Lock-free, allocation-free, and ISR-safe. Returns `true` if queued, `false` if full.
#[no_mangle]
pub extern "C" fn rust_on_local_button_press() -> bool {
    enqueue_command(SwitchCommand::Toggle)
}

/// Called from `src/board_io.c`'s periodic watchdog timer ISR so `switch_controller`
/// wakes on the Embassy executor thread and feeds the hardware watchdog.
///
/// Lock-free, allocation-free, and ISR-safe.
#[no_mangle]
pub extern "C" fn rust_on_watchdog_tick() {
    WATCHDOG_SIGNAL.signal(());
}

const STATE_OFF: u8 = 0;
const STATE_ON: u8 = 1;
const STATE_UNAVAILABLE: u8 = 2;

/// Switch state published for remote status queries. Starts as unavailable until
/// `SwitchOutput::init()` verifies `aliases::switch0` before remote transports start.
static SWITCH_STATE: AtomicU8 = AtomicU8::new(STATE_UNAVAILABLE);

#[cfg(any(target_os = "none", test))]
fn publish_switch_state(state: SwitchState) {
    let raw = match state {
        SwitchState::Off => STATE_OFF,
        SwitchState::On => STATE_ON,
        SwitchState::Unavailable => STATE_UNAVAILABLE,
    };
    SWITCH_STATE.store(raw, Ordering::Release);
}

fn published_switch_state() -> SwitchState {
    match SWITCH_STATE.load(Ordering::Acquire) {
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
struct SwitchOutput {
    available: bool,
}

#[cfg(target_os = "none")]
impl SwitchOutput {
    fn init() -> Self {
        // SAFETY: Invoked once from `rust_main` in Zephyr thread context before any
        // remote transport or executor task runs. `board_switch_init` checks
        // `k_is_in_isr()`, verifies GPIO readiness, and checks the driver return code.
        let available = unsafe { board_switch_init() };
        publish_switch_state(if available {
            SwitchState::Off
        } else {
            SwitchState::Unavailable
        });
        Self { available }
    }

    const fn is_available(&self) -> bool {
        self.available
    }

    fn set(&mut self, on: bool) -> bool {
        if !self.available {
            return false;
        }
        // SAFETY: Invoked only from `switch_controller` in Zephyr thread context.
        // `board_switch_set` checks `k_is_in_isr()` and returns negative errno on failure.
        let err = unsafe { board_switch_set(on) };
        if err < 0 {
            warn!("GPIO write to `switch0` failed ({err}); marking output unavailable");
            self.available = false;
            publish_switch_state(SwitchState::Unavailable);
            false
        } else {
            true
        }
    }
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

#[cfg(target_os = "none")]
fn start_watchdog() {
    // SAFETY: Invoked once from `rust_main` in Zephyr thread context before entering the
    // executor loop. `board_watchdog_start` checks `k_is_in_isr()` and guards single-start
    // initialization with an atomic CAS.
    let err = unsafe { board_watchdog_start() };
    if err < 0 {
        warn!("Hardware watchdog not started ({err})");
    }
}

#[cfg(target_os = "none")]
fn feed_watchdog() {
    // SAFETY: Invoked only from `switch_controller` in Zephyr thread context.
    // `board_watchdog_feed` checks `k_is_in_isr()` and is a no-op if the watchdog is not armed.
    unsafe {
        board_watchdog_feed();
    }
}

#[cfg(target_os = "none")]
fn is_cellular_healthy() -> bool {
    // SAFETY: `cellular_is_healthy` only performs atomic reads of static state and
    // `k_uptime_seconds()`, and is safe to call from any thread context.
    unsafe { cellular_is_healthy() }
}

#[cfg(target_os = "none")]
fn supervise_watchdog() {
    if is_cellular_healthy() {
        feed_watchdog();
    } else {
        warn!("Cellular transport unresponsive; withholding watchdog feed");
    }
}

#[cfg(target_os = "none")]
enum ControllerEvent {
    Command(SwitchCommand),
    WatchdogTick,
}

#[cfg(target_os = "none")]
async fn next_controller_event() -> ControllerEvent {
    use core::future::Future;
    use core::task::Poll;

    let mut wdt = core::pin::pin!(WATCHDOG_SIGNAL.wait());
    let mut cmd = core::pin::pin!(COMMAND_CHANNEL.receive());
    core::future::poll_fn(|cx| {
        if let Poll::Ready(()) = wdt.as_mut().poll(cx) {
            return Poll::Ready(ControllerEvent::WatchdogTick);
        }
        if let Poll::Ready(command) = cmd.as_mut().poll(cx) {
            return Poll::Ready(ControllerEvent::Command(command));
        }
        Poll::Pending
    })
    .await
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
async fn switch_controller(mut switch_output: SwitchOutput) -> ! {
    let mut is_on = false;
    if switch_output.set(false) {
        publish_switch_state(SwitchState::Off);
        set_status_led(false);
        info!("Switch state -> OFF");
        info!("Ready: press button to toggle `switch0` output");
    } else {
        publish_switch_state(SwitchState::Unavailable);
        set_status_led(false);
        warn!("Degraded mode: `switch0` output unavailable; switch forced OFF");
    }
    if should_confirm_boot_image(switch_output.is_available(), cfg!(dt = "aliases::switch0")) {
        confirm_boot_image();
    } else {
        warn!("Skipping MCUboot image confirmation due to unavailable `switch0` output");
    }
    supervise_watchdog();

    loop {
        log_dropped_commands_if_any();
        let cmd = match next_controller_event().await {
            ControllerEvent::WatchdogTick => {
                supervise_watchdog();
                continue;
            }
            ControllerEvent::Command(cmd) => cmd,
        };
        supervise_watchdog();
        log_dropped_commands_if_any();

        match evaluate_command(&mut is_on, switch_output.is_available(), cmd) {
            StateTransition::Changed(new_state) => {
                if switch_output.set(new_state) {
                    publish_switch_state(if new_state {
                        SwitchState::On
                    } else {
                        SwitchState::Off
                    });
                    set_status_led(new_state);
                    info!("Switch state -> {}", if new_state { "ON" } else { "OFF" });
                } else {
                    is_on = false;
                    set_status_led(false);
                }
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
    let switch_output = SwitchOutput::init();
    start_watchdog();
    init_remote_control();

    // SAFETY: Called once from thread context after `SwitchOutput::init()`, the remote key,
    // and the replay counter are initialized, so the cellular thread never observes a
    // half-initialized handler or bogus initial `unavailable` state.
    let err = unsafe { cellular_start() };
    if err != 0 {
        warn!("Cellular link not started ({err}); remote control unavailable");
    }

    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.spawn(switch_controller(switch_output).unwrap());
    });
}

#[cfg(test)]
mod tests;
