/*
 * ESP32-S3 I2C controller. See the header for what this is and why.
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
#include "hw/i2c/esp32s3_i2c.h"

static void esp32s3_i2c_update_irq(Esp32s3I2CState *s)
{
    uint32_t status = s->regs[R_I2C_INT_RAW] & s->regs[R_I2C_INT_ENA];

    s->regs[R_I2C_INT_STATUS] = status;
    /*
     * Visible under `-d unimp`. Which bits are raised against which are
     * enabled is the whole question when a driver reports a transaction that
     * never completed: raising an event nobody enabled looks identical, from
     * the guest side, to raising nothing at all.
     */
    qemu_log_mask(LOG_UNIMP, "i2c%u: irq raw=%08x ena=%08x st=%08x\n",
                  s->vpb_controller, s->regs[R_I2C_INT_RAW],
                  s->regs[R_I2C_INT_ENA], status);
    qemu_set_irq(s->irq, status != 0);
}

static void esp32s3_i2c_raise(Esp32s3I2CState *s, uint32_t mask)
{
    s->regs[R_I2C_INT_RAW] |= mask;
}

/*
 * Hand the buffered write to whatever owns this address.
 *
 * Called when the command list reaches something that ends a write: a read,
 * a stop, or a repeated start. A zero-length write still goes out -- that is
 * exactly what an address probe is, and its only result is the acknowledgement.
 */
static void esp32s3_i2c_flush_write(Esp32s3I2CState *s, bool stop)
{
    if (!s->addressed) {
        return;
    }

    bool nacked = false;
    bool delivered = esp_vpb_i2c_write(&s->vpb, s->vpb_controller, s->address,
                                       s->pending, s->pending_len, stop,
                                       &nacked);
    /*
     * Nothing listening reads as an empty bus, which is a NACK at every
     * address -- the same thing firmware sees with no device fitted.
     */
    if (!delivered || nacked) {
        s->nacked = true;
    }
    s->pending_len = 0;
}

static void esp32s3_i2c_do_write(Esp32s3I2CState *s, uint32_t cmd)
{
    uint32_t count = FIELD_EX32(cmd, I2C_COMD, BYTE_NUM);

    for (uint32_t i = 0; i < count && !fifo8_is_empty(&s->tx_fifo); i++) {
        uint8_t byte = fifo8_pop(&s->tx_fifo);

        if (!s->addressed) {
            /*
             * The first byte after a start is the address and direction. The
             * direction bit is not carried onward: the transaction type says
             * which way the data goes.
             */
            s->address = byte >> 1;
            s->addressed = true;
            s->nacked = false;
            s->pending_len = 0;
            continue;
        }

        if (s->pending_len < sizeof(s->pending)) {
            s->pending[s->pending_len++] = byte;
        }
    }
}

static void esp32s3_i2c_do_read(Esp32s3I2CState *s, uint32_t cmd)
{
    uint32_t count = FIELD_EX32(cmd, I2C_COMD, BYTE_NUM);

    /* A read is preceded by the register address, written without a stop. */
    esp32s3_i2c_flush_write(s, false);

    uint8_t buf[ESP32S3_I2C_FIFO_LENGTH];
    count = MIN(count, sizeof(buf));
    memset(buf, 0xff, count);

    bool nacked = false;
    bool got = esp_vpb_i2c_read(&s->vpb, s->vpb_controller, s->address, buf,
                                count, &nacked);
    if (!got || nacked) {
        s->nacked = true;
    }

    for (uint32_t i = 0; i < count; i++) {
        if (fifo8_num_free(&s->rx_fifo) == 0) {
            qemu_log_mask(LOG_GUEST_ERROR, "%s: RX FIFO overflow\n", __func__);
            break;
        }
        fifo8_push(&s->rx_fifo, buf[i]);
    }
}

/*
 * Run the command list.
 *
 * Stops at STOP or END, which is what the hardware does: END means the driver
 * has more commands to load and will restart the list, so state has to
 * survive between runs.
 */
