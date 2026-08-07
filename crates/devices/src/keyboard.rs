//! The T-Deck's BBQ10-style keyboard, as an I2C device.
//!
//! The T-Deck runs a small microcontroller behind the keys that speaks I2C at
//! 0x55. Its protocol is not the BBQ10 one despite the shared hardware
//! lineage: the T-Deck firmware answers a plain one-byte read with the ASCII
//! of the pending key, or zero when nothing is waiting. That is the whole
//! interface, and it is why this model is short.
//!
//! Keys come from the UI through a shared queue, so typing into the emulator
//! window arrives here.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use vpb::{Claim, EventSink, Peripheral, Response, Transaction};

/// Keys waiting to be read, oldest first.
pub type KeyQueue = Arc<Mutex<VecDeque<u8>>>;

/// Bound so a window left focused with a key repeating cannot grow this
/// without limit while the guest is not polling.
const MAX_PENDING: usize = 64;

#[derive(Debug)]
pub struct TdeckKeyboard {
    claim: Claim,
    keys: KeyQueue,
}

impl TdeckKeyboard {
    pub fn new(claim: Claim) -> Self {
        TdeckKeyboard {
            claim,
            keys: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// The shared queue, for the UI to push into.
    pub fn keys(&self) -> &KeyQueue {
        &self.keys
    }

    /// Queue a keypress. Anything above ASCII is dropped: the real keyboard
    /// reports single bytes and has no way to express a wider character.
    pub fn press(queue: &KeyQueue, key: char) {
        if !key.is_ascii() {
            return;
        }
        let Ok(mut q) = queue.lock() else { return };
        if q.len() < MAX_PENDING {
            q.push_back(key as u8);
        }
    }
}

impl Peripheral for TdeckKeyboard {
    fn kind(&self) -> &str {
        "tdeck-keyboard"
    }

    fn claims(&self) -> Vec<Claim> {
        vec![self.claim.clone()]
    }

    fn transact(&mut self, tx: &Transaction, _events: &mut dyn EventSink) -> Response {
        let Transaction::I2cRead { len, .. } = tx else {
            // Writes configure backlight and alt-mode on the real part; none
            // of that changes what the keys report.
            return Response::None;
        };

        // Zero means "nothing pressed", which the driver polls for constantly.
        let mut out = vec![0u8; *len as usize];
        if let Ok(mut q) = self.keys.lock() {
            for slot in out.iter_mut() {
                match q.pop_front() {
                    Some(key) => *slot = key,
                    None => break,
                }
            }
        }
        Response::data(out)
    }

    fn decode(&self, tx: &Transaction, response: &Response) -> Option<String> {
        let Transaction::I2cRead { .. } = tx else {
            return None;
        };
        match response.payload().first() {
            Some(0) | None => Some("poll (idle)".into()),
            Some(&key) if key.is_ascii_graphic() => {
                Some(format!("key '{}'", key as char))
            }
            Some(&key) => Some(format!("key {key:#04x}")),
        }
    }
}
