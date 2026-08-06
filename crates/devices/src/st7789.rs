//! ST7789 TFT controller.
//!
//! Decodes the SPI stream a display driver produces into a framebuffer the
//! shell can draw. The panel is write-only in practice: firmware pushes
//! pixels and never reads them back, so this keeps the image and answers
//! reads with zeroes.
//!
//! The controller distinguishes a command byte from pixel data by a GPIO, not
//! by anything on the bus, so the emulated SPI controller latches that pin and
//! carries its level on each transaction. Without it the same byte is both
//! `RAMWR` and a shade of grey.

use std::sync::{Arc, Mutex};

use vpb::{Claim, EventSink, Peripheral, Response, Transaction};

/// The decoded image, shared with whatever draws it.
///
/// Held outside the device because the renderer and the bus run on different
/// threads: the vpb server is blocked reading a socket while the UI wants to
/// upload a texture. The alternative -- reaching into the registry and
/// downcasting a `dyn Peripheral` back to a display -- makes every consumer
/// know what kind of panel it is looking at.
#[derive(Debug, Clone)]
pub struct Screen {
    pub width: u16,
    pub height: u16,
    /// RGB565 as the driver wrote it, row-major.
    pub pixels: Vec<u16>,
    /// Bumped on every write, so a renderer can skip an unchanged frame.
    pub generation: u64,
    /// Whether the driver has turned the panel on. A panel that is off shows
    /// black whatever is in its RAM.
    pub on: bool,
    /// Set by MADCTL: the 16-bit values are B-G-R rather than R-G-B.
    pub bgr: bool,
    pub inverted: bool,
}

pub type ScreenHandle = Arc<Mutex<Screen>>;

impl Screen {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; width as usize * height as usize],
            generation: 0,
            on: false,
            bgr: false,
            inverted: false,
        }
    }

    pub fn handle(width: u16, height: u16) -> ScreenHandle {
        Arc::new(Mutex::new(Self::new(width, height)))
    }

    /// The image as 8-bit RGB triples, ready to hand to a texture upload.
    ///
    /// Colour order and inversion are applied here rather than at write time,
    /// so a driver that flips either after drawing gets the right result
    /// instead of a half-converted buffer.
    pub fn rgb888(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len() * 3);

        for &pixel in &self.pixels {
            let p = if self.inverted { !pixel } else { pixel };
            // 5-6-5 scaled to 8 bits by repeating the high bits, which keeps
            // full white at 0xff instead of 0xf8.
            let hi = ((p >> 11) & 0x1f) as u8;
            let mid = ((p >> 5) & 0x3f) as u8;
            let lo = (p & 0x1f) as u8;
            let hi = (hi << 3) | (hi >> 2);
            let mid = (mid << 2) | (mid >> 4);
            let lo = (lo << 3) | (lo >> 2);

            if self.bgr {
                out.extend_from_slice(&[lo, mid, hi]);
            } else {
                out.extend_from_slice(&[hi, mid, lo]);
            }
        }
        out
    }
}

/// Commands this model acts on. Everything else is panel tuning -- porch
/// control, gamma, power -- which changes how the glass looks and nothing
/// about what pixels are where, so it is accepted and dropped.
mod cmd {
    pub const SWRESET: u8 = 0x01;
    pub const SLPOUT: u8 = 0x11;
    pub const INVOFF: u8 = 0x20;
    pub const INVON: u8 = 0x21;
    pub const DISPOFF: u8 = 0x28;
    pub const DISPON: u8 = 0x29;
    pub const CASET: u8 = 0x2a;
    pub const RASET: u8 = 0x2b;
    pub const RAMWR: u8 = 0x2c;
    pub const RAMWRC: u8 = 0x3c;
    pub const MADCTL: u8 = 0x36;
    pub const COLMOD: u8 = 0x3a;
}

/// MADCTL bit 3: set means the 16-bit value is B-G-R rather than R-G-B.
const MADCTL_BGR: u8 = 0x08;

/// COLMOD value for 16 bits per pixel, the only format this decodes.
const COLMOD_16BPP: u8 = 0x55;

