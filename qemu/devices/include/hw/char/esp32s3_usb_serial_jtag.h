/*
 * ESP32-S3 USB Serial/JTAG controller — serial endpoint.
 *
 * Espressif's QEMU maps this peripheral but implements it as a stub: reads
 * return zero and writes are dropped. Firmware built with
 * CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG therefore boots completely silently,
 * because its console writes into nothing. That covers any board whose USB-C
 * goes straight to the S3 rather than through a bridge chip — the T-Deck
 * among them.
 *
 * This models the serial endpoint against the ESP-IDF v5.3.5 HAL
 * (components/hal/esp32s3/include/hal/usb_serial_jtag_ll.h), which is the
 * definitive statement of what firmware expects. The JTAG side is not
 * modelled; nothing needs it to get a console.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */
#pragma once

#include "hw/hw.h"
#include "hw/sysbus.h"
#include "hw/registerfields.h"
#include "chardev/char-fe.h"

#define TYPE_ESP32S3_USJ "char.esp32s3.usb_serial_jtag"
#define ESP32S3_USJ(obj) OBJECT_CHECK(Esp32s3UsjState, (obj), TYPE_ESP32S3_USJ)

#define ESP32S3_USJ_MEM_SIZE   0x84
#define ESP32S3_USJ_REG_COUNT  (ESP32S3_USJ_MEM_SIZE / sizeof(uint32_t))

/*
 * The USB endpoint is 64 bytes. Hardware flushes automatically when it fills,
 * which firmware relies on: a driver may write a long string and only call
 * flush at the end.
 */
#define ESP32S3_USJ_EP_SIZE    64

/* Host-to-device bytes waiting to be read. Generous, so a pasted command
 * cannot overrun before firmware drains it. */
#define ESP32S3_USJ_RX_SIZE    1024

REG32(USJ_EP1, 0x000)
    FIELD(USJ_EP1, RDWR_BYTE, 0, 8)

REG32(USJ_EP1_CONF, 0x004)
    /* Write 1 to hand the endpoint buffer to the host. Reads back as 0. */
    FIELD(USJ_EP1_CONF, WR_DONE, 0, 1)
    /* Read-only: room left in the TX endpoint. */
    FIELD(USJ_EP1_CONF, SERIAL_IN_EP_DATA_FREE, 1, 1)
    /* Read-only: a byte is waiting from the host. */
    FIELD(USJ_EP1_CONF, SERIAL_OUT_EP_DATA_AVAIL, 2, 1)

REG32(USJ_INT_RAW, 0x008)
REG32(USJ_INT_ST,  0x00c)
REG32(USJ_INT_ENA, 0x010)
REG32(USJ_INT_CLR, 0x014)

/* Host consumed the buffer we flushed; room again. */
#define USJ_SERIAL_IN_EMPTY_INT     (1u << 3)
/* A packet arrived from the host. */
#define USJ_SERIAL_OUT_RECV_PKT_INT (1u << 2)

typedef struct Esp32s3UsjState {
    SysBusDevice parent_obj;

    MemoryRegion iomem;
    CharBackend chr;
    qemu_irq irq;

    uint32_t regs[ESP32S3_USJ_REG_COUNT];

    /* Bytes written by firmware, not yet handed to the host. */
    uint8_t tx[ESP32S3_USJ_EP_SIZE];
    unsigned tx_len;

    /* Bytes from the host, not yet read by firmware. */
    uint8_t rx[ESP32S3_USJ_RX_SIZE];
    unsigned rx_head;
    unsigned rx_len;
} Esp32s3UsjState;
