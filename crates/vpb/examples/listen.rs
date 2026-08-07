//! A peripheral server that logs what the emulated SoC puts on its buses.
//!
//! Runs the same loop the shell does, with a fixed T-Deck Plus device set and
//! everything traced. This is the quickest way to find out what a driver
//! actually does before writing a model of the chip it is talking to.
//!
//! Usage: cargo run -p vpb --example listen -- [port] [sd.img] [screen.png]
//!
//! Then start the emulator with the controllers pointed at it:
//!   -global driver=ssi.esp32s3.gpspi,property=vpb-port,value=5559
//!   -global driver=ssi.esp32s3.gpspi,property=dc-gpio,value=11

use std::net::TcpListener;

use vpb::registry::Registry;
use vpb::trace::TraceConfig;
use vpb::Event;

fn main() -> std::io::Result<()> {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5559);

    let mut registry = Registry::new();

    // Optional: a raw SD image to attach at the T-Deck's chip select line.
    // Without it the card is simply absent, which firmware handles.
    if let Some(img) = std::env::args().nth(2) {
        match devices::SdCard::open(&img, vpb::Claim::Spi { controller: 2, cs: 5 }) {
            Ok(card) => {
                eprintln!("vpb: SD card {img} ({} blocks)", card.capacity_blocks());
                registry
                    .register(Box::new(card))
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            }
            Err(e) => eprintln!("vpb: could not open {img}: {e}"),
        }
    }

    // The T-Deck's panel is wired inverted -- `invert = true` in its board
    // file -- which is why its driver sends INVON and leaves it on.
    let screen = devices::st7789::Screen::handle(320, 240, true);
    registry
        .register(Box::new(devices::St7789::new(
            vpb::Claim::Spi { controller: 2, cs: 0 },
            screen.clone(),
        )))
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    // Optional: where to drop the display contents when the emulator exits.
    let snapshot = std::env::args().nth(3);

    // Trace everything; the whole point here is to see the traffic. The
    // registry traces from inside dispatch, so each line names the device that
    // actually answered and carries that device's own decode.
    registry.set_trace(TraceConfig {
        max_bytes: 24,
        ..TraceConfig::all()
    });

    let listener = TcpListener::bind(("127.0.0.1", port))?;
    eprintln!("vpb: listening on 127.0.0.1:{port}");

    vpb::server::listen(
        &listener,
        &mut registry,
        &mut |event| {
            if let Event::Trace(record) = event {
                println!("{record}");
            }
        },
        &mut || eprintln!("vpb: emulator connected"),
        &mut |outcome| {
            match outcome {
                Ok(n) => eprintln!("vpb: emulator disconnected after {n} transactions"),
                Err(e) => eprintln!("vpb: connection ended: {e}"),
            }
            let Some(path) = &snapshot else { return };
            let s = screen.lock().expect("screen");
            let out = devices::png::encode_rgb(s.width.into(), s.height.into(), &s.rgb888());
            match std::fs::write(path, &out) {
                Ok(()) => eprintln!(
                    "vpb: wrote {path} ({}x{}, {} writes, panel {})",
                    s.width,
                    s.height,
                    s.generation,
                    if s.on { "on" } else { "off" }
                ),
                Err(e) => eprintln!("vpb: could not write {path}: {e}"),
            }
        },
    );
    Ok(())
}
