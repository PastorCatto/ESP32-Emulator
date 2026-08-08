//! Turning a board file into running device models, and serving them.
//!
//! This is the seam a third party extends. A board file names a `kind` and
//! some parameters; [`build`] maps that name to a [`vpb::Peripheral`]. Adding
//! hardware means adding a model and one arm here -- nothing above this
//! module learns a new type.
//!
//! Devices that are named but not modelled are reported rather than dropped.
//! A board listing a GT911 the emulator has no driver for should say so, not
//! quietly behave like a board with no touchscreen.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use boards::{Board, PeripheralSpec};
use devices::gt911::TouchHandle;
use devices::keyboard::KeyQueue;
use devices::st7789::{Screen, ScreenHandle};
use vpb::registry::Registry;
use vpb::trace::TraceConfig;
use vpb::input::Rotation;
use vpb::{Claim, Event, Peripheral};

/// What the UI needs to know about the bus while it runs.
#[derive(Debug)]
pub struct Hardware {
    /// Port the emulator should be told to connect to.
    pub port: u16,
    /// The display, when the board has one this build can model.
    pub screen: Option<ScreenHandle>,
    /// The touch panel, for the UI to feed mouse events into.
    pub touch: Option<TouchHandle>,
    /// The keyboard's pending-key queue, for the UI to type into.
    pub keys: Option<KeyQueue>,
    /// Kinds that were attached, in board order.
    pub attached: Vec<String>,
    /// Kinds named by the board that this build has no model for.
    pub unmodelled: Vec<String>,
    /// Trace records and device events, drained by the UI each frame.
    pub events: Receiver<Event>,
    /// Whether the emulator is currently connected.
    connected: Arc<AtomicBool>,
    /// Cleared on drop to stop the server thread at the next accept.
    running: Arc<AtomicBool>,
    /// Bus tracing, read by the server thread before each transaction.
    trace: Arc<Mutex<Option<TraceConfig>>>,
}

impl Hardware {
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Change what the bus tracer reports. Takes effect at the next
    /// transaction, never partway through one.
    pub fn set_trace(&self, config: TraceConfig) {
        if let Ok(mut slot) = self.trace.lock() {
            *slot = Some(config);
        }
    }
}

impl Drop for Hardware {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// A model, plus the handles the UI needs to reach into it afterwards.
///
/// Taken at construction rather than recovered later: once the box is a
/// `dyn Peripheral` the concrete type is gone, and getting it back would mean
/// either a downcast -- which every third-party model would have to opt into
/// -- or an unsound cast keyed on the device's own name string.
struct Built {
    device: Box<dyn Peripheral>,
    screen: Option<ScreenHandle>,
    touch: Option<TouchHandle>,
    keys: Option<KeyQueue>,
}

impl Built {
    fn new(device: Box<dyn Peripheral>) -> Self {
        Built { device, screen: None, touch: None, keys: None }
    }

    fn screen(mut self, handle: ScreenHandle) -> Self {
        self.screen = Some(handle);
        self
    }

    fn touch(mut self, handle: TouchHandle) -> Self {
        self.touch = Some(handle);
        self
    }

