//! Derive a byte signature for a function from a corpus of builds.
//!
//! Firmware without a `.elf` cannot be patched by symbol, so the fallback is
//! to recognise the function by its compiled bytes. The hard part is knowing
//! which bytes are stable. A function's code contains relocated fields --
//! `l32r` literal offsets, `call8` targets -- that differ in every build, and
//! a signature that includes them matches exactly one image: the one it came
//! from, which is the one case where we did not need it.
//!
//! Guessing which bytes are volatile from the instruction encoding is one
//! option. Measuring is better. Given several builds that each ship a `.elf`,
//! this extracts the same function from each, and marks a byte as fixed only
//! where every build agrees. What comes out is a signature whose wildcards are
//! evidence rather than assumption.
//!
//! Usage:
//!   derive-signature <symbol> <elf> <bin> [<elf> <bin>]...
//!
//! Prints the consensus pattern, and for each build the offset it matched at
//! and whether that match is unique in the image -- a signature that hits
//! twice is worse than useless.

use flashimg::{Dropped, ElfSymbols};
use std::path::Path;

/// Bytes of context to dump either side of the function.
///
/// The point is to see what the signature is sitting next to. A pattern that
/// looks stable can be surrounded by padding that shifts between builds, and
/// the only way to notice is to look wider than the function itself.
const CONTEXT: usize = 64;

/// One build's copy of the function.
struct Sample {
    build: String,
    addr: u32,
    /// The function body.
    body: Vec<u8>,
    /// Body plus `CONTEXT` bytes either side, for eyeballing.
    window: Vec<u8>,
    /// Where the body starts inside `window`.
    window_start: usize,
    /// The whole app payload, for the uniqueness check.
    image: Vec<u8>,
    segments: Vec<(u32, u32, usize)>,
}

fn flash_offset(segments: &[(u32, u32, usize)], addr: u32) -> Option<usize> {
    segments.iter().find_map(|&(load, len, file)| {
        let end = load.checked_add(len)?;
        (addr >= load && addr < end).then(|| file + (addr - load) as usize)
    })
}

fn load(symbol: &str, elf_path: &Path, bin_path: &Path) -> Result<Sample, String> {
    let elf_raw = std::fs::read(elf_path).map_err(|e| format!("{}: {e}", elf_path.display()))?;
    let bin_raw = std::fs::read(bin_path).map_err(|e| format!("{}: {e}", bin_path.display()))?;

    let symbols = ElfSymbols::parse(&elf_raw).map_err(|e| format!("{}: {e}", elf_path.display()))?;
    let sym = symbols
        .find(symbol)
        .ok_or_else(|| format!("{}: no symbol {symbol}", elf_path.display()))?;
    if sym.size == 0 {
        return Err(format!("{}: {symbol} has zero size", elf_path.display()));
    }

    // A build directory ships the bare app image, not a merged flash image.
    let app = match Dropped::identify(&bin_raw) {
        Dropped::AppOnly(a) => *a,
        Dropped::MergedFlash(m) => m.app.clone().ok_or("merged image has no app")?,
        other => return Err(format!("{}: not an app image ({other:?})", bin_path.display())),
    };
    let segments: Vec<(u32, u32, usize)> = app
        .segments
        .iter()
        .map(|s| (s.load_addr, s.len, s.file_offset))
        .collect();

    let at = flash_offset(&segments, sym.address)
        .ok_or_else(|| format!("{symbol} at {:#010x} is in no segment", sym.address))?;
    let end = at + sym.size as usize;
    if end > bin_raw.len() {
        return Err(format!("{symbol} runs past the end of {}", bin_path.display()));
    }

    let wstart = at.saturating_sub(CONTEXT);
    let wend = (end + CONTEXT).min(bin_raw.len());

    Ok(Sample {
        build: bin_path
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| bin_path.display().to_string()),
        addr: sym.address,
        body: bin_raw[at..end].to_vec(),
        window: bin_raw[wstart..wend].to_vec(),
        window_start: at - wstart,
        image: bin_raw,
        segments,
    })
}

/// Bytes every sample agrees on. `None` is a wildcard.
fn consensus(samples: &[Sample]) -> Vec<Option<u8>> {
    let len = samples.iter().map(|s| s.body.len()).min().unwrap_or(0);
    (0..len)
        .map(|i| {
            let first = samples[0].body[i];
            samples.iter().all(|s| s.body[i] == first).then_some(first)
        })
        .collect()
}

fn matches_at(haystack: &[u8], at: usize, pattern: &[Option<u8>]) -> bool {
    haystack
        .get(at..at + pattern.len())
        .is_some_and(|w| w.iter().zip(pattern).all(|(&b, p)| p.is_none_or(|e| b == e)))
}

