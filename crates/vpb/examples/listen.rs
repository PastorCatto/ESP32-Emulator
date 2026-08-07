//! A peripheral server that logs what the emulated SoC puts on its buses.
//!
//! Answers every transaction as an absent device would, so firmware keeps
//! running, and prints what it saw. This is the quickest way to find out what
//! a driver actually does before writing a model of the chip it is talking to.
//!
//! Usage: cargo run -p vpb --example listen -- [port]
//!
//! Then start the emulator with the controllers pointed at it:
//!   -global ssi.esp32s3.gpspi.vpb-port=5559

use std::io::{BufReader, BufWriter, Write};
use std::net::TcpListener;

use vpb::registry::{EventQueue, Registry};
use vpb::trace::TraceConfig;
use vpb::wire::{self, DeviceMessage, HostMessage};
use vpb::{Response, Transaction};

fn main() -> std::io::Result<()> {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5559);

    // Optional second argument: a raw SD image to attach at the T-Deck's
    // chip select. Without it the bus is empty and every read answers as
    // absent hardware.
    let mut registry = Registry::new();
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

    // Optional third argument: where to drop the display contents when the
    // emulator disconnects. There is no window yet, so a file is how you find
    // out whether the panel is being driven correctly.
    let snapshot = std::env::args().nth(3);
    // The T-Deck's panel is wired inverted (`invert = true` in its board
    // file), which is why its driver sends INVON and leaves it on.
    let screen = devices::st7789::Screen::handle(320, 240, true);
    registry
        .register(Box::new(devices::St7789::new(
            vpb::Claim::Spi { controller: 2, cs: 0 },
            screen.clone(),
        )))
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let listener = TcpListener::bind(("127.0.0.1", port))?;
    eprintln!("vpb: listening on 127.0.0.1:{port}");

    for stream in listener.incoming() {
        let stream = stream?;
        stream.set_nodelay(true)?;
        eprintln!("vpb: emulator connected");

        if let Err(e) = serve(stream, &mut registry) {
            eprintln!("vpb: connection ended: {e}");
        }

        if let Some(path) = &snapshot {
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
        }
    }
    Ok(())
}

fn serve(stream: std::net::TcpStream, registry: &mut Registry) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    // Trace everything: the whole point here is to see the traffic. The
    // registry does the tracing itself, so each line names the device that
    // actually answered and carries that device's own decode -- "CASET
    // x=0..319" rather than five hex bytes attributed to whatever was
    // registered first.
    registry.set_trace(TraceConfig {
        max_bytes: 24,
        ..TraceConfig::all()
    });

    let mut count: u64 = 0;
    loop {
        let msg = match wire::recv_host(&mut reader) {
            Ok(m) => m,
            // A closed connection is how a run ends, not an error worth noise.
            Err(wire::WireError::Io(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                eprintln!("vpb: emulator disconnected after {count} transactions");
                return Ok(());
            }
            Err(e) => {
                eprintln!("vpb: {e}");
                return Ok(());
            }
        };

        let HostMessage::Transact { id, transaction } = msg else {
            continue;
        };
        count += 1;

        // Route to whichever device claimed the address; anything unclaimed
        // answers as absent hardware would.
        let mut sink = EventQueue::default();
        let response = registry.dispatch(&transaction, &mut sink);
        let response = match (&response, &transaction) {
            (Response::None, Transaction::SpiTransfer { read_len, .. }) if *read_len > 0 => {
                Response::data(vec![0u8; *read_len as usize])
            }
            _ => response,
        };

        for event in sink.drain() {
            if let vpb::Event::Trace(record) = event {
                println!("{record}");
            }
        }

        if let Some(id) = id {
            wire::send_device(
                &mut writer,
                &DeviceMessage::Response { id: Some(id), response },
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?;
            writer.flush()?;
        }
    }
}
