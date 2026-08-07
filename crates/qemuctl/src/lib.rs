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
    /// Directory holding the ESP ROM images, when we can find it.
    ///
    /// Espressif's release tarballs put these in `../share/qemu` next to the
    /// binary and find them without help. A locally built QEMU bakes in its
    /// install prefix instead, and without `-L` fails with "ROM code binary
    /// not found" — so we locate it ourselves rather than making every caller
    /// remember.
    pub data_dir: Option<PathBuf>,
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
                    return Ok(Qemu::from_binary(cand));
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
                    return Ok(Qemu::from_binary(cand));
                }
                searched.push(cand);
            }
        }

        if let Some(found) = Self::search_path(&exe) {
            return Ok(Qemu::from_binary(found));
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
        Qemu::from_binary(binary.into())
    }

    /// Wrap a known binary, looking for the ROM images alongside it.
    fn from_binary(binary: PathBuf) -> Self {
        let data_dir = Self::find_data_dir(&binary);
        Qemu { binary, data_dir }
    }

    /// Find the directory holding `esp32s3_rev0_rom.bin` and friends.
    ///
    /// A release tarball puts them in `../share/qemu`; a source build leaves
    /// them in the tree's `pc-bios`. Presence of an actual ROM is the test,
    /// rather than the directory merely existing, so a stale empty directory
    /// does not shadow a good one.
    fn find_data_dir(binary: &Path) -> Option<PathBuf> {
        let bin_dir = binary.parent()?;
        let candidates = [
            bin_dir.join("..").join("share").join("qemu"),
            bin_dir.join("share").join("qemu"),
            bin_dir.join("pc-bios"),
            bin_dir.join("..").join("pc-bios"),
        ];
        candidates
            .into_iter()
            .find(|d| d.join("esp32s3_rev0_rom.bin").is_file() || d.join("esp32-v3-rom.bin").is_file())
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

/// External PSRAM attached to the SoC.
///
/// Boards like the T-Deck put their framebuffer and heap in PSRAM, and their
/// firmware calls `abort()` during startup when it is missing — so getting this
/// wrong is not a degraded experience, it is a boot loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Psram {
    pub size_mb: u32,
    /// Octal (OPI) rather than quad. The S3 checks the line mode during
    /// detection, so a quad model answering an octal probe fails outright.
    pub octal: bool,
}

