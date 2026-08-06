/*
 * Virtual peripheral bus transport.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */

#include "qemu/osdep.h"
#include "qemu/bswap.h"
#include "qemu/log.h"
#include "qemu/error-report.h"
#include "qemu/sockets.h"
#include "qapi/error.h"
#include "io/channel-socket.h"
#include "hw/misc/esp_vpb.h"

/*
 * How long to wait for a device to answer a read.
 *
 * The guest is blocked for this whole time, so it has to be short enough that
 * a wedged driver does not look like a wedged emulator, and long enough that
 * an ordinary answer is never cut off. A device that misses this is treated
 * as absent rather than retried: retrying a bus transaction invents traffic
 * the guest never asked for.
 */
#define ESP_VPB_TIMEOUT_MS 2000

static bool esp_vpb_connect(EspVpbClient *c)
{
    if (c->connected) {
        return true;
    }
    if (c->port == 0 || c->gave_up) {
        return false;
    }

    g_autofree char *port = g_strdup_printf("%u", c->port);
    InetSocketAddress saddr = {
        .host = (char *)"127.0.0.1",
        .port = port,
    };
    Error *err = NULL;
    int fd = inet_connect_saddr(&saddr, &err);

    if (fd < 0) {
        /*
         * Nothing listening. Say so once and then stay quiet: an emulator
         * with no device process attached is a normal way to run, and the
         * buses should simply look empty.
         */
        warn_report("vpb: no peripheral server on port %u (%s); "
                    "buses will appear empty",
                    c->port, err ? error_get_pretty(err) : "connect failed");
        error_free(err);
        c->gave_up = true;
        return false;
    }

    /* Latency matters far more than throughput for a request/response bus. */
    socket_set_nodelay(fd);
    c->fd = fd;
    c->connected = true;
    return true;
}

void esp_vpb_close(EspVpbClient *c)
{
    if (c->connected) {
        closesocket(c->fd);
        c->connected = false;
        c->fd = -1;
    }
}

/* Drop the connection and stop trying; something is out of sync. */
static void esp_vpb_fail(EspVpbClient *c, const char *why)
{
    warn_report("vpb: %s; disconnecting", why);
    esp_vpb_close(c);
    c->gave_up = true;
}

static bool esp_vpb_send_all(EspVpbClient *c, const void *buf, size_t len)
{
    const uint8_t *p = buf;

    while (len > 0) {
        ssize_t n = send(c->fd, (const char *)p, len, 0);
        if (n <= 0) {
            return false;
        }
        p += n;
        len -= n;
    }
    return true;
}

static bool esp_vpb_recv_all(EspVpbClient *c, void *buf, size_t len)
{
    uint8_t *p = buf;

    while (len > 0) {
        ssize_t n = recv(c->fd, (char *)p, len, 0);
        if (n <= 0) {
            return false;
        }
        p += n;
        len -= n;
    }
    return true;
}

/* body_len | header_len | header | payload */
static bool esp_vpb_write_frame(EspVpbClient *c, const char *header,
                                const uint8_t *payload, uint32_t payload_len)
{
    uint32_t header_len = strlen(header);
    uint32_t body_len = 4 + header_len + payload_len;
    uint32_t prefix[2] = { cpu_to_le32(body_len), cpu_to_le32(header_len) };

    return esp_vpb_send_all(c, prefix, sizeof(prefix)) &&
           esp_vpb_send_all(c, header, header_len) &&
           (payload_len == 0 || esp_vpb_send_all(c, payload, payload_len));
}

/*
 * Read one frame, returning its payload.
 *
 * The header is only inspected for whether it is a data reply; a NACK or an
 * empty response both simply yield no bytes, which is what an absent device
 * looks like on a real bus.
 */
