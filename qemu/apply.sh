#!/usr/bin/env bash
# Graft our device models into a vendored Espressif QEMU source tree.
#
# New files are copied in wholesale. Edits to QEMU's own files are made by
# anchored insertion rather than line-based patches, so a version bump that
# shifts line numbers still applies cleanly -- and if an anchor genuinely
# disappears, this fails loudly instead of producing a subtly broken tree.
#
# Safe to run repeatedly: every edit checks whether it is already present.
#
# Usage: qemu/apply.sh [path-to-qemu-source]

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="${1:-$(cd "$HERE/.." && pwd)/vendor/src}"

if [ ! -f "$SRC/hw/xtensa/esp32s3.c" ]; then
  echo "error: $SRC does not look like an Espressif QEMU tree" >&2
  echo "       (expected hw/xtensa/esp32s3.c)" >&2
  exit 1
fi

echo "Applying to $SRC"

# --- new files -------------------------------------------------------------

copied=0
while IFS= read -r -d '' file; do
  rel="${file#"$HERE/devices/"}"
  mkdir -p "$SRC/$(dirname "$rel")"
  cp "$file" "$SRC/$rel"
  echo "  + $rel"
  copied=$((copied + 1))
done < <(find "$HERE/devices" -type f -print0)
echo "  $copied file(s) copied"

# --- anchored edits --------------------------------------------------------

# insert_after <file> <anchor-substring> <text-to-insert> <already-present-marker>
#
# The marker must be unique to *this* insertion, not merely present somewhere
# in the inserted text. Reusing a marker that an earlier insertion already
# added makes this silently skip, and the omission only shows up much later as
# a device that does not work.
insert_after() {
  local file="$1" anchor="$2" text="$3" marker="$4"
  local path="$SRC/$file"

  if grep -qF -- "$marker" "$path"; then
    echo "  = $file already has $marker"
    return 0
  fi

  local hits
  hits=$(grep -cF -- "$anchor" "$path" || true)
  if [ "$hits" -ne 1 ]; then
    echo "error: anchor in $file matched $hits times, expected exactly 1" >&2
    echo "       anchor: $anchor" >&2
    echo "       the upstream file has changed; update qemu/apply.sh" >&2
    exit 1
  fi

  # awk rather than sed: the inserted text contains slashes and quotes, and
  # escaping those for sed is a reliable source of silent corruption.
  awk -v anchor="$anchor" -v ins="$text" '
    { print }
    index($0, anchor) { print ins }
  ' "$path" > "$path.tmp"
  mv "$path.tmp" "$path"
  echo "  ~ $file += $marker"
}

insert_after "hw/misc/meson.build" \
  "'esp32s3_rtc_cntl.c'," \
  "  'esp32s3_sens.c'," \
  "esp32s3_sens.c"

insert_after "hw/misc/meson.build" \
  "  'esp32s3_sens.c'," \
  "  'esp_vpb.c'," \
  "'esp_vpb.c',"

insert_after "hw/xtensa/esp32s3.c" \
  '#include "hw/misc/esp32s3_rtc_cntl.h"' \
  '#include "hw/misc/esp32s3_sens.h"' \
  'esp32s3_sens.h'

insert_after "hw/xtensa/esp32s3.c" \
  "    Esp32s3RtcCntlState rtc_cntl;" \
  "    Esp32s3SensState sens;" \
  "Esp32s3SensState sens;"

insert_after "hw/xtensa/esp32s3.c" \
  '    object_initialize_child(obj, "rtc_cntl", &s->rtc_cntl, TYPE_ESP32S3_RTC_CNTL);' \
  '    object_initialize_child(obj, "sens", &s->sens, TYPE_ESP32S3_SENS);' \
  'TYPE_ESP32S3_SENS);'

insert_after "hw/xtensa/esp32s3.c" \
  "    esp32s3_soc_add_periph_device(sys_mem, &s->rtc_cntl, DR_REG_RTCCNTL_BASE);" \
  "
    sysbus_realize(SYS_BUS_DEVICE(&s->sens), &error_fatal);
    esp32s3_soc_add_periph_device(sys_mem, &s->sens, DR_REG_SENS_BASE);" \
  "DR_REG_SENS_BASE);"

# --- general-purpose SPI (GP-SPI2 / GP-SPI3) --------------------------------

insert_after "hw/ssi/meson.build" \
  "system_ss.add(when: 'CONFIG_XTENSA_ESP32S3', if_true: files('esp32s3_spi.c'))" \
  "system_ss.add(when: 'CONFIG_XTENSA_ESP32S3', if_true: files('esp32s3_gpspi.c'))" \
  "esp32s3_gpspi.c"

insert_after "hw/xtensa/esp32s3.c" \
  '#include "hw/misc/esp32s3_sens.h"' \
  '#include "hw/ssi/esp32s3_gpspi.h"' \
  'esp32s3_gpspi.h'

insert_after "hw/xtensa/esp32s3.c" \
  "    Esp32s3SensState sens;" \
  "    Esp32s3GpspiState gpspi2;
    Esp32s3GpspiState gpspi3;" \
  "Esp32s3GpspiState gpspi2;"

insert_after "hw/xtensa/esp32s3.c" \
  '    object_initialize_child(obj, "sens", &s->sens, TYPE_ESP32S3_SENS);' \
  '    object_initialize_child(obj, "gpspi2", &s->gpspi2, TYPE_ESP32S3_GPSPI);
    object_initialize_child(obj, "gpspi3", &s->gpspi3, TYPE_ESP32S3_GPSPI);' \
  'TYPE_ESP32S3_GPSPI);'

