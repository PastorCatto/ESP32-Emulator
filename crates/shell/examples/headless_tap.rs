//! Boot a firmware with no window, tap the touch screen on a schedule, and take screenshots.
//!
//! Like `headless`, plus taps, so a run can open an app and click through its menu. Written for
//! chasing a crash after a tap (the serial log is printed at the end, panic output included).
//!
//! Usage:
//!   BOARD_TOML=<board.toml> HEADLESS_TAPS='60:160:120,75:100:60' HEADLESS_SHOTS=55,70,90 \
//!     cargo run -p shell --example headless_tap -- <flash.bin> [seconds] [out.png]
//!
//! HEADLESS_CMD=<file>: the run also reads that file as it goes. Each new line is a command:
//!   tap X Y        tap the screen (320x240 view)
//!   shot NAME      write <out>-NAME.png
//!   drag X1 Y1 X2 Y2   drag (scroll) from one point to another
//!   type TEXT      type on the emulated keyboard (\n is Enter)
//!   serial TEXT    send TEXT and Enter to serial port 0
//!   dump           write the serial log so far to <out>-serial.txt
//!   quit           stop (the final screenshot and serial log are written)
//!
//! A tap is `second:x:y`, x and y in the 320x240 view of the screen (landscape, as the user sees it).
//! HEADLESS_SERIAL='cmd\ncmd\n' sends lines to serial port 0 from HEADLESS_SERIAL_AT seconds (default 30).
//! VPB_TRACE=i2c,spi VPB_TRACE_FILE=trace.log writes the bus trace, each record stamped with seconds since boot.
//! ESP32_QEMU_ARGS='-d guest_errors,unimp -D qemu.log' passes flags to QEMU (guest errors and register-level logs).
#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use vpb::input::PointerPhase;