    fn keys(mut self, handle: KeyQueue) -> Self {
        self.keys = Some(handle);
        self
    }
}

/// Build one device model from a board entry.
///
/// `Ok(None)` means "no model for this kind in this build", which is a normal
/// answer and not a failure -- features are switches, and a board file
/// describes hardware rather than what happens to be compiled in.
fn build(
    spec: &PeripheralSpec,
    claim: Option<Claim>,
    sd_image: Option<&Path>,
) -> Result<Option<Built>, String> {
    match spec.kind.as_str() {
        "st7789" => {
            let claim = claim.ok_or("st7789 needs a bus and a chip select")?;
            let width = spec.params.u16_or("width", 320).map_err(|e| e.to_string())?;
            let height = spec.params.u16_or("height", 240).map_err(|e| e.to_string())?;
            // `invert` describes the glass, not the controller's register.
            let inverts = spec.params.bool_or("invert", false).map_err(|e| e.to_string())?;
            let screen = Screen::handle(width, height, inverts);
            let panel = devices::St7789::new(claim, screen.clone());
            Ok(Some(Built::new(Box::new(panel)).screen(screen)))
        }
        "gt911" => {
            let claim = claim.ok_or("gt911 needs a bus and an address")?;
            // The resolution the chip advertises, which drivers read...
            let width = spec.params.u16_or("width", 320).map_err(|e| e.to_string())?;
            let height = spec.params.u16_or("height", 240).map_err(|e| e.to_string())?;
            // ...and the range its point registers actually span, which is a
            // different thing on a panel mounted sideways. Defaulting to the
            // advertised figures keeps an unmeasured board behaving sanely.
            let point_width = spec.params.u16_or("point_width", width).map_err(|e| e.to_string())?;
            let point_height =
                spec.params.u16_or("point_height", height).map_err(|e| e.to_string())?;
            let rotation = Rotation::from_degrees(
                spec.params.u16_or("rotation", 0).map_err(|e| e.to_string())?,
            );
            let geometry = devices::gt911::Geometry::new(width, height)
                .points(point_width, point_height)
                .rotated(rotation);
            let panel = devices::Gt911::new(claim, geometry);
            let touch = panel.touch().clone();
            Ok(Some(Built::new(Box::new(panel)).touch(touch)))
        }
        "tdeck-keyboard" => {
            let claim = claim.ok_or("tdeck-keyboard needs a bus and an address")?;
            let kb = devices::TdeckKeyboard::new(claim);
            let keys = kb.keys().clone();
            Ok(Some(Built::new(Box::new(kb)).keys(keys)))
        }
        "sdcard" => {
            let claim = claim.ok_or("sdcard needs a bus and a chip select")?;
            // A card with no image behind it is an empty slot, which is a
            // legitimate way to run and not something to complain about.
            let Some(path) = sd_image else { return Ok(None) };
            let card = devices::SdCard::open(path, claim).map_err(|e| e.to_string())?;
            Ok(Some(Built::new(Box::new(card))))
        }
        _ => Ok(None),
    }
}

/// Attach a board's devices and start serving them on a background thread.
///
/// Binds port 0 and reports what the OS gave back, so two emulator windows do
/// not fight over a fixed port.
pub fn start(board: &Board, sd_image: Option<PathBuf>) -> std::io::Result<Hardware> {
    let mut registry = Registry::new();
    let mut screen = None;
    let mut touch = None;
    let mut keys = None;
    let mut attached = Vec::new();
    let mut unmodelled = Vec::new();

    for spec in board.peripherals.iter().filter(|p| p.enabled) {
        let claim = spec.claim().ok().flatten();
        match build(spec, claim, sd_image.as_deref()) {
            Ok(Some(built)) => {
                screen = built.screen.or(screen);
                touch = built.touch.or(touch);
                keys = built.keys.or(keys);
                match registry.register(built.device) {
                    Ok(_) => attached.push(spec.kind.clone()),
                    // A refused claim means two devices want the same address,
                    // which is a board file bug worth surfacing.
                    Err(e) => unmodelled.push(format!("{}: {e}", spec.kind)),
                }
            }
            Ok(None) => unmodelled.push(spec.kind.clone()),
            Err(e) => unmodelled.push(format!("{}: {e}", spec.kind)),
        }
    }

    // Tracing is off until the UI turns it on; the toggle is per bus.
    registry.set_trace(TraceConfig::default());

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();

    let (tx, events) = std::sync::mpsc::channel();
    let connected = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicBool::new(true));
    let trace = Arc::new(Mutex::new(None));

    let thread = Arc::new(Threaded {
        connections: AtomicUsize::new(0),
        connected: connected.clone(),
        running: running.clone(),
        trace: trace.clone(),
    });
    let shared = Arc::new(Mutex::new(registry));
    std::thread::Builder::new()
        .name("vpb".into())
        .spawn(move || serve_until_stopped(&listener, &shared, &tx, &thread))?;

