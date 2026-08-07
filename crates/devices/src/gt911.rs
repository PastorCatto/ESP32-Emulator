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

pub struct Gt911 {
    claim: Claim,
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
    pub fn new(claim: Claim, width: u16, height: u16, rotation: Rotation) -> Self {
        Gt911 {
            claim,
            touch: Arc::new(Mutex::new(TouchState::new(width, height, rotation))),
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
        vec![self.claim.clone()]
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
