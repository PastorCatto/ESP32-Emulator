//! A WIZnet W5500 Ethernet controller on an SPI bus, in MAC-raw mode.
//!
//! The ESP32-S3 has no Ethernet MAC, so a wired board hangs this chip off SPI.
//! The point of modelling it is that ESP-IDF's own `esp_eth` W5500 driver runs
//! unchanged: it resets the chip, checks `VERSIONR`, sizes socket 0's buffers,
//! opens it in MACRAW and then moves whole Ethernet frames through the socket's
//! TX and RX memory. That is the whole surface implemented here. Sockets 1-7,
//! TCP/UDP offload and the interrupt pin are not modelled: the driver is told
//! to poll, and a board file gives it no INT line.
//!
//! The SPI side is the chip's variable-length mode: a 16-bit address, a control
//! byte (block select in bits 7:3, write in bit 2, mode in bits 1:0), then data
//! until chip select rises. The controller model forwards the address and
//! control bytes as the start of each transfer for a board entry marked
//! `framed = true`, so a transfer arrives here whole.
//!
//! Frames go to and from the outside world over UDP, one datagram per frame,
//! which QEMU's `dgram` netdev joins to its user-mode network (DHCP, DNS, NAT).

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};

use vpb::{Claim, EventSink, Peripheral, Response, Transaction};

/// What `VERSIONR` reads on a real chip.
const CHIP_VERSION: u8 = 0x04;

/// Block-select values.
const BSB_COMMON: u8 = 0x00;

/// Common register offsets.
const REG_MR: usize = 0x00;
const REG_SHAR: usize = 0x09;
const REG_IR: usize = 0x15;
const REG_PHYCFGR: usize = 0x2e;
const REG_VERSIONR: usize = 0x39;
const COMMON_LEN: usize = 0x40;

/// MR bits.
const MR_RST: u8 = 1 << 7;

/// PHY configuration as a healthy 100 Mbit full-duplex link reports it:
/// not in reset (bit 7), all-capable negotiation mode, full duplex, 100 Mbit,
/// link up.
const PHYCFGR_LINK_100_FULL: u8 = 0xbf;

/// Socket register offsets.
const SN_MR: usize = 0x00;
const SN_CR: usize = 0x01;
const SN_IR: usize = 0x02;
const SN_SR: usize = 0x03;
const SN_RXBUF_SIZE: usize = 0x1e;
const SN_TXBUF_SIZE: usize = 0x1f;
const SN_TX_RD: usize = 0x22;
const SN_TX_WR: usize = 0x24;
const SN_RX_RD: usize = 0x28;
const SN_RX_WR: usize = 0x2a;
const SOCKET_LEN: usize = 0x30;

/// Sn_MR: protocol field and the MAC filter bit.
const SMR_PROTOCOL: u8 = 0x0f;
const SMR_MACRAW: u8 = 0x04;
const SMR_MAC_FILTER: u8 = 1 << 7;

/// Sn_CR commands.
const CR_OPEN: u8 = 0x01;
const CR_CLOSE: u8 = 0x10;
const CR_SEND: u8 = 0x20;
const CR_RECV: u8 = 0x40;

/// Sn_IR bits.
const IR_RECV: u8 = 1 << 2;
const IR_SEND_OK: u8 = 1 << 4;

/// Sn_SR value for an open MACRAW socket.
const SR_MACRAW: u8 = 0x42;

/// Memory behind each of socket 0's two buffers. The chip has 16 KiB of each
/// in total and the driver gives it all to socket 0.
const BUFFER_MEM: usize = 16 * 1024;

/// The largest frame we pass on, VLAN tag and all.
const MAX_FRAME: usize = 1522;

/// The path frames take to and from the outside world.
#[derive(Debug)]
pub struct Link {
    socket: UdpSocket,
    peer: SocketAddr,
    inbox: Arc<Mutex<VecDeque<Vec<u8>>>>,
}

impl Link {
    /// Send frames to `peer` and receive them on `socket`.
    ///
    /// A thread reads the socket so a frame arriving between SPI transfers is
    /// not lost; the model drains the queue the next time the bus is used.
    pub fn new(socket: UdpSocket, peer: SocketAddr) -> std::io::Result<Self> {
        let reader = socket.try_clone()?;
        let inbox: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::default();
        let queue = inbox.clone();
        std::thread::Builder::new().name("w5500-rx".into()).spawn(move || {
            let mut buf = vec![0u8; 2048];
            while let Ok(n) = reader.recv(&mut buf) {
                if n == 0 || n > MAX_FRAME {
                    continue;
                }
                if let Ok(mut q) = queue.lock() {
                    // A guest that never reads must not grow this without bound.
                    if q.len() < 256 {
                        q.push_back(buf[..n].to_vec());
                    }
                }
            }
        })?;
        Ok(Link { socket, peer, inbox })
    }

