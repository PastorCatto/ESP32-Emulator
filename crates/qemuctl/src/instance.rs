//! A running QEMU process, and the serial stream to and from it.

use crate::{LaunchConfig, Qemu, QemuError, QmpClient};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

/// Forward everything a serial port produces until it closes.
///
/// Raw bytes rather than lines: firmware output is not reliably
/// line-buffered, and waiting for a newline would make prompts and partial
/// output invisible.
fn pump(mut reader: impl Read, port: usize, tx: &Sender<SerialChunk>) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = SerialChunk { port, bytes: buf[..n].to_vec() };
                if tx.send(chunk).is_err() {
                    break; // receiver gone; nobody is listening
                }
            }
            Err(_) => break,
        }
    }
}

/// Bytes read from one of the emulated serial ports.
#[derive(Debug, Clone)]
pub struct SerialChunk {
    /// Which port produced them, indexed as the machine wires them: on an
    /// ESP32-S3, 0 and 1 are UART0 and UART1, and 2 is the USB Serial/JTAG
    /// console.
    pub port: usize,
    pub bytes: Vec<u8>,
}

/// A live emulator process.
#[derive(Debug)]
pub struct Instance {
    child: Child,
    stdin: Option<ChildStdin>,
    serial: Receiver<SerialChunk>,
    /// Write halves of the socket-backed serial ports, once connected.
    serial_out: Vec<Arc<Mutex<Option<TcpStream>>>>,
    /// Anything QEMU wrote to stderr, kept for diagnosing a failed launch.
    stderr: Arc<Mutex<String>>,
    qmp_port: Option<u16>,
    exited: Option<Option<i32>>,
}

impl Instance {
    /// Start QEMU.
    ///
    /// Serial is piped rather than inherited so the shell can render it; that
    /// also means the reader thread must keep draining, or QEMU will block on
    /// a full pipe once firmware becomes chatty.
    pub fn spawn(qemu: &Qemu, config: &LaunchConfig) -> Result<Self, QemuError> {
        // A locally built QEMU cannot find its ROM images without -L, so fill
        // the directory in from wherever the binary was located. Left to the
        // caller, this is a step everyone forgets exactly once.
        let mut config = config.clone();
        if config.data_dir.is_none() {
            config.data_dir = qemu.data_dir.clone();
        }

        // Bind before spawning: the emulator dials out at startup and does not
        // retry, and binding afterwards would race the connection.
        let listeners: Vec<TcpListener> = (0..config.serial_count)
            .map(|_| TcpListener::bind(("127.0.0.1", 0)))
            .collect::<std::io::Result<_>>()
            .map_err(QemuError::Io)?;
        config.serial_ports = listeners
            .iter()
            .map(|l| l.local_addr().map(|a| a.port()))
            .collect::<std::io::Result<_>>()
            .map_err(QemuError::Io)?;

        let args = config.to_args()?;

        let mut child = Command::new(&qemu.binary)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(QemuError::Io)?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");

        let (tx, serial) = mpsc::channel();

        // Port 0 on stdio when no sockets were asked for, so the simple case
        // stays a plain pipe.
        if listeners.is_empty() {
            let tx = tx.clone();
            thread::Builder::new()
                .name("qemu-serial".into())
                .spawn(move || pump(stdout, 0, &tx))
                .map_err(QemuError::Io)?;
        }

        let mut serial_out = Vec::with_capacity(listeners.len());
        for (port, listener) in listeners.into_iter().enumerate() {
            let writer = Arc::new(Mutex::new(None));
            let sink = Arc::clone(&writer);
            let tx = tx.clone();
            thread::Builder::new()
                .name(format!("qemu-serial{port}"))
                .spawn(move || {
                    // One connection per port, for the life of the machine.
                    let Ok((stream, _)) = listener.accept() else { return };
                    let Ok(out) = stream.try_clone() else { return };
                    if let Ok(mut slot) = sink.lock() {
                        *slot = Some(out);
                    }
                    pump(stream, port, &tx);
                    // Dropped so a write after shutdown fails loudly rather
                    // than disappearing into a dead socket.
                    if let Ok(mut slot) = sink.lock() {
                        *slot = None;
                    }
                })
                .map_err(QemuError::Io)?;
            serial_out.push(writer);
        }

        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_sink = Arc::clone(&stderr);
        thread::Builder::new()
            .name("qemu-stderr".into())
            .spawn(move || {
                let mut reader = stderr_pipe;
                let mut text = String::new();
                let _ = reader.read_to_string(&mut text);
                if let Ok(mut guard) = stderr_sink.lock() {
                    guard.push_str(&text);
                }
            })
            .map_err(QemuError::Io)?;

        Ok(Instance {
            child,
            stdin,
            serial,
            serial_out,
            stderr,
            qmp_port: config.qmp_port,
            exited: None,
        })
    }

    /// Take everything the serial ports have produced since the last call.
    ///
    /// Non-blocking, so the UI can call it every frame without stalling.
    /// Chunks arrive in the order they were read, which is what makes a merged
    /// view of several ports readable.
    pub fn read_serial(&mut self) -> Vec<SerialChunk> {
        let mut out = Vec::new();
        // Both Empty and Disconnected mean "nothing more right now"; a
        // disconnected channel is handled by is_running, not here.
        while let Ok(chunk) = self.serial.try_recv() {
            out.push(chunk);
        }
        out
    }

    /// Send bytes to a serial port, as if typed into a terminal.
    pub fn write_serial(&mut self, port: usize, bytes: &[u8]) -> std::io::Result<()> {
        let closed = || {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "serial input is closed")
        };

        // No sockets means the single stdio port, whatever index was asked for.
        if self.serial_out.is_empty() {
            let stdin = self.stdin.as_mut().ok_or_else(closed)?;
            stdin.write_all(bytes)?;
            return stdin.flush();
        }

        let slot = self.serial_out.get(port).ok_or_else(closed)?;
        let mut guard = slot.lock().map_err(|_| closed())?;
        let stream = guard.as_mut().ok_or_else(closed)?;
        stream.write_all(bytes)?;
        stream.flush()
    }

    /// Has the process finished? Reaps it if so, so we do not leave a zombie.
    pub fn is_running(&mut self) -> bool {
        if self.exited.is_some() {
            return false;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exited = Some(status.code());
                false
            }
            Ok(None) => true,
            // Treat an unreadable status as dead rather than spinning on it.
            Err(_) => {
                self.exited = Some(None);
                false
            }
        }
    }

    pub fn exit_code(&self) -> Option<Option<i32>> {
        self.exited
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Connect to the QMP control socket, if one was configured.
    pub fn qmp(&self) -> Option<Result<QmpClient, crate::QmpError>> {
        self.qmp_port.map(QmpClient::connect)
    }

    /// Stop the machine.
    ///
    /// Tries QMP first: QEMU may hold buffered writes to the flash image, and
    /// killing it outright can lose them. Falls back to a kill when QMP is
    /// unavailable or unresponsive.
    pub fn shutdown(&mut self) {
        if !self.is_running() {
            return;
        }
        if let Some(Ok(mut qmp)) = self.qmp() {
            if qmp.quit().is_ok() {
                for _ in 0..50 {
                    if !self.is_running() {
                        return;
                    }
                    thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.exited = Some(None);
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Never leave an orphaned emulator behind holding the flash image open.
        self.shutdown();
    }
}
