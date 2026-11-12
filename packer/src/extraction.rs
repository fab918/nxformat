//! Bounded, read-only native filesystem extraction. BKTR maps section-relative
//! virtual offsets to the exact base dependency, never to a guessed version.
use super::*;
use crate::native::container::{u32_at, u64_at};
use crate::native::nca::ParsedNca;

const NONE: u32 = u32::MAX;
const MAX_NODES: usize = 1_000_000;
const MAX_PATH_BYTES: usize = 64 * 1024 * 1024;

fn parse(reader: &mut NativeReader<'_>) -> Result<ParsedNca, WriterError> {
    let size = reader.size;
    native::nca::parse_nca(&mut Source::new(reader, size), 0, size)
        .map_err(|_| bad("invalid or encrypted native NCA header"))
}

/// Resolve the Program actually supplied by a snapshot. A data-only update
/// inherits it through exact RequiredBase identities, never by version/name.
pub(super) fn content_file<'a>(
    path: &Path,
    archive: &'a SourceArchive,
    snapshot: &'a SourceSnapshot,
    budget: &MemoryBudget,
    options: &WriterOptions,
) -> Result<(u32, &'a SourceSnapshot), WriterError> {
    let mut current = snapshot;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current.snapshot.identity) {
            return Err(bad("native Program inheritance cycle"));
        }
        let mut file = File::open(path)?;
        let report = inspect_base(
            path,
            &mut file,
            archive,
            current,
            &InventoryOptions::default(),
            options,
            &mut NativeAddStats::default(),
            &Rc::new(Cell::new(budget.limit.saturating_sub(budget.current))),
        )?;
        let meta = report
            .metas
            .iter()
            .find(|m| m.meta.meta_content_id == Some(current.snapshot.content_id))
            .ok_or_else(|| bad("snapshot CNMT is absent"))?;
        let id = if current.snapshot.snapshot_type == SnapshotType::Dlc as u8 {
            let mut data = meta
                .meta
                .contents
                .iter()
                .filter(|r| r.record_type == native::ContentRecordType::Data);
            let id = data
                .next()
                .ok_or_else(|| invalid("DLC has no Data NCA"))?
                .content_id;
            if data.next().is_some() {
                return Err(invalid(
                    "multiple DLC Data NCAs are not supported; export raw NCAs",
                ));
            }
            Some(id)
        } else {
            meta.meta
                .programs
                .iter()
                .find(|p| Some(p.program_id) == current.program_id)
                .map(|p| p.content_id)
        };
        if let Some(id) = id {
            let name = format!("{}.nca", crate::import::hex(&id));
            let file = snapshot_entries(archive, current)?
                .into_iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(&name))
                .map(|(_, id)| id)
                .ok_or_else(|| bad("snapshot content NCA is absent"))?;
            return Ok((file, current));
        }
        if current.snapshot.snapshot_type != SnapshotType::Update as u8
            && current.snapshot.snapshot_type != SnapshotType::StandaloneUpdate as u8
        {
            return Err(bad("snapshot Program NCA association is absent"));
        }
        let mut dependencies = current
            .snapshot
            .dependencies
            .iter()
            .filter(|d| d.kind == DependencyKind::RequiredBase);
        let dependency = dependencies
            .next()
            .ok_or_else(|| bad("inherited Program requires its exact base snapshot"))?;
        if dependencies.next().is_some() {
            return Err(bad("ambiguous native Program inheritance"));
        }
        current = archive
            .snapshots
            .iter()
            .find(|s| {
                s.snapshot.identity == dependency.target_snapshot_identity
                    && s.key == snapshot.key
                    && s.program_id == snapshot.program_id
            })
            .ok_or_else(|| bad("inherited Program base snapshot is absent"))?;
    }
}

#[derive(Clone, Debug)]
struct Mapping {
    start: u64,
    physical: u64,
    patch: bool,
}

fn relocations(
    src: &mut Source<'_>,
    nca: &ParsedNca,
    slot: usize,
    limit: u64,
) -> Result<(Vec<Mapping>, u64), WriterError> {
    let s = &nca.summary.sections[slot];
    let p = &nca.patch_data[slot];
    if &p[16..20] != b"BKTR" || u32_at(p, 20) != 1 {
        return Err(invalid("unsupported BKTR relocation header"));
    }
    let offset = u64_at(p, 0);
    let size = u64_at(p, 8);
    if offset
        .checked_add(size)
        .map_or(true, |end| end > s.end - s.start)
    {
        return Err(bad("BKTR table exceeds section"));
    }
    let bytes = src
        .read_vec_at(s.start + offset, size, limit / 8, "BKTR relocations")
        .map_err(|e| bad(e.to_string()))?;
    if bytes.len() < 0x4000 {
        return Err(bad("truncated BKTR root"));
    }
    let buckets = u32_at(&bytes, 4) as usize;
    let total = u64_at(&bytes, 8);
    if buckets == 0 || buckets > 0x3ff0 / 8 || (buckets + 1) * 0x4000 != bytes.len() {
        return Err(invalid("unsupported or malformed BKTR bucket tree"));
    }
    let mut mappings = Vec::new();
    for b in 0..buckets {
        let bucket = &bytes[(b + 1) * 0x4000..(b + 2) * 0x4000];
        let count = u32_at(bucket, 4) as usize;
        let end = u64_at(bucket, 8);
        let expected_end = if b + 1 == buckets {
            total
        } else {
            u64_at(&bytes, 16 + (b + 1) * 8)
        };
        if count == 0 || count > 0x3ff0 / 20 || end != expected_end {
            return Err(bad("invalid BKTR bucket size or end"));
        }
        for i in 0..count {
            let at = 16 + i * 20;
            let start = u64_at(bucket, at);
            let flag = u32_at(bucket, at + 16);
            if flag > 1
                || start >= end
                || (i == 0 && start != u64_at(&bytes, 16 + b * 8))
                || mappings.last().is_some_and(|m: &Mapping| m.start >= start)
            {
                return Err(bad("invalid BKTR relocation order"));
            }
            mappings.push(Mapping {
                start,
                physical: u64_at(bucket, at + 8),
                patch: flag == 1,
            });
        }
    }
    if mappings.first().map(|m| m.start) != Some(0) || mappings.len() != u32_at(p, 24) as usize {
        return Err(bad("BKTR entries do not cover the virtual section"));
    }
    Ok((mappings, total))
}

