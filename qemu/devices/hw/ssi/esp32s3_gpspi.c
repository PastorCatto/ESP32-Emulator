/*
 * ESP32-S3 general-purpose SPI controller (GP-SPI2 / GP-SPI3).
 *
 * The bytes move synchronously when the guest starts a transfer, but
 * completion is reported on a timer. Finishing inside the guest's store to
 * SPI_CMD lets the ISR re-enter the driver before it has finished the
 * bookkeeping that follows starting a transaction -- a race that cannot
 * happen on hardware, where a transfer always takes microseconds.
 *
 * DMA is not modelled. Drivers fall back to the W0..W15 buffer for transfers
 * up to 64 bytes, which covers control traffic; a DMA-only path is logged
 * rather than silently doing nothing.
 *
 * Several behaviours here are measured against a real T-Deck Plus rather than
 * inferred from the TRM, via the PURR OS hardware probe: the clock gate, the
 * 16x register mirroring, SPI_DMA_CONF's stuck low bits, SPI_DATE's constant,
 * and MISO reading zero. Each is marked at its use.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */

#include "qemu/osdep.h"
#include "qemu/log.h"
#include "qemu/timer.h"
#include "qemu/main-loop.h"
#include "hw/hw.h"
#include "hw/irq.h"
#include "hw/sysbus.h"
#include "hw/qdev-properties.h"
#include "migration/vmstate.h"
#include "hw/ssi/esp32s3_gpspi.h"

static uint64_t esp32s3_gpspi_duration_ns(Esp32s3GpspiState *s, unsigned bytes);
static void esp32s3_gpspi_done(void *opaque);
static void esp32s3_gpspi_deassert(void *opaque);

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
    /* Rule 1: combinational, recomputed on every write to either register. */
    s->regs[R_GPSPI_DMA_INT_ST] =
        s->regs[R_GPSPI_DMA_INT_RAW] & s->regs[R_GPSPI_DMA_INT_ENA];

    bool want = s->regs[R_GPSPI_DMA_INT_ST] != 0;

    if (want) {
        /*
         * Rule 4: assert whenever ST is non-zero, level-style rather than on
         * the transition. This is what makes arming ENA on an already-set RAW
         * work, instead of losing the wakeup.
         */
        if (!s->line_high) {
            s->line_high = true;
            qemu_irq_raise(s->irq);
        }
        return;
    }

    /*
     * Rule 5: hold the line rather than dropping it here, so the CPU takes the
     * interrupt once more. Dropping it synchronously gives one ISR entry where
     * hardware gives two.
     *
     * A bottom half, not a timer. A virtual-clock delay is unusable for this:
     * without icount the CPU runs an unbounded number of instructions between
     * timer checks, so any window long enough to guarantee one re-entry also
     * admits thousands. Measured with the probe's `intfire`, a 1us hold gave
     * count=1862 against hardware's 2. A bottom half runs at the next main
     * loop iteration, which bounds the re-entry to roughly the one that
     * hardware exhibits.
     */
    if (s->line_high) {
        qemu_bh_schedule(s->deassert_bh);
    }
}

