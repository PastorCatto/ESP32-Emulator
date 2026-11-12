//! A button-and-LED module built on an MCP23017 I2C port expander.
//!
//! The chip is generic: two 8-bit ports, a direction register, pull-ups, and
//! a latch per port. What makes it a game pad is what is wired to the pins,
//! so the wiring lives in [`Wiring`] and the chip logic does not know about
//! buttons. The one wiring modelled here is a small diode matrix with one LED
//! per button:
//!
//! | Pin | Meaning |
//! |---|---|
//! | GPB0, GPB1 | matrix rows 1 and 2 |
//! | GPB2..GPB5 | matrix columns 1 to 4 |
//! | GPB6 | the function button, wired straight to ground |
//! | GPB7 | the LED of button "up" |
//! | GPA0..GPA7 | the other LEDs |
//!
//! Each button is in series with a diode whose anode is on the column and
//! whose cathode is on the row. A column can therefore only be pulled low by
//! a row that is an output driven low, which is how the scan reads it: make
//! one row an output at 0, read the columns (pulled up), and a column reading
//! 0 has a pressed button at that row. LEDs have their cathode on the pin, so
//! a pin driven low lights its LED.
//!
//! Registers follow the chip's default `IOCON.BANK = 0` layout with
//! sequential addressing. Not modelled: `BANK = 1` addressing, and the
//! interrupt outputs (the registers are stored, but nothing is ever flagged
//! and no INT pin is driven). Written from the driver source that uses it and
//! from memory of the chip's register map, not from a read of the datasheet.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use vpb::{Claim, EventSink, Peripheral, Response, Transaction};

/// One bit per button, in the order a game-pad API usually numbers them.
pub mod button {
    pub const UP: u16 = 1 << 0;
    pub const DOWN: u16 = 1 << 1;
    pub const LEFT: u16 = 1 << 2;
    pub const RIGHT: u16 = 1 << 3;
    pub const A: u16 = 1 << 4;
    pub const B: u16 = 1 << 5;
    pub const X: u16 = 1 << 6;
    pub const Y: u16 = 1 << 7;
    pub const FUNC: u16 = 1 << 8;

    /// Every button, with the name the window shows.
    pub const ALL: [(u16, &str); 9] = [
        (UP, "up"),
        (DOWN, "down"),
        (LEFT, "left"),
        (RIGHT, "right"),
        (A, "A"),
        (B, "B"),
        (X, "X"),
        (Y, "Y"),
        (FUNC, "func"),
    ];
}

/// What the window and the chip share: which buttons are held and which LEDs
/// are lit. Bits are the [`button`] constants.
#[derive(Debug, Default)]
pub struct PadState {
    buttons: AtomicU16,
    leds: AtomicU16,
}

impl PadState {
    /// Press or release the buttons in `mask`.
    pub fn set(&self, mask: u16, down: bool) {
        if down {
            self.buttons.fetch_or(mask, Ordering::Relaxed);
        } else {
            self.buttons.fetch_and(!mask, Ordering::Relaxed);
        }
    }

    /// Replace the whole set of held buttons at once, so a reader never sees
    /// a half-updated state between a release and a press.
    pub fn hold(&self, mask: u16) {
        self.buttons.store(mask, Ordering::Relaxed);
    }

    /// Release everything, as when the module is unplugged.
    pub fn release_all(&self) {
        self.buttons.store(0, Ordering::Relaxed);
    }

    pub fn buttons(&self) -> u16 {
        self.buttons.load(Ordering::Relaxed)
    }

    /// The buttons whose LED is lit right now.
    pub fn leds(&self) -> u16 {
        self.leds.load(Ordering::Relaxed)
    }
}

/// Shared handle the UI keeps.
pub type PadHandle = Arc<PadState>;

/// What the pins are connected to, outside the chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wiring {
    /// Nothing: inputs read their pull-up (or 0 with none), outputs read back
    /// their latch. Useful as a bare expander.
    Bare,
    /// The diode matrix with LEDs described in the module documentation.
    Pad,
}