# Realize and map, keyed on its own marker. Sharing the SENS insertion would
# make this silently skip once SENS is already applied.
insert_after "hw/xtensa/esp32s3.c" \
  "    esp32s3_soc_add_periph_device(sys_mem, &s->sens, DR_REG_SENS_BASE);" \
  "
    sysbus_realize(SYS_BUS_DEVICE(&s->gpspi2), &error_fatal);
    esp32s3_soc_add_periph_device(sys_mem, &s->gpspi2, DR_REG_SPI2_BASE);
    sysbus_realize(SYS_BUS_DEVICE(&s->gpspi3), &error_fatal);
    esp32s3_soc_add_periph_device(sys_mem, &s->gpspi3, DR_REG_SPI3_BASE);" \
  "DR_REG_SPI2_BASE);"

# The async spi_master path waits on an interrupt, so without these the driver
# completes nothing and reports ESP_ERR_TIMEOUT forever.
insert_after "hw/xtensa/esp32s3.c" \
  "    esp32s3_soc_add_periph_device(sys_mem, &s->gpspi2, DR_REG_SPI2_BASE);" \
  "    sysbus_connect_irq(SYS_BUS_DEVICE(&s->gpspi2), 0,
                       qdev_get_gpio_in(intmatrix_dev, ETS_SPI2_INTR_SOURCE));" \
  "ETS_SPI2_INTR_SOURCE"

insert_after "hw/xtensa/esp32s3.c" \
  "    esp32s3_soc_add_periph_device(sys_mem, &s->gpspi3, DR_REG_SPI3_BASE);" \
  "    sysbus_connect_irq(SYS_BUS_DEVICE(&s->gpspi3), 0,
                       qdev_get_gpio_in(intmatrix_dev, ETS_SPI3_INTR_SOURCE));" \
  "ETS_SPI3_INTR_SOURCE"

# Hand the SPI controllers the GDMA engine, so transfers longer than the
# 64-byte register buffer can go through it. Done alongside the SHA and AES
# links, which is after the GDMA itself is realized.
insert_after "hw/xtensa/esp32s3.c" \
  "        ss->sha.parent.gdma = ESP_GDMA(&ss->gdma);" \
  "        ss->gpspi2.gdma = ESP_GDMA(&ss->gdma);
        ss->gpspi2.gdma_periph = GDMA_SPI2;
        ss->gpspi3.gdma = ESP_GDMA(&ss->gdma);
        ss->gpspi3.gdma_periph = GDMA_SPI3;" \
  "gpspi2.gdma = ESP_GDMA"

# --- analog master (regi2c) -------------------------------------------------
#
# Unmapped, so it reads zero, so the PHY's readiness poll never completes and
# a boot stops dead after "phy_init: falling back to full calibration".

insert_after "hw/misc/meson.build" \
  "  'esp_vpb.c'," \
  "  'esp32s3_ana_mst.c'," \
  "esp32s3_ana_mst.c"

insert_after "hw/xtensa/esp32s3.c" \
  '#include "hw/misc/esp32s3_sens.h"' \
  '#include "hw/misc/esp32s3_ana_mst.h"' \
  'hw/misc/esp32s3_ana_mst.h'

insert_after "hw/xtensa/esp32s3.c" \
  "    Esp32s3SensState sens;" \
  "    Esp32s3AnaMstState ana_mst;" \
  "Esp32s3AnaMstState ana_mst;"

insert_after "hw/xtensa/esp32s3.c" \
  '    object_initialize_child(obj, "sens", &s->sens, TYPE_ESP32S3_SENS);' \
  '    object_initialize_child(obj, "ana_mst", &s->ana_mst, TYPE_ESP32S3_ANA_MST);' \
  'TYPE_ESP32S3_ANA_MST);'

insert_after "hw/xtensa/esp32s3.c" \
  "    esp32s3_soc_add_periph_device(sys_mem, &s->i2c1, DR_REG_I2C1_EXT_BASE);" \
  "
    sysbus_realize(SYS_BUS_DEVICE(&s->ana_mst), &error_fatal);
    esp32s3_soc_add_periph_device(sys_mem, &s->ana_mst, DR_REG_I2C_ANA_MST_BASE);" \
  "DR_REG_I2C_ANA_MST_BASE"

# Not in the vendored register map, and not in ESP-IDF's public reg_base.h
# either -- IDF hardcodes the individual addresses in regi2c_defs.h.
insert_after "include/hw/misc/esp32s3_reg.h" \
  "#define DR_REG_RTC_I2C_BASE                     0x60008C00" \
  "#define DR_REG_I2C_ANA_MST_BASE                 0x6000E000" \
  'DR_REG_I2C_ANA_MST_BASE'

# --- I2C (I2C0 / I2C1) ------------------------------------------------------
#
# There is an ESP32 I2C model in the tree, but nothing instantiates one on the
# S3, so a board's I2C devices are simply absent. On a T-Deck that is the GT911
# touchscreen and the BBQ20 keyboard, both probed during startup and both
# reported missing.

insert_after "hw/i2c/meson.build" \
  "i2c_ss.add(when: 'CONFIG_XTENSA_ESP32S3', if_true: files('esp32_i2c.c'))" \
  "i2c_ss.add(when: 'CONFIG_XTENSA_ESP32S3', if_true: files('esp32s3_i2c.c'))" \
  "esp32s3_i2c.c"

