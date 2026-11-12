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
    /// The game pad's buttons and LEDs, for the window to drive and show.
    pub pad: Option<devices::PadHandle>,
    /// The motion sensor's state, for something to tilt.
    pub imu: Option<devices::ImuHandle>,
    /// UDP ports joining a wired NIC to QEMU's network, as
    /// `(qemu_local, ours)`, when the board has one this build can model.
    pub net: Option<(u16, u16)>,
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
    /// The device registry, shared with the server thread, so a module can be
    /// switched on and off while the guest runs.
    registry: Arc<Mutex<Registry>>,
    /// What is in each slot now, as an option number.
    chosen: Mutex<Vec<Option<usize>>>,
    /// The slots this board has, with the registry index of each option.
    slots: Vec<SlotLive>,
}

/// A place on the board where one of several modules can be plugged in, as
/// the board file describes it: devices that share a `slot` name.
///
/// Every option is built and registered, and at most one is switched on. A
/// switched-off device behaves as though the chip were absent (an I2C probe is
/// not acknowledged), which is how real hardware looks when a module is not
/// plugged in; firmware notices only when it next probes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotDef {
    pub name: String,
    /// `(kind, label)` for each option, in board order.
    pub options: Vec<(String, String)>,
    /// The option that is plugged in at start, if any.
    pub default: Option<usize>,
}

#[derive(Debug)]
struct SlotLive {
    /// Registry index of each option, aligned with [`SlotDef::options`].
    indices: Vec<usize>,
}

/// The slots a board has. Devices with a `slot = "name"` key belong to the
/// slot of that name; `slot_default = true` plugs one in at start.
pub fn slot_defs(board: &Board) -> Vec<SlotDef> {
    let mut slots: Vec<SlotDef> = Vec::new();
    for spec in board.peripherals.iter().filter(|p| p.enabled) {
        let name = spec.params.string_or("slot", "").unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let label = spec.label.clone().unwrap_or_else(|| spec.kind.clone());
        let is_default = spec.params.bool_or("slot_default", false).unwrap_or(false);
        let at = match slots.iter().position(|s| s.name == name) {
            Some(at) => at,
            None => {
                slots.push(SlotDef { name, options: Vec::new(), default: None });
                slots.len() - 1
            }
        };
        slots[at].options.push((spec.kind.clone(), label));
        if is_default && slots[at].default.is_none() {
            slots[at].default = Some(slots[at].options.len() - 1);
        }
    }
    slots
}

impl Hardware {
    /// Plug option `option` into slot `slot`, or empty the slot with `None`,
    /// while the guest runs. Things the old module left behind (pressed
    /// buttons, typed keys nobody read) are dropped.
    pub fn select_module(&self, slot: usize, option: Option<usize>) {
        let Some(live) = self.slots.get(slot) else { return };
        if let Ok(mut registry) = self.registry.lock() {
            for (i, &index) in live.indices.iter().enumerate() {
                registry.set_enabled(index, Some(i) == option);
            }
        }
        if let Ok(mut chosen) = self.chosen.lock() {
            if let Some(c) = chosen.get_mut(slot) {
                *c = option;
            }
        }
        if let Some(pad) = &self.pad {
            pad.release_all();
        }
        if let Some(keys) = &self.keys {
            if let Ok(mut q) = keys.lock() {
                q.clear();
            }
        }
    }

    /// What is in slot `slot` now.
    pub fn selected_module(&self, slot: usize) -> Option<usize> {
        self.chosen.lock().ok().and_then(|c| c.get(slot).copied().flatten())
    }

    /// Whether a device of this kind is on the bus and switched on.
    pub fn is_active(&self, kind: &str) -> bool {
        let Ok(registry) = self.registry.lock() else { return false };
        (0..registry.len()).any(|i| {
            registry.is_enabled(i) && registry.get(i).is_some_and(|d| d.kind() == kind)
        })
    }

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
    pad: Option<devices::PadHandle>,
    imu: Option<devices::ImuHandle>,
    net: Option<(u16, u16)>,
}

impl Built {
    fn new(device: Box<dyn Peripheral>) -> Self {
        Built { device, screen: None, touch: None, keys: None, pad: None, imu: None, net: None }
    }

    fn pad(mut self, handle: devices::PadHandle) -> Self {
        self.pad = Some(handle);
        self
    }

    fn imu(mut self, handle: devices::ImuHandle) -> Self {
        self.imu = Some(handle);
        self
    }

