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

#[cfg(feature = "st7789")]
pub mod st7789;

#[cfg(feature = "st7789")]
pub use st7789::St7789;

#[cfg(feature = "gt911")]
pub mod gt911;

#[cfg(feature = "gt911")]
pub use gt911::Gt911;

#[cfg(feature = "tdeck-keyboard")]
pub mod keyboard;

#[cfg(feature = "tdeck-keyboard")]
pub use keyboard::TdeckKeyboard;

pub mod png;

#[cfg(all(test, feature = "gt911", feature = "tdeck-keyboard"))]
mod i2c_tests {
    use super::gt911::Geometry;
    use super::{Gt911, TdeckKeyboard};
    use vpb::input::{PointerPhase, Rotation};
    use vpb::registry::EventQueue;
    use vpb::{Claim, Peripheral, Transaction};

    fn touch_panel() -> Gt911 {
        Gt911::new(
            vec![Claim::I2c { controller: 0, address: 0x5d, alt: None }],
            Geometry::new(320, 240).rotated(Rotation::None),
        )
    }

    fn write(d: &mut impl Peripheral, bytes: &[u8]) {
        let mut ev = EventQueue::default();
        d.transact(
            &Transaction::I2cWrite {
                controller: 0,
                address: 0x5d,
                data: bytes.to_vec(),
                stop: true,
            },
            &mut ev,
        );
    }

    fn read(d: &mut impl Peripheral, len: u32) -> Vec<u8> {
        let mut ev = EventQueue::default();
        d.transact(
            &Transaction::I2cRead { controller: 0, address: 0x5d, len },
            &mut ev,
        )
        .payload()
        .to_vec()
    }

    #[test]
    fn the_product_id_is_what_every_driver_matches_on() {
        let mut g = touch_panel();
        // Register address is big-endian, unlike everything else on this chip.
        write(&mut g, &[0x81, 0x40]);
        assert_eq!(read(&mut g, 4), b"911\0");
    }

    #[test]
    fn a_read_split_across_transfers_continues_from_the_cursor() {
        // The S3's command list caps how many bytes one transfer carries, so
        // ESP-IDF splits a 4-byte read into 3 and 1. Restarting at the seek
        // address instead would return "911" twice and never the terminator.
        let mut g = touch_panel();
        write(&mut g, &[0x81, 0x40]);
        assert_eq!(read(&mut g, 3), b"911");
        assert_eq!(read(&mut g, 1), b"\0");
    }

    #[test]
    fn resolution_is_reported_little_endian() {
        let mut g = touch_panel();
        write(&mut g, &[0x81, 0x46]);
        assert_eq!(read(&mut g, 4), [0x40, 0x01, 0xf0, 0x00], "320 then 240");
    }

    #[test]
    fn an_idle_panel_reports_ready_with_no_points() {
        // Not "not ready": a driver polling for the buffer-ready bit before
        // believing the count would wait forever.
        let mut g = touch_panel();
        write(&mut g, &[0x81, 0x4e]);
        assert_eq!(read(&mut g, 1), [0x80]);
    }