/// How to start the machine.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    pub chip: Chip,
    /// Full flash image, written as the MTD backing store.
    pub flash_image: PathBuf,
    /// External PSRAM, if the board has any.
    pub psram: Option<Psram>,
    /// Passed as `-L`. Filled in from the located QEMU when left unset.
    pub data_dir: Option<PathBuf>,
    /// Enable QEMU's own framebuffer window. Off for us: the SPI display is
    /// rendered by the shell, and a second window would only confuse.
    pub graphics: bool,
    /// How many serial ports to give the machine.
    ///
    /// Zero means one port on stdio, which is enough for a console and is what
    /// the tests build. Firmware does not always log where you expect: an
    /// ESP32-S3 wires UART0, UART1 and the USB Serial/JTAG console to the
    /// first three, and PURR OS puts its ROM and bootloader output on the
    /// third while its own logging goes to the first. Reading one of those and
    /// not the other looks exactly like a hang.
    pub serial_count: usize,
    /// TCP ports the serial devices connect out to, in order.
    ///
    /// Filled in by [`Instance`], which binds the listeners before spawning --
    /// the emulator connects at startup and does not retry.
    pub serial_ports: Vec<u16>,
    /// Port of the peripheral server the SPI controllers should connect to.
    /// Without it the general-purpose SPI buses look empty, which is a
    /// legitimate way to run.
    pub vpb_port: Option<u16>,
    /// GPIO carrying the display's data/command line, from the board file.
    /// An ST7789 tells a command byte from pixel data by this pin and nothing
    /// on the bus, so a display model cannot decode the stream without it.
    pub display_dc_gpio: Option<u8>,
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
            psram: None,
            serial_count: 0,
            serial_ports: Vec::new(),
            vpb_port: None,
            display_dc_gpio: None,
            data_dir: None,
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

        let mut args: Vec<String> = Vec::new();

        // Must come before anything that loads a ROM.
        if let Some(dir) = &self.data_dir {
            args.push("-L".into());
            args.push(dir.display().to_string());
        }

        args.push("-machine".into());
        if self.graphics {
            args.push(format!("{machine},graphics=on"));
        } else {
            args.push(machine.into());
            args.push("-display".into());
            args.push("none".into());
        }
        // The monitor is always off. `-nographic` would multiplex it onto the
        // serial stream, which makes serial unusable programmatically.
        args.extend(["-monitor", "none"].map(String::from));

        if self.serial_ports.is_empty() {
            args.extend(["-serial", "stdio"].map(String::from));
        } else {
            // The emulator dials out to us rather than listening, so no output
            // is lost between it starting and something connecting.
            for (i, port) in self.serial_ports.iter().enumerate() {
                args.push("-chardev".into());
                args.push(format!(
                    "socket,id=ser{i},host=127.0.0.1,port={port},server=off"
                ));
                args.push("-serial".into());
                args.push(format!("chardev:ser{i}"));
            }
        }

        args.push("-drive".into());
        args.push(format!(
            "file={},if=mtd,format=raw",
            self.flash_image.display()
        ));

        // PSRAM needs two separate settings, and supplying only one silently
        // does nothing: `-m` sets how much RAM actually exists, while the
        // global switches the modelled chip to octal. The S3 verifies the line
        // mode during detection, so both must agree.
        if let Some(psram) = self.psram {
            args.push("-m".into());
            args.push(format!("{}M", psram.size_mb));
            if psram.octal {
                args.push("-global".into());
                args.push("driver=ssi_psram,property=is_octal,value=true".into());
            }
        }

        // Both are set on the controller type rather than an instance,
        // because there is no id to address these devices by -- the machine
        // creates them. That means SPI2 and SPI3 get the same values, which is
        // fine: they are told apart by the controller number in each
        // transaction, and the machine fans the D/C line out to both.
        if let Some(port) = self.vpb_port {
            args.push("-global".into());
            args.push(format!(
                "driver=ssi.esp32s3.gpspi,property=vpb-port,value={port}"
            ));
        }
        if let Some(pin) = self.display_dc_gpio {
            args.push("-global".into());
            args.push(format!(
                "driver=ssi.esp32s3.gpspi,property=dc-gpio,value={pin}"
            ));
        }

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
    fn the_peripheral_bridge_is_set_on_both_spi_controllers() {
        // -global matches on type name and the machine creates these devices,
        // so there is no instance to address. Both controllers get the same
        // values; each transaction says which one it came from.
        let mut c = cfg();
        c.vpb_port = Some(5559);
        c.display_dc_gpio = Some(11);
        let args = c.to_args().unwrap();

        assert!(args.contains(
            &"driver=ssi.esp32s3.gpspi,property=vpb-port,value=5559".to_string()
        ));
        assert!(args.contains(
            &"driver=ssi.esp32s3.gpspi,property=dc-gpio,value=11".to_string()
        ));
    }

    #[test]
    fn no_bridge_means_no_arguments_rather_than_a_zero_port() {
        // Port 0 is "pick one for me" to the OS, so emitting it by accident
        // would point the emulator at nothing in particular.
        let args = cfg().to_args().unwrap();
        assert!(!args.iter().any(|a| a.contains("vpb-port")));
        assert!(!args.iter().any(|a| a.contains("dc-gpio")));
    }

    #[test]
    fn builds_the_command_line_verified_against_real_qemu() {
        // This exact argument set was confirmed to boot the ESP32-S3 ROM.
        assert_eq!(
            cfg().to_args().unwrap(),
            vec![
                "-machine", "esp32s3",
                "-display", "none",
                "-monitor", "none",
                "-serial", "stdio",
                "-drive", "file=flash.bin,if=mtd,format=raw",
            ]
        );
    }

    #[test]
    fn each_serial_port_gets_its_own_socket_the_emulator_dials_out_to() {
        // server=off matters: the emulator connects at startup, so nothing is
        // lost between it starting and something being ready to read. The
        // other way round, the ROM banner is gone before you can attach.
        let mut c = cfg();
        c.serial_ports = vec![7001, 7002, 7003];
        let args = c.to_args().unwrap();

        assert!(!args.contains(&"stdio".to_string()), "sockets replace stdio");
        for (i, port) in [7001, 7002, 7003].iter().enumerate() {
            assert!(args.contains(&format!(
                "socket,id=ser{i},host=127.0.0.1,port={port},server=off"
            )));
            assert!(args.contains(&format!("chardev:ser{i}")));
        }
    }

    #[test]
    fn the_ports_stay_in_order_because_the_machine_wires_them_that_way() {
        // An ESP32-S3 gives UART0, UART1 and the USB Serial/JTAG console the
        // first three chardevs in order, so which log lands where depends on
        // this sequence being stable.
        let mut c = cfg();
        c.serial_ports = vec![9001, 9002, 9003];
        let args = c.to_args().unwrap();

        let at = |needle: &str| args.iter().position(|a| a.contains(needle)).unwrap();
        assert!(at("port=9001") < at("port=9002"));
        assert!(at("port=9002") < at("port=9003"));
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
    fn octal_psram_emits_both_required_flags() {
        // Verified by hand against the vendored QEMU: this exact pair is what
        // gets real T-Deck firmware past `Failed to init external RAM`.
        // Supplying only one of them silently does nothing.
        let mut c = cfg();
        c.psram = Some(Psram { size_mb: 8, octal: true });
        let args = c.to_args().unwrap();
        assert!(args.windows(2).any(|w| w[0] == "-m" && w[1] == "8M"));
        assert!(args.contains(&"driver=ssi_psram,property=is_octal,value=true".to_string()));
    }

    #[test]
    fn quad_psram_sets_size_without_the_octal_global() {
        let mut c = cfg();
        c.psram = Some(Psram { size_mb: 4, octal: false });
        let args = c.to_args().unwrap();
        assert!(args.windows(2).any(|w| w[0] == "-m" && w[1] == "4M"));
        assert!(!args.iter().any(|a| a.contains("is_octal")));
    }

    #[test]
    fn no_psram_means_no_memory_flag_at_all() {
        // The S3 machine has no PSRAM by default, and passing -m 0M would be
        // a different thing entirely.
        let args = cfg().to_args().unwrap();
        assert!(!args.iter().any(|a| a == "-m"));
        assert!(!args.iter().any(|a| a.contains("psram")));
    }

    #[test]
    fn data_dir_becomes_a_leading_dash_l() {
        // A source-built QEMU cannot find its ROM images without this and
        // fails with "ROM code binary not found".
        let mut c = cfg();
        c.data_dir = Some(PathBuf::from("/opt/qemu/share/qemu"));
        let args = c.to_args().unwrap();
        assert_eq!(args[0], "-L");
        assert_eq!(args[1], "/opt/qemu/share/qemu");
    }

    #[test]
    fn no_data_dir_means_no_dash_l() {
        // Release tarballs find their own ROMs; passing an empty -L would
        // stop them doing so.
        assert!(!cfg().to_args().unwrap().iter().any(|a| a == "-L"));
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