/* The held-off deassert from rule 5 finally landing. */
static void esp32s3_gpspi_deassert(void *opaque)
{
    Esp32s3GpspiState *s = ESP32S3_GPSPI(opaque);

    /* Re-check: the condition may have come back during the hold window. */
    if (s->regs[R_GPSPI_DMA_INT_ST] == 0 && s->line_high) {
        s->line_high = false;
        qemu_irq_lower(s->irq);
    }
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

/* Clock one byte out and one byte in. */
static uint8_t esp32s3_gpspi_xfer_byte(Esp32s3GpspiState *s, uint8_t out,
                                       bool anything_attached)
{
    uint32_t in = ssi_transfer(s->spi, out);
    /*
     * Measured on a T-Deck Plus: MISO reads 0x00 with the panel selected. The
     * ST7789 shares MISO (GPIO38) with SD and LoRa and does not drive it, so
     * zeros are the correct value here rather than a failed read.
     */
    return anything_attached ? (uint8_t)in : 0x00;
}

/*
 * Hand a whole transaction to the external device models.
 *
 * Returns true when it was delivered, in which case `buf` holds whatever was
 * clocked back. Returns false when nothing is listening, and the caller falls
 * back to the in-QEMU SSI bus -- so a board can mix the two, and the emulator
 * still runs with no device process attached.
 */
static bool esp32s3_gpspi_via_vpb(Esp32s3GpspiState *s, uint8_t *buf,
                                  unsigned bytes, bool want_miso)
{
    int cs = esp32s3_gpspi_active_cs(s);

    if (cs < 0) {
        return false;
    }
    return esp_vpb_spi_transfer(&s->vpb, s->vpb_controller, (uint8_t)cs,
                                s->dc_level, buf, bytes,
                                buf, want_miso ? bytes : 0);
}

/* Is either DMA direction armed for this transfer? */
static bool esp32s3_gpspi_dma_active(Esp32s3GpspiState *s)
{
    uint32_t conf = s->regs[R_GPSPI_DMA_CONF];

    return s->gdma != NULL &&
           (FIELD_EX32(conf, GPSPI_DMA_CONF, DMA_TX_ENA) ||
            FIELD_EX32(conf, GPSPI_DMA_CONF, DMA_RX_ENA));
}

/*
 * Move the data phase through the GDMA engine.
 *
 * TX pulls the outgoing bytes out of memory, RX pushes what came back into
 * it. Full duplex runs both against the same byte count, which is what an
 * `spi_device_transmit` with both buffers set does.
 *
 * Descriptor writeback is deliberately asymmetric, because the hardware is:
 * measured on a real T-Deck Plus, an RX descriptor comes back with `owner`
 * cleared, `length` filled in and `suc_eof` set, while the TX descriptor in
 * the *same* transfer is untouched. Firmware therefore cannot learn a TX
 * buffer is reusable by polling `owner` -- it has to wait for TRANS_DONE --
 * and modelling both directions the same way "for symmetry" gets one of them
 * wrong. The GDMA engine already implements this split, so this only has to
 * avoid undoing it.
 */
static unsigned esp32s3_gpspi_dma_data(Esp32s3GpspiState *s, unsigned bytes,
                                       bool mosi, bool miso, bool attached)
{
    uint32_t conf = s->regs[R_GPSPI_DMA_CONF];
    bool tx = mosi && FIELD_EX32(conf, GPSPI_DMA_CONF, DMA_TX_ENA);
    bool rx = miso && FIELD_EX32(conf, GPSPI_DMA_CONF, DMA_RX_ENA);
    uint32_t chan;

    if (bytes > ESP32S3_GPSPI_DMA_MAX) {
        qemu_log_mask(LOG_UNIMP,
                      "%s: %u-byte DMA transfer exceeds the %u-byte staging "
                      "buffer; truncating\n",
                      __func__, bytes, ESP32S3_GPSPI_DMA_MAX);
        bytes = ESP32S3_GPSPI_DMA_MAX;
    }

    /*
     * Default to idle-bus bytes so a TX-less transfer still clocks something
     * sensible out, matching the programmed-I/O path.
     */
    memset(s->dma_buf, 0xff, bytes);

    if (tx) {
        if (!esp_gdma_get_channel_periph(s->gdma, s->gdma_periph,
                                         ESP_GDMA_OUT_IDX, &chan) ||
            !esp_gdma_read_channel(s->gdma, chan, s->dma_buf, bytes)) {
            qemu_log_mask(LOG_GUEST_ERROR,
                          "%s: DMA TX enabled but no usable out channel\n",
                          __func__);
            return 0;
        }
    }

    /* Clock the bytes over the bus, collecting MISO in place. */
    if (!esp32s3_gpspi_via_vpb(s, s->dma_buf, bytes, rx)) {
        for (unsigned i = 0; i < bytes; i++) {
            uint8_t in = esp32s3_gpspi_xfer_byte(s, s->dma_buf[i], attached);
            s->dma_buf[i] = in;
        }
    }

    if (rx) {
        if (!esp_gdma_get_channel_periph(s->gdma, s->gdma_periph,
                                         ESP_GDMA_IN_IDX, &chan) ||
            !esp_gdma_write_channel(s->gdma, chan, s->dma_buf, bytes)) {
            qemu_log_mask(LOG_GUEST_ERROR,
                          "%s: DMA RX enabled but no usable in channel\n",
                          __func__);
        }
    }

    return bytes;
}

static void esp32s3_gpspi_transfer(Esp32s3GpspiState *s)
{
    const uint32_t user = s->regs[R_GPSPI_USER];
    const int cs = esp32s3_gpspi_active_cs(s);
    unsigned total_bytes = 0;

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
            total_bytes++;
        }
    }

    /* Address phase, likewise most significant byte first. */
    if (FIELD_EX32(user, GPSPI_USER, USR_ADDR)) {
        uint32_t addr = s->regs[R_GPSPI_ADDR];
        unsigned bits = FIELD_EX32(s->regs[R_GPSPI_USER1], GPSPI_USER1,
                                   USR_ADDR_BITLEN) + 1;
        for (int shift = ((bits + 7) / 8) * 8 - 8; shift >= 0; shift -= 8) {
            esp32s3_gpspi_xfer_byte(s, (addr >> shift) & 0xff, attached);
            total_bytes++;
        }
    }

    /* Dummy phase: clocks with no meaningful data either way. */
    if (FIELD_EX32(user, GPSPI_USER, USR_DUMMY)) {
        unsigned cycles = FIELD_EX32(s->regs[R_GPSPI_USER1], GPSPI_USER1,
                                     USR_DUMMY_CYCLELEN) + 1;
        for (unsigned i = 0; i < cycles / 8; i++) {
            esp32s3_gpspi_xfer_byte(s, 0xff, attached);
            total_bytes++;
        }
    }

    /* Data phase. MS_DLEN holds the bit count minus one. */
    bool mosi = FIELD_EX32(user, GPSPI_USER, USR_MOSI);
    bool miso = FIELD_EX32(user, GPSPI_USER, USR_MISO);

    if (mosi || miso) {
        unsigned bits = FIELD_EX32(s->regs[R_GPSPI_MS_DLEN], GPSPI_MS_DLEN,
                                   MS_DATA_BITLEN) + 1;
        unsigned bytes = (bits + 7) / 8;

        if (esp32s3_gpspi_dma_active(s)) {
            total_bytes += esp32s3_gpspi_dma_data(s, bytes, mosi, miso, attached);
        } else {
            if (bytes > ESP32S3_GPSPI_BUF_BYTES) {
                /*
                 * Longer than the register buffer with no DMA enabled: the
                 * driver has asked for something the hardware could not do
                 * this way either. Clamp rather than read past the buffer.
                 */
                qemu_log_mask(LOG_GUEST_ERROR,
                              "%s: %u-byte programmed transfer exceeds the "
                              "%u-byte buffer; truncating\n",
                              __func__, bytes, ESP32S3_GPSPI_BUF_BYTES);
                bytes = ESP32S3_GPSPI_BUF_BYTES;
            }
            uint8_t staging[ESP32S3_GPSPI_BUF_BYTES];
            for (unsigned i = 0; i < bytes; i++) {
                staging[i] = mosi ? esp32s3_gpspi_buf_read(s, i) : 0xff;
            }

            if (!esp32s3_gpspi_via_vpb(s, staging, bytes, miso)) {
                for (unsigned i = 0; i < bytes; i++) {
                    staging[i] = esp32s3_gpspi_xfer_byte(s, staging[i], attached);
                }
            }

            if (miso) {
                for (unsigned i = 0; i < bytes; i++) {
                    esp32s3_gpspi_buf_write(s, i, staging[i]);
                }
            }
            total_bytes += bytes;
        }
    }

    if (cs >= 0) {
        qemu_set_irq(s->cs_gpio[cs], 1);
    }

    /*
     * The bytes have moved, but completion is deferred. Reporting it here
     * would make the transfer finish inside the guest's store to SPI_CMD,
     * so the ISR could run before the driver had finished the bookkeeping
     * that follows starting a transaction. Real hardware always takes some
     * microseconds; taking zero is its own kind of wrong.
     */
    uint64_t ns = esp32s3_gpspi_duration_ns(s, total_bytes);
    s->busy = true;
    timer_mod_ns(s->done_timer,
                 qemu_clock_get_ns(QEMU_CLOCK_VIRTUAL) + ns);
}