    Ok(Hardware {
        port,
        screen,
        touch,
        keys,
        attached,
        unmodelled,
        events,
        connected,
        running,
        trace,
    })
}


/// The half of [`Hardware`] the server thread owns a copy of.
struct Threaded {
    /// How many controllers are connected right now.
    connections: AtomicUsize,
    connected: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    trace: Arc<Mutex<Option<TraceConfig>>>,
}

/// Accept and serve, one thread per connection.
///
/// The emulator opens a connection per controller -- SPI2, SPI3, I2C0, I2C1 --
/// and holds each for the life of the machine, so serving them in turn would
/// leave every bus after the first permanently waiting to be accepted.
fn serve_until_stopped(
    listener: &TcpListener,
    registry: &Arc<Mutex<Registry>>,
    tx: &Sender<Event>,
    shared: &Arc<Threaded>,
) {
    for stream in listener.incoming() {
        if !shared.running.load(Ordering::Relaxed) {
            return;
        }
        let Ok(stream) = stream else { continue };

        let registry = Arc::clone(registry);
        let shared = Arc::clone(shared);
        let tx = tx.clone();

        // Counted rather than a flag: with several connections, the last one
        // to close is what "disconnected" means.
        shared.connections.fetch_add(1, Ordering::Relaxed);
        shared.connected.store(true, Ordering::Relaxed);

        let spawned = std::thread::Builder::new()
            .name("vpb-conn".into())
            .spawn(move || {
                // A send failure means the UI is gone; nobody to tell.
                let _ = vpb::server::serve_with(
                    stream,
                    &registry,
                    &mut |event| {
                        let _ = tx.send(event);
                    },
                    &mut |registry| {
                        // Taken, not read: reapplying the same config on every
                        // one of thousands of transactions would be waste.
                        let pending = shared.trace.lock().ok().and_then(|mut s| s.take());
                        if let Some(config) = pending {
                            registry.set_trace(config);
                        }
                    },
                );
                if shared.connections.fetch_sub(1, Ordering::Relaxed) == 1 {
                    shared.connected.store(false, Ordering::Relaxed);
                }
            });
        if spawned.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T_DECK: &str = include_str!("../../../boards/t-deck-plus.toml");

    fn board() -> Board {
        Board::from_toml(T_DECK).expect("t-deck board should parse")
    }

    #[test]
    fn attaches_the_display_and_reports_what_it_cannot_model() {
        let hw = start(&board(), None).expect("server should start");

        assert!(hw.attached.contains(&"st7789".to_string()));
        assert!(hw.screen.is_some(), "the display must expose its frame");
        // Named by the board, no model in this build. Reported, not dropped:
        // a board listing a radio should not behave like a board without one.
        assert!(hw.unmodelled.iter().any(|k| k.starts_with("sx1262")));
        assert!(hw.port != 0, "the OS should have assigned a real port");
    }

    #[test]
    fn the_i2c_devices_expose_their_input_handles() {
        // Both are useless without these: the UI has no other way to say a
        // key was pressed or the panel was touched.
        let hw = start(&board(), None).expect("server should start");
        assert!(hw.attached.contains(&"gt911".to_string()));
        assert!(hw.attached.contains(&"tdeck-keyboard".to_string()));
        assert!(hw.touch.is_some());
        assert!(hw.keys.is_some());
    }

    #[test]
    fn a_card_with_no_image_is_an_empty_slot_not_an_error() {
        let hw = start(&board(), None).expect("server should start");
        assert!(!hw.attached.contains(&"sdcard".to_string()));
        assert!(hw.unmodelled.contains(&"sdcard".to_string()));
    }

    #[test]
    fn the_display_is_sized_and_inverted_from_the_board_file() {
        // The T-Deck mounts a 240x320 panel sideways and wires the glass
        // inverted; both come from the file, not from the model's defaults.
        let hw = start(&board(), None).expect("server should start");
        let screen = hw.screen.as_ref().expect("display");
        let s = screen.lock().expect("screen");
        assert_eq!((s.width, s.height), (320, 240));
        assert!(s.panel_inverts);
    }
}