    fn send(&self, frame: &[u8]) {
        // A send that fails is a dropped frame, which is what a wire does.
        let _ = self.socket.send_to(frame, self.peer);
    }

    fn take(&self) -> Option<Vec<u8>> {
        self.inbox.lock().ok()?.pop_front()
    }
}

#[derive(Debug)]
pub struct W5500 {
    claim: Claim,
    link: Option<Link>,
    common: [u8; COMMON_LEN],
    sockets: [[u8; SOCKET_LEN]; 8],
    tx_mem: Vec<u8>,
    rx_mem: Vec<u8>,
}

impl W5500 {
    pub fn new(claim: Claim, link: Option<Link>) -> Self {
        let mut chip = W5500 {
            claim,
            link,
            common: [0; COMMON_LEN],
            sockets: [[0; SOCKET_LEN]; 8],
            tx_mem: vec![0; BUFFER_MEM],
            rx_mem: vec![0; BUFFER_MEM],
        };
        chip.reset();
        chip
    }

    /// Everything a software or power-on reset puts back.
    fn reset(&mut self) {
        self.common = [0; COMMON_LEN];
        self.common[REG_VERSIONR] = CHIP_VERSION;
        self.common[REG_PHYCFGR] = PHYCFGR_LINK_100_FULL;
        self.sockets = [[0; SOCKET_LEN]; 8];
        for s in &mut self.sockets {
            s[SN_RXBUF_SIZE] = 2;
            s[SN_TXBUF_SIZE] = 2;
        }
    }

    /// Size of socket 0's TX or RX memory, in bytes. Never zero, so pointer
    /// arithmetic can wrap on it.
    fn size(&self, reg: usize) -> usize {
        (usize::from(self.sockets[0][reg]) * 1024).clamp(1, BUFFER_MEM)
    }

    fn sock16(&self, reg: usize) -> u16 {
        u16::from_be_bytes([self.sockets[0][reg], self.sockets[0][reg + 1]])
    }

    fn set_sock16(&mut self, reg: usize, value: u16) {
        self.sockets[0][reg..reg + 2].copy_from_slice(&value.to_be_bytes());
    }

    fn rx_received(&self) -> u16 {
        self.sock16(SN_RX_WR).wrapping_sub(self.sock16(SN_RX_RD))
    }

    fn tx_free(&self) -> u16 {
        let used = self.sock16(SN_TX_WR).wrapping_sub(self.sock16(SN_TX_RD));
        (self.size(SN_TXBUF_SIZE) as u16).wrapping_sub(used)
    }

    fn read_byte(&self, bsb: u8, offset: u16) -> u8 {
        let off = usize::from(offset);
        match bsb {
            BSB_COMMON => self.common.get(off).copied().unwrap_or(0),
            b if b % 4 == 1 => {
                let n = usize::from(b / 4);
                match (n, off) {
                    (0, 0x20) => (self.tx_free() >> 8) as u8,
                    (0, 0x21) => self.tx_free() as u8,
                    (0, 0x26) => (self.rx_received() >> 8) as u8,
                    (0, 0x27) => self.rx_received() as u8,
                    _ => self.sockets.get(n).and_then(|s| s.get(off)).copied().unwrap_or(0),
                }
            }
            2 => self.tx_mem[off % self.size(SN_TXBUF_SIZE)],
            3 => self.rx_mem[off % self.size(SN_RXBUF_SIZE)],
            _ => 0,
        }
    }

    fn write_byte(&mut self, bsb: u8, offset: u16, value: u8) {
        let off = usize::from(offset);
        match bsb {
            BSB_COMMON => match off {
                REG_MR => {
                    if value & MR_RST != 0 {
                        self.reset();
                    } else {
                        self.common[REG_MR] = value;
                    }
                }
                REG_IR => self.common[REG_IR] &= !value,
                REG_PHYCFGR | REG_VERSIONR => {}
                o if o < COMMON_LEN => self.common[o] = value,
                _ => {}
            },
            b if b % 4 == 1 => {
                let n = usize::from(b / 4);
                if off >= SOCKET_LEN {
                    return;
                }
                match off {
                    SN_CR => {
                        if n == 0 {
                            self.command(value);
                        }
                    }
                    // Interrupt flags clear by writing a one.
                    SN_IR => self.sockets[n][SN_IR] &= !value,
                    SN_SR => {}
                    _ => self.sockets[n][off] = value,
                }
            }
            2 => {
                let size = self.size(SN_TXBUF_SIZE);
                self.tx_mem[off % size] = value;
            }
            // The RX memory is the chip's to write; the host only reads it.
            _ => {}
        }
    }

