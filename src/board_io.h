/* SPDX-License-Identifier: MIT */
/*
 * C/Rust FFI boundary declarations for board-level status LED, local button
 * input, MCUboot image confirmation, and authenticated remote command hooks.
 */

#ifndef REMOTE_SWITCH_BOARD_IO_H_
#define REMOTE_SWITCH_BOARD_IO_H_

#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * @brief Update the onboard host-controlled status LED.
 *
 * Must be called from thread context only because the underlying PMIC LED
 * driver communicates over blocking I2C. Calls from ISR context are rejected
 * safely without touching the bus.
 *
 * @param on true to turn the status LED on, false to turn it off.
 */
void board_status_led_set(bool on);

/**
 * @brief Mark the running MCUboot slot image as confirmed if unconfirmed.
 *
 * Must be called from thread context after application initialization has
 * placed the switch output into its known-safe state. Safe no-op when
 * CONFIG_MCUBOOT_IMG_MANAGER is disabled or the image is already confirmed.
 */
void board_confirm_image(void);

/**
 * @brief Enqueue a local toggle command from the debounced button callback.
 *
 * Lock-free, heap-allocation-free, and safe to invoke from thread or ISR
 * context.
 *
 * @return true if enqueued, false if dropped because the command queue is full.
 */
bool rust_on_local_button_press(void);

/**
 * @brief Enqueue a remote switch state command from an authenticated transport.
 *
 * SECURITY PRECONDITION: Callers MUST authenticate, authorize, integrity-check,
 * and replay-verify the command over an encrypted channel (such as MQTTS,
 * HTTPS, CoAP/DTLS, or authenticated DECT NR+) BEFORE calling this function.
 * Never wire unauthenticated or cleartext transports to this entry point.
 *
 * Accepts an integer rather than C _Bool so arbitrary C callers cannot pass an
 * invalid Rust bool discriminant across the FFI boundary. Lock-free,
 * heap-allocation-free, and safe in thread or ISR context.
 *
 * @param turn_on Non-zero to command the switch ON, 0 to command it OFF.
 * @return true if enqueued, false if dropped because the command queue is full.
 */
bool rust_on_remote_command(int turn_on);

#ifdef __cplusplus
}
#endif

#endif /* REMOTE_SWITCH_BOARD_IO_H_ */
