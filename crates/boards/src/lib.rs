//! Board definitions.
//!
//! A board is data. Panel geometry, pin assignments, and bus addresses live in
//! TOML, so supporting a different display or a moved chip select is an edit
//! rather than a code change.
//!
//! Peripheral `kind` is deliberately a free string rather than an enum. An
//! unrecognised kind is not a parse error: it may be served by an external
//! driver that registers for the same address at runtime. Validation here is
//! about structure — do bus references resolve, do two devices collide — not
//! about whether we happen to ship a driver.

pub mod params;

use flashimg::{Chip, FlashSize};
pub use params::{ParamError, ParamResult, Params};
use serde::Deserialize;
use std::collections::HashMap;
use vpb::Claim;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardError {
    Toml(String),
    UnknownChip(String),
    BadFlashSize(String),
    /// A peripheral referenced a bus id that no `[[bus]]` block defines.
    UnknownBus { peripheral: String, bus: String },
    /// A peripheral needs a bus but named none.
    MissingBus { peripheral: String },
    /// Two peripherals claim one address; the emulator would refuse to attach
    /// the second, so reject the board rather than half-load it.
    ClaimConflict { a: String, b: String, claim: Claim },
    /// A required key was absent or malformed.
    Param { peripheral: String, source: ParamError },
    DuplicateBusId(String),
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoardError::Toml(e) => write!(f, "invalid TOML: {e}"),
            BoardError::UnknownChip(c) => write!(
                f,
                "unknown chip {c:?}; expected one of {}",
                flashimg::ALL_CHIPS
                    .iter()
                    .map(|c| c.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            BoardError::BadFlashSize(s) => {
                write!(f, "unparseable flash size {s:?}; try something like \"16MB\"")
            }
            BoardError::UnknownBus { peripheral, bus } => {
                write!(f, "peripheral {peripheral:?} references undefined bus {bus:?}")
            }
            BoardError::MissingBus { peripheral } => {
                write!(f, "peripheral {peripheral:?} needs a `bus` key")
            }
            BoardError::ClaimConflict { a, b, claim } => {
                write!(f, "peripherals {a:?} and {b:?} both claim {claim:?}")
            }
            BoardError::Param { peripheral, source } => {
                write!(f, "peripheral {peripheral:?}: {source}")
            }
            BoardError::DuplicateBusId(id) => write!(f, "two buses share the id {id:?}"),
        }
    }
}

