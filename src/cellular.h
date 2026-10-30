/* SPDX-License-Identifier: MIT */
/*
 * Cellular (LTE-M / NB-IoT) transport for authenticated remote commands.
 */

#ifndef REMOTE_SWITCH_CELLULAR_H_
#define REMOTE_SWITCH_CELLULAR_H_

#ifdef __cplusplus
extern "C" {
#endif

/**
 * @brief Start the cellular link and the UDP command listener.
 *
 * Spawns a thread that initializes the modem, attaches to the network, and
 * serves authenticated commands on CONFIG_REMOTE_SWITCH_UDP_PORT. Returns
 * immediately. Call once from thread context after the remote key and the
 * replay counter have been initialized.
 *
 * @return 0 on success, -EALREADY if already started, or -ENOTSUP when
 *         CONFIG_REMOTE_SWITCH_CELLULAR is disabled.
 */
int cellular_start(void);

#ifdef __cplusplus
}
#endif

#endif /* REMOTE_SWITCH_CELLULAR_H_ */

