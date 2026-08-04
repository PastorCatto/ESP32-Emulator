//! A minimal QMP client.
//!
//! Only what the emulator needs: reset, pause, resume, and a clean quit. QMP
//! speaks line-delimited JSON, and the handful of commands we send have no
//! arguments, so this avoids pulling in a JSON dependency and matches on the
//! response shape instead.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Debug)]
pub enum QmpError {
    Io(std::io::Error),
    /// QEMU answered, but with an error object.
    Command { command: &'static str, detail: String },
    /// The greeting never arrived; usually QEMU is not listening yet.
    NoGreeting,
}

impl std::fmt::Display for QmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QmpError::Io(e) => write!(f, "QMP io error: {e}"),
            QmpError::Command { command, detail } => {
                write!(f, "QMP command {command:?} failed: {detail}")
            }
            QmpError::NoGreeting => write!(f, "QMP socket sent no greeting"),
        }
    }
}

impl std::error::Error for QmpError {}

impl From<std::io::Error> for QmpError {
    fn from(e: std::io::Error) -> Self {
        QmpError::Io(e)
    }
}

/// Pull the string out of `{"return": "...text..."}`, undoing JSON escapes.
///
/// Hand-rolled rather than pulling in a JSON parser: this crate needs exactly
/// one field from one message shape, and the monitor's output is the only
/// place a string ever appears.
fn extract_return_string(line: &str) -> Option<String> {
    let rest = line.split_once("\"return\"")?.1;
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let body = rest.strip_prefix('"')?;

    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    // Skip the four hex digits; monitor output is ASCII in
                    // practice, so approximating these is harmless.
                    for _ in 0..4 {
                        chars.next()?;
                    }
                    out.push('?');
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

#[derive(Debug)]
pub struct QmpClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl QmpClient {
    pub fn connect(port: u16) -> Result<Self, QmpError> {
        let addr = ("127.0.0.1", port)
            .to_socket_addrs()?
            .next()
            .ok_or(QmpError::NoGreeting)?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;

        let writer = stream.try_clone()?;
        let mut client = QmpClient {
            reader: BufReader::new(stream),
            writer,
        };

        // QEMU greets with a capabilities banner, and ignores every command
        // until we leave negotiation mode.
        let greeting = client.read_line()?;
        if !greeting.contains("QMP") {
            return Err(QmpError::NoGreeting);
        }
        client.command("qmp_capabilities")?;
        Ok(client)
    }

    fn read_line(&mut self) -> Result<String, QmpError> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(QmpError::NoGreeting);
        }
        Ok(line)
    }

    /// Send a no-argument command and wait for its result, skipping the
    /// asynchronous events QEMU interleaves with replies.
    fn command(&mut self, name: &'static str) -> Result<String, QmpError> {
        writeln!(self.writer, "{{\"execute\":\"{name}\"}}")?;
        self.writer.flush()?;

        for _ in 0..32 {
            let line = self.read_line()?;
            if line.contains("\"error\"") {
                return Err(QmpError::Command {
                    command: name,
                    detail: line.trim().to_string(),
                });
            }
            if line.contains("\"return\"") {
                return Ok(line);
            }
            // Anything else is an event; keep reading.
        }
        Err(QmpError::Command {
            command: name,
            detail: "no result among the first 32 messages".into(),
        })
    }

    /// Run a QEMU monitor command and return its raw text output.
    ///
    /// The monitor exposes diagnostics QMP has no typed equivalent for --
    /// `info registers` above all, which is how you find out where firmware is
    /// stuck when it stops producing serial output.
    pub fn human_monitor(&mut self, command: &str) -> Result<String, QmpError> {
        // The command travels inside a JSON string, so quotes and backslashes
        // have to be escaped or the frame is malformed.
        let escaped: String = command
            .chars()
            .flat_map(|c| match c {
                '"' => vec!['\\', '"'],
                '\\' => vec!['\\', '\\'],
                '\n' => vec!['\\', 'n'],
                c => vec![c],
            })
            .collect();
        writeln!(
            self.writer,
            "{{\"execute\":\"human-monitor-command\",\"arguments\":{{\"command-line\":\"{escaped}\"}}}}"
        )?;
        self.writer.flush()?;

        for _ in 0..32 {
            let line = self.read_line()?;
            if line.contains("\"error\"") {
                return Err(QmpError::Command {
                    command: "human-monitor-command",
                    detail: line.trim().to_string(),
                });
            }
            if let Some(text) = extract_return_string(&line) {
                return Ok(text);
            }
        }
        Err(QmpError::Command {
            command: "human-monitor-command",
            detail: "no result among the first 32 messages".into(),
        })
    }

    /// Reset the machine, as the physical reset button would.
    pub fn reset(&mut self) -> Result<(), QmpError> {
        self.command("system_reset").map(drop)
    }

    /// Halt the CPUs without tearing the machine down.
    pub fn pause(&mut self) -> Result<(), QmpError> {
        self.command("stop").map(drop)
    }

    pub fn resume(&mut self) -> Result<(), QmpError> {
        self.command("cont").map(drop)
    }

    /// Ask QEMU to exit cleanly, flushing any buffered writes to the flash
    /// image. QEMU may close the socket before replying, which is success.
    pub fn quit(&mut self) -> Result<(), QmpError> {
        match self.command("quit") {
            Ok(_) => Ok(()),
            Err(QmpError::Io(_)) | Err(QmpError::NoGreeting) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::extract_return_string;

    #[test]
    fn pulls_monitor_text_out_of_a_reply() {
        let line = r#"{"return": "PC 0x42001234\r\nAR0 0x00000001\r\n"}"#;
        assert_eq!(
            extract_return_string(line).unwrap(),
            "PC 0x42001234\r\nAR0 0x00000001\r\n"
        );
    }

    #[test]
    fn handles_an_empty_return_and_escaped_quotes() {
        assert_eq!(extract_return_string(r#"{"return": ""}"#).unwrap(), "");
        assert_eq!(
            extract_return_string(r#"{"return": "say \"hi\""}"#).unwrap(),
            r#"say "hi""#
        );
    }

    #[test]
    fn ignores_messages_that_are_not_returns() {
        assert_eq!(extract_return_string(r#"{"event": "RESET"}"#), None);
        // An object-valued return is not monitor text.
        assert_eq!(extract_return_string(r#"{"return": {}}"#), None);
    }

    #[test]
    fn unterminated_string_is_not_treated_as_complete() {
        assert_eq!(extract_return_string(r#"{"return": "oops"#), None);
    }
}
