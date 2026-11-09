//! Preserve source-system bytes without interpreting them as game content.
use super::container::{parse_partition, u64_at, PartitionEntry, Source};
use super::{InventoryOptions, NativeError};
use crate::{DirectoryInput, EntryInput, FileInput, Platform, SourceResourceInput};
use std::{
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub(crate) fn update_partition(
    path: &Path,
    options: &InventoryOptions,
) -> Result<Option<PartitionEntry>, NativeError> {
    let file = crate::source_file::open(path)?;
    let len = file.len()?;
    if len < 0x140 {
        return Ok(None);
    }
    let mut source = Source::new(file, len);
    let mut header = [0u8; 0x140];
    source.read_exact_at(0, &mut header)?;
    if &header[0x100..0x104] != b"DXCI" {
        return Ok(None);
    }
    for part in parse_partition(&mut source, u64_at(&header, 0x130), len, true, options)? {
        if part.name == "update" {
            let entries = parse_partition(
                &mut source,
                part.offset,
                part.offset + part.size,
                true,
                options,
            )?;
            return Ok((!entries.is_empty()).then_some(part));
        }
    }
    Ok(None)
}

pub(crate) fn source_resources(
    path: &Path,
    platform: Platform,
    application_id: u64,
    options: &InventoryOptions,
) -> Result<Vec<SourceResourceInput>, NativeError> {
    let Some(part) = update_partition(path, options)? else {
        return Ok(Vec::new());
    };
    let mut file = crate::source_file::open(path)?;
    file.seek(SeekFrom::Start(part.offset))?;
    Ok(vec![SourceResourceInput {
        platform,
        application_id,
        kind: 1,
        root: DirectoryInput::new(vec![EntryInput::file(
            "partition.hfs0",
            FileInput::new(part.size, file.take(part.size)),
        )]),
    }])
}
