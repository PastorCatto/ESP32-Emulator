//! The wire protocol for out-of-process peripherals.
//!
//! A custom driver is any process that connects to the emulator's device socket
//! and speaks these frames. It needs no Rust and no rebuild of the emulator.
//!
//! # Framing
//!
//! ```text
//! +----------------+-----------------+------------------+---------------+
//! | body_len : u32 | header_len : u32| header : JSON    | payload : raw |
//! +----------------+-----------------+------------------+---------------+
//!   little-endian     little-endian     header_len bytes   rest of body
//! ```
//!
//! `body_len` counts everything after itself. Bulk bytes ride as a raw payload
//! rather than inside the JSON, so pixel data costs no encoding overhead and a
//! driver in any language can memcpy it.
//!
//! # Conversation
//!
//! 1. Emulator sends [`HostMessage::Hello`].
//! 2. Driver replies [`DeviceMessage::Register`] naming the addresses it owns.
//! 3. Emulator sends [`HostMessage::Transact`] as the firmware touches those
//!    addresses. The driver answers [`DeviceMessage::Response`] for any
//!    transaction with `expects_reply`, and may send [`DeviceMessage::Event`]
//!    at any time to raise an interrupt or log.

use crate::{Claim, Event, Response, Transaction};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Bump when a change would break existing drivers.
pub const PROTOCOL_VERSION: u32 = 1;

/// Refuse absurd frames rather than trying to allocate for them; a 320x240
/// 16bpp full-frame write is 150 KiB, so this is generous.
pub const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Hello {
        protocol: u32,
        emulator: String,
        /// Board id from the loaded TOML, so a driver can adapt to the board.
        board: String,
    },
    Transact {
        /// Correlates a reply with its request. Absent when no reply is wanted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        #[serde(flatten)]
        transaction: Transaction,
    },
    /// Emulated time advanced.
    Tick { elapsed_us: u64 },
    /// Emulator is going away.
    Goodbye,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceMessage {
    Register {
        /// Matches the `kind` field in a board TOML.
        kind: String,
        protocol: u32,
        claims: Vec<Claim>,
    },
    Response {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        #[serde(flatten)]
        response: Response,
    },
    Event {
        #[serde(flatten)]
        event: Event,
    },
}

#[derive(Debug)]
pub enum WireError {
    Io(io::Error),
    Json(serde_json::Error),
    /// A frame claimed a length we refuse to allocate.
    FrameTooLarge(usize),
    /// header_len exceeded the body it lives in.
    Malformed(&'static str),
    /// Driver announced a protocol we do not implement.
    VersionMismatch { theirs: u32, ours: u32 },
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Io(e) => write!(f, "io error: {e}"),
            WireError::Json(e) => write!(f, "malformed header: {e}"),
            WireError::FrameTooLarge(n) => {
                write!(f, "frame of {n} bytes exceeds the {MAX_FRAME_LEN}-byte limit")
            }
            WireError::Malformed(m) => write!(f, "malformed frame: {m}"),
            WireError::VersionMismatch { theirs, ours } => {
                write!(f, "driver speaks protocol {theirs}, emulator speaks {ours}")
            }
        }
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        WireError::Io(e)
    }
}

impl From<serde_json::Error> for WireError {
    fn from(e: serde_json::Error) -> Self {
        WireError::Json(e)
    }
}

/// Write one frame: a JSON header plus an opaque payload.
pub fn write_frame<W: Write>(w: &mut W, header: &[u8], payload: &[u8]) -> Result<(), WireError> {
    let body_len = 4 + header.len() + payload.len();
    if body_len > MAX_FRAME_LEN {
        return Err(WireError::FrameTooLarge(body_len));
    }
    w.write_all(&(body_len as u32).to_le_bytes())?;
    w.write_all(&(header.len() as u32).to_le_bytes())?;
    w.write_all(header)?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// Read one frame, returning the header and payload halves.
pub fn read_frame<R: Read>(r: &mut R) -> Result<(Vec<u8>, Vec<u8>), WireError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let body_len = u32::from_le_bytes(len_buf) as usize;
    if body_len > MAX_FRAME_LEN {
        return Err(WireError::FrameTooLarge(body_len));
    }
    if body_len < 4 {
        return Err(WireError::Malformed("body shorter than its header-length field"));
    }

    let mut body = vec![0u8; body_len];
    r.read_exact(&mut body)?;
    let header_len = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if header_len > body_len - 4 {
        return Err(WireError::Malformed("header length exceeds frame body"));
    }
    let header = body[4..4 + header_len].to_vec();
    let payload = body[4 + header_len..].to_vec();
    Ok((header, payload))
}

/// Encode a host message, splitting bulk bytes out of the JSON.
pub fn send_host<W: Write>(w: &mut W, msg: &HostMessage) -> Result<(), WireError> {
    let payload: &[u8] = match msg {
        HostMessage::Transact { transaction, .. } => transaction.payload(),
        _ => &[],
    };
    let header = serde_json::to_vec(msg)?;
    write_frame(w, &header, payload)
}

