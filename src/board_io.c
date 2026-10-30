// SPDX-License-Identifier: MIT
/*
 * Thin C bridge for Zephyr/Nordic subsystems not yet wrapped in (or whose error
 * codes are discarded by) zephyr-lang-rust:
 *   1. Checked `aliases::switch0` GPIO output with early POST_KERNEL safe OFF.
 *   2. Onboard nPM1300 status LED (channel 2, host-controlled blue LED).
 *   3. Debounced button events via Zephyr's gpio-keys input subsystem.
 *   4. Hardware watchdog (`aliases::watchdog0`) + periodic tick timer.
 *   5. MCUboot runtime image confirmation after safe application startup.
 *   6. Flash persistence of the remote-command replay counter (settings/NVS).
 */

#include "board_io.h"

#include <errno.h>
#include <stdbool.h>
#include <stdint.h>

#include <zephyr/device.h>
#include <zephyr/devicetree.h>
#include <zephyr/init.h>
#include <zephyr/kernel.h>
#include <zephyr/logging/log.h>
#include <zephyr/sys/util.h>

#if IS_ENABLED(CONFIG_GPIO)
#include <zephyr/drivers/gpio.h>
#endif

#if IS_ENABLED(CONFIG_LED)
#include <zephyr/drivers/led.h>
#endif

#if IS_ENABLED(CONFIG_INPUT)
#include <zephyr/input/input.h>
#endif

#if IS_ENABLED(CONFIG_MCUBOOT_IMG_MANAGER)
#include <zephyr/dfu/mcuboot.h>
#endif

#if IS_ENABLED(CONFIG_REMOTE_SWITCH_WATCHDOG)
#include <zephyr/drivers/watchdog.h>
#endif

#if IS_ENABLED(CONFIG_SETTINGS)
#include <zephyr/settings/settings.h>
#endif

LOG_MODULE_REGISTER(board_io, LOG_LEVEL_INF);

#define HAS_SWITCH0_GPIO                                                                           \
	(DT_NODE_HAS_STATUS_OKAY(DT_ALIAS(switch0)) &&                                             \
	 DT_NODE_HAS_PROP(DT_ALIAS(switch0), gpios) && IS_ENABLED(CONFIG_GPIO))

#define HAS_NPM1300_STATUS_LED                                                                     \
	(DT_NODE_HAS_STATUS_OKAY(DT_NODELABEL(npm1300_leds)) && IS_ENABLED(CONFIG_LED))

#define HAS_SW0_INPUT                                                                              \
	(DT_NODE_HAS_STATUS_OKAY(DT_ALIAS(sw0)) &&                                                 \
	 DT_NODE_HAS_STATUS_OKAY(DT_PARENT(DT_ALIAS(sw0))) &&                                      \
	 DT_NODE_HAS_PROP(DT_ALIAS(sw0), zephyr_code) && IS_ENABLED(CONFIG_INPUT))

#define HAS_WATCHDOG0                                                                              \
	(DT_NODE_HAS_STATUS_OKAY(DT_ALIAS(watchdog0)) && IS_ENABLED(CONFIG_REMOTE_SWITCH_WATCHDOG))

#if HAS_SWITCH0_GPIO
static const struct gpio_dt_spec switch0_gpio = GPIO_DT_SPEC_GET(DT_ALIAS(switch0), gpios);
static atomic_t switch0_ready;

static bool configure_switch0_inactive(void)
{
	int err;

	if (!gpio_is_ready_dt(&switch0_gpio)) {
		LOG_ERR("Switch GPIO controller not ready");
		return false;
	}
	err = gpio_pin_configure_dt(&switch0_gpio, GPIO_OUTPUT_INACTIVE);
	if (err < 0) {
		LOG_ERR("Failed to configure switch0 inactive: %d", err);
		return false;
	}
	atomic_set(&switch0_ready, 1);
	return true;
}

/*
 * Drive `switch0` inactive during POST_KERNEL initialization so the pin does
 * not stay high-impedance while APPLICATION-level board hooks (such as nfed's
 * 2 s regulator settle sleep) execute before `rust_main`.
 */
static int board_switch_early_init(void)
{
	return configure_switch0_inactive() ? 0 : -EIO;
}

SYS_INIT(board_switch_early_init, POST_KERNEL, CONFIG_APPLICATION_INIT_PRIORITY);
#endif /* HAS_SWITCH0_GPIO */

bool board_switch_init(void)
{
	if (k_is_in_isr()) {
		LOG_ERR("%s cannot be called from ISR context", __func__);
		return false;
	}

#if HAS_SWITCH0_GPIO
	if (atomic_get(&switch0_ready)) {
		int err = gpio_pin_set_dt(&switch0_gpio, 0);

		if (err == 0) {
			return true;
		}
		LOG_ERR("Failed to hold switch0 inactive: %d", err);
		atomic_set(&switch0_ready, 0);
		return false;
	}
	return configure_switch0_inactive();
#else
	LOG_WRN("Devicetree alias `switch0` is not defined");
	return false;
#endif
}

int board_switch_set(bool on)
{
	if (k_is_in_isr()) {
		LOG_ERR("%s cannot be called from ISR context", __func__);
		return -EWOULDBLOCK;
	}

#if HAS_SWITCH0_GPIO
	int err;

	if (!atomic_get(&switch0_ready)) {
		return -ENODEV;
	}
	err = gpio_pin_set_dt(&switch0_gpio, on ? 1 : 0);
	if (err < 0) {
		LOG_ERR("Failed to drive switch0 %s: %d", on ? "ON" : "OFF", err);
		(void)gpio_pin_configure_dt(&switch0_gpio, GPIO_DISCONNECTED);
		atomic_set(&switch0_ready, 0);
	}
	return err;
#else
	ARG_UNUSED(on);
	return -ENOTSUP;
#endif
}

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