/* Estimate how long `bytes` would take at the configured clock. */
static uint64_t esp32s3_gpspi_duration_ns(Esp32s3GpspiState *s, unsigned bytes)
{
    /*
     * SPI_CLOCK encodes a divider off the 80MHz APB clock. Deriving the exact
     * rate is more precision than anything here needs: the point is a delay
     * that is small but non-zero, so the floor usually wins.
     */
    uint64_t ns = ((uint64_t)bytes * 8 * 1000ULL) / 80; /* 80 MHz, ns */

    return ns < ESP32S3_GPSPI_MIN_XFER_NS ? ESP32S3_GPSPI_MIN_XFER_NS : ns;
}

/* Transfer finished: raise completion and let the driver's ISR run. */
static void esp32s3_gpspi_done(void *opaque)
{
    Esp32s3GpspiState *s = ESP32S3_GPSPI(opaque);

    s->busy = false;

    /*
     * USR clearing and TRANS_DONE rising are one event, not two. Splitting
     * them lets a driver polling either one disagree with the other about
     * whether the transfer finished.
     */
    s->regs[R_GPSPI_CMD] &= ~R_GPSPI_CMD_USR_MASK;
    s->regs[R_GPSPI_DMA_INT_RAW] |= GPSPI_TRANS_DONE_INT;
    esp32s3_gpspi_update_irq(s);
}