    /// Socket 0's command register. It reads back zero once accepted, which
    /// the driver polls for, so there is nothing to leave pending.
    fn command(&mut self, command: u8) {
        match command {
            CR_OPEN => {
                let protocol = self.sockets[0][SN_MR] & SMR_PROTOCOL;
                self.sockets[0][SN_SR] = if protocol == SMR_MACRAW { SR_MACRAW } else { 0 };
            }
            CR_CLOSE => self.sockets[0][SN_SR] = 0,
            CR_SEND => self.send(),
            CR_RECV => {
                // More frames waiting means the flag comes straight back.
                if self.rx_received() > 0 {
                    self.sockets[0][SN_IR] |= IR_RECV;
                }
            }
            _ => {}
        }
    }

    fn send(&mut self) {
        let size = self.size(SN_TXBUF_SIZE);
        let rd = self.sock16(SN_TX_RD);
        let wr = self.sock16(SN_TX_WR);
        let len = usize::from(wr.wrapping_sub(rd));
        let frame: Vec<u8> =
            (0..len).map(|i| self.tx_mem[(usize::from(rd) + i) % size]).collect();
        if let Some(link) = &self.link {
            if !frame.is_empty() {
                link.send(&frame);
            }
        }
        self.set_sock16(SN_TX_RD, wr);
        self.sockets[0][SN_IR] |= IR_SEND_OK;
    }

    /// Whether the MAC filter would let this frame through.
    fn accepts(&self, frame: &[u8]) -> bool {
        if self.sockets[0][SN_MR] & SMR_MAC_FILTER == 0 {
            return true;
        }
        let Some(dst) = frame.get(..6) else { return false };
        dst == [0xff; 6] || dst == &self.common[REG_SHAR..REG_SHAR + 6]
    }

    /// Move any frames that have arrived into socket 0's RX memory.
    fn receive_pending(&mut self) {
        if self.sockets[0][SN_SR] != SR_MACRAW {
            // Closed: a real chip would not be listening, so drop what came.
            while self.link.as_ref().and_then(Link::take).is_some() {}
            return;
        }
        let size = self.size(SN_RXBUF_SIZE);
        while let Some(frame) = self.link.as_ref().and_then(Link::take) {
            if !self.accepts(&frame) {
                continue;
            }
            let need = frame.len() + 2;
            let free = size - usize::from(self.rx_received());
            if need > free {
                // No room: the chip drops the frame, and so do we.
                continue;
            }
            let wr = self.sock16(SN_RX_WR);
            let header = (need as u16).to_be_bytes();
            for (i, byte) in header.iter().chain(frame.iter()).enumerate() {
                self.rx_mem[(usize::from(wr) + i) % size] = *byte;
            }
            self.set_sock16(SN_RX_WR, wr.wrapping_add(need as u16));
            self.sockets[0][SN_IR] |= IR_RECV;
        }
    }
}

impl Peripheral for W5500 {
    fn kind(&self) -> &str {
        "w5500"
    }

    fn claims(&self) -> Vec<Claim> {
        vec![self.claim.clone()]
    }

    fn transact(&mut self, tx: &Transaction, _events: &mut dyn EventSink) -> Response {
        let Transaction::SpiTransfer { mosi, read_len, .. } = tx else {
            return Response::None;
        };

        self.receive_pending();

        let mut miso = vec![0u8; (*read_len as usize).max(mosi.len())];
        if mosi.len() >= 3 {
            let mut address = u16::from_be_bytes([mosi[0], mosi[1]]);
            let control = mosi[2];
            let bsb = control >> 3;
            let write = control & 0x04 != 0;
            // The driver only uses variable-length mode, where the data runs
            // until chip select rises; the fixed-length modes are not modelled.
            for (i, byte) in mosi.iter().enumerate().skip(3) {
                if write {
                    self.write_byte(bsb, address, *byte);
                } else {
                    miso[i] = self.read_byte(bsb, address);
                }
                address = address.wrapping_add(1);
            }
            // A read clocks out zeros in mosi but asks for bytes past them.
            if !write {
                for slot in miso.iter_mut().skip(mosi.len().max(3)) {
                    *slot = self.read_byte(bsb, address);
                    address = address.wrapping_add(1);
                }
            }
        }

        if *read_len == 0 {
            Response::None
        } else {
            miso.truncate(*read_len as usize);
            Response::data(miso)
        }
    }

