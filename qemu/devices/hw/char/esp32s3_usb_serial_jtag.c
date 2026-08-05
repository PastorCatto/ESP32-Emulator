/*
 * ESP32-S3 USB Serial/JTAG controller — serial endpoint.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */

#include "qemu/osdep.h"
#include "qemu/log.h"
#include "qemu/module.h"
#include "hw/hw.h"
#include "hw/irq.h"
#include "hw/sysbus.h"
#include "hw/qdev-properties.h"
#include "hw/qdev-properties-system.h"
#include "migration/vmstate.h"
#include "chardev/char-fe.h"
#include "hw/char/esp32s3_usb_serial_jtag.h"

static void esp32s3_usj_update_irq(Esp32s3UsjState *s)
{
    s->regs[R_USJ_INT_ST] = s->regs[R_USJ_INT_RAW] & s->regs[R_USJ_INT_ENA];
    qemu_set_irq(s->irq, s->regs[R_USJ_INT_ST] != 0);
}

/*
 * Hand the endpoint buffer to the host.
 *
 * Called both when firmware sets WR_DONE and when the 64-byte buffer fills,
 * because hardware flushes automatically at that point — a driver may write a
 * long string and only flush at the end, and without the automatic flush its
 * output would stall after the first 64 bytes.
 */
static void esp32s3_usj_flush_tx(Esp32s3UsjState *s)
{
    if (s->tx_len == 0) {
        return;
    }

    /*
     * Blocking write: this is a console, and dropping output because a pipe
     * was briefly full turns a missing log line into a debugging mystery.
     */
    qemu_chr_fe_write_all(&s->chr, s->tx, s->tx_len);
    s->tx_len = 0;

    /* The host has taken it, so the endpoint is empty again. */
    s->regs[R_USJ_INT_RAW] |= USJ_SERIAL_IN_EMPTY_INT;
    esp32s3_usj_update_irq(s);
}

static uint64_t esp32s3_usj_read(void *opaque, hwaddr addr, unsigned int size)
{
    Esp32s3UsjState *s = ESP32S3_USJ(opaque);
    hwaddr index = addr / sizeof(uint32_t);

    if (index >= ESP32S3_USJ_REG_COUNT) {
        return 0;
    }

    switch (addr) {
    case A_USJ_EP1: {
        /*
         * Reading the FIFO consumes a byte. The HAL checks DATA_AVAIL before
         * each read, but a read past the end must still not wander off the
         * buffer.
         */
        if (s->rx_len == 0) {
            return 0;
        }
        uint8_t b = s->rx[s->rx_head];
        s->rx_head = (s->rx_head + 1) % ESP32S3_USJ_RX_SIZE;
        s->rx_len--;

        /* Room again now that we have taken a byte. */
        qemu_chr_fe_accept_input(&s->chr);
        return b;
    }

    case A_USJ_EP1_CONF: {
        uint32_t v = 0;
        /* WR_DONE is write-only; it always reads back as zero. */
        if (s->tx_len < ESP32S3_USJ_EP_SIZE) {
            v |= R_USJ_EP1_CONF_SERIAL_IN_EP_DATA_FREE_MASK;
        }
        if (s->rx_len > 0) {
            v |= R_USJ_EP1_CONF_SERIAL_OUT_EP_DATA_AVAIL_MASK;
        }
        return v;
    }

    default:
        return s->regs[index];
    }
}

static void esp32s3_usj_write(void *opaque, hwaddr addr, uint64_t value,
                              unsigned int size)
{
    Esp32s3UsjState *s = ESP32S3_USJ(opaque);
    hwaddr index = addr / sizeof(uint32_t);

    if (index >= ESP32S3_USJ_REG_COUNT) {
        return;
    }

    switch (addr) {
    case A_USJ_EP1:
        if (s->tx_len < ESP32S3_USJ_EP_SIZE) {
            s->tx[s->tx_len++] = value & 0xff;
        }
        /* Hardware flushes on its own once the endpoint is full. */
        if (s->tx_len == ESP32S3_USJ_EP_SIZE) {
            esp32s3_usj_flush_tx(s);
        }
        break;

    case A_USJ_EP1_CONF:
        if (value & R_USJ_EP1_CONF_WR_DONE_MASK) {
            esp32s3_usj_flush_tx(s);
        }
        break;

    case A_USJ_INT_CLR:
        s->regs[R_USJ_INT_RAW] &= ~(uint32_t)value;
        esp32s3_usj_update_irq(s);
        break;

    case A_USJ_INT_ENA:
        s->regs[index] = (uint32_t)value;
        esp32s3_usj_update_irq(s);
        break;

    case A_USJ_INT_ST:
        /* Status only. */
        break;

    default:
        s->regs[index] = (uint32_t)value;
        break;
    }
}

