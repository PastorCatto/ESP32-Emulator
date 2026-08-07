//! A detachable serial terminal, in its own OS window.
//!
//! The console embedded in the main window is for watching a boot. This is for
//! *working*: talking to a firmware shell, poking a REPL, driving a bootloader.
//! It behaves like a real serial terminal — command history, configurable line
//! endings, local echo, control-key shortcuts, and a hex view for when the
//! other end is not sending text.

use crate::session::Session;
use egui::{Color32, RichText};

/// What to append when Enter is pressed.
///
/// This matters more than it looks. ESP-IDF's console uses linenoise, which
/// treats CR *or* LF as end-of-line, so sending CRLF submits an extra empty
/// line. `idf.py monitor` sends CR, and so do we by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    None,
    Cr,
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::None => "",
            LineEnding::Cr => "\r",
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LineEnding::None => "none",
            LineEnding::Cr => "CR",
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }

    pub const ALL: [LineEnding; 4] = [
        LineEnding::Cr,
        LineEnding::Lf,
        LineEnding::CrLf,
        LineEnding::None,
    ];
}

#[derive(Debug)]
pub struct Terminal {
    pub open: bool,
    input: String,
    /// Previously sent lines, newest last.
    history: Vec<String>,
    /// Where Up/Down currently sits in `history`; `None` means the live input.
    history_pos: Option<usize>,
    /// Stash of the in-progress line while browsing history.
    stashed: String,
    pub line_ending: LineEnding,
    pub local_echo: bool,
    pub autoscroll: bool,
    pub hex_view: bool,
    /// Which serial port typing goes to. A board can have a shell on any of
    /// them, and the one carrying the boot log is often not the one listening.
    pub port: usize,
    /// Locally echoed text, interleaved into the view.
    echo: String,
}

impl Default for Terminal {
    fn default() -> Self {
        Terminal {
            open: false,
            input: String::new(),
            history: Vec::new(),
            history_pos: None,
            stashed: String::new(),
            line_ending: LineEnding::Cr,
            local_echo: false,
            autoscroll: true,
            hex_view: false,
            // UART0 is where an application shell usually lives; the boot log
            // arriving on another port does not mean commands go there.
            port: 0,
            echo: String::new(),
        }
    }
}