insert_after "hw/xtensa/esp32s3.c" \
  '#include "hw/ssi/esp32s3_gpspi.h"' \
  '#include "hw/i2c/esp32s3_i2c.h"' \
  'hw/i2c/esp32s3_i2c.h'

insert_after "hw/xtensa/esp32s3.c" \
  "    Esp32s3GpspiState gpspi2;" \
  "    Esp32s3I2CState i2c0;
    Esp32s3I2CState i2c1;" \
  "Esp32s3I2CState i2c0;"

insert_after "hw/xtensa/esp32s3.c" \
  '    object_initialize_child(obj, "gpspi2", &s->gpspi2, TYPE_ESP32S3_GPSPI);' \
  '    object_initialize_child(obj, "i2c0", &s->i2c0, TYPE_ESP32S3_I2C);
    object_initialize_child(obj, "i2c1", &s->i2c1, TYPE_ESP32S3_I2C);' \
  'TYPE_ESP32S3_I2C);'

# The controller number is what a device model routes on, so it has to match
# the base address the driver talks to rather than being left at its default.
insert_after "hw/xtensa/esp32s3.c" \
  "    esp32s3_soc_add_periph_device(sys_mem, &s->gpspi3, DR_REG_SPI3_BASE);" \
  "
    s->i2c0.vpb_controller = 0;
    sysbus_realize(SYS_BUS_DEVICE(&s->i2c0), &error_fatal);
    esp32s3_soc_add_periph_device(sys_mem, &s->i2c0, DR_REG_I2C_EXT_BASE);
    sysbus_connect_irq(SYS_BUS_DEVICE(&s->i2c0), 0,
                       qdev_get_gpio_in(intmatrix_dev, ETS_I2C_EXT0_INTR_SOURCE));

    s->i2c1.vpb_controller = 1;
    sysbus_realize(SYS_BUS_DEVICE(&s->i2c1), &error_fatal);
    esp32s3_soc_add_periph_device(sys_mem, &s->i2c1, DR_REG_I2C1_EXT_BASE);
    sysbus_connect_irq(SYS_BUS_DEVICE(&s->i2c1), 0,
                       qdev_get_gpio_in(intmatrix_dev, ETS_I2C_EXT1_INTR_SOURCE));" \
  "DR_REG_I2C_EXT_BASE"

# --- SD over SPI -----------------------------------------------------------
#
# The T-Deck's microSD shares the display's SPI bus. Without a card, SD init
# fails, leaves SPI2 initialised and holds the bus lock -- and the display
# driver that comes next starves, reporting ESP_ERR_TIMEOUT on transfers that
# never get scheduled. So an absent card breaks the screen, and the fix is to
# let one be attached.
#
# QEMU already ships ssi-sd, an SSI-to-SD bridge, but it is not selected for
# Xtensa targets.
# Both ESP32 and ESP32-S3 need it -- the CYD boards are plain ESP32 -- so this
# appends to every Xtensa machine rather than using insert_after, which
# requires a unique anchor.
KCONFIG="$SRC/hw/xtensa/Kconfig"
if grep -qF "select SSI_SD" "$KCONFIG"; then
  echo "  = hw/xtensa/Kconfig already selects SSI_SD"
else
  awk '
    { print }
    /^    select SSI_M25P80$/ { print "    select SSI_SD" }
  ' "$KCONFIG" > "$KCONFIG.tmp"
  mv "$KCONFIG.tmp" "$KCONFIG"
  echo "  ~ hw/xtensa/Kconfig: select SSI_SD"
fi

# replace_once <file> <old-text> <new-text> <already-present-marker>
#
# Literal, single-occurrence replacement. Unlike insert_after this changes
# QEMU's own text, so it is only used where an insertion cannot express the
# edit.
replace_once() {
  local file="$1" old="$2" new="$3" marker="$4"
  local path="$SRC/$file"

  if grep -qF -- "$marker" "$path"; then
    echo "  = $file already has $marker"
    return 0
  fi
  if ! grep -qF -- "$old" "$path"; then
    echo "error: text to replace not found in $file" >&2
    echo "       looked for: $old" >&2
    exit 1
  fi

  # index/substr rather than sub(), which takes a regex and would silently
  # fail to match anything containing parentheses or dots.
  awk -v old="$old" -v new="$new" '
    !done {
      p = index($0, old)
      if (p > 0) {
        $0 = substr($0, 1, p - 1) new substr($0, p + length(old))
        done = 1
      }
    }
    { print }
  ' "$path" > "$path.tmp"
  mv "$path.tmp" "$path"
  echo "  ~ $file: $marker"
}

# Replace a whole span, from the line containing `start` to the first line
# that is exactly `end`. For rewriting a function body, where replace_once's
# single-line matching cannot reach.
replace_range() {
  local file="$1" start="$2" end="$3" new="$4" marker="$5"
  local path="$SRC/$file"

  if grep -qF -- "$marker" "$path"; then
    echo "  = $file already has $marker"
    return 0
  fi
  if ! grep -qF -- "$start" "$path"; then
    echo "error: start of range not found in $file" >&2
    echo "       looked for: $start" >&2
    exit 1
  fi

  awk -v start="$start" -v end="$end" -v new="$new" '
    !done && !inrange && index($0, start) { inrange = 1 }
    inrange {
      if ($0 == end) { print new; inrange = 0; done = 1 }
      next
    }
    { print }
  ' "$path" > "$path.tmp"
  mv "$path.tmp" "$path"
  echo "  ~ $file: $marker"
}

