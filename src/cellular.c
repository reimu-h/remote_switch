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
 *   - It sends exactly one small reply datagram per authenticated request, and
 *     drops unauthenticated datagrams without replying, so probes cannot make it
 *     spend data.
 *   - PSM and eDRX are disabled so downlink commands arrive within seconds. This
 *     costs power but no data; the board is expected to be mains powered.
 *   - Modem byte counters (AT%XCONNSTAT) are logged so data usage can be
 *     audited on the serial console.
 */

#include "cellular.h"

#include <errno.h>

#include <zephyr/kernel.h>
#include <zephyr/sys/util.h>

#if IS_ENABLED(CONFIG_REMOTE_SWITCH_CELLULAR)

#include "board_io.h"

#include <modem/lte_lc.h>
#include <modem/nrf_modem_lib.h>
#include <nrf_modem_at.h>
#include <zephyr/logging/log.h>
#include <zephyr/net/socket.h>

LOG_MODULE_REGISTER(cellular, LOG_LEVEL_INF);

/* One extra byte detects (and drops) oversized datagrams. */
#define RX_BUF_LEN            (REMOTE_MESSAGE_MAX_LEN + 1)
#define POLL_TIMEOUT_MS       (60 * MSEC_PER_SEC)
#define USAGE_LOG_INTERVAL_MS (6 * 60 * 60 * MSEC_PER_SEC)
#define INIT_RETRY_DELAY      K_SECONDS(30)

K_THREAD_STACK_DEFINE(cellular_stack, CONFIG_REMOTE_SWITCH_CELLULAR_STACK_SIZE);
static struct k_thread cellular_thread;
static atomic_t started;

/* Default PDN (CID 0) state, updated from lte_lc events on the system workqueue. */
static atomic_t pdn_active;
static atomic_t pdn_generation;
static K_SEM_DEFINE(pdn_changed, 0, 1);

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
 * Runs after every modem library (re)initialization, including the automatic
 * re-initialization after a modem fault, so these settings always apply.
 */
NRF_MODEM_LIB_ON_INIT(remote_switch_modem_init_hook, on_modem_lib_init, NULL);

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

	if (nrf_modem_at_scanf("AT+CGMR", "%31s", fw) == 1) {
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
	int err = nrf_modem_lib_init();

	if (err) {
		LOG_ERR("Modem library init failed: %d", err);
		return err;
	}
	log_modem_info();

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

/* Serves commands until the PDN connection drops or is replaced. */
static void serve_udp(void)
{
	const atomic_val_t generation = atomic_get(&pdn_generation);
	struct net_sockaddr_in local = {
		.sin_family = NET_AF_INET,
		.sin_port = net_htons(CONFIG_REMOTE_SWITCH_UDP_PORT),
		.sin_addr.s_addr = NET_INADDR_ANY,
	};
	int64_t last_usage_log = k_uptime_get();
	uint8_t rx[RX_BUF_LEN];
	int fd;

	fd = zsock_socket(NET_AF_INET, NET_SOCK_DGRAM, NET_IPPROTO_UDP);
	if (fd < 0) {
		LOG_ERR("Failed to create socket: %d", errno);
		return;
	}
	if (zsock_bind(fd, (struct net_sockaddr *)&local, sizeof(local)) < 0) {
		LOG_ERR("Failed to bind UDP port %d: %d", CONFIG_REMOTE_SWITCH_UDP_PORT, errno);
		goto out;
	}
	log_listen_address();

	while (atomic_get(&pdn_active) && atomic_get(&pdn_generation) == generation) {
		struct zsock_pollfd pfd = {.fd = fd, .events = ZSOCK_POLLIN};
		struct net_sockaddr_storage from;
		net_socklen_t from_len = sizeof(from);
		ssize_t len;
		int ret = zsock_poll(&pfd, 1, POLL_TIMEOUT_MS);

		if (k_uptime_get() - last_usage_log >= USAGE_LOG_INTERVAL_MS) {
			last_usage_log = k_uptime_get();
			log_data_usage();
		}
		if (ret == 0) {
			continue; /* Timeout: re-check the PDN state. */
		}
		if (ret < 0 || (pfd.revents & (ZSOCK_POLLERR | ZSOCK_POLLHUP | ZSOCK_POLLNVAL))) {
			LOG_WRN("Socket error (poll %d, revents 0x%x, errno %d)", ret, pfd.revents,
				errno);
			break;
		}

		len = zsock_recvfrom(fd, rx, sizeof(rx), ZSOCK_MSG_DONTWAIT,
				     (struct net_sockaddr *)&from, &from_len);
		if (len < 0) {
			if (errno == EAGAIN || errno == EWOULDBLOCK) {
				continue;
			}
			LOG_WRN("recvfrom failed: %d", errno);
			break;
		}
		handle_datagram(fd, rx, len, (struct net_sockaddr *)&from, from_len);
	}
out:
	zsock_close(fd);
}

static void cellular_thread_fn(void *p1, void *p2, void *p3)
{
	ARG_UNUSED(p1);
	ARG_UNUSED(p2);
	ARG_UNUSED(p3);

	while (modem_setup() != 0) {
		k_sleep(INIT_RETRY_DELAY);
	}

	for (;;) {
		int err;

		if (atomic_get(&pdn_active)) {
			serve_udp();
			continue;
		}
		err = k_sem_take(&pdn_changed,
				 K_SECONDS(CONFIG_REMOTE_SWITCH_PDN_RECOVERY_TIMEOUT));
		if (err == -EAGAIN && !atomic_get(&pdn_active)) {
			/* Re-attach in case the modem was reset or got stuck searching. */
			LOG_WRN("No PDN connection for %d s; re-attaching",
				CONFIG_REMOTE_SWITCH_PDN_RECOVERY_TIMEOUT);
			(void)lte_lc_offline();
			(void)lte_lc_normal();
		}
	}
}

int cellular_start(void)
{
	if (!atomic_cas(&started, 0, 1)) {
		return -EALREADY;
	}
	k_thread_create(&cellular_thread, cellular_stack, K_THREAD_STACK_SIZEOF(cellular_stack),
			cellular_thread_fn, NULL, NULL, NULL, K_PRIO_PREEMPT(7), 0, K_NO_WAIT);
	k_thread_name_set(&cellular_thread, "cellular");
	return 0;
}

#else /* !CONFIG_REMOTE_SWITCH_CELLULAR */

int cellular_start(void)
{
	return -ENOTSUP;
}

#endif /* CONFIG_REMOTE_SWITCH_CELLULAR */
