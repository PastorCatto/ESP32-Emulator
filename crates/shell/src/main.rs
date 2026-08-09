//! The ESP32 emulator application.

// Do not pop a console window alongside the GUI on Windows release builds,
// but keep it in debug builds where the panic output is worth having.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod hardware;
mod screen;
mod terminal;
mod session;

/// Files named on the command line, classified the same way a drop is.
///
/// Dragging things in is the intended way to use this, but a path argument is
/// what makes the app usable from a shell, a desktop "open with", and a script
/// that wants to reproduce a run.
///
/// Usage: esp32-emulator [firmware.bin] [card.img] [board.toml] [--run] [--bypass]
fn main() -> eframe::Result<()> {
    let mut paths = Vec::new();
    let mut autostart = false;
    let mut bypass = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--run" => autostart = true,
            // What the "Bypass + boot" button does, so a scripted run and a
            // clicked one exercise the same path.
            "--bypass" => {
                bypass = true;
                autostart = true;
            }
            _ => paths.push(std::path::PathBuf::from(arg)),
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("ESP32 Emulator"),
        ..Default::default()
    };

    eframe::run_native(
        "ESP32 Emulator",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::with_files(cc, &paths, autostart, bypass)))),
    )
}