// Register numbers, `IOCON.BANK = 0`.
const IODIRA: usize = 0x00;
const IODIRB: usize = 0x01;
const IPOLA: usize = 0x02;
const IPOLB: usize = 0x03;
const IOCON: usize = 0x0a;
const IOCON_MIRROR: usize = 0x0b;
const GPPUA: usize = 0x0c;
const GPPUB: usize = 0x0d;
const INTFA: usize = 0x0e;
const INTCAPB: usize = 0x11;
const GPIOA: usize = 0x12;
const GPIOB: usize = 0x13;
const OLATA: usize = 0x14;
const OLATB: usize = 0x15;
const REGS: usize = 0x16;

/// `IOCON.SEQOP`: when set the register pointer stays put instead of
/// advancing after each byte.
const SEQOP: u8 = 1 << 5;

// The pad's wiring, by pin number within port B.
const ROW_PINS: [u8; 2] = [0, 1];
const COL_PINS: [u8; 4] = [2, 3, 4, 5];
const FUNC_PIN: u8 = 6;

/// `[row][column]` -> button.
const MATRIX: [[u16; 4]; 2] = [
    [button::UP, button::DOWN, button::LEFT, button::RIGHT],
    [button::A, button::B, button::X, button::Y],
];

/// Where each button's LED is: `(button, port_is_b, bit)`.
const LEDS: [(u16, bool, u8); 9] = [
    (button::UP, true, 7),
    (button::DOWN, false, 0),
    (button::LEFT, false, 1),
    (button::RIGHT, false, 2),
    (button::A, false, 3),
    (button::B, false, 4),
    (button::X, false, 5),
    (button::Y, false, 6),
    (button::FUNC, false, 7),
];

#[derive(Debug)]
pub struct Mcp23017 {
    claims: Vec<Claim>,
    wiring: Wiring,
    pad: PadHandle,
    regs: [u8; REGS],
    pointer: usize,
}

impl Mcp23017 {
    pub fn new(claims: Vec<Claim>, wiring: Wiring) -> Self {
        let mut chip = Mcp23017 {
            claims,
            wiring,
            pad: Arc::new(PadState::default()),
            regs: [0; REGS],
            pointer: 0,
        };
        chip.power_on();
        chip
    }

    /// The pad's buttons and LEDs, for the window to drive and show.
    pub fn pad(&self) -> &PadHandle {
        &self.pad
    }

    fn power_on(&mut self) {
        self.regs = [0; REGS];
        // Every pin an input at power-on; everything else zero.
        self.regs[IODIRA] = 0xff;
        self.regs[IODIRB] = 0xff;
        self.pointer = 0;
        self.update_leds();
    }

    /// The level of every pin on a port, as a read of GPIO would see it.
    fn pins(&self, port_b: bool) -> u8 {
        let (dir, pol, latch) = if port_b {
            (IODIRB, IPOLB, OLATB)
        } else {
            (IODIRA, IPOLA, OLATA)
        };
        let dir = self.regs[dir];
        let mut level = 0u8;
        for bit in 0..8 {
            let mask = 1u8 << bit;
            let high = if dir & mask == 0 {
                // An output reads back what it drives.
                self.regs[latch] & mask != 0
            } else {
                self.input_level(port_b, bit)
            };
            if high {
                level |= mask;
            }
        }
        // Polarity inversion only applies to inputs.
        level ^ (self.regs[pol] & dir)
    }

    /// What an input pin sees from the outside world.
    fn input_level(&self, port_b: bool, bit: u8) -> bool {
        let pulled_up = self.regs[if port_b { GPPUB } else { GPPUA }] & (1 << bit) != 0;
        if self.wiring == Wiring::Pad && port_b {
            let held = self.pad.buttons();
            // A column is pulled low through a button's diode by a row that
            // is an output driven low.
            if let Some(col) = COL_PINS.iter().position(|&p| p == bit) {
                for (row, &row_pin) in ROW_PINS.iter().enumerate() {
                    let row_mask = 1u8 << row_pin;
                    let row_low_output =
                        self.regs[IODIRB] & row_mask == 0 && self.regs[OLATB] & row_mask == 0;
                    if row_low_output && held & MATRIX[row][col] != 0 {
                        return false;
                    }
                }
            }
            // The function button goes straight to ground.
            if bit == FUNC_PIN && held & button::FUNC != 0 {
                return false;
            }
        }
        pulled_up
    }

