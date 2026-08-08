/*
 * ESP32-S3 SENS (SAR ADC) peripheral.
 *
 * Conversions complete immediately, which is measured rather than assumed:
 * on a real T-Deck Plus the done bit and the sample are both already valid by
 * the CPU's first read after starting one. There is nothing to defer.
 *
 * The start/done handshake is likewise measured. See the comment on
 * esp32s3_sens_write_meas -- clearing START does not clear DONE, which is the
 * opposite of what seemed sensible when this was first written.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */

#include "qemu/osdep.h"
#include "qemu/log.h"
#include "hw/hw.h"
#include "hw/sysbus.h"
#include "hw/qdev-properties.h"
#include "migration/vmstate.h"
#include "hw/misc/esp32s3_sens.h"

/*
 * Apply a guest write to one of the MEASn_CTRL2 registers.
 *
 * DATA and DONE are hardware-owned: guest writes do not reach them, and they
 * survive until the next conversion overwrites them.
 *
 * In particular, clearing START does **not** clear DONE. Measured on a real
 * T-Deck Plus:
 *
 *     w 6000880c 0x60000  ->  readback 0x000709ec   (start+force, done, sample)
 *     w 6000880c 0x0      ->  readback 0x000109ec   (done still set, stale sample)
 *
 * So a driver that re-arms by writing zero and then polls DONE really does see
 * a stale completion and read the previous sample. Silicon offers no
 * protection there.
 *
 * An earlier version of this cleared DONE on that write, reasoning that it
 * would be the sane behaviour. It is -- and modelling it made the emulator
 * *safer* than the hardware, which is the worst direction for a divergence to
 * point: firmware carrying that race would pass here and be flaky on the
 * board.
 */
static void esp32s3_sens_write_meas(Esp32s3SensState *s, hwaddr index,
                                    uint32_t written, uint32_t raw)
{
    const uint32_t hw_owned = SENS_MEAS_DATA_MASK | SENS_MEAS_DONE_BIT;

    /* Guest bits, with the hardware-owned ones carried over untouched. */
    uint32_t value = (written & ~hw_owned) | (s->regs[index] & hw_owned);

    if (written & SENS_MEAS_START_BIT) {
        /*
         * Conversions complete within a single CPU read -- confirmed on
         * hardware -- so there is nothing to defer here.
         */
        value &= ~SENS_MEAS_DATA_MASK;
        value |= (raw & SENS_SAR_MAX_RAW) << SENS_MEAS_DATA_SHIFT;
        value |= SENS_MEAS_DONE_BIT;
    }

    s->regs[index] = value;
}

static uint64_t esp32s3_sens_read(void *opaque, hwaddr addr, unsigned int size)
{
    Esp32s3SensState *s = ESP32S3_SENS(opaque);
    hwaddr index = addr / sizeof(uint32_t);

    if (index >= ESP32S3_SENS_REG_COUNT) {
        qemu_log_mask(LOG_GUEST_ERROR,
                      "%s: read past the SENS block at 0x%" HWADDR_PRIx "\n",
                      __func__, addr);
        return 0;
    }

    /*
     * A conversion finishes as soon as it is asked for. Reporting READY only
     * while powered up matters: firmware powers the sensor down between
     * readings, and a permanently-ready sensor would let a driver read a
     * value it never actually requested.
     */
    if (addr == A_SENS_SAR_TSENS_CTRL) {
        uint32_t value = s->regs[index];
        if (FIELD_EX32(value, SENS_SAR_TSENS_CTRL, POWER_UP)) {
            value = FIELD_DP32(value, SENS_SAR_TSENS_CTRL, READY, 1);
            value = FIELD_DP32(value, SENS_SAR_TSENS_CTRL, OUT,
                               ESP32S3_SENS_TSENS_RAW);
        }
        return value;
    }

    return s->regs[index];
}

