//! Small, non-copyrighted packages for independent source-resource conformance.
use nxformat_packer::native::synthetic::{dxci, partition, title_entries, TitleSpec};
use std::{fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(std::env::args().nth(1).ok_or("output directory required")?);
    fs::create_dir_all(&dir)?;
    let app = 0x0100000000010000;
    let system = vec![(
        "unknown-system.bin".to_owned(),
        (0..32000).map(|n| (n * 17) as u8).collect(),
    )];
    fs::write(dir.join("original.hfs0"), partition(&system, true))?;
    // A later cart revision that carries the very same firmware.
    fs::write(
        dir.join("patch-same.dxci"),
        dxci(&[
            ("update", system.clone()),
            ("secure", title_entries(&TitleSpec::patch(app, 2, 9))),
        ]),
    )?;
    fs::write(
        dir.join("source.dxci"),
        dxci(&[
            ("update", system),
            ("secure", title_entries(&TitleSpec::application(app, 0, 4))),
        ]),
    )?;
    fs::write(
        dir.join("patch.dxci"),
        dxci(&[
            (
                "update",
                vec![("other-system.bin".to_owned(), vec![0x55; 3000])],
            ),
            ("secure", title_entries(&TitleSpec::patch(app, 1, 9))),
        ]),
    )?;
    Ok(())
}
