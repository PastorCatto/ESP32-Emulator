/*
 * ESP32-S3 general-purpose SPI controller (GP-SPI2 / GP-SPI3).
 *
 * Espressif's QEMU models SPI_MEM, the controller behind flash and PSRAM, and
 * instantiates it as spi1. GP-SPI2 and GP-SPI3 are a *different peripheral
 * with a different register map*, and no model for them exists in the tree for
 * any chip -- so anything on a board's general-purpose SPI bus is invisible.
 *
 * On a T-Deck that is the display, the SD card and the LoRa radio. Firmware
 * starts a transfer and then spins in spi_hal_usr_is_done() forever, because
 * SPI_DMA_INT_RAW.trans_done never sets on an unmapped peripheral.
 *
 * Register offsets and bit positions here come from ESP-IDF v5.3.5,
 * components/soc/esp32s3/include/soc/spi_reg.h.
 *
 * SPDX-License-Identifier: GPL-2.0-or-later
 */
#pragma once

#include "hw/hw.h"
#include "hw/sysbus.h"
#include "hw/registerfields.h"
#include "hw/ssi/ssi.h"
#include "hw/dma/esp_gdma.h"
#include "hw/misc/esp_vpb.h"

#define TYPE_ESP32S3_GPSPI "ssi.esp32s3.gpspi"
#define ESP32S3_GPSPI(obj) OBJECT_CHECK(Esp32s3GpspiState, (obj), TYPE_ESP32S3_GPSPI)

/*
 * The register file is 0x100 bytes, mirrored 16x across a 4 KiB window.
 *
 * Measured on a real T-Deck Plus: every offset in 0x000..0xFFF equals the
 * offset at `offset & 0xFF`. An emulator that decodes the full 12 bits returns
 * zero where hardware returns a live value, so the window is the full 4 KiB
 * and addresses are masked.
 */
#define ESP32S3_GPSPI_WINDOW_SIZE 0x1000
#define ESP32S3_GPSPI_MEM_SIZE   0x100
#define ESP32S3_GPSPI_ADDR_MASK  (ESP32S3_GPSPI_MEM_SIZE - 1)
#define ESP32S3_GPSPI_REG_COUNT  (ESP32S3_GPSPI_MEM_SIZE / sizeof(uint32_t))

/*
 * Hardwired constant in SPI_DATE, measured as 0x02101190. Firmware can probe
 * for a peripheral by reading this, and zero means "absent".
 */
#define ESP32S3_GPSPI_DATE_VALUE 0x02101190u

/*
 * SPI_DMA_CONF does not read back what is written: bits 0 and 1 are reset
 * controls that re-assert. Measured -- write 0x00000000, read 0x00000003;
 * write 0x18180000, read 0x18180003.
 */
#define ESP32S3_GPSPI_DMA_CONF_SET 0x00000003u

/* SPI_CLK_GATE bits: CLK_EN, MST_CLK_ACTIVE, MST_CLK_SEL. */
#define ESP32S3_GPSPI_CLK_EN       (1u << 0)

/* W0..W15 hold the data payload: 16 words, so 64 bytes per transaction. */
#define ESP32S3_GPSPI_BUF_WORDS  16
#define ESP32S3_GPSPI_BUF_BYTES  (ESP32S3_GPSPI_BUF_WORDS * 4)

/* CS0..CS5. */
#define ESP32S3_GPSPI_CS_COUNT   6

/*
 * Largest single DMA transfer we stage in one go. SPI_MS_DLEN is 18 bits, so
 * the hardware maximum is 32 KiB; a driver asking for more than this is
 * clamped and told, rather than overrunning the buffer.
 */
#define ESP32S3_GPSPI_DMA_MAX    (32 * 1024)

REG32(GPSPI_CMD, 0x000)
    FIELD(GPSPI_CMD, UPDATE, 23, 1)
    FIELD(GPSPI_CMD, USR, 24, 1)

REG32(GPSPI_ADDR,     0x004)
REG32(GPSPI_CTRL,     0x008)
REG32(GPSPI_CLOCK,    0x00c)

REG32(GPSPI_USER, 0x010)
    FIELD(GPSPI_USER, DOUTDIN,        0, 1)
    FIELD(GPSPI_USER, MISO_HIGHPART, 24, 1)
    FIELD(GPSPI_USER, MOSI_HIGHPART, 25, 1)
    FIELD(GPSPI_USER, USR_MOSI,      27, 1)
    FIELD(GPSPI_USER, USR_MISO,      28, 1)
    FIELD(GPSPI_USER, USR_DUMMY,     29, 1)
    FIELD(GPSPI_USER, USR_ADDR,      30, 1)
    FIELD(GPSPI_USER, USR_COMMAND,   31, 1)

REG32(GPSPI_USER1, 0x014)
    FIELD(GPSPI_USER1, USR_DUMMY_CYCLELEN,  0, 8)
    FIELD(GPSPI_USER1, USR_ADDR_BITLEN,    27, 5)

