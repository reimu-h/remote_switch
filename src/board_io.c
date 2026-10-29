// SPDX-License-Identifier: MIT
/*
 * Thin C bridge for Zephyr/Nordic subsystems not yet wrapped in
 * zephyr-lang-rust:
 *   1. Onboard nPM1300 status LED (channel 2, host-controlled blue LED).
 *   2. Debounced button events via Zephyr's gpio-keys input subsystem.
 *   3. MCUboot runtime image confirmation after safe application startup.
 */

#include "board_io.h"

#include <stdbool.h>

#include <zephyr/device.h>
#include <zephyr/devicetree.h>
#include <zephyr/kernel.h>
#include <zephyr/logging/log.h>
#include <zephyr/sys/util.h>

#if IS_ENABLED(CONFIG_LED)
#include <zephyr/drivers/led.h>
#endif

#if IS_ENABLED(CONFIG_INPUT)
#include <zephyr/input/input.h>
#endif

#if IS_ENABLED(CONFIG_MCUBOOT_IMG_MANAGER)
#include <zephyr/dfu/mcuboot.h>
#endif

LOG_MODULE_REGISTER(board_io, LOG_LEVEL_INF);

#define HAS_NPM1300_STATUS_LED                                                                     \
	(DT_NODE_HAS_STATUS_OKAY(DT_NODELABEL(npm1300_leds)) && IS_ENABLED(CONFIG_LED))

#define HAS_SW0_INPUT                                                                              \
	(DT_NODE_HAS_STATUS_OKAY(DT_ALIAS(sw0)) &&                                                 \
	 DT_NODE_HAS_STATUS_OKAY(DT_PARENT(DT_ALIAS(sw0))) &&                                      \
	 DT_NODE_HAS_PROP(DT_ALIAS(sw0), zephyr_code) && IS_ENABLED(CONFIG_INPUT))

#if HAS_NPM1300_STATUS_LED
#define NPM1300_HOST_LED_CHANNEL 2U
static const struct device *const status_leds = DEVICE_DT_GET(DT_NODELABEL(npm1300_leds));
#endif

void board_status_led_set(bool on)
{
	if (k_is_in_isr()) {
		LOG_ERR("%s cannot be called from ISR context", __func__);
		return;
	}

#if HAS_NPM1300_STATUS_LED
	int err;

	if (!device_is_ready(status_leds)) {
		LOG_WRN("nPM1300 LED device not ready");
		return;
	}

	err = on ? led_on(status_leds, NPM1300_HOST_LED_CHANNEL)
		 : led_off(status_leds, NPM1300_HOST_LED_CHANNEL);
	if (err < 0) {
		LOG_WRN("Failed to update status LED: %d", err);
	}
#else
	ARG_UNUSED(on);
#endif
}

void board_confirm_image(void)
{
	if (k_is_in_isr()) {
		LOG_ERR("%s cannot be called from ISR context", __func__);
		return;
	}

#if IS_ENABLED(CONFIG_MCUBOOT_IMG_MANAGER)
	if (!boot_is_img_confirmed()) {
		int err = boot_write_img_confirmed();

		if (err < 0) {
			LOG_ERR("Failed to confirm MCUboot image: %d", err);
		} else {
			LOG_INF("MCUboot application image confirmed");
		}
	}
#endif
}

#if HAS_SW0_INPUT
static void on_button_input(struct input_event *evt, void *user_data)
{
	ARG_UNUSED(user_data);

	if (evt->type == INPUT_EV_KEY && evt->code == DT_PROP(DT_ALIAS(sw0), zephyr_code) &&
	    evt->value == 1) {
		(void)rust_on_local_button_press();
	}
}

INPUT_CALLBACK_DEFINE(DEVICE_DT_GET(DT_PARENT(DT_ALIAS(sw0))), on_button_input, NULL);
#endif