/// What the controller is doing with the bytes arriving on the data line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sink {
    /// Collecting parameters for `cmd`.
    Params(u8),
    /// Writing pixels into the address window.
    Pixels,
}

pub struct St7789 {
    claim: Claim,
    width: u16,
    height: u16,
    screen: ScreenHandle,

    sink: Sink,
    params: Vec<u8>,

    /// Address window, inclusive, as CASET and RASET set it.
    col: (u16, u16),
    row: (u16, u16),
    /// Where the next pixel lands.
    at: (u16, u16),
    /// First half of a pixel split across two transfers.
    half: Option<u8>,

    /// False once COLMOD asks for something other than 16bpp, after which the
    /// pixel stream cannot be interpreted and is dropped rather than guessed.
    sixteen_bpp: bool,
}

/// Written by hand because the derived version would print every pixel.
impl std::fmt::Debug for St7789 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("St7789")
            .field("size", &(self.width, self.height))
            .field("window", &(self.col, self.row))
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

impl St7789 {
    pub fn new(claim: Claim, screen: ScreenHandle) -> Self {
        let (width, height) = {
            let s = screen.lock().expect("screen lock");
            (s.width, s.height)
        };
        Self {
            claim,
            width,
            height,
            screen,
            sink: Sink::Params(0),
            params: Vec::new(),
            col: (0, width.saturating_sub(1)),
            row: (0, height.saturating_sub(1)),
            at: (0, 0),
            half: None,
            sixteen_bpp: true,
        }
    }

    pub fn screen(&self) -> &ScreenHandle {
        &self.screen
    }

    /// Apply `f` to the shared screen. A poisoned lock means a renderer
    /// panicked mid-frame; the pixels are still structurally fine, and losing
    /// the display for the rest of the session would be the worse outcome.
    fn with_screen(&self, f: impl FnOnce(&mut Screen)) {
        match self.screen.lock() {
            Ok(mut s) => f(&mut s),
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }

    /// A command byte, arriving with the data/command line low.
    fn command(&mut self, code: u8) {
        self.params.clear();
        self.sink = Sink::Params(code);

        match code {
            cmd::SWRESET => {
                self.col = (0, self.width.saturating_sub(1));
                self.row = (0, self.height.saturating_sub(1));
                self.sixteen_bpp = true;
                self.half = None;
                self.with_screen(|s| {
                    s.on = false;
                    s.bgr = false;
                    s.inverted = false;
                });
            }
            cmd::DISPON => self.with_screen(|s| s.on = true),
            cmd::DISPOFF => self.with_screen(|s| s.on = false),
            cmd::INVON => self.with_screen(|s| s.inverted = true),
            cmd::INVOFF => self.with_screen(|s| s.inverted = false),
            cmd::RAMWR => {
                // Rewinds to the top-left of the window; RAMWRC continues from
                // wherever the last write stopped.
                self.at = (self.col.0, self.row.0);
                self.half = None;
                self.sink = Sink::Pixels;
            }
            cmd::RAMWRC => {
                self.half = None;
                self.sink = Sink::Pixels;
            }
            cmd::SLPOUT => {}
            _ => {}
        }
    }

    /// Bytes arriving with the data/command line high.
    fn data(&mut self, bytes: &[u8]) {
        match self.sink {
            Sink::Pixels => self.pixels(bytes),
            Sink::Params(code) => {
                self.params.extend_from_slice(bytes);
                self.apply_params(code);
            }
        }
    }

    fn apply_params(&mut self, code: u8) {
        match code {
            cmd::CASET if self.params.len() >= 4 => {
                self.col = (
                    u16::from_be_bytes([self.params[0], self.params[1]]),
                    u16::from_be_bytes([self.params[2], self.params[3]]),
                );
                self.at = (self.col.0, self.at.1);
            }
            cmd::RASET if self.params.len() >= 4 => {
                self.row = (
                    u16::from_be_bytes([self.params[0], self.params[1]]),
                    u16::from_be_bytes([self.params[2], self.params[3]]),
                );
                self.at = (self.at.0, self.row.0);
            }
            cmd::MADCTL if !self.params.is_empty() => {
                let bgr = self.params[0] & MADCTL_BGR != 0;
                self.with_screen(|s| s.bgr = bgr);
            }
            cmd::COLMOD if !self.params.is_empty() => {
                self.sixteen_bpp = self.params[0] == COLMOD_16BPP;
            }
            _ => {}
        }
    }

    fn pixels(&mut self, bytes: &[u8]) {
        if !self.sixteen_bpp || bytes.is_empty() {
            return;
        }

        // One lock for the whole transfer, not one per pixel: a full frame
        // arrives in ten chunks, and taking the mutex 8192 times per chunk
        // would cost more than decoding the pixels does.
        let mut at = self.at;
        let mut half = self.half.take();
        let (col, row) = (self.col, self.row);
        let (width, height) = (self.width, self.height);

        self.with_screen(|screen| {
            let mut iter = bytes.iter().copied();
            // A pixel can straddle two transfers, because the driver chunks by
            // buffer size and not by pixel count.
            while let Some(high) = half.take().or_else(|| iter.next()) {
                let Some(low) = iter.next() else {
                    half = Some(high);
                    break;
                };

                let (x, y) = at;
                if x < width && y < height {
                    screen.pixels[y as usize * width as usize + x as usize] =
                        u16::from_be_bytes([high, low]);
                }

                // Walk the window left to right, top to bottom, wrapping to
                // its first column at the end of each row -- the window's
                // edge, not the panel's.
                at = if x >= col.1 {
                    (col.0, if y >= row.1 { row.0 } else { y + 1 })
                } else {
                    (x + 1, y)
                };
            }
            screen.generation += 1;
        });

        self.at = at;
        self.half = half;
    }
}

impl Peripheral for St7789 {
    fn kind(&self) -> &str {
        "st7789"
    }

