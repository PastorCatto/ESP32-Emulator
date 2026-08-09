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
#include "qemu/main-loop.h"
#include "qemu/timer.h"
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
 * `nacked`, when given, reports whether the device refused the address. SPI
 * has no such thing and passes NULL; I2C needs it, because a NACK is how
 * firmware discovers which addresses are populated -- a bus scan that reads
 * zeroes instead finds a device at every address.
 *
 * Matched as a substring rather than parsed. The header is machine-generated
 * by one known writer, and a JSON parser in a device model is a dependency
 * this does not need.
 */
static bool esp_vpb_read_frame(EspVpbClient *c, uint8_t *payload,
                               uint32_t payload_cap, uint32_t *payload_len,
                               bool *nacked)
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

    g_autofree char *header = g_malloc(header_len + 1);
    if (!esp_vpb_recv_all(c, header, header_len)) {
        return false;
    }
    header[header_len] = '\0';

    if (nacked) {
        *nacked = strstr(header, "\"reply\":\"nack\"") != NULL;
    }

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

/*
 * Wait for a reply without freezing the rest of the machine.
 *
 * These calls come out of MMIO handlers, and QEMU runs those with the big
 * lock held. Blocking there stops far more than the core that asked: the
 * other vCPU cannot run and no timer fires, so from inside the guest *time
 * stops* for the length of the round trip. An RTOS notices. FreeRTOS ticks
 * vanish, timeouts are measured against a clock that jumped, watchdogs fire
 * against work that did happen -- and the resulting misbehaviour looks like
 * impossible firmware bugs rather than a stalled host.
 *
 * It is also self-amplifying: the device server lives in the UI process, so
 * a busy UI answers slowly, and every slow answer freezes the whole VM for
 * that much longer.
 *
 * Dropping the lock across the wait is safe here. This device's registers are
 * only touched by the vCPU already inside the transfer, and the socket
 * belongs to this client alone, so there is nothing for another thread to
 * race against while we sit in recv().
 */
static bool esp_vpb_read_frame_unlocked(EspVpbClient *c, uint8_t *payload,
                                        uint32_t capacity, uint32_t *got,
                                        bool *nacked)
{
    const bool held = bql_locked();

    if (held) {
        bql_unlock();
    }
    const bool ok = esp_vpb_read_frame(c, payload, capacity, got, nacked);
    if (held) {
        bql_lock();
    }
    return ok;
}

/* How long a partial buffer may sit before it goes out anyway. */
#define ESP_VPB_COALESCE_MS 2

/*
 * Beyond this a buffer is sent immediately. Well under the frame limit, and
 * large enough that a full screen is tens of frames rather than thousands.
 */
#define ESP_VPB_COALESCE_MAX (16 * 1024)

void esp_vpb_flush(EspVpbClient *c)
{
    if (!c->has_pending) {
        return;
    }

    /* Cleared first: a send failure must not leave the bytes queued again. */
    uint32_t len = c->pending_len;
    uint8_t controller = c->pending_controller;
    uint8_t cs = c->pending_cs;
    int dc = c->pending_dc;

    c->has_pending = false;
    c->pending_len = 0;
    if (c->flush_timer) {
        timer_del(c->flush_timer);
    }

    g_autofree char *header =
        g_strdup_printf("{\"type\":\"transact\","
                        "\"op\":\"spi_transfer\",\"controller\":%u,\"cs\":%u,"
                        "%s\"read_len\":0}",
                        controller, cs,
                        dc < 0 ? "" : (dc ? "\"dc\":true," : "\"dc\":false,"));

    if (!esp_vpb_write_frame(c, header, c->pending, len)) {
        esp_vpb_fail(c, "send failed");
    }
}

static void esp_vpb_flush_timeout(void *opaque)
{
    esp_vpb_flush((EspVpbClient *)opaque);
}

/*
 * Can these bytes join what is already buffered?
 *
 * Only when they are going to the same place: a different device, or the same
 * device with the data/command line the other way, is a different meaning for
 * the bytes and must stay a separate frame.
 */
