//! Synthetic Switch 2 NUP interoperability cases; no executable game code.
use nxformat_packer::{
    import::{self, synthetic_nup::*},
    inspect_archive, IntegrityPolicy, Platform, SnapshotType, WriterOptions,
};
use std::{error::Error, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("output directory required")?,
    );
    fs::create_dir_all(&root)?;
    let legacy = 0x0100_4d30_0c5a_e000;
    let ounce = 0x0400_4d30_0c5a_e001;
    let native = 0x0400_0000_0000_1000;
    let upgrade = |flags| {
        let mut p = update(ounce & !0xfff, 65536, flags);
        p.exefs = Some(exefs(0x73, ounce));
        p
    };
    let mut multi = base(native);
    for (name, index) in [("program1", 1), ("program15", 15)] {
        multi.extras.push(Node::Dir(
            name,
            vec![
                Node::Dir("exefs", exefs(0x50 + index as u8, native | index)),
                Node::Dir(
                    "romfs",
                    vec![Node::File("common.bin", vec![index as u8; 31])],
                ),
                Node::Dir(
                    "data",
                    vec![Node::File("common.bin", vec![index as u8; 17])],
                ),
            ],
        ));
    }
    let cases = [
        (
            "edition",
            vec![
                base(legacy),
                update(legacy, 65536, 0),
                upgrade(0),
                addon(legacy),
            ],
        ),
        ("standalone-edition", vec![upgrade(1), addon(legacy)]),
        ("multi-program", vec![multi, update(native, 65536, 0)]),
    ];
    let limits = import::ImportLimits::default();
    let options = WriterOptions::default();
    for (name, packages) in cases {
        for compressed in [false, true] {
            let source = root.join(format!("{name}-{compressed}.nup"));
            fs::write(&source, build(&packages, compressed))?;
            for stream in [false, true] {
                let out = root.join(format!("{name}-{compressed}-{stream}.nx"));
                import::convert_nup(
                    &source,
                    &out,
                    &import::NupOptions {
                        platform: Some(Platform::Switch2),
                        stream,
                        ..Default::default()
                    },
                    &limits,
                    &options,
                )?;
                let check = import::check_nup(&source, &out, false, &limits, &options)?;
                assert_eq!(
                    check.snapshots,
                    match name {
                        "edition" => 4,
                        "standalone-edition" => 2,
                        _ => 6,
                    }
                );
                let archive = inspect_archive(&out, &options, IntegrityPolicy::Strict)?;
                let index = archive.applications[0]
                    .snapshots
                    .iter()
                    .position(|s| {
                        if name == "multi-program" {
                            s.program_id == Some(native | 15) && s.numeric_version == 65536
                        } else {
                            s.snapshot_type == SnapshotType::Dlc
                        }
                    })
                    .ok_or("expected snapshot missing")?;
                let (path, bytes) = if name == "multi-program" {
                    ("romfs/common.bin", vec![15u8; 31])
                } else {
                    ("romfs/dlc.bin", b"dlc".to_vec())
                };
                let expected = out.with_extension("bin");
                fs::write(&expected, &bytes)?;
                fs::write(
                    out.with_extension("tsv"),
                    format!(
                        "{index}\t{path}\t{}\t0\t{}\n",
                        expected.display(),
                        bytes.len()
                    ),
                )?;
            }
        }
    }
    Ok(())
}
