//! The virtual peripheral bus.
//!
//! QEMU emulates the SoC's *bus controllers* (SPI, I2C, UART, GPIO) and nothing
//! that hangs off them. Every attached chip — display, touch panel, SD card,
//! radio — is a [`Peripheral`] living on this side of the wire.
//!
//! There are three ways to add hardware, in increasing order of independence:
//!
//! 1. **Config only.** Change a board TOML. Enough for a different panel size,
//!    a moved chip select, another I2C address.
//! 2. **In-tree driver.** Implement [`Peripheral`] and register it in the
//!    driver registry, behind a Cargo feature so it can be compiled out.
//! 3. **Out-of-process driver.** Speak the wire protocol in
//!    [`wire`] over a socket, in any language. The emulator treats it exactly
//!    like an in-tree driver, because [`Peripheral`] is what it gets adapted to.
//!
//! The unifying idea is the [`Claim`]: a device announces which bus addresses it
//! owns, the registry routes transactions accordingly, and nothing else in the
//! system needs to know which of the three kinds it is.

pub mod registry;
pub mod trace;
pub mod wire;

use serde::{Deserialize, Serialize};

/// A bus address a peripheral owns. Mirrors how real hardware is addressed, so
/// routing is unambiguous and two devices cannot silently claim the same slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "bus", rename_all = "lowercase")]
pub enum Claim {
    /// A chip select on a SPI controller.
    Spi { controller: u8, cs: u8 },
    /// An address on an I2C controller. `alt` covers parts strapped to one of
    /// two addresses, like the GT911.
    I2c {
        controller: u8,
        address: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alt: Option<u8>,
    },
    /// A whole UART, for things like a GNSS receiver spewing NMEA.
    Uart { controller: u8 },
    /// A bare GPIO, for interrupt lines, resets, and the trackball.
    Gpio { pin: u8 },
}

impl Claim {
    /// Does this claim answer for `other`? Handles the I2C alternate address.
    pub fn matches(&self, other: &Claim) -> bool {
        match (self, other) {
            (
                Claim::I2c { controller: c1, address: a1, alt },
                Claim::I2c { controller: c2, address: a2, .. },
            ) => c1 == c2 && (a1 == a2 || alt.as_ref() == Some(a2)),
            _ => self == other,
        }
    }
}

/// Something the SoC did, routed to whichever peripheral claimed the address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Transaction {
    /// One complete SPI transfer, from chip-select assert to deassert.
    ///
    /// Batching a whole transfer rather than a byte at a time is what keeps
    /// this protocol viable: a full 320x240 frame is 150 KiB, and a per-byte
    /// round trip would be unusable.
    SpiTransfer {
        controller: u8,
        cs: u8,
        /// Level of the data/command line during this transfer. The ST7789 and
        /// friends need it to tell a command byte from pixel data, and the
        /// GPIO is sampled at the point the transfer begins.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dc: Option<bool>,
        /// Bytes clocked out by the SoC. Carried as the frame payload, not JSON.
        #[serde(skip)]
        mosi: Vec<u8>,
        /// How many bytes the SoC expects back. Zero means write-only, which
        /// lets the emulator skip waiting for a reply entirely.
        read_len: u32,
    },
    I2cWrite {
        controller: u8,
        address: u8,
        #[serde(skip)]
        data: Vec<u8>,
        /// False for a repeated start, as used by register reads.
        stop: bool,
    },
    I2cRead {
        controller: u8,
        address: u8,
        len: u32,
    },
    /// The SoC drove a GPIO. Devices watch these for reset and backlight lines.
    GpioWrite { pin: u8, level: bool },
    /// Bytes the SoC transmitted on a UART.
    UartTx {
        controller: u8,
        #[serde(skip)]
        data: Vec<u8>,
    },
    /// The machine was reset; drop all device state.
    Reset,
}

impl Transaction {
    /// Which peripheral should handle this.
    pub fn claim(&self) -> Claim {
        match *self {
            Transaction::SpiTransfer { controller, cs, .. } => Claim::Spi { controller, cs },
            Transaction::I2cWrite { controller, address, .. }
            | Transaction::I2cRead { controller, address, .. } => Claim::I2c {
                controller,
                address,
                alt: None,
            },
            Transaction::UartTx { controller, .. } => Claim::Uart { controller },
            Transaction::GpioWrite { pin, .. } => Claim::Gpio { pin },
            // Reset is a broadcast and has no single owner.
            Transaction::Reset => Claim::Gpio { pin: u8::MAX },
        }
    }

