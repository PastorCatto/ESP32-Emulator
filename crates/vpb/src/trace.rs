//! Bus tracing.
//!
//! Every transaction is routed through [`crate::registry::Registry::dispatch`],
//! so a single interception point there sees all traffic on every bus. Tracing
//! is off by default and costs nothing when disabled: we build no records at
//! all rather than building them and discarding them.
//!
//! Devices can decode their own traffic via [`crate::Peripheral::decode`], which
//! turns a hex dump into something like `CASET x=0..319` — the difference
//! between confirming bytes moved and confirming the *right* bytes moved.

use crate::{Response, Transaction};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Which buses to trace. Each is an independent switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceConfig {
    pub i2c: bool,
    pub spi: bool,
    pub uart: bool,
    pub gpio: bool,
    /// Cap on bytes recorded per transaction. A full-frame SPI write is 150 KiB
    /// and logging it whole would drown the log and stall the UI.
    pub max_bytes: usize,
    /// Ask devices to decode their own traffic.
    pub decode: bool,
}

impl Default for TraceConfig {
    fn default() -> Self {
        TraceConfig {
            i2c: false,
            spi: false,
            uart: false,
            gpio: false,
            max_bytes: 64,
            decode: true,
        }
    }
}

impl TraceConfig {
    /// Nothing traced.
    pub fn off() -> Self {
        TraceConfig::default()
    }

    /// The common case: watch the I2C bus, leave the display firehose alone.
    pub fn i2c_only() -> Self {
        TraceConfig { i2c: true, ..TraceConfig::default() }
    }

    pub fn all() -> Self {
        TraceConfig {
            i2c: true,
            spi: true,
            uart: true,
            gpio: true,
            ..TraceConfig::default()
        }
    }

    pub fn any(&self) -> bool {
        self.i2c || self.spi || self.uart || self.gpio
    }

