/*
 * ESP32-S3 analog master interface. See the header for what this is and why
 * it is a stub rather than a model.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */

#include "qemu/osdep.h"
#include "qemu/log.h"
#include "hw/hw.h"
#include "hw/sysbus.h"
#include "migration/vmstate.h"
#include "hw/misc/esp32s3_ana_mst.h"

/*
 * Bits that always read set, because whatever they report finished is
 * finished by the time firmware can ask.
 *
 * A table rather than a switch: every entry here was found the same way --
 * a boot stops, the PC lands in a two-instruction poll, and the register and
 * mask come out of the disassembly. Keeping them together makes the list of
 * what firmware waits for visible at a glance, and adding the next one a
 * one-line change.
 */
static const struct {
    hwaddr offset;
    uint32_t always_set;
} esp32s3_ana_mst_done[] = {
    { A_ANA_MST_CONF0,  R_ANA_MST_CONF0_BBPLL_CAL_DONE_MASK },
    { A_ANA_MST_CMD,    R_ANA_MST_CMD_DONE_MASK },
    { A_ANA_MST_STATUS, R_ANA_MST_STATUS_READY_MASK },
};

static uint64_t esp32s3_ana_mst_read(void *opaque, hwaddr addr, unsigned size)
{
    Esp32s3AnaMstState *s = ESP32S3_ANA_MST(opaque);
    unsigned index = addr / sizeof(uint32_t);
    uint32_t value = index < ARRAY_SIZE(s->regs) ? s->regs[index] : 0;

    for (unsigned i = 0; i < ARRAY_SIZE(esp32s3_ana_mst_done); i++) {
        if (esp32s3_ana_mst_done[i].offset == addr) {
            value |= esp32s3_ana_mst_done[i].always_set;
            break;
        }
    }

    /*
     * Visible under `-d unimp`. The PHY blob is closed, so the only way to
     * learn what it is waiting for is to watch what it reads and how often:
     * a register polled thousands of times is a readiness bit whose value we
     * are getting wrong.
     */
    qemu_log_mask(LOG_UNIMP, "ana_mst: R %03x = %08x\n",
                  (unsigned)addr, value);
    return value;
}

static void esp32s3_ana_mst_write(void *opaque, hwaddr addr, uint64_t value,
                                  unsigned size)
{
    Esp32s3AnaMstState *s = ESP32S3_ANA_MST(opaque);
    unsigned index = addr / sizeof(uint32_t);

    /*
     * Stored and handed back, so a driver that reads a field to modify it
     * sees what it wrote. Nothing acts on the values: the registers on the
     * far side of this master control an analog front end that does not
     * exist here.
     */
    if (index < ARRAY_SIZE(s->regs)) {
        s->regs[index] = (uint32_t)value;
    }
    qemu_log_mask(LOG_UNIMP, "ana_mst: W %03x = %08x\n",
                  (unsigned)addr, (uint32_t)value);
}

static const MemoryRegionOps esp32s3_ana_mst_ops = {
    .read = esp32s3_ana_mst_read,
    .write = esp32s3_ana_mst_write,
    .endianness = DEVICE_LITTLE_ENDIAN,
};

static void esp32s3_ana_mst_reset_hold(Object *obj, ResetType type)
{
    Esp32s3AnaMstState *s = ESP32S3_ANA_MST(obj);

    memset(s->regs, 0, sizeof(s->regs));
}

static void esp32s3_ana_mst_init(Object *obj)
{
    Esp32s3AnaMstState *s = ESP32S3_ANA_MST(obj);
    SysBusDevice *sbd = SYS_BUS_DEVICE(obj);

    memory_region_init_io(&s->iomem, obj, &esp32s3_ana_mst_ops, s,
                          TYPE_ESP32S3_ANA_MST, ESP32S3_ANA_MST_MEM_SIZE);
    sysbus_init_mmio(sbd, &s->iomem);
}

static const VMStateDescription vmstate_esp32s3_ana_mst = {
    .name = TYPE_ESP32S3_ANA_MST,
    .version_id = 1,
    .minimum_version_id = 1,
    .fields = (const VMStateField[]) {
        VMSTATE_UINT32_ARRAY(regs, Esp32s3AnaMstState,
                             ESP32S3_ANA_MST_MEM_SIZE / sizeof(uint32_t)),
        VMSTATE_END_OF_LIST()
    }
};

static void esp32s3_ana_mst_class_init(ObjectClass *klass, void *data)
{
    DeviceClass *dc = DEVICE_CLASS(klass);
    ResettableClass *rc = RESETTABLE_CLASS(klass);

    rc->phases.hold = esp32s3_ana_mst_reset_hold;
    dc->vmsd = &vmstate_esp32s3_ana_mst;
}

static const TypeInfo esp32s3_ana_mst_info = {
    .name = TYPE_ESP32S3_ANA_MST,
    .parent = TYPE_SYS_BUS_DEVICE,
    .instance_size = sizeof(Esp32s3AnaMstState),
    .instance_init = esp32s3_ana_mst_init,
    .class_init = esp32s3_ana_mst_class_init,
};

static void esp32s3_ana_mst_register_types(void)
{
    type_register_static(&esp32s3_ana_mst_info);
}

type_init(esp32s3_ana_mst_register_types)
