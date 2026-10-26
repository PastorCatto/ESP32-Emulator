/*
 * SPI bit-banged over GPIO. See the header for why this exists.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */
#include "qemu/osdep.h"
#include "hw/qdev-properties.h"
#include "hw/misc/esp_gpio_spi.h"

static bool pin_level(uint64_t out, int32_t pin)
{
    return pin >= 0 && pin < 64 && ((out >> pin) & 1);
}

bool esp_gpio_spi_active(EspGpioSpiState *s)
{
    return s->vpb.port != 0 && s->sclk_gpio >= 0 && s->mosi_gpio >= 0;
}

void esp_gpio_spi_out_changed(void *opaque, uint64_t out, uint64_t enable)
{
    EspGpioSpiState *s = opaque;

    const bool sclk = pin_level(out, s->sclk_gpio);
    const bool mosi = pin_level(out, s->mosi_gpio);
    const bool dc = pin_level(out, s->dc_gpio);
    /* With no CS pin the device is always selected, which reads as low. */
    const bool cs = s->cs_gpio >= 0 && pin_level(out, s->cs_gpio);

    /*
     * Either edge of CS abandons a half-clocked byte. A device resynchronises
     * on its select line, and so does this.
     */
    if (cs != s->cs) {
        s->bits = 0;
        s->shift = 0;
    }

    /*
     * Rising SCLK samples MOSI. The level used is the one held before this
     * write: firmware sets the data line and then raises the clock, and if
     * both ever changed in one write the data would not have had its setup
     * time.
     */
    if (sclk && !s->sclk && !cs) {
        s->shift = (uint8_t)((s->shift << 1) | (s->mosi ? 1 : 0));
        if (++s->bits == 8) {
            const uint8_t byte = s->shift;

            s->bits = 0;
            s->shift = 0;
            /*
             * One byte at a time is deliberate. The client already merges a
             * run of bytes for the same device and data/command level into
             * one frame and flushes it when the guest stops drawing, so a
             * 150 KiB screen still crosses the socket as a handful of frames.
             */
            esp_vpb_spi_transfer(&s->vpb, s->vpb_controller, s->cs_line,
                                 s->dc_gpio >= 0 ? (dc ? 1 : 0) : -1,
                                 &byte, 1, NULL, 0);
        }
    }

    s->sclk = sclk;
    s->mosi = mosi;
    s->cs = cs;
}

static Property esp_gpio_spi_properties[] = {
    DEFINE_PROP_UINT16("vpb-port", EspGpioSpiState, vpb.port, 0),
    DEFINE_PROP_UINT8("vpb-controller", EspGpioSpiState, vpb_controller, 2),
    DEFINE_PROP_UINT8("cs-line", EspGpioSpiState, cs_line, 0),
    DEFINE_PROP_INT32("sclk-gpio", EspGpioSpiState, sclk_gpio, -1),
    DEFINE_PROP_INT32("mosi-gpio", EspGpioSpiState, mosi_gpio, -1),
    DEFINE_PROP_INT32("cs-gpio", EspGpioSpiState, cs_gpio, -1),
    DEFINE_PROP_INT32("dc-gpio", EspGpioSpiState, dc_gpio, -1),
    DEFINE_PROP_END_OF_LIST(),
};

static void esp_gpio_spi_reset(Object *obj, ResetType type)
{
    EspGpioSpiState *s = ESP_GPIO_SPI(obj);

    s->sclk = false;
    s->mosi = false;
    /* Idle high: nothing is selected until the firmware drives CS low. */
    s->cs = true;
    s->bits = 0;
    s->shift = 0;
}

static void esp_gpio_spi_class_init(ObjectClass *klass, void *data)
{
    DeviceClass *dc = DEVICE_CLASS(klass);
    ResettableClass *rc = RESETTABLE_CLASS(klass);

    device_class_set_props(dc, esp_gpio_spi_properties);
    rc->phases.hold = esp_gpio_spi_reset;
    /* Created by the machine from -global settings, never from the command line. */
    dc->user_creatable = false;
}

static const TypeInfo esp_gpio_spi_info = {
    .name = TYPE_ESP_GPIO_SPI,
    .parent = TYPE_DEVICE,
    .instance_size = sizeof(EspGpioSpiState),
    .class_init = esp_gpio_spi_class_init,
};

static void esp_gpio_spi_register_types(void)
{
    type_register_static(&esp_gpio_spi_info);
}

type_init(esp_gpio_spi_register_types)
