//! The emulator window.

use crate::metro::{self, Icon};
use crate::screen::{ScaleMode, ScreenView};
use crate::session::{classify, DropKind, Session, SessionError};
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
    ("t-deck-plus-vm", include_str!("../../../boards/t-deck-plus-vm.toml")),
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

/// The headings of the log window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogTab {
    Log,
    Serial,
    Images,
}

pub struct App {
    session: Session,
    accent: Color32,
    /// The application bar is showing labels and the `···` menu.
    bar_open: bool,
    /// Measured each frame, so the menu can sit exactly above the bar.
    bar_height: f32,
    log_open: bool,
    log_tab: LogTab,
    /// The add-an-image form on the firmware images tab.
    image_path: String,
    image_offset: String,
    /// The title last sent to the window, so it is only sent when it changes.
    shown_title: String,
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
    /// Writes what the panel holds to a file, when ESP32_SCREEN_DUMP is set.
    screen_dump: Option<crate::dump::ScreenDump>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        metro::apply(&cc.egui_ctx, metro::CYAN);

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

        let run_dir = std::env::current_dir().unwrap_or_default().join("run");
        let mut session = Session::new(board);
        session.run_dir = run_dir.clone();

        App {
            session,
            accent: metro::CYAN,
            bar_open: false,
            bar_height: 62.0,
            log_open: false,
            log_tab: LogTab::Log,
            image_path: String::new(),
            image_offset: String::new(),
            shown_title: String::new(),
            boards,
            selected_board: 0,
            notes: vec![Note {
                text: "Drop a firmware .bin here to begin.".into(),
                error: false,
            }],
            trace: TraceConfig::default(),
            device_enabled,
            run_dir,
            autoscroll: true,
            terminal: Terminal::default(),
            screen_view: ScreenView::default(),
            // UART0 carries the ROM banner and, for most firmware, the
            // application log too -- the closest thing to a single
            // complete view.
            serial_view: Some(0),
            bus_log: Vec::new(),
            last_activity: (0, 0),
            screen_dump: crate::dump::ScreenDump::from_env(),
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
        accent: Option<Color32>,
    ) -> Self {
        let mut app = App::new(cc);
        if let Some(accent) = accent {
            app.accent = accent;
            metro::apply(&cc.egui_ctx, accent);
        }

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
        if autostart && app.session.can_boot() {
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

        if let (Some(dump), Some(screen)) = (self.screen_dump.as_mut(), self.session.screen()) {
            dump.tick(screen);
        }

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

        self.update_title(&ctx, running);
        self.app_bar(ui, running);
        self.pad_panel(ui);
        self.docked_terminal(ui);
        self.stage(ui, running);
        self.mimic_keyboard(&ctx);
        self.draw_drop_overlay(&ctx);

        // The detached terminal and the log are their own OS windows.
        self.terminal.show(&ctx, &mut self.session);
        self.log_window(&ctx);
    }
}
// QEMU is shut down when the App drops, via Instance's Drop impl.

impl App {
    /// The window title carries the state, so it shows in a taskbar too.
    fn update_title(&mut self, ctx: &egui::Context, running: bool) {
        let state = if running {
            "running"
        } else if self.session.can_boot() {
            "ready"
        } else {
            "no firmware"
        };
        let title = format!("ESP32 Emulator — {} · {state}", self.session.board.name);
        if title != self.shown_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.shown_title = title;
        }
    }

