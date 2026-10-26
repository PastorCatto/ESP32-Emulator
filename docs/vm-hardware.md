# VM hardware contract (draft)

Status: **design draft, nothing here is implemented or verified.** Every address
and register below is a proposal until it has been checked against the ESP32-S3
memory map and built into QEMU. Scope is the ESP32-S3 only; the interfaces are
written so another chip could reuse them.

The VM cannot emulate the Wi-Fi radio (see [architecture.md](architecture.md),
section 6). So the PURR OS **VM flavor** replaces the radio stack with a backend
that talks to emulator-provided hardware, and adds adapters real boards have.
Firmware above the backend (lwIP, TLS, the recovery loader, the app layer) runs
unchanged, which is what makes internet recovery testable without an OS
installed.

Trust: the VM flavor is signed with the published developer keys and reports
`verified (developer key)`. It never carries the production boot key.

## 1. Virtual NIC (`vnic`)

A QEMU device that moves Ethernet frames between the guest and QEMU's
`-netdev user` backend, which supplies DHCP, DNS and NAT with no host privileges.
Both the Wi-Fi backend and the Ethernet adapter sit on top of it.

| Offset | Register | Notes |
|---|---|---|
| 0x00 | `ID` | read-only magic, lets firmware detect the device |
| 0x04 | `CTRL` | bit 0 enable, bit 1 reset |
| 0x08 | `STATUS` | bit 0 link up |
| 0x0C | `IRQ_STATUS` / `IRQ_MASK` | bit 0 RX ready, bit 1 TX done |
| 0x10-0x18 | `MAC` | six bytes, set by the host |
| 0x20 | `TX_BASE`, `TX_LEN`, `TX_HEAD`, `TX_TAIL` | descriptor ring in guest RAM |
| 0x40 | `RX_BASE`, `RX_LEN`, `RX_HEAD`, `RX_TAIL` | descriptor ring in guest RAM |

Base address and interrupt line: **to be chosen** after checking what is unmapped
on the S3. A frame is a plain Ethernet frame, at most 1522 bytes.

## 2. Wi-Fi backend (firmware side)

A VM-flavor implementation of the `esp_wifi` calls PURR OS uses, registering an
`esp_netif` on the `vnic`. It must keep the real calls' behavior: the same return
codes, the same events, in the same order.

| Call | Behavior |
|---|---|
| `esp_wifi_init` / `start` / `stop` | state only |
| `esp_wifi_set_mode`, `set_config` | stored |
| `esp_wifi_scan_start` | after a short delay, posts `WIFI_EVENT_SCAN_DONE` |
| `esp_wifi_scan_get_ap_num`, `get_ap_records` | the simulated AP table |
| `esp_wifi_connect` | fails for an unknown SSID or wrong password, like hardware |
| `esp_wifi_sta_get_ap_info` | reports the live, spoofed RSSI |
| events | `STA_CONNECTED`, `STA_DISCONNECTED`, then `IP_EVENT_STA_GOT_IP` once DHCP completes |

### Simulated environment

The AP table lives in the emulator, not in the image, and can be edited while the
guest runs. Each entry: SSID, BSSID, channel, auth mode, password, RSSI (dBm),
optional RSSI jitter. The backend reads it through a small vpb device
(`wifi-env`); the wire format is still to be written.

## 3. Ethernet

The S3 has no Ethernet MAC. Real boards use a SPI chip. Proposal: emulate a
**W5500** as a vpb SPI device in Rust so the real ESP-IDF driver runs, backed by
the same network path as the `vnic`. Undecided: this versus exposing the `vnic`
as an Ethernet netif directly (quicker, but exercises no real driver).

## 4. USB host

The S3's USB OTG is a Synopsys DWC2. Plan: map QEMU's existing DWC2 host model at
the S3's OTG address and pass a physical device through with `usb-host`. Needs a
spike before any contract: whether the S3's DWC2 configuration works with that
model and with IDF's USB host stack, and whether macOS lets us claim the device.

## Open items

- Address and interrupt assignment for each device.
- Whether the build has the user-mode network backend (libslirp).
- The `wifi-env` vpb message format.
- Ethernet: W5500 or direct `vnic`.
- USB: the spike above.
