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

use vpb::trace::{TraceConfig, TraceRecord};
use vpb::wire::{self, DeviceMessage, HostMessage};
use vpb::{Response, Transaction};

fn main() -> std::io::Result<()> {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5559);

    let listener = TcpListener::bind(("127.0.0.1", port))?;
    eprintln!("vpb: listening on 127.0.0.1:{port}");

    for stream in listener.incoming() {
        let stream = stream?;
        stream.set_nodelay(true)?;
        eprintln!("vpb: emulator connected");

        if let Err(e) = serve(stream) {
            eprintln!("vpb: connection ended: {e}");
        }
    }
    Ok(())
}

fn serve(stream: std::net::TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    // Trace everything: the whole point here is to see the traffic.
    let cfg = TraceConfig {
        max_bytes: 24,
        ..TraceConfig::all()
    };

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

        // Absent-device behaviour: zeros back, which is what this board's
        // shared MISO actually reads.
        let response = match &transaction {
            Transaction::SpiTransfer { read_len, .. } if *read_len > 0 => {
                Response::data(vec![0u8; *read_len as usize])
            }
            _ => Response::None,
        };

        println!(
            "{}",
            TraceRecord::build(&transaction, &response, Some("listen"), None, &cfg)
        );

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
