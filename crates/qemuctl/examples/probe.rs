//! Boot a flash image, let it run, then report where the CPU ended up.
//!
//! For answering "my firmware stopped printing, what is it doing?" — the
//! program counter plus the last serial output usually identifies the stall
//! immediately.
//!
//! Usage: cargo run -p qemuctl --example probe -- <flash.bin> [seconds]

use flashimg::Chip;
use qemuctl::{Instance, LaunchConfig, Psram, Qemu};
use std::time::{Duration, Instant};

fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(image) = args.next() else {
        eprintln!("usage: probe <flash.bin> [seconds]");
        return std::process::ExitCode::FAILURE;
    };
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(15);

    let qemu = match Qemu::locate(Chip::Esp32S3) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut config = LaunchConfig::new(Chip::Esp32S3, &image);
    config.psram = Some(Psram { size_mb: 8, octal: true });
    config.qmp_port = Some(55_711);

    // Point the SPI controllers at an external peripheral server, so a stall
    // that only happens with device models attached can be probed too.
    if let Ok(port) = std::env::var("ESP32_EMULATOR_VPB_PORT") {
        config.extra_args.push("-global".into());
        config
            .extra_args
            .push(format!("driver=ssi.esp32s3.gpspi,property=vpb-port,value={port}"));
    }

    let mut inst = match Instance::spawn(&qemu, &config) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("could not start QEMU: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Collect serial while it runs, noting when output stopped: the gap between
    // the last byte and the probe is what tells you it is stuck rather than slow.
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut serial = String::new();
    let mut last_output = Instant::now();
    while Instant::now() < deadline {
        let chunk = inst.read_serial();
        if !chunk.is_empty() {
            last_output = Instant::now();
            serial.push_str(&String::from_utf8_lossy(&chunk));
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let quiet_for = last_output.elapsed().as_secs_f32();
    let stripped = strip_ansi(&serial);
    println!("=== last serial lines ===");
    for line in stripped.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev() {
        println!("  {line}");
    }
    println!("\nsilent for {quiet_for:.1}s of a {secs}s run");

    println!("\n=== program counter samples ===");
    match inst.qmp() {
        Some(Ok(mut qmp)) => {
            // One sample cannot distinguish a tight spin from slow progress.
            // Several, spread out, show the shape of the loop.
            let mut seen = Vec::new();
            for _ in 0..8 {
                match qmp.human_monitor("info registers") {
                    Ok(text) => {
                        if let Some(pc) = text
                            .split_whitespace()
                            .find_map(|w| w.strip_prefix("PC="))
                        {
                            seen.push(pc.to_string());
                        }
                    }
                    Err(e) => {
                        println!("  {e}");
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(120));
            }

            let mut unique: Vec<&String> = Vec::new();
            for pc in &seen {
                if !unique.contains(&pc) {
                    unique.push(pc);
                }
            }
            for pc in &seen {
                println!("  0x{pc}");
            }
            println!(
                "\n  {} samples, {} distinct address(es){}",
                seen.len(),
                unique.len(),
                if unique.len() <= 3 && seen.len() > 3 {
                    " — a tight loop"
                } else {
                    ""
                }
            );

            // Disassembling settles what kind of stall this is: a polling loop
            // looks like a load and a branch, whereas a parked CPU sits on a
            // single `waiti`.
            if let Some(pc) = seen.first() {
                // Disassemble from before the sampled PC. A spin loop branches
                // backwards, so the instructions that matter are usually the
                // ones above where the sample landed.
                let from = u32::from_str_radix(pc, 16)
                    .map(|v| v.saturating_sub(0x30))
                    .unwrap_or(0);
                println!("\n=== instructions around the stall (PC = 0x{pc}) ===");
                match qmp.human_monitor(&format!("x/28i 0x{from:08x}")) {
                    Ok(text) => {
                        for line in text.lines().take(30) {
                            println!("  {}", line.trim_end());
                        }
                    }
                    Err(e) => println!("  {e}"),
                }
            }

            // A poll loop reads through a register held in an address
            // register, so the full dump is what identifies the peripheral.
            if let Ok(text) = qmp.human_monitor("info registers") {
                println!("\n=== address registers ===");
                for line in text.lines().filter(|l| {
                    let l = l.trim_start();
                    l.starts_with('A') && l.contains('=')
                }) {
                    println!("  {}", line.trim_end());
                }
            }

            // The second core matters: one core parked in the idle task while
            // the other works is normal, and looks nothing like a real hang.
            println!("\n=== both cores ===");
            for cpu in [0, 1] {
                let _ = qmp.human_monitor(&format!("cpu {cpu}"));
                if let Ok(text) = qmp.human_monitor("info registers") {
                    let pc = text
                        .split_whitespace()
                        .find_map(|w| w.strip_prefix("PC="))
                        .unwrap_or("?");
                    println!("  CPU#{cpu} PC=0x{pc}");
                }
            }
        }
        Some(Err(e)) => println!("  could not reach QMP: {e}"),
        None => println!("  no QMP port configured"),
    }

    inst.shutdown();
    std::process::ExitCode::SUCCESS
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
        } else if chars.peek() == Some(&'[') {
            chars.next();
            for t in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&t) {
                    break;
                }
            }
        }
    }
    out
}
