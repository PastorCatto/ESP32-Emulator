//! Routing from bus transactions to whichever peripheral claimed the address.

use crate::trace::{TraceConfig, TraceRecord};
use crate::{Claim, Event, EventSink, Peripheral, Response, Transaction};

/// Why a device could not be registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// Two devices claimed the same bus address. Refused rather than resolved,
    /// because silently shadowing a device produces bugs that look like
    /// firmware faults.
    Conflict {
        claim: Claim,
        existing: String,
        incoming: String,
    },
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterError::Conflict { claim, existing, incoming } => write!(
                f,
                "{incoming:?} claims {claim:?}, already owned by {existing:?}"
            ),
        }
    }
}

impl std::error::Error for RegisterError {}

/// Everything attached to the emulated buses.
#[derive(Default)]
pub struct Registry {
    devices: Vec<Entry>,
    trace: TraceConfig,
}

struct Entry {
    peripheral: Box<dyn Peripheral>,
    claims: Vec<Claim>,
    enabled: bool,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field(
                "devices",
                &self
                    .devices
                    .iter()
                    .map(|e| (e.peripheral.kind(), e.enabled, e.claims.len()))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    /// Attach a device, refusing any address another device already owns.
    pub fn register(&mut self, peripheral: Box<dyn Peripheral>) -> Result<usize, RegisterError> {
        let claims = peripheral.claims();
        for claim in &claims {
            if let Some(existing) = self.owner_of(claim) {
                return Err(RegisterError::Conflict {
                    claim: claim.clone(),
                    existing: existing.to_string(),
                    incoming: peripheral.kind().to_string(),
                });
            }
        }
        self.devices.push(Entry {
            peripheral,
            claims,
            enabled: true,
        });
        Ok(self.devices.len() - 1)
    }

    fn owner_of(&self, claim: &Claim) -> Option<&str> {
        self.devices
            .iter()
            .find(|e| e.claims.iter().any(|c| c.matches(claim)))
            .map(|e| e.peripheral.kind())
    }

    /// Turn a device off without detaching it. The bus then behaves as though
    /// the chip were absent, which is the switch-like behaviour we want for
    /// isolating whether a peripheral is causing a firmware problem.
    pub fn set_enabled(&mut self, index: usize, enabled: bool) {
        if let Some(e) = self.devices.get_mut(index) {
            e.enabled = enabled;
        }
    }

    pub fn is_enabled(&self, index: usize) -> bool {
        self.devices.get(index).is_some_and(|e| e.enabled)
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.devices.iter().map(|e| e.peripheral.kind())
    }

    pub fn get(&self, index: usize) -> Option<&dyn Peripheral> {
        self.devices.get(index).map(|e| &*e.peripheral)
    }

    /// Route one transaction.
    ///
    /// A reset is broadcast; everything else goes to the single claiming
    /// device, or nowhere. Traffic to an unclaimed address returns
    /// [`Response::None`], which is indistinguishable from absent hardware —
    /// exactly what firmware probing a bus should see.
    pub fn dispatch(&mut self, tx: &Transaction, events: &mut dyn EventSink) -> Response {
        if matches!(tx, Transaction::Reset) {
            for entry in &mut self.devices {
                entry.peripheral.transact(tx, events);
            }
            return Response::None;
        }

        let target = tx.claim();
        let hit = self
            .devices
            .iter()
            .position(|e| e.enabled && e.claims.iter().any(|c| c.matches(&target)));

        // Only build trace state when tracing is actually on, so the disabled
        // path stays a plain dispatch.
        let traced = self.trace.wants(tx);

        let (response, device, decoded) = match hit {
            Some(i) => {
                let entry = &mut self.devices[i];
                let response = entry.peripheral.transact(tx, events);
                let device = traced.then(|| entry.peripheral.kind().to_string());
                let decoded = (traced && self.trace.decode)
                    .then(|| entry.peripheral.decode(tx, &response))
                    .flatten();
                (response, device, decoded)
            }
            None => {
                // An unanswered I2C address must NACK, not return zeroes, or
                // firmware probing the bus will believe every address holds a
                // device.
                let response = if matches!(
                    tx,
                    Transaction::I2cWrite { .. } | Transaction::I2cRead { .. }
                ) {
                    Response::Nack
                } else {
                    Response::None
                };
                (response, None, None)
            }
        };

        if traced {
            events.emit(Event::Trace(TraceRecord::build(
                tx,
                &response,
                device.as_deref(),
                decoded,
                &self.trace,
            )));
        }
        response
    }

    /// Switch bus tracing on or off. Each bus is an independent toggle.
    pub fn set_trace(&mut self, trace: TraceConfig) {
        self.trace = trace;
    }

    pub fn trace_config(&self) -> TraceConfig {
        self.trace
    }

    /// Advance every enabled device's own sense of time.
    pub fn tick(&mut self, elapsed_us: u64, events: &mut dyn EventSink) {
        for entry in &mut self.devices {
            if entry.enabled {
                entry.peripheral.tick(elapsed_us, events);
            }
        }
    }

    /// The first enabled device offering pixels, which the UI draws as the screen.
    pub fn primary_display(&self) -> Option<usize> {
        self.devices
            .iter()
            .position(|e| e.enabled && e.peripheral.framebuffer().is_some())
    }
}

/// Collects events while also counting them, so the UI can tell whether a tick
/// produced anything worth repainting for.
#[derive(Debug, Default)]
pub struct EventQueue {
    pub events: Vec<Event>,
}

impl EventSink for EventQueue {
    fn emit(&mut self, event: Event) {
        self.events.push(event);
    }
}

impl EventQueue {
    pub fn drain(&mut self) -> std::vec::Drain<'_, Event> {
        self.events.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Framebuffer, LogLevel};

