/*
 * ESP32-S3 general-purpose SPI controller (GP-SPI2 / GP-SPI3).
 *
 * Transfers complete synchronously inside the register write that starts
 * them. Real hardware clocks bits out over many microseconds and raises an
 * interrupt at the end, but firmware observes completion only through
 * SPI_DMA_INT_RAW.trans_done, so finishing immediately is indistinguishable
 * from finishing fast.
 *
 * DMA is not modelled. Drivers fall back to the W0..W15 buffer for transfers
 * up to 64 bytes, which covers control traffic; a DMA-only path is logged
 * rather than silently doing nothing.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */

#include "qemu/osdep.h"
#include "qemu/log.h"
#include "hw/hw.h"
#include "hw/irq.h"
#include "hw/sysbus.h"
#include "hw/qdev-properties.h"
#include "migration/vmstate.h"
#include "hw/ssi/esp32s3_gpspi.h"

/*
 * Recompute masked status and drive the interrupt line.
 *
 * Every path that touches RAW or ENA must go through here. ESP-IDF's
 * spi_master does not only wait for a hardware-raised completion -- it kicks
 * its own ISR by writing SPI_DMA_INT_SET, and if that write updates the status
 * registers without asserting the line, the driver queues transactions that
 * are never serviced and eventually reports ESP_ERR_TIMEOUT with the queue
 * full.
 */
static void esp32s3_gpspi_update_irq(Esp32s3GpspiState *s)
{
    s->regs[R_GPSPI_DMA_INT_ST] =
        s->regs[R_GPSPI_DMA_INT_RAW] & s->regs[R_GPSPI_DMA_INT_ENA];

    qemu_set_irq(s->irq, s->regs[R_GPSPI_DMA_INT_ST] != 0);
}

/* Which chip select is asserted, or -1 when the driver has selected none. */
static int esp32s3_gpspi_active_cs(Esp32s3GpspiState *s)
{
    uint32_t dis = FIELD_EX32(s->regs[R_GPSPI_MISC], GPSPI_MISC, CS_DIS);

    for (int i = 0; i < ESP32S3_GPSPI_CS_COUNT; i++) {
        if (!(dis & (1u << i))) {
            return i;
        }
    }
    return -1;
}

/* Byte `index` of the W0..W15 payload buffer. */
static uint8_t esp32s3_gpspi_buf_read(Esp32s3GpspiState *s, unsigned index)
{
    uint32_t word = s->regs[R_GPSPI_W0 + (index / 4)];
    return (word >> ((index % 4) * 8)) & 0xff;
}

static void esp32s3_gpspi_buf_write(Esp32s3GpspiState *s, unsigned index,
                                    uint8_t value)
{
    uint32_t *word = &s->regs[R_GPSPI_W0 + (index / 4)];
    unsigned shift = (index % 4) * 8;

    *word = (*word & ~(0xffu << shift)) | ((uint32_t)value << shift);
}

/*
 * Clock one byte out and one byte in.
 *
 * With nothing attached to the bus, ssi_transfer returns zero. Real MISO
 * floats high, and drivers probing for a device read 0xFF as "absent" but
 * 0x00 as a device answering with zeros -- so an empty bus reporting zeros
 * would look like phantom hardware.
 */
static uint8_t esp32s3_gpspi_xfer_byte(Esp32s3GpspiState *s, uint8_t out,
                                       bool anything_attached)
{
    uint32_t in = ssi_transfer(s->spi, out);
    return anything_attached ? (uint8_t)in : 0xff;
}

