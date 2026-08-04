//! Locating and launching QEMU.
//!
//! Argument construction is kept as a pure function ([`LaunchConfig::to_args`])
//! so the command line can be tested without spawning a process, which is
//! where most of the fiddly detail lives.

mod instance;
mod qmp;

pub use instance::{Instance, SerialChunk};
pub use qmp::{QmpClient, QmpError};

use flashimg::Chip;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub enum QemuError {
    /// No binary found for this chip.
    NotFound { binary: &'static str, searched: Vec<PathBuf> },
    /// The chip has no QEMU support at all — most of the family, currently.
    UnsupportedChip(Chip),
    /// The located binary does not know this machine, usually because it is
    /// upstream QEMU rather than Espressif's fork.
    UnsupportedMachine { machine: String, binary: PathBuf },
    Io(std::io::Error),
    /// QEMU exited while we were still setting up.
    Exited { status: Option<i32>, output: String },
}

impl fmt::Display for QemuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QemuError::NotFound { binary, searched } => {
                write!(f, "could not find {binary}; looked in:")?;
                for p in searched {
                    write!(f, "\n  {}", p.display())?;
                }
                Ok(())
            }
            QemuError::UnsupportedChip(c) => write!(
                f,
                "{c} has no QEMU machine; only ESP32, ESP32-S3, and ESP32-C3 are emulated"
            ),
            QemuError::UnsupportedMachine { machine, binary } => write!(
                f,
                "{} does not support machine {machine:?}; this looks like upstream QEMU rather than Espressif's fork",
                binary.display()
            ),
            QemuError::Io(e) => write!(f, "io error: {e}"),
            QemuError::Exited { status, output } => {
                write!(f, "QEMU exited early")?;
                if let Some(s) = status {
                    write!(f, " with status {s}")?;
                }
                if !output.is_empty() {
                    write!(f, ":\n{output}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for QemuError {}

impl From<std::io::Error> for QemuError {
    fn from(e: std::io::Error) -> Self {
        QemuError::Io(e)
    }
}

/// A located QEMU binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Qemu {
    pub binary: PathBuf,
}

impl Qemu {
    /// Find a QEMU able to run `chip`.
    ///
    /// Search order puts the vendored copy ahead of `PATH`, so the version we
    /// ship and test against wins over whatever a developer happens to have
    /// installed. `ESP32_EMULATOR_QEMU` overrides everything, for working
    /// against a locally built fork.
    pub fn locate(chip: Chip) -> Result<Self, QemuError> {
        let binary = chip.qemu_binary().ok_or(QemuError::UnsupportedChip(chip))?;
        let exe = if cfg!(windows) {
            format!("{binary}.exe")
        } else {
            binary.to_string()
        };

        let mut searched = Vec::new();

        if let Some(dir) = std::env::var_os("ESP32_EMULATOR_QEMU") {
            let p = PathBuf::from(dir);
            // Accept either the binary itself or the directory holding it.
            for cand in [p.clone(), p.join(&exe), p.join("bin").join(&exe)] {
                if cand.is_file() {
                    return Ok(Qemu { binary: cand });
                }
                searched.push(cand);
            }
        }

        for root in Self::vendor_roots() {
            // Accept both the normalised layout and the shape Espressif's
            // tarball unpacks into, so a hand-extracted archive still works.
            for cand in [
                root.join("qemu").join("bin").join(&exe),
                root.join("qemu").join("qemu").join("bin").join(&exe),
            ] {
                if cand.is_file() {
                    return Ok(Qemu { binary: cand });
                }
                searched.push(cand);
            }
        }

        if let Some(found) = Self::search_path(&exe) {
            return Ok(Qemu { binary: found });
        }
        searched.push(PathBuf::from(format!("$PATH/{exe}")));

        Err(QemuError::NotFound { binary, searched })
    }

    /// Places a vendored QEMU might live: next to our executable when
    /// installed, or in the repo when developing.
    fn vendor_roots() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            // target/debug/shell.exe -> repo root is three levels up.
            let mut dir = exe.parent().map(Path::to_path_buf);
            for _ in 0..4 {
                let Some(d) = dir else { break };
                roots.push(d.join("vendor"));
                dir = d.parent().map(Path::to_path_buf);
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            roots.push(cwd.join("vendor"));
        }
        roots
    }

    fn search_path(exe: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|d| d.join(exe))
            .find(|c| c.is_file())
    }

    /// Use a specific binary, skipping the search.
    pub fn at(binary: impl Into<PathBuf>) -> Self {
        Qemu { binary: binary.into() }
    }

    pub fn version(&self) -> Result<String, QemuError> {
        let out = Command::new(&self.binary).arg("--version").output()?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string())
    }

    /// Check the binary actually has the machine we need, so an upstream QEMU
    /// on `PATH` produces a clear message instead of a confusing failure.
    pub fn supports_machine(&self, machine: &str) -> Result<bool, QemuError> {
        let out = Command::new(&self.binary).args(["-M", "help"]).output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .any(|l| l.split_whitespace().next() == Some(machine)))
    }
}

/// How to start the machine.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    pub chip: Chip,
    /// Full flash image, written as the MTD backing store.
    pub flash_image: PathBuf,
    /// Enable QEMU's own framebuffer window. Off for us: the SPI display is
    /// rendered by the shell, and a second window would only confuse.
    pub graphics: bool,
    /// Expose a QMP control socket on this TCP port.
    pub qmp_port: Option<u16>,
    /// Expose a GDB stub on this TCP port.
    pub gdb_port: Option<u16>,
    /// Start with the CPU halted, so a debugger can attach before reset.
    pub start_halted: bool,
    /// Escape hatch for anything we have not modelled.
    pub extra_args: Vec<String>,
}

