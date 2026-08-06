//! An SD card in SPI mode, backed by a raw disk image.
//!
//! Boards like the T-Deck put the card on the same SPI bus as the display, and
//! ESP-IDF's driver holds the bus lock while it probes. If nothing answers,
//! initialisation fails but the lock is never released, and the *display*
//! driver then starves — so an absent card manifests as a blank screen rather
//! than a missing filesystem. Modelling the card is what unblocks the panel.
//!
//! Only SPI mode is implemented, which is all a board wired this way can use.
//! The native 4-bit SD protocol is a different interface entirely.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use vpb::{Claim, EventSink, Peripheral, Response, Transaction};

/// SD blocks are 512 bytes in every version that matters here.
pub const BLOCK_LEN: usize = 512;

/// R1 response bits. Zero means "ready, no errors".
const R1_IDLE: u8 = 0x01;
const R1_ILLEGAL_COMMAND: u8 = 0x04;

/// Sent before a data block, and returned before one.
const TOKEN_START_BLOCK: u8 = 0xfe;
/// Data accepted, in the token returned after a write.
const TOKEN_DATA_ACCEPTED: u8 = 0x05;

/// The card is idle and there is nothing to say. A real card holds MISO high.
const IDLE_BYTE: u8 = 0xff;

/// What the card is in the middle of doing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Nothing outstanding; every clock returns 0xFF.
    Idle,
    /// Bytes queued to be clocked out, oldest first.
    Replying(Vec<u8>),
    /// Waiting for a write payload: token, data, CRC.
    AwaitingWrite { block: u32, buf: Vec<u8> },
    /// R1 has been sent; a data block is ready but not yet offered.
    ///
    /// The host reads a data block in a *separate* transfer from the command
    /// that asked for it, and finds the block by polling for the 0xFE start
    /// token. Emitting that token inside the command's own transfer puts it
    /// where the host is not looking: it consumes the token as part of the
    /// response, then polls for one that never comes again.
    DataPending(Vec<u8>),
}

#[derive(Debug)]
pub struct SdCard {
    claim: Claim,
    image: File,
    path: PathBuf,
    blocks: u32,
    /// Cards start in an idle state and only leave it after ACMD41.
    idle: bool,
    /// CMD55 sets this; the next command is an application command.
    app_cmd: bool,
    /// A command frame being clocked in: six bytes of index, argument and CRC.
    pending: Vec<u8>,
    /// A data block owed to the host, released on the next transfer.
    deferred: Option<Vec<u8>>,
    phase: Phase,
    /// True once the host has completed initialisation, purely for the UI.
    pub initialised: bool,
}

impl SdCard {
    /// Open a raw image. The file is the card: writes go straight through, so
    /// firmware that formats the card leaves a filesystem behind.
    pub fn open(path: impl AsRef<Path>, claim: Claim) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let image = OpenOptions::new().read(true).write(true).open(&path)?;
        let len = image.metadata()?.len();

