/*
 * ESP32-S3 SENS (SAR ADC) peripheral.
 *
 * Espressif's QEMU maps nothing at DR_REG_SENS_BASE for the S3, so firmware
 * that starts an ADC conversion and waits for the done bit waits forever.
 * Real T-Deck firmware does exactly that during startup, reading the battery
 * sense pin, and both cores end up spinning in the poll loop.
 *
 * This models just enough for conversions to complete.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */
#pragma once

#include "hw/hw.h"
#include "hw/sysbus.h"
#include "hw/registerfields.h"

#define TYPE_ESP32S3_SENS "misc.esp32s3.sens"
#define ESP32S3_SENS(obj) OBJECT_CHECK(Esp32s3SensState, (obj), TYPE_ESP32S3_SENS)

/* SENS sits between RTCIO (0x60008400) and RTC_I2C (0x60008C00). */
#define ESP32S3_SENS_MEM_SIZE   0x400
#define ESP32S3_SENS_REG_COUNT  (ESP32S3_SENS_MEM_SIZE / sizeof(uint32_t))

/* Offsets taken from the ESP32-S3 SENS register block. */
REG32(SENS_SAR_READER1_CTRL, 0x000)
REG32(SENS_SAR_MEAS1_CTRL1,  0x008)
REG32(SENS_SAR_MEAS1_CTRL2,  0x00c)
REG32(SENS_SAR_MEAS1_MUX,    0x010)
REG32(SENS_SAR_ATTEN1,       0x014)
REG32(SENS_SAR_READER2_CTRL, 0x024)
REG32(SENS_SAR_MEAS2_CTRL1,  0x02c)
REG32(SENS_SAR_MEAS2_CTRL2,  0x030)
REG32(SENS_SAR_MEAS2_MUX,    0x034)
REG32(SENS_SAR_ATTEN2,       0x038)
REG32(SENS_SAR_POWER_XPD_SAR, 0x03c)

/*
 * MEAS1_CTRL2 and MEAS2_CTRL2 share a layout, so the bit positions are
 * defined once rather than duplicated per register.
 *
 * DONE_SAR at bit 16 is the bit firmware polls; it was confirmed against a
 * real stall, where the loop was `l32i.n a9, a8, 12` / `bbci a9, 16`.
 */
#define SENS_MEAS_DATA_SHIFT    0
#define SENS_MEAS_DATA_WIDTH    16
#define SENS_MEAS_DATA_MASK     0xffffu
#define SENS_MEAS_DONE_BIT      (1u << 16)
#define SENS_MEAS_START_BIT     (1u << 17)
#define SENS_MEAS_START_FORCE   (1u << 18)

/* The SAR converters are 12-bit. */
#define SENS_SAR_MAX_RAW        0xfffu

typedef struct Esp32s3SensState {
    SysBusDevice parent_obj;
    MemoryRegion iomem;

    uint32_t regs[ESP32S3_SENS_REG_COUNT];

    /*
     * Raw counts handed back for a conversion on each SAR unit. Exposed as
     * properties so a board can present a plausible battery level rather than
     * a hard-coded one.
     */
    uint32_t adc1_raw;
    uint32_t adc2_raw;
} Esp32s3SensState;
