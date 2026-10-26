/*
 * The slave side of an I2C bus driven by hand over GPIO. See the header.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */
#include "qemu/osdep.h"
#include "hw/qdev-properties.h"
#include "hw/misc/esp_gpio_i2c.h"

/*
 * What a pin reads as on the bus: the chip's drive wired-AND with whatever else
 * is holding the line low. Open drain means the chip only ever drives low, by
 * enabling its output with the latch at zero, and lets go by disabling it.
 */
static bool bus_level(const EspGpioI2cState *s, uint64_t out, uint64_t enable,
                      int32_t pin)
{
    if (pin < 0 || pin >= 64) {
        return true;
    }
    const bool chip_high = !((enable >> pin) & 1) || ((out >> pin) & 1);
    const bool held_low = s->gpio && ((s->gpio->ext_low >> pin) & 1);

    return chip_high && !held_low;
}

static void hold_sda(EspGpioI2cState *s, bool low)
{
    if (s->gpio && s->sda_gpio >= 0) {
        esp32_gpio_set_ext_low(s->gpio, s->sda_gpio, low);
    }
}

/*
 * Ask the device for its next byte. It is fetched when the master is about to
 * clock it out rather than when it asked, because the device decides what to
 * report at the moment it is read: a keyboard hands over the oldest pending
 * key, and fetching early would take a key nobody collects.
 */
static bool fetch_byte(EspGpioI2cState *s)
{
    bool nacked = false;
    uint8_t byte = 0;

    if (!esp_vpb_i2c_read(&s->vpb, s->vpb_controller, s->address, &byte, 1,
                          &nacked) || nacked) {
        return false;
    }
    s->tx_byte = byte;
    return true;
}

/* A 0 pulls SDA low, a 1 lets go. Most significant bit first. */
static void drive_bit(EspGpioI2cState *s)
{
    hold_sda(s, !((s->tx_byte >> (7 - s->bits)) & 1));
}

static void start_condition(EspGpioI2cState *s)
{
    /* A repeated START lands here too, and begins a fresh address. */
    s->phase = I2C_ADDRESS;
    s->bits = 0;
    s->shift = 0;
    hold_sda(s, false);
}

static void stop_condition(EspGpioI2cState *s)
{
    s->phase = I2C_IDLE;
    hold_sda(s, false);
}

/* Data is valid while the clock is high. */
static void clock_rising(EspGpioI2cState *s, bool sda)
{
    switch (s->phase) {
    case I2C_ADDRESS:
        s->shift = (uint8_t)((s->shift << 1) | (sda ? 1 : 0));
        s->bits++;
        break;
    case I2C_MASTER_ACK:
        /* The master pulls SDA low to ask for another byte. */
        s->master_acked = !sda;
        break;
    default:
        break;
    }
}

/* A device changes SDA only while the clock is low. */
static void clock_falling(EspGpioI2cState *s)
{
    switch (s->phase) {
    case I2C_ADDRESS:
        if (s->bits == 8) {
            const bool read = s->shift & 1;

            s->address = s->shift >> 1;
            /*
             * Acknowledge only what the device model confirms is there, which
             * is the same answer a bus scan would get from a real controller.
             * A write address is left alone: see the header.
             */
            if (read && fetch_byte(s)) {
                hold_sda(s, true);
                s->phase = I2C_ADDR_ACK;
            } else {
                s->phase = I2C_IGNORE;
            }
        }
        break;

    case I2C_ADDR_ACK:
        /* The acknowledge clock has ended; the first bit goes out now. */
        s->bits = 0;
        s->phase = I2C_SEND;
        drive_bit(s);
        break;

    case I2C_SEND:
        if (++s->bits == 8) {
            /* Let go, so the master can say whether it wants another. */
            hold_sda(s, false);
            s->phase = I2C_MASTER_ACK;
        } else {
            drive_bit(s);
        }
        break;

    case I2C_MASTER_ACK:
        if (s->master_acked && fetch_byte(s)) {
            s->bits = 0;
            s->phase = I2C_SEND;
            drive_bit(s);
        } else {
            s->phase = I2C_IGNORE;
        }
        break;

    default:
        break;
    }
}

bool esp_gpio_i2c_active(EspGpioI2cState *s)
{
    return s->vpb.port != 0 && s->sda_gpio >= 0 && s->scl_gpio >= 0;
}

void esp_gpio_i2c_changed(void *opaque, uint64_t out, uint64_t enable)
{
    EspGpioI2cState *s = opaque;

    const bool sda = bus_level(s, out, enable, s->sda_gpio);
    const bool scl = bus_level(s, out, enable, s->scl_gpio);

    if (scl && s->scl) {
        /* SDA moving while the clock is high is never data. */
        if (s->sda && !sda) {
            start_condition(s);
        } else if (!s->sda && sda) {
            stop_condition(s);
        }
    } else if (scl && !s->scl) {
        clock_rising(s, sda);
    } else if (!scl && s->scl) {
        clock_falling(s);
    }

    /* Our own pull may have moved SDA; remember what the bus reads now. */
    s->sda = bus_level(s, out, enable, s->sda_gpio);
    s->scl = scl;
}

static Property esp_gpio_i2c_properties[] = {
    DEFINE_PROP_UINT16("vpb-port", EspGpioI2cState, vpb.port, 0),
    DEFINE_PROP_UINT8("vpb-controller", EspGpioI2cState, vpb_controller, 0),
    DEFINE_PROP_INT32("sda-gpio", EspGpioI2cState, sda_gpio, -1),
    DEFINE_PROP_INT32("scl-gpio", EspGpioI2cState, scl_gpio, -1),
    DEFINE_PROP_END_OF_LIST(),
};

static void esp_gpio_i2c_reset(Object *obj, ResetType type)
{
    EspGpioI2cState *s = ESP_GPIO_I2C(obj);

    /* Both lines idle high. */
    s->sda = true;
    s->scl = true;
    s->phase = I2C_IDLE;
    s->bits = 0;
    s->shift = 0;
    s->master_acked = false;
}

static void esp_gpio_i2c_class_init(ObjectClass *klass, void *data)
{
    DeviceClass *dc = DEVICE_CLASS(klass);
    ResettableClass *rc = RESETTABLE_CLASS(klass);

    device_class_set_props(dc, esp_gpio_i2c_properties);
    rc->phases.hold = esp_gpio_i2c_reset;
    /* Created by the machine from -global settings, never from the command line. */
    dc->user_creatable = false;
}

static const TypeInfo esp_gpio_i2c_info = {
    .name = TYPE_ESP_GPIO_I2C,
    .parent = TYPE_DEVICE,
    .instance_size = sizeof(EspGpioI2cState),
    .class_init = esp_gpio_i2c_class_init,
};

static void esp_gpio_i2c_register_types(void)
{
    type_register_static(&esp_gpio_i2c_info);
}

type_init(esp_gpio_i2c_register_types)