# --- GPIO output levels ------------------------------------------------------
#
# The vendored GPIO model is a stub: it answers GPIO_STRAP and drops every
# write. Nothing on a board can see a pin change, which matters more than it
# sounds -- an ST7789 tells a command byte from pixel data by the data/command
# pin and nothing on the SPI bus itself, so without this a display model
# cannot decode the stream it is being sent.
#
# Modelled here: the OUT/OUT1 latches and their W1TS/W1TC aliases, the ENABLE
# latches, and one qemu_irq per pin so devices can be wired to them.
#
# GPIO_IN reads back the output latch for driven pins and 1 for the rest.
# Undriven is not really 1 -- it depends on the pin's pull, which lives in
# IO_MUX and is not modelled -- but every input on these boards is an
# active-low button or interrupt line with a pull-up, so idle-high is both the
# common case and the safe one. Reading 0 would report every key held down.

replace_once "include/hw/gpio/esp32_gpio.h" \
  "REG32(GPIO_STRAP, 0x0038)" \
  "REG32(GPIO_OUT,          0x0004)
REG32(GPIO_OUT_W1TS,     0x0008)
REG32(GPIO_OUT_W1TC,     0x000c)
REG32(GPIO_OUT1,         0x0010)
REG32(GPIO_OUT1_W1TS,    0x0014)
REG32(GPIO_OUT1_W1TC,    0x0018)
REG32(GPIO_ENABLE,       0x0020)
REG32(GPIO_ENABLE_W1TS,  0x0024)
REG32(GPIO_ENABLE_W1TC,  0x0028)
REG32(GPIO_ENABLE1,      0x002c)
REG32(GPIO_ENABLE1_W1TS, 0x0030)
REG32(GPIO_ENABLE1_W1TC, 0x0034)
REG32(GPIO_STRAP, 0x0038)
REG32(GPIO_IN,           0x003c)
REG32(GPIO_IN1,          0x0040)

/* Two 32-bit banks. The S3 wires 0..48, the ESP32 0..39. */
#define ESP32_GPIO_PIN_COUNT 64" \
  'ESP32_GPIO_PIN_COUNT'

replace_once "include/hw/gpio/esp32_gpio.h" \
  "    uint32_t strap_mode;" \
  "    uint32_t strap_mode;

    /* Output latch and direction, both banks in one word. */
    uint64_t out;
    uint64_t enable;
    /* One line per pin, for devices that need to watch one. */
    qemu_irq out_lines[ESP32_GPIO_PIN_COUNT];" \
  'qemu_irq out_lines[ESP32_GPIO_PIN_COUNT];'

replace_range "hw/gpio/esp32_gpio.c" \
  "static uint64_t esp32_gpio_read(void *opaque, hwaddr addr, unsigned int size)" \
  "}" \
  '/* Drive the lines for every pin whose level actually changed. */
static void esp32_gpio_set_out(Esp32GpioState *s, uint64_t value)
{
    uint64_t changed = value ^ s->out;

    s->out = value;
    while (changed != 0) {
        const int pin = ctz64(changed);
        changed &= changed - 1;
        qemu_set_irq(s->out_lines[pin], (value >> pin) & 1);
    }
}

static uint64_t esp32_gpio_read(void *opaque, hwaddr addr, unsigned int size)
{
    Esp32GpioState *s = ESP32_GPIO(opaque);
    /* Driven pins read their own latch; the rest read high, see apply.sh. */
    const uint64_t in = s->out | ~s->enable;
    uint64_t r = 0;

    switch (addr) {
    case A_GPIO_STRAP:
        r = s->strap_mode;
        break;

    case A_GPIO_OUT:
        r = s->out & 0xffffffff;
        break;
    case A_GPIO_OUT1:
        r = s->out >> 32;
        break;
    case A_GPIO_ENABLE:
        r = s->enable & 0xffffffff;
        break;
    case A_GPIO_ENABLE1:
        r = s->enable >> 32;
        break;
    case A_GPIO_IN:
        r = in & 0xffffffff;
        break;
    case A_GPIO_IN1:
        r = in >> 32;
        break;

    default:
        break;
    }
    return r;
}' \
  'esp32_gpio_set_out'

replace_range "hw/gpio/esp32_gpio.c" \
  "static void esp32_gpio_write(void *opaque, hwaddr addr," \
  "}" \
  'static void esp32_gpio_write(void *opaque, hwaddr addr,
                       uint64_t value, unsigned int size)
{
    Esp32GpioState *s = ESP32_GPIO(opaque);
    const uint64_t low = value & 0xffffffff;
    const uint64_t high = low << 32;

    switch (addr) {
    case A_GPIO_OUT:
        esp32_gpio_set_out(s, (s->out & ~0xffffffffULL) | low);
        break;
    case A_GPIO_OUT_W1TS:
        esp32_gpio_set_out(s, s->out | low);
        break;
    case A_GPIO_OUT_W1TC:
        esp32_gpio_set_out(s, s->out & ~low);
        break;
    case A_GPIO_OUT1:
        esp32_gpio_set_out(s, (s->out & 0xffffffffULL) | high);
        break;
    case A_GPIO_OUT1_W1TS:
        esp32_gpio_set_out(s, s->out | high);
        break;
    case A_GPIO_OUT1_W1TC:
        esp32_gpio_set_out(s, s->out & ~high);
        break;

    /* Direction only gates what GPIO_IN reports; no line moves. */
    case A_GPIO_ENABLE:
        s->enable = (s->enable & ~0xffffffffULL) | low;
        break;
    case A_GPIO_ENABLE_W1TS:
        s->enable |= low;
        break;
    case A_GPIO_ENABLE_W1TC:
        s->enable &= ~low;
        break;
    case A_GPIO_ENABLE1:
        s->enable = (s->enable & 0xffffffffULL) | high;
        break;
    case A_GPIO_ENABLE1_W1TS:
        s->enable |= high;
        break;
    case A_GPIO_ENABLE1_W1TC:
        s->enable &= ~high;
        break;

    default:
        break;
    }
}' \
  'esp32_gpio_set_out(s, s->out | low);'