    /// The application bar along the bottom: the four things you do all the
    /// time, and `···` for everything else.
    fn app_bar(&mut self, ui: &mut egui::Ui, running: bool) {
        let accent = self.accent;
        let can_boot = self.session.can_boot();
        let labels = self.bar_open;

        let bar = egui::Panel::bottom("appbar")
            .frame(
                egui::Frame::NONE
                    .fill(metro::BAR)
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    if metro::bar_button(ui, Icon::Play, "boot", can_boot && !running, labels, accent)
                        .clicked()
                    {
                        self.boot();
                    }
                    if metro::bar_button(ui, Icon::Reset, "reset", running, labels, accent).clicked() {
                        self.boot();
                    }
                    if metro::bar_button(ui, Icon::Stop, "stop", running, labels, accent).clicked() {
                        self.session.stop();
                        self.note("Stopped");
                    }
                    if metro::bar_button(ui, Icon::Log, "log", true, labels, accent).clicked() {
                        self.log_open = !self.log_open;
                    }
                    // Only boards with a place to plug a module in get this.
                    if !crate::hardware::slot_defs(&self.session.board).is_empty()
                        && metro::bar_button(ui, Icon::Pad, "input", true, labels, accent)
                            .on_hover_text(self.module_summary())
                            .clicked()
                    {
                        self.cycle_module(0);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        if metro::dots_button(ui, accent).clicked() {
                            self.bar_open = !self.bar_open;
                        }
                    });
                });
            });
        self.bar_height = bar.response.rect.height();

        if self.bar_open {
            if ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
                self.bar_open = false;
            } else {
                self.bar_menu(ui.ctx(), running);
            }
        }
    }

    /// What `···` opens: a flat list over the screen, so showing it does not
    /// change the shape the screen is fitted to.
    fn bar_menu(&mut self, ctx: &egui::Context, running: bool) {
        let max_height = (ctx.content_rect().height() - self.bar_height - 24.0).max(120.0);
        egui::Area::new(egui::Id::new("bar-menu"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_BOTTOM, [0.0, -self.bar_height])
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(metro::BAR)
                    .inner_margin(egui::Margin::symmetric(18, 12))
                    .show(ui, |ui| {
                        ui.set_width(270.0);
                        egui::ScrollArea::vertical()
                            .max_height(max_height)
                            .show(ui, |ui| self.menu_items(ui, running));
                    });
            });
    }

    fn menu_items(&mut self, ui: &mut egui::Ui, running: bool) {
        // Board.
        menu_heading(ui, "board");
        let names: Vec<String> = self.boards.iter().map(|b| b.name.clone()).collect();
        let mut pick = self.selected_board;
        egui::ComboBox::from_id_salt("board")
            .selected_text(names.get(self.selected_board).cloned().unwrap_or_else(|| "—".into()))
            .width(230.0)
            .show_ui(ui, |ui| {
                for (i, n) in names.iter().enumerate() {
                    ui.selectable_value(&mut pick, i, n);
                }
            });
        if pick != self.selected_board {
            self.select_board(pick);
        }

        // Modules: what is plugged into each slot, changeable while running.
        self.module_menu(ui);
        self.motion_menu(ui);

        // How the panel is scaled to the window.
        menu_heading(ui, "screen");
        ui.radio_value(&mut self.screen_view.mode, ScaleMode::Crisp, "crisp pixels")
            .on_hover_text("Whole-number scaling, sharp pixels, black borders between sizes");
        ui.radio_value(&mut self.screen_view.mode, ScaleMode::Smooth, "smooth")
            .on_hover_text("Any size, always fills the window, slightly soft");

        // The serial terminal, in its own window or inside this one.
        menu_heading(ui, "serial terminal");
        if menu_row(ui, true, if self.terminal.open { "close terminal" } else { "open terminal" })
            .clicked()
        {
            self.terminal.open = !self.terminal.open;
        }
        ui.radio_value(&mut self.terminal.docked, false, "separate window");
        ui.radio_value(&mut self.terminal.docked, true, "docked in this window");

        // Patching, kept as visible steps rather than part of booting.
        menu_heading(ui, "radio bypass");
        let have_fw = self.session.firmware.is_some();
        let can_patch = self.session.can_patch();
        let patched = !self.session.patches.is_empty();
        let hint = if patched {
            "Already patched; reload the firmware to start over"
        } else if self.session.symbols.is_none() {
            "Drop the firmware's .elf first — patches are located by symbol"
        } else if !have_fw {
            "Load firmware first"
        } else {
            "Replace the radio entry points so the boot completes"
        };
        if menu_row(ui, can_patch, "bypass radio").on_hover_text(hint).on_disabled_hover_text(hint).clicked() {
            self.patch_radio();
        }
        if menu_row(ui, can_patch && !running, "bypass + boot").clicked() {
            self.patch_radio();
            if !self.session.patches.is_empty() {
                self.boot();
            }
        }
        if patched {
            ui.label(
                RichText::new(format!("⚑ {} patched — not what hardware would run", self.session.patches.len()))
                    .small()
                    .color(metro::WARN),
            );
        }

        egui::CollapsingHeader::new(RichText::new("devices").size(13.0).color(metro::DIM))
            .show(ui, |ui| self.devices_section(ui));
        egui::CollapsingHeader::new(RichText::new("bus tracer").size(13.0).color(metro::DIM))
            .show(ui, |ui| self.tracer_section(ui));

        if !self.session.board.warnings.is_empty() {
            menu_heading(ui, "board warnings");
            for w in &self.session.board.warnings {
                ui.label(RichText::new(w).small().color(metro::WARN));
            }
        }

        ui.add_space(8.0);
        ui.label(
            RichText::new("drop a .bin, .elf, .img or board .toml anywhere on the window")
                .small()
                .color(metro::DIM),
        );
    }

    fn devices_section(&mut self, ui: &mut egui::Ui) {
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
    }

    fn tracer_section(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Logs every transaction on the selected buses.").small().weak());
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
        ui.label(RichText::new(format!("{} lines captured", self.bus_log.len())).small().weak());
    }

    /// The terminal as a panel of this window, when it is set to dock.
    fn docked_terminal(&mut self, ui: &mut egui::Ui) {
        if !(self.terminal.open && self.terminal.docked) {
            return;
        }
        egui::Panel::bottom("terminal-dock")
            .resizable(true)
            .default_size(230.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("serial terminal").size(18.0));
                    if ui.small_button("pop out").clicked() {
                        self.terminal.docked = false;
                    }
                    if ui.small_button("close").clicked() {
                        self.terminal.open = false;
                    }
                });
                self.terminal.show_docked(ui, &mut self.session);
            });
    }

    /// The emulated panel, scaled to whatever the window gives it. Everything
    /// else on this screen is chrome; this takes the rest.
    fn stage(&mut self, ui: &mut egui::Ui, running: bool) {
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ui, |ui| {
                if let Some(screen) = self.session.screen().cloned() {
                    let response = self.screen_view.show(ui, &screen);
                    self.mimic_touch(ui, &response);
                } else {
                    self.idle_stage(ui, running);
                }
            });
    }

    /// What fills the window before there is a panel to show.
    fn idle_stage(&mut self, ui: &mut egui::Ui, running: bool) {
        let hint = if running {
            "this board has no display this build can model"
        } else if self.session.can_boot() {
            "press boot"
        } else {
            "drop a firmware .bin here, or add images in the log window"
        };
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() / 2.0 - 60.0).max(8.0));
            metro::kicker(ui, "ESP32 Emulator");
            ui.label(RichText::new(self.session.board.name.to_lowercase()).size(40.0));
            ui.label(RichText::new(hint).color(metro::DIM));
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
        // Only while a keyboard is actually plugged in: typing at an empty
        // slot must not queue keys for whichever module arrives next.
        let Some(keys) = self
            .session
            .hardware
            .as_ref()
            .filter(|h| h.is_active("tdeck-keyboard"))
            .and_then(|h| h.keys.clone())
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

    /// What the input slot holds, for a tooltip.
    fn module_summary(&self) -> String {
        let defs = crate::hardware::slot_defs(&self.session.board);
        let Some(def) = defs.first() else { return String::new() };
        let now = match self.session.module(0).and_then(|i| def.options.get(i)) {
            Some((_, label)) => label.clone(),
            None => "nothing".into(),
        };
        format!("{}: {now}. Click to change.", def.name)
    }

    /// Step a slot through "nothing" and each of its options, in order.
    fn cycle_module(&mut self, slot: usize) {
        let defs = crate::hardware::slot_defs(&self.session.board);
        let Some(def) = defs.get(slot) else { return };
        let next = match self.session.module(slot) {
            None => (!def.options.is_empty()).then_some(0),
            Some(i) if i + 1 < def.options.len() => Some(i + 1),
            Some(_) => None,
        };
        self.session.set_module(slot, next);
        let what = match next.and_then(|i| def.options.get(i)) {
            Some((_, label)) => format!("{} plugged in: {label}", def.name),
            None => format!("{} emptied", def.name),
        };
        self.note(&what);
    }

    /// The `···` menu's module section.
    fn module_menu(&mut self, ui: &mut egui::Ui) {
        let defs = crate::hardware::slot_defs(&self.session.board);
        for (slot, def) in defs.iter().enumerate() {
            menu_heading(ui, &def.name);
            let now = self.session.module(slot);
            let mut pick = None;
            if ui.radio(now.is_none(), "nothing").clicked() {
                pick = Some(None);
            }
            for (i, (_, label)) in def.options.iter().enumerate() {
                if ui.radio(now == Some(i), label).clicked() {
                    pick = Some(Some(i));
                }
            }
            if let Some(choice) = pick {
                if choice != now {
                    self.session.set_module(slot, choice);
                }
            }
        }
    }

    /// Tilt for the motion sensor, when the board has one.
    fn motion_menu(&mut self, ui: &mut egui::Ui) {
        let Some(imu) = self.session.hardware.as_ref().and_then(|h| h.imu.clone()) else {
            return;
        };
        let Ok(mut state) = imu.lock() else { return };
        menu_heading(ui, "motion sensor");
        for (axis, name) in ["x", "y", "z"].iter().enumerate() {
            ui.add(
                egui::Slider::new(&mut state.accel[axis], -2.0..=2.0)
                    .text(format!("accel {name} (g)"))
                    .fixed_decimals(2),
            );
        }
        if menu_row(ui, true, "lay flat").clicked() {
            *state = devices::ImuState::default();
        }
    }

    /// The on-screen game pad: shown while one is plugged in. Mouse and keys
    /// both hold buttons; the lamps show what the guest has lit.
    fn pad_panel(&mut self, ui: &mut egui::Ui) {
        use devices::mcp23017::button;
        let Some(pad) = self
            .session
            .hardware
            .as_ref()
            .filter(|h| h.is_active("gamepad"))
            .and_then(|h| h.pad.clone())
        else {
            return;
        };
        let accent = self.accent;
        let typing = ui.ctx().egui_wants_keyboard_input();
        let key_down = |keys: &[egui::Key]| {
            !typing && ui.ctx().input(|i| keys.iter().any(|k| i.key_down(*k)))
        };
        // Arrows for the d-pad; Z X A S for the face buttons; Space for func.
        let keyboard = [
            (button::UP, key_down(&[egui::Key::ArrowUp])),
            (button::DOWN, key_down(&[egui::Key::ArrowDown])),
            (button::LEFT, key_down(&[egui::Key::ArrowLeft])),
            (button::RIGHT, key_down(&[egui::Key::ArrowRight])),
            (button::A, key_down(&[egui::Key::Z])),
            (button::B, key_down(&[egui::Key::X])),
            (button::X, key_down(&[egui::Key::A])),
            (button::Y, key_down(&[egui::Key::S])),
            (button::FUNC, key_down(&[egui::Key::Space])),
        ];
        let leds = pad.leds();
        let mut held = 0u16;
        for (mask, down) in keyboard {
            if down {
                held |= mask;
            }
        }

        egui::Panel::bottom("pad")
            .frame(
                egui::Frame::NONE
                    .fill(metro::BAR)
                    .inner_margin(egui::Margin::symmetric(10, 8)),
            )
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| {
                // (button, column, row) on a small grid, with the d-pad as a
                // plus on the left and the four face buttons to its right.
                let cell = 46.0_f32;
                let gap = 6.0_f32;
                let layout: [(u16, f32, f32); 9] = [
                    (button::UP, 1.0, 0.0),
                    (button::LEFT, 0.0, 1.0),
                    (button::RIGHT, 2.0, 1.0),
                    (button::DOWN, 1.0, 2.0),
                    (button::A, 4.5, 1.0),
                    (button::B, 5.5, 1.0),
                    (button::X, 6.5, 1.0),
                    (button::Y, 7.5, 1.0),
                    (button::FUNC, 9.0, 1.0),
                ];
                let width = 10.0 * (cell + gap);
                let height = 3.0 * (cell + gap);
                let (area, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
                let origin = egui::pos2((ui.max_rect().center().x - width / 2.0).max(area.min.x), area.min.y);
                for (mask, col, row) in layout {
                    let rect = egui::Rect::from_min_size(
                        origin + egui::vec2(col * (cell + gap), row * (cell + gap)),
                        egui::vec2(cell, cell),
                    );
                    let id = ui.id().with(("pad", mask));
                    let response = ui.interact(rect, id, egui::Sense::click_and_drag());
                    if response.is_pointer_button_down_on() {
                        held |= mask;
                    }
                    let down = held & mask != 0;
                    let painter = ui.painter();
                    let fill = if down { accent } else if response.hovered() { Color32::from_gray(0x3a) } else { metro::TILE };
                    painter.rect_filled(rect, egui::CornerRadius::ZERO, fill);
                    let name = button::ALL.iter().find(|(m, _)| *m == mask).map(|(_, n)| *n).unwrap_or("?");
                    painter.text(
                        rect.center() + egui::vec2(0.0, 4.0),
                        egui::Align2::CENTER_CENTER,
                        name,
                        egui::FontId::proportional(14.0),
                        Color32::WHITE,
                    );
                    // The lamp: lit when the guest drove its pin low.
                    let lamp = rect.right_top() + egui::vec2(-9.0, 9.0);
                    let lit = leds & mask != 0;
                    painter.circle_filled(
                        lamp,
                        4.0,
                        if lit { Color32::from_rgb(0xff, 0xc1, 0x07) } else { Color32::from_gray(0x55) },
                    );
                }
            });
        pad.hold(held);
    }

    /// The log, in its own window: log, serial, and firmware images.
    fn log_window(&mut self, ctx: &egui::Context) {
        if !self.log_open {
            return;
        }
        let id = egui::ViewportId::from_hash_of("log-window");
        let builder = egui::ViewportBuilder::default()
            .with_title("log — ESP32 Emulator")
            .with_inner_size([640.0, 540.0])
            .with_min_inner_size([420.0, 320.0]);

        let mut close_requested = false;
        ctx.show_viewport_immediate(id, builder, |ui, _class| {
            self.log_contents(ui);
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                close_requested = true;
            }
        });
        if close_requested {
            self.log_open = false;
        }
    }

    fn log_contents(&mut self, ui: &mut egui::Ui) {
        // Follow, clear and copy only mean something for the two text panes.
        if self.log_tab != LogTab::Images {
            self.log_bar(ui);
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(Color32::BLACK)
                    .inner_margin(egui::Margin::symmetric(18, 12)),
            )
            .show(ui, |ui| {
                metro::kicker(ui, "ESP32 Emulator");
                let mut tab = self.log_tab;
                metro::pivot(
                    ui,
                    &mut tab,
                    &[
                        (LogTab::Log, "log"),
                        (LogTab::Serial, "serial"),
                        (LogTab::Images, "firmware images"),
                    ],
                );
                self.log_tab = tab;
                ui.add_space(8.0);
                match self.log_tab {
                    LogTab::Log => self.log_pane(ui),
                    LogTab::Serial => self.serial_pane(ui),
                    LogTab::Images => self.images_pane(ui),
                }
            });
    }

    fn log_bar(&mut self, ui: &mut egui::Ui) {
        let accent = self.accent;
        egui::Panel::bottom("log-bar")
            .frame(
                egui::Frame::NONE
                    .fill(metro::BAR)
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show_separator_line(false)
            .resizable(false)
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let follow = if self.autoscroll { "follow on" } else { "follow off" };
                    if metro::bar_button(ui, Icon::Follow, follow, true, true, accent).clicked() {
                        self.autoscroll = !self.autoscroll;
                    }
                    if metro::bar_button(ui, Icon::Clear, "clear", true, true, accent).clicked() {
                        match self.log_tab {
                            LogTab::Serial => self.session.serial.clear(),
                            _ => self.notes.clear(),
                        }
                    }
                    if metro::bar_button(ui, Icon::Copy, "copy", true, true, accent).clicked() {
                        let text = match self.log_tab {
                            LogTab::Serial => {
                                self.session.serial.view(self.serial_view).text().to_string()
                            }
                            _ => self
                                .notes
                                .iter()
                                .map(|n| n.text.as_str())
                                .collect::<Vec<_>>()
                                .join("\n"),
                        };
                        ui.ctx().copy_text(text);
                    }
                });
            });
    }

    /// The notes the app writes as it works.
    fn log_pane(&mut self, ui: &mut egui::Ui) {
        // Whether the emulator has actually reached the peripheral
        // server. A boot that never connects looks identical to one
        // whose devices are all silent, and this tells them apart.
        if let Some(hw) = &self.session.hardware {
            let (text, colour) = if hw.is_connected() {
                ("bus connected", metro::OK)
            } else {
                ("bus waiting", metro::WARN)
            };
            ui.label(RichText::new(text).small().color(colour));
        }
        egui::ScrollArea::vertical()
            .stick_to_bottom(self.autoscroll)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for n in &self.notes {
                    if n.error {
                        ui.colored_label(metro::ERROR, &n.text);
                    } else {
                        ui.label(&n.text);
                    }
                }
            });
    }

    /// Serial output, one port or all of them merged.
    fn serial_pane(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // Which port to read. The S3 ROM writes its banner to UART0 and
            // the USB console both, so merging shows the boot twice; the
            // application afterwards splits across them, so one port shows
            // half. Hence the choice.
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

            let view = self.session.serial.view(self.serial_view);
            if view.trimmed > 0 {
                ui.label(RichText::new(format!("({} KiB trimmed)", view.trimmed / 1024)).small().weak());
            }
            ui.label(RichText::new("open the serial terminal to send commands").small().weak());
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
    }

    /// What is loaded, plus raw images laid into flash at chosen offsets.
    ///
    /// The second half is what lets the emulator run on its own: a
    /// bootloader, a partition table and an app placed by hand, or somebody
    /// else's firmware, with no merged image required.
    fn images_pane(&mut self, ui: &mut egui::Ui) {
        // Images dropped on this window go in here, not into the session.
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
            i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
        });
        for path in dropped {
            let offset = self.suggest_offset(&path);
            self.add_image(&path, offset);
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::NONE
                .fill(Color32::from_rgb(0x3a, 0x2a, 0x00))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(
                            "Heads up: this emulator was tailored to the PURR OS project. \
                             Running your own images is supported, but it may need some \
                             work before it is fully functional.",
                        )
                        .color(metro::WARN),
                    );
                });
            ui.add_space(12.0);

            self.loaded_firmware_section(ui);

            ui.add_space(14.0);
            ui.label(RichText::new("custom images").size(20.0));
            ui.label(
                RichText::new(
                    "Written into flash at boot, over the loaded firmware if there is \
                     one and over blank flash if there is not.",
                )
                .small()
                .color(metro::DIM),
            );
            ui.add_space(6.0);

            let mut remove = None;
            for (i, image) in self.session.extra_images.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{:#010x}", image.offset)).monospace());
                    ui.label(name_of(&image.path));
                    ui.label(RichText::new(format!("{:.1} KiB", image.size as f32 / 1024.0)).small().weak());
                    if ui.small_button("remove").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                self.session.remove_image(i);
            }
            if self.session.extra_images.is_empty() {
                ui.label(RichText::new("none").italics().weak());
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.image_path)
                        .hint_text("path to an image file")
                        .desired_width(240.0),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.image_offset)
                        .hint_text("offset, e.g. 0x10000")
                        .desired_width(130.0)
                        .font(egui::TextStyle::Monospace),
                );
                if ui.button("add").clicked() {
                    self.add_typed_image();
                }
            });
            let chip = self.session.firmware.as_ref().map_or(self.session.board.chip, |f| f.chip);
            ui.label(
                RichText::new(format!(
                    "{chip}: bootloader at {:#x}, partition table at 0x8000, app at 0x10000. \
                     Drop files on this window to add them at those offsets.",
                    chip.bootloader_offset()
                ))
                .small()
                .color(metro::DIM),
            );
        });
    }

    /// What the loaded firmware is, and what has been done to it.
    fn loaded_firmware_section(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("loaded firmware").size(20.0));
        match &self.session.firmware {
            None => {
                ui.label(RichText::new("nothing loaded").italics().weak());
            }
            Some(fw) => {
                egui::Grid::new("fw").num_columns(2).striped(true).show(ui, |ui| {
                    let mut row = |k: &str, v: String| {
                        ui.label(RichText::new(k).weak());
                        ui.label(v);
                        ui.end_row();
                    };
                    row("file", name_of(&fw.path));
                    row("chip", fw.chip.to_string());
                    row("kind", fw.kind.to_string());
                    if let Some(p) = &fw.project {
                        row("project", p.clone());
                    }
                    if let Some(v) = &fw.version {
                        row("version", v.clone());
                    }
                    if let Some(v) = &fw.idf_version {
                        row("IDF", v.clone());
                    }
                    row("size", format!("{:.1} KiB", fw.size as f32 / 1024.0));
                });
            }
        }

        ui.add_space(8.0);
        match &self.session.symbols {
            Some(s) => {
                ui.label(
                    RichText::new(format!("symbols: {} ({})", name_of(&s.path), s.count))
                        .small()
                        .weak(),
                );
                // Say which build these came from when it is not this one.
                // Greying the button out without a reason sends people
                // looking for the wrong problem.
                match self.session.symbol_match() {
                    crate::session::SymbolMatch::Mismatch => {
                        ui.colored_label(
                            metro::ERROR,
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
                                "image records no ELF digest, so the pair cannot be checked",
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
            ui.label(RichText::new("no patches applied").small().weak());
        } else {
            ui.colored_label(metro::WARN, RichText::new("patched: not what hardware would run").small());
            for p in &self.session.patches {
                ui.label(
                    RichText::new(format!("{} @ {:#010x}", p.symbol, p.address))
                        .small()
                        .monospace(),
                );
            }
        }
    }

    /// Where a dropped file most likely belongs, from its name.
    fn suggest_offset(&self, path: &Path) -> u32 {
        let name = name_of(path).to_lowercase();
        let chip = self.session.firmware.as_ref().map_or(self.session.board.chip, |f| f.chip);
        if name.contains("bootloader") {
            chip.bootloader_offset()
        } else if name.contains("partition") {
            0x8000
        } else {
            0x10000
        }
    }

    fn add_image(&mut self, path: &Path, offset: u32) {
        match self.session.add_image(path, offset) {
            Ok(()) => {
                self.note(format!("Image: {} at {offset:#x}", name_of(path)));
                if self.session.is_running() {
                    self.note("Restart to apply it.");
                }
            }
            Err(e) => self.error(format!("{}: {e}", name_of(path))),
        }
    }

    fn add_typed_image(&mut self) {
        let path = PathBuf::from(self.image_path.trim().trim_matches('"'));
        let text = self.image_offset.trim();
        let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => text.parse::<u32>().ok(),
        };
        let Some(offset) = parsed.or_else(|| text.is_empty().then(|| self.suggest_offset(&path)))
        else {
            return self.error(format!("{text:?} is not an offset; try 0x10000"));
        };
        self.add_image(&path, offset);
        self.image_path.clear();
        self.image_offset.clear();
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

/// A section label in the `···` menu.
fn menu_heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text).size(13.0).color(metro::DIM));
}

/// A flat, left-aligned row in the `···` menu.
fn menu_row(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    ui.add_enabled(enabled, egui::Button::selectable(false, RichText::new(text).size(16.0)))
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
