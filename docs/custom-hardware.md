# Adding hardware

There are three ways to add a device, and you should always reach for the
lowest-numbered one that works.

| | Approach | Rebuild needed | Language | Good for |
|---|---|---|---|---|
| 1 | Board TOML | none | — | different panel size, moved pin, new I²C address |
| 2 | In-tree driver | emulator | Rust | devices we ship for everyone |
| 3 | External driver | none | any | your own hardware, private prototypes |

All three end up as a `Peripheral` behind the same routing, so nothing else in
the emulator can tell them apart.

---

## 1. Board TOML

A board is data, not code. A 480×320 panel instead of a 320×240 one is an edit,
not a patch:

```toml
[[peripheral]]
kind = "st7789"
bus = "fspi"
cs = 12
dc = 11
width = 480      # <- just change these
height = 320
rotation = 90
```

Drop a `.toml` on the emulator window and it registers as a board you can pick.
Fields are per-`kind`; unknown fields are reported rather than ignored, so a
typo surfaces instead of silently doing nothing.

## 2. In-tree driver

Implement `Peripheral` and gate it behind a Cargo feature so it can be compiled
out:

```rust
impl Peripheral for MyOled {
    fn kind(&self) -> &str { "my-oled" }

    fn claims(&self) -> Vec<Claim> {
        vec![Claim::Spi { controller: 2, cs: 7 }]
    }

    fn transact(&mut self, tx: &Transaction, events: &mut dyn EventSink) -> Response {
        // ...
        Response::None
    }
}
```

Implement `decode` too — it costs a few lines and turns the bus tracer from a
hex dump into something you can read:

```rust
fn decode(&self, tx: &Transaction, _r: &Response) -> Option<String> {
    let cmd = tx.payload().first()?;
    Some(match cmd { 0x2a => "CASET".into(), 0x2c => "RAMWR".into(), c => format!("cmd {c:#04x}") })
}
```

## 3. External driver

**This is the interesting one.** Your driver is a separate process in any
language. Connect to the emulator's device socket, say what you own, and answer
transactions. No Rust, no rebuild, nothing of yours in our tree.

### Framing

```text
+----------------+------------------+-------------------+----------------+
| body_len : u32 | header_len : u32 | header : JSON     | payload : raw  |
+----------------+------------------+-------------------+----------------+
  little-endian    little-endian      header_len bytes    rest of body
```

`body_len` counts everything after itself. Bulk bytes travel as a raw payload
rather than inside the JSON, so a 150 KiB frame write costs 150 KiB on the
wire instead of four times that.

### Conversation

1. Emulator sends `hello`.
2. You send `register` with your claims.
3. Emulator sends `transact` whenever firmware touches an address you own.
   Answer with `response` when `id` is present; ignore it when absent.
4. Send `event` any time to raise an interrupt or log a line.

### A complete driver, in Python

```python
import json, socket, struct

def send(sock, header, payload=b""):
    h = json.dumps(header).encode()
    sock.sendall(struct.pack("<II", 4 + len(h) + len(payload), len(h)) + h + payload)

def recv(sock):
    (body_len,) = struct.unpack("<I", read_exactly(sock, 4))
    body = read_exactly(sock, body_len)
    (header_len,) = struct.unpack("<I", body[:4])
    return json.loads(body[4:4 + header_len]), body[4 + header_len:]

def read_exactly(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("emulator closed the connection")
        buf += chunk
    return buf

sock = socket.create_connection(("127.0.0.1", 5559))
recv(sock)  # hello

send(sock, {
    "type": "register",
    "kind": "my-sensor",
    "protocol": 1,
    "claims": [{"bus": "i2c", "controller": 0, "address": 0x48}],
})

while True:
    header, payload = recv(sock)
    if header.get("type") != "transact":
        continue

    if header["op"] == "i2c_read":
        # Report a fixed 25.0 °C.
        send(sock, {"type": "response", "id": header["id"], "reply": "data"},
             bytes([0x19, 0x00]))
    elif header["op"] == "i2c_write":
        print("firmware wrote", payload.hex())
        send(sock, {"type": "response", "id": header["id"], "reply": "none"})
```

Run the emulator, run that, and firmware reading I²C address `0x48` gets your
sensor. Point a board TOML at `kind = "my-sensor"` to have it attach
automatically.

### Claims

```json
{"bus": "spi",  "controller": 2, "cs": 7}
{"bus": "i2c",  "controller": 0, "address": 93, "alt": 20}
{"bus": "uart", "controller": 1}
{"bus": "gpio", "pin": 21}
```

Two devices may not claim the same address. The emulator refuses the second one
rather than silently shadowing it, because a shadowed device produces bugs that
look like firmware faults.

### Replying

- `expects_reply` is false for `gpio_write`, `uart_tx`, and any `spi_transfer`
  with `read_len == 0`. Those arrive without an `id` and want no answer —
  this is what keeps display writes fast.
- An I²C address with nothing on it must NACK, not return zeroes. The emulator
  handles that for unclaimed addresses; if you claim an address, you own the
  answer.
- Taking too long to answer a blocking transaction stalls the emulated CPU,
  exactly as slow real hardware would.

### Debugging

Turn on the bus tracer for your bus. Every transaction is logged with
direction, address, the device that answered, and a hex dump:

```text
I²C0 → 0x48 my-sensor  w[01 60]
I²C0 ← 0x48 my-sensor  r[19 00]  · 25.0 °C
I²C0 ← 0x77 <unclaimed>
```

`<unclaimed>` means nothing owns that address — usually a wrong pin in a board
TOML, or a driver that never registered.
