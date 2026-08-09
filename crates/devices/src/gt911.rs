//! GT911 capacitive touch controller.
//!
//! Register-addressed over I2C: the host writes a 16-bit big-endian register
//! address, then either continues writing or issues a repeated start and
//! reads. Everything interesting lives in three places -- the product id, the
//! status byte, and the point buffer.
//!
//! The touch itself comes from [`vpb::input::TouchState`], shared with the UI,
//! so a mouse click on the emulated panel arrives here as a press at panel
//! coordinates.

use std::sync::{Arc, Mutex};

use vpb::input::{Rotation, TouchState};
use vpb::{Claim, EventSink, Peripheral, Response, Transaction};

/// Registers, from the GT911 programming guide.
mod reg {
    /// Four ASCII bytes: "911\0". How every driver identifies the part.
    pub const PRODUCT_ID: u16 = 0x8140;
    /// Firmware version, two bytes little-endian.
    pub const FIRMWARE_VERSION: u16 = 0x8144;
    /// Resolution, four bytes: x then y, little-endian.
    pub const X_RESOLUTION: u16 = 0x8146;
    pub const VENDOR_ID: u16 = 0x814a;
    /// Buffer status and touch count. Bit 7 means the points are fresh; the
    /// host clears the whole byte when it has read them.
    pub const STATUS: u16 = 0x814e;
    /// First of five 8-byte point records.
    pub const POINT_0: u16 = 0x814f;
    /// Start of the configuration block, which drivers write and re-read.
    pub const CONFIG_START: u16 = 0x8047;
    pub const CONFIG_END: u16 = 0x80ff;
}

const STATUS_READY: u8 = 0x80;
const POINT_LEN: u16 = 8;

/// Shared with the UI so a click on the panel widget lands here.
pub type TouchHandle = Arc<Mutex<TouchState>>;

/// How a finger on the glass turns into numbers in the point registers.
///
/// Two resolutions, because on real hardware they disagree. The chip
/// *advertises* one at `X_RESOLUTION` and drivers read it; the points it
/// actually emits span another, in the panel's own frame rather than the
/// display's. A T-Deck advertises the display's 320x240 while its points run
/// 0..=311 by 0..=235 rotated a quarter turn -- so a model that emits screen
/// coordinates straight through puts every tap in the wrong place, and only
/// the centre of the screen looks right.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    /// Resolution reported at `X_RESOLUTION`, in the display's frame.
    pub width: u16,
    pub height: u16,
    /// Range the point registers span, in the panel's own frame.
    pub point_width: u16,
    pub point_height: u16,
    /// How the panel is mounted relative to the displayed image.
    pub rotation: Rotation,
}

impl Geometry {
    /// A panel whose points span exactly what it advertises, mounted square.
    /// True of most boards, and the right default for one we have not
    /// measured.
    pub fn new(width: u16, height: u16) -> Self {
        Geometry {
            width,
            height,
            point_width: width,
            point_height: height,
            rotation: Rotation::None,
        }
    }

    /// Override the range the point registers span.
    pub fn points(mut self, width: u16, height: u16) -> Self {
        self.point_width = width;
        self.point_height = height;
        self
    }

    pub fn rotated(mut self, rotation: Rotation) -> Self {
        self.rotation = rotation;
        self
    }
}

pub struct Gt911 {
    /// Every address this panel answers on -- one per controller that can
    /// drive its pins, since firmware chooses the controller.
    claims: Vec<Claim>,
    touch: TouchHandle,
    width: u16,
    height: u16,
    /// Register address set by the last write, auto-incrementing across reads.
    cursor: u16,
    /// The configuration block, kept because drivers write it and read it
    /// back to confirm. The contents mean nothing to this model.
    config: Vec<u8>,
    /// Set when a touch is latched into the point buffer, cleared when the
    /// host acknowledges by writing zero to STATUS.
    reported: Option<(u16, u16)>,
}

impl std::fmt::Debug for Gt911 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gt911")
            .field("size", &(self.width, self.height))
            .field("cursor", &format_args!("{:#06x}", self.cursor))
            .field("reported", &self.reported)
            .finish_non_exhaustive()
    }
}

impl Gt911 {
    pub fn new(claims: Vec<Claim>, geometry: Geometry) -> Self {
        let Geometry { width, height, point_width, point_height, rotation } = geometry;
        Gt911 {
            claims,
            touch: Arc::new(Mutex::new(TouchState::new(point_width, point_height, rotation))),
            width,
            height,
            cursor: 0,
            config: vec![0; (reg::CONFIG_END - reg::CONFIG_START + 1) as usize],
            reported: None,
        }
    }

    /// The shared touch state, for the UI to drive.
    pub fn touch(&self) -> &TouchHandle {
        &self.touch
    }

