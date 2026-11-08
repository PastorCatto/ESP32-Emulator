# Documentation

Each numbered chapter has the same sections: Short version, What it is for, The
files, How it works, How to build and test it, What goes wrong, What is not
finished. Every chapter says plainly what is untested. The first four pages are
older and keep their own shape.

| Page | Covers |
|---|---|
| [architecture.md](architecture.md) | How the pieces fit together, and why each decision was made |
| [board-files.md](board-files.md) | The board TOML format, field by field |
| [custom-hardware.md](custom-hardware.md) | Three ways to add a device, including a working driver in Python |
| [vm-hardware.md](vm-hardware.md) | The VM hardware contract: what was built (W5500, VM info) and what is still a draft |
| [10-headless-and-trace.md](10-headless-and-trace.md) | Running without a window, the command file, the bus trace |
| [11-vm-image-layout.md](11-vm-image-layout.md) | What a VM flash image looks like (flash map v2) and what the emulator does with it |
| [12-purr-run-and-the-sdk-kit.md](12-purr-run-and-the-sdk-kit.md) | How `purr emulator`, `purr run` and the SDK kit use the emulator |
| [13-the-patched-qemu.md](13-the-patched-qemu.md) | What we add to QEMU, and how |
| [14-building-on-macos.md](14-building-on-macos.md) | The static build for macOS 12 and later |
| [15-building-on-linux.md](15-building-on-linux.md) | Linux: nothing tested, where to start |
| [16-building-on-windows.md](16-building-on-windows.md) | Windows: what is known, and why there is no emulator in the Windows kit |
| [17-licences.md](17-licences.md) | Licences, item by item |

Also: [../qemu/README.md](../qemu/README.md) (the long account of each QEMU model),
[../LICENSES.md](../LICENSES.md), [../LICENSE](../LICENSE).

To check that every relative link in the Markdown files resolves:

```sh
python3 tools/check_links.py
```