    /// Recompute which LEDs are lit: an output pin driven low.
    fn update_leds(&self) {
        if self.wiring != Wiring::Pad {
            self.pad.leds.store(0, Ordering::Relaxed);
            return;
        }
        let mut lit = 0u16;
        for (button, port_b, bit) in LEDS {
            let (dir, latch) = if port_b { (IODIRB, OLATB) } else { (IODIRA, OLATA) };
            let mask = 1u8 << bit;
            if self.regs[dir] & mask == 0 && self.regs[latch] & mask == 0 {
                lit |= button;
            }
        }
        self.pad.leds.store(lit, Ordering::Relaxed);
    }

    fn read_reg(&self, reg: usize) -> u8 {
        match reg {
            GPIOA => self.pins(false),
            GPIOB => self.pins(true),
            IOCON_MIRROR => self.regs[IOCON],
            r if r < REGS => self.regs[r],
            _ => 0,
        }
    }

    fn write_reg(&mut self, reg: usize, value: u8) {
        match reg {
            // Writing the port register writes the latch.
            GPIOA => self.regs[OLATA] = value,
            GPIOB => self.regs[OLATB] = value,
            IOCON_MIRROR => self.regs[IOCON] = value,
            // The interrupt flag and capture registers are read-only.
            r if (INTFA..=INTCAPB).contains(&r) => {}
            r if r < REGS => self.regs[r] = value,
            _ => {}
        }
        self.update_leds();
    }

    fn advance(&mut self) {
        if self.regs[IOCON] & SEQOP != 0 {
            return;
        }
        self.pointer = (self.pointer + 1) % REGS;
    }
}

fn reg_name(reg: usize) -> &'static str {
    const NAMES: [&str; REGS] = [
        "IODIRA", "IODIRB", "IPOLA", "IPOLB", "GPINTENA", "GPINTENB", "DEFVALA", "DEFVALB",
        "INTCONA", "INTCONB", "IOCON", "IOCON", "GPPUA", "GPPUB", "INTFA", "INTFB", "INTCAPA",
        "INTCAPB", "GPIOA", "GPIOB", "OLATA", "OLATB",
    ];
    NAMES.get(reg).copied().unwrap_or("?")
}

impl Peripheral for Mcp23017 {
    fn kind(&self) -> &str {
        "gamepad"
    }

    fn claims(&self) -> Vec<Claim> {
        self.claims.clone()
    }

    fn transact(&mut self, tx: &Transaction, _events: &mut dyn EventSink) -> Response {
        match tx {
            Transaction::I2cWrite { data, .. } => {
                // An empty write is an address probe: acknowledge it.
                let Some((&reg, values)) = data.split_first() else {
                    return Response::None;
                };
                // Addresses past the last register belong to the other
                // `BANK` layout, which is not modelled; fold them in.
                self.pointer = reg as usize % REGS;
                for &value in values {
                    self.write_reg(self.pointer, value);
                    self.advance();
                }
                Response::None
            }
            Transaction::I2cRead { len, .. } => {
                let mut out = Vec::with_capacity(*len as usize);
                for _ in 0..*len {
                    out.push(self.read_reg(self.pointer));
                    self.advance();
                }
                Response::data(out)
            }
            Transaction::Reset => {
                self.power_on();
                Response::None
            }
            _ => Response::None,
        }
    }

