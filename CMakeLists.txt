# SPDX-License-Identifier: MIT

cmake_minimum_required(VERSION 3.21.0)

# Merge the git-ignored remote-control key (README "Setup") into the
# application configuration when present. Zephyr combines this with the
# sysbuild-provided EXTRA_CONF_FILE (sysbuild/remote_switch.conf).
# CONFIGURE_DEPENDS makes Ninja re-run CMake if the key file is created or
# deleted after an initial build.
set(REMOTE_SWITCH_LOCAL_CONF ${CMAKE_CURRENT_LIST_DIR}/remote_switch.local.conf)
file(GLOB REMOTE_SWITCH_LOCAL_CONF_MATCH CONFIGURE_DEPENDS "${REMOTE_SWITCH_LOCAL_CONF}")
if(REMOTE_SWITCH_LOCAL_CONF_MATCH)
  message(STATUS "remote_switch: using ${REMOTE_SWITCH_LOCAL_CONF}")
  list(APPEND EXTRA_CONF_FILE ${REMOTE_SWITCH_LOCAL_CONF})
else()
  message(WARNING "remote_switch: ${REMOTE_SWITCH_LOCAL_CONF} not found; "
                  "remote control will be disabled (see README \"Setup\")")
endif()

find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(remote_switch)

rust_cargo_application()
target_sources(app PRIVATE src/board_io.c src/cellular.c)
target_sources_ifdef(CONFIG_REMOTE_SWITCH_DEBUG_SHELL app PRIVATE src/remote_shell.c)
set_source_files_properties(
  src/board_io.c
  src/cellular.c
  src/remote_shell.c
  PROPERTIES COMPILE_OPTIONS "-Wall;-Wextra;-Wmissing-prototypes"
)