REG32(GPSPI_USER2, 0x018)
    FIELD(GPSPI_USER2, USR_COMMAND_VALUE,   0, 16)
    FIELD(GPSPI_USER2, USR_COMMAND_BITLEN, 28, 4)

REG32(GPSPI_MS_DLEN, 0x01c)
    /* Holds bit count minus one. */
    FIELD(GPSPI_MS_DLEN, MS_DATA_BITLEN, 0, 18)

REG32(GPSPI_MISC, 0x020)
    /* One bit per chip select; 1 disables the line, so the active CS is a
     * zero. All ones means no device is selected. */
    FIELD(GPSPI_MISC, CS_DIS, 0, 6)

REG32(GPSPI_DMA_CONF,    0x030)
    /* Bit 21. Set for full-duplex DMA. */
    FIELD(GPSPI_DMA_CONF, RX_EOF_EN, 21, 1)
    /* Bit 27: memory <- peripheral. Bit 28: memory -> peripheral. */
    FIELD(GPSPI_DMA_CONF, DMA_RX_ENA, 27, 1)
    FIELD(GPSPI_DMA_CONF, DMA_TX_ENA, 28, 1)
REG32(GPSPI_DMA_INT_ENA, 0x034)
REG32(GPSPI_DMA_INT_CLR, 0x038)
REG32(GPSPI_DMA_INT_RAW, 0x03c)
REG32(GPSPI_DMA_INT_ST,  0x040)
REG32(GPSPI_DMA_INT_SET, 0x044)

/*
 * Bit 12 of the interrupt registers. This is the one firmware polls through
 * spi_hal_usr_is_done(), and the reason an unmodelled controller hangs a boot.
 */
#define GPSPI_TRANS_DONE_INT     (1u << 12)

REG32(GPSPI_W0,       0x098)
REG32(GPSPI_W15,      0x0d4)
REG32(GPSPI_SLAVE,    0x0e0)
REG32(GPSPI_CLK_GATE, 0x0e8)
REG32(GPSPI_DATE,     0x0f0)

/*
 * Floor on how long a transfer appears to take.
 *
 * Completion must not be visible before the guest's store to SPI_CMD has
 * retired and the driver has finished its post-start bookkeeping. Real
 * hardware always takes microseconds; finishing in zero guest time lets the
 * ISR re-enter a driver that assumes it cannot be interrupted there.
 */
#define ESP32S3_GPSPI_MIN_XFER_NS  2000

typedef struct Esp32s3GpspiState {
    SysBusDevice parent_obj;

    MemoryRegion iomem;
    SSIBus *spi;
    qemu_irq cs_gpio[ESP32S3_GPSPI_CS_COUNT];
    qemu_irq irq;

    /* Raises trans_done once the modelled transfer duration has elapsed. */
    QEMUTimer *done_timer;
    bool busy;

    /*
     * Whether the interrupt line is currently asserted. Tracked so the line is
     * driven on transitions only, and held for as long as a masked status bit
     * is set: ESP-IDF re-enables a queued SPI transaction by repointing the
     * interrupt matrix at a line the peripheral has been holding high since
     * the previous transfer.
     */
    bool line_high;

    /*
     * The GDMA engine, when the machine wires one up. Transfers longer than
     * the 64-byte register buffer go through it, which is how a display
     * driver pushes a framebuffer.
     */
    ESPGdmaState *gdma;
    /* Which peripheral slot to claim on the GDMA: SPI2 or SPI3. */
    GdmaPeripheral gdma_periph;

    /*
     * Forwards whole transactions to device models running outside QEMU.
     * When nothing is listening the bus simply looks empty, which is a
     * legitimate way to run the emulator.
     */
    EspVpbClient vpb;
    /* Controller number reported to device models: 2 for SPI2, 3 for SPI3. */
    uint8_t vpb_controller;

    /*
     * Set when the guest writes W0..W15, cleared when a transfer consumes
     * them. Distinguishes a programmed-I/O transfer from a DMA one.
     */
    bool w_written;

    /*
     * Level of the data/command GPIO, latched when a transfer begins. An
     * ST7789 distinguishes a command byte from pixel data by this pin and
     * nothing on the bus itself, so a display driver cannot decode the
     * stream without it. -1 means the board has no such pin.
     */
    int dc_level;

    /*
     * Which GPIO carries data/command, so the machine knows what to connect
     * to this controller's "dc" input. Board-specific -- 11 on a T-Deck Plus,
     * 2 on a CYD -- so it comes from the board file, not from the SoC. -1
     * leaves the pin unwired and dc_level pinned at -1.
     */
    int32_t dc_gpio;

    /*
     * Staging for a DMA transfer. Sized for one descriptor's worth of the
     * largest transfer a driver is likely to queue; longer transfers are
     * chunked by the GDMA's own descriptor walk.
     */
    uint8_t dma_buf[ESP32S3_GPSPI_DMA_MAX];

    uint32_t regs[ESP32S3_GPSPI_REG_COUNT];
} Esp32s3GpspiState;
