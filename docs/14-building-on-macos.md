# 14. Building on macOS

## Short version

- Tested on **one** computer: an Intel Mac running macOS 26.7. Nothing else.
- The program (`esp32-emulator`, `headless_tap`) builds with plain `cargo`. Its recorded minimum macOS is **10.12**, rustc's default; the window code's real minimum has not been tried.
- The patched QEMU builds with its libraries linked **statically**, with `MACOSX_DEPLOYMENT_TARGET=12.0`. The result links only `/usr/lib` and `/System/Library`, so no libraries travel beside it. Its recorded minimum is **macOS 12.0**.
- The only Mac available was running macOS 26, so the binary has **not been run on any older macOS**. The 12.0 is what the file says, not something observed.
- Apple Silicon: untested. The build is x86_64; on Apple Silicon it would run under Rosetta 2. A native arm64 or universal build has not been attempted.

## What it is for

A copy of QEMU linked against MacPorts or Homebrew libraries records the *host's* macOS as its minimum (every MacPorts library here was minos 26.0) and finds its libraries by an absolute path on the build machine. That makes it useless on any other computer. This chapter is how to build one that is not.

## The files

| File | What it is |
|---|---|
| [scripts/build-macos-deps.sh](../scripts/build-macos-deps.sh) | Builds glib, libintl, pcre2, libffi, pixman, libgpg-error, libgcrypt and libslirp as static archives into `vendor/deps-macos12` |
| [qemu/build.sh](../qemu/build.sh) | Sets `MACOSX_DEPLOYMENT_TARGET` (default 12.0) on macOS; with `QEMU_DEPS_PREFIX` set, restricts `pkg-config` to that prefix |
| [qemu/apply.sh](../qemu/apply.sh) | Includes the edit that stops QEMU using `strchrnul` on macOS |
| [qemu/README.md](../qemu/README.md), "The macOS floor" | The short version of this page |

## How it works

### Why static, and which libraries

zlib, bzip2 and iconv come from the macOS SDK and are always there. The rest QEMU needs are built from source tarballs into a private prefix, `vendor/deps-macos12`, as `.a` files with the deployment target set. Nothing is installed outside that folder. The script downloads the tarballs into `vendor/deps-cache` unless they are already there.

| Library | Version | Why |
|---|---|---|
| libffi | 3.4.8 | needed by glib |
| pcre2 | 10.48 | needed by glib |
| libintl (gettext-runtime) | 1.0 | glib insists on a libintl even with translations off |
| glib | 2.88.3 | QEMU |
| pixman | 0.46.0 | QEMU's display code |
| libgpg-error, libgcrypt | 1.61, 1.12.4 | the Espressif fork includes `gcrypt.h` directly |
| libslirp | 4.9.1 | QEMU's user-mode network (DHCP, DNS, NAT) |

Left out on purpose: nettle, gnutls, zstd. glib's translations are off.

### The steps

```sh
scripts/fetch-qemu-src.sh                      # Espressif's source, tag esp-develop-9.2.2-20260417
scripts/build-macos-deps.sh                    # the static libraries
QEMU_DEPS_PREFIX=vendor/deps-macos12 qemu/build.sh
```

You need `make`, `curl`, `tar`, a C compiler (Xcode command line tools), `meson`, `ninja`, `pkg-config`, and a Python of **3.11 or newer**: QEMU's configure builds a virtual environment and needs `tomllib`. The system Python 3.9 stopped it with "found no usable tomli". Point at another with the `PYTHON` environment variable, which `configure` reads (read in its source; not run through `build.sh`).

### What was actually done

The sequence above was **not run as one command**. The libraries were built with `scripts/build-macos-deps.sh`. QEMU was configured and built by a scratch script with the same steps as `build.sh` plus a `PATH` containing nothing from MacPorts except `ninja` and `pkg-config`, which were linked in by name. `build.sh` with `QEMU_DEPS_PREFIX` has not been run end to end.

### Keep MacPorts and Homebrew off the path

Two things go wrong silently when a package manager is on `PATH` while configuring:

- QEMU also finds `libgcrypt` by running `libgcrypt-config` if it is on `PATH`, and a hit adds `-L/…/macports/lib`. The link then picks MacPorts's `libiconv.dylib` (minos 26, and its symbols are named `libiconv_open`, not `iconv_open`). The failure is `Undefined symbols: _iconv_open`.
- Any `pkg-config` file from the package manager brings back a library built for the newer macOS. `build.sh` sets `PKG_CONFIG_LIBDIR` to the private prefix so this cannot happen through `pkg-config`; it cannot stop a tool found by name.

## How to build and test it

After a build:

```sh
otool -l vendor/src/build/qemu-system-xtensa | grep minos        # expect: minos 12.0
otool -L vendor/src/build/qemu-system-xtensa                     # only /usr/lib and /System/Library
nm -m vendor/src/build/qemu-system-xtensa | grep "weak external" # see below
strings vendor/src/build/qemu-system-xtensa | grep -c macports   # expect: 0
```

(the path is the build directory you used.) What was measured on the one build made: `minos 12.0`, SDK 26.5, 27.1 MB, libraries `libSystem`, `libz`, `libiconv`, `libresolv`, `libpam`, `libbz2`, `libobjc` and system frameworks, and one weak symbol, `_pthread_fchdir_np`, which QEMU guards at the call. Every object in each of the 14 static archives was minos 12.0 (checked with `otool -l` on each).

**The weak-symbol check matters more than the version.** The linker accepts a symbol newer than the deployment target as a *weak* import. The program then loads on the older system and crashes the first time it calls the missing function. `strchrnul` was one: it exists from macOS 15.4, QEMU's configure found it in the macOS 26 SDK, and the first build showed it as `weak external`. `apply.sh` now stops meson from using it on macOS, and the second build had none.

The same program was started (`-version`, `-machine help`, `-machine esp32s3 -S` staying up for 3 seconds) and the PURR OS session reports that it boots the VM image and runs its scripted flow. That was on macOS 26.

The Rust programs:

```sh
cargo build --release -p shell --bin esp32-emulator --example headless_tap
otool -l target/release/esp32-emulator | grep -A3 VERSION_MIN
```

A debug build was measured: `LC_VERSION_MIN_MACOSX 10.12`, SDK 26.5. A release build was not measured.

## What goes wrong

| Symptom | Cause |
|---|---|
| `found no usable tomli, please install it` | The Python found is older than 3.11. Set `PYTHON` |
| `Undefined symbols: _iconv_open` at link | A package manager's `-L` is on the link line (see above) |
| `'gcrypt.h' file not found` | `libgcrypt` is not in the prefix; run `build-macos-deps.sh` first |
| `ld: warning: … built for newer version 26.0` | A dylib from a package manager was linked. The result has the host's floor |
| `clang: error: unsupported option '-print-multi-os-directory'` while building libffi | Noise from libffi's configure on macOS. The build continues |
| `meson.build:… ERROR: … could not be parsed as unicode` after editing | `apply.sh` edits with `sed`; BSD `awk` mangles `\x27`. Do not edit with escapes |

## What is not finished

- **Not run on any macOS older than 26.** The minimum of 12.0 is the file's recorded value.
- **Lower than 12.0 was not tried**, so what blocks it is not known.
- **Apple Silicon: untested**, both under Rosetta 2 and as a native build. A native arm64 build would need every library and QEMU built twice.
- `qemu/build.sh` with `QEMU_DEPS_PREFIX`: not run end to end.
- The release build's minimum macOS is unmeasured.
- The licence consequences of linking LGPL libraries statically are written down, not settled: [chapter 17](17-licences.md).
