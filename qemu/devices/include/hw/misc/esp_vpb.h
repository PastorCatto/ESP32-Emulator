/*
 * Virtual peripheral bus transport.
 *
 * Forwards bus transactions out of QEMU to whatever is listening on a socket,
 * so the devices hanging off a board's buses can be implemented outside the
 * emulator -- in the Rust shell, or in someone else's process in any language.
 *
 * The wire format is documented in docs/custom-hardware.md. In short: a
 * length-prefixed frame carrying a JSON header and an opaque payload, so bulk
 * data (a 150 KiB framebuffer write) costs no encoding overhead.
 *
 * Whole transactions are forwarded, never single bytes. A per-byte round trip
 * to another process would make a display unusable.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */
#pragma once

#include "qemu/osdep.h"

/* Larger than any single SPI transaction the hardware can express. */
#define ESP_VPB_MAX_PAYLOAD (64 * 1024)

typedef struct EspVpbClient {
    /* TCP port on localhost. Zero disables forwarding entirely. */
    uint16_t port;

    int fd;
    bool connected;
    /*
     * Set once a connection attempt has failed, so a missing listener costs
     * one attempt rather than one per transaction. The emulator stays usable
     * with nothing attached; the bus simply looks empty.
     */
    bool gave_up;

    uint64_t next_id;
} EspVpbClient;

/**
 * Forward one SPI transaction and, when read_len is non-zero, wait for the
 * bytes clocked back.
 *
 * Returns false if the transaction was not delivered, in which case the
 * caller should behave as though nothing is attached to the bus. `dc` is the
 * data/command line level, or -1 when the board has none.
 */
bool esp_vpb_spi_transfer(EspVpbClient *c, uint8_t controller, uint8_t cs,
                          int dc, const uint8_t *mosi, uint32_t len,
                          uint8_t *miso, uint32_t read_len);

/**
 * Forward one I2C write and wait for the acknowledgement.
 *
 * Unlike SPI, this always waits: `nacked` is how firmware learns an address
 * is empty, and a bus scan is nothing but a sequence of one-byte writes whose
 * only interesting result is whether they were answered.
 *
 * `stop` is false for the repeated start a register read begins with.
 * Returns false when nothing is listening, leaving `nacked` clear -- an
 * unattached bus is not the same as a device saying no.
 */
bool esp_vpb_i2c_write(EspVpbClient *c, uint8_t controller, uint8_t address,
                       const uint8_t *data, uint32_t len, bool stop,
                       bool *nacked);

/** Forward one I2C read. See [esp_vpb_i2c_write] for `nacked`. */
bool esp_vpb_i2c_read(EspVpbClient *c, uint8_t controller, uint8_t address,
                      uint8_t *data, uint32_t len, bool *nacked);

/** Release the connection, if any. */
void esp_vpb_close(EspVpbClient *c);
