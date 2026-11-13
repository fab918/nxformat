//! Driver for benchmarks/restore_unreproducible.py (synthetic archives only).
use nxformat_packer::*;
use std::{path::Path, time::Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 && !(args.len() == 4 && args[3] == "--verify") {
        return Err("usage: restoration_bench ARCHIVE.nx EXTRACTED_FOLDER [--verify]".into());
    }
    let options = WriterOptions::default();
    let summary = inspect_archive(&args[1], &options, IntegrityPolicy::Strict)?;
    let application = &summary.applications[0];
    let snapshot = &application.snapshots[0];
    let key: String = snapshot
        .identity
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let manifest = format!("NXIMPORT\t1\nsnapshot\t{key}\tbase\tswitch1\t{:016x}\t{:016x}\tprimary\tbase\t0\tcomplete\tmaterialized-v1\tnone\n", application.application_id, snapshot.program_id.unwrap());
    let started = Instant::now();
    let stats = create_restoration(
        Path::new(&args[1]),
        Path::new(&args[2]),
        &[(snapshot.identity, "base".into())],
        &manifest,
        &options,
    )?;
    let elapsed = started.elapsed().as_secs_f64();
    let verified = if args.len() == 4 {
        let output = Path::new(&args[2])
            .parent()
            .unwrap()
            .join("verified-restoration.nx");
        if !restore_extraction(&args[2], output, &options)?.unchanged {
            return Err("unexpected edited source".into());
        }
        "true"
    } else {
        "null"
    };
    println!("{{\"archive_bytes\":{},\"recipe_bytes\":{},\"elapsed_seconds\":{:.6},\"compression_attempts\":{},\"exhaustive_searches\":{},\"literal_fallback_blocks\":{},\"restored_exactly\":{}}}", stats.archive_bytes, stats.recipe_bytes, elapsed, stats.compression_attempts, stats.exhaustive_searches, stats.literal_fallback_blocks, verified);
    Ok(())
}
