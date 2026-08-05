//! Resolve addresses to symbols using a firmware ELF.
//!
//! Usage: cargo run -p flashimg --example addr2sym -- <elf> <addr>...
//!
//! Addresses may be bare hex ("42119853") or prefixed ("0x42119853").

use flashimg::ElfSymbols;

fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: addr2sym <elf> <addr>...");
        return std::process::ExitCode::FAILURE;
    };

    let raw = match std::fs::read(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{path}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let syms = match ElfSymbols::parse(&raw) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{path}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    eprintln!("{} symbols, entry {:#010x}", syms.len(), syms.entry);

    for arg in args {
        let text = arg.trim_start_matches("0x");
        let Ok(addr) = u32::from_str_radix(text, 16) else {
            println!("{arg}: not a hex address");
            continue;
        };
        match syms.resolve_detailed(addr) {
            Some((sym, off, exact)) => {
                // Flag guesses loudly. A nearest-preceding match looks
                // identical to a real one and will send you debugging a
                // function the CPU was never in.
                let note = if exact {
                    String::new()
                } else {
                    format!(
                        "   [GUESS: past end of {} (size {:#x}) — not a containing symbol]",
                        sym.name, sym.size
                    )
                };
                if off == 0 {
                    println!("{addr:#010x}  {}{note}", sym.name);
                } else {
                    println!("{addr:#010x}  {}+{off:#x}{note}", sym.name);
                }
            }
            None => println!("{addr:#010x}  <no symbol>"),
        }
    }
    std::process::ExitCode::SUCCESS
}
