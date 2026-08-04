/*
 * ESP32-S3 SENS (SAR ADC) peripheral.
 *
 * Conversions complete immediately. That is not how the hardware behaves --
 * a real SAR conversion takes microseconds -- but nothing in firmware can
 * tell the difference through this interface, because the only way to observe
 * completion is the done bit, and the only way to observe timing is a
 * separate timer. Modelling the delay would add a timer and a state machine
 * to buy nothing.
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
 * Apply the start/done handshake to one of the MEASn_CTRL2 registers.
 *
 * Firmware sets START, then polls DONE. Clearing START must clear DONE too,
 * or a driver that re-arms by writing zero would see a stale completion and
 * read the previous sample.
 */
static void esp32s3_sens_update_meas(Esp32s3SensState *s, hwaddr index,
                                     uint32_t raw)
{
    uint32_t value = s->regs[index];

    if (value & SENS_MEAS_START_BIT) {
        value &= ~SENS_MEAS_DATA_MASK;
        value |= (raw & SENS_SAR_MAX_RAW) << SENS_MEAS_DATA_SHIFT;
        value |= SENS_MEAS_DONE_BIT;
    } else {
        value &= ~SENS_MEAS_DONE_BIT;
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

    /*
     * Registers we do not model are still stored, so firmware reads back what
     * it wrote. Configuration it never re-reads costs nothing to keep, and
     * doing so avoids surprising drivers that verify their own writes.
     */
    s->regs[index] = (uint32_t)value;

    switch (addr) {
    case A_SENS_SAR_MEAS1_CTRL2:
        esp32s3_sens_update_meas(s, index, s->adc1_raw);
        break;
    case A_SENS_SAR_MEAS2_CTRL2:
        esp32s3_sens_update_meas(s, index, s->adc2_raw);
        break;
    default:
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
     * Mid-scale by default: a believable reading for either converter without
     * pretending to model any particular board's divider network.
     */
    DEFINE_PROP_UINT32("adc1-raw", Esp32s3SensState, adc1_raw, 2048),
    DEFINE_PROP_UINT32("adc2-raw", Esp32s3SensState, adc2_raw, 2048),
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