#[path = "../src/hardware.rs"]
mod hardware;
#[path = "../src/session.rs"]
mod session;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let flash = PathBuf::from(args.next().ok_or("usage: headless_tap <flash.bin> [seconds] [out.png]")?);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);
    let out = args.next().unwrap_or_else(|| "tap.png".into());

    let board_path = std::env::var("BOARD_TOML").map_err(|_| "set BOARD_TOML to a board file")?;
    let board = boards::Board::from_toml(&std::fs::read_to_string(board_path)?)?;
    let mut sess = session::Session::new(board);

    let run_dir = std::env::temp_dir().join("esp32-emulator-headless-tap");
    sess.load_firmware(&flash, &run_dir)?;
    sess.boot()?;
    if let Some(hw) = &sess.hardware {
        println!("attached: {}", hw.attached.join(", "));
        println!("not modelled: {}", hw.unmodelled.join(", "));
        println!("keyboard queue: {}", if hw.keys.is_some() { "yes" } else { "no" });
    }

    let mut taps: Vec<(u64, f32, f32)> = std::env::var("HEADLESS_TAPS")
        .map(|s| {
            s.split(',')
                .filter_map(|t| {
                    let mut p = t.trim().split(':');
                    Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?))
                })
                .collect()
        })
        .unwrap_or_default();
    taps.sort_by_key(|t| t.0);
    let mut next_tap = 0usize;
    let mut release_at: Option<std::time::Instant> = None;

    let mut shots: Vec<u64> = std::env::var("HEADLESS_SHOTS")
        .map(|s| s.split(',').filter_map(|v| v.trim().parse().ok()).collect())
        .unwrap_or_default();
    shots.sort_unstable();
    let mut next_shot = 0usize;

    let serial_lines: Vec<String> = std::env::var("HEADLESS_SERIAL")
        .map(|s| s.split("\\n").filter(|l| !l.is_empty()).map(|l| format!("{l}\r")).collect())
        .unwrap_or_default();
    let serial_at: u64 = std::env::var("HEADLESS_SERIAL_AT").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
    let mut serial_sent = 0usize;

    // VPB_TRACE=i2c,spi turns the bus tracer on for those buses and
    // VPB_TRACE_FILE=<path> says where the records go (default vpb-trace.log in
    // the current directory). An spi trace includes every framebuffer push, so
    // expect it to be large; i2c alone is small.
    let mut trace_file = None;
    if let (Ok(buses), Some(hw)) = (std::env::var("VPB_TRACE"), &sess.hardware) {
        let has = |name: &str| buses.split(',').any(|b| b.trim() == name);
        hw.set_trace(vpb::trace::TraceConfig {
            i2c: has("i2c"),
            spi: has("spi"),
            uart: has("uart"),
            gpio: has("gpio"),
            decode: true,
            max_bytes: 24,
        });
        let path = std::env::var("VPB_TRACE_FILE").unwrap_or_else(|_| "vpb-trace.log".into());
        trace_file = std::fs::File::create(&path)
            .map_err(|e| eprintln!("trace file {path}: {e}"))
            .ok();
        println!("bus trace ({buses}) -> {path}");
    }

    let cmd_path = std::env::var("HEADLESS_CMD").ok();
    let mut cmd_done = 0usize;
    let mut quit = false;

    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline && !quit {
        sess.pump();
        if let Some(path) = &cmd_path {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            while cmd_done < lines.len() {
                let line = lines[cmd_done].trim();
                cmd_done += 1;
                let mut w = line.splitn(2, ' ');
                let (verb, rest) = (w.next().unwrap_or(""), w.next().unwrap_or(""));
                let stem = out.trim_end_matches(".png").to_string();
                match verb {
                    "tap" => {
                        let mut n = rest.split_whitespace().filter_map(|v| v.parse::<f32>().ok());
                        if let (Some(x), Some(y), Some(touch)) = (n.next(), n.next(), sess.hardware.as_ref().and_then(|h| h.touch.clone())) {
                            if let Ok(mut t) = touch.lock() {
                                t.pointer(PointerPhase::Press, x, y, 320.0, 240.0);
                            }
                            std::thread::sleep(Duration::from_millis(300));
                            sess.pump();
                            if let Ok(mut t) = touch.lock() {
                                t.pointer(PointerPhase::Release, 0.0, 0.0, 320.0, 240.0);
                            }
                        }
                    }
                    "shot" => {
                        if let Some(screen) = sess.screen() {
                            let s = screen.lock().expect("screen");
                            let png = devices::png::encode_rgb(s.width.into(), s.height.into(), &s.rgb888());
                            let _ = std::fs::write(format!("{stem}-{rest}.png"), png);
                        }
                    }
                    "drag" => {
                        let n: Vec<f32> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
                        if let (4, Some(touch)) = (n.len(), sess.hardware.as_ref().and_then(|h| h.touch.clone())) {
                            let steps = 8;
                            if let Ok(mut t) = touch.lock() {
                                t.pointer(PointerPhase::Press, n[0], n[1], 320.0, 240.0);
                            }
                            for i in 1..=steps {
                                std::thread::sleep(Duration::from_millis(60));
                                sess.pump();
                                let f = i as f32 / steps as f32;
                                if let Ok(mut t) = touch.lock() {
                                    t.pointer(PointerPhase::Drag, n[0] + (n[2] - n[0]) * f, n[1] + (n[3] - n[1]) * f, 320.0, 240.0);
                                }
                            }
                            std::thread::sleep(Duration::from_millis(120));
                            sess.pump();
                            if let Ok(mut t) = touch.lock() {
                                t.pointer(PointerPhase::Release, 0.0, 0.0, 320.0, 240.0);
                            }
                        }
                    }
                    "type" => {
                        // `\n` is Enter. Says what it did, so a run where typing has no
                        // effect shows whether the keys never left here or the guest
                        // never read them (the i2c trace shows `key 'a'` for a read).
                        if let Some(keys) = sess.hardware.as_ref().and_then(|h| h.keys.clone()) {
                            let mut sent = 0;
                            for ch in rest.replace("\\n", "\r").chars() {
                                devices::TdeckKeyboard::press(&keys, ch);
                                sent += 1;
                                for _ in 0..3 {
                                    sess.pump();
                                    std::thread::sleep(Duration::from_millis(50));
                                }
                            }
                            let left = keys.lock().map(|q| q.len()).unwrap_or(0);
                            println!("type: {sent} keys at {}s, {left} still unread by the guest", started.elapsed().as_secs());
                        } else {
                            println!("type: this board has no keyboard device, nothing sent");
                        }
                    }
                    "serial" => sess.send_serial(0, &format!("{rest}\r")),
                    "dump" => {
                        let _ = std::fs::write(format!("{stem}-serial.txt"), sess.serial.view(None).text());
                    }
                    "quit" => quit = true,
                    _ => {}
                }
            }
        }
        let touch = sess.hardware.as_ref().and_then(|h| h.touch.clone());
        if let Some(touch) = touch {
            if next_tap < taps.len() && release_at.is_none() && started.elapsed() >= Duration::from_secs(taps[next_tap].0) {
                let (_, x, y) = taps[next_tap];
                if let Ok(mut t) = touch.lock() {
                    t.pointer(PointerPhase::Press, x, y, 320.0, 240.0);
                }
                println!("tap {x},{y} at {}s", started.elapsed().as_secs());
                release_at = Some(std::time::Instant::now() + Duration::from_millis(300));
                next_tap += 1;
            }
            if let Some(at) = release_at {
                if std::time::Instant::now() >= at {
                    if let Ok(mut t) = touch.lock() {
                        t.pointer(PointerPhase::Release, 0.0, 0.0, 320.0, 240.0);
                    }
                    release_at = None;
                }
            }
        }
        if serial_sent < serial_lines.len() && started.elapsed() >= Duration::from_secs(serial_at + serial_sent as u64 * 3) {
            sess.send_serial(0, &serial_lines[serial_sent]);
            serial_sent += 1;
        }
        while next_shot < shots.len() && started.elapsed() >= Duration::from_secs(shots[next_shot]) {
            if let Some(screen) = sess.screen() {
                let s = screen.lock().expect("screen");
                let png = devices::png::encode_rgb(s.width.into(), s.height.into(), &s.rgb888());
                let stem = out.trim_end_matches(".png");
                let _ = std::fs::write(format!("{stem}-{}.png", shots[next_shot]), png);
            }
            next_shot += 1;
        }
        // The channel is unbounded, so it has to drain whether or not a trace
        // file is open. Each record is stamped with seconds since boot so it
        // lines up with the `tap` lines and the serial log.
        if let Some(hw) = &sess.hardware {
            for event in hw.events.try_iter() {
                if let vpb::Event::Trace(record) = event {
                    if let Some(file) = trace_file.as_mut() {
                        let _ = writeln!(file, "[{:8.3}] {record}", started.elapsed().as_secs_f32());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    if let Some(screen) = sess.screen() {
        let s = screen.lock().expect("screen");
        let png = devices::png::encode_rgb(s.width.into(), s.height.into(), &s.rgb888());
        std::fs::write(&out, png)?;
        println!("wrote {out} ({}x{})", s.width, s.height);
    }
    print!("{}", sess.serial.view(None).text());
    if let Some(inst) = &sess.instance {
        let stderr = inst.stderr();
        if !stderr.trim().is_empty() {
            println!("--- QEMU stderr ---\n{}", stderr.trim_end());
        }
    }
    sess.stop();
    Ok(())
}