    fn claims(&self) -> Vec<Claim> {
        vec![self.claim.clone()]
    }

    fn transact(&mut self, tx: &Transaction, _events: &mut dyn EventSink) -> Response {
        let Transaction::SpiTransfer { dc, mosi, read_len, .. } = tx else {
            return Response::None;
        };

        match dc {
            Some(false) => {
                for &byte in mosi {
                    self.command(byte);
                }
            }
            Some(true) => self.data(mosi),
            // No data/command pin wired. A driver sends each command as its
            // own single-byte transfer and everything else in larger ones, so
            // length separates them -- but a one-byte *parameter* is
            // indistinguishable from a command, which is exactly why the pin
            // exists. Set `dc` in the board file; this is a fallback, not a
            // second supported mode.
            None => {
                if mosi.len() == 1 && !matches!(self.sink, Sink::Pixels) {
                    self.command(mosi[0]);
                } else {
                    self.data(mosi);
                }
            }
        }

        if *read_len > 0 {
            Response::data(vec![0u8; *read_len as usize])
        } else {
            Response::None
        }
    }

    fn decode(&self, tx: &Transaction, _response: &Response) -> Option<String> {
        let Transaction::SpiTransfer { dc, mosi, .. } = tx else {
            return None;
        };
        // Only a command transfer is worth naming; pixel data is a hex dump
        // either way, and there is a great deal of it.
        if *dc == Some(true) || mosi.len() != 1 {
            return None;
        }
        Some(match mosi[0] {
            cmd::SWRESET => "SWRESET".into(),
            cmd::SLPOUT => "SLPOUT".into(),
            cmd::INVOFF => "INVOFF".into(),
            cmd::INVON => "INVON".into(),
            cmd::DISPOFF => "DISPOFF".into(),
            cmd::DISPON => "DISPON".into(),
            cmd::CASET => format!("CASET x={}..{}", self.col.0, self.col.1),
            cmd::RASET => format!("RASET y={}..{}", self.row.0, self.row.1),
            cmd::RAMWR => "RAMWR".into(),
            cmd::RAMWRC => "RAMWRC".into(),
            cmd::MADCTL => "MADCTL".into(),
            cmd::COLMOD => "COLMOD".into(),
            other => format!("cmd 0x{other:02x}"),
        })
    }
}