/*
 * Is the peripheral's master clock running?
 *
 * IDF sets SPI_CLK_GATE when a *device* is added to the bus, not when the bus
 * is initialised. Measured on hardware: with the gate clear the whole register
 * file reads as zero -- including SPI_DATE, a hardwired constant -- and
 * CMD.USR latches high and never clears. Firmware that configures SPI2
 * correctly in every other respect still hangs forever if this is not
 * modelled.
 */
static bool esp32s3_gpspi_clocked(Esp32s3GpspiState *s)
{
    return (s->regs[R_GPSPI_CLK_GATE] & ESP32S3_GPSPI_CLK_EN) != 0;
}

static uint64_t esp32s3_gpspi_read(void *opaque, hwaddr addr, unsigned int size)
{
    Esp32s3GpspiState *s = ESP32S3_GPSPI(opaque);
    /* The register file is mirrored every 0x100 across the 4 KiB window. */
    hwaddr reg = addr & ESP32S3_GPSPI_ADDR_MASK;
    hwaddr index = reg / sizeof(uint32_t);

    /*
     * An unclocked peripheral reads as all-zeros on silicon. It does not
     * stall, and it does not return reset values.
     */
    if (!esp32s3_gpspi_clocked(s) && reg != A_GPSPI_CLK_GATE) {
        return 0;
    }

    switch (reg) {
    case A_GPSPI_DATE:
        return ESP32S3_GPSPI_DATE_VALUE;

    case A_GPSPI_DMA_CONF:
        return s->regs[index] | ESP32S3_GPSPI_DMA_CONF_SET;

    default:
        return s->regs[index];
    }
}