struct SectionReader<R: Read + Seek> {
    patch: R,
    base: Option<R>,
    patch_start: u64,
    base_start: u64,
    mappings: Vec<Mapping>,
    len: u64,
    position: u64,
}
impl<R: Read + Seek> Read for SectionReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let (physical, patch, end) = if self.mappings.is_empty() {
            (self.position, true, self.len)
        } else {
            let i = self.mappings.partition_point(|m| m.start <= self.position) - 1;
            let m = &self.mappings[i];
            (
                m.physical + self.position - m.start,
                m.patch,
                self.mappings.get(i + 1).map_or(self.len, |m| m.start),
            )
        };
        let n = buf.len().min((end - self.position) as usize);
        let (reader, start) = if patch {
            (&mut self.patch, self.patch_start)
        } else {
            (
                self.base
                    .as_mut()
                    .ok_or_else(|| io::Error::other("BKTR base is absent"))?,
                self.base_start,
            )
        };
        reader.seek(SeekFrom::Start(start + physical))?;
        reader.read_exact(&mut buf[..n])?;
        self.position += n as u64;
        Ok(n)
    }
}
impl<R: Read + Seek> Seek for SectionReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.position as i128 + n as i128,
            SeekFrom::End(n) => self.len as i128 + n as i128,
        };
        if position < 0 || position > self.len as i128 {
            return Err(io::Error::other("section seek out of bounds"));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}

fn region(data: &[u8], section_len: u64) -> Result<(u64, u64), WriterError> {
    if &data[..4] != b"IVFC" {
        return Err(invalid("unsupported RomFS integrity format"));
    }
    let count = u32_at(data, 12) as usize;
    if !(2..=7).contains(&count) {
        return Err(bad("invalid IVFC level count"));
    }
    let at = 16 + (count - 2) * 24;
    let (off, size) = (u64_at(data, at), u64_at(data, at + 8));
    if off.checked_add(size).map_or(true, |end| end > section_len) {
        return Err(bad("RomFS exceeds virtual section"));
    }
    Ok((off, size))
}

