//! End-to-end launch tests against a real QEMU.
//!
//! These skip rather than fail when no QEMU is available, so a checkout
//! without `vendor/` still passes `cargo test`. Run `scripts/fetch-qemu.sh` to
//! make them meaningful.

use flashimg::{Chip, FlashImage, FlashSize};
use qemuctl::{Instance, LaunchConfig, Psram, Qemu};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Locate QEMU, or skip loudly.
///
/// A test that silently passes while doing nothing is worse than one that
/// fails, so the skip is printed unmissably, and setting
/// `ESP32_EMULATOR_REQUIRE_QEMU=1` turns it into a hard failure for CI.
fn qemu_or_skip() -> Option<Qemu> {
    match Qemu::locate(Chip::Esp32S3) {
        Ok(q) => Some(q),
        Err(e) => {
            if std::env::var_os("ESP32_EMULATOR_REQUIRE_QEMU").is_some() {
                panic!("ESP32_EMULATOR_REQUIRE_QEMU is set but QEMU was not found.\n{e}");
            }
            eprintln!(
                "\n>>> SKIPPED: no QEMU found, so this test verified NOTHING.\n\
                 >>> Run scripts/fetch-qemu.sh to make it meaningful.\n{e}\n"
            );
            None
        }
    }
}

/// Write a blank flash image to a uniquely named temp file.
fn blank_flash(tag: &str) -> PathBuf {
    let img = FlashImage::blank(Chip::Esp32S3, FlashSize::MB16);
    let path = std::env::temp_dir().join(format!(
        "esp32emu-test-{tag}-{}-{:?}.bin",
        std::process::id(),
        std::thread::current().id()
    ));
    let mut f = std::fs::File::create(&path).expect("create temp flash");
    f.write_all(img.as_bytes()).expect("write temp flash");
    path
}

/// Collect serial output until `needle` appears or we run out of patience.
fn wait_for_serial(inst: &mut Instance, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut text = String::new();
    while Instant::now() < deadline {
        for chunk in inst.read_serial() {
            text.push_str(&String::from_utf8_lossy(&chunk.bytes));
        }
        if text.contains(needle) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    text
}

#[test]
fn locates_qemu_and_reports_a_version() {
    let Some(qemu) = qemu_or_skip() else { return };
    let version = qemu.version().expect("qemu --version");
    assert!(version.contains("QEMU"), "unexpected version line: {version:?}");
}

#[test]
fn located_qemu_supports_the_esp32s3_machine() {
    let Some(qemu) = qemu_or_skip() else { return };
    // Upstream QEMU would fail this, which is exactly the confusion the check
    // exists to prevent.
    assert!(
        qemu.supports_machine("esp32s3").expect("query machines"),
        "located QEMU at {} has no esp32s3 machine",
        qemu.binary.display()
    );
}

#[test]
fn boots_the_rom_bootloader_from_a_blank_flash() {
    let Some(qemu) = qemu_or_skip() else { return };
    let flash = blank_flash("rom");
    let mut inst =
        Instance::spawn(&qemu, &LaunchConfig::new(Chip::Esp32S3, &flash)).expect("spawn qemu");

    // Wait for the later of the two markers, so both assertions below are
    // checked against output that has actually arrived.
    let out = wait_for_serial(&mut inst, "invalid header", Duration::from_secs(20));
    inst.shutdown();
    let _ = std::fs::remove_file(&flash);

    assert!(
        out.contains("ESP-ROM:esp32s3"),
        "expected the S3 ROM banner, got:\n{out}"
    );
    // A blank flash has no image, and the ROM must say so rather than hanging.
    assert!(
        out.contains("invalid header"),
        "expected the ROM to reject an erased flash, got:\n{out}"
    );
}

#[test]
fn shutdown_stops_the_process() {
    let Some(qemu) = qemu_or_skip() else { return };
    let flash = blank_flash("shutdown");
    let mut inst =
        Instance::spawn(&qemu, &LaunchConfig::new(Chip::Esp32S3, &flash)).expect("spawn qemu");

    assert!(inst.is_running());
    inst.shutdown();
    assert!(!inst.is_running(), "process should be gone after shutdown");
    let _ = std::fs::remove_file(&flash);
}

/// Boot a real firmware image all the way into application code.
///
/// Opt in by pointing `ESP32_EMULATOR_TEST_FIRMWARE` at a merged flash image
/// for an ESP32-S3 board with 8MB octal PSRAM, e.g. a T-Deck build. Real
/// firmware is not in the repo, so this cannot run by default.
#[test]
fn boots_real_firmware_into_application_code() {
    let Some(qemu) = qemu_or_skip() else { return };
    let Some(src) = std::env::var_os("ESP32_EMULATOR_TEST_FIRMWARE") else {
        eprintln!("skipping: set ESP32_EMULATOR_TEST_FIRMWARE to a merged flash image");
        return;
    };
    let src = PathBuf::from(src);

    // Pad to the full device size; a merged image is usually shorter than the
    // flash it targets.
    let mut bytes = std::fs::read(&src).expect("read firmware");
    let full = FlashSize::MB16.bytes() as usize;
    assert!(bytes.len() <= full, "image is larger than 16MB");
    bytes.resize(full, 0xff);

    let flash = std::env::temp_dir().join(format!("esp32emu-real-{}.bin", std::process::id()));
    std::fs::write(&flash, &bytes).expect("write flash");

    let mut config = LaunchConfig::new(Chip::Esp32S3, &flash);
    config.psram = Some(Psram { size_mb: 8, octal: true });
    let mut inst = Instance::spawn(&qemu, &config).expect("spawn qemu");

    let out = wait_for_serial(&mut inst, "app_init: Project name", Duration::from_secs(60));
    inst.shutdown();
    let _ = std::fs::remove_file(&flash);

    // Reaching app_init means the ROM, the second-stage bootloader, our
    // partition table, PSRAM, and every segment load all worked.
    assert!(
        out.contains("Found 8MB PSRAM device"),
        "octal PSRAM was not detected, got:\n{out}"
    );
    assert!(
        out.contains("app_init: Project name"),
        "never reached application startup, got:\n{out}"
    );
    // A boot loop would show this instead of progressing.
    assert!(
        !out.contains("Failed to init external RAM"),
        "PSRAM init failed, got:\n{out}"
    );
}

#[test]
fn qmp_can_reset_a_running_machine() {
    let Some(qemu) = qemu_or_skip() else { return };
    let flash = blank_flash("qmp");
    let mut config = LaunchConfig::new(Chip::Esp32S3, &flash);
    // Port zero would be ambiguous; pick something unlikely to collide.
    config.qmp_port = Some(55_593);
    let mut inst = Instance::spawn(&qemu, &config).expect("spawn qemu");

    wait_for_serial(&mut inst, "invalid header", Duration::from_secs(20));

    let mut qmp = inst
        .qmp()
        .expect("qmp port configured")
        .expect("connect to qmp");
    qmp.reset().expect("system_reset");

    // A reset must make the ROM banner appear again. Wait for the exact string
    // asserted on, or the read can stop mid-line and fail spuriously.
    let after = wait_for_serial(&mut inst, "ESP-ROM:esp32s3", Duration::from_secs(15));
    inst.shutdown();
    let _ = std::fs::remove_file(&flash);

    assert!(
        after.contains("ESP-ROM:esp32s3"),
        "expected the ROM to run again after reset, got:\n{after}"
    );
}
