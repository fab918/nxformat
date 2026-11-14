//! Inspect local NUP metadata. --audit additionally validates all executable
//! structures and staged associations without writing a full NX archive.
use nxformat_packer::{import, CancellationToken, SnapshotType, WriterOptions};
use std::{error::Error, path::Path};

fn inspect(path: &Path, audit: bool) -> Result<(), Box<dyn Error>> {
    let limits = import::ImportLimits::for_nup();
    let token = CancellationToken::default();
    let info = import::inspect_nup(path, &limits, &token)?;
    println!(
        "PASS\t{}\t{:?}\t{:016x}\t{}\t{} packages",
        path.display(),
        info.platform,
        info.application_id,
        info.title.as_deref().unwrap_or_default(),
        info.packages.len()
    );
    for package in &info.packages {
        println!(
            "PACKAGE\t{:?}\t{}\t{}",
            package.kind,
            package.version,
            package.display_version.as_deref().unwrap_or_default()
        );
    }
    if audit {
        let options = WriterOptions {
            max_temporary_disk_bytes: 4 * 1024 * 1024 * 1024,
            ..WriterOptions::default()
        };
        let stage = import::stage_nup_with_options(
            path,
            &std::env::temp_dir(),
            &import::NupOptions {
                platform: Some(info.platform),
                stream: true,
                ..import::NupOptions::default()
            },
            &limits,
            &options,
        )?;
        let mut modules = 0;
        for snapshot in &stage.manifest.snapshots {
            if snapshot.snapshot_type == SnapshotType::Dlc {
                continue;
            }
            let facts = import::audit_executables(
                &stage.package().join(&snapshot.root),
                snapshot.npdm_profile,
                &limits,
                &token,
            )?;
            if facts.npdm.as_ref().map(|p| p.program_id) != snapshot.program_id {
                return Err(format!(
                    "{}: staged NPDM disagrees with declared Program",
                    snapshot.key
                )
                .into());
            }
            if !facts.modules.contains_key("main") {
                return Err(format!("{}: missing executable main", snapshot.key).into());
            }
            modules += facts.modules.len();
        }
        println!(
            "AUDIT\t{}\tsnapshots={}\tmodules={}\tstreamed_files={}\tstaged_bytes={}",
            path.display(),
            stage.manifest.snapshots.len(),
            modules,
            stage.streamed_files().count(),
            stage.staged_bytes
        );
    }
    Ok(())
}

fn main() {
    let mut audit = false;
    let mut paths = Vec::new();
    for arg in std::env::args_os().skip(1) {
        if arg == "--audit" {
            audit = true;
        } else {
            paths.push(arg);
        }
    }
    if paths.is_empty() {
        eprintln!("usage: inspect_nup [--audit] FILE.nup...");
        std::process::exit(2);
    }
    let mut failed = false;
    for path in paths {
        let path = Path::new(&path);
        if let Err(error) = inspect(path, audit) {
            eprintln!("FAIL\t{}\t{error}", path.display());
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
}