        Ok(SdCard {
            claim,
            image,
            path,
            // A card reports capacity in blocks, so a trailing partial block
            // is simply not addressable.
            blocks: (len / BLOCK_LEN as u64) as u32,
            idle: true,
            app_cmd: false,
            pending: Vec::new(),
            deferred: None,
            phase: Phase::Idle,
            initialised: false,
        })
    }

    /// Create a blank image of `megabytes` if one does not already exist.
    pub fn create_if_missing(path: impl AsRef<Path>, megabytes: u64) -> std::io::Result<()> {
        let path = path.as_ref();
        if path.exists() {
            return Ok(());
        }
        let f = File::create(path)?;
        // Sparse where the filesystem allows it: a blank 1GB card should not
        // cost 1GB on disk.
        f.set_len(megabytes * 1024 * 1024)?;
        Ok(())
    }

    pub fn capacity_blocks(&self) -> u32 {
        self.blocks
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queue a response, preceded by the NCR gap a real card takes.
    ///
    /// A card does not answer on the very next clock. The spec allows one to
    /// eight idle bytes before the response, and hosts rely on it: ESP-IDF
    /// lays its command frame out as six command bytes, one NCR byte, then
    /// R1, and its search "for r1 in the buffer after 1 clocks to max 8
    /// clocks" skips the byte immediately following the command.
    ///
    /// Answering instantly is therefore *too fast to be seen* — the reply
    /// lands in the one position the host never looks at, and every command
    /// times out while the trace shows a perfectly good response.
    fn reply(&mut self, bytes: Vec<u8>) {
        let mut queued = Vec::with_capacity(bytes.len() + 1);
        queued.push(IDLE_BYTE);
        queued.extend(bytes);
        self.phase = Phase::Replying(queued);
    }

    /// Queue R1 now and the data block for the following transfer.
    fn reply_then_block(&mut self, r1: Vec<u8>, block: Vec<u8>) {
        self.reply(r1);
        self.deferred = Some(block);
    }

    /// Take the next byte the card would drive onto MISO.
    fn next_byte(&mut self) -> u8 {
        match &mut self.phase {
            Phase::Replying(queue) if !queue.is_empty() => {
                let b = queue.remove(0);
                if queue.is_empty() {
                    // A data block owed from this command becomes available
                    // only once the host starts a new transfer.
                    self.phase = match self.deferred.take() {
                        Some(block) => Phase::DataPending(block),
                        None => Phase::Idle,
                    };
                }
                b
            }
            _ => IDLE_BYTE,
        }
    }

    fn read_block(&mut self, block: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(BLOCK_LEN + 4);
        // A real card takes a variable time to fetch; a single busy byte is
        // enough for a driver that polls for the start token.
        out.push(IDLE_BYTE);
        out.push(TOKEN_START_BLOCK);

        let mut data = vec![0u8; BLOCK_LEN];
        if block < self.blocks {
            let offset = block as u64 * BLOCK_LEN as u64;
            // A read failure is reported as zeros rather than propagated: the
            // card interface has no way to say "the host's disk broke", and
            // failing the whole emulator over it would be worse.
            let _ = self
                .image
                .seek(SeekFrom::Start(offset))
                .and_then(|_| self.image.read_exact(&mut data));
        }
        out.extend_from_slice(&data);
        // CRC16, which SPI mode leaves unchecked by default.
        out.extend_from_slice(&[0, 0]);
        out
    }

    fn write_block(&mut self, block: u32, data: &[u8]) {
        if block >= self.blocks {
            return;
        }
        let offset = block as u64 * BLOCK_LEN as u64;
        let _ = self
            .image
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.image.write_all(data));
    }

    /// Handle one six-byte command frame.
    fn command(&mut self, cmd: u8, arg: u32) {
        let index = cmd & 0x3f;
        let app = std::mem::take(&mut self.app_cmd);

        match (app, index) {
            // GO_IDLE_STATE: software reset.
            (false, 0) => {
                self.idle = true;
                self.reply(vec![R1_IDLE]);
            }

            // SEND_IF_COND: the host asks whether we understand SD v2 voltage
            // signalling. Echoing the check pattern back is what marks us as
            // v2; refusing it makes the host fall back to a v1 card.
            (false, 8) => {
                let mut r = vec![R1_IDLE];
                r.extend_from_slice(&[0x00, 0x00, 0x01, (arg & 0xff) as u8]);
                self.reply(r);
            }

            // APP_CMD: the next command is an ACMD.
            (false, 55) => {
                self.app_cmd = true;
                self.reply(vec![if self.idle { R1_IDLE } else { 0 }]);
            }

            // SD_SEND_OP_COND: the host polls this until we leave idle.
            // Leaving immediately is fine; a real card takes a few hundred ms
            // and the host is written to wait either way.
            (true, 41) => {
                self.idle = false;
                self.initialised = true;
                self.reply(vec![0]);
            }

            // READ_OCR. CCS set marks a high-capacity card, which means block
            // addressing -- the same convention the read/write paths assume.
            (false, 58) => {
                let mut r = vec![if self.idle { R1_IDLE } else { 0 }];
                r.extend_from_slice(&[0xc0, 0xff, 0x80, 0x00]);
                self.reply(r);
            }

            // SET_BLOCKLEN. Fixed at 512 here, so acknowledge and ignore.
            (false, 16) => self.reply(vec![0]),

            // READ_SINGLE_BLOCK.
            (false, 17) => {
                let block = self.read_block(arg);
                self.reply_then_block(vec![0], block);
            }

            // WRITE_BLOCK: acknowledge, then take the payload that follows.
            (false, 24) => {
                self.reply(vec![0]);
                self.phase = Phase::AwaitingWrite {
                    block: arg,
                    buf: Vec::with_capacity(BLOCK_LEN + 3),
                };
            }

            // SEND_CSD / SEND_CID: a plausible descriptor is enough to get
            // past capacity detection.
            (false, 9) | (false, 10) => {
                let mut block = vec![TOKEN_START_BLOCK];
                block.extend_from_slice(&self.csd());
                block.extend_from_slice(&[0, 0]);
                self.reply_then_block(vec![0], block);
            }

            // CRC_ON_OFF and STOP_TRANSMISSION are both no-ops here.
            (false, 59) | (false, 12) => self.reply(vec![0]),

            _ => self.reply(vec![R1_ILLEGAL_COMMAND]),
        }
    }

    /// A minimal CSD version 2.0, which reports capacity in 512 KiB units.
    fn csd(&self) -> [u8; 16] {
        let mut csd = [0u8; 16];
        csd[0] = 0x40; // CSD_STRUCTURE = 1 (v2.0)
        csd[1] = 0x0e;
        csd[2] = 0x00;
        csd[3] = 0x32;
        csd[4] = 0x5b;
        csd[5] = 0x59;

        // C_SIZE, in units of 512 KiB, minus one.
        let c_size = (self.blocks / 1024).saturating_sub(1);
        csd[7] = ((c_size >> 16) & 0x3f) as u8;
        csd[8] = ((c_size >> 8) & 0xff) as u8;
        csd[9] = (c_size & 0xff) as u8;

        csd[10] = 0x7f;
        csd[11] = 0x80;
        csd[12] = 0x0a;
        csd[13] = 0x40;
        csd[14] = 0x00;
        csd[15] = 0x01;
        csd
    }

    /// Feed one byte clocked in from the host, returning what goes out.
    fn step(&mut self, incoming: u8) -> u8 {
        // A write payload swallows bytes until the block is complete.
        if let Phase::AwaitingWrite { block, buf } = &mut self.phase {
            // The host clocks idle bytes until it is ready to send the token.
            if buf.is_empty() && incoming != TOKEN_START_BLOCK {
                return IDLE_BYTE;
            }
            buf.push(incoming);

            // Token plus data plus a two-byte CRC.
            if buf.len() == BLOCK_LEN + 3 {
                let block = *block;
                let data = buf[1..=BLOCK_LEN].to_vec();
                self.write_block(block, &data);
                // Accept token, then one busy byte before going ready.
                self.reply(vec![TOKEN_DATA_ACCEPTED, 0x00, IDLE_BYTE]);
            }
            return IDLE_BYTE;
        }

        // Mid-frame: keep collecting until the six bytes are in.
        if !self.pending.is_empty() {
            self.pending.push(incoming);
            if self.pending.len() == 6 {
                let cmd = self.pending[0];
                let arg = u32::from_be_bytes([
                    self.pending[1],
                    self.pending[2],
                    self.pending[3],
                    self.pending[4],
                ]);
                self.pending.clear();
                self.command(cmd, arg);
            }
            return IDLE_BYTE;
        }

        /*
         * A command frame starts with 01 in the top two bits. Only accept one
         * when there is nothing queued to send, so a reply byte that happens
         * to look like a command is not mistaken for one.
         */
        if matches!(self.phase, Phase::Idle) && (incoming & 0xc0) == 0x40 {
            self.pending.push(incoming);
            return IDLE_BYTE;
        }

        self.next_byte()
    }
}