    fn decode(&self, tx: &Transaction, _response: &Response) -> Option<String> {
        let mosi = tx.payload();
        if mosi.len() < 3 {
            return None;
        }
        let address = u16::from_be_bytes([mosi[0], mosi[1]]);
        let control = mosi[2];
        let bsb = control >> 3;
        let verb = if control & 0x04 != 0 { "write" } else { "read" };
        let block = match bsb {
            0 => "common".to_string(),
            b if b % 4 == 1 => format!("socket{} regs", b / 4),
            b if b % 4 == 2 => format!("socket{} tx", b / 4),
            b => format!("socket{} rx", b / 4),
        };
        Some(format!("W5500 {verb} {block} @{address:#06x} ({} bytes)", mosi.len() - 3))
    }

    fn tick(&mut self, _elapsed_us: u64, _events: &mut dyn EventSink) {
        self.receive_pending();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip() -> W5500 {
        W5500::new(Claim::Spi { controller: 2, cs: 1 }, None)
    }

    /// A transfer as the controller forwards it: address, control, data.
    fn xfer(chip: &mut W5500, address: u16, bsb: u8, write: bool, data: &[u8], read: usize) -> Vec<u8> {
        let mut sink: Vec<vpb::Event> = Vec::new();
        let mut mosi = address.to_be_bytes().to_vec();
        mosi.push(bsb << 3 | if write { 0x04 } else { 0 });
        mosi.extend_from_slice(data);
        mosi.resize(3 + data.len().max(read), 0);
        let len = mosi.len() as u32;
        let tx = Transaction::SpiTransfer { controller: 2, cs: 1, dc: None, mosi, read_len: len };
        match chip.transact(&tx, &mut sink) {
            Response::Data { bytes } => bytes[3..].to_vec(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn reports_its_version_and_a_live_link() {
        let mut c = chip();
        assert_eq!(xfer(&mut c, 0x39, 0, false, &[], 1), [CHIP_VERSION]);
        assert_eq!(xfer(&mut c, 0x2e, 0, false, &[], 1)[0] & 0x81, 0x81);
    }

    #[test]
    fn reset_clears_the_mac_but_keeps_the_version() {
        let mut c = chip();
        xfer(&mut c, 0x09, 0, true, &[1, 2, 3, 4, 5, 6], 0);
        assert_eq!(xfer(&mut c, 0x09, 0, false, &[], 6), [1, 2, 3, 4, 5, 6]);
        xfer(&mut c, 0, 0, true, &[MR_RST], 0);
        assert_eq!(xfer(&mut c, 0x09, 0, false, &[], 6), [0; 6]);
        assert_eq!(xfer(&mut c, 0x39, 0, false, &[], 1), [CHIP_VERSION]);
    }

    #[test]
    fn sending_a_frame_advances_the_tx_pointers_and_flags_it() {
        let mut c = chip();
        xfer(&mut c, SN_RXBUF_SIZE as u16, 1, true, &[16], 0);
        xfer(&mut c, SN_TXBUF_SIZE as u16, 1, true, &[16], 0);
        xfer(&mut c, SN_MR as u16, 1, true, &[SMR_MACRAW], 0);
        xfer(&mut c, SN_CR as u16, 1, true, &[CR_OPEN], 0);
        assert_eq!(xfer(&mut c, SN_SR as u16, 1, false, &[], 1), [SR_MACRAW]);
        assert_eq!(xfer(&mut c, 0x20, 1, false, &[], 2), [0x40, 0x00]);

        let frame = [0xaa; 60];
        xfer(&mut c, 0, 2, true, &frame, 0);
        xfer(&mut c, SN_TX_WR as u16, 1, true, &[0, 60], 0);
        xfer(&mut c, SN_CR as u16, 1, true, &[CR_SEND], 0);
        assert_eq!(xfer(&mut c, SN_CR as u16, 1, false, &[], 1), [0]);
        assert_eq!(xfer(&mut c, SN_TX_RD as u16, 1, false, &[], 2), [0, 60]);
        assert_eq!(xfer(&mut c, SN_IR as u16, 1, false, &[], 1)[0] & IR_SEND_OK, IR_SEND_OK);
        xfer(&mut c, SN_IR as u16, 1, true, &[IR_SEND_OK], 0);
        assert_eq!(xfer(&mut c, SN_IR as u16, 1, false, &[], 1)[0] & IR_SEND_OK, 0);
    }
}
