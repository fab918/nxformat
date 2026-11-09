//! Small, non-copyrighted package for independent source-resource conformance.
use nxformat_packer::{
    cli,
    native::synthetic::{dxci, partition, title_entries, TitleSpec},
};
use std::{fs, path::PathBuf};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(std::env::args().nth(1).ok_or("output directory required")?);
    fs::create_dir_all(&dir)?;
    let system = vec![(
        "unknown-system.bin".to_owned(),
        (0..32000).map(|n| (n * 17) as u8).collect(),
    )];
    fs::write(dir.join("original.hfs0"), partition(&system, true))?;
    let bytes = dxci(&[
        ("update", system),
        (
            "secure",
            title_entries(&TitleSpec::application(0x0100000000010000, 0, 4)),
        ),
    ]);
    let source = dir.join("source.dxci");
    fs::write(&source, bytes)?;
    cli::run(
        vec![
            "pack".into(),
            source.display().to_string(),
            dir.join("game.nx").display().to_string(),
        ],
        None,
        None,
    )?;
    Ok(())
}