    fn net(mut self, ports: (u16, u16)) -> Self {
        self.net = Some(ports);
        self
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
    claims: Vec<Claim>,
    sd_image: Option<&Path>,
) -> Result<Option<Built>, String> {
    // Single-address devices take the first; an I2C device takes the lot,
    // because firmware picks which controller drives its pins.
    let claim = claims.first().cloned();
    match spec.kind.as_str() {
        "st7789" => {
            let claim = claim.ok_or("st7789 needs a bus and a chip select")?;
            let width = spec.params.u16_or("width", 320).map_err(|e| e.to_string())?;
            let height = spec.params.u16_or("height", 240).map_err(|e| e.to_string())?;
            // `invert` describes the glass, not the controller's register.
            let inverts = spec.params.bool_or("invert", false).map_err(|e| e.to_string())?;
            // Likewise `color_order` is how the glass is wired, not what the
            // driver writes to MADCTL: a BGR-wired panel with the BGR bit set
            // is the correct setup, and shows red as red.
            let glass_bgr = match spec
                .params
                .string_or("color_order", "rgb")
                .map_err(|e| e.to_string())?
                .as_str()
            {
                "rgb" => false,
                "bgr" => true,
                other => {
                    return Err(format!(
                        "st7789 color_order must be \"rgb\" or \"bgr\", not \"{other}\""
                    ))
                }
            };
            let screen = Screen::handle_for_glass(width, height, inverts, glass_bgr);
            let panel = devices::St7789::new(claim, screen.clone());
            Ok(Some(Built::new(Box::new(panel)).screen(screen)))
        }
        "gt911" => {
            if claims.is_empty() {
                return Err("gt911 needs a bus and an address".into());
            }
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
            let panel = devices::Gt911::new(claims, geometry);
            let touch = panel.touch().clone();
            Ok(Some(Built::new(Box::new(panel)).touch(touch)))
        }
        "tdeck-keyboard" => {
            if claims.is_empty() {
                return Err("tdeck-keyboard needs a bus and an address".into());
            }
            let kb = devices::TdeckKeyboard::new(claims);
            let keys = kb.keys().clone();
            Ok(Some(Built::new(Box::new(kb)).keys(keys)))
        }
        "gamepad" => {
            if claims.is_empty() {
                return Err("gamepad needs a bus and an address".into());
            }
            let chip = devices::Mcp23017::new(claims, devices::Wiring::Pad);
            let pad = chip.pad().clone();
            Ok(Some(Built::new(Box::new(chip)).pad(pad)))
        }
        "icm42670" => {
            if claims.is_empty() {
                return Err("icm42670 needs a bus and an address".into());
            }
            let chip = devices::Icm42670::new(claims);
            let imu = chip.state().clone();
            Ok(Some(Built::new(Box::new(chip)).imu(imu)))
        }
        "sdcard" => {
            let claim = claim.ok_or("sdcard needs a bus and a chip select")?;
            // A card with no image behind it is an empty slot, which is a
            // legitimate way to run and not something to complain about.
            let Some(path) = sd_image else { return Ok(None) };
            let card = devices::SdCard::open(path, claim).map_err(|e| e.to_string())?;
            Ok(Some(Built::new(Box::new(card))))
        }
        "vm-info" => {
            if claims.is_empty() {
                return Err("vm-info needs a bus and an address".into());
            }
            // The Ethernet controller's own bus, when the board gives it one.
            let nic = if spec.params.contains("nic_host") {
                let get = |k: &str| spec.params.u8(k).map_err(|e| e.to_string());
                Some(devices::vminfo::NicBus {
                    host: get("nic_host")?,
                    sclk: get("nic_sclk")?,
                    mosi: get("nic_mosi")?,
                    miso: get("nic_miso")?,
                    cs: get("nic_cs")?,
                })
            } else {
                None
            };
            Ok(Some(Built::new(Box::new(devices::VmInfo::new(claims, nic)))))
        }
        "w5500" => {
            let claim = claim.ok_or("w5500 needs a bus and a chip select")?;
            let (link, ports) = open_link().map_err(|e| format!("w5500 network: {e}"))?;
            let chip = devices::W5500::new(claim, Some(link));
            Ok(Some(Built::new(Box::new(chip)).net(ports)))
        }
        _ => Ok(None),
    }
}

/// Open the UDP socket a wired NIC talks to QEMU over.
///
/// QEMU's `dgram` netdev needs a fixed local port, so one is found by binding
/// and releasing a socket. Another process could take it in the gap; the
/// consequence is QEMU failing to start with a clear message, not a silent
/// wrong answer.
fn open_link() -> std::io::Result<(devices::w5500::Link, (u16, u16))> {
    let ours = std::net::UdpSocket::bind(("127.0.0.1", 0))?;
    let our_port = ours.local_addr()?.port();
    let qemu_port = {
        let probe = std::net::UdpSocket::bind(("127.0.0.1", 0))?;
        probe.local_addr()?.port()
    };
    let peer = std::net::SocketAddr::from(([127, 0, 0, 1], qemu_port));
    Ok((devices::w5500::Link::new(ours, peer)?, (qemu_port, our_port)))
}

/// Attach a board's devices and start serving them on a background thread.
///
/// Binds port 0 and reports what the OS gave back, so two emulator windows do
/// not fight over a fixed port.
///
/// `modules` says what to plug into each slot, aligned with [`slot_defs`]:
/// `Some(Some(n))` plugs in option `n`, `Some(None)` leaves the slot empty,
/// and `None` (or a slot beyond the end of the list) takes the board's default.
pub fn start(
    board: &Board,
    sd_image: Option<PathBuf>,
    modules: &[Option<Option<usize>>],
) -> std::io::Result<Hardware> {
    let mut registry = Registry::new();
    let mut screen = None;
    let mut touch = None;
    let mut keys = None;
    let mut pad = None;
    let mut imu = None;
    let mut net = None;
    let mut attached = Vec::new();
    let defs = slot_defs(board);
    let mut slot_indices: Vec<Vec<usize>> = vec![Vec::new(); defs.len()];
    let mut unmodelled = Vec::new();

    for spec in board.peripherals.iter().filter(|p| p.enabled) {
        let claims = spec.all_claims().unwrap_or_default();
        match build(spec, claims, sd_image.as_deref()) {
            Ok(Some(built)) => {
                screen = built.screen.or(screen);
                touch = built.touch.or(touch);
                keys = built.keys.or(keys);
                pad = built.pad.or(pad);
                imu = built.imu.or(imu);
                net = built.net.or(net);
                match registry.register(built.device) {
                    Ok(index) => {
                        attached.push(spec.kind.clone());
                        let slot = spec.params.string_or("slot", "").unwrap_or_default();
                        if let Some(at) = defs.iter().position(|d| d.name == slot && !slot.is_empty()) {
                            slot_indices[at].push(index);
                        }
                    }
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

    // Plug in what was asked for (or the board's default) and switch the
    // rest of each slot off.
    let mut chosen = Vec::with_capacity(defs.len());
    for (at, def) in defs.iter().enumerate() {
        let want = match modules.get(at) {
            Some(Some(choice)) => *choice,
            _ => def.default,
        };
        let want = want.filter(|&w| w < slot_indices[at].len());
        for (i, &index) in slot_indices[at].iter().enumerate() {
            registry.set_enabled(index, Some(i) == want);
        }
        chosen.push(want);
    }

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
    let served = shared.clone();
    std::thread::Builder::new()
        .name("vpb".into())
        .spawn(move || serve_until_stopped(&listener, &served, &tx, &thread))?;

    Ok(Hardware {
        port,
        screen,
        touch,
        keys,
        pad,
        imu,
        net,
        attached,
        unmodelled,
        events,
        connected,
        running,
        trace,
        registry: shared,
        chosen: Mutex::new(chosen),
        slots: slot_indices.into_iter().map(|indices| SlotLive { indices }).collect(),
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
        let hw = start(&board(), None, &[]).expect("server should start");

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
        let hw = start(&board(), None, &[]).expect("server should start");
        assert!(hw.attached.contains(&"gt911".to_string()));
        assert!(hw.attached.contains(&"tdeck-keyboard".to_string()));
        assert!(hw.touch.is_some());
        assert!(hw.keys.is_some());
    }

    #[test]
    fn a_card_with_no_image_is_an_empty_slot_not_an_error() {
        let hw = start(&board(), None, &[]).expect("server should start");
        assert!(!hw.attached.contains(&"sdcard".to_string()));
        assert!(hw.unmodelled.contains(&"sdcard".to_string()));
    }

    #[test]
    fn the_display_is_sized_and_inverted_from_the_board_file() {
        // The T-Deck mounts a 240x320 panel sideways and wires the glass
        // inverted; both come from the file, not from the model's defaults.
        let hw = start(&board(), None, &[]).expect("server should start");
        let screen = hw.screen.as_ref().expect("display");
        let s = screen.lock().expect("screen");
        assert_eq!((s.width, s.height), (320, 240));
        assert!(s.panel_inverts);
    }

    /// A small board with one input slot and a motion sensor outside it.
    const SLOTTED: &str = r#"
[board]
id = "slotted"
name = "Slotted test board"
chip = "esp32s3"
flash_size = "16MB"

[board.psram]
kind = "octal"
size = "8MB"

[[bus]]
id = "i2c"
kind = "i2c"
controller = 0
controllers = [0, 1]
sda = 17
scl = 18

[[peripheral]]
kind = "icm42670"
bus = "i2c"
address = 0x68

[[peripheral]]
kind = "gamepad"
label = "game pad"
bus = "i2c"
address = 0x20
slot = "input"
slot_default = true

[[peripheral]]
kind = "tdeck-keyboard"
label = "keyboard"
bus = "i2c"
address = 0x55
slot = "input"

[support]
implemented = []
planned = []
"#;

    fn slotted() -> Board {
        Board::from_toml(SLOTTED).expect("test board should parse")
    }

    /// What firmware sees when it probes an address: a write with no bytes.
    fn acknowledges(hw: &Hardware, address: u8) -> bool {
        let tx = vpb::Transaction::I2cWrite { controller: 0, address, data: vec![], stop: true };
        let mut events = Vec::new();
        let reply = hw.registry.lock().unwrap().dispatch(&tx, &mut events);
        reply != vpb::Response::Nack
    }

    #[test]
    fn a_board_lists_its_slots_and_their_options() {
        let defs = slot_defs(&slotted());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "input");
        let kinds: Vec<&str> = defs[0].options.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, ["gamepad", "tdeck-keyboard"]);
        assert_eq!(defs[0].default, Some(0));
    }

    #[test]
    fn a_board_without_slots_has_none() {
        assert!(slot_defs(&board()).is_empty());
    }

    #[test]
    fn the_default_module_answers_and_the_others_do_not() {
        let hw = start(&slotted(), None, &[]).unwrap();
        assert!(acknowledges(&hw, 0x20), "the game pad is plugged in");
        assert!(!acknowledges(&hw, 0x55), "the keyboard is not");
        assert!(acknowledges(&hw, 0x68), "the sensor is not in the slot");
        assert_eq!(hw.selected_module(0), Some(0));
    }

    #[test]
    fn the_boot_choice_overrides_the_default() {
        let hw = start(&slotted(), None, &[Some(Some(1))]).unwrap();
        assert!(!acknowledges(&hw, 0x20));
        assert!(acknowledges(&hw, 0x55));
        let hw = start(&slotted(), None, &[Some(None)]).unwrap();
        assert!(!acknowledges(&hw, 0x20));
        assert!(!acknowledges(&hw, 0x55));
        assert_eq!(hw.selected_module(0), None);
    }

    #[test]
    fn swapping_a_module_while_running_changes_what_answers() {
        let hw = start(&slotted(), None, &[]).unwrap();
        hw.select_module(0, Some(1));
        assert!(!acknowledges(&hw, 0x20));
        assert!(acknowledges(&hw, 0x55));
        assert!(hw.is_active("tdeck-keyboard"));
        assert!(!hw.is_active("gamepad"));
        hw.select_module(0, None);
        assert!(!acknowledges(&hw, 0x55));
        hw.select_module(0, Some(0));
        assert!(acknowledges(&hw, 0x20));
    }

    #[test]
    fn unplugging_drops_held_buttons_and_unread_keys() {
        let hw = start(&slotted(), None, &[]).unwrap();
        hw.pad.as_ref().unwrap().set(devices::mcp23017::button::A, true);
        devices::TdeckKeyboard::press(hw.keys.as_ref().unwrap(), 'x');
        hw.select_module(0, Some(1));
        assert_eq!(hw.pad.as_ref().unwrap().buttons(), 0);
        assert!(hw.keys.as_ref().unwrap().lock().unwrap().is_empty());
    }

    #[test]
    fn a_choice_past_the_options_leaves_the_slot_empty() {
        let hw = start(&slotted(), None, &[Some(Some(9))]).unwrap();
        assert!(!acknowledges(&hw, 0x20));
        assert!(!acknowledges(&hw, 0x55));
    }
}
