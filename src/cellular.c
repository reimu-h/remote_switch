// SPDX-License-Identifier: MIT
/*
 * Cellular (LTE-M / NB-IoT) transport for authenticated remote commands.
 *
 * Data-usage policy (the SIM has a small data allowance):
 *   - The device never starts traffic. It attaches with an IPv4-only PDN (so
 *     there are no IPv6 router advertisements), resolves no hostnames, and sends
 *     no keep-alives, heartbeats, or NTP requests.
 *   - It listens on a UDP port. Commands reach it over the carrier's private
 *     network (for 1NCE, the OpenVPN service or the 1NCE API), so it needs no
 *     NAT keep-alive traffic to stay reachable.
 *   - It sends one small reply datagram per fresh authenticated request (plus a
 *     bounded retry budget for duplicate/stale requests), and drops
 *     unauthenticated or rate-limited datagrams without replying, so probes and
 *     replayed captures cannot make it spend unbounded data.
 *   - PSM and eDRX are disabled so downlink commands arrive within seconds. This
 *     costs power but no data; the board is expected to be mains powered.
 *   - Modem byte counters (AT%XCONNSTAT) are logged so data usage can be
 *     audited on the serial console.
 */

#include "cellular.h"

#include <errno.h>
#include <stdbool.h>
#include <stdint.h>

#include <zephyr/kernel.h>
#include <zephyr/sys/util.h>

#if IS_ENABLED(CONFIG_REMOTE_SWITCH_CELLULAR)

#include "board_io.h"

#include <inttypes.h>

#include <modem/lte_lc.h>
#include <modem/nrf_modem_lib.h>
#include <nrf_modem.h>
#include <nrf_modem_at.h>
#include <zephyr/logging/log.h>
#include <zephyr/net/socket.h>

LOG_MODULE_REGISTER(cellular, LOG_LEVEL_INF);

/* One extra byte detects (and drops) oversized datagrams. */
#define RX_BUF_LEN            (REMOTE_MESSAGE_MAX_LEN + 1)
#define POLL_TIMEOUT_MS       (60 * MSEC_PER_SEC)
#define PDN_WAIT_SLICE_MS     ((int64_t)POLL_TIMEOUT_MS)
#define USAGE_LOG_INTERVAL_MS (6 * 60 * 60 * MSEC_PER_SEC)
#define INIT_RETRY_DELAY      K_SECONDS(30)
#define SOCKET_RETRY_DELAY    K_SECONDS(5)

K_THREAD_STACK_DEFINE(cellular_stack, CONFIG_REMOTE_SWITCH_CELLULAR_STACK_SIZE);
static struct k_thread cellular_thread;
static atomic_t started;
static atomic_t last_heartbeat_sec;

/* Default PDN (CID 0) state and modem fault flag, safe across ISR and thread contexts. */
static atomic_t pdn_active;
static atomic_t pdn_generation;
static atomic_t modem_faulted;
static K_SEM_DEFINE(pdn_changed, 0, 1);

static void record_heartbeat(void)
{
	atomic_set(&last_heartbeat_sec, (atomic_val_t)k_uptime_seconds());
}

static const char *reg_status_str(enum lte_lc_nw_reg_status status)
{
	switch (status) {
	case LTE_LC_NW_REG_NOT_REGISTERED:
		return "not registered";
	case LTE_LC_NW_REG_REGISTERED_HOME:
		return "registered (home)";
	case LTE_LC_NW_REG_SEARCHING:
		return "searching";
	case LTE_LC_NW_REG_REGISTRATION_DENIED:
		return "registration denied";
	case LTE_LC_NW_REG_REGISTERED_ROAMING:
		return "registered (roaming)";
	case LTE_LC_NW_REG_UICC_FAIL:
		return "SIM failure";
	case LTE_LC_NW_REG_NO_SUITABLE_CELL:
		return "no suitable cell";
	default:
		return "unknown";
	}
}

static void set_pdn_active(bool active)
{
	if (active) {
		atomic_inc(&pdn_generation);
	}
	atomic_set(&pdn_active, active);
	k_sem_give(&pdn_changed);
}

