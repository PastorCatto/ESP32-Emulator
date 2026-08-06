//! Device models that hang off the emulated SoC's buses.
//!
//! Each is an ordinary [`vpb::Peripheral`], gated behind a Cargo feature so a
//! build carries only what it needs. Nothing here knows about QEMU: a device
//! sees bus transactions and answers them, whether those arrive over the
//! socket from the emulator or from a test.

#[cfg(feature = "sdcard")]
pub mod sdcard;

#[cfg(feature = "sdcard")]
pub use sdcard::SdCard;

#[cfg(test)]
mod tests {
    use super::sdcard::{SdCard, BLOCK_LEN};
    use vpb::registry::EventQueue;
    use vpb::{Claim, Peripheral, Response, Transaction};

    fn card(tag: &str) -> (SdCard, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "esp32emu-sd-{tag}-{}-{:?}.img",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        SdCard::create_if_missing(&path, 2).expect("create image");
        let c = SdCard::open(&path, Claim::Spi { controller: 2, cs: 5 }).expect("open");
        (c, path)
    }

    /// Clock `bytes` through the card and collect what comes back.
    fn xfer(card: &mut SdCard, bytes: &[u8], read: usize) -> Vec<u8> {
        let mut ev = EventQueue::default();
        let tx = Transaction::SpiTransfer {
            controller: 2,
            cs: 5,
            dc: None,
            mosi: bytes.to_vec(),
            read_len: read as u32,
        };
        match card.transact(&tx, &mut ev) {
            Response::Data { bytes } => bytes,
            _ => Vec::new(),
        }
    }

    /// Send a command frame and return the bytes clocked out while polling.
    fn command(card: &mut SdCard, cmd: u8, arg: u32, poll: usize) -> Vec<u8> {
        let a = arg.to_be_bytes();
        let frame = [0x40 | cmd, a[0], a[1], a[2], a[3], 0x95];
        xfer(card, &frame, 6);
        xfer(card, &vec![0xff; poll], poll)
    }

    /// Strip the idle bytes a host clocks while waiting for a response.
    fn first_response(bytes: &[u8]) -> Option<u8> {
        bytes.iter().copied().find(|&b| b != 0xff)
    }

    #[test]
    fn reset_reports_idle() {
        let (mut c, path) = card("reset");
        let r = command(&mut c, 0, 0, 8);
        assert_eq!(first_response(&r), Some(0x01), "CMD0 should answer R1 idle");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn send_if_cond_echoes_the_check_pattern() {
        // This is the command that was failing with 0x108 against no card.
        // Echoing the pattern back is what identifies us as an SD v2 card.
        let (mut c, path) = card("ifcond");
        command(&mut c, 0, 0, 8);
        let r = command(&mut c, 8, 0x1aa, 10);
        let start = r.iter().position(|&b| b != 0xff).expect("a response");
        assert_eq!(r[start], 0x01, "R1 idle");
        assert_eq!(r[start + 4], 0xaa, "check pattern must come back");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_full_init_sequence_leaves_idle() {
        let (mut c, path) = card("init");
        command(&mut c, 0, 0, 8);
        command(&mut c, 8, 0x1aa, 10);

        // The host polls CMD55 + ACMD41 until the card reports ready.
        command(&mut c, 55, 0, 8);
        let r = command(&mut c, 41, 0x4000_0000, 8);
        assert_eq!(first_response(&r), Some(0x00), "ACMD41 should report ready");
        assert!(c.initialised);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_ocr_reports_high_capacity() {
        let (mut c, path) = card("ocr");
        command(&mut c, 0, 0, 8);
        let r = command(&mut c, 58, 0, 10);
        let start = r.iter().position(|&b| b != 0xff).expect("a response");
        // CCS set means block addressing, which the read path assumes.
        assert_eq!(r[start + 1] & 0x40, 0x40, "CCS should be set");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_written_block_reads_back() {
        let (mut c, path) = card("rw");
        command(&mut c, 0, 0, 8);

        // Write block 3.
        let payload: Vec<u8> = (0..BLOCK_LEN).map(|i| (i % 251) as u8).collect();
        command(&mut c, 24, 3, 2);
        let mut frame = vec![0xfe];
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&[0, 0]);
        xfer(&mut c, &frame, frame.len());
        // Let the accept token and busy byte drain.
        xfer(&mut c, &[0xff; 4], 4);

        // Read it back.
        let r = command(&mut c, 17, 3, BLOCK_LEN + 8);
        let token = r.iter().position(|&b| b == 0xfe).expect("start-block token");
        assert_eq!(&r[token + 1..token + 1 + BLOCK_LEN], &payload[..]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn writes_reach_the_image_on_disk() {
        let (mut c, path) = card("persist");
        command(&mut c, 0, 0, 8);
        command(&mut c, 24, 1, 2);
        let mut frame = vec![0xfe];
        frame.extend_from_slice(&[0xab; BLOCK_LEN]);
        frame.extend_from_slice(&[0, 0]);
        xfer(&mut c, &frame, frame.len());
        xfer(&mut c, &[0xff; 4], 4);
        drop(c);

        // The image is the card, so firmware formatting it leaves a real
        // filesystem behind for the next run.
        let raw = std::fs::read(&path).expect("read image");
        assert_eq!(&raw[BLOCK_LEN..BLOCK_LEN + 4], &[0xab; 4]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_past_the_end_return_zeros_rather_than_failing() {
        let (mut c, path) = card("oob");
        command(&mut c, 0, 0, 8);
        let blocks = c.capacity_blocks();
        let r = command(&mut c, 17, blocks + 10, BLOCK_LEN + 8);
        let token = r.iter().position(|&b| b == 0xfe).expect("start-block token");
        assert!(r[token + 1..token + 1 + 16].iter().all(|&b| b == 0));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_unknown_command_is_refused_not_ignored() {
        let (mut c, path) = card("illegal");
        let r = command(&mut c, 37, 0, 8);
        assert_eq!(first_response(&r), Some(0x04), "illegal command bit");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn commands_are_decoded_for_the_tracer() {
        let (c, path) = card("decode");
        let tx = Transaction::SpiTransfer {
            controller: 2,
            cs: 5,
            dc: None,
            mosi: vec![0x48, 0, 0, 1, 0xaa, 0x87],
            read_len: 0,
        };
        assert_eq!(
            c.decode(&tx, &Response::None).as_deref(),
            Some("CMD8 SEND_IF_COND")
        );
        let _ = std::fs::remove_file(path);
    }
}