static void esp32s3_gpspi_transfer(Esp32s3GpspiState *s)
{
    const uint32_t user = s->regs[R_GPSPI_USER];
    const int cs = esp32s3_gpspi_active_cs(s);

    /*
     * ssi_transfer works even with no peripheral attached, so ask the bus
     * whether anything is there rather than inferring it from the data.
     */
    bool attached = s->spi != NULL &&
                    !QTAILQ_EMPTY(&BUS(s->spi)->children);

    if (cs >= 0) {
        qemu_set_irq(s->cs_gpio[cs], 0);
    }

    /* Command phase: up to 16 bits, most significant byte first. */
    if (FIELD_EX32(user, GPSPI_USER, USR_COMMAND)) {
        uint32_t value = FIELD_EX32(s->regs[R_GPSPI_USER2], GPSPI_USER2,
                                    USR_COMMAND_VALUE);
        unsigned bits = FIELD_EX32(s->regs[R_GPSPI_USER2], GPSPI_USER2,
                                   USR_COMMAND_BITLEN) + 1;
        for (int shift = ((bits + 7) / 8) * 8 - 8; shift >= 0; shift -= 8) {
            esp32s3_gpspi_xfer_byte(s, (value >> shift) & 0xff, attached);
        }
    }

    /* Address phase, likewise most significant byte first. */
    if (FIELD_EX32(user, GPSPI_USER, USR_ADDR)) {
        uint32_t addr = s->regs[R_GPSPI_ADDR];
        unsigned bits = FIELD_EX32(s->regs[R_GPSPI_USER1], GPSPI_USER1,
                                   USR_ADDR_BITLEN) + 1;
        for (int shift = ((bits + 7) / 8) * 8 - 8; shift >= 0; shift -= 8) {
            esp32s3_gpspi_xfer_byte(s, (addr >> shift) & 0xff, attached);
        }
    }

    /* Dummy phase: clocks with no meaningful data either way. */
    if (FIELD_EX32(user, GPSPI_USER, USR_DUMMY)) {
        unsigned cycles = FIELD_EX32(s->regs[R_GPSPI_USER1], GPSPI_USER1,
                                     USR_DUMMY_CYCLELEN) + 1;
        for (unsigned i = 0; i < cycles / 8; i++) {
            esp32s3_gpspi_xfer_byte(s, 0xff, attached);
        }
    }

    /* Data phase. MS_DLEN holds the bit count minus one. */
    bool mosi = FIELD_EX32(user, GPSPI_USER, USR_MOSI);
    bool miso = FIELD_EX32(user, GPSPI_USER, USR_MISO);

    if (mosi || miso) {
        unsigned bits = FIELD_EX32(s->regs[R_GPSPI_MS_DLEN], GPSPI_MS_DLEN,
                                   MS_DATA_BITLEN) + 1;
        unsigned bytes = (bits + 7) / 8;

        if (bytes > ESP32S3_GPSPI_BUF_BYTES) {
            /*
             * Longer than the register buffer means the driver used DMA,
             * which we do not model. Clamp rather than read past the buffer,
             * and say so: a truncated display update is confusing on its own.
             */
            qemu_log_mask(LOG_UNIMP,
                          "%s: %u-byte transfer needs DMA (max %u); truncating\n",
                          __func__, bytes, ESP32S3_GPSPI_BUF_BYTES);
            bytes = ESP32S3_GPSPI_BUF_BYTES;
        }

        for (unsigned i = 0; i < bytes; i++) {
            uint8_t out = mosi ? esp32s3_gpspi_buf_read(s, i) : 0xff;
            uint8_t in = esp32s3_gpspi_xfer_byte(s, out, attached);
            if (miso) {
                esp32s3_gpspi_buf_write(s, i, in);
            }
        }
    }

    if (cs >= 0) {
        qemu_set_irq(s->cs_gpio[cs], 1);
    }

    /* Completion is the whole point: this is what the poll loop waits on. */
    s->regs[R_GPSPI_DMA_INT_RAW] |= GPSPI_TRANS_DONE_INT;
    esp32s3_gpspi_update_irq(s);
}

static uint64_t esp32s3_gpspi_read(void *opaque, hwaddr addr, unsigned int size)
{
    Esp32s3GpspiState *s = ESP32S3_GPSPI(opaque);
    hwaddr index = addr / sizeof(uint32_t);

    if (index >= ESP32S3_GPSPI_REG_COUNT) {
        qemu_log_mask(LOG_GUEST_ERROR,
                      "%s: read past the register block at 0x%" HWADDR_PRIx "\n",
                      __func__, addr);
        return 0;
    }
    return s->regs[index];
}