static void esp32s3_gpspi_write(void *opaque, hwaddr addr, uint64_t value,
                                unsigned int size)
{
    Esp32s3GpspiState *s = ESP32S3_GPSPI(opaque);
    hwaddr reg = addr & ESP32S3_GPSPI_ADDR_MASK;
    hwaddr index = reg / sizeof(uint32_t);

    /*
     * Register-level trace, off unless `-d unimp` is passed. Bringing up a
     * driver against this controller is mostly a question of which registers
     * it touches and in what order, and guessing that is far slower than
     * looking.
     */
    qemu_log_mask(LOG_UNIMP, "gpspi: W %03" HWADDR_PRIx " = %08x\n",
                  reg, (uint32_t)value);

    /*
     * Writes to an unclocked peripheral go nowhere, except to the gate itself
     * -- otherwise there would be no way to turn it on.
     */
    if (!esp32s3_gpspi_clocked(s) && reg != A_GPSPI_CLK_GATE) {
        return;
    }

    switch (reg) {
    case A_GPSPI_CMD: {
        /*
         * SPI_UPDATE latches configuration and is not a transfer. Measured as
         * self-clearing immediately, and drivers poll it to see the latch
         * complete, so it must read back as zero straight away.
         *
         * SPI_USR is the opposite: it stays set for the duration of the
         * transfer and clears *at the same moment* TRANS_DONE is raised.
         * Measured on hardware as a one-cycle gap against 1600-2600 cycles of
         * jitter, which is no gap at all. Clearing it here instead would let
         * a driver that watches USR conclude the transfer finished while
         * TRANS_DONE still read zero -- and firmware watching the other one
         * would then hang.
         */
        bool start = FIELD_EX32((uint32_t)value, GPSPI_CMD, USR);
        s->regs[index] = (uint32_t)value & ~R_GPSPI_CMD_UPDATE_MASK;
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

    timer_del(s->done_timer);
    s->busy = false;
    s->line_high = false;
    esp_vpb_close(&s->vpb);

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
                          TYPE_ESP32S3_GPSPI, ESP32S3_GPSPI_WINDOW_SIZE);
    sysbus_init_mmio(sbd, &s->iomem);
    sysbus_init_irq(sbd, &s->irq);

    s->spi = ssi_create_bus(DEVICE(s), "spi");
    qdev_init_gpio_out_named(DEVICE(s), &s->cs_gpio[0], SSI_GPIO_CS,
                             ESP32S3_GPSPI_CS_COUNT);

    s->done_timer = timer_new_ns(QEMU_CLOCK_VIRTUAL, esp32s3_gpspi_done, s);
    s->deassert_bh = qemu_bh_new(esp32s3_gpspi_deassert, s);

    /* No data/command pin known until a board wires one. */
    s->dc_level = -1;
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

static Property esp32s3_gpspi_properties[] = {
    /*
     * TCP port of the external peripheral server. Zero, the default, keeps
     * everything inside QEMU so the emulator runs standalone.
     */
    DEFINE_PROP_UINT16("vpb-port", Esp32s3GpspiState, vpb.port, 0),
    /* Reported to the device models so they can tell SPI2 from SPI3. */
    DEFINE_PROP_UINT8("vpb-controller", Esp32s3GpspiState, vpb_controller, 2),
    DEFINE_PROP_END_OF_LIST(),
};

static void esp32s3_gpspi_class_init(ObjectClass *klass, void *data)
{
    DeviceClass *dc = DEVICE_CLASS(klass);
    ResettableClass *rc = RESETTABLE_CLASS(klass);

    rc->phases.hold = esp32s3_gpspi_reset_hold;
    dc->vmsd = &vmstate_esp32s3_gpspi;
    device_class_set_props(dc, esp32s3_gpspi_properties);
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