// Stream every level, including the data level; retain at most one hash block.
fn verify_ivfc(
    reader: &mut SectionReader<impl Read + Seek>,
    data: &[u8],
    options: &WriterOptions,
) -> Result<(), WriterError> {
    region(data, reader.len)?;
    let master_size = u32_at(data, 8) as usize;
    if master_size == 0 || master_size > 32 {
        return Err(invalid("unsupported IVFC master hash size"));
    }
    let count = u32_at(data, 12) as usize - 1;
    for i in 0..count {
        let at = 16 + i * 24;
        let (off, size, log2) = (
            u64_at(data, at),
            u64_at(data, at + 8),
            u32_at(data, at + 16),
        );
        if !(9..=24).contains(&log2) || off.checked_add(size).map_or(true, |e| e > reader.len) {
            return Err(bad("invalid IVFC level bounds"));
        }
        let block = 1u64 << log2;
        if block > options.max_memory_bytes / 8 {
            return Err(limit_error("IVFC block exceeds memory budget"));
        }
        let hashes = size
            .div_ceil(block)
            .checked_mul(32)
            .ok_or_else(|| bad("IVFC size overflow"))?;
        let parent = if i == 0 {
            if hashes > master_size as u64 {
                return Err(bad("IVFC master table too small"));
            }
            None
        } else {
            let prev = at - 24;
            if hashes > u64_at(data, prev + 8) {
                return Err(bad("IVFC parent table too small"));
            }
            Some(u64_at(data, prev))
        };
        let mut buf = vec![0; block as usize];
        let mut hash_page = vec![0u8; 32 * 1024];
        let mut hash_page_start = u64::MAX;
        for b in 0..size.div_ceil(block) {
            cancelled(options)?;
            let n = (size - b * block).min(block) as usize;
            buf.fill(0);
            reader.seek(SeekFrom::Start(off + b * block))?;
            reader.read_exact(&mut buf[..n])?;
            let mut expected = [0; 32];
            if let Some(parent) = parent {
                let page_start = (b * 32 / 32768) * 32768;
                if hash_page_start != page_start {
                    let n = (hashes - page_start).min(32768) as usize;
                    reader.seek(SeekFrom::Start(parent + page_start))?;
                    reader.read_exact(&mut hash_page[..n])?;
                    hash_page_start = page_start;
                }
                let at = (b * 32 - page_start) as usize;
                expected.copy_from_slice(&hash_page[at..at + 32]);
            } else {
                expected.copy_from_slice(&data[0xc0..0xe0]);
            }
            if sha256(&buf) != expected {
                return Err(bad(format!("IVFC level {i} block {b} hash mismatch")));
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct RomEntry {
    path: String,
    range: Option<(u64, u64)>,
}

fn romfs_entries(
    reader: &mut SectionReader<impl Read + Seek>,
    data: &[u8],
    metadata_limit: u64,
) -> Result<Vec<RomEntry>, WriterError> {
    let (base, size) = region(data, reader.len)?;
    if size < 0x50 {
        return Err(bad("truncated RomFS header"));
    }
    let mut source = Source::new(reader, base + size);
    let mut header = [0; 0x50];
    source
        .read_exact_at(base, &mut header)
        .map_err(|e| bad(e.to_string()))?;
    if u64_at(&header, 0) != 0x50 {
        return Err(bad("invalid RomFS header"));
    }
    let data_off = u64_at(&header, 0x48);
    if data_off > size {
        return Err(bad("RomFS data offset out of bounds"));
    }
    let mut tables = Vec::new();
    let mut charged = 0u64;
    for at in [0x18, 0x38] {
        let (off, len) = (u64_at(&header, at), u64_at(&header, at + 8));
        charged = charged
            .checked_add(len)
            .ok_or_else(|| bad("RomFS table size overflow"))?;
        if off < 0x50
            || off.checked_add(len).map_or(true, |e| e > size)
            || charged > metadata_limit / 4
        {
            return Err(limit_error(
                "RomFS tables exceed bounds or metadata memory budget",
            ));
        }
        tables.push(
            source
                .read_vec_at(base + off, len, metadata_limit / 4, "RomFS metadata")
                .map_err(|e| bad(e.to_string()))?,
        );
    }
    let (dirs, files) = (&tables[0], &tables[1]);
    let mut result = Vec::new();
    let mut pending = vec![(0u32, 0u32, String::new())];
    let mut seen_dirs = HashSet::new();
    let mut seen_files = HashSet::new();
    let mut paths = 0usize;
    let mut names = HashSet::new();
    while let Some((id, parent, prefix)) = pending.pop() {
        if !seen_dirs.insert(id) || result.len() >= MAX_NODES {
            return Err(bad("RomFS directory cycle or node limit"));
        }
        let at = id as usize;
        if at.checked_add(24).map_or(true, |e| e > dirs.len()) || id % 4 != 0 {
            return Err(bad("invalid RomFS directory offset"));
        }
        if u32_at(dirs, at) != parent {
            return Err(bad("RomFS directory parent mismatch"));
        }
        let len = u32_at(dirs, at + 20) as usize;
        let end = at
            .checked_add(24 + len)
            .filter(|&e| e <= dirs.len())
            .ok_or_else(|| bad("RomFS directory name out of bounds"))?;
        let name = std::str::from_utf8(&dirs[at + 24..end])
            .map_err(|_| bad("invalid RomFS directory name"))?;
        let path = if id == 0 {
            if !name.is_empty() || u32_at(dirs, at + 4) != NONE {
                return Err(bad("invalid RomFS root"));
            }
            String::new()
        } else {
            child_path(&prefix, name)?
        };
        paths = paths
            .checked_add(path.len())
            .ok_or_else(|| bad("path size overflow"))?;
        if !path.is_empty() {
            if !names.insert(path.to_lowercase()) {
                return Err(bad("RomFS portable path collision"));
            }
            result.push(RomEntry {
                path: path.clone(),
                range: None,
            });
        }
        let sibling = u32_at(dirs, at + 4);
        if sibling != NONE {
            pending.push((sibling, parent, prefix));
        }
        let child = u32_at(dirs, at + 8);
        if child != NONE {
            pending.push((child, id, path.clone()));
        }
        let mut file = u32_at(dirs, at + 12);
        while file != NONE {
            let at = file as usize;
            if !seen_files.insert(file)
                || result.len() >= MAX_NODES
                || at.checked_add(32).map_or(true, |e| e > files.len())
                || file % 4 != 0
            {
                return Err(bad("RomFS file cycle, offset or node limit"));
            }
            if u32_at(files, at) != id {
                return Err(bad("RomFS file parent mismatch"));
            }
            let len = u32_at(files, at + 28) as usize;
            let end = at
                .checked_add(32 + len)
                .filter(|&e| e <= files.len())
                .ok_or_else(|| bad("RomFS filename out of bounds"))?;
            let name = std::str::from_utf8(&files[at + 32..end])
                .map_err(|_| bad("invalid RomFS filename"))?;
            let name = child_path(&path, name)?;
            if !names.insert(name.to_lowercase()) {
                return Err(bad("RomFS portable path collision"));
            }
            paths = paths
                .checked_add(name.len())
                .ok_or_else(|| bad("path size overflow"))?;
            let (off, len) = (u64_at(files, at + 8), u64_at(files, at + 16));
            if data_off
                .checked_add(off)
                .and_then(|v| v.checked_add(len))
                .map_or(true, |e| e > size)
            {
                return Err(bad("RomFS file data out of bounds"));
            }
            result.push(RomEntry {
                path: name,
                range: Some((base + data_off + off, len)),
            });
            file = u32_at(files, at + 4);
            if paths > MAX_PATH_BYTES
                || (paths as u64).saturating_mul(8) + (result.len() as u64) * 512
                    > metadata_limit / 2
            {
                return Err(limit_error("RomFS path memory budget exceeded"));
            }
        }
        // Conservatively account for traversal nodes, sets, Strings and output paths.
        if paths > MAX_PATH_BYTES
            || (paths as u64).saturating_mul(8) + (result.len() as u64) * 512 > metadata_limit / 2
        {
            return Err(limit_error("RomFS traversal memory budget exceeded"));
        }
    }
    Ok(result)
}

fn child_path(parent: &str, name: &str) -> Result<String, WriterError> {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return Err(bad("invalid RomFS path component"));
    }
    let path = if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    };
    crate::import::portable_path(&path, &crate::import::ImportLimits::default())?;
    Ok(path)
}

pub(super) fn extract_native(
    path: &Path,
    archive: &SourceArchive,
    snapshot: &SourceSnapshot,
    destination: &Path,
    budget: &MemoryBudget,
    options: &WriterOptions,
) -> Result<(), WriterError> {
    native_view(path, archive, snapshot, destination, budget, options, true).map(|_| ())
}

pub(in crate::writer::repack) fn collect_native_ranges(
    path: &Path,
    archive: &SourceArchive,
    snapshot: &SourceSnapshot,
    budget: &MemoryBudget,
    options: &WriterOptions,
) -> Result<Vec<super::super::restoration::RestoreRange>, WriterError> {
    native_view(
        path,
        archive,
        snapshot,
        Path::new(""),
        budget,
        options,
        false,
    )
}

fn physical_range(
    reader: &SectionReader<NativeReader<'_>>,
    index: u32,
    virtual_offset: u64,
) -> Result<(u32, u64, u64), WriterError> {
    if reader.mappings.is_empty() {
        return Ok((index, reader.patch_start + virtual_offset, reader.len));
    }
    let i = reader
        .mappings
        .partition_point(|m| m.start <= virtual_offset)
        - 1;
    let m = &reader.mappings[i];
    let file = if m.patch {
        index
    } else {
        reader
            .base
            .as_ref()
            .ok_or_else(|| bad("missing BKTR base"))?
            .extents[0]
            .file
    };
    let start = if m.patch {
        reader.patch_start
    } else {
        reader.base_start
    };
    Ok((
        file,
        start + m.physical + virtual_offset - m.start,
        reader.mappings.get(i + 1).map_or(reader.len, |m| m.start),
    ))
}

fn native_view(
    path: &Path,
    archive: &SourceArchive,
    snapshot: &SourceSnapshot,
    destination: &Path,
    budget: &MemoryBudget,
    options: &WriterOptions,
    export: bool,
) -> Result<Vec<super::super::restoration::RestoreRange>, WriterError> {
    use super::super::restoration::RestoreRange;
    let mut ranges = Vec::new();
    // Three transports can coexist while resolving a base: patch, base and
    // the CNMT inventory. Reserve their frame/decoded blocks and decoder windows.
    let transport = archive
        .blocks
        .iter()
        .map(|b| b.logical_size as u64 + b.stored_size as u64)
        .max()
        .unwrap_or(0);
    let workspace = transport.saturating_add(2 * 1024 * 1024).saturating_mul(3) + 256 * 1024;
    let budget = &MemoryBudget {
        current: budget.current.saturating_add(workspace),
        peak: 0,
        limit: budget.limit,
    };
    if budget.current >= budget.limit {
        return Err(limit_error(
            "native extraction transport exceeds memory budget",
        ));
    }
    let (index, content_snapshot) = content_file(path, archive, snapshot, budget, options)?;
    let mut patch_file = File::open(path)?;
    let mut base_file = File::open(path)?;
    let mut patch = snapshot_file_reader(&mut patch_file, archive, index, options)?;
    let nca = parse(&mut patch)?;
    let Some(slot) = nca.summary.sections.iter().position(|s| s.fs_type == 0) else {
        if nca.summary.content_type != native::NcaContentType::Program {
            return Err(invalid("native Data NCA has no RomFS section"));
        }
        drop(patch);
        if export {
            crate::cli::emit(
                true,
                "Native Program NCA has no RomFS section; exporting its verified ExeFS only."
                    .into(),
            );
        }
        return native_exefs_view(
            path,
            archive,
            snapshot,
            destination,
            budget,
            options,
            export,
        );
    };
    let section = &nca.summary.sections[slot];
    if section.sparse_flattened || nca.unsupported_storage[slot] {
        return Err(invalid("sparse RomFS cannot be extracted; export raw NCAs"));
    }
    if section.hash_type != 3 {
        return Err(invalid("only IVFC RomFS is supported; export raw NCAs"));
    }
    let limit = budget.limit.saturating_sub(budget.current);
    let (mappings, len) = if section.has_patch_info {
        let size = patch.size;
        relocations(&mut Source::new(&mut patch, size), &nca, slot, limit)?
    } else {
        (Vec::new(), section.end - section.start)
    };
    let mut base_start = 0;
    let base = if mappings.iter().any(|m| !m.patch) {
        let mut dependencies = content_snapshot
            .snapshot
            .dependencies
            .iter()
            .filter(|d| d.kind == DependencyKind::RequiredBase);
        let dependency = dependencies.next().ok_or_else(|| invalid("BKTR requires its exact base snapshot; rebuild the original package or export raw NCAs"))?;
        if dependencies.next().is_some() {
            return Err(bad("ambiguous BKTR base dependency"));
        }
        let base_snapshot = archive
            .snapshots
            .iter()
            .find(|s| {
                s.snapshot.identity == dependency.target_snapshot_identity
                    && s.key == snapshot.key
                    && s.program_id == snapshot.program_id
            })
            .ok_or_else(|| bad("BKTR base snapshot or Program association is absent"))?;
        let (id, _) = content_file(path, archive, base_snapshot, budget, options)?;
        let mut base = snapshot_file_reader(&mut base_file, archive, id, options)?;
        let parsed = parse(&mut base)?;
        if parsed
            .summary
            .sections
            .iter()
            .enumerate()
            .any(|(i, s)| s.fs_type == 0 && (s.sparse_flattened || parsed.unsupported_storage[i]))
        {
            return Err(invalid(
                "BKTR base has unsupported sparse or compressed storage",
            ));
        }
        let section = parsed
            .summary
            .sections
            .iter()
            .find(|s| s.fs_type == 0 && !s.has_patch_info)
            .ok_or_else(|| invalid("BKTR base must have a standalone RomFS section"))?;
        base_start = section.start;
        for (i, m) in mappings.iter().enumerate().filter(|(_, m)| !m.patch) {
            let end = mappings.get(i + 1).map_or(len, |m| m.start);
            if m.physical
                .checked_add(end - m.start)
                .map_or(true, |e| e > section.end - section.start)
            {
                return Err(bad("BKTR mapping exceeds base section"));
            }
        }
        Some(base)
    } else {
        None
    };
    for (i, m) in mappings.iter().enumerate().filter(|(_, m)| m.patch) {
        let end = mappings.get(i + 1).map_or(len, |m| m.start);
        if m.physical
            .checked_add(end - m.start)
            .map_or(true, |e| e > section.end - section.start)
        {
            return Err(bad("BKTR mapping exceeds patch section"));
        }
    }
    let mut reader = SectionReader {
        patch,
        base,
        patch_start: section.start,
        base_start,
        mappings,
        len,
        position: 0,
    };
    let largest_block = (0..u32_at(&nca.hash_data[slot], 12).saturating_sub(1).min(6))
        .map(|i| u32_at(&nca.hash_data[slot], 32 + i as usize * 24))
        .max()
        .unwrap_or(0);
    if largest_block > 24 || (1u64 << largest_block) + 32768 > limit / 2 {
        return Err(limit_error(
            "IVFC workspace exceeds remaining memory budget",
        ));
    }
    if export {
        verify_ivfc(&mut reader, &nca.hash_data[slot], options)?;
    }
    let entries = romfs_entries(&mut reader, &nca.hash_data[slot], limit)?;
    let romfs = destination.join("romfs");
    if export {
        std::fs::create_dir(&romfs)?;
    }
    if export {
        options
            .progress
            .add_total(entries.iter().filter_map(|e| e.range.map(|(_, n)| n)).sum());
    }
    for entry in entries {
        cancelled(options)?;
        let target = crate::import::join(&romfs, &entry.path);
        if let Some((off, size)) = entry.range {
            let mut done = 0;
            while !export && done < size {
                let virtual_offset = off + done;
                let (file, physical, end) = physical_range(&reader, index, virtual_offset)?;
                let len = (size - done).min(end - virtual_offset);
                ranges.push(RestoreRange {
                    file,
                    offset: physical,
                    len,
                    path: format!("romfs/{}", entry.path),
                    file_offset: done,
                    hash_size: 0,
                    hash_offset: 0,
                });
                done += len;
            }
            if !export {
                // The largest incompressible metadata is the IVFC leaf hash
                // table. A hash wholly covered by one exported file can be
                // recomputed instead of storing 32 random bytes in the recipe.
                let layers = u32_at(&nca.hash_data[slot], 12) as usize;
                if layers >= 3 {
                    let at = 16 + (layers - 2) * 24;
                    let data_offset = u64_at(&nca.hash_data[slot], at);
                    let parent_offset = u64_at(&nca.hash_data[slot], at - 24);
                    let block = 1u64 << u32_at(&nca.hash_data[slot], at + 16);
                    if off >= data_offset {
                        let mut begin = data_offset + (off - data_offset).div_ceil(block) * block;
                        while begin.checked_add(block).is_some_and(|e| e <= off + size) {
                            let hash_at = parent_offset + (begin - data_offset) / block * 32;
                            let mut done = 0;
                            while done < 32 {
                                let (file, physical, end) =
                                    physical_range(&reader, index, hash_at + done)?;
                                let len = (32 - done).min(end - hash_at - done);
                                let path = format!("romfs/{}", entry.path);
                                if let Some(previous) = ranges.last_mut().filter(|r| {
                                    r.file == file
                                        && r.hash_size == block as u32
                                        && r.hash_offset == 0
                                        && r.len % 32 == 0
                                        && done == 0
                                        && len == 32
                                        && r.offset + r.len == physical
                                        && r.path == path
                                        && r.file_offset + r.len / 32 * block == begin - off
                                }) {
                                    previous.len += 32;
                                } else {
                                    ranges.push(RestoreRange {
                                        file,
                                        offset: physical,
                                        len,
                                        path,
                                        file_offset: begin - off,
                                        hash_size: block as u32,
                                        hash_offset: done as u8,
                                    });
                                }
                                done += len;
                            }
                            if ranges.len() as u64 * (256 + entry.path.len() as u64 * 2) > limit / 3
                            {
                                return Err(limit_error(
                                    "derived restoration hashes exceed memory budget",
                                ));
                            }
                            begin += block;
                        }
                    }
                }
            }
            if export {
                reader.seek(SeekFrom::Start(off))?;
                export_exefs_file(&mut reader, &target, size, None, options)?;
            }
        } else if export {
            std::fs::create_dir(target)?;
        }
    }
    drop(reader);
    ranges.extend(native_exefs_view(
        path,
        archive,
        snapshot,
        destination,
        budget,
        options,
        export,
    )?);
    Ok(ranges)
}

fn native_exefs_view(
    path: &Path,
    archive: &SourceArchive,
    snapshot: &SourceSnapshot,
    destination: &Path,
    budget: &MemoryBudget,
    options: &WriterOptions,
    export: bool,
) -> Result<Vec<super::super::restoration::RestoreRange>, WriterError> {
    use super::super::restoration::RestoreRange;
    let mut ranges = Vec::new();
    if snapshot.snapshot.snapshot_type != SnapshotType::Dlc as u8 {
        let mut file = File::open(path)?;
        let (index, entries) = native_exefs(path, &mut file, archive, snapshot, budget, options)?;
        let exefs = destination.join("exefs");
        if export {
            std::fs::create_dir(&exefs)?;
        }
        if export {
            options
                .progress
                .add_total(entries.iter().map(|e| e.size).sum());
        }
        let mut reader = snapshot_file_reader(&mut file, archive, index, options)?;
        for entry in entries {
            crate::import::portable_path(&entry.name, &crate::import::ImportLimits::default())?;
            ranges.push(RestoreRange {
                file: index,
                offset: entry.offset,
                len: entry.size,
                path: format!("exefs/{}", entry.name),
                file_offset: 0,
                hash_size: 0,
                hash_offset: 0,
            });
            if export {
                reader.seek(SeekFrom::Start(entry.offset))?;
                export_exefs_file(
                    &mut reader,
                    &exefs.join(entry.name),
                    entry.size,
                    None,
                    options,
                )?;
            }
        }
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn view(bytes: Vec<u8>) -> SectionReader<Cursor<Vec<u8>>> {
        let len = bytes.len() as u64;
        SectionReader {
            patch: Cursor::new(bytes),
            base: None,
            patch_start: 0,
            base_start: 0,
            mappings: Vec::new(),
            len,
            position: 0,
        }
    }

    fn image() -> (Vec<u8>, [u8; 0xf8]) {
        let mut bytes = vec![0; 0xb0];
        for (at, n) in [
            (0, 0x50u64),
            (0x18, 0x50),
            (0x20, 24),
            (0x38, 0x68),
            (0x40, 36),
            (0x48, 0xa0),
        ] {
            bytes[at..at + 8].copy_from_slice(&n.to_le_bytes());
        }
        for at in [0x54, 0x58, 0x60, 0x6c, 0x80] {
            bytes[at..at + 4].copy_from_slice(&NONE.to_le_bytes());
        }
        bytes[0x78..0x80].copy_from_slice(&3u64.to_le_bytes());
        bytes[0x84..0x88].copy_from_slice(&1u32.to_le_bytes());
        bytes[0x88] = b'a';
        bytes[0xa0..0xa3].copy_from_slice(b"abc");
        let mut data = [0; 0xf8];
        data[..4].copy_from_slice(b"IVFC");
        data[12..16].copy_from_slice(&2u32.to_le_bytes());
        data[24..32].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        (bytes, data)
    }

    #[test]
    fn bktr_reads_cross_base_patch_boundaries_and_seeks() {
        let mut r = view(b"PATCH!".to_vec());
        r.base = Some(Cursor::new(b"base??".to_vec()));
        r.mappings = vec![
            Mapping {
                start: 0,
                physical: 0,
                patch: false,
            },
            Mapping {
                start: 4,
                physical: 1,
                patch: true,
            },
        ];
        r.len = 8;
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"baseATCH");
        r.seek(SeekFrom::End(-5)).unwrap();
        let mut out = [0; 3];
        r.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"eAT");
        assert!(r.seek(SeekFrom::End(1)).is_err());
        r.base = None;
        r.seek(SeekFrom::Start(0)).unwrap();
        assert!(r.read(&mut out).is_err());
    }

    #[test]
    fn bktr_relocation_tables_reject_hostile_counts_ranges_and_order() {
        let spec = native::synthetic::TitleSpec::patch(0x0100_0000_0001_0000, 1, 2);
        let (_, bytes) = native::synthetic::title_entries(&spec)
            .into_iter()
            .find(|(_, b)| b[0x205] == 0)
            .unwrap();
        let size = bytes.len() as u64;
        let mut input = Cursor::new(bytes.clone());
        let nca = native::nca::parse_nca(&mut Source::new(&mut input, size), 0, size)
            .unwrap_or_else(|_| panic!("synthetic header"));
        let slot = nca
            .summary
            .sections
            .iter()
            .position(|s| s.fs_type == 0)
            .unwrap();
        let root = (nca.summary.sections[slot].start + u64_at(&nca.patch_data[slot], 0)) as usize;
        let (mappings, _) =
            relocations(&mut Source::new(&mut input, size), &nca, slot, 1024 * 1024).unwrap();
        assert_eq!(mappings.len(), 3);
        assert!(!mappings[1].patch);
        for (at, replacement) in [
            (root + 4, u32::MAX),
            (root + 0x4000 + 4, u32::MAX),
            (root + 0x4000 + 16, 1),
            (root + 0x4000 + 32, 2),
        ] {
            let mut damaged = bytes.clone();
            damaged[at..at + 4].copy_from_slice(&replacement.to_le_bytes());
            let mut input = Cursor::new(damaged);
            assert!(
                relocations(&mut Source::new(&mut input, size), &nca, slot, 1024 * 1024).is_err()
            );
        }
    }

    #[test]
    fn romfs_walk_checks_cycles_paths_data_and_memory() {
        let (bytes, data) = image();
        let entries = romfs_entries(&mut view(bytes.clone()), &data, 1024 * 1024).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "a");
        assert_eq!(entries[0].range, Some((0xa0, 3)));
        assert!(romfs_entries(&mut view(bytes.clone()), &data, 64).is_err());
        let mut cycle = bytes.clone();
        cycle[0x6c..0x70].fill(0);
        assert!(romfs_entries(&mut view(cycle), &data, 1024 * 1024).is_err());
        let mut traversal = bytes.clone();
        traversal[0x88] = b'.';
        assert!(romfs_entries(&mut view(traversal), &data, 1024 * 1024).is_err());
        let mut bounds = bytes;
        bounds[0x78..0x80].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(romfs_entries(&mut view(bounds), &data, 1024 * 1024).is_err());
    }

    #[test]
    fn ivfc_verifies_padding_and_rejects_corruption_and_cancellation() {
        let bytes = b"payload".to_vec();
        let mut data = [0; 0xf8];
        data[..4].copy_from_slice(b"IVFC");
        data[8..12].copy_from_slice(&32u32.to_le_bytes());
        data[12..16].copy_from_slice(&2u32.to_le_bytes());
        data[24..32].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        data[32..36].copy_from_slice(&9u32.to_le_bytes());
        let mut padded = bytes.clone();
        padded.resize(512, 0);
        data[0xc0..0xe0].copy_from_slice(&sha256(&padded));
        let options = WriterOptions::default();
        verify_ivfc(&mut view(bytes.clone()), &data, &options).unwrap();
        let mut damaged = bytes.clone();
        damaged[0] ^= 1;
        assert!(verify_ivfc(&mut view(damaged), &data, &options).is_err());
        options.cancellation.cancel();
        assert!(verify_ivfc(&mut view(bytes), &data, &options).is_err());
    }
    #[test]
    fn native_base_bktr_versions_multi_program_dlc_and_raw_roundtrip() {
        use native::synthetic::{self, TitleSpec};
        const APP: u64 = 0x0100_0000_0001_0000;
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nx-extraction-{}-{id}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let mut base = TitleSpec::application(APP, 0, 1);
        base.program.as_mut().unwrap().romfs_bytes = 262144;
        base.program.as_mut().unwrap().npdm = Some(crate::import::synthetic::npdm(
            APP,
            crate::import::structures::NpdmProfile::MetaAcid2048V1,
        ));
        let mut secondary = base.program.clone().unwrap();
        secondary.program_id += 1;
        secondary.npdm = Some(crate::import::synthetic::npdm(
            APP + 1,
            crate::import::structures::NpdmProfile::MetaAcid2048V1,
        ));
        secondary.build_id = [4; 32];
        base.extra_programs.push(secondary.clone());
        let mut patch = TitleSpec::patch(APP, 17, 2);
        patch.program.as_mut().unwrap().romfs_bytes = 262144;
        patch.program.as_mut().unwrap().npdm = Some(crate::import::synthetic::npdm(
            APP,
            crate::import::structures::NpdmProfile::MetaAcid2048V1,
        ));
        secondary.bktr = true;
        secondary.build_id = [5; 32];
        patch.extra_programs.push(secondary);
        let dlc = TitleSpec::add_on(APP, APP + 0x1001, 3, 3);
        let mut args = vec!["pack".into()];
        for (i, spec) in [base, patch, dlc].iter().enumerate() {
            let path = dir.join(format!("{i}.dnsp"));
            std::fs::write(&path, synthetic::dnsp(&synthetic::title_entries(spec))).unwrap();
            args.push(path.to_string_lossy().into_owned());
        }
        let nx = dir.join("game.nx");
        args.extend([nx.to_string_lossy().into_owned(), "--no-compress".into()]);
        crate::cli::run(args, None, None).unwrap();
        let options = WriterOptions::default();
        let summary = inspect_archive(&nx, &options, IntegrityPolicy::Strict).unwrap();
        assert_eq!(summary.applications[0].snapshots.len(), 5);
        for (i, snapshot) in summary.applications[0].snapshots.iter().enumerate() {
            let output = dir.join(format!("dump-{i}"));
            extract_tree(&nx, snapshot.identity, &output, &options).unwrap();
            if snapshot.snapshot_type == SnapshotType::Dlc {
                assert!(output.join("romfs/dlc.bin").is_file());
                assert!(!output.join("exefs").exists());
            } else {
                assert!(std::fs::read(output.join("exefs/main"))
                    .unwrap()
                    .starts_with(b"NSO0"));
                let expected = if snapshot.snapshot_type == SnapshotType::Base {
                    1
                } else {
                    2
                };
                assert_eq!(
                    std::fs::read(output.join("romfs/data.bin")).unwrap(),
                    vec![expected; 262144]
                );
            }
            assert!(extract_tree(&nx, snapshot.identity, &output, &options).is_err());
            let raw = dir.join(format!("raw-{i}"));
            extract_raw_ncas(&nx, snapshot.identity, &raw, &options).unwrap();
            let files =
                inspect_snapshot_files(&nx, snapshot.identity, &options, IntegrityPolicy::Fast)
                    .unwrap();
            for file in files {
                let bytes = std::fs::read(crate::import::join(&raw, &file.path)).unwrap();
                assert_eq!(bytes.len() as u64, file.size);
                assert_eq!(sha256(&bytes), file.digest);
            }
        }
        let dump = dir.join("all");
        crate::cli::run(
            vec![
                "extract".into(),
                nx.to_string_lossy().into_owned(),
                dump.to_string_lossy().into_owned(),
            ],
            None,
            None,
        )
        .unwrap();
        assert!(dump
            .join(format!("base-{APP:016x}/romfs/data.bin"))
            .is_file());
        assert!(dump
            .join(format!("updates/17-{:016x}/romfs/data.bin", APP + 1))
            .is_file());
        assert!(dump
            .join(format!("dlc/{:016x}/3/romfs/dlc.bin", APP + 0x1001))
            .is_file());
        let restored = dir.join("restored.nx");
        let stats =
            super::super::restoration::restore_extraction(&dump, &restored, &options).unwrap();
        assert!(stats.unchanged);
        assert_eq!(
            std::fs::read(&nx).unwrap(),
            std::fs::read(&restored).unwrap()
        );
        assert_eq!(
            crate::recipe::embedded_recipes(&restored, &options)
                .unwrap()
                .len(),
            3
        );
        for recipe in crate::recipe::embedded_recipes(&restored, &options).unwrap() {
            crate::recipe::check_embedded(&restored, &recipe.bytes, &options).unwrap();
        }
        let changed = format!("base-{APP:016x}/romfs/data.bin");
        std::fs::write(
            crate::import::join(&dump, &changed),
            b"edited file with a different size",
        )
        .unwrap();
        let modified = dir.join("modified.nx");
        let stats =
            super::super::restoration::restore_extraction(&dump, &modified, &options).unwrap();
        assert!(!stats.unchanged);
        let mod_summary = inspect_archive(&modified, &options, IntegrityPolicy::Strict).unwrap();
        assert_eq!(mod_summary.applications[0].snapshots.len(), 5);
        for s in &mod_summary.applications[0].snapshots {
            let original = summary.applications[0]
                .snapshots
                .iter()
                .find(|old| {
                    old.program_id == s.program_id
                        && old.numeric_version == s.numeric_version
                        && old.snapshot_type == s.snapshot_type
                })
                .unwrap();
            assert_eq!(s.nacp_digest, original.nacp_digest);
            let old_display =
                read_snapshot_display(&nx, original.identity, &options, true).unwrap();
            let new_display = read_snapshot_display(&modified, s.identity, &options, true).unwrap();
            assert_eq!(new_display.nacp, old_display.nacp);
            assert_eq!(new_display.icon, old_display.icon);
            assert_eq!(new_display.title, old_display.title);
            assert_eq!(new_display.publisher, old_display.publisher);
            let output = dir.join(format!(
                "mod-{}-{}",
                s.program_id.unwrap_or(0),
                s.numeric_version
            ));
            extract_tree(&modified, s.identity, &output, &options).unwrap();
            if s.snapshot_type != SnapshotType::Dlc {
                let bytes = std::fs::read(output.join("romfs/data.bin")).unwrap();
                if s.snapshot_type == SnapshotType::Base && s.program_id == Some(APP) {
                    assert_eq!(bytes, b"edited file with a different size");
                } else {
                    assert_eq!(
                        bytes,
                        vec![
                            if s.snapshot_type == SnapshotType::Base {
                                1
                            } else {
                                2
                            };
                            262144
                        ]
                    );
                }
            }
        }
        let mut input = File::open(&nx).unwrap();
        let mut budget = MemoryBudget {
            current: 0,
            peak: 0,
            limit: options.max_memory_bytes,
        };
        let mut parsed = parse_archive(&mut input, 0, &options, &mut budget).unwrap();
        let index = parsed
            .snapshots
            .iter()
            .position(|s| s.snapshot.snapshot_type == SnapshotType::Update as u8)
            .unwrap();
        parsed.snapshots[index].snapshot.dependencies[0].target_snapshot_identity = [0xee; 32];
        let missing = dir.join("missing-base");
        assert!(extract_native(
            &nx,
            &parsed,
            &parsed.snapshots[index],
            &missing,
            &budget,
            &options
        )
        .is_err());
        assert!(!missing.exists());
        let constrained = WriterOptions {
            max_memory_bytes: 1024 * 1024,
            ..Default::default()
        };
        let failed = dir.join("low-memory");
        assert!(extract_tree(
            &nx,
            summary.applications[0].snapshots[0].identity,
            &failed,
            &constrained
        )
        .is_err());
        assert!(!failed.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn exefs_only_programs_and_data_only_updates_extract_and_restore() {
        use native::synthetic::{self, TitleSpec};
        const APP: u64 = 0x0100_0000_0001_0000;
        for exefs_only in [true, false] {
            let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("nx-exefs-only-{}-{id}", std::process::id()));
            std::fs::create_dir(&dir).unwrap();
            let mut base = TitleSpec::application(APP, 0, 1);
            base.program.as_mut().unwrap().romfs_bytes = if exefs_only { 0 } else { 4096 };
            let mut update = TitleSpec::patch(APP, 1, 2);
            update.program = None;
            update.control.as_mut().unwrap().title = "Data-only update".into();
            let mut args = vec!["pack".into()];
            for (i, spec) in [base, update].iter().enumerate() {
                let path = dir.join(format!("{i}.dnsp"));
                std::fs::write(&path, synthetic::dnsp(&synthetic::title_entries(spec))).unwrap();
                args.push(path.to_string_lossy().into_owned());
            }
            let nx = dir.join("game.nx");
            args.push(nx.to_string_lossy().into_owned());
            crate::cli::run(args, None, None).unwrap();
            let dump = dir.join("dump");
            let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured = messages.clone();
            crate::cli::run(
                vec![
                    "extract".into(),
                    nx.to_string_lossy().into_owned(),
                    dump.to_string_lossy().into_owned(),
                ],
                Some(Arc::new(move |_, line| {
                    captured.lock().unwrap().push(line.to_owned())
                })),
                None,
            )
            .unwrap();
            assert!(dump.join("base/exefs/main").is_file());
            assert_eq!(
                std::fs::read(dump.join("base/exefs/main")).unwrap(),
                std::fs::read(dump.join("updates/1/exefs/main")).unwrap()
            );
            assert_eq!(dump.join("updates/1/romfs").exists(), !exefs_only);
            assert_eq!(
                messages
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|m| m.contains("has no RomFS section")),
                exefs_only
            );
            let restored = dir.join("restored.nx");
            super::super::restoration::restore_extraction(
                &dump,
                &restored,
                &WriterOptions::default(),
            )
            .unwrap();
            assert_eq!(
                std::fs::read(&nx).unwrap(),
                std::fs::read(&restored).unwrap()
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