impl std::error::Error for BoardError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BusKind {
    Spi,
    I2c,
    Uart,
    I2s,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bus {
    pub id: String,
    pub kind: BusKind,
    /// Controller index on the SoC: SPI2, I2C0, UART1.
    pub controller: u8,
    /// Every controller that can drive these pins.
    ///
    /// A device is soldered to pins, not to a peripheral. The GPIO matrix
    /// then lets firmware drive those pins from whichever controller it
    /// opens, and that choice is arbitrary -- PURR OS opens I2C0 for the
    /// T-Deck's touch panel, the Arduino Launcher opens I2C1, and both are
    /// correct on hardware.
    ///
    /// Routing purely by `controller` models a distinction the silicon does
    /// not have, and the symptom is unpleasant: the bus answers NACK, the
    /// driver reports a real transfer error, and the device looks absent
    /// rather than misrouted.
    ///
    /// Defaults to just `controller`, so a board says nothing unless its
    /// pins really can be driven from more than one place.
    pub controllers: Vec<u8>,
    pub pins: HashMap<String, u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PsramKind {
    #[default]
    None,
    Quad,
    Octal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Psram {
    pub kind: PsramKind,
    pub size: u32,
}

/// One device attached to the board.
#[derive(Debug, Clone)]
pub struct PeripheralSpec {
    pub kind: String,
    pub label: Option<String>,
    /// Resolved bus, if this device sits on one.
    pub bus: Option<String>,
    pub bus_kind: Option<BusKind>,
    pub controller: Option<u8>,
    /// Every controller that can reach this device, from its bus.
    pub controllers: Vec<u8>,
    /// Enabled devices attach at load; disabled ones are listed but absent
    /// from the bus, so they can be toggled without editing the file.
    pub enabled: bool,
    pub params: Params,
}

impl PeripheralSpec {
    /// The bus address this device occupies, derived from its kind and params.
    ///
    /// Returns `None` for devices that are not bus-addressed, like the
    /// trackball, which is a handful of plain GPIOs.
    pub fn claim(&self) -> Result<Option<Claim>, BoardError> {
        let err = |source| BoardError::Param {
            peripheral: self.kind.clone(),
            source,
        };
        let Some(controller) = self.controller else {
            return Ok(None);
        };
        Ok(match self.bus_kind {
            // `cs` is the GPIO the chip select comes out on, which is what a
            // schematic gives you and what the emulated SoC needs for pin
            // routing. The *bus* routes on the controller's CS line index,
            // 0..5, which is a different number: the driver allocates it at
            // runtime through the GPIO matrix. On a T-Deck the display is
            // GPIO 12 on line 0, and the SD card GPIO 39 on line 5.
            //
            // So a board file gives both, and `cs_line` is the one that
            // decides which device answers. Falling back to `cs` keeps older
            // files loading, and is right only where the two coincide.
            Some(BusKind::Spi) => Some(Claim::Spi {
                controller,
                cs: match self.params.opt_u8("cs_line").map_err(err)? {
                    Some(line) => line,
                    None => self.params.u8("cs").map_err(err)?,
                },
            }),
            Some(BusKind::I2c) => Some(Claim::I2c {
                controller,
                address: self.params.u8("address").map_err(err)?,
                alt: self.params.opt_u8("alt_address").map_err(err)?,
            }),
            Some(BusKind::Uart) => Some(Claim::Uart { controller }),
            // I2S carries no addressing we route on.
            Some(BusKind::I2s) | None => None,
        })
    }

    /// Every address this device answers on.
    ///
    /// One per controller that can drive its pins. On most boards that is a
    /// single claim and this is [`Self::claim`] in a vector; where a bus
    /// lists several controllers it is one each, because firmware picks the
    /// controller and the device has no say in it.
    pub fn all_claims(&self) -> Result<Vec<Claim>, BoardError> {
        let Some(base) = self.claim()? else {
            return Ok(Vec::new());
        };
        let others = self.controllers.iter().copied().filter(|c| Some(*c) != self.controller);

        let mut out = vec![base.clone()];
        for controller in others {
            out.push(match &base {
                Claim::Spi { cs, .. } => Claim::Spi { controller, cs: *cs },
                Claim::I2c { address, alt, .. } => Claim::I2c {
                    controller,
                    address: *address,
                    alt: *alt,
                },
                Claim::Uart { .. } => Claim::Uart { controller },
                // A GPIO belongs to the pin, not to a peripheral, so there is
                // no second controller to offer it on.
                Claim::Gpio { .. } => continue,
            });
        }
        Ok(out)
    }

    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.kind)
    }
}

#[derive(Debug, Clone)]
pub struct Board {
    pub id: String,
    pub name: String,
    pub chip: Chip,
    pub flash_size: FlashSize,
    pub psram: Psram,
    pub default_for_chip: bool,
    pub buses: Vec<Bus>,
    pub peripherals: Vec<PeripheralSpec>,
    /// Config keys nobody read, per peripheral. Surfaced in the UI rather than
    /// silently dropped, because a mistyped key otherwise just does nothing.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Deserialisation shapes. Kept separate from the public types so the file
// format can change without the rest of the emulator noticing.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawFile {
    board: RawBoard,
    #[serde(default)]
    bus: Vec<RawBus>,
    #[serde(default)]
    peripheral: Vec<toml::Table>,
}

#[derive(Deserialize)]
struct RawBoard {
    id: String,
    name: String,
    chip: String,
    flash_size: String,
    #[serde(default)]
    default_for_chip: bool,
    #[serde(default)]
    psram: Option<RawPsram>,
}

#[derive(Deserialize)]
struct RawPsram {
    kind: PsramKind,
    size: String,
}

#[derive(Deserialize)]
struct RawBus {
    id: String,
    kind: BusKind,
    controller: u8,
    #[serde(default)]
    controllers: Vec<u8>,
    #[serde(flatten)]
    pins: HashMap<String, toml::Value>,
}

impl Board {
    pub fn from_toml(src: &str) -> Result<Self, BoardError> {
        let raw: RawFile = toml::from_str(src).map_err(|e| BoardError::Toml(e.to_string()))?;

        let chip: Chip = raw
            .board
            .chip
            .parse()
            .map_err(|_| BoardError::UnknownChip(raw.board.chip.clone()))?;
        let flash_size: FlashSize = raw
            .board
            .flash_size
            .parse()
            .map_err(|_| BoardError::BadFlashSize(raw.board.flash_size.clone()))?;

        let psram = match raw.board.psram {
            None => Psram::default(),
            Some(p) => Psram {
                kind: p.kind,
                // A "0" size alongside kind = none is normal, so tolerate an
                // unparseable size only when there is no PSRAM at all.
                size: match p.size.parse::<FlashSize>() {
                    Ok(s) => s.bytes(),
                    Err(()) if p.kind == PsramKind::None => 0,
                    Err(()) => return Err(BoardError::BadFlashSize(p.size)),
                },
            },
        };

        let mut buses = Vec::with_capacity(raw.bus.len());
        for b in raw.bus {
            if buses.iter().any(|e: &Bus| e.id == b.id) {
                return Err(BoardError::DuplicateBusId(b.id));
            }
            let pins = b
                .pins
                .iter()
                .filter_map(|(k, v)| v.as_integer().and_then(|i| u8::try_from(i).ok()).map(|p| (k.clone(), p)))
                .collect();
            buses.push(Bus {
                id: b.id,
                kind: b.kind,
                controller: b.controller,
                controllers: if b.controllers.is_empty() {
                    vec![b.controller]
                } else {
                    b.controllers.clone()
                },
                pins,
            });
        }

        let mut peripherals = Vec::with_capacity(raw.peripheral.len());
        for mut table in raw.peripheral {
            // Pull the structural keys out; whatever remains is the driver's.
            let kind = match table.remove("kind") {
                Some(toml::Value::String(s)) => s,
                _ => {
                    return Err(BoardError::Param {
                        peripheral: "<unnamed>".into(),
                        source: ParamError::Missing { key: "kind".into() },
                    })
                }
            };
            let label = table.remove("label").and_then(|v| v.as_str().map(str::to_owned));
            let enabled = table.remove("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let bus_ref = table.remove("bus").and_then(|v| v.as_str().map(str::to_owned));

            let (bus_kind, controller, controllers) = match &bus_ref {
                Some(id) => {
                    // `bus = "i2s"` in a board file names a kind rather than a
                    // declared bus; treat an unmatched reference to a known
                    // kind as an untracked bus rather than an error.
                    match buses.iter().find(|b| &b.id == id) {
                        Some(b) => (Some(b.kind), Some(b.controller), b.controllers.clone()),
                        None if id == "i2s" => (Some(BusKind::I2s), None, Vec::new()),
                        None => {
                            return Err(BoardError::UnknownBus {
                                peripheral: kind,
                                bus: id.clone(),
                            })
                        }
                    }
                }
                None => (None, None, Vec::new()),
            };

            peripherals.push(PeripheralSpec {
                kind,
                label,
                bus: bus_ref,
                bus_kind,
                controller,
                controllers,
                enabled,
                params: Params::new(table),
            });
        }

        let board = Board {
            id: raw.board.id,
            name: raw.board.name,
            chip,
            flash_size,
            psram,
            default_for_chip: raw.board.default_for_chip,
            buses,
            peripherals,
            warnings: Vec::new(),
        };
        board.validate()?;
        Ok(board)
    }

    /// Reject boards the emulator could not fully attach.
    fn validate(&self) -> Result<(), BoardError> {
        let mut taken: Vec<(Claim, &str)> = Vec::new();
        for p in self.peripherals.iter().filter(|p| p.enabled) {
            let Some(claim) = p.claim()? else { continue };
            if let Some((c, other)) = taken.iter().find(|(c, _)| c.matches(&claim)) {
                return Err(BoardError::ClaimConflict {
                    a: (*other).to_string(),
                    b: p.kind.clone(),
                    claim: c.clone(),
                });
            }
            taken.push((claim, &p.kind));
        }
        Ok(())
    }

    /// Config keys no driver read. Call after drivers have been constructed,
    /// since reads happen during construction.
    pub fn collect_warnings(&mut self) {
        self.warnings = self
            .peripherals
            .iter()
            .flat_map(|p| {
                p.params
                    .unused()
                    .into_iter()
                    .map(move |k| format!("{}: unrecognised key {k:?}", p.display_name()))
            })
            .collect();
    }

    /// This board's PSRAM as QEMU needs it described.
    ///
    /// `None` when the board has no PSRAM, which is different from zero: the
    /// S3 machine has none by default and passing a size of zero is not the
    /// same as passing nothing.
    pub fn qemu_psram(&self) -> Option<qemuctl::Psram> {
        if self.psram.kind == PsramKind::None || self.psram.size == 0 {
            return None;
        }
        Some(qemuctl::Psram {
            size_mb: (self.psram.size / (1024 * 1024)).max(1),
            octal: self.psram.kind == PsramKind::Octal,
        })
    }

    /// The GPIO carrying the display's data/command line.
    ///
    /// Needed by the emulated SPI controller, not by the display model: the
    /// level has to be sampled where the transfer starts and sent along with
    /// it, because by the time the bytes reach a device model the pin has
    /// already moved on.
    ///
    /// The first display panel that declares one wins. Boards with two panels
    /// on separate controllers exist, but nothing here handles them yet, and
    /// silently picking one is better than refusing to show either.
    pub fn display_dc_gpio(&self) -> Option<u8> {
        self.peripherals
            .iter()
            .filter(|p| p.enabled)
            .find_map(|p| p.params.opt_u8("dc").ok().flatten())
    }

    pub fn bus(&self, id: &str) -> Option<&Bus> {
        self.buses.iter().find(|b| b.id == id)
    }

    pub fn peripheral(&self, kind: &str) -> Option<&PeripheralSpec> {
        self.peripherals.iter().find(|p| p.kind == kind)
    }

    /// Claims for every enabled peripheral, for wiring up the registry.
    pub fn claims(&self) -> Result<Vec<(String, Claim)>, BoardError> {
        let mut out = Vec::new();
        for p in self.peripherals.iter().filter(|p| p.enabled) {
            for c in p.all_claims()? {
                out.push((p.kind.clone(), c));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T_DECK: &str = include_str!("../../../boards/t-deck-plus.toml");
    const GENERIC: &str = include_str!("../../../boards/generic-esp32s3.toml");
    const CYD: &str = include_str!("../../../boards/cyd-esp32-2432s028r.toml");
    const CYD_S024C: &str = include_str!("../../../boards/cyd-s024c.toml");
    const CYD_S028R: &str = include_str!("../../../boards/cyd-s028r.toml");

    #[test]
    fn parses_the_shipped_t_deck_definition() {
        let b = Board::from_toml(T_DECK).expect("t-deck-plus.toml should parse");
        assert_eq!(b.id, "t-deck-plus");
        assert_eq!(b.chip, Chip::Esp32S3);
        assert_eq!(b.flash_size, FlashSize::MB16);
        assert_eq!(b.psram.kind, PsramKind::Octal);
        assert_eq!(b.psram.size, 8 * 1024 * 1024);
    }

    #[test]
    fn resolves_the_shared_spi_bus_to_distinct_chip_selects() {
        let b = Board::from_toml(T_DECK).unwrap();
        // Display, SD, and LoRa share one bus and differ only by chip select;
        // getting this wrong is the classic T-Deck emulation bug.
        //
        // A claim routes on the controller's CS line, not the GPIO the line
        // comes out on. Those are different numbers -- the display is GPIO 12
        // on line 0 -- and matching against the GPIO means no device ever
        // answers, which looks exactly like a bus that is not working.
        let claims: HashMap<String, Claim> = b.claims().unwrap().into_iter().collect();
        assert_eq!(claims["st7789"], Claim::Spi { controller: 2, cs: 0 });
        assert_eq!(claims["sdcard"], Claim::Spi { controller: 2, cs: 5 });
        // No `cs_line` recorded for the radio yet, so this still falls back to
        // its GPIO -- and will not match the bus until it is measured.
        assert_eq!(claims["sx1262"], Claim::Spi { controller: 2, cs: 9 });
    }

    #[test]
    fn resolves_i2c_devices_including_the_alternate_address() {
        let b = Board::from_toml(T_DECK).unwrap();
        let all = b.claims().unwrap();
        let for_kind = |kind: &str| -> Vec<Claim> {
            all.iter().filter(|(k, _)| k == kind).map(|(_, c)| c.clone()).collect()
        };
        assert!(for_kind("gt911")
            .contains(&Claim::I2c { controller: 0, address: 0x5d, alt: Some(0x14) }));
        assert!(for_kind("tdeck-keyboard")
            .contains(&Claim::I2c { controller: 0, address: 0x55, alt: None }));
    }

    #[test]
    fn an_i2c_device_answers_on_every_controller_that_can_reach_its_pins() {
        // The T-Deck's panel and keyboard hang off SDA 18 / SCL 8, and the
        // GPIO matrix lets either I2C peripheral drive those pins. PURR OS
        // opens I2C0 and the Arduino Launcher opens I2C1 -- both correct on
        // hardware, so the device has to answer either way. Routing on the
        // controller alone made the Launcher see an empty bus and report
        // "GT911 not found".
        let b = Board::from_toml(T_DECK).unwrap();
        let gt911 = b
            .peripherals
            .iter()
            .find(|p| p.kind == "gt911")
            .expect("t-deck has a touch panel");

        let claims = gt911.all_claims().unwrap();
        assert!(claims.contains(&Claim::I2c { controller: 0, address: 0x5d, alt: Some(0x14) }));
        assert!(claims.contains(&Claim::I2c { controller: 1, address: 0x5d, alt: Some(0x14) }));
        assert_eq!(claims.len(), 2, "one per controller, no duplicates");
    }

    #[test]
    fn a_bus_on_one_controller_still_yields_one_claim() {
        // The common case must not change: only a board that says its pins
        // can be driven from elsewhere gets extra claims.
        let b = Board::from_toml(T_DECK).unwrap();
        let display = b
            .peripherals
            .iter()
            .find(|p| p.kind == "st7789")
            .expect("t-deck has a display");
        assert_eq!(display.all_claims().unwrap().len(), 1);
    }

    #[test]
    fn display_geometry_is_readable_config() {
        let b = Board::from_toml(T_DECK).unwrap();
        let lcd = b.peripheral("st7789").expect("display present");
        assert_eq!(lcd.params.u16("width").unwrap(), 320);
        assert_eq!(lcd.params.u16("height").unwrap(), 240);
        assert_eq!(lcd.params.u8("dc").unwrap(), 11);
    }

    #[test]
    fn t_deck_psram_reaches_qemu_as_octal_8mb() {
        // Real T-Deck firmware aborts during startup without this, so the
        // board file getting it right is load-bearing, not cosmetic.
        let b = Board::from_toml(T_DECK).unwrap();
        assert_eq!(
            b.qemu_psram(),
            Some(qemuctl::Psram { size_mb: 8, octal: true })
        );
    }

    #[test]
    fn a_board_without_psram_asks_qemu_for_none() {
        let b = Board::from_toml(GENERIC).unwrap();
        assert_eq!(b.qemu_psram(), None);
    }

    #[test]
    fn every_shipped_board_file_parses() {
        // Board files are data, and data with no test is data that rots.
        for (name, src) in [
            ("t-deck-plus", T_DECK),
            ("generic-esp32s3", GENERIC),
            ("cyd-esp32-2432s028r", CYD),
            ("cyd-s024c", CYD_S024C),
            ("cyd-s028r", CYD_S028R),
        ] {
            let b = Board::from_toml(src).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(b.id, name, "board id should match its filename");
        }
    }

    #[test]
    fn cyd_puts_touch_on_its_own_bus_not_the_display_bus() {
        // The hardware wires XPT2046 to its own CLK/MOSI/MISO. Sharing the
        // display bus is a common misreading, so pin it down.
        let b = Board::from_toml(CYD).unwrap();
        let tft = b.bus("tft").expect("display bus");
        let tspi = b.bus("tspi").expect("touch bus");
        assert_ne!(tft.controller, tspi.controller);
        assert_eq!(tspi.pins["sck"], 25);
        assert_eq!(tspi.pins["mosi"], 32);
        assert_eq!(tspi.pins["miso"], 39);

        let claims: HashMap<String, Claim> = b.claims().unwrap().into_iter().collect();
        assert_eq!(claims["ili9341"], Claim::Spi { controller: 2, cs: 15 });
        assert_eq!(claims["xpt2046"], Claim::Spi { controller: 4, cs: 33 });
        assert_eq!(claims["sdcard"], Claim::Spi { controller: 3, cs: 5 });
    }

    #[test]
    fn cyd_is_a_plain_esp32_with_no_psram() {
        let b = Board::from_toml(CYD).unwrap();
        assert_eq!(b.chip, Chip::Esp32);
        assert_eq!(b.flash_size, FlashSize(4 * 1024 * 1024));
        assert_eq!(b.qemu_psram(), None);
    }

    #[test]
    fn the_capacitive_cyd_uses_i2c_touch_and_a_moved_backlight() {
        let b = Board::from_toml(CYD_S024C).unwrap();
        let claims: HashMap<String, Claim> = b.claims().unwrap().into_iter().collect();
        assert_eq!(
            claims["cst816s"],
            Claim::I2c { controller: 0, address: 0x15, alt: None }
        );
        // Verified against hardware as 27 rather than the 21 used elsewhere.
        let lcd = b.peripheral("ili9341").unwrap();
        assert_eq!(lcd.params.u8("backlight").unwrap(), 27);
        // No SPI touch bus on this variant at all.
        assert!(b.bus("tspi").is_none());
    }

    #[test]
    fn the_flipped_cyd_differs_only_in_orientation() {
        let base = Board::from_toml(CYD).unwrap();
        let flipped = Board::from_toml(CYD_S028R).unwrap();
        assert_eq!(base.chip, flipped.chip);

        let a = base.peripheral("ili9341").unwrap();
        let b = flipped.peripheral("ili9341").unwrap();
        assert_eq!(a.params.u8("cs").unwrap(), b.params.u8("cs").unwrap());
        assert_eq!(a.params.u16_or("rotation", 0).unwrap(), 0);
        assert_eq!(b.params.u16_or("rotation", 0).unwrap(), 180);
    }

    #[test]
    fn generic_board_has_no_peripherals_so_unknown_boards_still_boot() {
        let b = Board::from_toml(GENERIC).unwrap();
        assert!(b.peripherals.is_empty());
        assert!(b.default_for_chip);
        assert_eq!(b.psram.kind, PsramKind::None);
    }

    #[test]
    fn unknown_peripheral_kind_is_allowed_for_external_drivers() {
        let src = r#"
[board]
id = "x"
name = "X"
chip = "esp32s3"
flash_size = "4MB"

[[bus]]
id = "i2c0"
kind = "i2c"
controller = 0

[[peripheral]]
kind = "nobody-has-ever-heard-of-this"
bus = "i2c0"
address = 0x48
"#;
        let b = Board::from_toml(src).expect("unknown kinds must not be a parse error");
        assert_eq!(
            b.claims().unwrap()[0].1,
            Claim::I2c { controller: 0, address: 0x48, alt: None }
        );
    }

    #[test]
    fn rejects_two_devices_on_one_chip_select() {
        let src = r#"
[board]
id = "x"
name = "X"
chip = "esp32s3"
flash_size = "4MB"

[[bus]]
id = "spi2"
kind = "spi"
controller = 2

[[peripheral]]
kind = "st7789"
bus = "spi2"
cs = 12

[[peripheral]]
kind = "sdcard"
bus = "spi2"
cs = 12
"#;
        assert!(matches!(
            Board::from_toml(src),
            Err(BoardError::ClaimConflict { .. })
        ));
    }

    #[test]
    fn rejects_a_reference_to_an_undefined_bus() {
        let src = r#"
[board]
id = "x"
name = "X"
chip = "esp32s3"
flash_size = "4MB"

[[peripheral]]
kind = "st7789"
bus = "typo"
cs = 12
"#;
        let err = Board::from_toml(src).unwrap_err();
        assert_eq!(
            err.to_string(),
            "peripheral \"st7789\" references undefined bus \"typo\""
        );
    }

    #[test]
    fn disabled_peripheral_claims_nothing() {
        let src = r#"
[board]
id = "x"
name = "X"
chip = "esp32s3"
flash_size = "4MB"

[[bus]]
id = "spi2"
kind = "spi"
controller = 2

[[peripheral]]
kind = "st7789"
bus = "spi2"
cs = 12
enabled = false

[[peripheral]]
kind = "sdcard"
bus = "spi2"
cs = 12
"#;
        // The disabled display frees the chip select, so this is not a conflict.
        let b = Board::from_toml(src).unwrap();
        assert_eq!(b.claims().unwrap().len(), 1);
    }

    #[test]
    fn unknown_chip_lists_the_valid_ones() {
        let src = r#"
[board]
id = "x"
name = "X"
chip = "esp32s9"
flash_size = "4MB"
"#;
        let err = Board::from_toml(src).unwrap_err();
        assert!(err.to_string().starts_with("unknown chip \"esp32s9\"; expected one of ESP32,"));
    }

    #[test]
    fn chip_and_flash_size_accept_the_usual_spellings() {
        for s in ["esp32s3", "ESP32-S3", "esp32_s3", "s3"] {
            assert_eq!(s.parse::<Chip>(), Ok(Chip::Esp32S3), "failed on {s:?}");
        }
        for s in ["16MB", "16M", "16 MiB", "16mb"] {
            assert_eq!(s.parse::<FlashSize>(), Ok(FlashSize::MB16), "failed on {s:?}");
        }
        assert!("banana".parse::<FlashSize>().is_err());
        assert!("0MB".parse::<FlashSize>().is_err());
    }

    #[test]
    fn mistyped_keys_are_reported_as_warnings() {
        let src = r#"
[board]
id = "x"
name = "X"
chip = "esp32s3"
flash_size = "4MB"

[[bus]]
id = "spi2"
kind = "spi"
controller = 2

[[peripheral]]
kind = "st7789"
bus = "spi2"
cs = 12
wdith = 320
"#;
        let mut b = Board::from_toml(src).unwrap();
        // Simulate a driver reading the keys it knows about.
        let p = &b.peripherals[0];
        let _ = p.params.u8("cs");
        let _ = p.params.u16("width");
        b.collect_warnings();
        assert_eq!(b.warnings, vec!["st7789: unrecognised key \"wdith\""]);
    }
}
