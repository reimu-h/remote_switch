// SPDX-License-Identifier: MIT
/*
 * Debug shell command that feeds a remote message to the same authenticated
 * handler the UDP transport uses, so the full command path (authentication,
 * replay protection, flash persistence, actuation) can be tested over the
 * serial console without spending cellular data:
 *
 *   uart:~$ rs msg on 1761840000123 16d82a013add92ef
 *   on 1761840000123 50d3e76858638e40
 *
 * Generate messages with `tools/rsctl.py payload <on|off|status>`.
 */

#include "board_io.h"

#include <errno.h>
#include <string.h>

#include <zephyr/shell/shell.h>

static int cmd_rs_msg(const struct shell *sh, size_t argc, char **argv)
{
	uint8_t msg[REMOTE_MESSAGE_MAX_LEN];
	uint8_t reply[REMOTE_MESSAGE_MAX_LEN];
	size_t len = 0;
	int reply_len;

	/* Re-join the shell arguments into one space-separated message. */
	for (size_t i = 1; i < argc; i++) {
		size_t arg_len = strlen(argv[i]);

		if (len + (i > 1 ? 1 : 0) + arg_len > REMOTE_MESSAGE_MAX_LEN) {
			shell_error(sh, "Message longer than %d bytes", REMOTE_MESSAGE_MAX_LEN);
			return -EINVAL;
		}
		if (i > 1) {
			msg[len++] = ' ';
		}
		memcpy(&msg[len], argv[i], arg_len);
		len += arg_len;
	}

	reply_len = rust_handle_remote_message(msg, len, reply, sizeof(reply));
	if (reply_len <= 0) {
		shell_warn(sh, "Dropped (unauthenticated, malformed, or remote control disabled)");
		return -EACCES;
	}
	shell_print(sh, "%.*s", reply_len, (const char *)reply);
	return 0;
}

SHELL_STATIC_SUBCMD_SET_CREATE(rs_cmds,
			       SHELL_CMD_ARG(msg, NULL,
					     "Handle a remote message: msg <cmd> <counter> <tag>",
					     cmd_rs_msg, 4, 0),
			       SHELL_SUBCMD_SET_END);

SHELL_CMD_REGISTER(rs, &rs_cmds, "Remote switch commands", NULL);