fn scan(haystack: &[u8], pattern: &[Option<u8>]) -> Vec<usize> {
    if pattern.is_empty() || haystack.len() < pattern.len() {
        return Vec::new();
    }
    (0..=haystack.len() - pattern.len())
        .filter(|&i| matches_at(haystack, i, pattern))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

fn render(pattern: &[Option<u8>]) -> String {
    pattern
        .iter()
        .map(|p| match p {
            Some(b) => format!("{b:02x}"),
            None => "??".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    let all: Vec<String> = std::env::args().skip(1).collect();
    // Everything after --against is an image with no .elf: the case the
    // signature exists for, so being able to try it here is the whole point.
    let split = all.iter().position(|a| a == "--against");
    let (args, against) = match split {
        Some(i) => (all[..i].to_vec(), all[i + 1..].to_vec()),
        None => (all.clone(), Vec::new()),
    };
    if args.len() < 3 || (args.len() - 1) % 2 != 0 {
        eprintln!(
            "usage: derive-signature <symbol> <elf> <bin> [<elf> <bin>]... \
             [--against <bin>...]"
        );
        std::process::exit(2);
    }
    let symbol = &args[0];

    let mut samples = Vec::new();
    for pair in args[1..].chunks(2) {
        match load(symbol, Path::new(&pair[0]), Path::new(&pair[1])) {
            Ok(s) => samples.push(s),
            Err(e) => eprintln!("skipped: {e}"),
        }
    }
    if samples.is_empty() {
        eprintln!("no usable builds");
        std::process::exit(1);
    }

    println!("== {symbol} across {} builds ==\n", samples.len());
    for s in &samples {
        println!("{:<30} {:#010x}  {} bytes", s.build, s.addr, s.body.len());
    }
    if std::env::var_os("DUMP_BODIES").is_some() {
        for s in &samples {
            println!("\n--- {} body ---\n{}", s.build, hex(&s.body));
        }
    }

    let sizes: Vec<usize> = samples.iter().map(|s| s.body.len()).collect();
    let uniform = sizes.iter().all(|&n| n == sizes[0]);
    if !uniform {
        println!(
            "\nNOTE: the function is not the same size in every build ({sizes:?}); \
             the consensus covers the shortest."
        );
    }

    let pattern = consensus(&samples);
    let fixed = pattern.iter().filter(|p| p.is_some()).count();
    println!(
        "\n-- consensus over {} bytes: {fixed} fixed, {} wildcard --\n{}\n",
        pattern.len(),
        pattern.len() - fixed,
        render(&pattern)
    );

    // The longest stretch of agreement is the part worth trusting; a signature
    // riddled with wildcards can match by accident.
    let mut best = (0usize, 0usize);
    let mut run = 0usize;
    for (i, p) in pattern.iter().enumerate() {
        if p.is_some() {
            run += 1;
            if run > best.1 {
                best = (i + 1 - run, run);
            }
        } else {
            run = 0;
        }
    }
    println!("longest fixed run: {} bytes at offset {}\n", best.1, best.0);

    // The consensus needs many builds. The rule needs one -- which is the
    // situation that actually arises. Check the rule against the consensus
    // here, where both are available, so it can be trusted where only the
    // rule is.
    let rule = flashimg::signature::Signature::from_xtensa(symbol, &samples[0].body);
    println!(
        "-- rule-derived from {} alone: {} of {} bits pinned --\n{}\n",
        samples[0].build,
        rule.pinned_bits(),
        samples[0].body.len() * 8,
        rule.render()
    );
    println!("-- rule signature against every build --");
    for s in &samples {
        let hits = rule.scan(&s.image);
        let want = flash_offset(&s.segments, s.addr);
        println!(
            "{:<30} {} hit(s)  {}",
            s.build,
            hits.len(),
            if hits.len() == 1 && Some(hits[0]) == want { "ok" } else { "REVIEW" }
        );
    }
    println!();

    println!("-- uniqueness across each whole image --");
    for s in &samples {
        let hits = scan(&s.image, &pattern);
        let want = flash_offset(&s.segments, s.addr);
        let ok = hits.len() == 1 && Some(hits[0]) == want;
        println!(
            "{:<30} {} hit(s){}  {}",
            s.build,
            hits.len(),
            match want {
                Some(w) if hits.contains(&w) => format!(" incl. the real one @{w:#x}"),
                Some(w) => format!(" MISSED the real one @{w:#x}"),
                None => String::new(),
            },
            if ok { "ok" } else { "REVIEW" }
        );
    }

    if !against.is_empty() {
        println!("\n-- scanning images with no .elf --");
        for path in &against {
            match std::fs::read(path) {
                Ok(raw) => {
                    // The rule signature, not the consensus: an image with no
                    // .elf is exactly the case the consensus cannot be built
                    // for, so the rule is what has to carry it.
                    let hits = rule.scan(&raw);
                    let where_ = hits
                        .iter()
                        .map(|h| format!("{h:#x}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "{:<44} {} hit(s) {}",
                        Path::new(path)
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        hits.len(),
                        where_
                    );
                    // Dump the match so it can be read against a known-good
                    // body rather than trusted because the count was 1.
                    for h in hits.iter().take(2) {
                        let end = (h + pattern.len()).min(raw.len());
                        println!("  @{h:#x}: {}", hex(&raw[*h..end]));
                    }
                }
                Err(e) => println!("{path}: {e}"),
            }
        }
    }

    println!("\n-- context window of the first build ({}) --", samples[0].build);
    let s = &samples[0];
    println!("before:\n{}", hex(&s.window[..s.window_start]));
    println!("body:\n{}", hex(&s.window[s.window_start..s.window_start + s.body.len()]));
    println!("after:\n{}", hex(&s.window[s.window_start + s.body.len()..]));
}