static void esp32s3_gpspi_write(void *opaque, hwaddr addr, uint64_t value,
                                unsigned int size)
{
    Esp32s3GpspiState *s = ESP32S3_GPSPI(opaque);
    hwaddr index = addr / sizeof(uint32_t);

    /*
     * Register-level trace, off unless `-d unimp` is passed. Bringing up a
     * driver against this controller is mostly a question of which registers
     * it touches and in what order, and guessing that is far slower than
     * looking.
     */
    qemu_log_mask(LOG_UNIMP, "gpspi: W %03" HWADDR_PRIx " = %08x\n",
                  addr, (uint32_t)value);

    if (index >= ESP32S3_GPSPI_REG_COUNT) {
        qemu_log_mask(LOG_GUEST_ERROR,
                      "%s: write past the register block at 0x%" HWADDR_PRIx "\n",
                      __func__, addr);
        return;
    }

    switch (addr) {
    case A_GPSPI_CMD: {
        /*
         * SPI_UPDATE latches configuration and is not a transfer. Storing it
         * would leave the bit set, and drivers poll it to see the latch
         * complete, so it must read back as zero.
         */
        bool start = FIELD_EX32((uint32_t)value, GPSPI_CMD, USR);
        s->regs[index] = (uint32_t)value &
                         ~(R_GPSPI_CMD_UPDATE_MASK | R_GPSPI_CMD_USR_MASK);
        if (start) {
            esp32s3_gpspi_transfer(s);
        }
        break;
    }

    case A_GPSPI_DMA_INT_CLR:
        /* Write-one-to-clear against the raw register. */
        s->regs[R_GPSPI_DMA_INT_RAW] &= ~(uint32_t)value;
        esp32s3_gpspi_update_irq(s);
        break;

    case A_GPSPI_DMA_INT_SET:
        /* Software-triggered interrupt; spi_master uses this to run its ISR. */
        s->regs[R_GPSPI_DMA_INT_RAW] |= (uint32_t)value;
        esp32s3_gpspi_update_irq(s);
        break;

    case A_GPSPI_DMA_INT_RAW:
    case A_GPSPI_DMA_INT_ST:
        /* Status registers; writes are ignored by the hardware. */
        break;

    case A_GPSPI_DMA_INT_ENA:
        s->regs[index] = (uint32_t)value;
        esp32s3_gpspi_update_irq(s);
        break;

    default:
        s->regs[index] = (uint32_t)value;
        break;
    }
}

static const MemoryRegionOps esp32s3_gpspi_ops = {
    .read = esp32s3_gpspi_read,
    .write = esp32s3_gpspi_write,
    .endianness = DEVICE_LITTLE_ENDIAN,
    .valid.min_access_size = 4,
    .valid.max_access_size = 4,
};

static void esp32s3_gpspi_reset_hold(Object *obj, ResetType type)
{
    Esp32s3GpspiState *s = ESP32S3_GPSPI(obj);

    memset(s->regs, 0, sizeof(s->regs));
    /* Every chip select released. */
    s->regs[R_GPSPI_MISC] = R_GPSPI_MISC_CS_DIS_MASK;
    for (int i = 0; i < ESP32S3_GPSPI_CS_COUNT; i++) {
        qemu_set_irq(s->cs_gpio[i], 1);
    }
    qemu_irq_lower(s->irq);
}

static void esp32s3_gpspi_init(Object *obj)
{
    Esp32s3GpspiState *s = ESP32S3_GPSPI(obj);
    SysBusDevice *sbd = SYS_BUS_DEVICE(obj);

    memory_region_init_io(&s->iomem, obj, &esp32s3_gpspi_ops, s,
                          TYPE_ESP32S3_GPSPI, ESP32S3_GPSPI_MEM_SIZE);
    sysbus_init_mmio(sbd, &s->iomem);
    sysbus_init_irq(sbd, &s->irq);

    s->spi = ssi_create_bus(DEVICE(s), "spi");
    qdev_init_gpio_out_named(DEVICE(s), &s->cs_gpio[0], SSI_GPIO_CS,
                             ESP32S3_GPSPI_CS_COUNT);
}

static const VMStateDescription vmstate_esp32s3_gpspi = {
    .name = TYPE_ESP32S3_GPSPI,
    .version_id = 1,
    .minimum_version_id = 1,
    .fields = (const VMStateField[]) {
        VMSTATE_UINT32_ARRAY(regs, Esp32s3GpspiState, ESP32S3_GPSPI_REG_COUNT),
        VMSTATE_END_OF_LIST()
    }
};

static void esp32s3_gpspi_class_init(ObjectClass *klass, void *data)
{
    DeviceClass *dc = DEVICE_CLASS(klass);
    ResettableClass *rc = RESETTABLE_CLASS(klass);

    rc->phases.hold = esp32s3_gpspi_reset_hold;
    dc->vmsd = &vmstate_esp32s3_gpspi;
}

static const TypeInfo esp32s3_gpspi_info = {
    .name = TYPE_ESP32S3_GPSPI,
    .parent = TYPE_SYS_BUS_DEVICE,
    .instance_size = sizeof(Esp32s3GpspiState),
    .instance_init = esp32s3_gpspi_init,
    .class_init = esp32s3_gpspi_class_init,
};

static void esp32s3_gpspi_register_types(void)
{
    type_register_static(&esp32s3_gpspi_info);
}

type_init(esp32s3_gpspi_register_types)
