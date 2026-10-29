# SPDX-License-Identifier: MIT

cmake_minimum_required(VERSION 3.21.0)
find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(remote_switch)

rust_cargo_application()
target_sources(app PRIVATE src/board_io.c)
set_source_files_properties(
  src/board_io.c
  PROPERTIES COMPILE_OPTIONS "-Wall;-Wextra;-Wmissing-prototypes"
)