static void lte_handler(const struct lte_lc_evt *const evt)
{
	switch (evt->type) {
	case LTE_LC_EVT_NW_REG_STATUS:
		LOG_INF("Network: %s", reg_status_str(evt->nw_reg_status));
		break;
	case LTE_LC_EVT_LTE_MODE_UPDATE:
		if (evt->lte_mode != LTE_LC_LTE_MODE_NONE) {
			LOG_INF("Access technology: %s",
				evt->lte_mode == LTE_LC_LTE_MODE_NBIOT ? "NB-IoT" : "LTE-M");
		}
		break;
	case LTE_LC_EVT_CELL_UPDATE:
		LOG_DBG("Cell id 0x%08x, tac 0x%04x", evt->cell.id, evt->cell.tac);
		break;
	case LTE_LC_EVT_RRC_UPDATE:
		LOG_DBG("RRC %s",
			evt->rrc_mode == LTE_LC_RRC_MODE_CONNECTED ? "connected" : "idle");
		break;
	case LTE_LC_EVT_PDN:
		if (evt->pdn.cid != 0) {
			break;
		}
		switch (evt->pdn.type) {
		case LTE_LC_EVT_PDN_ACTIVATED:
			LOG_INF("PDN connection active");
			set_pdn_active(true);
			break;
		case LTE_LC_EVT_PDN_DEACTIVATED:
		case LTE_LC_EVT_PDN_NETWORK_DETACH:
			LOG_WRN("PDN connection lost");
			set_pdn_active(false);
			break;
		default:
			break;
		}
		break;
	default:
		break;
	}
}

/*
 * Runs after every modem library (re)initialization, including recovery after
 * a modem fault, so these settings always apply.
 */
NRF_MODEM_LIB_ON_INIT(remote_switch_modem_init_hook, on_modem_lib_init, NULL);
NRF_MODEM_LIB_ON_SHUTDOWN(remote_switch_modem_shutdown_hook, on_modem_lib_shutdown, NULL);

static void on_modem_lib_init(int ret, void *ctx)
{
	ARG_UNUSED(ctx);

	if (ret != 0) {
		return;
	}
	/* Stay reachable: no PSM or eDRX sleep between paging occasions. */
	if (nrf_modem_at_printf("AT+CPSMS=0") || nrf_modem_at_printf("AT+CEDRXS=0")) {
		LOG_WRN("Failed to disable PSM/eDRX");
	}
	/* Start the modem's byte counters so data usage can be audited. */
	if (nrf_modem_at_printf("AT%%XCONNSTAT=1")) {
		LOG_WRN("Failed to start connection statistics");
	}
}

static void on_modem_lib_shutdown(void *ctx)
{
	ARG_UNUSED(ctx);
	set_pdn_active(false);
}

#if IS_ENABLED(CONFIG_NRF_MODEM_LIB_ON_FAULT_APPLICATION_SPECIFIC)
void nrf_modem_fault_handler(struct nrf_modem_fault_info *fault_info)
{
	if (fault_info != NULL) {
		LOG_ERR("Modem fault 0x%" PRIx32 " (pc 0x%08" PRIx32 ")", fault_info->reason,
			fault_info->program_counter);
	} else {
		LOG_ERR("Modem fault");
	}
	atomic_set(&modem_faulted, 1);
	set_pdn_active(false);
}
#endif

static void log_data_usage(void)
{
	int sms_tx, sms_rx, kb_tx, kb_rx, pkt_max, pkt_avg;
	int ret = nrf_modem_at_scanf("AT%XCONNSTAT?", "%%XCONNSTAT: %d,%d,%d,%d,%d,%d", &sms_tx,
				     &sms_rx, &kb_tx, &kb_rx, &pkt_max, &pkt_avg);

	if (ret == 6) {
		LOG_INF("Data since boot: %d kB up, %d kB down (largest packet %d B)", kb_tx, kb_rx,
			pkt_max);
	}
}

static void log_modem_info(void)
{
	char fw[32];

	if (nrf_modem_at_scanf("AT+CGMR", "%31[^\r\n]", fw) == 1) {
		LOG_INF("Modem firmware %s", fw);
	}
}