    #[test]
    fn a_press_shows_up_as_one_point_at_panel_coordinates() {
        let mut g = touch_panel();
        g.touch()
            .lock()
            .unwrap()
            .pointer(PointerPhase::Press, 80.0, 60.0, 320.0, 240.0);

        write(&mut g, &[0x81, 0x4e]);
        assert_eq!(read(&mut g, 1), [0x81], "ready, one point");

        write(&mut g, &[0x81, 0x4f]);
        let p = read(&mut g, 8);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 80);
        assert_eq!(u16::from_le_bytes([p[3], p[4]]), 60);
    }

    #[test]
    fn the_point_stays_until_the_host_clears_the_status() {
        // The host acknowledges by writing zero. Dropping the point before
        // that loses touches whenever a poll lands mid-read.
        let mut g = touch_panel();
        g.touch()
            .lock()
            .unwrap()
            .pointer(PointerPhase::Press, 10.0, 10.0, 320.0, 240.0);

        write(&mut g, &[0x81, 0x4e]);
        assert_eq!(read(&mut g, 1), [0x81]);
        write(&mut g, &[0x81, 0x4e]);
        assert_eq!(read(&mut g, 1), [0x81], "still there, unacknowledged");

        write(&mut g, &[0x81, 0x4e, 0x00]);
        g.touch().lock().unwrap().pointer(PointerPhase::Release, 10.0, 10.0, 320.0, 240.0);
        // The release is still reported once -- see TouchState::take_report.
        write(&mut g, &[0x81, 0x4e]);
        assert_eq!(read(&mut g, 1), [0x81]);
        write(&mut g, &[0x81, 0x4e, 0x00]);
        write(&mut g, &[0x81, 0x4e]);
        assert_eq!(read(&mut g, 1), [0x80], "and then nothing");
    }

    #[test]
    fn the_configuration_block_reads_back_what_was_written() {
        // Drivers write the config and re-read it to confirm; the T-Deck's
        // corrects the resolution that way.
        let mut g = touch_panel();
        write(&mut g, &[0x80, 0x48, 0x40, 0x01, 0xf0, 0x00]);
        write(&mut g, &[0x80, 0x48]);
        assert_eq!(read(&mut g, 4), [0x40, 0x01, 0xf0, 0x00]);
    }

    #[test]
    fn the_keyboard_reports_zero_when_nothing_is_pressed() {
        let mut kb = TdeckKeyboard::new(vec![Claim::I2c {
            controller: 0,
            address: 0x55,
            alt: None,
        }]);
        assert_eq!(read(&mut kb, 1), [0]);

        TdeckKeyboard::press(kb.keys(), 'k');
        assert_eq!(read(&mut kb, 1), [b'k']);
        assert_eq!(read(&mut kb, 1), [0], "and it is consumed");
    }

    #[test]
    fn the_keyboard_drops_what_it_could_not_express() {
        // One byte per key on the real part, so there is nowhere to put this.
        let mut kb = TdeckKeyboard::new(vec![Claim::I2c {
            controller: 0,
            address: 0x55,
            alt: None,
        }]);
        TdeckKeyboard::press(kb.keys(), 'é');
        assert_eq!(read(&mut kb, 1), [0]);
    }
}

#[cfg(all(test, feature = "st7789"))]
mod display_tests {
    use super::st7789::{Screen, ScreenHandle};
    use super::St7789;
    use vpb::registry::EventQueue;
    use vpb::{Claim, Peripheral, Transaction};

    fn panel() -> (St7789, ScreenHandle) {
        let screen = Screen::handle(8, 4, false);
        (St7789::new(Claim::Spi { controller: 2, cs: 0 }, screen.clone()), screen)
    }

    /// One transfer with the data/command line at `dc`.
    fn send(p: &mut St7789, dc: bool, bytes: &[u8]) {
        let mut ev = EventQueue::default();
        p.transact(
            &Transaction::SpiTransfer {
                controller: 2,
                cs: 0,
                dc: Some(dc),
                mosi: bytes.to_vec(),
                read_len: 0,
            },
            &mut ev,
        );
    }

    fn window(p: &mut St7789, x: (u16, u16), y: (u16, u16)) {
        send(p, false, &[0x2a]);
        send(p, true, &[(x.0 >> 8) as u8, x.0 as u8, (x.1 >> 8) as u8, x.1 as u8]);
        send(p, false, &[0x2b]);
        send(p, true, &[(y.0 >> 8) as u8, y.0 as u8, (y.1 >> 8) as u8, y.1 as u8]);
    }

    fn rgb(screen: &ScreenHandle) -> Vec<u8> {
        screen.lock().expect("screen").rgb888()
    }

