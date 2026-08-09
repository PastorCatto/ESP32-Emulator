//! The emulator window.

use crate::session::{classify, DropKind, Session, SessionError};
use crate::screen::ScreenView;
use crate::terminal::Terminal;
use boards::Board;
use egui::{Color32, RichText};
use std::path::{Path, PathBuf};
use vpb::input::PointerPhase;
use vpb::trace::TraceConfig;

/// Boards compiled into the binary, so a fresh install has something to pick
/// without hunting for files. Dropping a `.toml` adds to this at runtime.
const BUILTIN_BOARDS: &[(&str, &str)] = &[
    ("t-deck-plus", include_str!("../../../boards/t-deck-plus.toml")),
    ("cyd-esp32-2432s028r", include_str!("../../../boards/cyd-esp32-2432s028r.toml")),
    ("cyd-s024c", include_str!("../../../boards/cyd-s024c.toml")),
    ("cyd-s028r", include_str!("../../../boards/cyd-s028r.toml")),
    ("generic-esp32s3", include_str!("../../../boards/generic-esp32s3.toml")),
];

/// Bus trace lines kept in memory. A single framebuffer push is thousands of
/// transactions, so this is a tail, not a log.
const BUS_LOG_LIMIT: usize = 5000;

const GENERIC: &str = include_str!("../../../boards/generic-esp32s3.toml");

/// A message shown in the status log.
#[derive(Debug, Clone)]
struct Note {
    text: String,
    error: bool,
}

pub struct App {
    session: Session,
    /// Board definitions available to pick from, including any dropped in.
    boards: Vec<Board>,
    selected_board: usize,
    notes: Vec<Note>,
    trace: TraceConfig,
    /// Per-peripheral enable switches, indexed alongside the board's list.
    device_enabled: Vec<bool>,
    run_dir: PathBuf,
    autoscroll: bool,
    terminal: Terminal,
    screen_view: ScreenView,
    /// Which serial port the console shows; None is the merged view.
    serial_view: Option<usize>,
    /// Recent bus traffic, when the tracer is on.
    bus_log: Vec<String>,
    /// Serial byte total and panel write counter as of the last frame, so a
    /// repaint can be skipped when neither moved.
    last_activity: (usize, u64),
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let mut boards = Vec::new();
        for &(name, src) in BUILTIN_BOARDS {
            match Board::from_toml(src) {
                Ok(b) => boards.push(b),
                // A built-in board failing to parse is a bug in our own data,
                // but it must not stop the app from starting.
                Err(e) => eprintln!("built-in board {name} failed to load: {e}"),
            }
        }
        let board = boards
            .first()
            .cloned()
            .unwrap_or_else(|| Board::from_toml(GENERIC).expect("generic board must parse"));
        let device_enabled = board.peripherals.iter().map(|p| p.enabled).collect();