static void log_listen_address(void)
{
	char ip[NET_INET_ADDRSTRLEN];

	if (nrf_modem_at_scanf("AT+CGPADDR=0", "+CGPADDR: %*d,\"%15[0-9.]\"", ip) == 1) {
		LOG_INF("Listening for commands on %s:%d/udp", ip, CONFIG_REMOTE_SWITCH_UDP_PORT);
	} else {
		LOG_INF("Listening for commands on UDP port %d", CONFIG_REMOTE_SWITCH_UDP_PORT);
	}
}

static int modem_setup(void)
{
	int err;

	if (atomic_cas(&modem_faulted, 1, 0) && nrf_modem_is_initialized()) {
		err = nrf_modem_lib_shutdown();
		if (err) {
			LOG_ERR("Modem library shutdown after fault failed: %d", err);
			atomic_set(&modem_faulted, 1);
			return err;
		}
	}

	if (!nrf_modem_is_initialized()) {
		err = nrf_modem_lib_init();
		if (err) {
			LOG_ERR("Modem library init failed: %d", err);
			return err;
		}
		log_modem_info();
	}

	lte_lc_register_handler(lte_handler);
	err = lte_lc_pdn_default_ctx_events_enable();
	if (err) {
		LOG_ERR("Failed to enable PDN events: %d", err);
		return err;
	}
	/* Asynchronous attach; progress is reported through lte_handler(). */
	err = lte_lc_connect_async(NULL);
	if (err) {
		LOG_ERR("Failed to start LTE connection: %d", err);
	}
	return err;
}

static void run_modem_setup(void)
{
	record_heartbeat();
	while (modem_setup() != 0) {
		k_sleep(INIT_RETRY_DELAY);
	}
	record_heartbeat();
}

static void handle_datagram(int fd, const uint8_t *msg, size_t len, const struct net_sockaddr *from,
			    net_socklen_t from_len)
{
	uint8_t reply[REMOTE_MESSAGE_MAX_LEN];
	int reply_len;

	if (len > REMOTE_MESSAGE_MAX_LEN) {
		LOG_WRN("Dropped oversized datagram");
		return;
	}
	reply_len = rust_handle_remote_message(msg, len, reply, sizeof(reply));
	if (reply_len <= 0) {
		return; /* Rejected: stay silent so probes cannot make us spend data. */
	}
	if (zsock_sendto(fd, reply, reply_len, 0, from, from_len) < 0) {
		LOG_WRN("Failed to send reply: %d", errno);
	}
	log_data_usage();
}

/*
 * Serves commands until the PDN connection drops, is replaced, or hits a
 * socket error. Returns true on a normal PDN state transition and false on
 * socket creation/bind/poll/receive failure so the caller can back off.
 */
static bool serve_udp(void)
{
	const atomic_val_t generation = atomic_get(&pdn_generation);
	struct net_sockaddr_in local = {
		.sin_family = NET_AF_INET,
		.sin_port = net_htons(CONFIG_REMOTE_SWITCH_UDP_PORT),
		.sin_addr.s_addr = NET_INADDR_ANY,
	};
	int64_t last_usage_log = k_uptime_get();
	uint8_t rx[RX_BUF_LEN];
	bool clean_exit = false;
	int fd;

	fd = zsock_socket(NET_AF_INET, NET_SOCK_DGRAM, NET_IPPROTO_UDP);
	if (fd < 0) {
		LOG_ERR("Failed to create socket: %d", errno);
		return false;
	}
	if (zsock_bind(fd, (struct net_sockaddr *)&local, sizeof(local)) < 0) {
		LOG_ERR("Failed to bind UDP port %d: %d", CONFIG_REMOTE_SWITCH_UDP_PORT, errno);
		goto out;
	}
	log_listen_address();

	while (atomic_get(&pdn_active) && !atomic_get(&modem_faulted) &&
	       atomic_get(&pdn_generation) == generation) {
		struct zsock_pollfd pfd = {.fd = fd, .events = ZSOCK_POLLIN};
		struct net_sockaddr_storage from;
		net_socklen_t from_len = sizeof(from);
		ssize_t len;
		int ret = zsock_poll(&pfd, 1, POLL_TIMEOUT_MS);

		if (!atomic_get(&modem_faulted) &&
		    k_uptime_get() - last_usage_log >= USAGE_LOG_INTERVAL_MS) {
			last_usage_log = k_uptime_get();
			log_data_usage();
		}
		if (ret == 0) {
			record_heartbeat();
			continue; /* Timeout: re-check the PDN state. */
		}
		if (ret < 0 || (pfd.revents & (ZSOCK_POLLERR | ZSOCK_POLLHUP | ZSOCK_POLLNVAL))) {
			LOG_WRN("Socket error (poll %d, revents 0x%x, errno %d)", ret, pfd.revents,
				errno);
			goto out;
		}

		len = zsock_recvfrom(fd, rx, sizeof(rx), ZSOCK_MSG_DONTWAIT,
				     (struct net_sockaddr *)&from, &from_len);
		if (len < 0) {
			if (errno == EAGAIN || errno == EWOULDBLOCK) {
				record_heartbeat();
				continue;
			}
			LOG_WRN("recvfrom failed: %d", errno);
			goto out;
		}
		handle_datagram(fd, rx, len, (struct net_sockaddr *)&from, from_len);
		record_heartbeat();
	}
	clean_exit = true;
out:
	zsock_close(fd);
	return clean_exit;
}