    #[test]
    fn pixels_land_inside_the_address_window() {
        let (mut p, screen) = panel();
        window(&mut p, (2, 3), (1, 2));
        send(&mut p, false, &[0x2c]);
        // Four pixels fill the 2x2 window row by row.
        send(&mut p, true, &[0xf8, 0x00, 0x07, 0xe0, 0x00, 0x1f, 0xff, 0xff]);

        let rgb = rgb(&screen);
        let at = |x: usize, y: usize| {
            let i = (y * 8 + x) * 3;
            [rgb[i], rgb[i + 1], rgb[i + 2]]
        };
        assert_eq!(at(2, 1), [255, 0, 0], "red");
        assert_eq!(at(3, 1), [0, 255, 0], "green");
        assert_eq!(at(2, 2), [0, 0, 255], "blue");
        assert_eq!(at(3, 2), [255, 255, 255], "white");
        // Nothing outside the window was touched.
        assert_eq!(at(0, 0), [0, 0, 0]);
        assert_eq!(at(4, 1), [0, 0, 0]);
    }

    #[test]
    fn a_pixel_split_across_two_transfers_is_reassembled() {
        // The driver chunks by buffer size, not by pixel count, so the two
        // halves of a pixel routinely arrive in different transfers.
        let (mut p, screen) = panel();
        window(&mut p, (0, 1), (0, 0));
        send(&mut p, false, &[0x2c]);
        send(&mut p, true, &[0xf8]);
        send(&mut p, true, &[0x00, 0x07, 0xe0]);

        let rgb = rgb(&screen);
        assert_eq!(&rgb[0..3], &[255, 0, 0], "first pixel spans the boundary");
        assert_eq!(&rgb[3..6], &[0, 255, 0]);
    }

    #[test]
    fn writes_wrap_at_the_window_edge_not_the_panel_edge() {
        let (mut p, screen) = panel();
        window(&mut p, (5, 6), (0, 1));
        send(&mut p, false, &[0x2c]);
        // Three pixels: two fill row 0, the third wraps to row 1 column 5.
        send(&mut p, true, &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);

        let rgb = rgb(&screen);
        let lit = |x: usize, y: usize| rgb[(y * 8 + x) * 3] == 255;
        assert!(lit(5, 0) && lit(6, 0));
        assert!(lit(5, 1), "wrapped to the window's first column");
        assert!(!lit(7, 0), "did not run past the window");
    }

    #[test]
    fn madctl_bgr_swaps_the_colour_order() {
        let (mut p, screen) = panel();
        send(&mut p, false, &[0x36]);
        send(&mut p, true, &[0x08]);
        window(&mut p, (0, 0), (0, 0));
        send(&mut p, false, &[0x2c]);
        send(&mut p, true, &[0xf8, 0x00]);

        // The same bits that read as red in RGB order are blue in BGR.
        assert_eq!(&rgb(&screen)[0..3], &[0, 0, 255]);
    }

    #[test]
    fn a_command_byte_is_not_mistaken_for_a_pixel() {
        // This is the whole reason the data/command line is carried on the
        // transaction: 0x2c is both RAMWR and a perfectly good pixel byte.
        let (mut p, screen) = panel();
        window(&mut p, (0, 1), (0, 0));
        send(&mut p, false, &[0x2c]);
        send(&mut p, true, &[0x2c, 0x2c]);

        let before = screen.lock().expect("screen").generation;
        send(&mut p, false, &[0x2c]);
        assert_eq!(
            screen.lock().expect("screen").generation,
            before,
            "a command wrote no pixels"
        );
    }

    #[test]
    fn invon_against_an_inverted_panel_cancels_out() {
        // The T-Deck's glass is wired inverted, so its driver sends INVON and
        // leaves it on -- the controller's inversion is what makes the picture
        // look right. Counting only the register produced a photo negative of
        // a correctly driven display: black text on white, when the firmware
        // had drawn white text on black.
        let screen = Screen::handle(1, 1, true);
        let mut p = St7789::new(Claim::Spi { controller: 2, cs: 0 }, screen.clone());

        window(&mut p, (0, 0), (0, 0));
        send(&mut p, false, &[0x2c]);
        send(&mut p, true, &[0x00, 0x00]);
        assert_eq!(&rgb(&screen)[0..3], &[255, 255, 255], "panel alone inverts");

        send(&mut p, false, &[0x21]);
        assert_eq!(
            &rgb(&screen)[0..3],
            &[0, 0, 0],
            "INVON cancels the panel, so a black pixel shows black"
        );

        send(&mut p, false, &[0x20]);
        assert_eq!(&rgb(&screen)[0..3], &[255, 255, 255], "INVOFF inverts again");
    }

