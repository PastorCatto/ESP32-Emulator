//! Boot a firmware through the shell's own path, with no window.
//!
//! Exists because the interesting failures are in the wiring between the
//! board file, the device models and the emulator's command line -- and none
//! of that is reachable from a test that never launches QEMU, nor checkable by
//! eye in a GUI that has to be watched in real time.
//!
//! Usage:
//!   cargo run -p shell --example headless -- <flash.bin> [seconds] [sd.img] [out.png]

// The modules come in whole; an example that used every part of the UI's
// plumbing would not be headless.
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

#[path = "../src/hardware.rs"]
mod hardware;
#[path = "../src/session.rs"]
mod session;

const T_DECK: &str = include_str!("../../../boards/t-deck-plus.toml");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let flash = PathBuf::from(args.next().ok_or("usage: headless <flash.bin> [seconds] [sd.img] [out.png]")?);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let sd_image = args.next().map(PathBuf::from);
    let out = args.next().unwrap_or_else(|| "screen.png".into());

    let board = boards::Board::from_toml(T_DECK)?;
    let mut sess = session::Session::new(board);
    sess.sd_image = sd_image;

    let run_dir = std::env::temp_dir().join("esp32-emulator-headless");
    sess.load_firmware(&flash, &run_dir)?;
    let fw = sess.firmware.clone().ok_or("firmware did not load")?;
    println!("firmware: {} {} ({} KiB)", fw.chip, fw.kind, fw.size / 1024);

    sess.boot()?;
    if let Some(hw) = &sess.hardware {
        // VPB_TRACE=i2c,spi turns the bus tracer on for those buses.
        if let Ok(buses) = std::env::var("VPB_TRACE") {
            let has = |name: &str| buses.split(',').any(|b| b.trim() == name);
            hw.set_trace(vpb::trace::TraceConfig {
                i2c: has("i2c"),
                spi: has("spi"),
                uart: has("uart"),
                gpio: has("gpio"),
                decode: true,
                max_bytes: 24,
            });
        }
        println!("bus on port {}", hw.port);
        println!("attached: {}", hw.attached.join(", "));
        println!("not modelled: {}", hw.unmodelled.join(", "));
    }

    // Serial has to be drained or the pipe fills and the guest blocks on its
    // console -- which looks like a hang somewhere much more interesting.
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    let mut traced = 0usize;
    while std::time::Instant::now() < deadline {
        sess.pump();
        // The channel is unbounded and a traced boot fills it fast, so this
        // has to drain whether or not anything is printed.
        if let Some(hw) = &sess.hardware {
            for event in hw.events.try_iter() {
                if let vpb::Event::Trace(record) = event {
                    // Bounded: a framebuffer push alone is thousands of lines.
                    if traced < 400 {
                        eprintln!("{record}");
                    }
                    traced += 1;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if traced > 0 {
        eprintln!("({traced} traced transactions)");
    }

    let connected = sess.hardware.as_ref().is_some_and(hardware::Hardware::is_connected);
    println!("bus connected: {connected}");

    if let Some(screen) = sess.screen() {
        let s = screen.lock().expect("screen");
        let png = devices::png::encode_rgb(s.width.into(), s.height.into(), &s.rgb888());
        std::fs::write(&out, png)?;
        println!(
            "wrote {out} ({}x{}, {} writes, panel {})",
            s.width,
            s.height,
            s.generation,
            if s.on { "on" } else { "off" }
        );
    } else {
        println!("no display attached");
    }

    // Every port, labelled where the source changes. On a real boot the ROM
    // writes the same banner to UART0 and the USB console, so seeing it twice
    // here is correct and is the whole reason this prints the merged view.
    let counts = sess.serial.counts();
    for (i, name) in sess.serial_port_names().iter().enumerate() {
        println!("{name}: {} bytes", counts.get(i).copied().unwrap_or(0));
    }
    print!("{}", sess.serial.view(None).text());
    sess.stop();
    Ok(())
}
