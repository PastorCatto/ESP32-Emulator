//! The ESP32 emulator application.

// Do not pop a console window alongside the GUI on Windows release builds,
// but keep it in debug builds where the panic output is worth having.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod terminal;
mod session;

fn main() -> eframe::Result<()> {
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
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
