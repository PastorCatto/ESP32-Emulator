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

        if (pending == s->driven[cpu]) {
            continue;
        }

        const uint32_t changed = pending ^ s->driven[cpu];
        for (int i = 0; i < s->cpu[cpu]->env.config->nextint; ++i) {
            const unsigned out = s->cpu[cpu]->env.config->extint[i] & 0x1f;
            if (changed & (1u << out)) {
                qemu_set_irq(s->outputs[cpu][i], (pending >> out) & 1);
            }
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