#if HAS_WATCHDOG0
static const struct device *const watchdog_dev = DEVICE_DT_GET(DT_ALIAS(watchdog0));
static int watchdog_channel = -1;
static atomic_t watchdog_started;

static void on_watchdog_timer(struct k_timer *timer)
{
	ARG_UNUSED(timer);
	rust_on_watchdog_tick();
}

static K_TIMER_DEFINE(watchdog_timer, on_watchdog_timer, NULL);

int board_watchdog_start(void)
{
	const struct wdt_timeout_cfg cfg = {
		.window =
			{
				.min = 0U,
				.max = CONFIG_REMOTE_SWITCH_WATCHDOG_TIMEOUT_MS,
			},
		.callback = NULL,
		.flags = WDT_FLAG_RESET_SOC,
	};
	const k_timeout_t feed_period = K_MSEC(CONFIG_REMOTE_SWITCH_WATCHDOG_TIMEOUT_MS / 3);
	int channel;
	int err;

	if (k_is_in_isr()) {
		LOG_ERR("%s cannot be called from ISR context", __func__);
		return -EWOULDBLOCK;
	}
	if (!atomic_cas(&watchdog_started, 0, 1)) {
		return -EALREADY;
	}
	if (!device_is_ready(watchdog_dev)) {
		LOG_ERR("Watchdog device not ready");
		atomic_set(&watchdog_started, 0);
		return -ENODEV;
	}

	channel = wdt_install_timeout(watchdog_dev, &cfg);
	if (channel < 0) {
		LOG_ERR("Failed to install watchdog timeout: %d", channel);
		atomic_set(&watchdog_started, 0);
		return channel;
	}
	err = wdt_setup(watchdog_dev, WDT_OPT_PAUSE_HALTED_BY_DBG);
	if (err < 0) {
		LOG_ERR("Failed to start watchdog: %d", err);
		atomic_set(&watchdog_started, 0);
		return err;
	}

	watchdog_channel = channel;
	(void)wdt_feed(watchdog_dev, watchdog_channel);
	k_timer_start(&watchdog_timer, feed_period, feed_period);
	LOG_INF("Hardware watchdog active (%d ms timeout)",
		CONFIG_REMOTE_SWITCH_WATCHDOG_TIMEOUT_MS);
	return 0;
}

void board_watchdog_feed(void)
{
	if (k_is_in_isr()) {
		LOG_ERR("%s cannot be called from ISR context", __func__);
		return;
	}
	if (watchdog_channel >= 0) {
		int err = wdt_feed(watchdog_dev, watchdog_channel);

		if (err < 0) {
			LOG_WRN("Failed to feed watchdog: %d", err);
		}
	}
}
#else
int board_watchdog_start(void)
{
	if (k_is_in_isr()) {
		LOG_ERR("%s cannot be called from ISR context", __func__);
		return -EWOULDBLOCK;
	}
	return 0;
}

void board_watchdog_feed(void)
{
}
#endif /* HAS_WATCHDOG0 */

#if IS_ENABLED(CONFIG_SETTINGS)
#define COUNTER_SETTINGS_KEY "remote_switch/ctr"

/* Serializes flash writes and guards `stored_counter`/`settings_ready`. */
static K_MUTEX_DEFINE(counter_lock);
static uint64_t stored_counter;
static bool settings_ready;

static int counter_settings_init_locked(void)
{
	if (!settings_ready) {
		int err = settings_subsys_init();

		if (err) {
			LOG_ERR("Settings init failed: %d", err);
			return err;
		}
		settings_ready = true;
	}
	return 0;
}

static int counter_load_cb(const char *key, size_t len, settings_read_cb read_cb, void *cb_arg,
			   void *param)
{
	uint64_t *out = param;

	/* Only accept the exact key (no sub-keys) with the expected size. */
	if ((key != NULL && key[0] != '\0') || len != sizeof(*out)) {
		return 0;
	}
	return read_cb(cb_arg, out, sizeof(*out)) == sizeof(*out) ? 1 : 0;
}

bool board_counter_load(uint64_t *value)
{
	uint64_t loaded = 0;
	bool found = false;

	if (value == NULL || k_is_in_isr()) {
		return false;
	}

	k_mutex_lock(&counter_lock, K_FOREVER);
	if (counter_settings_init_locked() == 0 &&
	    settings_load_subtree_direct(COUNTER_SETTINGS_KEY, counter_load_cb, &loaded) == 0 &&
	    loaded != 0) {
		stored_counter = MAX(stored_counter, loaded);
		*value = loaded;
		found = true;
	}
	k_mutex_unlock(&counter_lock);
	return found;
}

int board_counter_store(uint64_t value)
{
	int err;

	if (k_is_in_isr()) {
		return -EWOULDBLOCK;
	}

	k_mutex_lock(&counter_lock, K_FOREVER);
	err = counter_settings_init_locked();
	if (err == 0 && value > stored_counter) {
		err = settings_save_one(COUNTER_SETTINGS_KEY, &value, sizeof(value));
		if (err == 0) {
			stored_counter = value;
		} else {
			LOG_ERR("Failed to store replay counter: %d", err);
		}
	}
	k_mutex_unlock(&counter_lock);
	return err;
}
#else
bool board_counter_load(uint64_t *value)
{
	ARG_UNUSED(value);
	return false;
}

int board_counter_store(uint64_t value)
{
	ARG_UNUSED(value);
	return -ENOTSUP;
}
#endif /* CONFIG_SETTINGS */

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