static void esp32s3_i2c_run(Esp32s3I2CState *s)
{
    for (unsigned i = 0; i < ESP32S3_I2C_CMD_COUNT; i++) {
        uint32_t cmd = s->cmd[i];
        unsigned opcode = FIELD_EX32(cmd, I2C_COMD, OPCODE);

        s->cmd[i] = FIELD_DP32(cmd, I2C_COMD, DONE, 1);

        switch (opcode) {
        case I2C_OPCODE_RSTART:
            /* A repeated start closes the write without releasing the bus. */
            esp32s3_i2c_flush_write(s, false);
            s->addressed = false;
            break;

        case I2C_OPCODE_WRITE:
            esp32s3_i2c_do_write(s, cmd);
            break;

        case I2C_OPCODE_READ:
            esp32s3_i2c_do_read(s, cmd);
            break;

        case I2C_OPCODE_STOP:
            esp32s3_i2c_flush_write(s, true);
            s->addressed = false;
            esp32s3_i2c_raise(s, R_I2C_INT_RAW_TRANS_COMPLETE_MASK);
            if (s->nacked) {
                esp32s3_i2c_raise(s, R_I2C_INT_RAW_NACK_MASK);
            }
            esp32s3_i2c_update_irq(s);
            return;

        case I2C_OPCODE_END:
            /*
             * More commands are coming. The buffered write is deliberately
             * *not* flushed: the driver is splitting one logical transfer
             * across command lists, and flushing here would turn it into two
             * on the wire.
             */
            esp32s3_i2c_raise(s, R_I2C_INT_RAW_END_DETECT_MASK);
            if (s->nacked) {
                esp32s3_i2c_raise(s, R_I2C_INT_RAW_NACK_MASK);
            }
            esp32s3_i2c_update_irq(s);
            return;

        default:
            qemu_log_mask(LOG_GUEST_ERROR, "%s: bad opcode %u at %u\n",
                          __func__, opcode, i);
            break;
        }
    }

    /* Ran off the end of the list without a stop; report it done anyway. */
    esp32s3_i2c_raise(s, R_I2C_INT_RAW_TRANS_COMPLETE_MASK);
    esp32s3_i2c_update_irq(s);
}

static uint64_t esp32s3_i2c_read_reg(void *opaque, hwaddr addr, unsigned size)
{
    Esp32s3I2CState *s = ESP32S3_I2C(opaque);
    unsigned index = addr / sizeof(uint32_t);

    if (addr >= A_I2C_COMD &&
        addr < A_I2C_COMD + ESP32S3_I2C_CMD_COUNT * sizeof(uint32_t)) {
        return s->cmd[(addr - A_I2C_COMD) / sizeof(uint32_t)];
    }

    switch (addr) {
    case A_I2C_SR: {
        uint32_t r = 0;
        r = FIELD_DP32(r, I2C_SR, RXFIFO_CNT, fifo8_num_used(&s->rx_fifo));
        r = FIELD_DP32(r, I2C_SR, TXFIFO_CNT, fifo8_num_used(&s->tx_fifo));
        return r;
    }

    case A_I2C_DATA:
        if (fifo8_is_empty(&s->rx_fifo)) {
            /*
             * Reading an empty FIFO is a driver bug, not something to model
             * exactly; 0xff is what an undriven, pulled-up bus looks like.
             */
            qemu_log_mask(LOG_GUEST_ERROR, "%s: read from an empty FIFO\n",
                          __func__);
            return 0xff;
        }
        return fifo8_pop(&s->rx_fifo);

    case A_I2C_INT_STATUS:
        return s->regs[R_I2C_INT_RAW] & s->regs[R_I2C_INT_ENA];

    case A_I2C_FIFO_ST:
        return 0;

    default:
        return index < ARRAY_SIZE(s->regs) ? s->regs[index] : 0;
    }
}

static void esp32s3_i2c_write_reg(void *opaque, hwaddr addr, uint64_t value,
                                  unsigned size)
{
    Esp32s3I2CState *s = ESP32S3_I2C(opaque);
    unsigned index = addr / sizeof(uint32_t);

    if (addr >= A_I2C_COMD &&
        addr < A_I2C_COMD + ESP32S3_I2C_CMD_COUNT * sizeof(uint32_t)) {
        s->cmd[(addr - A_I2C_COMD) / sizeof(uint32_t)] = (uint32_t)value;
        return;
    }

    switch (addr) {
    case A_I2C_CTR:
        /* Write-one-to-trigger, and it does not stay set. */
        if (FIELD_EX32((uint32_t)value, I2C_CTR, TRANS_START)) {
            s->regs[R_I2C_CTR] =
                (uint32_t)value & ~R_I2C_CTR_TRANS_START_MASK;
            esp32s3_i2c_run(s);
        } else {
            s->regs[R_I2C_CTR] = (uint32_t)value;
        }
        break;

    case A_I2C_DATA:
        if (fifo8_num_free(&s->tx_fifo) == 0) {
            qemu_log_mask(LOG_GUEST_ERROR, "%s: write to a full FIFO\n",
                          __func__);
            break;
        }
        fifo8_push(&s->tx_fifo, (uint8_t)value);
        break;

    case A_I2C_FIFO_CONF:
        if (FIELD_EX32((uint32_t)value, I2C_FIFO_CONF, RX_FIFO_RST)) {
            fifo8_reset(&s->rx_fifo);
        }
        if (FIELD_EX32((uint32_t)value, I2C_FIFO_CONF, TX_FIFO_RST)) {
            fifo8_reset(&s->tx_fifo);
        }
        if (FIELD_EX32((uint32_t)value, I2C_FIFO_CONF, NONFIFO_EN)) {
            qemu_log_mask(LOG_UNIMP, "%s: non-FIFO mode is not modelled\n",
                          __func__);
        }
        /* The reset bits are self-clearing; the thresholds are not. */
        s->regs[R_I2C_FIFO_CONF] = (uint32_t)value &
            ~(R_I2C_FIFO_CONF_RX_FIFO_RST_MASK |
              R_I2C_FIFO_CONF_TX_FIFO_RST_MASK);
        break;

    case A_I2C_INT_CLR:
        s->regs[R_I2C_INT_RAW] &= ~(uint32_t)value;
        esp32s3_i2c_update_irq(s);
        break;

    case A_I2C_INT_ENA:
        s->regs[R_I2C_INT_ENA] = (uint32_t)value;
        esp32s3_i2c_update_irq(s);
        break;

    case A_I2C_INT_RAW:
    case A_I2C_INT_STATUS:
    case A_I2C_SR:
        /* Status registers; the hardware ignores writes. */
        break;

    default:
        if (index < ARRAY_SIZE(s->regs)) {
            s->regs[index] = (uint32_t)value;
        }
        break;
    }
}