    /// Should this transaction be recorded?
    pub fn wants(&self, tx: &Transaction) -> bool {
        match tx {
            Transaction::I2cWrite { .. } | Transaction::I2cRead { .. } => self.i2c,
            Transaction::SpiTransfer { .. } => self.spi,
            Transaction::UartTx { .. } => self.uart,
            Transaction::GpioWrite { .. } => self.gpio,
            Transaction::Reset => self.any(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceBus {
    I2c,
    Spi,
    Uart,
    Gpio,
    System,
}

impl TraceBus {
    /// Short glyph for the UI's bus column.
    pub fn icon(self) -> &'static str {
        match self {
            TraceBus::I2c => "I²C",
            TraceBus::Spi => "SPI",
            TraceBus::Uart => "TTY",
            TraceBus::Gpio => "IO",
            TraceBus::System => "SYS",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// SoC to device.
    Write,
    /// Device to SoC.
    Read,
    /// SPI clocks both ways at once.
    Duplex,
}

impl Direction {
    /// Arrow for the UI, read as "SoC → device".
    pub fn arrow(self) -> &'static str {
        match self {
            Direction::Write => "→",
            Direction::Read => "←",
            Direction::Duplex => "⇄",
        }
    }
}

/// One recorded bus transaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceRecord {
    pub bus: TraceBus,
    pub controller: u8,
    pub direction: Direction,
    /// Address on the bus: an I2C address, a SPI chip select, a GPIO number.
    pub target: String,
    /// Device that answered, or `None` if nothing claimed the address.
    pub device: Option<String>,
    /// Bytes the SoC sent, truncated to `max_bytes`.
    pub wrote: Vec<u8>,
    /// Bytes the device returned, truncated to `max_bytes`.
    pub read: Vec<u8>,
    /// Bytes dropped by truncation, so the log can say "+1234 more".
    pub elided: usize,
    /// Device-supplied interpretation, when it offers one.
    pub decoded: Option<String>,
    /// Address went unanswered. On I2C this is normal probing; everywhere else
    /// it usually means a board config with the wrong pin.
    pub unclaimed: bool,
}

impl TraceRecord {
    /// Build a record from a transaction and the response it produced.
    pub fn build(
        tx: &Transaction,
        response: &Response,
        device: Option<&str>,
        decoded: Option<String>,
        cfg: &TraceConfig,
    ) -> Self {
        let (bus, controller, direction, target) = match tx {
            Transaction::I2cWrite { controller, address, .. } => {
                (TraceBus::I2c, *controller, Direction::Write, format!("{address:#04x}"))
            }
            Transaction::I2cRead { controller, address, .. } => {
                (TraceBus::I2c, *controller, Direction::Read, format!("{address:#04x}"))
            }
            Transaction::SpiTransfer { controller, cs, read_len, .. } => (
                TraceBus::Spi,
                *controller,
                if *read_len > 0 { Direction::Duplex } else { Direction::Write },
                format!("cs{cs}"),
            ),
            Transaction::UartTx { controller, .. } => {
                (TraceBus::Uart, *controller, Direction::Write, format!("uart{controller}"))
            }
            Transaction::GpioWrite { pin, level } => (
                TraceBus::Gpio,
                0,
                Direction::Write,
                format!("gpio{pin}={}", u8::from(*level)),
            ),
            Transaction::Reset => (TraceBus::System, 0, Direction::Write, "reset".to_string()),
        };

        let written = tx.payload();
        let readback = response.payload();
        let cap = cfg.max_bytes;
        let kept_write = written.len().min(cap);
        let kept_read = readback.len().min(cap);

        TraceRecord {
            bus,
            controller,
            direction,
            target,
            device: device.map(str::to_owned),
            wrote: written[..kept_write].to_vec(),
            read: readback[..kept_read].to_vec(),
            // Each direction is capped independently, so count what each one
            // actually dropped rather than assuming both were truncated.
            elided: (written.len() - kept_write) + (readback.len() - kept_read),
            decoded,
            unclaimed: device.is_none(),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02x}"));
    }
    s
}

impl fmt::Display for TraceRecord {
    /// One log line: `I²C0 → 0x5d gt911  81 4e  · read touch: 1 point`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{} {} {}",
            self.bus.icon(),
            self.controller,
            self.direction.arrow(),
            self.target
        )?;
        if let Some(dev) = &self.device {
            write!(f, " {dev}")?;
        } else if self.bus != TraceBus::System {
            write!(f, " <unclaimed>")?;
        }
        if !self.wrote.is_empty() {
            write!(f, "  w[{}]", hex(&self.wrote))?;
        }
        if !self.read.is_empty() {
            write!(f, "  r[{}]", hex(&self.read))?;
        }
        if self.elided > 0 {
            write!(f, " +{} more", self.elided)?;
        }
        if let Some(d) = &self.decoded {
            write!(f, "  · {d}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_switches_select_only_their_bus() {
        let cfg = TraceConfig::i2c_only();
        assert!(cfg.wants(&Transaction::I2cRead { controller: 0, address: 0x5d, len: 1 }));
        assert!(!cfg.wants(&Transaction::SpiTransfer {
            controller: 2,
            cs: 12,
            dc: None,
            mosi: vec![],
            read_len: 0,
        }));
        assert!(!TraceConfig::off().any());
        assert!(TraceConfig::all().any());
    }

    #[test]
    fn formats_an_i2c_read_with_decode() {
        let tx = Transaction::I2cRead { controller: 0, address: 0x5d, len: 2 };
        let resp = Response::data(vec![0x81, 0x4e]);
        let rec = TraceRecord::build(
            &tx,
            &resp,
            Some("gt911"),
            Some("1 touch point".into()),
            &TraceConfig::i2c_only(),
        );
        assert_eq!(rec.to_string(), "I²C0 ← 0x5d gt911  r[81 4e]  · 1 touch point");
    }

    #[test]
    fn marks_unanswered_addresses() {
        let tx = Transaction::I2cWrite {
            controller: 0,
            address: 0x33,
            data: vec![0x01],
            stop: true,
        };
        let rec = TraceRecord::build(&tx, &Response::Nack, None, None, &TraceConfig::i2c_only());
        assert!(rec.unclaimed);
        assert_eq!(rec.to_string(), "I²C0 → 0x33 <unclaimed>  w[01]");
    }

    #[test]
    fn large_transfers_are_truncated_not_logged_whole() {
        let tx = Transaction::SpiTransfer {
            controller: 2,
            cs: 12,
            dc: Some(true),
            mosi: vec![0xaa; 150 * 1024],
            read_len: 0,
        };
        let cfg = TraceConfig { spi: true, max_bytes: 8, ..TraceConfig::default() };
        let rec = TraceRecord::build(&tx, &Response::None, Some("st7789"), None, &cfg);
        assert_eq!(rec.wrote.len(), 8);
        assert!(rec.elided > 150_000);
        assert!(rec.to_string().contains("+153592 more"));
    }

    #[test]
    fn gpio_writes_show_their_level() {
        let tx = Transaction::GpioWrite { pin: 42, level: true };
        let cfg = TraceConfig { gpio: true, ..TraceConfig::default() };
        let rec = TraceRecord::build(&tx, &Response::None, Some("backlight"), None, &cfg);
        assert_eq!(rec.to_string(), "IO0 → gpio42=1 backlight");
    }
}