    /// True when the SoC is blocked until a peripheral answers. Write-only
    /// traffic is fire-and-forget, which is the difference between a usable
    /// display and a slideshow.
    pub fn expects_reply(&self) -> bool {
        match self {
            Transaction::SpiTransfer { read_len, .. } => *read_len > 0,
            Transaction::I2cRead { .. } => true,
            Transaction::I2cWrite { .. } => true, // needs ACK/NACK
            Transaction::GpioWrite { .. } | Transaction::UartTx { .. } | Transaction::Reset => false,
        }
    }

    /// The bulk bytes carried alongside the JSON header.
    pub fn payload(&self) -> &[u8] {
        match self {
            Transaction::SpiTransfer { mosi, .. } => mosi,
            Transaction::I2cWrite { data, .. } => data,
            Transaction::UartTx { data, .. } => data,
            _ => &[],
        }
    }
}

/// A peripheral's answer to a [`Transaction`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Response {
    /// Nothing to say; also what an unclaimed address returns.
    None,
    /// Bytes clocked back to the SoC, carried as frame payload.
    Data {
        #[serde(skip)]
        bytes: Vec<u8>,
    },
    /// I2C address not acknowledged — how firmware probes for absent chips, so
    /// this is a normal answer rather than an error.
    Nack,
}

impl Response {
    pub fn data(bytes: impl Into<Vec<u8>>) -> Self {
        Response::Data { bytes: bytes.into() }
    }

    pub fn payload(&self) -> &[u8] {
        match self {
            Response::Data { bytes } => bytes,
            _ => &[],
        }
    }
}

/// Something a peripheral raises on its own, without being asked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Assert or release an interrupt line back into the SoC. This is how a
    /// touch panel or keyboard tells firmware it has something to report.
    GpioIrq { pin: u8, level: bool },
    /// Bytes arriving on a UART, such as an NMEA sentence.
    UartRx {
        controller: u8,
        #[serde(skip)]
        data: Vec<u8>,
    },
    /// A display finished a frame and the UI should repaint. Pixels live in the
    /// device's own buffer rather than being copied through here.
    FrameReady { width: u16, height: u16 },
    /// Surfaced in the UI log. Custom drivers use this for their own reporting.
    Log { level: LogLevel, message: String },
    /// A traced bus transaction, emitted only while tracing is switched on.
    Trace(crate::trace::TraceRecord),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Where a peripheral pushes events and interrupts.
pub trait EventSink {
    fn emit(&mut self, event: Event);
}

impl EventSink for Vec<Event> {
    fn emit(&mut self, event: Event) {
        self.push(event);
    }
}

/// A device hanging off one of the SoC's buses.
///
/// In-tree drivers implement this directly; out-of-process drivers are adapted
/// to it by the registry, so the rest of the emulator cannot tell them apart.
pub trait Peripheral: Send {
    /// Stable identifier, matching the `kind` field in a board TOML.
    fn kind(&self) -> &str;

    /// Bus addresses this device answers for.
    fn claims(&self) -> Vec<Claim>;

    /// Handle one transaction. Returning [`Response::None`] for a transaction
    /// that expects data leaves the SoC reading zeroes, which is what an absent
    /// chip looks like.
    fn transact(&mut self, tx: &Transaction, events: &mut dyn EventSink) -> Response;

    /// Explain this device's own traffic for the bus tracer, e.g. turning
    /// `2a 00 00 01 3f` into `CASET x=0..319`.
    ///
    /// Only called while tracing is enabled, so it may be as expensive as it
    /// needs to be. Returning `None` leaves the tracer showing a hex dump.
    fn decode(&self, _tx: &Transaction, _response: &Response) -> Option<String> {
        None
    }

    /// Called periodically so devices can do their own timing: a GNSS emitting
    /// a sentence every second, a touch panel debouncing, an animation.
    ///
    /// `elapsed_us` is emulated time since the previous call.
    fn tick(&mut self, _elapsed_us: u64, _events: &mut dyn EventSink) {}

    /// RGBA8888 pixels, for devices that are displays. The UI polls this rather
    /// than having frames pushed through the event channel.
    fn framebuffer(&self) -> Option<Framebuffer<'_>> {
        None
    }
}

/// A borrowed view of a display's pixels.
#[derive(Debug, Clone, Copy)]
pub struct Framebuffer<'a> {
    pub width: u16,
    pub height: u16,
    /// Row-major RGBA8888, `width * height * 4` bytes.
    pub rgba: &'a [u8],
}