static bool esp_vpb_can_append(EspVpbClient *c, uint8_t controller, uint8_t cs,
                               int dc, uint32_t len)
{
    return c->has_pending && c->pending_controller == controller &&
           c->pending_cs == cs && c->pending_dc == dc &&
           c->pending_len + len <= ESP_VPB_COALESCE_MAX;
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
     * Hold write-only traffic back briefly and send it as one frame.
     *
     * An ESP-IDF driver hands over a whole framebuffer by DMA and this changes
     * nothing. Arduino's TFT_eSPI writes the SPI registers directly and pushes
     * pixels through the 64-byte register FIFO, so one screen is tens of
     * thousands of transfers -- Bruce issues about 127,000 during a boot. Each
     * one used to cost a JSON header, a syscall and a wakeup on the far side.
     *
     * Safe only because the bytes still arrive in order, on the same
     * connection, before anything that could observe them: a read flushes
     * first, and so does a change of device or data/command level.
     */
    if (read_len == 0 && len > 0) {
        if (!esp_vpb_can_append(c, controller, cs, dc, len)) {
            esp_vpb_flush(c);
        }

        if (!c->pending) {
            c->pending = g_malloc(ESP_VPB_COALESCE_MAX);
        }
        memcpy(c->pending + c->pending_len, mosi, len);
        c->pending_len += len;
        c->pending_controller = controller;
        c->pending_cs = cs;
        c->pending_dc = dc;
        c->has_pending = true;

        if (c->pending_len >= ESP_VPB_COALESCE_MAX) {
            esp_vpb_flush(c);
            return true;
        }

        /*
         * Nothing may force the buffer out if the guest stops drawing, so the
         * last partial frame of a screen would sit here indefinitely and the
         * display would show all but its final strip.
         */
        if (!c->flush_timer) {
            c->flush_timer = timer_new_ms(QEMU_CLOCK_VIRTUAL,
                                          esp_vpb_flush_timeout, c);
        }
        timer_mod(c->flush_timer,
                  qemu_clock_get_ms(QEMU_CLOCK_VIRTUAL) + ESP_VPB_COALESCE_MS);
        return true;
    }

    /* Anything that reads has to see the writes that came before it. */
    esp_vpb_flush(c);

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
    /*
     * SO_RCVTIMEO takes different things on different platforms: a DWORD of
     * milliseconds on Windows, a struct timeval everywhere else. Passing a
     * timeval on Windows silently reads its first four bytes as the
     * millisecond count -- a 2000 ms timeout becomes 2 ms, and every reply
     * looks like a dead peer.
     */
#ifdef _WIN32
    DWORD tv = ESP_VPB_TIMEOUT_MS;
#else
    struct timeval tv = {
        .tv_sec = ESP_VPB_TIMEOUT_MS / 1000,
        .tv_usec = (ESP_VPB_TIMEOUT_MS % 1000) * 1000,
    };
#endif
    setsockopt(c->fd, SOL_SOCKET, SO_RCVTIMEO, (const char *)&tv, sizeof(tv));

    /*
     * The guest is stalled for the round trip, and sees that time pass.
     *
     * Pausing the virtual clock here was tried and reverted: cpu_disable_ticks
     * is meant for whole-VM suspend, and calling it from a device read froze
     * the machine outright. It also was not needed -- firmware tolerates the
     * latency fine, because a real bus transaction blocks too.
     */
    uint32_t got = 0;
    bool ok = esp_vpb_read_frame_unlocked(c, miso, read_len, &got, NULL);

    if (!ok) {
        esp_vpb_fail(c, "no reply within the timeout");
        return false;
    }

    /* A short answer leaves the rest of the buffer as the caller set it. */
    return got > 0;
}

/*
 * Wait for the answer to a transaction that was sent with an id.
 *
 * Shared by both I2C directions, which unlike SPI always wait: a write has to
 * know whether the address acknowledged.
 */
static bool esp_vpb_await(EspVpbClient *c, uint8_t *data, uint32_t len,
                          uint32_t *got, bool *nacked)
{
    qemu_socket_set_block(c->fd);
#ifdef _WIN32
    DWORD tv = ESP_VPB_TIMEOUT_MS;
#else
    struct timeval tv = {
        .tv_sec = ESP_VPB_TIMEOUT_MS / 1000,
        .tv_usec = (ESP_VPB_TIMEOUT_MS % 1000) * 1000,
    };
#endif
    setsockopt(c->fd, SOL_SOCKET, SO_RCVTIMEO, (const char *)&tv, sizeof(tv));

    if (!esp_vpb_read_frame_unlocked(c, data, len, got, nacked)) {
        esp_vpb_fail(c, "no reply within the timeout");
        return false;
    }
    return true;
}

bool esp_vpb_i2c_write(EspVpbClient *c, uint8_t controller, uint8_t address,
                       const uint8_t *data, uint32_t len, bool stop,
                       bool *nacked)
{
    *nacked = false;
    if (!esp_vpb_connect(c)) {
        return false;
    }
    if (len > ESP_VPB_MAX_PAYLOAD) {
        return false;
    }

    /*
     * Always carries an id, unlike a write-only SPI transfer. The answer is
     * the acknowledgement, and firmware acts on it.
     */
    uint64_t id = c->next_id++;
    g_autofree char *header = g_strdup_printf(
        "{\"type\":\"transact\",\"id\":%" PRIu64 ",\"op\":\"i2c_write\","
        "\"controller\":%u,\"address\":%u,\"stop\":%s}",
        id, controller, address, stop ? "true" : "false");

    if (!esp_vpb_write_frame(c, header, data, len)) {
        esp_vpb_fail(c, "send failed");
        return false;
    }

    uint32_t got = 0;
    return esp_vpb_await(c, NULL, 0, &got, nacked);
}

bool esp_vpb_i2c_read(EspVpbClient *c, uint8_t controller, uint8_t address,
                      uint8_t *data, uint32_t len, bool *nacked)
{
    *nacked = false;
    if (!esp_vpb_connect(c) || len > ESP_VPB_MAX_PAYLOAD) {
        return false;
    }

    uint64_t id = c->next_id++;
    g_autofree char *header = g_strdup_printf(
        "{\"type\":\"transact\",\"id\":%" PRIu64 ",\"op\":\"i2c_read\","
        "\"controller\":%u,\"address\":%u,\"len\":%u}",
        id, controller, address, len);

    if (!esp_vpb_write_frame(c, header, NULL, 0)) {
        esp_vpb_fail(c, "send failed");
        return false;
    }

    uint32_t got = 0;
    if (!esp_vpb_await(c, data, len, &got, nacked)) {
        return false;
    }
    return got > 0;
}