static const MemoryRegionOps esp32s3_i2c_ops = {
    .read = esp32s3_i2c_read_reg,
    .write = esp32s3_i2c_write_reg,
    .endianness = DEVICE_LITTLE_ENDIAN,
};

static void esp32s3_i2c_reset_hold(Object *obj, ResetType type)
{
    Esp32s3I2CState *s = ESP32S3_I2C(obj);

    fifo8_reset(&s->rx_fifo);
    fifo8_reset(&s->tx_fifo);
    memset(s->regs, 0, sizeof(s->regs));
    memset(s->cmd, 0, sizeof(s->cmd));
    s->addressed = false;
    s->pending_len = 0;
    s->nacked = false;
    esp_vpb_close(&s->vpb);
    qemu_irq_lower(s->irq);
}

static void esp32s3_i2c_init(Object *obj)
{
    Esp32s3I2CState *s = ESP32S3_I2C(obj);
    SysBusDevice *sbd = SYS_BUS_DEVICE(obj);

    memory_region_init_io(&s->iomem, obj, &esp32s3_i2c_ops, s,
                          TYPE_ESP32S3_I2C, ESP32S3_I2C_MEM_SIZE);
    sysbus_init_mmio(sbd, &s->iomem);
    sysbus_init_irq(sbd, &s->irq);

    fifo8_create(&s->rx_fifo, ESP32S3_I2C_FIFO_LENGTH);
    fifo8_create(&s->tx_fifo, ESP32S3_I2C_FIFO_LENGTH);
}

static const VMStateDescription vmstate_esp32s3_i2c = {
    .name = TYPE_ESP32S3_I2C,
    .version_id = 1,
    .minimum_version_id = 1,
    .fields = (const VMStateField[]) {
        VMSTATE_UINT32_ARRAY(regs, Esp32s3I2CState,
                             ESP32S3_I2C_MEM_SIZE / sizeof(uint32_t)),
        VMSTATE_UINT32_ARRAY(cmd, Esp32s3I2CState, ESP32S3_I2C_CMD_COUNT),
        VMSTATE_END_OF_LIST()
    }
};

static Property esp32s3_i2c_properties[] = {
    /* TCP port of the external peripheral server; zero disables forwarding. */
    DEFINE_PROP_UINT16("vpb-port", Esp32s3I2CState, vpb.port, 0),
    /* Reported to device models so they can tell I2C0 from I2C1. */
    DEFINE_PROP_UINT8("vpb-controller", Esp32s3I2CState, vpb_controller, 0),
    DEFINE_PROP_END_OF_LIST(),
};

static void esp32s3_i2c_class_init(ObjectClass *klass, void *data)
{
    DeviceClass *dc = DEVICE_CLASS(klass);
    ResettableClass *rc = RESETTABLE_CLASS(klass);

    rc->phases.hold = esp32s3_i2c_reset_hold;
    dc->vmsd = &vmstate_esp32s3_i2c;
    device_class_set_props(dc, esp32s3_i2c_properties);
}

static const TypeInfo esp32s3_i2c_info = {
    .name = TYPE_ESP32S3_I2C,
    .parent = TYPE_SYS_BUS_DEVICE,
    .instance_size = sizeof(Esp32s3I2CState),
    .instance_init = esp32s3_i2c_init,
    .class_init = esp32s3_i2c_class_init,
};

static void esp32s3_i2c_register_types(void)
{
    type_register_static(&esp32s3_i2c_info);
}

type_init(esp32s3_i2c_register_types)
