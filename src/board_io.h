/* SPDX-License-Identifier: MIT */
/*
 * C/Rust FFI boundary declarations for board-level status LED, local button
 * input, MCUboot image confirmation, replay-counter storage, and the
 * authenticated remote message handler.
 */

#ifndef REMOTE_SWITCH_BOARD_IO_H_
#define REMOTE_SWITCH_BOARD_IO_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Maximum length of a remote message or reply in bytes. Must match
 * `protocol::MAX_MESSAGE_LEN` in `src/protocol.rs`.
 */
#define REMOTE_MESSAGE_MAX_LEN 64

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
 * @brief Load the last accepted remote-command counter from flash.
 *
 * Thread context only. Initializes the settings subsystem on first use.
 *
 * @param value Receives the stored counter.
 * @return true if a counter was found, false if none is stored or storage is
 *         unavailable (value is left untouched).
 */
bool board_counter_load(uint64_t *value);

/**
 * @brief Persist the last accepted remote-command counter to flash.
 *
 * Thread context only. Writes are serialized and monotonic: values that are not
 * larger than the last stored counter are ignored, so concurrent transports can
 * never move the stored counter backwards.
 *
 * @param value Counter to store.
 * @return 0 on success (or when value is not newer), negative errno otherwise.
 */
int board_counter_store(uint64_t value);

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
 * @brief Authenticate and apply one remote message from any transport.
 *
 * The message must be an authenticated request ("<cmd> <counter> <tag>", see
 * `src/protocol.rs`). Unauthenticated, malformed, or oversized messages are
 * rejected and produce no reply, so transports must stay silent when this
 * returns 0. Thread context only (may write the replay counter to flash).
 *
 * @param message Received bytes (not NUL-terminated).
 * @param message_len Number of received bytes.
 * @param reply Buffer for the authenticated reply.
 * @param reply_capacity Size of @p reply; REMOTE_MESSAGE_MAX_LEN is enough.
 * @return Number of reply bytes to send back to the sender, or 0 to drop.
 */
int rust_handle_remote_message(const uint8_t *message, size_t message_len, uint8_t *reply,
			       size_t reply_capacity);

#ifdef __cplusplus
}
#endif

#endif /* REMOTE_SWITCH_BOARD_IO_H_ */