        App {
            session: Session::new(board),
            boards,
            selected_board: 0,
            notes: vec![Note {
                text: "Drop a firmware .bin here to begin.".into(),
                error: false,
            }],
            trace: TraceConfig::default(),
            device_enabled,
            run_dir: std::env::current_dir().unwrap_or_default().join("run"),
            autoscroll: true,
            terminal: Terminal::default(),
            screen_view: ScreenView::default(),
            // UART0 carries the ROM banner and, for most firmware, the
            // application log too -- the closest thing to a single
            // complete view.
            serial_view: Some(0),
            bus_log: Vec::new(),
            last_activity: (0, 0),
        }
    }

    /// Start with files already loaded, as if they had been dropped in.
    ///
    /// Routed through the same classifier, so a path argument and a drag
    /// behave identically -- including the ordering rule that a board file
    /// resets the session, which is why boards are applied first.
    pub fn with_files(
        cc: &eframe::CreationContext<'_>,
        paths: &[PathBuf],
        autostart: bool,
        bypass: bool,
    ) -> Self {
        let mut app = App::new(cc);

        let is_board = |p: &PathBuf| {
            matches!(classify(p, &read_head(p, 64)), DropKind::BoardDefinition)
        };
        for path in paths.iter().filter(|p| is_board(p)) {
            app.handle_drop(path);
        }
        for path in paths.iter().filter(|p| !is_board(p)) {
            app.handle_drop(path);
        }

        if bypass && app.session.can_patch() {
            app.patch_radio();
        }
        if autostart && app.session.firmware.is_some() {
            app.boot();
        }
        app
    }

    /// Did anything worth redrawing for change since the last frame?
    ///
    /// Deliberately cheap: a byte count per serial port and the panel's write
    /// counter. Both are already maintained, so this costs a lock and a few
    /// comparisons -- far less than the repaint it avoids.
    fn activity(&mut self) -> bool {
        let serial: usize = self.session.serial.counts().iter().sum();
        let frame = self
            .session
            .screen()
            .map(|s| match s.lock() {
                Ok(g) => g.generation,
                Err(poisoned) => poisoned.into_inner().generation,
            })
            .unwrap_or(0);

        let changed = (serial, frame) != self.last_activity;
        self.last_activity = (serial, frame);
        changed
    }

    fn note(&mut self, text: impl Into<String>) {
        self.notes.push(Note { text: text.into(), error: false });
        self.trim_notes();
    }

    fn error(&mut self, text: impl Into<String>) {
        self.notes.push(Note { text: text.into(), error: true });
        self.trim_notes();
    }

    fn trim_notes(&mut self) {
        if self.notes.len() > 200 {
            self.notes.drain(..self.notes.len() - 200);
        }
    }

    fn select_board(&mut self, index: usize) {
        let Some(board) = self.boards.get(index).cloned() else { return };
        self.session.stop();
        self.device_enabled = board.peripherals.iter().map(|p| p.enabled).collect();
        self.session.board = board;
        self.selected_board = index;
        let name = self.session.board.name.clone();
        self.note(format!("Board set to {name}"));
    }

    /// Route a dropped file by what it actually is.
    fn handle_drop(&mut self, path: &Path) {
        let head = read_head(path, 64);
        match classify(path, &head) {
            DropKind::Firmware => self.load_firmware(path),
            DropKind::BoardDefinition => self.load_board(path),
            DropKind::Symbols => self.load_symbols(path),
            DropKind::SdCard => self.load_sd_image(path),
            DropKind::Unrecognised => {
                self.error(format!("{}: not something I recognise", name_of(path)))
            }
        }
    }

    fn load_firmware(&mut self, path: &Path) {
        let run_dir = self.run_dir.clone();
        match self.session.load_firmware(path, &run_dir) {
            Ok(()) => {
                let fw = self.session.firmware.clone().expect("set on success");
                let mut msg = format!(
                    "Loaded {} ({}, {}, {:.1} KiB)",
                    name_of(path),
                    fw.kind,
                    fw.chip,
                    fw.size as f32 / 1024.0
                );
                if let Some(p) = &fw.project {
                    msg.push_str(&format!(" — {p}"));
                }
                if let Some(v) = &fw.idf_version {
                    msg.push_str(&format!(" [IDF {v}]"));
                }
                self.note(msg);

                if fw.chip != self.session.board.chip {
                    let (a, b) = (fw.chip, self.session.board.chip);
                    self.error(format!(
                        "Firmware is for {a} but the selected board is {b}. Pick a matching board."
                    ));
                }
            }
            Err(e) => self.error(format!("{}: {e}", name_of(path))),
        }
    }

    /// Load an ELF's symbols, which is what makes patching possible.
    fn load_symbols(&mut self, path: &Path) {
        match self.session.load_symbols(path) {
            Ok(n) => self.note(format!("Symbols: {} ({n} symbols)", name_of(path))),
            Err(e) => self.error(format!("{}: {e}", name_of(path))),
        }
    }

    /// Replace the radio entry points in the assembled image.
    ///
    /// Its own action rather than part of booting. Patched firmware is not
    /// running what it would run on hardware, and burying that inside "run"
    /// would make it invisible at exactly the moment it matters.
    fn patch_radio(&mut self) {
        match self.session.patch_radio() {
            Ok(0) => self.error("Nothing to patch: load firmware and its .elf first"),
            Ok(n) => {
                self.note(format!("Patched {n} radio entry points — this image no longer runs the code hardware would"));
                let details: Vec<String> = self
                    .session
                    .patches
                    .iter()
                    .map(|p| format!("{} @ {:#010x}", p.symbol, p.address))
                    .collect();
                self.note(details.join(", "));
            }
            Err(e) => self.error(format!("Patch failed: {e}")),
        }
    }

    /// Attach a disk image as the SD card.
    ///
    /// Takes effect at the next boot: the card is a device model, and swapping
    /// one underneath a mounted filesystem is not something the firmware would
    /// survive on real hardware either.
    fn load_sd_image(&mut self, path: &Path) {
        let size = match std::fs::metadata(path) {
            Ok(m) => m.len(),
            Err(e) => return self.error(format!("{}: {e}", name_of(path))),
        };
        self.session.sd_image = Some(path.to_path_buf());

        let mib = size as f64 / (1024.0 * 1024.0);
        self.note(format!("SD card: {} ({mib:.0} MiB)", name_of(path)));
        if self.session.is_running() {
            self.note("Restart to insert it.");
        }
    }

    fn load_board(&mut self, path: &Path) {
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => return self.error(format!("{}: {e}", name_of(path))),
        };
        match Board::from_toml(&src) {
            Ok(board) => {
                let name = board.name.clone();
                // Replace a board with the same id rather than accumulating
                // duplicates every time the file is re-dropped during editing.
                let index = match self.boards.iter().position(|b| b.id == board.id) {
                    Some(i) => {
                        self.boards[i] = board;
                        i
                    }
                    None => {
                        self.boards.push(board);
                        self.boards.len() - 1
                    }
                };
                self.note(format!("Loaded board definition {name}"));
                self.select_board(index);
            }
            Err(e) => self.error(format!("{}: {e}", name_of(path))),
        }
    }

    fn boot(&mut self) {
        match self.session.boot() {
            Ok(()) => {
                self.note("Booting");
                self.report_hardware();
                // The bus server starts fresh each boot, so the tracer has to
                // be told again what the UI is currently asking for.
                self.apply_trace();
            }
            Err(SessionError::Qemu(e)) => self.error(format!("Could not start QEMU: {e}")),
            Err(e) => self.error(e.to_string()),
        }
    }

    /// Say what actually got attached, and what the board asked for that this
    /// build has no model for. Silence there is how you end up debugging
    /// firmware that is behaving correctly against hardware that is not here.
    fn report_hardware(&mut self) {
        let Some(hw) = &self.session.hardware else { return };
        let (attached, unmodelled) = (hw.attached.join(", "), hw.unmodelled.join(", "));

        if attached.is_empty() {
            self.note("No devices attached; the buses will look empty");
        } else {
            self.note(format!("Attached: {attached}"));
        }
        if !unmodelled.is_empty() {
            self.note(format!("Not modelled, so absent from the bus: {unmodelled}"));
        }
    }

    fn apply_trace(&self) {
        if let Some(hw) = &self.session.hardware {
            hw.set_trace(self.trace);
        }
    }

    /// Drain what the devices reported since the last frame.
    ///
    /// Must happen every frame whether or not anything is displayed: the
    /// channel is unbounded, and a traced boot produces thousands of records.
    fn drain_events(&mut self) {
        let Some(hw) = &self.session.hardware else { return };
        let mut lines = Vec::new();
        for event in hw.events.try_iter() {
            if let vpb::Event::Trace(record) = event {
                lines.push(record.to_string());
            }
        }
        for line in lines {
            self.bus_log.push(line);
        }
        // Keep the tail. A framebuffer push is thousands of transactions and
        // the interesting part is almost always the most recent.
        if self.bus_log.len() > BUS_LOG_LIMIT {
            self.bus_log.drain(..self.bus_log.len() - BUS_LOG_LIMIT);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Files dropped anywhere on the window.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        for path in dropped {
            self.handle_drop(&path);
        }

        self.session.pump();
        self.drain_events();
        let running = self.session.is_running();
        if running {
            // Serial and framebuffer updates arrive asynchronously, so
            // repaints have to be driven rather than waited for. But driving
            // them at a flat 30fps burns half a core redrawing an unchanged
            // window -- and burns it on the same core QEMU needs, so the
            // emulator runs slower the harder the UI spins.
            //
            // The guest only redraws at a few frames a second, and serial
            // arrives in bursts. So poll fast while something is actually
            // changing and idle back when it is not. egui still repaints
            // immediately on input, so this costs nothing in responsiveness.
            let changed = self.activity();
            let delay = if changed { 33 } else { 250 };
            ctx.request_repaint_after(std::time::Duration::from_millis(delay));
        }

        self.top_bar(ui, running);
        self.status_bar(ui);
        self.side_panel(ui);
        self.display_panel(ui);
        self.mimic_keyboard(&ctx);
        self.serial_console(ui);
        self.draw_drop_overlay(&ctx);

        // The detached terminal is its own OS window.
        self.terminal.show(&ctx, &mut self.session);
    }
}
// QEMU is shut down when the App drops, via Instance's Drop impl.

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui, running: bool) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let names: Vec<String> = self.boards.iter().map(|b| b.name.clone()).collect();
                let mut pick = self.selected_board;
                egui::ComboBox::from_id_salt("board")
                    .selected_text(
                        names
                            .get(self.selected_board)
                            .cloned()
                            .unwrap_or_else(|| "—".into()),
                    )
                    .show_ui(ui, |ui| {
                        for (i, n) in names.iter().enumerate() {
                            ui.selectable_value(&mut pick, i, n);
                        }
                    });
                if pick != self.selected_board {
                    self.select_board(pick);
                }

                ui.separator();

                let have_fw = self.session.firmware.is_some();
                if ui
                    .add_enabled(have_fw && !running, egui::Button::new("▶ Boot"))
                    .clicked()
                {
                    self.boot();
                }
                if ui.add_enabled(running, egui::Button::new("⟳ Reset")).clicked() {
                    self.boot();
                }
                if ui.add_enabled(running, egui::Button::new("■ Stop")).clicked() {
                    self.session.stop();
                    self.note("Stopped");
                }

                ui.separator();

                // Patching and booting are separate buttons as well as a
                // combined one, so the one-shot path is convenient without
                // making the two-step path harder to reach.
                let can_patch = self.session.can_patch();
                let patched = !self.session.patches.is_empty();
                let patch_hint = if patched {
                    "Already patched; reload the firmware to start over"
                } else if self.session.symbols.is_none() {
                    "Drop the firmware's .elf first — patches are located by symbol"
                } else if !have_fw {
                    "Load firmware first"
                } else {
                    "Replace the radio entry points so the boot completes"
                };
                if ui
                    .add_enabled(can_patch, egui::Button::new("Bypass radio"))
                    .on_hover_text(patch_hint)
                    .on_disabled_hover_text(patch_hint)
                    .clicked()
                {
                    self.patch_radio();
                }
                if ui
                    .add_enabled(can_patch && !running, egui::Button::new("Bypass + boot"))
                    .on_hover_text("Patch, then start")
                    .clicked()
                {
                    self.patch_radio();
                    if !self.session.patches.is_empty() {
                        self.boot();
                    }
                }
                if patched {
                    ui.label(
                        RichText::new(format!("⚑ {} patched", self.session.patches.len()))
                            .small()
                            .color(Color32::from_rgb(230, 180, 100)),
                    )
                    .on_hover_text(
                        "This image no longer runs the code hardware would run",
                    );
                }

                ui.separator();
                // The serial backdoor: a real terminal in its own window, for
                // driving a firmware shell rather than just watching a boot.
                let label = if self.terminal.open {
                    "Serial terminal ✓"
                } else {
                    "Serial terminal"
                };
                if ui
                    .button(label)
                    .on_hover_text("Open a detached serial terminal window")
                    .clicked()
                {
                    self.terminal.open = !self.terminal.open;
                }

                ui.separator();
                if running {
                    ui.colored_label(Color32::from_rgb(120, 220, 120), "● running");
                } else if have_fw {
                    ui.colored_label(Color32::GRAY, "○ ready");
                } else {
                    ui.colored_label(Color32::GRAY, "○ no firmware");
                }
            });
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("side").default_size(290.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Firmware");
                match &self.session.firmware {
                    None => {
                        ui.label(RichText::new("Nothing loaded").italics().weak());
                    }
                    Some(fw) => {
                        egui::Grid::new("fw").num_columns(2).striped(true).show(ui, |ui| {
                            let mut row = |k: &str, v: String| {
                                ui.label(RichText::new(k).weak());
                                ui.label(v);
                                ui.end_row();
                            };
                            row("File", name_of(&fw.path));
                            row("Chip", fw.chip.to_string());
                            row("Kind", fw.kind.to_string());
                            if let Some(p) = &fw.project {
                                row("Project", p.clone());
                            }
                            if let Some(v) = &fw.version {
                                row("Version", v.clone());
                            }
                            if let Some(v) = &fw.idf_version {
                                row("IDF", v.clone());
                            }
                            row("Size", format!("{:.1} KiB", fw.size as f32 / 1024.0));
                        });
                    }
                }

                ui.add_space(12.0);
                ui.heading("Patches");
                match &self.session.symbols {
                    Some(s) => {
                        ui.label(
                            RichText::new(format!(
                                "symbols: {} ({})",
                                name_of(&s.path),
                                s.count
                            ))
                            .small()
                            .weak(),
                        );
                        // Say which build these came from when it is not this
                        // one. Greying the button out without a reason sends
                        // people looking for the wrong problem.
                        match self.session.symbol_match() {
                            crate::session::SymbolMatch::Mismatch => {
                                ui.colored_label(
                                    Color32::from_rgb(235, 130, 120),
                                    RichText::new(
                                        "these symbols are from a different build — \
                                         drop this image's own .elf",
                                    )
                                    .small(),
                                );
                            }
                            crate::session::SymbolMatch::Unverifiable => {
                                ui.label(
                                    RichText::new(
                                        "image records no ELF digest, so the pair \
                                         cannot be checked",
                                    )
                                    .small()
                                    .weak(),
                                );
                            }
                            _ => {}
                        }
                    }
                    None => {
                        ui.label(
                            RichText::new(
                                "No .elf: patching falls back to byte signatures. \
                                 Drop the matching .elf for exact addresses.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                }
                if self.session.patches.is_empty() {
                    ui.label(RichText::new("none applied").small().weak());
                } else {
                    ui.colored_label(
                        Color32::from_rgb(230, 180, 100),
                        RichText::new("not what hardware would run").small(),
                    );
                    for p in &self.session.patches {
                        ui.label(
                            RichText::new(format!("{} @ {:#010x}", p.symbol, p.address))
                                .small()
                                .monospace(),
                        );
                    }
                }

                ui.add_space(12.0);
                ui.heading("Devices");
                if self.session.board.peripherals.is_empty() {
                    ui.label(RichText::new("This board declares no peripherals.").italics().weak());
                }
                // Which of these are real is only known once a bus is running,
                // because it depends on what models this build carries and on
                // whether an SD image was supplied. Before that, say nothing
                // rather than guess.
                let attached: Vec<&str> = self
                    .session
                    .hardware
                    .as_ref()
                    .map(|h| h.attached.iter().map(String::as_str).collect())
                    .unwrap_or_default();
                let running = self.session.hardware.is_some();

                for (i, p) in self.session.board.peripherals.iter().enumerate() {
                    if let Some(on) = self.device_enabled.get_mut(i) {
                        ui.horizontal(|ui| {
                            ui.checkbox(on, p.display_name());
                            if running {
                                if attached.contains(&p.kind.as_str()) {
                                    ui.label(RichText::new("on the bus").small().weak());
                                } else {
                                    ui.label(RichText::new("not modelled").small().weak());
                                }
                            }
                        });
                    }
                }

                ui.add_space(12.0);
                ui.heading("Bus tracer");
                ui.label(
                    RichText::new("Logs every transaction on the selected buses.")
                        .small()
                        .weak(),
                );
                let mut changed = false;
                changed |= ui.checkbox(&mut self.trace.i2c, "I²C").changed();
                changed |= ui.checkbox(&mut self.trace.spi, "SPI").changed();
                changed |= ui.checkbox(&mut self.trace.uart, "UART").changed();
                changed |= ui.checkbox(&mut self.trace.gpio, "GPIO").changed();
                changed |= ui.checkbox(&mut self.trace.decode, "Decode commands").changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut self.trace.max_bytes, 8..=512)
                            .text("max bytes")
                            .logarithmic(true),
                    )
                    .changed();
                if changed {
                    // The registry lives on the bus thread, so this is handed
                    // over and applied between transactions.
                    if let Some(hw) = &self.session.hardware {
                        hw.set_trace(self.trace);
                    }
                }
                ui.label(
                    RichText::new(format!("{} lines captured", self.bus_log.len()))
                        .small()
                        .weak(),
                );

                if !self.session.board.warnings.is_empty() {
                    ui.add_space(12.0);
                    ui.heading("Board warnings");
                    for w in &self.session.board.warnings {
                        ui.colored_label(Color32::from_rgb(230, 180, 100), w);
                    }
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .default_size(110.0)
            .resizable(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Log");
                    if ui.small_button("clear").clicked() {
                        self.notes.clear();
                    }
                    // Whether the emulator has actually reached the peripheral
                    // server. A boot that never connects looks identical to one
                    // whose devices are all silent, and this tells them apart.
                    if let Some(hw) = &self.session.hardware {
                        let (text, colour) = if hw.is_connected() {
                            ("bus connected", Color32::from_rgb(120, 200, 130))
                        } else {
                            ("bus waiting", Color32::from_rgb(230, 180, 100))
                        };
                        ui.label(RichText::new(text).small().color(colour));
                    }
                });
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for n in &self.notes {
                            if n.error {
                                ui.colored_label(Color32::from_rgb(240, 120, 120), &n.text);
                            } else {
                                ui.label(&n.text);
                            }
                        }
                    });
            });
    }

    /// The emulated panel. Absent entirely on a board with no display this
    /// build can model, rather than showing an empty frame that looks broken.
    fn display_panel(&mut self, ui: &mut egui::Ui) {
        let Some(screen) = self.session.screen().cloned() else {
            return;
        };
        let (w, h) = {
            let s = screen.lock().expect("screen");
            (s.width, s.height)
        };

        egui::Panel::right("display")
            .default_size(f32::from(w) + 24.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Display");
                    ui.label(RichText::new(format!("{w}x{h}")).small().weak());
                });
                ui.separator();
                let response = self.screen_view.show(ui, &screen);
                self.mimic_touch(ui, &response);

                if self.session.hardware.as_ref().is_some_and(|h| h.touch.is_some()) {
                    ui.label(
                        RichText::new("click to touch · type to use the keyboard")
                            .small()
                            .weak(),
                    );
                }
            });
    }

    /// Turn mouse activity over the panel into touch events.
    ///
    /// Press, drag and release map straight through, so a drag gesture works
    /// rather than only taps. The panel widget's own rectangle is the
    /// reference frame, which is why this needs the response and not just the
    /// pointer position -- the panel is scaled and centred.
    fn mimic_touch(&mut self, ui: &egui::Ui, response: &egui::Response) {
        let Some(touch) = self.session.hardware.as_ref().and_then(|h| h.touch.clone())
        else {
            return;
        };
        let rect = response.rect;
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return;
        }

        let pointer = ui.ctx().pointer_interact_pos();
        let phase = if response.drag_started() || response.is_pointer_button_down_on() {
            PointerPhase::Press
        } else if response.drag_stopped() || ui.ctx().input(|i| i.pointer.any_released()) {
            PointerPhase::Release
        } else {
            return;
        };

        let Ok(mut state) = touch.lock() else { return };
        match (phase, pointer) {
            (PointerPhase::Release, _) => {
                state.pointer(PointerPhase::Release, 0.0, 0.0, rect.width(), rect.height());
            }
            (phase, Some(pos)) => state.pointer(
                phase,
                pos.x - rect.min.x,
                pos.y - rect.min.y,
                rect.width(),
                rect.height(),
            ),
            _ => {}
        }
    }

    /// Send typed characters to the emulated keyboard.
    ///
    /// Only while the window has focus and the user is not typing into the
    /// terminal, which has its own input box and would otherwise receive
    /// every keystroke twice.
    fn mimic_keyboard(&mut self, ctx: &egui::Context) {
        let Some(keys) = self.session.hardware.as_ref().and_then(|h| h.keys.clone())
        else {
            return;
        };
        if ctx.egui_wants_keyboard_input() {
            return;
        }

        let typed: Vec<char> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Text(text) => Some(text.chars()),
                    _ => None,
                })
                .flatten()
                .collect()
        });
        for key in typed {
            devices::TdeckKeyboard::press(&keys, key);
        }
        // Enter and Backspace do not arrive as Text events, and a shell is
        // unusable without them.
        for (key, code) in [(egui::Key::Enter, b'\r'), (egui::Key::Backspace, 8)] {
            if ctx.input(|i| i.key_pressed(key)) {
                devices::TdeckKeyboard::press(&keys, code as char);
            }
        }
    }

    fn serial_console(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Serial");

                // Which port to read. The S3 ROM writes its banner to UART0
                // and the USB console both, so merging shows the boot twice;
                // the application afterwards splits across them, so one port
                // shows half. Hence the choice.
                let names = self.session.serial_port_names();
                let counts = self.session.serial.counts();
                let label = match self.serial_view {
                    Some(p) => names.get(p).copied().unwrap_or("port"),
                    None => "All ports",
                };
                egui::ComboBox::from_id_salt("serial-view")
                    .selected_text(label)
                    .width(150.0)
                    .show_ui(ui, |ui| {
                        for (i, name) in names.iter().enumerate() {
                            let bytes = counts.get(i).copied().unwrap_or(0);
                            let text = if bytes > 0 {
                                format!("{name} ({} KiB)", bytes / 1024)
                            } else {
                                format!("{name} (silent)")
                            };
                            ui.selectable_value(&mut self.serial_view, Some(i), text);
                        }
                        ui.selectable_value(&mut self.serial_view, None, "All ports (merged)");
                    });

                ui.checkbox(&mut self.autoscroll, "follow");
                if ui.small_button("clear").clicked() {
                    self.session.serial.clear();
                }

                let view = self.session.serial.view(self.serial_view);
                if view.trimmed > 0 {
                    ui.label(
                        RichText::new(format!("({} KiB trimmed)", view.trimmed / 1024))
                            .small()
                            .weak(),
                    );
                }
                ui.label(
                    RichText::new("— open the serial terminal to send commands")
                        .small()
                        .weak(),
                );
            });

            // One label per *visible* line, not one label for the whole log.
            //
            // Handing the entire buffer to a single wrapped Label makes egui
            // shape and wrap every glyph in it to build the galley -- on every
            // frame, at 30fps, for up to half a megabyte, to display the forty
            // lines that fit on screen. That measured at around half a core
            // and got worse as the log grew, which is exactly backwards: the
            // emulator needs that CPU, and the serial pane was starving it.
            //
            // `show_rows` needs a uniform row height, so lines extend
            // horizontally rather than wrapping. That suits log output, which
            // is already line-oriented, and the horizontal scrollbar is a
            // better way to read a long line than a reflowed one anyway.
            let view = self.session.serial.view(self.serial_view);
            let text = view.text();
            let lines: Vec<&str> = text.lines().collect();
            let row_height = ui.text_style_height(&egui::TextStyle::Monospace);

            egui::ScrollArea::both()
                .stick_to_bottom(self.autoscroll)
                .auto_shrink([false, false])
                .show_rows(ui, row_height, lines.len(), |ui, rows| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for line in &lines[rows] {
                        ui.add(
                            egui::Label::new(RichText::new(*line).monospace())
                                .selectable(true)
                                .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    }
                });
        });
    }

    /// Visual feedback while a file is hovering over the window.
    fn draw_drop_overlay(&self, ctx: &egui::Context) {
        if ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let screen = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop"),
        ));
        painter.rect_filled(screen, 0, Color32::from_black_alpha(160));
        painter.text(
            screen.center(),
            egui::Align2::CENTER_CENTER,
            "Drop firmware, a board .toml, or an SD image",
            egui::FontId::proportional(22.0),
            Color32::WHITE,
        );
    }
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Read the first `n` bytes, for content sniffing. A short or unreadable file
/// simply yields fewer bytes and falls through to extension matching.
fn read_head(path: &Path, n: usize) -> Vec<u8> {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; n];
    match f.read(&mut buf) {
        Ok(read) => {
            buf.truncate(read);
            buf
        }
        Err(_) => Vec::new(),
    }
}
