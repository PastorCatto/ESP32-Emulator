//! Report what a firmware file is, without booting it.
//!
//! Usage: cargo run -p flashimg --example inspect -- <file>

use flashimg::{Dropped, PartitionTable};

fn main() -> std::process::ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: inspect <firmware file>");
        return std::process::ExitCode::FAILURE;
    };

    let raw = match std::fs::read(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{path}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    println!("{path}");
    println!("  size          {} bytes ({:.2} MiB)", raw.len(), raw.len() as f64 / (1024.0 * 1024.0));

    let dropped = Dropped::identify(&raw);
    match &dropped {
        Dropped::MergedFlash(m) => {
            println!("  kind          merged flash image");
            println!("  chip          {}", m.chip);
            println!(
                "  bootloader    {} bytes at {:#x}",
                m.bootloader.total_len,
                m.chip.bootloader_offset()
            );
            describe_app(m.app.as_ref());
            print_table(&m.table);
        }
        Dropped::AppOnly(app) => {
            println!("  kind          application image (no bootloader)");
            println!("  chip          {}", app.header.chip);
            describe_app(Some(app));
        }
        Dropped::Bootloader(bl) => {
            println!("  kind          bootloader");
            println!("  chip          {}", bl.header.chip);
        }
        Dropped::Elf => println!("  kind          ELF"),
        Dropped::Unknown => {
            println!("  kind          unrecognised");
            return std::process::ExitCode::FAILURE;
        }
    }
    std::process::ExitCode::SUCCESS
}

fn describe_app(app: Option<&flashimg::AppImage>) {
    let Some(app) = app else {
        println!("  app           not found");
        return;
    };
    println!("  entry         {:#010x}", app.header.entry_addr);
    println!("  segments      {}", app.segments.len());
    if let Some(size) = app.header.flash_size {
        println!("  flash (hdr)   {size}");
    }
    match &app.descriptor {
        None => println!("  descriptor    absent"),
        Some(d) => {
            println!("  project       {}", d.project_name);
            println!("  version       {}", d.app_version);
            println!("  built         {} {}", d.build_date, d.build_time);
            println!("  idf           {}", d.idf_version);
            if let Some((a, b, c)) = d.idf_semver() {
                println!("  idf parsed    {a}.{b}.{c}");
            }
        }
    }
}

fn print_table(table: &PartitionTable) {
    println!("  partitions    {}", table.entries.len());
    for p in &table.entries {
        println!(
            "    {:<16} {:<6} {:<10} {:#010x} {:>9} bytes",
            p.label,
            p.ty.to_string(),
            p.subtype_name(),
            p.offset,
            p.size
        );
    }
}
