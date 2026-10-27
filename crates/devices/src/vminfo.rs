//! A device that tells the guest it is running in a virtual machine.
//!
//! It sits on an I2C bus at a fixed address and answers a read with a short
//! block describing what the virtual hardware offers that the guest cannot
//! learn another way. A real board has nothing at that address, so the
//! guest's probe is refused and it knows it is on hardware.
//!
//! Block layout (12 bytes, returned from any read, whatever was written first):
//!
//! | Byte | Meaning |
//! |---|---|
//! | 0-2 | `P` `V` `M` |
//! | 3 | layout version (1) |
//! | 4 | flags: bit 0 = the Ethernet controller has an SPI bus of its own |
//! | 5 | that bus's SPI host (3 for SPI3) |
//! | 6-9 | its SCLK, MOSI, MISO and CS GPIOs, `0xFF` for none |
//! | 10-11 | reserved |
//!
//! It is information, never authority: nothing it says relaxes any security
//! decision the guest makes.

use vpb::{Claim, EventSink, Peripheral, Response, Transaction};

/// Bytes in the block.
pub const INFO_LEN: usize = 12;

/// Flag bit: the Ethernet controller has an SPI bus of its own.
pub const FLAG_NIC_OWN_BUS: u8 = 0x01;

/// What the device reports about the virtual Ethernet controller's bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NicBus {
    pub host: u8,
    pub sclk: u8,
    pub mosi: u8,
    pub miso: u8,
    pub cs: u8,
}

#[derive(Debug)]
pub struct VmInfo {
    claims: Vec<Claim>,
    block: [u8; INFO_LEN],
}

impl VmInfo {
    pub fn new(claims: Vec<Claim>, nic: Option<NicBus>) -> Self {
        let mut block = [0u8; INFO_LEN];
        block[..3].copy_from_slice(b"PVM");
        block[3] = 1;
        match nic {
            Some(n) => {
                block[4] = FLAG_NIC_OWN_BUS;
                block[5] = n.host;
                block[6..10].copy_from_slice(&[n.sclk, n.mosi, n.miso, n.cs]);
            }
            None => block[6..10].fill(0xff),
        }
        VmInfo { claims, block }
    }

    /// The block as the guest reads it.
    pub fn block(&self) -> &[u8; INFO_LEN] {
        &self.block
    }
}

impl Peripheral for VmInfo {
    fn kind(&self) -> &str {
        "vm-info"
    }

    fn claims(&self) -> Vec<Claim> {
        self.claims.clone()
    }

    fn transact(&mut self, tx: &Transaction, _events: &mut dyn EventSink) -> Response {
        match tx {
            Transaction::I2cRead { len, .. } => {
                // Repeat the block if asked for more than it holds, so an
                // over-long read still returns something sensible.
                let out: Vec<u8> =
                    (0..*len as usize).map(|i| self.block[i % INFO_LEN]).collect();
                Response::data(out)
            }
            // Writes are acknowledged and ignored: a register-style driver
            // writes an offset before reading, and the block has no registers.
            _ => Response::None,
        }
    }

    fn decode(&self, tx: &Transaction, _response: &Response) -> Option<String> {
        matches!(tx, Transaction::I2cRead { .. }).then(|| "VM info".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(dev: &mut VmInfo, len: u32) -> Vec<u8> {
        let mut sink: Vec<vpb::Event> = Vec::new();
        let tx = Transaction::I2cRead { controller: 0, address: 0x6e, len };
        match dev.transact(&tx, &mut sink) {
            Response::Data { bytes } => bytes,
            _ => Vec::new(),
        }
    }

    #[test]
    fn describes_a_nic_on_its_own_bus() {
        let nic = NicBus { host: 3, sclk: 47, mosi: 48, miso: 21, cs: 9 };
        let mut dev = VmInfo::new(vec![Claim::I2c { controller: 0, address: 0x6e, alt: None }], Some(nic));
        let b = read(&mut dev, INFO_LEN as u32);
        assert_eq!(&b[..4], b"PVM\x01");
        assert_eq!(b[4], FLAG_NIC_OWN_BUS);
        assert_eq!(&b[5..10], [3, 47, 48, 21, 9]);
    }

    #[test]
    fn without_a_nic_bus_the_flag_is_clear_and_pins_are_unset() {
        let mut dev = VmInfo::new(Vec::new(), None);
        let b = read(&mut dev, INFO_LEN as u32);
        assert_eq!(b[4], 0);
        assert_eq!(&b[6..10], [0xff; 4]);
    }

    #[test]
    fn a_long_read_repeats_the_block() {
        let mut dev = VmInfo::new(Vec::new(), None);
        let b = read(&mut dev, 24);
        assert_eq!(b[..12], b[12..]);
    }
}
