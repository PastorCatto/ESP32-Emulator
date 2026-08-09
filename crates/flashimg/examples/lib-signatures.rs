//! Extract signatures from Espressif's prebuilt libraries, and check them
//! against real firmware.
//!
//! Usage:
//!   lib-signatures <lib-dir> [--against <flash.bin>...]
//!
//! `<lib-dir>` is an IDF (or Arduino core) `esp_wifi/lib/<chip>` directory.
//! The point of `--against` is that it is the only honest test: a signature
//! taken from an unlinked object is worth nothing until it matches a linked
//! image nobody built for us.

use flashimg::archive;

/// The radio-bypass targets, plus memcpy, which filling a scan result needs.
const WANTED: &[&str] = &[
    "esp_wifi_init",
    "esp_wifi_set_mode",
    "esp_wifi_set_config",
    "esp_wifi_start",
    "esp_wifi_stop",
    "esp_wifi_connect",
    "esp_wifi_disconnect",
    "esp_wifi_scan_start",
    "esp_wifi_scan_get_ap_num",
    "esp_wifi_scan_get_ap_records",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let split = args.iter().position(|a| a == "--against");
    let (head, images) = match split {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (&args[..], Vec::new()),
    };
    let dir = head.first().ok_or("usage: lib-signatures <lib-dir> [--against <bin>...]")?;

    let mut archives = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "a") {
            archives.push((path.clone(), std::fs::read(&path)?));
        }
    }
    archives.sort_by(|a, b| a.0.cmp(&b.0));
    println!("{} archive(s) in {dir}\n", archives.len());

    let loaded: Vec<Vec<u8>> = images
        .iter()
        .map(std::fs::read)
        .collect::<std::io::Result<_>>()?;

    println!(
        "{:<30} {:>6} {:>7} {:>8}  match in firmware",
        "symbol", "bytes", "pinned", "reloc"
    );

    for symbol in WANTED {
        let mut found = None;
        for (path, data) in &archives {
            if let Some(f) = archive::find_function(data, symbol)? {
                found = Some((path.clone(), f));
                break;
            }
        }

        let Some((path, extracted)) = found else {
            println!("{symbol:<30} {:>6} {:>7} {:>8}  -", "-", "-", "-");
            continue;
        };

        let sig = extracted.signature();
        let relocs = extracted.relocated.iter().filter(|m| **m).count();

        // The whole question: does an unlinked object recognise linked code?
        let mut verdict = String::new();
        for (name, image) in images.iter().zip(&loaded) {
            let hits = sig.scan(image);
            let short = std::path::Path::new(name)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            verdict.push_str(&format!(
                "{short}:{} ",
                match hits.len() {
                    0 => "miss".to_string(),
                    1 => format!("HIT@{:#x}", hits[0]),
                    n => format!("{n} hits"),
                }
            ));
        }

        let from = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        println!(
            "{symbol:<30} {:>6} {:>7} {:>8}  {verdict} [{from}]",
            sig.len(),
            sig.pinned_bits(),
            relocs,
        );
    }

    Ok(())
}