replace_once "hw/gpio/esp32_gpio.c" \
  "    sysbus_init_irq(sbd, &s->irq);" \
  "    sysbus_init_irq(sbd, &s->irq);
    qdev_init_gpio_out(DEVICE(obj), s->out_lines, ESP32_GPIO_PIN_COUNT);" \
  'qdev_init_gpio_out(DEVICE(obj), s->out_lines, ESP32_GPIO_PIN_COUNT);'

# Connect each SPI controller's data/command input to the GPIO the board says
# carries it. Done here rather than in the controller because a device cannot
# reach across to another one from its own realize, and from a property rather
# than hardcoded because the pin differs per board -- 11 on a T-Deck Plus, 2
# on a CYD.
#
# Both controllers usually end up asking for the same pin, because -global
# matches on type name and there is no per-instance id to address. A GPIO out
# line is a single link, so connecting twice silently drops the first
# connection rather than fanning out -- which looks exactly like the pin never
# moving. Route through a splitter so every listener gets the level.

replace_once "hw/xtensa/esp32s3.c" \
  "        memory_region_add_subregion_overlap(sys_mem, DR_REG_GPIO_BASE, mr, 0);" \
  "        memory_region_add_subregion_overlap(sys_mem, DR_REG_GPIO_BASE, mr, 0);

        Esp32s3GpspiState *const gpspi[] = { &ss->gpspi2, &ss->gpspi3 };
        for (unsigned i = 0; i < ARRAY_SIZE(gpspi); i++) {
            const int32_t pin = gpspi[i]->dc_gpio;
            if (pin < 0 || pin >= ESP32_GPIO_PIN_COUNT) {
                continue;
            }

            /* Collect everyone wanting this pin, then wire them at once. */
            qemu_irq listeners[ARRAY_SIZE(gpspi)];
            unsigned n = 0;
            for (unsigned j = i; j < ARRAY_SIZE(gpspi); j++) {
                if (gpspi[j]->dc_gpio == pin) {
                    listeners[n++] =
                        qdev_get_gpio_in_named(DEVICE(gpspi[j]), \"dc\", 0);
                    /* Claimed; do not wire it again on a later pass. */
                    gpspi[j]->dc_gpio = -1;
                }
            }

            if (n == 1) {
                qdev_connect_gpio_out(DEVICE(&ss->gpio), pin, listeners[0]);
            } else {
                DeviceState *split = qdev_new(TYPE_SPLIT_IRQ);
                qdev_prop_set_uint32(split, \"num-lines\", n);
                qdev_realize_and_unref(split, NULL, &error_fatal);
                for (unsigned k = 0; k < n; k++) {
                    qdev_connect_gpio_out(split, k, listeners[k]);
                }
                qdev_connect_gpio_out(DEVICE(&ss->gpio), pin,
                                      qdev_get_gpio_in(split, 0));
            }
        }" \
  'gpspi[i]->dc_gpio'

replace_once "hw/xtensa/esp32s3.c" \
  '#include "hw/misc/esp32c3_jtag.h"' \
  '#include "hw/misc/esp32c3_jtag.h"
#include "hw/core/split-irq.h"' \
  'hw/core/split-irq.h'

# TYPE_SPLIT_IRQ exists in the tree but nothing in the xtensa build selects it,
# so instantiating one fails at runtime with "unknown type 'split-irq'" rather
# than at link time.
#
# The whole block is rewritten because the ESP32 and ESP32-S3 entries differ by
# one line, so no single `select` in either is a unique anchor -- and matching
# the wrong one would quietly configure the other chip.
replace_range "hw/xtensa/Kconfig" \
  "config XTENSA_ESP32S3" \
  "    select ESP_RGB" \
  'config XTENSA_ESP32S3
    bool
    default y
    depends on XTENSA
    select SSI
    select SSI_M25P80
    select SSI_SD
    select UNIMP
    select OPENCORES_ETH
    select DWC_SDMMC
    select TMP105
    select ESP_RGB
    select SPLIT_IRQ' \
  'select SPLIT_IRQ'

# --- Interrupt matrix: re-evaluate on mapping changes ------------------------
#
# The matrix model forwards a source's level to a CPU interrupt only when the
# *source* toggles. Real hardware is combinational in both inputs: the level
# and the mapping. Nothing re-drives the CPU line when firmware repoints a
# source that is already asserted.
#
# ESP-IDF depends on exactly that. esp_intr_disable does not mask the CPU
# interrupt; for a non-shared source it rewrites the matrix entry to
# INT_MUX_DISABLED_INTNO (6, the same value the matrix resets to). spi_master
# leans on it deliberately -- from its own header comment: "If SPI is done
# transmitting/receiving but nothing is in the queue, it will not clear the
# SPI interrupt but just disable it by esp_intr_disable. This way, when a new
# thing is sent, pushing the packet into the send queue and re-enabling the
# interrupt (by esp_intr_enable) will trigger the interrupt again."
#
# So the wakeup for a queued SPI transaction is a *mapping* write against a
# peripheral line that has been held high since the previous transfer. Drop it
# and the first spi_device_transmit after any polling traffic never starts:
# the task blocks on its semaphore forever. Observed during SD card init --
# the peripheral held IRQ high, INTENABLE had the mapped bit set, and the
# CPU's INTERRUPT register never saw it.
#
# Recomputing means a CPU interrupt is now the OR of every source mapped to
# it, which is also what the hardware does and what shared interrupts need.

replace_once "include/hw/xtensa/esp32s3_intc.h" \
  "    uint8_t irq_map[ESP32S3_CPU_COUNT][ESP32S3_INT_MATRIX_INPUTS];" \
  "    uint8_t irq_map[ESP32S3_CPU_COUNT][ESP32S3_INT_MATRIX_INPUTS];
    /* Last level driven by each source, so a mapping change can re-apply it. */
    bool source_level[ESP32S3_INT_MATRIX_INPUTS];
    /* Bitmask of CPU interrupts currently asserted, to skip no-op updates. */
    uint32_t driven[ESP32S3_CPU_COUNT];" \
  'bool source_level[ESP32S3_INT_MATRIX_INPUTS];'

replace_range "hw/xtensa/esp32s3_intc.c" \
  "static void esp32s3_intmatrix_irq_handler(void *opaque, int n, int level)" \
  "}" \
  '/*
 * Drive every CPU interrupt from the current source levels and mappings.
 *
 * Recomputed wholesale rather than tracking deltas: a single source can be
 * remapped, and several sources can share one CPU interrupt, so the only
 * consistent answer is the OR over all of them.
 */
static void esp32s3_intmatrix_refresh(Esp32s3IntMatrixState *s)
{
    for (int cpu = 0; cpu < ESP32S3_CPU_COUNT; ++cpu) {
        if (s->outputs[cpu] == NULL) {
            continue;
        }

        uint32_t pending = 0;
        for (int src = 0; src < ESP32S3_INT_MATRIX_INPUTS; ++src) {
            if (s->source_level[src]) {
                pending |= 1u << (IRQ_MAP(cpu, src) & 0x1f);
            }
        }

        /*
         * Every line written every time, with no "has this changed" shortcut.
         *
         * There used to be one -- skip the CPU when the mask matched last
         * time, and within it skip the bits that had not moved -- and it can
         * swallow a raise. A guest that misses a completion interrupt does not
         * fail loudly: it stops waking on the interrupt and creeps along one
         * transfer per FreeRTOS tick. Removing the gate took display init from
         * 5462ms to 1881ms.
         *
         * Both remaining departures from upstream are load-bearing, each
         * verified by putting it back and watching the machine hang at
         * "phase 0: baked-in drivers":
         *
         *   - the OR across sources sharing a CPU interrupt, because upstream
         *     drives the line with a single source level and breaks at the first
         *     match, so a source going low silences another still asserted;
         *   - the re-drive on a mapping write, because ESP-IDF re-arms a
         *     queued SPI transfer by writing the map register against a line
         *     the peripheral has held high since the previous transfer.
         */
        for (int i = 0; i < s->cpu[cpu]->env.config->nextint; ++i) {
            const unsigned out = s->cpu[cpu]->env.config->extint[i] & 0x1f;
            qemu_set_irq(s->outputs[cpu][i], (pending >> out) & 1);
        }
        s->driven[cpu] = pending;
    }
}

static void esp32s3_intmatrix_irq_handler(void *opaque, int n, int level)
{
    Esp32s3IntMatrixState *s = ESP32S3_INTMATRIX(opaque);

    if (n < 0 || n >= ESP32S3_INT_MATRIX_INPUTS) {
        return;
    }
    s->source_level[n] = level != 0;
    esp32s3_intmatrix_refresh(s);
}' \
  'esp32s3_intmatrix_refresh'

# The write that makes the above matter: repointing a source must re-drive it.
replace_once "hw/xtensa/esp32s3_intc.c" \
  "        *map_entry = value & 0x1f;" \
  "        const uint8_t previous = *map_entry;
        *map_entry = value & 0x1f;
        if (*map_entry != previous) {
            esp32s3_intmatrix_refresh(s);
        }" \
  'if (*map_entry != previous) {'

replace_once "hw/xtensa/esp32s3_intc.c" \
  "    memset(s->irq_map, INTMATRIX_UNINT_VALUE, sizeof(s->irq_map));" \
  "    memset(s->irq_map, INTMATRIX_UNINT_VALUE, sizeof(s->irq_map));
    memset(s->source_level, 0, sizeof(s->source_level));
    memset(s->driven, 0, sizeof(s->driven));" \
  'memset(s->source_level, 0, sizeof(s->source_level));'

# --- GDMA channel matching --------------------------------------------------
#
# Two bugs in the vendored GDMA model that only bite a peripheral whose trigger
# id is zero -- which SPI2's is -- and which together hand out a channel that
# nobody ever programmed.
#
# Measured on hardware: an unbound channel's PERI_SEL reads 0x3F, not 0. The
# model memsets channel state to zero on reset, so every unbound channel claims
# to be bound to trigger 0, i.e. SPI2.
#
# And the lookup asks for "peripheral matches OR started", where its own comment
# says the channel "must be marked as 'started' too" -- an AND. With OR, the
# first zeroed channel matches before the real one is ever considered.
#
# Together: a confident match on an unprogrammed channel whose descriptor
# address is zero, so the engine chases a descriptor at address 0. That is the
# flood of invalid reads at 0x0/0x4/0x8 seen during SD init, two per transfer,
# while the transfer still reports success.

replace_once "hw/dma/esp_gdma.c" \
  "            esp_gdma_reset_fifo(config);" \
  "            esp_gdma_reset_fifo(config);
            /* Unbound reads as 0x3F on silicon; zero would mean \"SPI2\". */
            config->peripheral = R_GDMA_PERI_SEL_PERI_SEL_MASK;" \
  'config->peripheral = R_GDMA_PERI_SEL_PERI_SEL_MASK;'

replace_once "hw/dma/esp_gdma.c" \
  "GDMA_PERI_SEL, PERI_SEL) == periph ||" \
  "GDMA_PERI_SEL, PERI_SEL) == periph &&" \
  'PERI_SEL) == periph &&'

# The same lookup reads the START bit through the OUT_LINK macro whichever
# direction it was asked about. Its comment says "IN/OUT PERI registers have
# the same organization, can use any macro" -- true of PERI_SEL, not of LINK:
#
#   IN_LINK:   ADDR[19:0] AUTO_RET(20) STOP(21) START(22) RESTART(23) PARK(24)
#   OUT_LINK:  ADDR[19:0]              STOP(20) START(21) RESTART(22) PARK(23)
#
# So on an RX channel it tests bit 21, which is INLINK_STOP. A started RX
# channel reads 0 there and is skipped; a stopped one matches. Harmless while
# the condition was an OR -- the peripheral half matched everything anyway --
# and load-bearing the moment it became an AND.

replace_once "hw/dma/esp_gdma.c" \
  "             FIELD_EX32(s->ch_conf[dir][i].link, GDMA_OUT_LINK, START)) {" \
  "             (dir == ESP_GDMA_IN_IDX
              ? FIELD_EX32(s->ch_conf[dir][i].link, GDMA_IN_LINK, START)
              : FIELD_EX32(s->ch_conf[dir][i].link, GDMA_OUT_LINK, START))) {" \
  'GDMA_IN_LINK, START)'

# --- Dummy cycles on a multi-line flash read --------------------------------
#
# The controller converts the dummy cycle count to bytes by dividing by 8, as
# though every cycle carried a single bit. That only holds for single-line SPI.
# Quad I/O moves four bits per cycle and dual moves two, so the same cycle
# count occupies fewer bytes on the wire.
#
# A quad read (0xeb) asks for 6 dummy cycles. Divided by 8 that is 1 byte,
# where the flash is waiting for 3. The controller therefore stops clocking two
# bytes early and the flash never emits the last two bytes of the burst: a
# 64-byte read returns 62, a 32-byte read returns 30. The caller keeps whatever
# its buffer already held in the gap, so the damage is silent and depends on
# what was read previously.
#
# It goes unnoticed for code, which the cache reads over a different path, and
# for the erased tail of most partitions, where the missing bytes were 0xff
# anyway. It is fatal to a filesystem: littlefs puts its commit checksum in the
# last four bytes of a 64-byte metadata commit, so the checksum never matches
# and a perfectly good volume reads as a corrupt dir pair. Dual reads are
# unaffected, which is why firmware configured for DIO mounts and the same
# firmware in QIO does not.

replace_once "hw/ssi/esp32s3_spi.c" \
  "    *len = (dummy_count + 7) / 8;" \
  "    uint32_t lines = 1;
    if (FIELD_EX32(s->mem_ctrl, SPI_MEM_CTRL, FREAD_QIO) ||
        FIELD_EX32(s->mem_ctrl, SPI_MEM_CTRL, FREAD_QUAD)) {
        lines = 4;
    } else if (FIELD_EX32(s->mem_ctrl, SPI_MEM_CTRL, FREAD_DIO) ||
               FIELD_EX32(s->mem_ctrl, SPI_MEM_CTRL, FREAD_DUAL)) {
        lines = 2;
    }

    *len = (dummy_count * lines + 7) / 8;" \
  'uint32_t lines = 1;'

# --- USB Serial/JTAG console ------------------------------------------------
#
# The stock device is a stub: reads return zero, writes are dropped. Firmware
# using CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG boots completely silently. Swap the
# S3 machine over to a model with a real chardev behind it.

insert_after "hw/char/meson.build" \
  "system_ss.add(when: 'CONFIG_XTENSA_ESP32S3', if_true: files('esp32_uart.c', 'esp32s3_uart.c'))" \
  "system_ss.add(when: 'CONFIG_XTENSA_ESP32S3', if_true: files('esp32s3_usb_serial_jtag.c'))" \
  "esp32s3_usb_serial_jtag.c"

insert_after "hw/xtensa/esp32s3.c" \
  '#include "hw/misc/esp32c3_jtag.h"' \
  '#include "hw/char/esp32s3_usb_serial_jtag.h"' \
  'esp32s3_usb_serial_jtag.h'

replace_once "hw/xtensa/esp32s3.c" \
  "    ESP32C3UsbJtagState jtag;" \
  "    Esp32s3UsjState jtag;" \
  "Esp32s3UsjState jtag;"

replace_once "hw/xtensa/esp32s3.c" \
  'object_initialize_child(OBJECT(ss), "jtag", &ss->jtag, TYPE_ESP32C3_JTAG);' \
  'object_initialize_child(OBJECT(ss), "jtag", &ss->jtag, TYPE_ESP32S3_USJ);' \
  'TYPE_ESP32S3_USJ);'

# UART0 and UART1 already take serial_hd(0) and (1), so the USB console gets
# the third slot: `-serial null -serial null -serial stdio` reaches it, and
# existing UART-console invocations are unaffected.
replace_once "hw/xtensa/esp32s3.c" \
  "        sysbus_realize(SYS_BUS_DEVICE(&ss->jtag), &error_fatal);" \
  "        qdev_prop_set_chr(DEVICE(&ss->jtag), \"chardev\", serial_hd(2));
        sysbus_realize(SYS_BUS_DEVICE(&ss->jtag), &error_fatal);" \
  'qdev_prop_set_chr(DEVICE(&ss->jtag)'

# Without this the RX interrupt goes nowhere, so firmware blocked on console
# input never wakes and the port looks dead in one direction only.
insert_after "hw/xtensa/esp32s3.c" \
  "        memory_region_add_subregion_overlap(sys_mem, DR_REG_USB_SERIAL_JTAG_BASE, mr, 0);" \
  "        sysbus_connect_irq(SYS_BUS_DEVICE(&ss->jtag), 0,
                           qdev_get_gpio_in(DEVICE(&ss->intmatrix),
                                            ETS_USB_SERIAL_JTAG_INTR_SOURCE));" \
  "ETS_USB_SERIAL_JTAG_INTR_SOURCE"