    /// Read `len` bytes starting at the cursor, advancing it.
    fn read(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(self.byte_at(self.cursor));
            self.cursor = self.cursor.wrapping_add(1);
        }
        out
    }

    fn byte_at(&mut self, addr: u16) -> u8 {
        match addr {
            // "911" and a NUL, which is what drivers match on.
            a if (reg::PRODUCT_ID..reg::PRODUCT_ID + 4).contains(&a) => {
                b"911\0"[(a - reg::PRODUCT_ID) as usize]
            }
            reg::FIRMWARE_VERSION => 0x60,
            a if a == reg::FIRMWARE_VERSION + 1 => 0x10,

            // Resolution, little-endian, so a driver that scales by it agrees
            // with the panel the display model is drawing.
            reg::X_RESOLUTION => self.width as u8,
            a if a == reg::X_RESOLUTION + 1 => (self.width >> 8) as u8,
            a if a == reg::X_RESOLUTION + 2 => self.height as u8,
            a if a == reg::X_RESOLUTION + 3 => (self.height >> 8) as u8,

            reg::VENDOR_ID => 0x01,

            reg::STATUS => self.status(),

            // One point record: track id, x, y, size, reserved.
            a if (reg::POINT_0..reg::POINT_0 + POINT_LEN).contains(&a) => {
                self.point_byte(a - reg::POINT_0)
            }

            a if (reg::CONFIG_START..=reg::CONFIG_END).contains(&a) => {
                self.config[(a - reg::CONFIG_START) as usize]
            }

            _ => 0,
        }
    }

    /// Latch a touch if one is waiting, and report whether the buffer is full.
    fn status(&mut self) -> u8 {
        if self.reported.is_none() {
            // Taking it here, at the moment the host asks, is what makes a
            // click shorter than one poll interval still register.
            let point = match self.touch.lock() {
                Ok(mut t) => t.take_report(),
                Err(poisoned) => poisoned.into_inner().take_report(),
            };
            self.reported = point.map(|p| (p.x, p.y));
        }
        match self.reported {
            Some(_) => STATUS_READY | 1,
            // Ready, zero points: the panel is working and nothing is touching
            // it. Reporting "not ready" instead makes a driver wait forever.
            None => STATUS_READY,
        }
    }

    fn point_byte(&self, offset: u16) -> u8 {
        let Some((x, y)) = self.reported else {
            return 0;
        };
        match offset {
            0 => 0,               // track id
            1 => x as u8,
            2 => (x >> 8) as u8,
            3 => y as u8,
            4 => (y >> 8) as u8,
            5 => 0x20,            // contact size; any non-zero value will do
            _ => 0,
        }
    }

    fn write(&mut self, data: &[u8]) {
        // Every write starts with the 16-bit register address, big-endian --
        // the one place this chip is not little-endian.
        let Some(addr) = data.get(..2) else {
            return;
        };
        self.cursor = u16::from_be_bytes([addr[0], addr[1]]);

        for &byte in &data[2..] {
            match self.cursor {
                // Clearing the status is the host saying it has taken the
                // points, which is what frees the buffer for the next one.
                reg::STATUS if byte == 0 => self.reported = None,
                a if (reg::CONFIG_START..=reg::CONFIG_END).contains(&a) => {
                    self.config[(a - reg::CONFIG_START) as usize] = byte;
                }
                _ => {}
            }
            self.cursor = self.cursor.wrapping_add(1);
        }
    }
}

impl Peripheral for Gt911 {
    fn kind(&self) -> &str {
        "gt911"
    }

    fn claims(&self) -> Vec<Claim> {
        self.claims.clone()
    }

    fn transact(&mut self, tx: &Transaction, _events: &mut dyn EventSink) -> Response {
        match tx {
            Transaction::I2cWrite { data, .. } => {
                self.write(data);
                Response::None
            }
            Transaction::I2cRead { len, .. } => Response::data(self.read(*len as usize)),
            _ => Response::None,
        }
    }

