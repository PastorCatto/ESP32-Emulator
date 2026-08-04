//! The emulator window.

use crate::session::{classify, DropKind, Session, SessionError};
use crate::terminal::Terminal;
use boards::Board;
use egui::{Color32, RichText};
use std::path::{Path, PathBuf};
use vpb::trace::TraceConfig;

const T_DECK: &str = include_str!("../../../boards/t-deck-plus.toml");
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
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let mut boards = Vec::new();
        for (name, src) in [("t-deck-plus", T_DECK), ("generic-esp32s3", GENERIC)] {
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
        }
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
            DropKind::SdCard => self.error(format!(
                "{}: SD card images are not wired up yet (needs the SPI controller)",
                name_of(path)
            )),
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
            Ok(()) => self.note("Booting"),
            Err(SessionError::Qemu(e)) => self.error(format!("Could not start QEMU: {e}")),
            Err(e) => self.error(e.to_string()),
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
        let running = self.session.is_running();
        if running {
            // Serial arrives asynchronously, so drive repaints rather than
            // waiting for input events.
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        self.top_bar(ui, running);
        self.status_bar(ui);
        self.side_panel(ui);
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
                ui.heading("Devices");
                if self.session.board.peripherals.is_empty() {
                    ui.label(RichText::new("This board declares no peripherals.").italics().weak());
                }
                for (i, p) in self.session.board.peripherals.iter().enumerate() {
                    if let Some(on) = self.device_enabled.get_mut(i) {
                        ui.horizontal(|ui| {
                            ui.checkbox(on, p.display_name());
                            // Be honest: the buses these hang off do not exist
                            // in QEMU yet, so nothing here is driving hardware.
                            ui.label(RichText::new("no driver").small().weak());
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
                ui.checkbox(&mut self.trace.i2c, "I²C");
                ui.checkbox(&mut self.trace.spi, "SPI");
                ui.checkbox(&mut self.trace.uart, "UART");
                ui.checkbox(&mut self.trace.gpio, "GPIO");
                ui.checkbox(&mut self.trace.decode, "Decode commands");
                ui.add(
                    egui::Slider::new(&mut self.trace.max_bytes, 8..=512)
                        .text("max bytes")
                        .logarithmic(true),
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

    fn serial_console(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Serial");
                ui.checkbox(&mut self.autoscroll, "follow");
                if ui.small_button("clear").clicked() {
                    self.session.serial.clear();
                }
                if self.session.serial.trimmed > 0 {
                    ui.label(
                        RichText::new(format!(
                            "({} KiB trimmed)",
                            self.session.serial.trimmed / 1024
                        ))
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

            egui::ScrollArea::vertical()
                .stick_to_bottom(self.autoscroll)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(self.session.serial.text()).monospace(),
                        )
                        .selectable(true)
                        .wrap(),
                    );
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