    struct Fake {
        kind: &'static str,
        claims: Vec<Claim>,
        seen: usize,
    }

    impl Peripheral for Fake {
        fn kind(&self) -> &str {
            self.kind
        }
        fn claims(&self) -> Vec<Claim> {
            self.claims.clone()
        }
        fn transact(&mut self, _tx: &Transaction, events: &mut dyn EventSink) -> Response {
            self.seen += 1;
            events.emit(Event::Log {
                level: LogLevel::Trace,
                message: self.kind.to_string(),
            });
            Response::data(vec![0xa5])
        }
        fn framebuffer(&self) -> Option<Framebuffer<'_>> {
            None
        }
    }

    fn fake(kind: &'static str, claims: Vec<Claim>) -> Box<dyn Peripheral> {
        Box::new(Fake { kind, claims, seen: 0 })
    }

    #[test]
    fn routes_to_the_claiming_device() {
        let mut reg = Registry::new();
        reg.register(fake("display", vec![Claim::Spi { controller: 2, cs: 12 }])).unwrap();
        reg.register(fake("sdcard", vec![Claim::Spi { controller: 2, cs: 39 }])).unwrap();

        let mut ev = EventQueue::default();
        let tx = Transaction::SpiTransfer {
            controller: 2,
            cs: 39,
            dc: None,
            mosi: vec![0x40],
            read_len: 1,
        };
        assert_eq!(reg.dispatch(&tx, &mut ev), Response::data(vec![0xa5]));
        assert_eq!(
            ev.events,
            vec![Event::Log { level: LogLevel::Trace, message: "sdcard".into() }]
        );
    }

    #[test]
    fn refuses_two_devices_on_one_chip_select() {
        let mut reg = Registry::new();
        reg.register(fake("display", vec![Claim::Spi { controller: 2, cs: 12 }])).unwrap();
        let err = reg
            .register(fake("other", vec![Claim::Spi { controller: 2, cs: 12 }]))
            .unwrap_err();
        assert!(matches!(err, RegisterError::Conflict { .. }));
    }

    #[test]
    fn unclaimed_i2c_address_nacks() {
        let mut reg = Registry::new();
        reg.register(fake("touch", vec![Claim::I2c { controller: 0, address: 0x5d, alt: None }]))
            .unwrap();
        let mut ev = EventQueue::default();
        let probe = Transaction::I2cRead { controller: 0, address: 0x33, len: 1 };
        assert_eq!(reg.dispatch(&probe, &mut ev), Response::Nack);
    }

    #[test]
    fn alternate_i2c_address_is_answered() {
        let mut reg = Registry::new();
        reg.register(fake(
            "gt911",
            vec![Claim::I2c { controller: 0, address: 0x5d, alt: Some(0x14) }],
        ))
        .unwrap();
        let mut ev = EventQueue::default();
        let tx = Transaction::I2cRead { controller: 0, address: 0x14, len: 1 };
        assert_eq!(reg.dispatch(&tx, &mut ev), Response::data(vec![0xa5]));
    }

    #[test]
    fn disabled_device_looks_absent() {
        let mut reg = Registry::new();
        let idx = reg
            .register(fake("touch", vec![Claim::I2c { controller: 0, address: 0x5d, alt: None }]))
            .unwrap();
        reg.set_enabled(idx, false);

        let mut ev = EventQueue::default();
        let tx = Transaction::I2cRead { controller: 0, address: 0x5d, len: 1 };
        assert_eq!(reg.dispatch(&tx, &mut ev), Response::Nack);
        assert!(ev.events.is_empty());
    }

    #[test]
    fn tracing_off_emits_no_trace_records() {
        let mut reg = Registry::new();
        reg.register(fake("touch", vec![Claim::I2c { controller: 0, address: 0x5d, alt: None }]))
            .unwrap();
        let mut ev = EventQueue::default();
        reg.dispatch(&Transaction::I2cRead { controller: 0, address: 0x5d, len: 1 }, &mut ev);
        assert!(!ev.events.iter().any(|e| matches!(e, Event::Trace(_))));
    }

    #[test]
    fn i2c_tracer_records_every_call_on_the_bus() {
        let mut reg = Registry::new();
        reg.register(fake("gt911", vec![Claim::I2c { controller: 0, address: 0x5d, alt: None }]))
            .unwrap();
        reg.register(fake("keyboard", vec![Claim::I2c { controller: 0, address: 0x55, alt: None }]))
            .unwrap();
        reg.set_trace(crate::trace::TraceConfig::i2c_only());

        let mut ev = EventQueue::default();
        reg.dispatch(&Transaction::I2cRead { controller: 0, address: 0x5d, len: 1 }, &mut ev);
        reg.dispatch(&Transaction::I2cRead { controller: 0, address: 0x55, len: 1 }, &mut ev);
        // An address with nothing on it must still be traced; that is exactly
        // the case worth seeing when a board config has the wrong pin.
        reg.dispatch(&Transaction::I2cRead { controller: 0, address: 0x77, len: 1 }, &mut ev);

        let traces: Vec<String> = ev
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Trace(r) => Some(r.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            traces,
            vec![
                "I²C0 ← 0x5d gt911  r[a5]",
                "I²C0 ← 0x55 keyboard  r[a5]",
                "I²C0 ← 0x77 <unclaimed>",
            ]
        );
    }

    #[test]
    fn i2c_tracer_leaves_the_spi_firehose_alone() {
        let mut reg = Registry::new();
        reg.register(fake("st7789", vec![Claim::Spi { controller: 2, cs: 12 }])).unwrap();
        reg.set_trace(crate::trace::TraceConfig::i2c_only());

        let mut ev = EventQueue::default();
        reg.dispatch(
            &Transaction::SpiTransfer {
                controller: 2,
                cs: 12,
                dc: Some(true),
                mosi: vec![0; 150 * 1024],
                read_len: 0,
            },
            &mut ev,
        );
        assert!(!ev.events.iter().any(|e| matches!(e, Event::Trace(_))));
    }

    #[test]
    fn reset_reaches_every_device() {
        let mut reg = Registry::new();
        reg.register(fake("a", vec![Claim::Spi { controller: 2, cs: 1 }])).unwrap();
        reg.register(fake("b", vec![Claim::Spi { controller: 2, cs: 2 }])).unwrap();
        let mut ev = EventQueue::default();
        reg.dispatch(&Transaction::Reset, &mut ev);
        assert_eq!(ev.events.len(), 2);
    }

    #[test]
    fn write_only_spi_needs_no_reply() {
        let tx = Transaction::SpiTransfer {
            controller: 2,
            cs: 12,
            dc: Some(true),
            mosi: vec![0; 4096],
            read_len: 0,
        };
        assert!(!tx.expects_reply());
    }
}
