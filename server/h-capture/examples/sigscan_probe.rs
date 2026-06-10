//! Offset-discovery helper for static-binary (Bun/BoringSSL) eBPF targets.
//!
//! Scans an ELF for a byte signature and prints the matching **file offsets** —
//! the values to put in a `[[sources.targets]]` `write_sig` / `read_sig` config
//! (the loader resolves the same offsets at attach time via the same scanner).
//!
//! Usage:
//!   cargo run -p h-capture --example sigscan_probe -- <binary> "<hex pattern>"
//! e.g.
//!   cargo run -p h-capture --example sigscan_probe -- ~/.bun/bin/bun "55 41 57 ?? 48 8b"
//!
//! A unique single match is what a good signature yields; zero means the
//! pattern is wrong for this build, and many means it is too loose.

use h_capture::ebpf::sigscan::{scan_elf_executable, Signature};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(binary), Some(pattern)) = (args.next(), args.next()) else {
        eprintln!("usage: sigscan_probe <binary> \"<hex pattern with ?? wildcards>\"");
        std::process::exit(2);
    };

    let data = match std::fs::read(&binary) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("read {binary}: {e}");
            std::process::exit(1);
        }
    };
    let Some(sig) = Signature::parse(&pattern) else {
        eprintln!("malformed pattern: {pattern:?}");
        std::process::exit(2);
    };

    let hits = scan_elf_executable(&data, &sig);
    println!(
        "{binary}: {} byte signature, {} match(es) in executable segments",
        sig.len(),
        hits.len()
    );
    for off in &hits {
        println!("  offset {off:#x} ({off})");
    }
    match hits.len() {
        1 => println!("OK: unique offset — usable as a uprobe attach point"),
        0 => {
            println!("MISS: no match (wrong build / signature)");
            std::process::exit(1);
        }
        n => println!("AMBIGUOUS: {n} matches — refine the signature for a unique hit"),
    }
}
