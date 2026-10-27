/*
 * SPI bit-banged over GPIO, decoded and forwarded to the device models.
 *
 * A SPI peripheral announces itself: the controller model sees a transaction
 * and hands the bytes to the virtual peripheral bus. Firmware that has no
 * peripheral to use -- a bootloader, say, which cannot count on any of the
 * chip's controllers being set up -- drives the same four wires by hand with
 * single-pin GPIO writes instead, and nothing in the machine sees a
 * transaction, only pins changing.
 *
 * This watches those pins, reassembles the bytes the way a receiving device
 * would, and sends them down the same path as a controller's traffic, so the
 * device model on the other end cannot tell the difference.
 *
 * SPI mode 0, most significant bit first: MOSI is sampled on the rising edge
 * of SCLK while CS is low. Write-only; nothing is read back.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */
#pragma once

#include "hw/qdev-core.h"
#include "qom/object.h"
#include "hw/misc/esp_vpb.h"

/* How many decoded bytes are held back before they are sent. */
#define ESP_GPIO_SPI_BATCH 4096

#define TYPE_ESP_GPIO_SPI "esp.gpio-spi"
OBJECT_DECLARE_SIMPLE_TYPE(EspGpioSpiState, ESP_GPIO_SPI)

struct EspGpioSpiState {
    DeviceState parent_obj;

    EspVpbClient vpb;

    /* What the forwarded bytes are labelled as: the controller the board file
     * puts the device on, and its chip select line on that controller. */
    uint8_t vpb_controller;
    uint8_t cs_line;

    /* GPIO numbers, -1 where the board has no such pin. CS is optional: a
     * device with its select tied low is always listening. */
    int32_t sclk_gpio;
    int32_t mosi_gpio;
    int32_t cs_gpio;
    int32_t dc_gpio;

    /* Pin levels as of the previous write, for edge detection. */
    bool sclk;
    bool mosi;
    bool cs;

    /* The byte being assembled. */
    uint8_t shift;
    uint8_t bits;

    /*
     * Finished bytes waiting to be sent, all at one data/command level. They go
     * out as a single transfer rather than one each. Every transfer re-arms the
     * client's flush timer, which wakes QEMU's main loop, which takes the global
     * lock the guest CPU needs for its next GPIO write; at 24 writes a byte and
     * 150,000 bytes a screen that was the largest cost left in drawing one.
     */
    uint8_t batch[ESP_GPIO_SPI_BATCH];
    uint32_t batch_len;
    bool batch_dc;
    /* Sends a partial batch once the guest stops drawing. */
    QEMUTimer *flush_timer;
};

/** Is there anything to do: a bus to send to and the two pins that carry data. */
bool esp_gpio_spi_active(EspGpioSpiState *s);

/**
 * Called by the GPIO model after every write that moved an output or a
 * direction, with both whole words. `opaque` is the EspGpioSpiState. Only the
 * output word matters here: a bit-banged SPI line is driven push-pull.
 */
void esp_gpio_spi_out_changed(void *opaque, uint64_t out, uint64_t enable);
