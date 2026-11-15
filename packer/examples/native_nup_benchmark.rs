//! Reproducible synthetic read corpus; no game data or executable code.
use nxformat_packer::{
    import::{self, synthetic_nup::*},
    CompressionMode, Platform, WriterOptions,
};
use std::{error::Error, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("output directory required")?,
    );
    fs::create_dir_all(&root)?;
    for compressed in [false, true] {
        let label = if compressed { "zstd" } else { "raw" };
        let mut package = base(0x0400_0000_0000_1000);
        let mut files = Vec::new();
        for index in 0..8 {
            let mut state = 0x4e58_2026_u64 + index;
            let mut bytes = vec![0; 4 * 1024 * 1024];
            for (offset, byte) in bytes.iter_mut().enumerate() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                // Four compressible and four incompressible files, distinct content.
                *byte = if index < 4 {
                    ((offset / 256 + index as usize) % 251) as u8
                } else {
                    state as u8
                };
            }
            let name = Box::leak(format!("asset-{index}.bin").into_boxed_str());
            files.push(Node::File(name, bytes));
        }
        package.romfs = Some(files);
        let nup = root.join(format!("{label}.nup"));
        fs::write(&nup, build(&[package], compressed))?;
        import::convert_nup(
            &nup,
            &root.join(format!("{label}.nx")),
            &import::NupOptions {
                platform: Some(Platform::Switch2),
                stream: true,
                ..Default::default()
            },
            &import::ImportLimits::for_nup(),
            &WriterOptions {
                compression_mode: if compressed {
                    CompressionMode::ZstandardAuto
                } else {
                    CompressionMode::RawOnly
                },
                ..Default::default()
            },
        )?;
    }
    Ok(())
}