    fn decode(&self, tx: &Transaction, response: &Response) -> Option<String> {
        match tx {
            Transaction::I2cWrite { data, .. } => match data.split_first() {
                None => Some("probe".into()),
                Some((&reg, [])) => Some(format!("point at {}", reg_name(reg as usize))),
                Some((&reg, values)) => Some(format!("write {} = {:02x?}", reg_name(reg as usize), values)),
            },
            Transaction::I2cRead { .. } => Some(format!(
                "read {} = {:02x?}",
                reg_name(self.pointer),
                response.payload()
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vpb::registry::EventQueue;

    const ADDR: u8 = 0x20;

    fn pad() -> Mcp23017 {
        Mcp23017::new(
            vec![Claim::I2c { controller: 0, address: ADDR, alt: None }],
            Wiring::Pad,
        )
    }

    fn write(d: &mut Mcp23017, bytes: &[u8]) {
        let mut ev = EventQueue::default();
        d.transact(
            &Transaction::I2cWrite { controller: 0, address: ADDR, data: bytes.to_vec(), stop: true },
            &mut ev,
        );
    }

    fn read(d: &mut Mcp23017, len: u32) -> Vec<u8> {
        let mut ev = EventQueue::default();
        d.transact(&Transaction::I2cRead { controller: 0, address: ADDR, len }, &mut ev)
            .payload()
            .to_vec()
    }

    /// Register read as the maker's driver does it: write the register
    /// number, then read one byte.
    fn reg(d: &mut Mcp23017, r: u8) -> u8 {
        write(d, &[r]);
        read(d, 1)[0]
    }

    /// What that driver's start-up writes.
    fn driver_init(d: &mut Mcp23017) {
        write(d, &[0x01, 0x7f]); // IODIRB: all inputs but the LED pin
        // Pull-ups on the columns and the function button, as one 16-bit write.
        let cols: u16 = (1 << 10) | (1 << 11) | (1 << 12) | (1 << 13) | (1 << 14);
        write(d, &[0x0c, (cols & 0xff) as u8, (cols >> 8) as u8]);
        write(d, &[0x15, 0x80]); // OLATB: rows low, LED_UP off
        write(d, &[0x00, 0x00]); // IODIRA: all outputs
        write(d, &[0x14, 0xff]); // OLATA: every LED off
    }

    /// One pass of that driver's scan for `row`: only that row becomes an
    /// output (the latch is already low), then GPIOB is read.
    fn scan_row(d: &mut Mcp23017, row: u8) -> u8 {
        write(d, &[0x01, 0x7f]);
        write(d, &[0x01, 0x7f & !(1 << row)]);
        reg(d, 0x13)
    }

    #[test]
    fn every_pin_is_an_input_at_power_on() {
        let mut d = pad();
        assert_eq!(reg(&mut d, 0x00), 0xff);
        assert_eq!(reg(&mut d, 0x01), 0xff);
        assert_eq!(reg(&mut d, 0x14), 0x00);
    }

    #[test]
    fn an_empty_write_is_acknowledged_as_a_probe() {
        let mut d = pad();
        let mut ev = EventQueue::default();
        let r = d.transact(
            &Transaction::I2cWrite { controller: 0, address: ADDR, data: vec![], stop: true },
            &mut ev,
        );
        assert_eq!(r, Response::None, "not a Nack: the chip is there");
    }

    #[test]
    fn a_sixteen_bit_write_lands_in_both_ports() {
        let mut d = pad();
        write(&mut d, &[0x0c, 0xaa, 0x55]);
        assert_eq!(reg(&mut d, 0x0c), 0xaa);
        assert_eq!(reg(&mut d, 0x0d), 0x55);
    }

    #[test]
    fn the_register_pointer_wraps_after_the_last_register() {
        let mut d = pad();
        write(&mut d, &[0x15]);
        let two = read(&mut d, 2);
        assert_eq!(two[1], 0xff, "the second byte is IODIRA again");
    }

    #[test]
    fn with_sequential_mode_off_the_pointer_stays_put() {
        let mut d = pad();
        write(&mut d, &[0x0a, SEQOP]);
        write(&mut d, &[0x14]);
        write(&mut d, &[0x14, 0x12, 0x34]);
        assert_eq!(reg(&mut d, 0x14), 0x34, "both bytes went to the same register");
    }

    #[test]
    fn nothing_is_pressed_until_a_button_is() {
        let mut d = pad();
        driver_init(&mut d);
        for row in 0..2 {
            let port = scan_row(&mut d, row);
            for col in COL_PINS {
                assert_ne!(port & (1 << col), 0, "row {row} column pin {col} should read high");
            }
        }
    }

    #[test]
    fn a_pressed_button_pulls_its_column_low_only_on_its_own_row() {
        let mut d = pad();
        driver_init(&mut d);
        // A is row 2, column 1.
        d.pad().set(button::A, true);
        let row1 = scan_row(&mut d, 0);
        let row2 = scan_row(&mut d, 1);
        assert_ne!(row1 & (1 << COL_PINS[0]), 0, "row 1 does not see A");
        assert_eq!(row2 & (1 << COL_PINS[0]), 0, "row 2 sees A in column 1");
        // And no other column.
        for col in &COL_PINS[1..] {
            assert_ne!(row2 & (1 << col), 0);
        }
    }

    #[test]
    fn every_button_is_found_at_its_row_and_column() {
        let mut d = pad();
        driver_init(&mut d);
        for (row, cols) in MATRIX.iter().enumerate() {
            for (col, &b) in cols.iter().enumerate() {
                d.pad().release_all();
                d.pad().set(b, true);
                let port = scan_row(&mut d, row as u8);
                assert_eq!(port & (1 << COL_PINS[col]), 0, "button {b:#x} at row {row} col {col}");
            }
        }
    }

    #[test]
    fn a_column_is_not_pulled_down_when_no_row_is_driving() {
        // The diodes only conduct column -> row, so with every row an input
        // a held button cannot be seen: this is why the driver switches rows.
        let mut d = pad();
        driver_init(&mut d);
        write(&mut d, &[0x01, 0x7f]);
        d.pad().set(button::UP, true);
        assert_ne!(reg(&mut d, 0x13) & (1 << COL_PINS[0]), 0);
    }

    #[test]
    fn the_function_button_reads_low_directly() {
        let mut d = pad();
        driver_init(&mut d);
        assert_ne!(reg(&mut d, 0x13) & (1 << FUNC_PIN), 0);
        d.pad().set(button::FUNC, true);
        assert_eq!(reg(&mut d, 0x13) & (1 << FUNC_PIN), 0);
    }

    #[test]
    fn the_driver_start_up_leaves_every_led_off() {
        let mut d = pad();
        driver_init(&mut d);
        assert_eq!(d.pad().leds(), 0);
    }

    #[test]
    fn an_led_lights_when_its_pin_is_driven_low() {
        let mut d = pad();
        driver_init(&mut d);
        // LED_A is GPA3: clear that latch bit.
        write(&mut d, &[0x14, !(1 << 3)]);
        assert_eq!(d.pad().leds(), button::A);
        // LED_UP is GPB7, in port B's latch.
        write(&mut d, &[0x15, 0x00]);
        assert_eq!(d.pad().leds(), button::A | button::UP);
        write(&mut d, &[0x15, 0x80]);
        write(&mut d, &[0x14, 0xff]);
        assert_eq!(d.pad().leds(), 0);
    }

    #[test]
    fn leds_follow_the_driver_mask_for_every_button() {
        let mut d = pad();
        driver_init(&mut d);
        // The driver's "all LEDs from a mask" call: port A is the inverse of
        // (mask without UP) >> 1, UP is bit 7 of port B, low = on.
        let mask = button::DOWN | button::Y | button::FUNC | button::UP;
        let a = !((mask & !button::UP) >> 1) as u8;
        write(&mut d, &[0x14, a]);
        write(&mut d, &[0x15, 0x00]);
        assert_eq!(d.pad().leds(), mask);
    }

    #[test]
    fn a_pin_left_as_an_input_does_not_light_its_led() {
        let mut d = pad();
        // Port A is still all inputs, so nothing is lit whatever the latch says.
        write(&mut d, &[0x14, 0x00]);
        assert_eq!(d.pad().leds(), 0);
    }

    #[test]
    fn a_reset_puts_the_chip_back_to_power_on() {
        let mut d = pad();
        driver_init(&mut d);
        write(&mut d, &[0x14, 0x00]);
        let mut ev = EventQueue::default();
        d.transact(&Transaction::Reset, &mut ev);
        assert_eq!(reg(&mut d, 0x00), 0xff);
        assert_eq!(d.pad().leds(), 0);
    }

    #[test]
    fn the_trace_names_registers() {
        let d = pad();
        let tx = Transaction::I2cWrite { controller: 0, address: ADDR, data: vec![0x0c, 1, 2], stop: true };
        assert_eq!(
            d.decode(&tx, &Response::None).as_deref(),
            Some("write GPPUA = [01, 02]")
        );
    }
}