static bool esp_vpb_read_frame(EspVpbClient *c, uint8_t *payload,
                               uint32_t payload_cap, uint32_t *payload_len)
{
    uint32_t prefix[2];

    if (!esp_vpb_recv_all(c, prefix, sizeof(prefix))) {
        return false;
    }
    uint32_t body_len = le32_to_cpu(prefix[0]);
    uint32_t header_len = le32_to_cpu(prefix[1]);

    if (body_len < 4 || header_len > body_len - 4 ||
        body_len > ESP_VPB_MAX_PAYLOAD + 4096) {
        esp_vpb_fail(c, "malformed reply frame");
        return false;
    }

    /* Skip the header; we do not need to parse it to route the payload. */
    g_autofree char *header = g_malloc(header_len + 1);
    if (!esp_vpb_recv_all(c, header, header_len)) {
        return false;
    }
    header[header_len] = '\0';

    uint32_t remaining = body_len - 4 - header_len;
    *payload_len = MIN(remaining, payload_cap);

    if (*payload_len && !esp_vpb_recv_all(c, payload, *payload_len)) {
        return false;
    }
    /* Drain anything that would not fit, so the stream stays aligned. */
    for (uint32_t left = remaining - *payload_len; left > 0; ) {
        uint8_t sink[256];
        uint32_t chunk = MIN(left, sizeof(sink));
        if (!esp_vpb_recv_all(c, sink, chunk)) {
            return false;
        }
        left -= chunk;
    }
    return true;
}

bool esp_vpb_spi_transfer(EspVpbClient *c, uint8_t controller, uint8_t cs,
                          int dc, const uint8_t *mosi, uint32_t len,
                          uint8_t *miso, uint32_t read_len)
{
    if (!esp_vpb_connect(c)) {
        return false;
    }
    if (len > ESP_VPB_MAX_PAYLOAD) {
        qemu_log_mask(LOG_UNIMP, "vpb: %u-byte transfer exceeds the frame limit\n",
                      len);
        return false;
    }

    /*
     * Only transactions that read anything carry an id. A write-only transfer
     * is fire-and-forget: not waiting for an acknowledgement nobody reads is
     * what keeps a display from becoming a slideshow.
     */
    uint64_t id = c->next_id++;
    g_autofree char *header = read_len > 0
        ? g_strdup_printf("{\"type\":\"transact\",\"id\":%" PRIu64 ","
                          "\"op\":\"spi_transfer\",\"controller\":%u,\"cs\":%u,"
                          "%s\"read_len\":%u}",
                          id, controller, cs,
                          dc < 0 ? "" : (dc ? "\"dc\":true," : "\"dc\":false,"),
                          read_len)
        : g_strdup_printf("{\"type\":\"transact\","
                          "\"op\":\"spi_transfer\",\"controller\":%u,\"cs\":%u,"
                          "%s\"read_len\":0}",
                          controller, cs,
                          dc < 0 ? "" : (dc ? "\"dc\":true," : "\"dc\":false,"));

    if (!esp_vpb_write_frame(c, header, mosi, len)) {
        esp_vpb_fail(c, "send failed");
        return false;
    }

    if (read_len == 0) {
        return true;
    }

    /*
     * Blocking read with a timeout. The guest is stalled here, which is
     * faithful -- a real transfer holds the bus too -- but it must not stall
     * forever because a driver crashed.
     */
    qemu_socket_set_block(c->fd);
    struct timeval tv = {
        .tv_sec = ESP_VPB_TIMEOUT_MS / 1000,
        .tv_usec = (ESP_VPB_TIMEOUT_MS % 1000) * 1000,
    };
    setsockopt(c->fd, SOL_SOCKET, SO_RCVTIMEO, (const char *)&tv, sizeof(tv));

    uint32_t got = 0;
    if (!esp_vpb_read_frame(c, miso, read_len, &got)) {
        esp_vpb_fail(c, "no reply within the timeout");
        return false;
    }

    /* A short answer leaves the rest of the buffer as the caller set it. */
    return got > 0;
}