# --- make --disable-slirp actually disable slirp ----------------------------
#
# Two bugs in this fork's meson.build conspire here.
#
# It asks for slirp with `static: true` hardcoded. MSYS2 ships libslirp as a
# static library only, so meson then resolves *its* glib dependency with
# `pkg-config --static` and pulls in libglib-2.0.a -- while QEMU has already
# found glib as a DLL import library. Linking both yields hundreds of
# "multiple definition of g_main_context_ref" errors.
#
# The obvious escape, --disable-slirp, does not work either: the block calls
# declare_dependency() unconditionally, with no `if slirp_dep.found()` guard,
# so `slirp` stays truthy and net/slirp.c is still compiled -- and then fails
# on a missing libslirp.h.
#
# Gating the whole block on .allowed() fixes both: disabled means the block is
# skipped and `slirp` keeps its `not_found` value.
MESON_BUILD="$SRC/meson.build"
SLIRP_OLD="if not get_option('slirp').auto() or have_system"
SLIRP_NEW="if get_option('slirp').allowed() and (not get_option('slirp').auto() or have_system)"

if grep -qF -- "$SLIRP_NEW" "$MESON_BUILD"; then
  echo "  = meson.build slirp guard already present"
elif grep -qF -- "$SLIRP_OLD" "$MESON_BUILD"; then
  # Literal replacement via index/substr. awk's sub() takes a *regex*, and
  # this anchor is full of parentheses and dots, so sub() silently matches
  # nothing while still appearing to succeed.
  awk -v old="$SLIRP_OLD" -v new="$SLIRP_NEW" '
    !done {
      p = index($0, old)
      if (p > 0) {
        $0 = substr($0, 1, p - 1) new substr($0, p + length(old))
        done = 1
      }
    }
    { print }
  ' "$MESON_BUILD" > "$MESON_BUILD.tmp"
  mv "$MESON_BUILD.tmp" "$MESON_BUILD"
  echo "  ~ meson.build: --disable-slirp now actually disables slirp"
