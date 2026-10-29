// SPDX-License-Identifier: MIT

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ZEPHYR_DTS");
    println!("cargo:rerun-if-env-changed=BINARY_DIR_INCLUDE_GENERATED");

    if let (Ok(dts), Ok(gen_dir)) = (
        env::var("ZEPHYR_DTS"),
        env::var("BINARY_DIR_INCLUDE_GENERATED"),
    ) {
        println!("cargo:rerun-if-changed={dts}");
        println!("cargo:rerun-if-changed={gen_dir}/devicetree_generated.h");
        // Emits `#[cfg(dt = "...")]` flags for all present Devicetree nodes and aliases.
        zephyr_build::dt_cfgs();
    } else if env::var_os("CARGO_CFG_TARGET_OS").is_some_and(|os| os == "none") {
        panic!(
            "ZEPHYR_DTS and BINARY_DIR_INCLUDE_GENERATED must be set when building for Zephyr (run via `west build`)"
        );
    }
}
