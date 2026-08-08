/*
 * ESP32-S3 analog master ("regi2c") interface.
 *
 * The block at 0x6000E000 is how the SoC reaches its analog front end: the
 * BBPLL, the SAR reference, and the radio's calibration registers all sit
 * behind an internal I2C-like master rather than on the APB. ESP-IDF's
 * regi2c_ctrl talks to it, and so does the closed PHY blob in ROM.
 *
 * This is a readiness stub, deliberately, and the distinction matters. There
 * is no analog to program -- no PLL to lock, no radio to calibrate -- so the
 * only faithful thing to model is the handshake: firmware writes a request
 * and polls for completion. Answering "done" lets it proceed; answering zero,
 * which is what an unmapped region gives, hangs it forever.
 *
 * That hang is where a T-Deck boot stopped: the PHY's ROM code spins on
 *
 *     l32i.n a8, a2, 0        ; read 0x6000E050
 *     extui  a8, a8, 24, 3    ; bits 26:24
 *     bnei   a8, 7, back      ; until they read 0b111
 *
 * with nothing after phy_init's "falling back to full calibration".
 *
 * Register names are from ESP-IDF v5.3.5, soc/esp32s3/include/soc/
 * regi2c_defs.h, which documents the block only as far as IDF itself uses it.
 * The status register below that one is not named anywhere public; what it
 * reports was read off the ROM's own poll.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */
#pragma once

#include "hw/hw.h"
#include "hw/sysbus.h"
#include "hw/registerfields.h"

#define TYPE_ESP32S3_ANA_MST "misc.esp32s3.ana_mst"
#define ESP32S3_ANA_MST(obj) \
    OBJECT_CHECK(Esp32s3AnaMstState, (obj), TYPE_ESP32S3_ANA_MST)

#define ESP32S3_ANA_MST_MEM_SIZE 0x100

/* Analog function control. Bit 24 reports the BBPLL calibration finished. */
REG32(ANA_MST_CONF0, 0x40)
    FIELD(ANA_MST_CONF0, BBPLL_CAL_DONE, 24, 1)

REG32(ANA_MST_ANA_CONFIG, 0x44)
REG32(ANA_MST_ANA_CONFIG2, 0x48)

/*
 * Two undocumented registers past the end of what ESP-IDF names, both
 * carrying completion bits that firmware spins on. What each reports was
 * read off the polling loops themselves:
 *
 *   0x4c bit 24        an operation on the analog bus has finished
 *   0x50 bits 26:24    all three internal channels are idle
 *
 * A stub cannot know which *other* bits here mean something, so it reports
 * only these and hands back whatever was written for the rest.
 */
REG32(ANA_MST_CMD, 0x4c)
    FIELD(ANA_MST_CMD, DONE, 24, 1)

REG32(ANA_MST_STATUS, 0x50)
    FIELD(ANA_MST_STATUS, READY, 24, 3)

#define ESP32S3_ANA_MST_READY 0x7

typedef struct Esp32s3AnaMstState {
    SysBusDevice parent_obj;

    MemoryRegion iomem;
    uint32_t regs[ESP32S3_ANA_MST_MEM_SIZE / sizeof(uint32_t)];
} Esp32s3AnaMstState;
