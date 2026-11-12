//! A stand-in ICM-42670-P motion sensor, enough for a driver to find it, set
//! it up and read plausible numbers.
//!
//! It answers the identity register, keeps every other register it is
//! written, and builds the temperature, accelerometer and gyroscope data
//! registers from a shared [`ImuState`] at the moment they are read, using the
//! full-scale range the driver has configured. By default it sits flat and
//! still: 1 g down on Z, no rotation, 25 degrees C.
//!
//! The register numbers and scale factors come from the open-source ESP-IDF
//! component that drives this chip (`espressif/icm42670` 2.1.0), which is
//! what real firmware for the board uses. The chip's datasheet was not read,
//! so anything that component does not touch (FIFO, interrupts, the APEX
//! motion features, the second register bank) is not modelled.

use std::sync::{Arc, Mutex};

use vpb::{Claim, EventSink, Peripheral, Response, Transaction};

const WHO_AM_I: usize = 0x75;
const CHIP_ID: u8 = 0x67;
const TEMP_DATA: usize = 0x09;
const ACCEL_DATA: usize = 0x0b;
const GYRO_DATA: usize = 0x11;
const GYRO_CONFIG0: usize = 0x20;
const ACCEL_CONFIG0: usize = 0x21;
const REGS: usize = 0x80;

/// What the sensor feels. Set by the window or a test; read by the chip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImuState {
    /// Acceleration in g.
    pub accel: [f32; 3],
    /// Rotation rate in degrees per second.
    pub gyro: [f32; 3],
    /// Die temperature in degrees C.
    pub temp_c: f32,
}

impl Default for ImuState {
    fn default() -> Self {
        ImuState { accel: [0.0, 0.0, 1.0], gyro: [0.0; 3], temp_c: 25.0 }
    }
}

/// Shared handle for whatever wants to move the sensor.
pub type ImuHandle = Arc<Mutex<ImuState>>;

#[derive(Debug)]
pub struct Icm42670 {
    claims: Vec<Claim>,
    state: ImuHandle,
    regs: [u8; REGS],
    pointer: usize,
}

impl Icm42670 {
    pub fn new(claims: Vec<Claim>) -> Self {
        Icm42670 {
            claims,
            state: Arc::new(Mutex::new(ImuState::default())),
            regs: [0; REGS],
            pointer: 0,
        }
    }

    pub fn state(&self) -> &ImuHandle {
        &self.state
    }

    /// LSB per g for the accelerometer's configured range (bits 6:5 of
    /// ACCEL_CONFIG0: 0 = 16 g, 1 = 8 g, 2 = 4 g, 3 = 2 g).
    fn accel_lsb_per_g(&self) -> f32 {
        match (self.regs[ACCEL_CONFIG0] >> 5) & 3 {
            0 => 2048.0,
            1 => 4096.0,
            2 => 8192.0,
            _ => 16384.0,
        }
    }

    /// LSB per degree per second (bits 6:5 of GYRO_CONFIG0: 0 = 2000, 1 = 1000,
    /// 2 = 500, 3 = 250 dps).
    fn gyro_lsb_per_dps(&self) -> f32 {
        match (self.regs[GYRO_CONFIG0] >> 5) & 3 {
            0 => 16.4,
            1 => 32.8,
            2 => 65.5,
            _ => 131.0,
        }
    }

    fn word(value: f32, lsb: f32) -> [u8; 2] {
        let raw = (value * lsb).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        raw.to_be_bytes()
    }

    fn read_reg(&self, reg: usize) -> u8 {
        let s = self.state.lock().map(|s| *s).unwrap_or_default();
        match reg {
            WHO_AM_I => CHIP_ID,
            // Data registers are big-endian pairs.
            r if (TEMP_DATA..TEMP_DATA + 2).contains(&r) => {
                // The driver reads the temperature as raw / 128 + 25.
                let bytes = Self::word(s.temp_c - 25.0, 128.0);
                bytes[r - TEMP_DATA]
            }
            r if (ACCEL_DATA..ACCEL_DATA + 6).contains(&r) => {
                let axis = (r - ACCEL_DATA) / 2;
                Self::word(s.accel[axis], self.accel_lsb_per_g())[(r - ACCEL_DATA) % 2]
            }
            r if (GYRO_DATA..GYRO_DATA + 6).contains(&r) => {
                let axis = (r - GYRO_DATA) / 2;
                Self::word(s.gyro[axis], self.gyro_lsb_per_dps())[(r - GYRO_DATA) % 2]
            }
            r if r < REGS => self.regs[r],
            _ => 0,
        }
    }

    fn write_reg(&mut self, reg: usize, value: u8) {
        // Identity and the data registers are read-only.
        let read_only = reg == WHO_AM_I
            || (TEMP_DATA..GYRO_DATA + 6).contains(&reg);
        if !read_only && reg < REGS {
            self.regs[reg] = value;
        }
    }
}

