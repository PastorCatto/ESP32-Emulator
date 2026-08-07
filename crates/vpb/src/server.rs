//! The peripheral side of the bridge: accept the emulator's connection and
//! answer its bus transactions from a [`Registry`].
//!
//! Split out of the `listen` example so the shell runs the same loop it does.
//! A divergence between "what the debugging tool sees" and "what the
//! application sees" is the kind of thing that costs a day.

use std::io::{BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};

use crate::registry::{EventQueue, Registry};
use crate::wire::{self, DeviceMessage, HostMessage};
use crate::{Event, Response, Transaction};

/// Serve one emulator connection until it closes.
///
/// `on_event` receives everything the devices emitted -- trace records, input,
/// whatever a peripheral raised -- in the order it happened.
/// Called just before each transaction is dispatched.
///
/// The registry lives on whichever thread is serving, so this is the only
/// point another thread can reach it -- switching the bus tracer on, enabling
/// or disabling a device. Doing it here rather than mid-transaction means a
/// change never lands between a command and its data phase.
pub type Reconfigure<'a> = &'a mut dyn FnMut(&mut Registry);

pub fn serve(
    stream: TcpStream,
    registry: &mut Registry,
    on_event: &mut dyn FnMut(Event),
) -> std::io::Result<u64> {
    serve_with(stream, registry, on_event, &mut |_| {})
}

/// [`serve`], plus a hook to apply changes from another thread.
pub fn serve_with(
    stream: TcpStream,
    registry: &mut Registry,
    on_event: &mut dyn FnMut(Event),
    reconfigure: Reconfigure<'_>,
) -> std::io::Result<u64> {
    // Latency here is per transaction and the emulator blocks on each one, so
    // Nagle would turn every bus access into a 40ms round trip.
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    let mut sink = EventQueue::default();
    let mut count: u64 = 0;

    loop {
        let msg = match wire::recv_host(&mut reader) {
            Ok(m) => m,
            // A closed connection is how a run ends, not a failure.
            Err(wire::WireError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(count);
            }
            Err(wire::WireError::Io(e)) => return Err(e),
            Err(e) => return Err(std::io::Error::other(e.to_string())),
        };

        let HostMessage::Transact { id, transaction } = msg else {
            continue;
        };
        count += 1;

        reconfigure(registry);
        let response = registry.dispatch(&transaction, &mut sink);
        // An unclaimed SPI read gets zeroes rather than nothing: that is what
        // firmware sees from a bus with no device on it, and it keeps running
        // instead of blocking on a reply that never comes.
        let response = match (&response, &transaction) {
            (Response::None, Transaction::SpiTransfer { read_len, .. }) if *read_len > 0 => {
                Response::data(vec![0u8; *read_len as usize])
            }
            _ => response,
        };

        for event in sink.drain() {
            on_event(event);
        }

        // No id means the emulator is not waiting for an answer.
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

/// Accept connections forever, serving each in turn.
///
/// One at a time on purpose. The emulator opens a single connection, and
/// handling several concurrently would mean bus transactions from different
/// controllers interleaving unpredictably -- the ordering between them is
/// exactly what a device model relies on.
pub fn listen(
    listener: &TcpListener,
    registry: &mut Registry,
    on_event: &mut dyn FnMut(Event),
    on_connect: &mut dyn FnMut(),
    on_disconnect: &mut dyn FnMut(std::io::Result<u64>),
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        on_connect();
        on_disconnect(serve(stream, registry, on_event));
    }
}