/// Decode a host message, reattaching the payload to the transaction.
pub fn recv_host<R: Read>(r: &mut R) -> Result<HostMessage, WireError> {
    let (header, payload) = read_frame(r)?;
    let mut msg: HostMessage = serde_json::from_slice(&header)?;
    if let HostMessage::Transact { transaction, .. } = &mut msg {
        attach_payload(transaction, payload);
    }
    Ok(msg)
}

pub fn send_device<W: Write>(w: &mut W, msg: &DeviceMessage) -> Result<(), WireError> {
    let payload: Vec<u8> = match msg {
        DeviceMessage::Response { response, .. } => response.payload().to_vec(),
        DeviceMessage::Event { event: Event::UartRx { data, .. } } => data.clone(),
        _ => Vec::new(),
    };
    let header = serde_json::to_vec(msg)?;
    write_frame(w, &header, &payload)
}

pub fn recv_device<R: Read>(r: &mut R) -> Result<DeviceMessage, WireError> {
    let (header, payload) = read_frame(r)?;
    let mut msg: DeviceMessage = serde_json::from_slice(&header)?;
    match &mut msg {
        DeviceMessage::Response { response: Response::Data { bytes }, .. } => *bytes = payload,
        DeviceMessage::Event { event: Event::UartRx { data, .. } } => *data = payload,
        _ => {}
    }
    Ok(msg)
}

fn attach_payload(tx: &mut Transaction, payload: Vec<u8>) {
    match tx {
        Transaction::SpiTransfer { mosi, .. } => *mosi = payload,
        Transaction::I2cWrite { data, .. } => *data = payload,
        Transaction::UartTx { data, .. } => *data = payload,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn spi_transfer_round_trips_with_its_payload() {
        let msg = HostMessage::Transact {
            id: Some(7),
            transaction: Transaction::SpiTransfer {
                controller: 2,
                cs: 12,
                dc: Some(true),
                mosi: vec![0xde, 0xad, 0xbe, 0xef],
                read_len: 0,
            },
        };
        let mut buf = Vec::new();
        send_host(&mut buf, &msg).unwrap();
        assert_eq!(recv_host(&mut Cursor::new(buf)).unwrap(), msg);
    }

    #[test]
    fn bulk_payload_is_not_json_encoded() {
        // 150 KiB of pixels should cost ~150 KiB on the wire, not 4x that.
        let pixels = vec![0x5au8; 150 * 1024];
        let msg = HostMessage::Transact {
            id: None,
            transaction: Transaction::SpiTransfer {
                controller: 2,
                cs: 12,
                dc: Some(true),
                mosi: pixels.clone(),
                read_len: 0,
            },
        };
        let mut buf = Vec::new();
        send_host(&mut buf, &msg).unwrap();
        assert!(
            buf.len() < pixels.len() + 512,
            "framing overhead was {} bytes",
            buf.len() - pixels.len()
        );
    }

    #[test]
    fn device_registration_round_trips() {
        let msg = DeviceMessage::Register {
            kind: "my-custom-oled".into(),
            protocol: PROTOCOL_VERSION,
            claims: vec![
                Claim::Spi { controller: 2, cs: 7 },
                Claim::Gpio { pin: 21 },
            ],
        };
        let mut buf = Vec::new();
        send_device(&mut buf, &msg).unwrap();
        assert_eq!(recv_device(&mut Cursor::new(buf)).unwrap(), msg);
    }

    #[test]
    fn response_data_round_trips() {
        let msg = DeviceMessage::Response {
            id: Some(1),
            response: Response::data(vec![1, 2, 3]),
        };
        let mut buf = Vec::new();
        send_device(&mut buf, &msg).unwrap();
        assert_eq!(recv_device(&mut Cursor::new(buf)).unwrap(), msg);
    }

    #[test]
    fn several_frames_stream_back_to_back() {
        let mut buf = Vec::new();
        for i in 0..4u64 {
            send_host(&mut buf, &HostMessage::Tick { elapsed_us: i }).unwrap();
        }
        let mut cur = Cursor::new(buf);
        for i in 0..4u64 {
            assert_eq!(recv_host(&mut cur).unwrap(), HostMessage::Tick { elapsed_us: i });
        }
    }

    #[test]
    fn oversized_frame_is_refused_without_allocating() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(u32::MAX).to_le_bytes());
        let err = read_frame(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(err, WireError::FrameTooLarge(_)));
    }

    #[test]
    fn header_longer_than_body_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&8u32.to_le_bytes()); // body_len
        buf.extend_from_slice(&999u32.to_le_bytes()); // header_len, a lie
        buf.extend_from_slice(&[0; 4]);
        let err = read_frame(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(err, WireError::Malformed(_)));
    }

    #[test]
    fn truncated_stream_reports_io_error_not_panic() {
        let mut buf = Vec::new();
        send_host(&mut buf, &HostMessage::Goodbye).unwrap();
        buf.truncate(buf.len() - 2);
        assert!(matches!(
            recv_host(&mut Cursor::new(buf)).unwrap_err(),
            WireError::Io(_)
        ));
    }
}