static void esp32s3_sens_write(void *opaque, hwaddr addr, uint64_t value,
                               unsigned int size)
{
    Esp32s3SensState *s = ESP32S3_SENS(opaque);
    hwaddr index = addr / sizeof(uint32_t);

    if (index >= ESP32S3_SENS_REG_COUNT) {
        qemu_log_mask(LOG_GUEST_ERROR,
                      "%s: write past the SENS block at 0x%" HWADDR_PRIx "\n",
                      __func__, addr);
        return;
    }

    switch (addr) {
    case A_SENS_SAR_MEAS1_CTRL2:
        esp32s3_sens_write_meas(s, index, (uint32_t)value, s->adc1_raw);
        break;
    case A_SENS_SAR_MEAS2_CTRL2:
        esp32s3_sens_write_meas(s, index, (uint32_t)value, s->adc2_raw);
        break;
    default:
        /*
         * Registers we do not model are still stored, so firmware reads back
         * what it wrote. Configuration it never re-reads costs nothing to
         * keep, and doing so avoids surprising drivers that verify their own
         * writes.
         */
        s->regs[index] = (uint32_t)value;
        break;
    }
}

static const MemoryRegionOps esp32s3_sens_ops = {
    .read = esp32s3_sens_read,
    .write = esp32s3_sens_write,
    .endianness = DEVICE_LITTLE_ENDIAN,
    .valid.min_access_size = 4,
    .valid.max_access_size = 4,
};

static void esp32s3_sens_reset_hold(Object *obj, ResetType type)
{
    Esp32s3SensState *s = ESP32S3_SENS(obj);
    memset(s->regs, 0, sizeof(s->regs));
}

static void esp32s3_sens_init(Object *obj)
{
    Esp32s3SensState *s = ESP32S3_SENS(obj);
    SysBusDevice *sbd = SYS_BUS_DEVICE(obj);

    memory_region_init_io(&s->iomem, obj, &esp32s3_sens_ops, s,
                          TYPE_ESP32S3_SENS, ESP32S3_SENS_MEM_SIZE);
    sysbus_init_mmio(sbd, &s->iomem);
}

static Property esp32s3_sens_properties[] = {
    /*
     * A live T-Deck Plus battery reads 2525-2533 counts, so 2528 is a real
     * measurement rather than the mid-scale placeholder this used to be.
     * Firmware that converts counts to a voltage and decides the battery is
     * flat gets a plausible answer.
     */
    DEFINE_PROP_UINT32("adc1-raw", Esp32s3SensState, adc1_raw, 2528),
    DEFINE_PROP_UINT32("adc2-raw", Esp32s3SensState, adc2_raw, 2528),
    DEFINE_PROP_END_OF_LIST(),
};

static const VMStateDescription vmstate_esp32s3_sens = {
    .name = TYPE_ESP32S3_SENS,
    .version_id = 1,
    .minimum_version_id = 1,
    .fields = (const VMStateField[]) {
        VMSTATE_UINT32_ARRAY(regs, Esp32s3SensState, ESP32S3_SENS_REG_COUNT),
        VMSTATE_END_OF_LIST()
    }
};

static void esp32s3_sens_class_init(ObjectClass *klass, void *data)
{
    DeviceClass *dc = DEVICE_CLASS(klass);
    ResettableClass *rc = RESETTABLE_CLASS(klass);

    rc->phases.hold = esp32s3_sens_reset_hold;
    dc->vmsd = &vmstate_esp32s3_sens;
    device_class_set_props(dc, esp32s3_sens_properties);
}

static const TypeInfo esp32s3_sens_info = {
    .name = TYPE_ESP32S3_SENS,
    .parent = TYPE_SYS_BUS_DEVICE,
    .instance_size = sizeof(Esp32s3SensState),
    .instance_init = esp32s3_sens_init,
    .class_init = esp32s3_sens_class_init,
};

static void esp32s3_sens_register_types(void)
{
    type_register_static(&esp32s3_sens_info);
}

type_init(esp32s3_sens_register_types)