    fn decode(&self, tx: &Transaction, _response: &Response) -> Option<String> {
        let name = |addr: u16| match addr {
            reg::PRODUCT_ID => "PRODUCT_ID",
            reg::STATUS => "STATUS",
            reg::POINT_0 => "POINT_0",
            reg::X_RESOLUTION => "RESOLUTION",
            a if (reg::CONFIG_START..=reg::CONFIG_END).contains(&a) => "CONFIG",
            _ => "reg",
        };
        match tx {
            Transaction::I2cWrite { data, .. } if data.len() >= 2 => {
                let addr = u16::from_be_bytes([data[0], data[1]]);
                Some(if data.len() == 2 {
                    format!("seek {} ({addr:#06x})", name(addr))
                } else {
                    format!("write {} ({addr:#06x}) {} bytes", name(addr), data.len() - 2)
                })
            }
            Transaction::I2cRead { len, .. } => {
                Some(format!("read {} ({:#06x}) x{len}", name(self.cursor), self.cursor))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vpb::input::PointerPhase;

    /// The T-Deck's panel, in the frame its point registers actually use.
    ///
    /// The numbers come from PURR OS's own driver, which measured them on
    /// hardware rather than trusting the datasheet: the raw fields span
    /// 0..=311 and 0..=235, and the panel reads back a quarter turn from the
    /// display. Points are one past each maximum, since a span of 0..=311 is
    /// 312 values wide.
    fn t_deck() -> Geometry {
        Geometry::new(320, 240).points(312, 236).rotated(Rotation::Cw270)
    }

    /// PURR OS's transform, transcribed from `drivers/touch/gt911/gt911.c`.
    ///
    /// This is the half of the contract we do not control. Writing it out
    /// here is what makes the test meaningful: it fails if our model stops
    /// agreeing with the firmware, not merely with itself.
    fn firmware_maps(raw_x: u16, raw_y: u16) -> (i32, i32) {
        const LCD_W: i32 = 320;
        const LCD_H: i32 = 240;
        const NATIVE_X_MAX: i32 = 311;
        const NATIVE_Y_MAX: i32 = 235;

        let sx = (i32::from(raw_y) * LCD_W) / NATIVE_Y_MAX;
        let sy = (LCD_H - 1) - (i32::from(raw_x) * LCD_H) / NATIVE_X_MAX;
        (sx.clamp(0, LCD_W - 1), sy.clamp(0, LCD_H - 1))
    }

    /// Where the firmware believes a click at `(x, y)` on the displayed
    /// 320x240 image happened.
    fn round_trip(geometry: Geometry, x: f32, y: f32) -> (i32, i32) {
        let panel = Gt911::new(vec![Claim::I2c { controller: 0, address: 0x5d, alt: None }], geometry);
        let mut touch = panel.touch().lock().unwrap();
        touch.pointer(PointerPhase::Press, x, y, 320.0, 240.0);
        let point = touch.current().expect("a press inside the image is a touch");
        firmware_maps(point.x, point.y)
    }

    #[track_caller]
    fn lands_at(x: f32, y: f32, want: (i32, i32)) {
        let got = round_trip(t_deck(), x, y);
        // Two axes of integer division, each rounding down; a couple of
        // pixels of slack is the arithmetic, not a mapping error.
        let slack = (got.0 - want.0).abs() <= 2 && (got.1 - want.1).abs() <= 2;
        assert!(slack, "click ({x}, {y}) reached the firmware as {got:?}, wanted {want:?}");
    }

    #[test]
    fn taps_land_where_the_firmware_thinks_they_did() {
        lands_at(160.0, 120.0, (160, 120));
        lands_at(0.0, 0.0, (0, 0));
        lands_at(319.0, 0.0, (319, 0));
        lands_at(0.0, 239.0, (0, 239));
        lands_at(319.0, 239.0, (319, 239));
        // Off-centre and asymmetric, which is where a swapped axis hides.
        lands_at(80.0, 60.0, (80, 60));
        lands_at(240.0, 180.0, (240, 180));
    }

    #[test]
    fn passing_screen_coordinates_straight_through_is_what_broke_it() {
        // The old configuration: no rotation, points assumed to span the
        // display. Kept as a test because the failure is so plausible-looking
        // -- the centre still lands on the centre, so the one gesture anybody
        // tries first (tap to unlock) works, and everything else is wrong.
        let naive = Geometry::new(320, 240);
        let centre = round_trip(naive, 160.0, 120.0);
        assert!((centre.0 - 160).abs() <= 4 && (centre.1 - 120).abs() <= 4, "{centre:?}");

        let corner = round_trip(naive, 10.0, 10.0);
        assert!(corner.1 > 200, "the top of the screen reported as the bottom: {corner:?}");
    }

    #[test]
    fn a_board_that_has_not_been_measured_maps_one_to_one() {
        let plain = Geometry::new(320, 240);
        assert_eq!(plain.point_width, 320);
        assert_eq!(plain.point_height, 240);
        assert_eq!(plain.rotation, Rotation::None);
    }

    #[test]
    fn the_advertised_resolution_is_not_the_point_range() {
        // A driver reading X_RESOLUTION still sees the display's numbers,
        // which is what the chip does on hardware even though its points
        // span something else.
        let mut panel =
            Gt911::new(vec![Claim::I2c { controller: 0, address: 0x5d, alt: None }], t_deck());
        panel.cursor = reg::X_RESOLUTION;
        let res = panel.read(4);
        assert_eq!(u16::from_le_bytes([res[0], res[1]]), 320);
        assert_eq!(u16::from_le_bytes([res[2], res[3]]), 240);
    }
}