impl Peripheral for Icm42670 {
    fn kind(&self) -> &str {
        "icm42670"
    }

    fn claims(&self) -> Vec<Claim> {
        self.claims.clone()
    }

    fn transact(&mut self, tx: &Transaction, _events: &mut dyn EventSink) -> Response {
        match tx {
            Transaction::I2cWrite { data, .. } => {
                let Some((&reg, values)) = data.split_first() else {
                    return Response::None; // address probe
                };
                self.pointer = reg as usize;
                for &v in values {
                    self.write_reg(self.pointer, v);
                    self.pointer = (self.pointer + 1) % 0x100;
                }
                Response::None
            }
            Transaction::I2cRead { len, .. } => {
                let mut out = Vec::with_capacity(*len as usize);
                for _ in 0..*len {
                    out.push(self.read_reg(self.pointer));
                    self.pointer = (self.pointer + 1) % 0x100;
                }
                Response::data(out)
            }
            Transaction::Reset => {
                self.regs = [0; REGS];
                self.pointer = 0;
                Response::None
            }
            _ => Response::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vpb::registry::EventQueue;

    fn chip() -> Icm42670 {
        Icm42670::new(vec![Claim::I2c { controller: 0, address: 0x68, alt: None }])
    }

    fn write(d: &mut Icm42670, bytes: &[u8]) {
        let mut ev = EventQueue::default();
        d.transact(
            &Transaction::I2cWrite { controller: 0, address: 0x68, data: bytes.to_vec(), stop: true },
            &mut ev,
        );
    }

    /// The component's register read: write the register number, then read.
    fn read(d: &mut Icm42670, reg: u8, len: u32) -> Vec<u8> {
        write(d, &[reg]);
        let mut ev = EventQueue::default();
        d.transact(&Transaction::I2cRead { controller: 0, address: 0x68, len }, &mut ev)
            .payload()
            .to_vec()
    }

    #[test]
    fn the_identity_register_says_it_is_the_chip_the_driver_wants() {
        assert_eq!(read(&mut chip(), 0x75, 1), [0x67]);
    }

    #[test]
    fn the_identity_register_cannot_be_overwritten() {
        let mut d = chip();
        write(&mut d, &[0x75, 0x00]);
        assert_eq!(read(&mut d, 0x75, 1), [0x67]);
    }

    #[test]
    fn written_configuration_reads_back() {
        let mut d = chip();
        // What the component's config call writes: gyro then accelerometer.
        write(&mut d, &[0x20, (3 << 5) | 9, (3 << 5) | 9]);
        assert_eq!(read(&mut d, 0x20, 2), [0x69, 0x69]);
    }

    #[test]
    fn flat_and_still_reads_one_g_on_z_at_the_two_g_range() {
        let mut d = chip();
        write(&mut d, &[0x21, 3 << 5]); // +/- 2 g
        let raw = read(&mut d, 0x0b, 6);
        let z = i16::from_be_bytes([raw[4], raw[5]]);
        assert_eq!(i16::from_be_bytes([raw[0], raw[1]]), 0);
        assert_eq!(i16::from_be_bytes([raw[2], raw[3]]), 0);
        assert_eq!(z, 16384, "the driver divides by 16384 at this range");
    }

    #[test]
    fn the_same_gravity_is_a_smaller_number_at_a_wider_range() {
        let mut d = chip();
        write(&mut d, &[0x21, 0]); // +/- 16 g
        let raw = read(&mut d, 0x0b, 6);
        assert_eq!(i16::from_be_bytes([raw[4], raw[5]]), 2048);
    }

    #[test]
    fn tilting_moves_the_numbers() {
        let mut d = chip();
        write(&mut d, &[0x21, 3 << 5]);
        d.state().lock().unwrap().accel = [0.5, -0.25, 0.8];
        let raw = read(&mut d, 0x0b, 6);
        assert_eq!(i16::from_be_bytes([raw[0], raw[1]]), 8192);
        assert_eq!(i16::from_be_bytes([raw[2], raw[3]]), -4096);
    }

    #[test]
    fn rotation_scales_with_the_gyro_range() {
        let mut d = chip();
        write(&mut d, &[0x20, 3 << 5]); // 250 dps
        d.state().lock().unwrap().gyro = [10.0, 0.0, 0.0];
        let raw = read(&mut d, 0x11, 6);
        assert_eq!(i16::from_be_bytes([raw[0], raw[1]]), 1310);
    }

    #[test]
    fn the_temperature_is_twenty_five_degrees_at_zero_raw() {
        let raw = read(&mut chip(), 0x09, 2);
        assert_eq!(i16::from_be_bytes([raw[0], raw[1]]), 0);
    }

    #[test]
    fn a_probe_is_acknowledged() {
        let mut d = chip();
        let mut ev = EventQueue::default();
        let r = d.transact(
            &Transaction::I2cWrite { controller: 0, address: 0x68, data: vec![], stop: true },
            &mut ev,
        );
        assert_eq!(r, Response::None);
    }
}
