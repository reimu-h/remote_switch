<!-- SPDX-License-Identifier: MIT -->

# Remote Switch (`remote_switch`)

Zephyr RTOS + Rust (`zephyr-lang-rust` / Embassy) firmware application for driving a physical relay or MOSFET switch output on the **Circuit Dojo nRF9151 Feather** (`circuitdojo_feather_nrf9151/nrf9151/ns`) with Trusted Firmware-M (TF-M) and MCUboot sysbuild support.

## Overview

- **Async Rust Switch Controller (`src/lib.rs`)**: Runs an Embassy executor task (`switch_controller`) on Zephyr that owns the `switch0` GPIO pin, confirms MCUboot test-swap images after safe `OFF` initialization, and processes `SwitchCommand` events (`Toggle`, `Set(bool)`) from a bounded lock-free `Channel<CriticalSectionRawMutex, SwitchCommand, 8>` with atomic dropped-command accounting.
- **C Board I/O Shim (`src/board_io.h`, `src/board_io.c`)**: Bridges debounced local button events (`aliases::sw0` via Zephyr's `input` subsystem) into Rust, drives the onboard nPM1300 PMIC host status LED (`npm1300_leds` channel 2 over I2C), and provides runtime MCUboot image confirmation (`board_confirm_image()`).
- **Non-Secure Partitioning & Bootloader (`sysbuild.conf`, `sysbuild/mcuboot.conf`, `sysbuild/remote_switch.conf`)**: Builds with Zephyr Sysbuild, pairing MCUboot (`64 KB`) with TF-M secure firmware (`256 KB`) and the non-secure Rust/C application (`192 KB` slot, with MCUboot DFU image confirmation configured for sysbuild targets).

## Hardware & Pin Mapping

Default peripheral assignments on `circuitdojo_feather_nrf9151/nrf9151/ns`:

| Function | Devicetree Node / Alias | Default Pin / Bus | Notes |
| --- | --- | --- | --- |
| Switch actuator output | `aliases::switch0` (`&switch_out`) | Header **`D2`** (`P0.26`, `GPIO_ACTIVE_HIGH`) | Configured in `boards/circuitdojo_feather_nrf9151_ns.overlay` |
| Local toggle button | `aliases::sw0` (`&button0`) | Onboard **`MODE`** button (`P0.03`) | Debounced `gpio-keys` input (`INPUT_KEY_0`) |
| Status indicator LED | `npm1300_leds` (channel `2`) | `i2c2` (`P0.02` SDA / `P0.01` SCL) | nPM1300 PMIC host-controlled blue LED |
| Console & MCUmgr UART | `zephyr,console` (`&uart0`) | `P0.11` TX / `P0.10` RX (`115200 8N1`) | Serial logs and MCUboot serial recovery |

### Hardware Fail-Safe & Load Isolation

> **Important:** nRF9151 GPIO pins remain in a high-impedance (floating) state during power-on reset, MCUboot execution, TF-M startup, and early kernel initialization until `switch_controller` configures `switch0` as `ZR_GPIO_OUTPUT_INACTIVE`.
>
> - **External Pull Resistor:** Always fit an external pull-down resistor (for example, `10 kOhm` to `GND` for `GPIO_ACTIVE_HIGH` MOSFET/relay stages, or a pull-up resistor to `VDD` for `GPIO_ACTIVE_LOW` stages) on the actuator control input so the switched load cannot glitch `ON` during reset or firmware updates.
> - **Driver Stage:** Do not drive inductive relay coils directly from an nRF9151 GPIO pin. Use a logic-level MOSFET or transistor buffer, flyback diode, and appropriate galvanic isolation rated for the target load.

### Customizing the Switch Output Pin

Edit `boards/circuitdojo_feather_nrf9151_ns.overlay` (or pass an extra gitignored overlay via `-- -DEXTRA_DTC_OVERLAY_FILE=boards/circuitdojo_feather_nrf9151_ns.local.overlay`, or replace the default board overlay via `-- -DDTC_OVERLAY_FILE=boards/circuitdojo_feather_nrf9151_ns.local.overlay`) to change `&gpio0 26` or switch between `GPIO_ACTIVE_HIGH` and `GPIO_ACTIVE_LOW`.

The `outputs` node uses `compatible = "gpio-leds"` so `zephyr-lang-rust` (`dt-rust.yaml`) generates a typed `GpioPin` accessor for `aliases::switch0`. `CONFIG_LED_GPIO=n` is set in `boards/circuitdojo_feather_nrf9151_ns.conf` so Zephyr's C `led_gpio` driver does not contend with Rust for ownership of the pin.

## Prerequisites

1. **Zephyr Dependencies & SDK**:
   - Python 3.10+, `west` (`>= 1.2`), CMake (`>= 3.21`), Ninja, and DeviceTree Compiler (`dtc`).
   - [Zephyr SDK](https://docs.zephyrproject.org/latest/develop/toolchains/zephyr_sdk.html) with the `arm-zephyr-eabi` toolchain installed (`ZEPHYR_SDK_INSTALL_DIR` configured if installed in a non-default location).
2. **Rust Toolchain (`>= 1.85`)**:
   ```sh
   rustup toolchain install stable
   rustup target add thumbv8m.main-none-eabi
   rustup component add clippy rustfmt
   ```

## Workspace Setup

`west.yml` pins both Circuit Dojo's Feather SDK manifest (`nfed` at `79ee909971558cbea852dd668134be6ef73adfe7`) and `zephyr-lang-rust` (`6823b690ed2daf636323cbb8a10447d142752e1f`) to immutable commit hashes for reproducible builds.

### Option A: Initialize from a Remote Git URL

```sh
west init -m <repo-url> remote-switch-workspace
cd remote-switch-workspace
west update
```

### Option B: Initialize from a Local Checkout of `remote_switch`

If you already have the `remote_switch` directory locally, place it inside an empty parent workspace directory and initialize `west` against it:

```sh
mkdir remote-switch-workspace
mv remote_switch remote-switch-workspace/remote_switch
cd remote-switch-workspace
west init -l remote_switch
west update
```

## Building, Testing & Flashing

Run all `west` commands from the parent `remote-switch-workspace` directory.

### Build Firmware (Sysbuild + Clippy)

```sh
west build -b circuitdojo_feather_nrf9151/nrf9151/ns -p -d build --sysbuild remote_switch
```

Every Zephyr build compiles MCUboot, TF-M, C sources, and the Rust static library (`librustapp.a`), and automatically enforces `cargo clippy -- -D warnings -D clippy::undocumented_unsafe_blocks`.

> **Tip:** Passing an external build directory (for example, `-d /tmp/remote-switch-build`) keeps `remote_switch/` free of generated CMake/Cargo build artifacts when preparing release archives.

### Run Host Unit Tests & Format Checks

Pure state-machine, bounded-queue, drop-counter, and FFI boundary tests run directly on the host target without hardware:

```sh
cargo test --manifest-path remote_switch/Cargo.toml
cargo fmt --manifest-path remote_switch/Cargo.toml -- --check
```

If dependencies are already cached locally, append `--offline` to `cargo test`.

### Validate with Zephyr Twister

```sh
west twister -T remote_switch --build-only -O twister-out
```

### Flash & Monitor Serial Console

Flash the merged sysbuild image over SWD (`probe-rs`, J-Link, or CMSIS-DAP):

```sh
west flash -d build --runner probe-rs
```

Connect a serial terminal to `uart0` at `115200` baud (`8N1`) and press the onboard **MODE** button to toggle `switch0`.

### Optional IDE (`rust-analyzer`) Configuration

During `west build`, `zephyr-lang-rust` generates `<build-dir>/remote_switch/rust/sample-cargo-config.toml`. For local `rust-analyzer` integration, you may copy it to `remote_switch/.cargo/config.toml` (ignored by `.gitignore`). Note that this generated file embeds machine-local absolute paths and should be deleted before packaging source tarballs.

### Path Remapping for Redistributable Binaries

Zephyr strips C `__FILE__` prefixes from loadable binary sections by default (`CONFIG_BUILD_OUTPUT_STRIP_PATHS=y`), whereas Rust panic metadata inside `.bin`, `.hex`, and `dfu_application.zip` embeds workspace and Cargo registry paths unless remapped. Before publishing prebuilt flash or DFU images (`dfu_application.zip`, `zephyr.signed.hex`, `merged.hex`, `zephyr.bin`), remap Rust host paths with `--remap-path-prefix`:

```sh
RUSTFLAGS="--remap-path-prefix=$(pwd)=/workspace --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo" \
  west build -b circuitdojo_feather_nrf9151/nrf9151/ns -p -d build --sysbuild remote_switch
```

(If your workspace or `CARGO_HOME` path contains spaces, pass `0x1f`-separated flags via `CARGO_ENCODED_RUSTFLAGS` instead of space-separated `RUSTFLAGS`.)

> **Note on ELF Debug Sections:** Distribute flashable/DFU payloads (`dfu_application.zip`, `.hex`, `.bin`) rather than unstripped `.elf` outputs (`zephyr.elf`, `tfm_s.elf`). Because `RUSTFLAGS` applies only to `rustc`, unstripped `.elf` files still retain C/ASM compiler and SDK toolchain DWARF debug paths (`.debug_line`, `.debug_str`). If you need to archive or share an `.elf` file, strip DWARF debug sections with `arm-zephyr-eabi-strip --strip-debug` (or pass C `-ffile-prefix-map` flags via `EXTRA_CPPFLAGS` for Zephyr images; note that TF-M's `tfm_s.elf` builds through its own ExternalProject toolchain and still requires `--strip-debug`).

## Security Considerations for Production Deployment

1. **Replace the Default MCUboot Signing Key (`SB_CONFIG_BOOT_SIGNATURE_KEY_FILE`)**:
   - Upstream MCUboot defaults to a publicly known debug key (`root-ec-p256.pem`). Because UART0 MCUmgr serial recovery is available on the board, **never deploy firmware signed with the default key**.
   - Generate a dedicated ECDSA-P256 private key at an **absolute path outside the repository and `west` workspace** (`*.pem` and `*.key` in `remote_switch/.gitignore` only guard `remote_switch/`, not the parent workspace, and MCUboot vs. `west sign` resolve relative paths against different base directories). Because `BOOT_SIGNATURE_KEY_FILE` is a Kconfig `string` symbol written into `extra_kconfig_options.conf`, **escape the inner quotes** (`\"...\"`) on the command line (matching `sysbuild.conf`) so your shell preserves literal quotes for Kconfig:
     ```sh
     imgtool keygen -k /secure/path/to/prod-key.pem -t ecdsa-p256
     west build -b circuitdojo_feather_nrf9151/nrf9151/ns -p -d build --sysbuild remote_switch \
       -- -DSB_CONFIG_BOOT_SIGNATURE_KEY_FILE=\"/secure/path/to/prod-key.pem\"
     ```
2. **Enable Hardware Debug Port Protection & Anti-Rollback**:
   - Lock SWD debug access (`-DSB_CONFIG_APPROTECT_LOCK=y -DSB_CONFIG_SECURE_APPROTECT_LOCK=y`) and enable anti-rollback protection: either MCUboot software version anti-rollback (`-Dmcuboot_CONFIG_MCUBOOT_DOWNGRADE_PREVENTION=y` or in `sysbuild/mcuboot.conf`) on the default static Devicetree partition layout, or hardware monotonic-counter downgrade prevention (`-DSB_CONFIG_PARTITION_MANAGER=y -DSB_CONFIG_MCUBOOT_HARDWARE_DOWNGRADE_PREVENTION=y` together with a static `pm_static.yml` and NSIB key, as `circuitdojo_feather_nrf9151` defaults `SB_CONFIG_PARTITION_MANAGER=n`).
3. **Authenticate, Encrypt & Replay-Protect Remote Commands**:
   - `rust_on_remote_command(int turn_on)` (declared in `src/board_io.h`) is an in-firmware actuation entry point only; it performs **no** network authentication or authorization itself.
   - **Never** connect unauthenticated or spoofable cleartext transports (such as raw SMS or unencrypted radio frames) to `rust_on_remote_command`.
   - Any remote radio integration (LTE-M / NB-IoT via `MQTTS`, `HTTPS`, or `CoAP/DTLS` backed by credentials in the nRF9151 modem / TF-M Protected Storage, or authenticated `DECT NR+`) **must** enforce mutual peer authentication, transport encryption, command authorization, and monotonic sequence/timestamp replay protection prior to invoking `rust_on_remote_command`.
4. **Fault Recovery & Watchdog Supervision**:
   - `CONFIG_RESET_ON_FATAL_ERROR=y` resets the SoC immediately upon any kernel fault or Rust panic so `switch0` returns to its inactive (`OFF`) state rather than remaining latched `ON`, and `[profile.release] overflow-checks = true` guards against silent integer wraparound. When integrating network/modem stacks, enable the hardware watchdog (`wdt0` / `CONFIG_WATCHDOG=y`) as an additional liveness backstop.

## License

Licensed under the [MIT License](LICENSE) (`MIT`).