static const MemoryRegionOps esp32s3_usj_ops = {
    .read = esp32s3_usj_read,
    .write = esp32s3_usj_write,
    .endianness = DEVICE_LITTLE_ENDIAN,
};

/* Backpressure: refuse host bytes we have nowhere to put. */
static int esp32s3_usj_can_receive(void *opaque)
{
    Esp32s3UsjState *s = ESP32S3_USJ(opaque);
    return ESP32S3_USJ_RX_SIZE - s->rx_len;
}

static void esp32s3_usj_receive(void *opaque, const uint8_t *buf, int size)
{
    Esp32s3UsjState *s = ESP32S3_USJ(opaque);

    for (int i = 0; i < size && s->rx_len < ESP32S3_USJ_RX_SIZE; i++) {
        unsigned tail = (s->rx_head + s->rx_len) % ESP32S3_USJ_RX_SIZE;
        s->rx[tail] = buf[i];
        s->rx_len++;
    }

    if (size > 0) {
        s->regs[R_USJ_INT_RAW] |= USJ_SERIAL_OUT_RECV_PKT_INT;
        esp32s3_usj_update_irq(s);
    }
}

static void esp32s3_usj_reset_hold(Object *obj, ResetType type)
{
    Esp32s3UsjState *s = ESP32S3_USJ(obj);

    memset(s->regs, 0, sizeof(s->regs));
    s->tx_len = 0;
    s->rx_head = 0;
    s->rx_len = 0;
    qemu_irq_lower(s->irq);
}

static void esp32s3_usj_realize(DeviceState *dev, Error **errp)
{
    Esp32s3UsjState *s = ESP32S3_USJ(dev);

    qemu_chr_fe_set_handlers(&s->chr, esp32s3_usj_can_receive,
                             esp32s3_usj_receive, NULL, NULL, s, NULL, true);
}

static void esp32s3_usj_init(Object *obj)
{
    Esp32s3UsjState *s = ESP32S3_USJ(obj);
    SysBusDevice *sbd = SYS_BUS_DEVICE(obj);

    memory_region_init_io(&s->iomem, obj, &esp32s3_usj_ops, s,
                          TYPE_ESP32S3_USJ, ESP32S3_USJ_MEM_SIZE);
    sysbus_init_mmio(sbd, &s->iomem);
    sysbus_init_irq(sbd, &s->irq);
}

static Property esp32s3_usj_properties[] = {
    DEFINE_PROP_CHR("chardev", Esp32s3UsjState, chr),
    DEFINE_PROP_END_OF_LIST(),
};

static const VMStateDescription vmstate_esp32s3_usj = {
    .name = TYPE_ESP32S3_USJ,
    .version_id = 1,
    .minimum_version_id = 1,
    .fields = (const VMStateField[]) {
        VMSTATE_UINT32_ARRAY(regs, Esp32s3UsjState, ESP32S3_USJ_REG_COUNT),
        VMSTATE_END_OF_LIST()
    }
};

static void esp32s3_usj_class_init(ObjectClass *klass, void *data)
{
    DeviceClass *dc = DEVICE_CLASS(klass);
    ResettableClass *rc = RESETTABLE_CLASS(klass);

    rc->phases.hold = esp32s3_usj_reset_hold;
    dc->realize = esp32s3_usj_realize;
    dc->vmsd = &vmstate_esp32s3_usj;
    device_class_set_props(dc, esp32s3_usj_properties);
}

static const TypeInfo esp32s3_usj_info = {
    .name = TYPE_ESP32S3_USJ,
    .parent = TYPE_SYS_BUS_DEVICE,
    .instance_size = sizeof(Esp32s3UsjState),
    .instance_init = esp32s3_usj_init,
    .class_init = esp32s3_usj_class_init,
};

static void esp32s3_usj_register_types(void)
{
    type_register_static(&esp32s3_usj_info);
}

type_init(esp32s3_usj_register_types)