impl Terminal {
    /// Draw the terminal into its own viewport.
    ///
    /// Returns false when the user closed the window.
    pub fn show(&mut self, ctx: &egui::Context, session: &mut Session) {
        if !self.open {
            return;
        }
        let viewport_id = egui::ViewportId::from_hash_of("serial-terminal");
        let builder = egui::ViewportBuilder::default()
            .with_title("Serial terminal")
            .with_inner_size([760.0, 520.0])
            .with_min_inner_size([420.0, 260.0]);

        let mut close_requested = false;
        ctx.show_viewport_immediate(viewport_id, builder, |ui, _class| {
            let running = session.is_running();
            self.contents(ui, session, running);
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                close_requested = true;
            }
        });
        if close_requested {
            self.open = false;
        }
    }

    fn contents(&mut self, ui: &mut egui::Ui, session: &mut Session, running: bool) {
        egui::Panel::top("term-top").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if running {
                    ui.colored_label(Color32::from_rgb(120, 220, 120), "● connected");
                } else {
                    ui.colored_label(Color32::GRAY, "○ not running");
                }
                ui.separator();

                // Which port this terminal is attached to: what it shows and
                // where typing goes. One port, like a real terminal -- the
                // main window's console is the place to watch all of them.
                let names = session.serial_port_names();
                let counts = session.serial.counts();
                egui::ComboBox::from_id_salt("serial-port")
                    .selected_text(names.get(self.port).copied().unwrap_or("port"))
                    .width(140.0)
                    .show_ui(ui, |ui| {
                        for (i, name) in names.iter().enumerate() {
                            // Byte counts, so a port nothing ever talks on is
                            // visibly the wrong one to be typing at.
                            let bytes = counts.get(i).copied().unwrap_or(0);
                            let label = if bytes > 0 {
                                format!("{name} ({bytes} B)")
                            } else {
                                format!("{name} (silent)")
                            };
                            ui.selectable_value(&mut self.port, i, label);
                        }
                    });
                ui.label(RichText::new("port").small().weak());

                ui.separator();
                egui::ComboBox::from_id_salt("line-ending")
                    .selected_text(self.line_ending.label())
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        for le in LineEnding::ALL {
                            ui.selectable_value(&mut self.line_ending, le, le.label());
                        }
                    });
                ui.label(RichText::new("on Enter").small().weak());

                ui.separator();
                ui.checkbox(&mut self.local_echo, "echo");
                ui.checkbox(&mut self.autoscroll, "follow");
                ui.checkbox(&mut self.hex_view, "hex");

                ui.separator();
                // Control keys a firmware shell is likely to want.
                if ui
                    .add_enabled(running, egui::Button::new("Ctrl-C"))
                    .on_hover_text("Interrupt (0x03)")
                    .clicked()
                {
                    session.send_serial(self.port, "\x03");
                }
                if ui
                    .add_enabled(running, egui::Button::new("Ctrl-D"))
                    .on_hover_text("End of transmission (0x04)")
                    .clicked()
                {
                    session.send_serial(self.port, "\x04");
                }
                if ui.button("clear").clicked() {
                    session.serial.clear();
                    self.echo.clear();
                }
            });
        });

        egui::Panel::bottom("term-input")
            .exact_size(34.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let hint = if running {
                        "type a command and press Enter"
                    } else {
                        "start the emulator to send"
                    };
                    let resp = ui.add_enabled(
                        running,
                        egui::TextEdit::singleline(&mut self.input)
                            .hint_text(hint)
                            .desired_width(ui.available_width() - 60.0)
                            .font(egui::TextStyle::Monospace),
                    );

                    if resp.has_focus() {
                        self.browse_history(ui);
                    }

                    let submit = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let clicked = ui.add_enabled(running, egui::Button::new("Send")).clicked();

                    if submit || clicked {
                        self.send(session);
                        resp.request_focus();
                    }
                });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .stick_to_bottom(self.autoscroll)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.hex_view {
                        ui.add(
                            egui::Label::new(
                                RichText::new(hex_dump(&session.serial.view(Some(self.port)).raw())).monospace(),
                            )
                            .selectable(true),
                        );
                    } else {
                        let mut body = session.serial.view(Some(self.port)).text().to_string();
                        if self.local_echo && !self.echo.is_empty() {
                            body.push_str(&self.echo);
                        }
                        ui.add(
                            egui::Label::new(RichText::new(body).monospace())
                                .selectable(true)
                                .wrap(),
                        );
                    }
                });
        });
    }

    /// Up/Down walk previously sent commands, as a shell would.
    fn browse_history(&mut self, ui: &egui::Ui) {
        let (up, down) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
            )
        });
        if self.history.is_empty() || (!up && !down) {
            return;
        }

        let pos = match (self.history_pos, up) {
            // Entering history: stash whatever is half-typed so it comes back.
            (None, true) => {
                self.stashed = std::mem::take(&mut self.input);
                Some(self.history.len() - 1)
            }
            (None, false) => None,
            (Some(0), true) => Some(0),
            (Some(p), true) => Some(p - 1),
            (Some(p), false) if p + 1 < self.history.len() => Some(p + 1),
            // Walked off the end: restore the stashed line.
            (Some(_), false) => None,
        };

        self.history_pos = pos;
        self.input = match pos {
            Some(p) => self.history[p].clone(),
            None => std::mem::take(&mut self.stashed),
        };
    }

    fn send(&mut self, session: &mut Session) {
        let line = std::mem::take(&mut self.input);
        self.history_pos = None;
        self.stashed.clear();

        // Skip consecutive duplicates, as a shell history would.
        if !line.is_empty() && self.history.last().map(String::as_str) != Some(line.as_str()) {
            self.history.push(line.clone());
            if self.history.len() > 200 {
                self.history.remove(0);
            }
        }

        if self.local_echo {
            self.echo.push_str(&line);
            self.echo.push('\n');
        }
        session.send_serial(self.port, &format!("{line}{}", self.line_ending.as_str()));
    }
}

/// Classic offset / hex / ASCII dump.
fn hex_dump(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4);
    for (row, chunk) in bytes.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", row * 16));
        for (i, b) in chunk.iter().enumerate() {
            out.push_str(&format!("{b:02x} "));
            if i == 7 {
                out.push(' ');
            }
        }
        // Pad a short final row so the ASCII column stays aligned.
        for i in chunk.len()..16 {
            out.push_str("   ");
            if i == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for b in chunk {
            out.push(if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            });
        }
        out.push_str("|\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cr_is_the_default_because_linenoise_treats_crlf_as_two_lines() {
        assert_eq!(Terminal::default().line_ending, LineEnding::Cr);
        assert_eq!(LineEnding::Cr.as_str(), "\r");
        assert_eq!(LineEnding::CrLf.as_str(), "\r\n");
        assert_eq!(LineEnding::None.as_str(), "");
    }

    #[test]
    fn hex_dump_aligns_a_short_final_row() {
        let dump = hex_dump(b"hello");
        assert_eq!(dump.lines().count(), 1);
        assert!(dump.starts_with("00000000  68 65 6c 6c 6f "));
        assert!(dump.trim_end().ends_with("|hello|"), "got {dump:?}");
    }

    #[test]
    fn hex_dump_marks_unprintable_bytes() {
        let dump = hex_dump(&[0x00, 0x1b, b'A', 0xff]);
        assert!(dump.trim_end().ends_with("|..A.|"), "got {dump:?}");
    }

    #[test]
    fn hex_dump_wraps_at_sixteen_bytes() {
        assert_eq!(hex_dump(&[0u8; 33]).lines().count(), 3);
    }
}
