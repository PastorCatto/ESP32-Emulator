/*
 * ESP32-S3 I2C controller.
 *
 * Espressif's tree has an ESP32 I2C model, but nothing instantiates one on the
 * S3, so a board's I2C devices are invisible -- on a T-Deck that is the GT911
 * touchscreen and the BBQ20 keyboard, both of which the firmware probes during
 * startup and finds absent.
 *
 * The register map is close enough to the ESP32's that the field definitions
 * carry over; the difference is where the transaction goes. This one forwards
 * to device models outside QEMU over the virtual peripheral bus, like the SPI
 * controller does, rather than to QEMU's own I2CBus.
 *
 * Offsets and bit positions come from ESP-IDF v5.3.5,
 * components/soc/esp32s3/include/soc/i2c_reg.h.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */
#pragma once

#include "hw/hw.h"
#include "hw/sysbus.h"
#include "hw/registerfields.h"
#include "qemu/fifo8.h"
#include "hw/misc/esp_vpb.h"

#define TYPE_ESP32S3_I2C "i2c.esp32s3"
#define ESP32S3_I2C(obj) OBJECT_CHECK(Esp32s3I2CState, (obj), TYPE_ESP32S3_I2C)

#define ESP32S3_I2C_MEM_SIZE    0x100
/* 32 bytes each way, and firmware chunks longer transfers to match. */
#define ESP32S3_I2C_FIFO_LENGTH 32
#define ESP32S3_I2C_CMD_COUNT   8

REG32(I2C_SCL_LOW_PERIOD, 0x00)

REG32(I2C_CTR, 0x04)
    FIELD(I2C_CTR, MS_MODE, 4, 1)
    FIELD(I2C_CTR, TRANS_START, 5, 1)
    FIELD(I2C_CTR, CONF_UPGATE, 11, 1)

REG32(I2C_SR, 0x08)
    FIELD(I2C_SR, BUS_BUSY, 4, 1)
    FIELD(I2C_SR, RXFIFO_CNT, 8, 6)
    FIELD(I2C_SR, TXFIFO_CNT, 18, 6)

REG32(I2C_TO, 0x0c)
REG32(I2C_SLAVE_ADDR, 0x10)

REG32(I2C_FIFO_ST, 0x14)
    FIELD(I2C_FIFO_ST, RXFIFO_RADDR, 0, 5)
    FIELD(I2C_FIFO_ST, TXFIFO_WADDR, 10, 5)

REG32(I2C_FIFO_CONF, 0x18)
    FIELD(I2C_FIFO_CONF, RXFIFO_WM_THRHD, 0, 5)
    FIELD(I2C_FIFO_CONF, TXFIFO_WM_THRHD, 5, 5)
    FIELD(I2C_FIFO_CONF, NONFIFO_EN, 10, 1)
    FIELD(I2C_FIFO_CONF, RX_FIFO_RST, 12, 1)
    FIELD(I2C_FIFO_CONF, TX_FIFO_RST, 13, 1)

REG32(I2C_DATA, 0x1c)

/*
 * The interrupt registers share a layout. Bit 10 is NACK on the S3, where the
 * ESP32 calls the same bit ACK_ERR.
 */
REG32(I2C_INT_RAW, 0x20)
    FIELD(I2C_INT_RAW, RXFIFO_WM, 0, 1)
    FIELD(I2C_INT_RAW, TXFIFO_WM, 1, 1)
    FIELD(I2C_INT_RAW, END_DETECT, 3, 1)
    FIELD(I2C_INT_RAW, BYTE_TRANS_DONE, 4, 1)
    FIELD(I2C_INT_RAW, TRANS_COMPLETE, 7, 1)
    FIELD(I2C_INT_RAW, TIME_OUT, 8, 1)
    FIELD(I2C_INT_RAW, NACK, 10, 1)

REG32(I2C_INT_CLR, 0x24)
REG32(I2C_INT_ENA, 0x28)
REG32(I2C_INT_STATUS, 0x2c)

REG32(I2C_SDA_HOLD, 0x30)
REG32(I2C_SDA_SAMPLE, 0x34)
REG32(I2C_SCL_HIGH_PERIOD, 0x38)
REG32(I2C_SCL_START_HOLD, 0x40)
REG32(I2C_SCL_RSTART_SETUP, 0x44)
REG32(I2C_SCL_STOP_HOLD, 0x48)
REG32(I2C_SCL_STOP_SETUP, 0x4c)

REG32(I2C_COMD, 0x58)
    FIELD(I2C_COMD, BYTE_NUM, 0, 8)
    FIELD(I2C_COMD, ACK_CHECK_EN, 8, 1)
    FIELD(I2C_COMD, ACK_EXP, 9, 1)
    FIELD(I2C_COMD, ACK_VAL, 10, 1)
    FIELD(I2C_COMD, OPCODE, 11, 3)
    FIELD(I2C_COMD, DONE, 31, 1)

/*
 * Command opcodes, and they are *not* the ESP32's.
 *
 * The S3 renumbered them: RESTART moved from 0 to 6, and READ and STOP
 * swapped. Copying the older values costs nothing at compile time and gives a
 * controller that runs every command list without ever driving the bus --
 * every restart decodes as an unknown opcode, and reads and stops trade
 * places.
 *
 *   ESP32:  RESTART 0  WRITE 1  READ 2  STOP 3  END 4
 *   S3:     RESTART 6  WRITE 1  READ 3  STOP 2  END 4
 *
 * From ESP-IDF v5.3.5, components/hal/esp32s3/include/hal/i2c_ll.h.
 */
typedef enum {
    I2C_OPCODE_WRITE   = 1,
    I2C_OPCODE_STOP    = 2,
    I2C_OPCODE_READ    = 3,
    I2C_OPCODE_END     = 4,
    I2C_OPCODE_RSTART  = 6,
} Esp32s3I2COpcode;

typedef struct Esp32s3I2CState {
    SysBusDevice parent_obj;

    MemoryRegion iomem;
    qemu_irq irq;

    Fifo8 rx_fifo;
    Fifo8 tx_fifo;

    /*
     * Forwards whole transactions to device models running outside QEMU.
     * When nothing is listening the bus looks empty, and every address NACKs.
     */
    EspVpbClient vpb;
    /* Controller number reported to device models: 0 or 1. */
    uint8_t vpb_controller;

    /*
     * A transaction in progress, accumulated across command-list entries.
     *
     * The hardware works in bus primitives -- start, write n bytes, repeated
     * start, read n bytes, stop -- while a device model is handed whole
     * writes and reads. So the address byte is remembered and the payload
     * buffered until something forces it out: a read, a stop, or a repeated
     * start.
     */
    bool addressed;
    uint8_t address;
    uint8_t pending[ESP32S3_I2C_FIFO_LENGTH];
    uint32_t pending_len;
    /* Set when a device refused the address, cleared at each start. */
    bool nacked;

    uint32_t regs[ESP32S3_I2C_MEM_SIZE / sizeof(uint32_t)];
    uint32_t cmd[ESP32S3_I2C_CMD_COUNT];
} Esp32s3I2CState;