else
  echo "warning: slirp guard anchor not found in meson.build; skipping" >&2
fi

# --- Windows build fix -----------------------------------------------------
#
# QEMU's install-tree step uses os.symlink, which Windows refuses without
# Developer Mode or elevation, and the failure aborts configure entirely.
# Falling back to a copy avoids demanding a machine-wide setting change just
# to build.
SYMLINK_SCRIPT="$SRC/scripts/symlink-install-tree.py"
if [ -f "$SYMLINK_SCRIPT" ] && ! grep -q "import shutil" "$SYMLINK_SCRIPT"; then
  python_fix=$(cat <<'PYFIX'
    try:
        os.symlink(source, bundle_dest)
    except BaseException as e:
        if isinstance(e, OSError) and e.errno == errno.EEXIST:
            pass
        elif os.name == 'nt':
            try:
                if os.path.isdir(source):
                    shutil.copytree(source, bundle_dest, dirs_exist_ok=True)
                else:
                    shutil.copy2(source, bundle_dest)
            except BaseException as copy_error:
                print(f'error copying {source} to {bundle_dest}', file=sys.stderr)
                raise copy_error
        else:
            print(f'error making symbolic link {dest}', file=sys.stderr)
            raise e
PYFIX
)
  awk -v fix="$python_fix" '
    /^import shlex$/ { print; print "import shutil"; next }
    /^    try:$/ && !done { intry = 1 }
    intry && /os\.symlink\(source, bundle_dest\)/ { print fix; skipping = 1; done = 1; next }
    skipping && /^$/ { skipping = 0; next }
    skipping { next }
    { print }
  ' "$SYMLINK_SCRIPT" > "$SYMLINK_SCRIPT.tmp"
  mv "$SYMLINK_SCRIPT.tmp" "$SYMLINK_SCRIPT"
  echo "  ~ scripts/symlink-install-tree.py: copy fallback for Windows"
fi

echo "Done."
