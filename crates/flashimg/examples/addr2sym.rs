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
        match syms.resolve(addr) {
            Some((sym, 0)) => println!("{addr:#010x}  {}", sym.name),
            Some((sym, off)) => println!("{addr:#010x}  {}+{off:#x}", sym.name),
            None => println!("{addr:#010x}  <no symbol>"),
        }
    }
    std::process::ExitCode::SUCCESS
}