impl Peripheral for SdCard {
    fn kind(&self) -> &str {
        "sdcard"
    }

    fn claims(&self) -> Vec<Claim> {
        vec![self.claim.clone()]
    }

    fn transact(&mut self, tx: &Transaction, _events: &mut dyn EventSink) -> Response {
        let Transaction::SpiTransfer { mosi, read_len, .. } = tx else {
            return Response::None;
        };

        /*
         * Each transfer is one chip-select window, and a host starts a new
         * command with the chip select freshly asserted. If a previous reply
         * still has bytes queued -- because the host read less of a data block
         * than we offered -- those must not swallow the incoming command.
         *
         * Without this the card silently eats the next command frame: the
         * host sends it, receives idle bytes back, and blocks forever waiting
         * for a response to a command the card never saw. Nothing looks
         * broken from either side, which makes it very hard to spot.
         */
        if matches!(self.phase, Phase::Replying(_)) {
            if let Some(&first) = mosi.first() {
                if (first & 0xc0) == 0x40 {
                    self.phase = Phase::Idle;
                    self.deferred = None;
                }
            }
        }

        /*
         * A data block owed from the previous command becomes readable now
         * that a new transfer has begun. The host polls for the 0xFE token in
         * this transfer, which is where it is looking for it.
         */
        if let Phase::DataPending(block) = &self.phase {
            let block = block.clone();
            // A new command takes priority: the host may have given up on the
            // block, and swallowing its command would hang it.
            if mosi.first().is_some_and(|&b| (b & 0xc0) == 0x40) {
                self.phase = Phase::Idle;
            } else {
                self.phase = Phase::Replying(block);
            }
        }

        let mut miso = Vec::with_capacity(mosi.len().max(*read_len as usize));
        for &b in mosi {
            miso.push(self.step(b));
        }
        // A transfer may clock more than it sends; the extra cycles still
        // advance the card.
        while miso.len() < *read_len as usize {
            miso.push(self.step(IDLE_BYTE));
        }

        if *read_len == 0 {
            Response::None
        } else {
            miso.truncate(*read_len as usize);
            Response::data(miso)
        }
    }

    fn decode(&self, tx: &Transaction, _r: &Response) -> Option<String> {
        let cmd = tx.payload().first()?;
        if (cmd & 0xc0) != 0x40 {
            return None;
        }
        Some(match cmd & 0x3f {
            0 => "CMD0 GO_IDLE_STATE".into(),
            8 => "CMD8 SEND_IF_COND".into(),
            9 => "CMD9 SEND_CSD".into(),
            16 => "CMD16 SET_BLOCKLEN".into(),
            17 => "CMD17 READ_SINGLE_BLOCK".into(),
            24 => "CMD24 WRITE_BLOCK".into(),
            41 => "ACMD41 SD_SEND_OP_COND".into(),
            55 => "CMD55 APP_CMD".into(),
            58 => "CMD58 READ_OCR".into(),
            n => format!("CMD{n}"),
        })
    }
}