static void cellular_thread_fn(void *p1, void *p2, void *p3)
{
	const int64_t recovery_ms =
		(int64_t)CONFIG_REMOTE_SWITCH_PDN_RECOVERY_TIMEOUT * (int64_t)MSEC_PER_SEC;
	int64_t pdn_lost_since;

	ARG_UNUSED(p1);
	ARG_UNUSED(p2);
	ARG_UNUSED(p3);

	run_modem_setup();
	pdn_lost_since = k_uptime_get();

	for (;;) {
		int64_t remaining_ms;

		if (atomic_get(&modem_faulted)) {
			LOG_WRN("Re-initializing modem after fault");
			run_modem_setup();
			pdn_lost_since = k_uptime_get();
			continue;
		}
		if (atomic_get(&pdn_active)) {
			if (serve_udp()) {
				pdn_lost_since = k_uptime_get();
			} else if (atomic_get(&pdn_active) && !atomic_get(&modem_faulted)) {
				k_sleep(SOCKET_RETRY_DELAY);
			} else {
				pdn_lost_since = k_uptime_get();
			}
			continue;
		}

		remaining_ms = recovery_ms - (k_uptime_get() - pdn_lost_since);
		if (remaining_ms > 0) {
			int64_t wait_ms = MIN(remaining_ms, PDN_WAIT_SLICE_MS);

			record_heartbeat();
			(void)k_sem_take(&pdn_changed, K_MSEC(wait_ms));
			continue;
		}

		/* Re-attach in case the modem was reset or got stuck searching. */
		LOG_WRN("No PDN connection for %d s; re-attaching",
			CONFIG_REMOTE_SWITCH_PDN_RECOVERY_TIMEOUT);
		if (lte_lc_offline() != 0 || lte_lc_normal() != 0) {
			LOG_WRN("PDN recovery AT command failed; re-initializing modem");
			atomic_set(&modem_faulted, 1);
		} else {
			record_heartbeat();
		}
		pdn_lost_since = k_uptime_get();
	}
}

int cellular_start(void)
{
	if (!atomic_cas(&started, 0, 1)) {
		return -EALREADY;
	}
	record_heartbeat();
	k_thread_create(&cellular_thread, cellular_stack, K_THREAD_STACK_SIZEOF(cellular_stack),
			cellular_thread_fn, NULL, NULL, NULL, K_PRIO_PREEMPT(7), 0, K_NO_WAIT);
	k_thread_name_set(&cellular_thread, "cellular");
	return 0;
}

bool cellular_is_healthy(void)
{
#if IS_ENABLED(CONFIG_REMOTE_SWITCH_WATCHDOG)
	uint32_t elapsed;

	if (!atomic_get(&started)) {
		return true;
	}
	elapsed = k_uptime_seconds() - (uint32_t)atomic_get(&last_heartbeat_sec);
	return elapsed <= (uint32_t)CONFIG_REMOTE_SWITCH_CELLULAR_WATCHDOG_TIMEOUT;
#else
	return true;
#endif
}

#else /* !CONFIG_REMOTE_SWITCH_CELLULAR */

int cellular_start(void)
{
	return -ENOTSUP;
}

bool cellular_is_healthy(void)
{
	return true;
}

#endif /* CONFIG_REMOTE_SWITCH_CELLULAR */