impl LaunchConfig {
    pub fn new(chip: Chip, flash_image: impl Into<PathBuf>) -> Self {
        LaunchConfig {
            chip,
            flash_image: flash_image.into(),
            graphics: false,
            qmp_port: None,
            gdb_port: None,
            start_halted: false,
            extra_args: Vec::new(),
        }
    }

    /// Build the full argument list.
    ///
    /// Serial goes to stdio and the monitor is disabled, rather than using
    /// `-nographic`, which multiplexes both onto one stream and makes serial
    /// unusable programmatically.
    pub fn to_args(&self) -> Result<Vec<String>, QemuError> {
        let machine = self
            .chip
            .qemu_machine()
            .ok_or(QemuError::UnsupportedChip(self.chip))?;

        let mut args: Vec<String> = vec!["-machine".into()];
        if self.graphics {
            args.push(format!("{machine},graphics=on"));
        } else {
            args.push(machine.into());
            args.push("-display".into());
            args.push("none".into());
        }
        args.extend(["-serial", "stdio", "-monitor", "none"].map(String::from));

        args.push("-drive".into());
        args.push(format!(
            "file={},if=mtd,format=raw",
            self.flash_image.display()
        ));

        if let Some(port) = self.qmp_port {
            args.push("-qmp".into());
            args.push(format!("tcp:127.0.0.1:{port},server,nowait"));
        }
        if let Some(port) = self.gdb_port {
            args.push("-gdb".into());
            args.push(format!("tcp::{port}"));
        }
        if self.start_halted {
            args.push("-S".into());
        }
        args.extend(self.extra_args.iter().cloned());
        Ok(args)
    }

    /// The command that would be run, for showing in the UI and for bug reports.
    pub fn command_line(&self, qemu: &Qemu) -> String {
        let args = self.to_args().unwrap_or_default();
        let quote = |s: &str| {
            if s.contains(' ') {
                format!("\"{s}\"")
            } else {
                s.to_string()
            }
        };
        std::iter::once(quote(&qemu.binary.display().to_string()))
            .chain(args.iter().map(|a| quote(a)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LaunchConfig {
        LaunchConfig::new(Chip::Esp32S3, "flash.bin")
    }

    #[test]
    fn builds_the_command_line_verified_against_real_qemu() {
        // This exact argument set was confirmed to boot the ESP32-S3 ROM.
        assert_eq!(
            cfg().to_args().unwrap(),
            vec![
                "-machine", "esp32s3",
                "-display", "none",
                "-serial", "stdio",
                "-monitor", "none",
                "-drive", "file=flash.bin,if=mtd,format=raw",
            ]
        );
    }

    #[test]
    fn serial_is_not_multiplexed_with_the_monitor() {
        // -nographic would put the monitor and serial on one stream, which
        // makes programmatic serial unusable. Guard against it coming back.
        let args = cfg().to_args().unwrap();
        assert!(!args.iter().any(|a| a == "-nographic"));
        assert_eq!(
            args.windows(2).find(|w| w[0] == "-monitor").map(|w| &w[1]),
            Some(&"none".to_string())
        );
    }

    #[test]
    fn adds_qmp_and_gdb_ports_when_asked() {
        let mut c = cfg();
        c.qmp_port = Some(55591);
        c.gdb_port = Some(1234);
        c.start_halted = true;
        let args = c.to_args().unwrap();
        assert!(args.contains(&"tcp:127.0.0.1:55591,server,nowait".to_string()));
        assert!(args.contains(&"tcp::1234".to_string()));
        assert!(args.contains(&"-S".to_string()));
    }

    #[test]
    fn graphics_flag_does_not_leave_a_contradictory_display_setting() {
        let mut c = cfg();
        c.graphics = true;
        let args = c.to_args().unwrap();
        assert!(args.contains(&"esp32s3,graphics=on".to_string()));
        // "-display none" must be gone, not merely overridden.
        assert!(!args.windows(2).any(|w| w[0] == "-display" && w[1] == "none"));
    }

    #[test]
    fn chips_without_a_machine_are_rejected_clearly() {
        let c = LaunchConfig::new(Chip::Esp32C6, "flash.bin");
        let err = c.to_args().unwrap_err();
        assert_eq!(
            err.to_string(),
            "ESP32-C6 has no QEMU machine; only ESP32, ESP32-S3, and ESP32-C3 are emulated"
        );
    }

    #[test]
    fn maps_chips_to_the_right_qemu_binary() {
        assert_eq!(Chip::Esp32S3.qemu_binary(), Some("qemu-system-xtensa"));
        assert_eq!(Chip::Esp32.qemu_binary(), Some("qemu-system-xtensa"));
        assert_eq!(Chip::Esp32C3.qemu_binary(), Some("qemu-system-riscv32"));
        assert_eq!(Chip::Esp32P4.qemu_binary(), None);
    }

    #[test]
    fn command_line_quotes_paths_with_spaces() {
        let c = LaunchConfig::new(Chip::Esp32S3, "C:/Program Files/flash.bin");
        let line = c.command_line(&Qemu::at("qemu.exe"));
        assert!(line.contains("\"file=C:/Program Files/flash.bin,if=mtd,format=raw\""));
    }

    #[test]
    fn missing_binary_error_lists_where_it_looked() {
        let err = QemuError::NotFound {
            binary: "qemu-system-xtensa",
            searched: vec![PathBuf::from("/a/b"), PathBuf::from("/c/d")],
        };
        let msg = err.to_string();
        assert!(msg.contains("could not find qemu-system-xtensa"));
        assert!(msg.contains("/a/b"));
        assert!(msg.contains("/c/d"));
    }
}