    #[test]
    fn the_panel_reports_whether_the_driver_turned_it_on() {
        let (mut p, screen) = panel();
        let on = || screen.lock().expect("screen").on;
        assert!(!on());
        send(&mut p, false, &[0x29]);
        assert!(on());
        send(&mut p, false, &[0x28]);
        assert!(!on());
    }
}

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

    /// Send a command and poll for its response in one transfer.
    ///
    /// This mirrors what ESP-IDF actually does, confirmed from a bus trace:
    /// the six command bytes and the R1 poll share a single chip-select
    /// window, and any data block is read in a *separate* transfer after it.
    fn command(card: &mut SdCard, cmd: u8, arg: u32, poll: usize) -> Vec<u8> {
        let a = arg.to_be_bytes();
        let mut frame = vec![0x40 | cmd, a[0], a[1], a[2], a[3], 0x95];
        frame.extend(std::iter::repeat_n(0xff, poll));
        let out = xfer(card, &frame, frame.len());
        // Drop the command bytes; the response is in the polling tail.
        out[6.min(out.len())..].to_vec()
    }

    /// Read a data block the previous command left waiting.
    fn read_data(card: &mut SdCard, len: usize) -> Vec<u8> {
        xfer(card, &vec![0xff; len], len)
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

        // Read it back. The command and its R1 share one transfer; the data
        // block arrives in the next, which is what the host actually does.
        command(&mut c, 17, 3, 8);
        let r = read_data(&mut c, BLOCK_LEN + 8);
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
        command(&mut c, 17, blocks + 10, 8);
        let r = read_data(&mut c, BLOCK_LEN + 8);
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
    fn the_block_crc_matches_what_esp_idf_computes() {
        // Taken from a real failure booting PURR OS against this model:
        //   E sdspi_host: data CRC failed, got=0xf49a expected=0x0000
        //   I sdspi_host: 40 0e 00 32 5b 59 00 00 00 7f 7f 80 0a 40 00 01
        // Those are the descriptor bytes we sent and the CRC IDF derived from
        // them, so this pins the polynomial and seed against a second
        // implementation rather than against itself.
        //
        // The two constants below look byte-swapped because they are. IDF's
        // sdspi_crc16 bswaps its result into "the on-the-wire format", then
        // compares it against the trailing bytes read back with memcpy on a
        // little-endian core -- so the value it prints is the wire order read
        // backwards. Sending the plain CRC big-endian is what lines up.
        let descriptor = [
            0x40, 0x0e, 0x00, 0x32, 0x5b, 0x59, 0x00, 0x00,
            0x00, 0x7f, 0x7f, 0x80, 0x0a, 0x40, 0x00, 0x01,
        ];
        assert_eq!(SdCard::crc16(&descriptor), 0x9af4);
        assert_eq!(SdCard::crc16(&descriptor).to_be_bytes(), [0x9a, 0xf4]);
    }

    #[test]
    fn a_descriptor_block_carries_its_crc() {
        // CRC checking is off until the host sends CMD59, but ESP-IDF sends it
        // during init and then verifies every block it reads. Zero bytes here
        // failed the very first one and the card never mounted.
        let (mut c, path) = card("crc");
        command(&mut c, 0, 0, 8);
        command(&mut c, 10, 0, 8);
        let r = read_data(&mut c, 32);

        let token = r.iter().position(|&b| b == 0xfe).expect("start-block token");
        let payload = &r[token + 1..token + 17];
        let sent = u16::from_be_bytes([r[token + 17], r[token + 18]]);

        assert_ne!(sent, 0, "a zero CRC is what the host rejected");
        assert_eq!(sent, SdCard::crc16(payload));
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
