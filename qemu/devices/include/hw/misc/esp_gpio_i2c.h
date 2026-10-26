/*
 * The slave side of an I2C bus that firmware drives by hand over GPIO.
 *
 * A bootloader cannot count on any I2C controller being set up, so PURR OS's
 * boot menu polls the keyboard by wiggling two pins open-drain: let go for a
 * 1, drive low for a 0, read the line back to see what the other end did. No
 * controller model sees a transaction, only pins changing, and nothing in the
 * machine can answer -- so the menu concludes the keyboard is absent.
 *
 * This follows the bus the way a real device would. It watches START and STOP,
 * collects the address byte, and for a read acknowledges and shifts a byte out
 * by holding SDA low through the GPIO model's input read. The byte itself comes
 * from the device model on the virtual peripheral bus, the same one a real
 * controller's traffic reaches, so the keyboard cannot tell the difference.
 *
 * Reads only. A write address is not acknowledged: nothing bit-banged here
 * needs one, and acknowledging every write address would make a bus scan find
 * devices that are not there.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */
#pragma once

#include "hw/qdev-core.h"
#include "qom/object.h"
#include "hw/gpio/esp32_gpio.h"
#include "hw/misc/esp_vpb.h"

#define TYPE_ESP_GPIO_I2C "esp.gpio-i2c"
OBJECT_DECLARE_SIMPLE_TYPE(EspGpioI2cState, ESP_GPIO_I2C)

typedef enum EspGpioI2cPhase {
    I2C_IDLE,       /* no transaction we are part of */
    I2C_ADDRESS,    /* clocking in the address byte */
    I2C_ADDR_ACK,   /* we are holding SDA low for the ninth clock */
    I2C_SEND,       /* shifting a byte out */
    I2C_MASTER_ACK, /* the master says whether it wants another byte */
    I2C_IGNORE,     /* not ours, or finished: wait for STOP or START */
} EspGpioI2cPhase;

struct EspGpioI2cState {
    DeviceState parent_obj;

    EspVpbClient vpb;

    /* The controller the board file puts the device on. */
    uint8_t vpb_controller;

    /* GPIO numbers, -1 where unset. */
    int32_t sda_gpio;
    int32_t scl_gpio;

    /* Where to hold a line low. Set by the machine; the GPIO model owns it. */
    Esp32GpioState *gpio;

    /* Line levels as of the previous change, for edge detection. */
    bool sda;
    bool scl;

    EspGpioI2cPhase phase;
    uint8_t shift;      /* the address byte being assembled */
    uint8_t bits;       /* bits clocked so far in this byte, either direction */
    uint8_t address;    /* 7-bit */
    uint8_t tx_byte;    /* what is being shifted out */
    bool master_acked;  /* the master's answer to the last byte we sent */
};

/** Is there anything to do: a bus to ask and the two pins. */
bool esp_gpio_i2c_active(EspGpioI2cState *s);

/**
 * Called by the GPIO model after every write that moved an output or a
 * direction, with both whole words. `opaque` is the EspGpioI2cState.
 */
void esp_gpio_i2c_changed(void *opaque, uint64_t out, uint64_t enable);
