//! Transactional editing of existing NX 0.2 archives: in-place append edits
//! (default for same-file edits) and full compacting rewrites.
//!
//! The parser deliberately keeps the wire representation separate from the
//! public logical-input writer. Retained block payloads are represented by a
//! source range and streamed unchanged into the replacement archive.

use super::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;

const NO_INDEX: u32 = u32::MAX;
const APP_SIZE: usize = 80;
const PROGRAM_SIZE: usize = 64;
const SNAPSHOT_SIZE: usize = 120;
const LOCAL_SIZE: usize = 24;
const DEPENDENCY_SIZE: usize = 56;
const EXTENSION_SIZE: usize = 32;
const MEDIA_RECORD_SIZE: usize = 64;
const MANIFEST_HEADER_SIZE: usize = 96;
const DIRECTORY_SIZE: usize = 16;
const ENTRY_SIZE: usize = 24;
const FILE_SIZE: usize = 64;
const BLOCK_SIZE: usize = 24;
const MAX_CATALOG: u64 = 16 * 1024 * 1024;
const MAX_MANIFEST: u64 = 4 * 1024 * 1024 * 1024;
const MAX_MEDIA_REGION: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_SIZE: u64 = i64::MAX as u64;
const HISTORY_EXTENSION: u32 = 0x0001_0004;
const NATIVE_EXTENSION: u32 = 0x0001_0005;
/// Optional, application-owned: recipes that rebuild the original packages.
const RECIPE_EXTENSION: u32 = 0x0001_0006;
/// All rebuild recipes of one archive together stay well inside the Catalog.
pub const MAX_EMBEDDED_RECIPE_BYTES: u64 = 8 * 1024 * 1024;
#[path = "native_add.rs"]
mod native_add;
#[path = "reclaim.rs"]
mod reclaim;
pub use native_add::{
    add_native_content, add_native_contents, read_snapshot_file_range, NativeAddRequest,
    NativeAddStats,
};
pub(crate) use native_add::{add_native_contents_with_recipes, verify_extracted_source_files};

type OptionalAppExtension = (u32, u32, u16, Vec<u8>);
type OptionalCatalogExtension = (u32, u32, u16, u32, Vec<u8>);
type ParsedManifest = (Vec<Vec<SourceEntry>>, Vec<FileBuild>, Vec<u32>);
type ParsedCatalog = (
    Vec<SourceApplication>,
    Vec<SourceProgram>,
    Vec<SourceSnapshot>,
    Vec<OptionalCatalogExtension>,
);
type SnapshotSortKey = (u32, u8, u32, [u8; 16], [u8; 32]);

/// FAST authenticates the header, Catalog, Manifest and block index, then
/// copies retained payloads without claiming they were reverified.
/// STRICT also decodes and verifies every source block and every source file
/// digest before publishing a replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrityPolicy {
    Fast,
    Strict,
}

/// Selects the exact snapshot whose native display metadata should be exposed
/// for an application after the edit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetadataSourceSelection {
    pub platform: Platform,
    pub application_id: u64,
    pub snapshot_identity: [u8; 32],
}

/// One atomic edit. Snapshot identities are exact 32-byte canonical IDs.
pub struct RepackPlan {
    pub remove_snapshots: Vec<[u8; 32]>,
    pub additions: ArchiveInput,
    pub metadata_sources: Vec<MetadataSourceSelection>,
}

impl RepackPlan {
    pub fn new() -> Self {
        Self {
            remove_snapshots: Vec::new(),
            additions: ArchiveInput::new(Vec::new()),
            metadata_sources: Vec::new(),
        }
    }
}

impl Default for RepackPlan {
    fn default() -> Self {
        Self::new()
    }
}

/// Full block verification is always performed; only duplicate file walks can reuse proof.
#[derive(Clone, Debug, Default)]
pub struct VerificationStats {
    pub stored_bytes_verified: u64,
    pub block_decodes: u64,
    pub decompressed_bytes: u64,
    pub files_verified: u64,
    pub files_reused: u64,
    pub logical_file_bytes_verified: u64,
    pub logical_file_bytes_reused: u64,
    pub cache_peak_entries: u16,
}

/// Actual archive and payload accounting for one successful repack.
#[derive(Clone, Debug)]
pub struct RepackStats {
    pub archive_bytes: u64,
    pub retained_snapshots: u32,
    pub removed_snapshots: u32,
    pub added_snapshots: u32,
    pub retained_stored_bytes_copied: u64,
    pub added_stored_bytes_copied: u64,
    pub added_blocks_reused: u64,
    /// Repeated logical chunks removed while staging the additions as RAW.
    pub added_input_deduplicated_blocks: u64,
    /// Logical bytes submitted to Zstandard after deduplication. This includes
    /// blocks that later fell back to RAW because the frame did not shrink.
    pub added_logical_bytes_considered_for_compression: u64,
    /// Logical bytes whose new Zstandard frame was retained in the output.
    pub added_logical_bytes_compressed: u64,
    pub compression_time: Duration,
    pub source_payload_bytes_verified: u64,
    pub source_verification: VerificationStats,
    pub output_verification: VerificationStats,
    pub output_bytes_written: u64,
    pub peak_temporary_disk_bytes: u64,
    pub no_op: bool,
    /// True when the edit appended to the source file (no base rewrite).
    pub in_place_append: bool,
    /// Archive bytes not used by the current generation (stale tails,
    /// removed blocks). A compaction reclaims them.
    pub free_bytes: u64,
    /// Bytes deallocated by the filesystem after an in-place removal.
    pub reclaimed_disk_bytes: u64,
    /// True when a same-file edit rewrote a compact archive because the
    /// append would have left `auto_compact_free_percent` or more unused.
    pub auto_compacted: bool,
    pub elapsed: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveSummary {
    pub applications: Vec<ApplicationSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationSummary {
    pub platform: Platform,
    pub application_id: u64,
    pub metadata_snapshot_identity: Option<[u8; 32]>,
    pub programs: Vec<ProgramSummary>,
    pub snapshots: Vec<SnapshotSummary>,
    /// Rebuild recipes kept inside the archive (see `recipe`).
    pub recipes: Vec<Vec<u8>>,
    /// Manifest metadata for files outside the executable snapshots.
    pub source_files: Vec<SourceFileSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFileSummary {
    pub kind: u32,
    pub path: String,
    pub size: u64,
    pub digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramSummary {
    pub program_id: u64,
    pub kind: ProgramKind,
    pub content_id: Option<[u8; 16]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotSummary {
    pub dependencies: Vec<DependencyInput>,
    pub identity: [u8; 32],
    pub program_id: Option<u64>,
    pub snapshot_type: SnapshotType,
    pub numeric_version: u32,
    pub content_id: Option<[u8; 16]>,
    /// Add-on title ID of a DLC stored without a native Content ID.
    pub addon_title_id: Option<u64>,
    pub build_id: Option<[u8; 32]>,
    pub has_display_metadata: bool,
    pub nacp_digest: Option<[u8; 32]>,
}

/// Immutable index evidence for the Increment 3 native overlay prototype.
/// Offsets refer to the inspected archive, never to an extracted file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotFileReference {
    pub path: String,
    pub size: u64,
    pub digest: [u8; 32],
    pub chunk_size_bytes: u32,
    pub blocks: Vec<SnapshotBlockReference>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotBlockReference {
    pub index: u32,
    pub offset: u64,
    pub stored_size: u32,
    pub logical_size: u32,
    pub codec: u8,
    /// CRC-32C of each 16 KiB slice of the logical block.
    pub checks: Vec<u32>,
}

/// Read a snapshot's authenticated file/block references without extraction or
/// decompression in Fast mode. This is an inspection prototype, not add-update.
pub fn inspect_snapshot_files(
    path: impl AsRef<Path>,
    identity: [u8; 32],
    options: &WriterOptions,
    policy: IntegrityPolicy,
) -> Result<Vec<SnapshotFileReference>, WriterError> {
    validate_options(options)?;
    let mut file = File::open(path)?;
    let mut budget = MemoryBudget {
        current: 0,
        peak: 0,
        limit: options.max_memory_bytes,
    };
    let archive = parse_archive(&mut file, 0, options, &mut budget)?;
    if policy == IntegrityPolicy::Strict {
        verify_source_payloads(&mut file, &archive, options)?;
    }
    let snapshot = archive
        .snapshots
        .iter()
        .find(|s| s.snapshot.identity == identity)
        .ok_or_else(|| invalid("snapshot identity not present"))?;
    reference_files(&archive, snapshot.snapshot.root, options, &mut budget)
}

fn reference_files(
    archive: &SourceArchive,
    root: u32,
    _options: &WriterOptions,
    budget: &mut MemoryBudget,
) -> Result<Vec<SnapshotFileReference>, WriterError> {
    let mut nodes = Vec::new();
    visit_identity_tree(
        root,
        &[],
        0,
        archive,
        &mut HashSet::new(),
        &mut nodes,
        &mut 0,
    )?;
    let mut result = Vec::new();
    for node in nodes {
        if let Some(index) = node.source_file {
            let f = &archive.files[index as usize];
            budget.charge(
                f.block_count as u64 * 80 + node.path.len() as u64 + 128,
                "snapshot reference prototype",
            )?;
            let blocks = (f.first_ref..f.first_ref + f.block_count)
                .map(|r| {
                    let index = archive.block_refs[r as usize];
                    let b = &archive.blocks[index as usize];
                    SnapshotBlockReference {
                        index,
                        offset: b.range.offset,
                        stored_size: b.stored_size,
                        logical_size: b.logical_size,
                        codec: b.codec,
                        checks: b.checks.clone(),
                    }
                })
                .collect();
            result.push(SnapshotFileReference {
                path: String::from_utf8(node.path).map_err(|_| bad("non-UTF8 path"))?,
                size: f.size,
                digest: f.digest,
                chunk_size_bytes: f.chunk_size_bytes,
                blocks,
            });
        }
    }
    Ok(result)
}

/// Independent source files, owned by an application rather than a game snapshot.
pub fn inspect_source_resources(
    path: impl AsRef<Path>,
    platform: Platform,
    application_id: u64,
    options: &WriterOptions,
) -> Result<Vec<(u32, Vec<SnapshotFileReference>)>, WriterError> {
    validate_options(options)?;
    let mut file = File::open(path)?;
    let mut budget = MemoryBudget {
        current: 0,
        peak: 0,
        limit: options.max_memory_bytes,
    };
    let archive = parse_archive(&mut file, 0, options, &mut budget)?;
    let mut result = Vec::new();
    for app in archive
        .applications
        .iter()
        .filter(|a| a.key == (platform as u8, application_id))
    {
        for &(kind, root) in &app.source_resources {
            result.push((kind, reference_files(&archive, root, options, &mut budget)?));
        }
    }
    Ok(result)
}

impl Default for RepackStats {
    fn default() -> Self {
        Self {
            archive_bytes: 0,
            retained_snapshots: 0,
            removed_snapshots: 0,
            added_snapshots: 0,
            retained_stored_bytes_copied: 0,
            added_stored_bytes_copied: 0,
            added_blocks_reused: 0,
            added_input_deduplicated_blocks: 0,
            added_logical_bytes_considered_for_compression: 0,
            added_logical_bytes_compressed: 0,
            compression_time: Duration::ZERO,
            source_payload_bytes_verified: 0,
            source_verification: VerificationStats::default(),
            output_verification: VerificationStats::default(),
            output_bytes_written: 0,
            peak_temporary_disk_bytes: 0,
            no_op: false,
            in_place_append: false,
            free_bytes: 0,
            reclaimed_disk_bytes: 0,
            auto_compacted: false,
            elapsed: Duration::ZERO,
        }
    }
}

#[derive(Clone, Copy)]
struct SourceRange {
    archive: u8,
    offset: u64,
    size: u64,
}

#[derive(Clone)]
struct SourceBlock {
    logical_size: u32,
    stored_size: u32,
    codec: u8,
    /// Slice CRC-32C integrity entries of the logical bytes.
    checks: Vec<u32>,
    range: SourceRange,
}

fn checks_match(block: &SourceBlock, logical: &[u8]) -> bool {
    let mut actual = Vec::with_capacity(block.checks.len());
    crate::hash::slice_checks(logical, &mut actual);
    actual == block.checks
}

#[derive(Clone)]
struct SourceMedia {
    media_type: u16,
    size: u64,
    digest: [u8; 32],
    range: SourceRange,
}

#[derive(Clone)]
struct SourceEntry {
    name: String,
    kind: u8,
    target: u32,
}

struct SourceSnapshot {
    archive: u8,
    key: (u8, u64),
    program_id: Option<u64>,
    snapshot: FinalSnapshot,
}

struct SourceProgram {
    key: (u8, u64),
    program_id: u64,
    kind: u8,
    content_id: [u8; 16],
    has_content_id: bool,
    first_snapshot: u32,
    snapshot_count: u32,
    npdm: u32,
}

struct SourceApplication {
    key: (u8, u64),
    selected_identity: Option<[u8; 32]>,
    optional_extensions: Vec<OptionalAppExtension>,
    source_resources: Vec<(u32, u32)>,
    recipes: Vec<Vec<u8>>,
}

struct SourceArchive {
    archive: u8,
    file_size: u64,
    generation: u64,
    /// Bytes to preserve when appending, including residue if a page is lost.
    slots_end: u64,
    active_slot: usize,
    tail_start: u64,
    applications: Vec<SourceApplication>,
    programs: Vec<SourceProgram>,
    snapshots: Vec<SourceSnapshot>,
    directories: Vec<Vec<SourceEntry>>,
    files: Vec<FileBuild>,
    block_refs: Vec<u32>,
    blocks: Vec<SourceBlock>,
    media: Vec<SourceMedia>,
    optional_global_extensions: Vec<OptionalCatalogExtension>,
    // Post-parse native additions may borrow flat files from source archive 0.
    // These are capabilities held by this transaction, never serialized indexes.
    inherited_files: HashMap<u32, u32>,
}

fn bad(message: impl Into<String>) -> WriterError {
    WriterError::new(WriterErrorKind::Integrity, message)
}

fn read_u16(b: &[u8], at: usize) -> Result<u16, WriterError> {
    let s = b
        .get(at..at + 2)
        .ok_or_else(|| bad("truncated u16 field"))?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}
fn read_u32(b: &[u8], at: usize) -> Result<u32, WriterError> {
    let s = b
        .get(at..at + 4)
        .ok_or_else(|| bad("truncated u32 field"))?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn read_u64(b: &[u8], at: usize) -> Result<u64, WriterError> {
    let s = b
        .get(at..at + 8)
        .ok_or_else(|| bad("truncated u64 field"))?;
    Ok(u64::from_le_bytes(s.try_into().unwrap()))
}
fn checked_range(
    bytes: &[u8],
    offset: u64,
    size: u64,
) -> Result<std::ops::Range<usize>, WriterError> {
    let start = usize::try_from(offset).map_err(|_| bad("section offset exceeds address space"))?;
    let len = usize::try_from(size).map_err(|_| bad("section size exceeds address space"))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| bad("section range overflow"))?;
    if end > bytes.len() {
        return Err(bad("section range is outside its container"));
    }
    Ok(start..end)
}
fn read_exact_at(file: &mut File, offset: u64, bytes: &mut [u8]) -> Result<(), WriterError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(bytes, offset)?;
    }
    #[cfg(not(unix))]
    {
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(bytes)?;
    }
    Ok(())
}
fn read_vec_at(
    file: &mut File,
    offset: u64,
    size: u64,
    max: u64,
    label: &str,
) -> Result<Vec<u8>, WriterError> {
    if size > max {
        return Err(limit_error(format!("{label} exceeds repack memory limit")));
    }
    let len =
        usize::try_from(size).map_err(|_| limit_error(format!("{label} exceeds address space")))?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(len)
        .map_err(|_| limit_error(format!("unable to reserve {label}")))?;
    result.resize(len, 0);
    read_exact_at(file, offset, &mut result)?;
    Ok(result)
}
fn hash_range(
    file: &mut File,
    offset: u64,
    size: u64,
    options: &WriterOptions,
) -> Result<[u8; 32], WriterError> {
    let mut hash = Sha256::new();
    let mut at = offset;
    let mut remaining = size;
    let mut buffer = [0u8; 64 * 1024];
    while remaining != 0 {
        cancelled(options)?;
        let amount = remaining.min(buffer.len() as u64) as usize;
        read_exact_at(file, at, &mut buffer[..amount])?;
        hash.update(&buffer[..amount]);
        at = at
            .checked_add(amount as u64)
            .ok_or_else(|| bad("hash range overflow"))?;
        remaining -= amount as u64;
    }
    Ok(hash.finish())
}

#[derive(Clone, Copy)]
struct SourceHeader {
    minor: u16,
    archive_size: u64,
    catalog_offset: u64,
    catalog_size: u64,
    media_offset: u64,
    media_size: u64,
    data_offset: u64,
    data_size: u64,
    manifest_offset: u64,
    manifest_size: u64,
    block_index_offset: u64,
    block_count: u64,
    integrity_offset: u64,
    integrity_size: u64,
    generation: u64,
    slots_end: u64,
    active_slot: usize,
}

fn zero_range(file: &mut File, offset: u64, size: u64) -> Result<(), WriterError> {
    let mut remaining = size;
    let mut at = offset;
    let mut buffer = [0u8; 4096];
    while remaining != 0 {
        let amount = remaining.min(buffer.len() as u64) as usize;
        read_exact_at(file, at, &mut buffer[..amount])?;
        if buffer[..amount].iter().any(|&byte| byte != 0) {
            return Err(bad("nonzero bytes in section padding"));
        }
        at += amount as u64;
        remaining -= amount as u64;
    }
    Ok(())
}

fn decode_slot(bytes: &[u8]) -> Result<Option<SourceHeader>, WriterError> {
    if &bytes[..4] != b"NXAR"
        || read_u16(bytes, 4)? != 0
        || !matches!(read_u16(bytes, 6)?, 2 | 3)
        || read_u32(bytes, 8)? != 256
        || read_u16(bytes, 12)? != PAGED_HEADERS
        || read_u16(bytes, 14)? != 0
        || read_u32(bytes, 224)? != crate::hash::CHECK_SIZE as u32
        || bytes[228..252].iter().any(|&byte| byte != 0)
        || read_u32(bytes, 252)? != crc32c(&bytes[..252])
    {
        return Ok(None);
    }
    Ok(Some(SourceHeader {
        minor: read_u16(bytes, 6)?,
        archive_size: read_u64(bytes, 16)?,
        catalog_offset: read_u64(bytes, 24)?,
        catalog_size: read_u64(bytes, 32)?,
        media_offset: read_u64(bytes, 40)?,
        media_size: read_u64(bytes, 48)?,
        data_offset: read_u64(bytes, 56)?,
        data_size: read_u64(bytes, 64)?,
        manifest_offset: read_u64(bytes, 72)?,
        manifest_size: read_u64(bytes, 80)?,
        block_index_offset: read_u64(bytes, 88)?,
        block_count: read_u64(bytes, 96)?,
        integrity_offset: read_u64(bytes, 104)?,
        integrity_size: read_u64(bytes, 112)?,
        generation: read_u64(bytes, 216)?,
        slots_end: 0,
        active_slot: 0,
    }))
}

fn parse_header(file: &mut File, options: &WriterOptions) -> Result<SourceHeader, WriterError> {
    let physical_size = file.metadata()?.len();
    if !(DATA_START..=MAX_ARCHIVE_SIZE).contains(&physical_size) {
        return Err(bad("source archive size is outside supported bounds"));
    }
    if physical_size > options.max_archive_size {
        return Err(limit_error("source archive exceeds configured size limit"));
    }
    let mut area = [0u8; DATA_START as usize];
    let mut readable = [true; 2];
    if let Err(error) = read_exact_at(file, 0, &mut area) {
        for (slot, page) in area.chunks_exact_mut(HEADER_PAGE_SIZE).enumerate() {
            readable[slot] = read_exact_at(file, HEADER_SLOTS[slot], page).is_ok();
        }
        if !readable[0] && !readable[1] {
            return Err(error);
        }
    }
    let read_slot = |slot: usize| -> Result<Option<SourceHeader>, WriterError> {
        let start = slot * HEADER_PAGE_SIZE;
        let page = &area[start..start + HEADER_PAGE_SIZE];
        if !readable[slot] || page[256..].iter().any(|&byte| byte != 0) {
            return Ok(None);
        }
        decode_slot(&page[..256])
    };
    let a = read_slot(0)?;
    let b = read_slot(1)?;
    let (mut header, active_slot) = match (a, b) {
        (None, None) => return Err(bad("no valid paged NX header")),
        (Some(a), None) => (a, 0),
        (None, Some(b)) => (b, 1),
        (Some(a), Some(b)) => {
            if a.generation == b.generation
                && area[..256] != area[HEADER_PAGE_SIZE..HEADER_PAGE_SIZE + 256]
            {
                return Err(bad(
                    "both header slots claim one generation with different bytes",
                ));
            }
            if b.generation > a.generation {
                (b, 1)
            } else {
                (a, 0)
            }
        }
    };
    let start = active_slot * HEADER_PAGE_SIZE;
    let bytes: [u8; 256] = area[start..start + 256].try_into().unwrap();
    header.active_slot = active_slot;
    header.slots_end = [a, b]
        .iter()
        .flatten()
        .map(|h| h.archive_size)
        .max()
        .unwrap_or(0);
    if header.archive_size > physical_size || header.slots_end > physical_size {
        return Err(bad(format!(
            "Header archive_size {} exceeds physical file length {}",
            header.archive_size, physical_size
        )));
    }
    if a.is_none() || b.is_none() {
        // The lost header may have described a tail beyond the survivor.
        // Keep all existing bytes when repairing that page.
        header.slots_end = physical_size;
    }
    if !(128..=MAX_CATALOG).contains(&header.catalog_size) {
        return Err(bad("Header Catalog size is outside supported bounds"));
    }
    if !(MANIFEST_HEADER_SIZE as u64..=MAX_MANIFEST).contains(&header.manifest_size) {
        return Err(bad("Header Manifest size is outside supported bounds"));
    }
    if header.block_count > u32::MAX as u64 {
        return Err(bad("Header block_count exceeds u32"));
    }
    if header.integrity_size % 4 != 0
        || header.integrity_size / 4 > header.block_count.saturating_mul(64)
        || (header.block_count != 0) != (header.integrity_size != 0)
    {
        return Err(bad("Header integrity_size does not fit the block count"));
    }
    let mut cursor = DATA_START;
    if header.block_count == 0 {
        if header.data_offset != 0
            || header.data_size != 0
            || header.block_index_offset != 0
            || header.integrity_offset != 0
        {
            return Err(bad("empty block tables must have zero ranges"));
        }
    } else {
        if header.data_offset != DATA_START || header.data_size == 0 {
            return Err(bad("Data region placement or size is invalid"));
        }
        cursor = DATA_START
            .checked_add(header.data_size)
            .ok_or_else(|| bad("Data range overflow"))?;
    }
    // The first tail region may follow free space; the rest is packed.
    let first_tail = if header.media_size != 0 {
        header.media_offset
    } else {
        header.catalog_offset
    };
    if first_tail < align_up(cursor, 16)? || first_tail % 16 != 0 {
        return Err(bad(
            "metadata tail starts inside the Data region or unaligned",
        ));
    }
    cursor = first_tail;
    if header.media_size == 0 {
        if header.media_offset != 0 {
            return Err(bad("absent Media region has a nonzero offset"));
        }
    } else {
        if header.media_size < 32 || header.media_size > MAX_MEDIA_REGION {
            return Err(bad("Media region placement or size is invalid"));
        }
        cursor = header
            .media_offset
            .checked_add(header.media_size)
            .ok_or_else(|| bad("Media range overflow"))?;
    }
    fn next(
        cursor: &mut u64,
        offset: u64,
        size: u64,
        what: &str,
        file: &mut File,
    ) -> Result<(), WriterError> {
        let expected = align_up(*cursor, 16)?;
        if offset != expected {
            return Err(bad(format!("{what} is not at its minimum aligned offset")));
        }
        zero_range(file, *cursor, expected - *cursor)?;
        *cursor = offset
            .checked_add(size)
            .ok_or_else(|| bad(format!("{what} range overflow")))?;
        Ok(())
    }
    if header.media_size != 0 {
        next(
            &mut cursor,
            header.catalog_offset,
            header.catalog_size,
            "Catalog",
            file,
        )?;
    } else {
        cursor = header
            .catalog_offset
            .checked_add(header.catalog_size)
            .ok_or_else(|| bad("Catalog range overflow"))?;
    }
    next(
        &mut cursor,
        header.manifest_offset,
        header.manifest_size,
        "Manifest",
        file,
    )?;
    if header.block_count != 0 {
        next(
            &mut cursor,
            header.block_index_offset,
            header.block_count * BLOCK_SIZE as u64,
            "block index",
            file,
        )?;
        next(
            &mut cursor,
            header.integrity_offset,
            header.integrity_size,
            "integrity table",
            file,
        )?;
    }
    if header.archive_size != cursor {
        return Err(bad("archive_size is not the end of the metadata tail"));
    }
    if hash_range(file, header.catalog_offset, header.catalog_size, options)? != bytes[120..152] {
        return Err(bad("Catalog SHA-256 mismatch"));
    }
    if hash_range(file, header.manifest_offset, header.manifest_size, options)? != bytes[152..184] {
        return Err(bad("Manifest SHA-256 mismatch"));
    }
    let mut index_hash = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    for (mut at, mut remaining) in [
        (
            header.block_index_offset,
            header.block_count * BLOCK_SIZE as u64,
        ),
        (header.integrity_offset, header.integrity_size),
    ] {
        while remaining != 0 {
            cancelled(options)?;
            let amount = remaining.min(buffer.len() as u64) as usize;
            read_exact_at(file, at, &mut buffer[..amount])?;
            index_hash.update(&buffer[..amount]);
            at += amount as u64;
            remaining -= amount as u64;
        }
    }
    if index_hash.finish() != bytes[184..216] {
        return Err(bad("block index and integrity table SHA-256 mismatch"));
    }
    Ok(header)
}

fn parse_media(
    file: &mut File,
    h: SourceHeader,
    archive: u8,
) -> Result<Vec<SourceMedia>, WriterError> {
    if h.media_size == 0 {
        return Ok(Vec::new());
    }
    let mut header = [0u8; 32];
    read_exact_at(file, h.media_offset, &mut header)?;
    if &header[..4] != b"NXMD"
        || read_u16(&header, 4)? != 1
        || read_u16(&header, 6)? != 32
        || read_u64(&header, 8)? != h.media_size
        || read_u16(&header, 20)? != MEDIA_RECORD_SIZE as u16
        || read_u32(&header, 24)? != 32
        || header[22..24].iter().any(|&v| v != 0)
    {
        return Err(bad("invalid Media region header"));
    }
    let count = read_u32(&header, 16)? as u64;
    let table_end = 32u64
        .checked_add(
            count
                .checked_mul(MEDIA_RECORD_SIZE as u64)
                .ok_or_else(|| bad("Media table size overflow"))?,
        )
        .ok_or_else(|| bad("Media table size overflow"))?;
    let payload_start = align_up(table_end, 16)?;
    if count == 0
        || count > 65535
        || read_u32(&header, 28)? as u64 != payload_start
        || payload_start > h.media_size
    {
        return Err(bad("Media record count or payload placement is invalid"));
    }
    zero_range(file, h.media_offset + table_end, payload_start - table_end)?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(count as usize)
        .map_err(|_| limit_error("unable to reserve Media descriptors"))?;
    let mut expected = payload_start;
    for i in 0..count {
        let at = h.media_offset + 32 + i * MEDIA_RECORD_SIZE as u64;
        let mut record = [0u8; MEDIA_RECORD_SIZE];
        read_exact_at(file, at, &mut record)?;
        let media_type = read_u16(&record, 0)?;
        let logical = read_u64(&record, 4)?;
        let stored = read_u64(&record, 12)?;
        let relative = read_u64(&record, 20)?;
        if !matches!(media_type, 1..=5)
            || record[2..4].iter().any(|&v| v != 0)
            || record[60..64].iter().any(|&v| v != 0)
            || logical == 0
            || logical != stored
            || logical > 64 * 1024 * 1024
            || relative != align_up(expected, 16)?
            || relative
                .checked_add(stored)
                .ok_or_else(|| bad("Media payload end overflow"))?
                > h.media_size
        {
            return Err(bad("invalid Media descriptor or payload range"));
        }
        zero_range(file, h.media_offset + expected, relative - expected)?;
        result.push(SourceMedia {
            media_type,
            size: logical,
            digest: record[28..60].try_into().unwrap(),
            range: SourceRange {
                archive,
                offset: h.media_offset + relative,
                size: stored,
            },
        });
        expected = relative + stored;
    }
    if expected != h.media_size {
        return Err(bad("Media region has trailing or missing payload bytes"));
    }
    Ok(result)
}

fn parse_blocks(
    file: &mut File,
    h: SourceHeader,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
    archive: u8,
) -> Result<Vec<SourceBlock>, WriterError> {
    let index_bytes = h
        .block_count
        .checked_mul(BLOCK_SIZE as u64)
        .ok_or_else(|| limit_error("block metadata size overflow"))?;
    let bytes_needed = h
        .block_count
        .checked_mul((BLOCK_SIZE + 48) as u64)
        .and_then(|v| v.checked_add(index_bytes))
        .and_then(|v| v.checked_add(h.integrity_size.saturating_mul(2)))
        .ok_or_else(|| limit_error("block metadata size overflow"))?;
    budget.charge(bytes_needed, "source block index and integrity metadata")?;
    let count = usize::try_from(h.block_count)
        .map_err(|_| limit_error("block count exceeds address space"))?;
    let index = read_vec_at(
        file,
        h.block_index_offset,
        index_bytes,
        index_bytes,
        "block index",
    )?;
    let integrity = read_vec_at(
        file,
        h.integrity_offset,
        h.integrity_size,
        h.integrity_size,
        "integrity table",
    )?;
    let mut blocks = Vec::new();
    blocks
        .try_reserve_exact(count)
        .map_err(|_| limit_error("unable to reserve source block index"))?;
    let data_end = h.data_offset + h.data_size;
    let mut minimum = h.data_offset;
    let mut next_check = 0u64;
    let mut last_end = h.data_offset;
    for i in 0..count {
        if i % 4096 == 0 {
            cancelled(options)?;
        }
        let record = &index[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE];
        let data_offset = read_u64(record, 0)?;
        let stored = read_u32(record, 8)?;
        let logical = read_u32(record, 12)?;
        let codec = record[16];
        let first_check = read_u32(record, 20)? as u64;
        if record[17..20].iter().any(|&v| v != 0)
            || !(1..=1024 * 1024).contains(&logical)
            || !(codec == 0 || codec == 1)
            || stored == 0
            || (codec == 0 && stored != logical)
            || (codec == 1 && stored as u64 > logical as u64 + 64 * 1024)
            || data_offset < minimum
            || data_offset % 16 != 0
            || first_check != next_check
            || data_offset
                .checked_add(stored as u64)
                .ok_or_else(|| bad("block payload end overflow"))?
                > data_end
        {
            return Err(bad("invalid block descriptor or Data payload range"));
        }
        let n = crate::hash::check_count(logical as usize) as u64;
        let end_check = next_check + n;
        if end_check * 4 > h.integrity_size {
            return Err(bad("block integrity entries exceed the integrity table"));
        }
        let checks = integrity[(next_check * 4) as usize..(end_check * 4) as usize]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        next_check = end_check;
        blocks.push(SourceBlock {
            logical_size: logical,
            stored_size: stored,
            codec,
            checks,
            range: SourceRange {
                archive,
                offset: data_offset,
                size: stored as u64,
            },
        });
        last_end = data_offset + stored as u64;
        minimum = align_up(last_end, 16)?;
    }
    if count != 0 && last_end != data_end {
        return Err(bad("Data region contains trailing or missing block bytes"));
    }
    if next_check * 4 != h.integrity_size {
        return Err(bad("integrity table has trailing or missing entries"));
    }
    Ok(blocks)
}

fn catalog_string(
    cat: &[u8],
    pool_off: u64,
    pool_size: u64,
    offset: u32,
    length: u32,
    optional: bool,
    what: &str,
) -> Result<Option<String>, WriterError> {
    if offset == NO_INDEX {
        if length != 0 {
            return Err(bad(format!("absent {what} has nonzero length")));
        }
        return Ok(None);
    }
    if length == 0 {
        return Err(bad(format!("present {what} is empty")));
    }
    let begin = offset as u64;
    let end = begin
        .checked_add(length as u64)
        .ok_or_else(|| bad(format!("{what} range overflow")))?;
    if end > pool_size {
        return Err(bad(format!("{what} is outside the Catalog string pool")));
    }
    let range = checked_range(cat, pool_off + begin, length as u64)?;
    let bytes = &cat[range];
    if bytes.contains(&0) {
        return Err(bad(format!("{what} contains NUL")));
    }
    let value = std::str::from_utf8(bytes).map_err(|_| bad(format!("{what} is not UTF-8")))?;
    if optional && value.is_empty() {
        return Err(bad(format!("{what} is empty")));
    }
    Ok(Some(value.to_owned()))
}

fn parse_manifest(
    file: &mut File,
    h: SourceHeader,
    blocks: &[SourceBlock],
    options: &WriterOptions,
    budget: &mut MemoryBudget,
) -> Result<ParsedManifest, WriterError> {
    let section_limit = options.max_memory_bytes.min(MAX_MANIFEST);
    budget.charge(h.manifest_size, "source Manifest section")?;
    let manifest = read_vec_at(
        file,
        h.manifest_offset,
        h.manifest_size,
        section_limit,
        "Manifest",
    )?;
    if manifest.len() < MANIFEST_HEADER_SIZE
        || &manifest[..4] != b"NXMF"
        || read_u16(&manifest, 4)? != 1
        || read_u16(&manifest, 6)? != MANIFEST_HEADER_SIZE as u16
        || read_u64(&manifest, 8)? != h.manifest_size
        || manifest[80..96].iter().any(|&v| v != 0)
    {
        return Err(bad("invalid Manifest header"));
    }
    let dir_count = read_u32(&manifest, 16)? as u64;
    let file_count = read_u32(&manifest, 20)? as u64;
    let entry_count = read_u32(&manifest, 24)? as u64;
    let ref_count = read_u32(&manifest, 28)? as u64;
    if dir_count == 0
        || dir_count > 16_777_216
        || file_count > 33_554_432
        || entry_count > 50_331_648
        || ref_count > u32::MAX as u64
    {
        return Err(bad("Manifest count exceeds NX 0.1 limits"));
    }
    if dir_count > options.max_directories as u64
        || file_count > options.max_files as u64
        || entry_count > options.max_entries as u64
    {
        return Err(limit_error(
            "source Manifest exceeds configured record limits",
        ));
    }
    let dir_off = read_u64(&manifest, 32)?;
    let file_off = read_u64(&manifest, 40)?;
    let entry_off = read_u64(&manifest, 48)?;
    let ref_off = read_u64(&manifest, 56)?;
    let path_off = read_u64(&manifest, 64)?;
    let path_size = read_u64(&manifest, 72)?;
    let mut cursor = MANIFEST_HEADER_SIZE as u64;
    let expected_dirs = align_up(cursor, 8)?;
    if dir_off != expected_dirs {
        return Err(bad("Manifest directory table offset is not canonical"));
    }
    cursor = dir_off
        .checked_add(dir_count * DIRECTORY_SIZE as u64)
        .ok_or_else(|| bad("directory table range overflow"))?;
    if file_count == 0 {
        if file_off != 0 {
            return Err(bad("empty file table has an offset"));
        }
    } else {
        let expected = align_up(cursor, 8)?;
        if file_off != expected {
            return Err(bad("Manifest file table offset is not canonical"));
        }
        cursor = file_off
            .checked_add(file_count * FILE_SIZE as u64)
            .ok_or_else(|| bad("file table range overflow"))?;
    }
    if entry_count == 0 {
        if entry_off != 0 {
            return Err(bad("empty Entry table has an offset"));
        }
    } else {
        let expected = align_up(cursor, 8)?;
        if entry_off != expected {
            return Err(bad("Manifest Entry table offset is not canonical"));
        }
        cursor = entry_off
            .checked_add(entry_count * ENTRY_SIZE as u64)
            .ok_or_else(|| bad("Entry table range overflow"))?;
    }
    if ref_count == 0 {
        if ref_off != 0 {
            return Err(bad("empty block-reference table has an offset"));
        }
    } else {
        let expected = align_up(cursor, 4)?;
        if ref_off != expected {
            return Err(bad("Manifest block-reference offset is not canonical"));
        }
        cursor = ref_off
            .checked_add(ref_count * 4)
            .ok_or_else(|| bad("block-reference range overflow"))?;
    }
    let expected_path = align_up(cursor, 8)?;
    if path_off != expected_path || path_off.checked_add(path_size) != Some(h.manifest_size) {
        return Err(bad("Manifest path pool placement or size is invalid"));
    }
    let dirs_range = checked_range(&manifest, dir_off, dir_count * DIRECTORY_SIZE as u64)?;
    let files_range = if file_count == 0 {
        0..0
    } else {
        checked_range(&manifest, file_off, file_count * FILE_SIZE as u64)?
    };
    let entries_range = if entry_count == 0 {
        0..0
    } else {
        checked_range(&manifest, entry_off, entry_count * ENTRY_SIZE as u64)?
    };
    let refs_range = if ref_count == 0 {
        0..0
    } else {
        checked_range(&manifest, ref_off, ref_count * 4)?
    };
    let _ = (dirs_range, files_range, entries_range, refs_range);
    // Decoded tables live alongside the raw section until parsing ends. Entry
    // names are charged as they are copied and then moved, never cloned.
    let decoded = file_count * std::mem::size_of::<FileBuild>() as u64
        + ref_count * 4
        + entry_count * std::mem::size_of::<(String, u8, u32)>() as u64
        + dir_count * std::mem::size_of::<Vec<SourceEntry>>() as u64
        + entry_count * std::mem::size_of::<SourceEntry>() as u64;
    budget.charge(decoded, "decoded source Manifest tables")?;
    let mut files = Vec::new();
    files
        .try_reserve_exact(file_count as usize)
        .map_err(|_| limit_error("unable to reserve Manifest file records"))?;
    for i in 0..file_count as usize {
        let at = file_off as usize + i * FILE_SIZE;
        let size = read_u64(&manifest, at)?;
        if size > options.max_file_size {
            return Err(limit_error("source file exceeds configured size limit"));
        }
        let first_ref = read_u32(&manifest, at + 8)?;
        let block_count = read_u32(&manifest, at + 12)?;
        let log2 = manifest[at + 16];
        if !matches!(log2, 16..=20)
            || manifest[at + 17..at + 20].iter().any(|&v| v != 0)
            || manifest[at + 52..at + 64].iter().any(|&v| v != 0)
        {
            return Err(bad("invalid Manifest File record"));
        }
        let chunk_size = 1u32 << log2;
        let expected_count = if size == 0 {
            0
        } else {
            (size - 1) / chunk_size as u64 + 1
        };
        if expected_count != block_count as u64
            || first_ref
                .checked_add(block_count)
                .filter(|&end| end as u64 <= ref_count)
                .is_none()
        {
            return Err(bad("File block-reference range does not match its size"));
        }
        files.push(FileBuild {
            size,
            digest: manifest[at + 20..at + 52].try_into().unwrap(),
            chunk_size_bytes: chunk_size,
            first_ref,
            block_count,
        });
    }
    let mut refs = Vec::new();
    refs.try_reserve_exact(ref_count as usize)
        .map_err(|_| limit_error("unable to reserve block-reference array"))?;
    for i in 0..ref_count as usize {
        let id = read_u32(&manifest, ref_off as usize + i * 4)?;
        if id as usize >= blocks.len() {
            return Err(bad("Manifest references a missing block"));
        }
        refs.push(id);
    }
    for f in &files {
        let mut remaining = f.size;
        for i in 0..f.block_count {
            let id = refs[(f.first_ref + i) as usize] as usize;
            let expected = remaining.min(f.chunk_size_bytes as u64) as u32;
            if blocks[id].logical_size != expected {
                return Err(bad(
                    "file block logical size disagrees with its chunk position",
                ));
            }
            remaining -= expected as u64;
        }
        if remaining != 0 {
            return Err(bad("file block sizes do not cover the file"));
        }
    }
    let mut raw_entries = Vec::<(String, u8, u32)>::new();
    raw_entries
        .try_reserve_exact(entry_count as usize)
        .map_err(|_| limit_error("unable to reserve Manifest entries"))?;
    for i in 0..entry_count as usize {
        let at = entry_off as usize + i * ENTRY_SIZE;
        let name_abs = read_u64(&manifest, at)?;
        let name_len = read_u32(&manifest, at + 8)?;
        if name_len > options.max_path_component_bytes {
            return Err(limit_error(
                "source path component exceeds configured byte limit",
            ));
        }
        let target = read_u32(&manifest, at + 12)?;
        let kind = manifest[at + 16];
        if manifest[at + 17..at + 24].iter().any(|&v| v != 0) || !matches!(kind, 1 | 2) {
            return Err(bad("invalid Manifest Entry flags or kind"));
        }
        let name_start = name_abs
            .checked_sub(path_off)
            .ok_or_else(|| bad("entry name precedes path pool"))?;
        let name_end = name_start
            .checked_add(name_len as u64)
            .ok_or_else(|| bad("entry name range overflow"))?;
        if name_len == 0 || name_end > path_size {
            return Err(bad("entry name is outside the path pool"));
        }
        let range = checked_range(&manifest, name_abs, name_len as u64)?;
        let bytes = &manifest[range];
        if bytes.contains(&0)
            || bytes == b"."
            || bytes == b".."
            || bytes.contains(&b'/')
            || bytes.contains(&b'\\')
        {
            return Err(bad("unsafe Manifest path component"));
        }
        budget.charge(name_len as u64, "source Manifest entry names")?;
        let name = std::str::from_utf8(bytes)
            .map_err(|_| bad("Manifest path is not UTF-8"))?
            .to_owned();
        if (kind == 1 && target as u64 >= dir_count) || (kind == 2 && target as u64 >= file_count) {
            return Err(bad("Manifest Entry target is out of range"));
        }
        raw_entries.push((name, kind, target));
    }
    let mut directories = Vec::new();
    directories
        .try_reserve_exact(dir_count as usize)
        .map_err(|_| limit_error("unable to reserve Manifest directories"))?;
    let mut next_entry = 0u32;
    for i in 0..dir_count as usize {
        let at = dir_off as usize + i * DIRECTORY_SIZE;
        let first = read_u32(&manifest, at)?;
        let count = read_u32(&manifest, at + 4)?;
        if read_u32(&manifest, at + 8)? != 0
            || read_u32(&manifest, at + 12)? != 0
            || first != next_entry
            || (first as u64 + count as u64) > entry_count
        {
            return Err(bad("invalid Manifest Directory record or Entry partition"));
        }
        let mut dir = Vec::new();
        dir.try_reserve_exact(count as usize)
            .map_err(|_| limit_error("unable to reserve directory entries"))?;
        let begin = first as usize;
        for e in &mut raw_entries[begin..begin + count as usize] {
            if dir
                .last()
                .is_some_and(|p: &SourceEntry| p.name.as_bytes() >= e.0.as_bytes())
            {
                return Err(bad("directory entry names are not strictly sorted"));
            }
            dir.push(SourceEntry {
                name: std::mem::take(&mut e.0),
                kind: e.1,
                target: e.2,
            });
        }
        next_entry = next_entry
            .checked_add(count)
            .ok_or_else(|| bad("Entry partition overflow"))?;
        directories.push(dir);
    }
    if next_entry as u64 != entry_count {
        return Err(bad("Manifest Entry table has unowned records"));
    }
    Ok((directories, files, refs))
}

fn metadata_refs_empty() -> MetadataRefs {
    MetadataRefs {
        title: None,
        publisher: None,
        icon: NO_INDEX,
        nacp: NO_INDEX,
        localized: Vec::new(),
        opaque: Vec::new(),
    }
}

fn has_metadata(metadata: &MetadataRefs) -> bool {
    metadata.title.is_some()
        || metadata.publisher.is_some()
        || metadata.icon != NO_INDEX
        || metadata.nacp != NO_INDEX
        || !metadata.localized.is_empty()
        || !metadata.opaque.is_empty()
}

fn check_media_ref(
    media: &[SourceMedia],
    index: u32,
    allowed: &[u16],
    what: &str,
) -> Result<(), WriterError> {
    if index == NO_INDEX {
        return Ok(());
    }
    let item = media
        .get(index as usize)
        .ok_or_else(|| bad(format!("{what} Media index is out of range")))?;
    if !allowed.contains(&item.media_type) {
        return Err(bad(format!("{what} has an incompatible Media type")));
    }
    Ok(())
}

fn parse_catalog(
    cat: &[u8],
    media: &[SourceMedia],
    archive: u8,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
) -> Result<ParsedCatalog, WriterError> {
    budget.charge(cat.len() as u64, "source Catalog section")?;
    if cat.len() < 128
        || &cat[..4] != b"NXCT"
        || read_u16(cat, 4)? != 1
        || read_u16(cat, 6)? != 128
        || read_u64(cat, 8)? != cat.len() as u64
        || read_u32(cat, 16)? != 0
        || read_u32(cat, 44)? != 0
        || cat[112..128].iter().any(|&b| b != 0)
    {
        return Err(bad("invalid Catalog header"));
    }
    let app_count = read_u32(cat, 20)? as u64;
    let program_count = read_u32(cat, 24)? as u64;
    let snapshot_count = read_u32(cat, 28)? as u64;
    let local_count = read_u32(cat, 32)? as u64;
    let dep_count = read_u32(cat, 36)? as u64;
    let ext_count = read_u32(cat, 40)? as u64;
    if !(1..=4096).contains(&app_count)
        || program_count > 16384
        || !(1..=65536).contains(&snapshot_count)
        || local_count > 16384
        || dep_count > 32768
        || ext_count > 4096
    {
        return Err(bad("Catalog count exceeds NX 0.1 limits"));
    }
    let decoded = app_count * (std::mem::size_of::<SourceApplication>() as u64 + APP_SIZE as u64)
        + program_count * (std::mem::size_of::<SourceProgram>() as u64 + 8)
        + snapshot_count * (std::mem::size_of::<SourceSnapshot>() as u64 + 32)
        + dep_count * std::mem::size_of::<DependencyInput>() as u64;
    budget.charge(decoded, "decoded source Catalog rows")?;
    let app_off = read_u64(cat, 48)?;
    let prog_off = read_u64(cat, 56)?;
    let snap_off = read_u64(cat, 64)?;
    let local_off = read_u64(cat, 72)?;
    let dep_off = read_u64(cat, 80)?;
    let ext_off = read_u64(cat, 88)?;
    let pool_off = read_u64(cat, 96)?;
    let pool_size = read_u64(cat, 104)?;
    let mut cursor = 128u64;
    let expected = align_up(cursor, 8)?;
    if app_off != expected {
        return Err(bad("Catalog application offset is not canonical"));
    }
    cursor = app_off
        .checked_add(app_count * APP_SIZE as u64)
        .ok_or_else(|| bad("application table overflow"))?;
    if program_count == 0 {
        if prog_off != 0 {
            return Err(bad("empty Program table has an offset"));
        }
    } else {
        let expected = align_up(cursor, 8)?;
        if prog_off != expected {
            return Err(bad("Catalog Program offset is not canonical"));
        }
        cursor = prog_off
            .checked_add(program_count * PROGRAM_SIZE as u64)
            .ok_or_else(|| bad("Program table overflow"))?;
    }
    let expected = align_up(cursor, 8)?;
    if snap_off != expected {
        return Err(bad("Catalog Snapshot offset is not canonical"));
    }
    cursor = snap_off
        .checked_add(snapshot_count * SNAPSHOT_SIZE as u64)
        .ok_or_else(|| bad("Snapshot table overflow"))?;
    for (count, offset, size, name) in [
        (local_count, local_off, LOCAL_SIZE, "Localized Entry"),
        (dep_count, dep_off, DEPENDENCY_SIZE, "Dependency"),
        (ext_count, ext_off, EXTENSION_SIZE, "Extension"),
    ] {
        if count == 0 {
            if offset != 0 {
                return Err(bad(format!("empty {name} table has an offset")));
            }
        } else {
            let expected = align_up(cursor, 8)?;
            if offset != expected {
                return Err(bad(format!("Catalog {name} offset is not canonical")));
            }
            cursor = offset
                .checked_add(count * size as u64)
                .ok_or_else(|| bad(format!("{name} table overflow")))?;
        }
    }
    let expected_pool = align_up(cursor, 8)?;
    if pool_off != expected_pool
        || pool_off
            .checked_add(pool_size)
            .map_or(true, |end| end > cat.len() as u64)
    {
        return Err(bad("Catalog string pool placement is invalid"));
    }
    let pool_end = pool_off + pool_size;
    let mut app_ranges = Vec::new();
    let mut program_ranges = Vec::new();
    let mut snapshot_ranges = Vec::new();
    let mut local_ranges = Vec::new();
    let mut dependency_ranges = Vec::new();
    let mut ext_ranges = Vec::new();
    if program_count != 0 {
        program_ranges.push((prog_off, prog_off + program_count * PROGRAM_SIZE as u64));
    }
    app_ranges.push((app_off, app_off + app_count * APP_SIZE as u64));
    snapshot_ranges.push((snap_off, snap_off + snapshot_count * SNAPSHOT_SIZE as u64));
    if local_count != 0 {
        local_ranges.push((local_off, local_off + local_count * LOCAL_SIZE as u64));
    }
    if dep_count != 0 {
        dependency_ranges.push((dep_off, dep_off + dep_count * DEPENDENCY_SIZE as u64));
    }
    if ext_count != 0 {
        ext_ranges.push((ext_off, ext_off + ext_count * EXTENSION_SIZE as u64));
    }
    for (lo, hi) in app_ranges
        .iter()
        .chain(program_ranges.iter())
        .chain(snapshot_ranges.iter())
        .chain(local_ranges.iter())
        .chain(dependency_ranges.iter())
        .chain(ext_ranges.iter())
    {
        if *hi > pool_off || *lo % 8 != 0 {
            return Err(bad("Catalog table overlaps string pool or is misaligned"));
        }
    }
    let _ = (pool_end, options);

    #[derive(Clone)]
    struct AppRow {
        key: (u8, u64),
        flags: u8,
        fp: u32,
        pc: u32,
        fs: u32,
        sc: u32,
        title: Option<String>,
        publisher: Option<String>,
        icon: u32,
        fl: u32,
        lc: u32,
        nacp: u32,
        of: u32,
        oc: u32,
        selected: u32,
    }
    let mut app_rows = Vec::<AppRow>::new();
    let mut prior_app = None;
    let mut next_program = 0u32;
    let mut next_snapshot = 0u32;
    let mut next_local = 0u32;
    let mut apps = Vec::<SourceApplication>::new();
    for i in 0..app_count as usize {
        let at = app_off as usize + i * APP_SIZE;
        let app_id = read_u64(cat, at)?;
        let platform = cat[at + 8];
        let flags = cat[at + 9];
        let key = (platform, app_id);
        let fp = read_u32(cat, at + 12)?;
        let pc = read_u32(cat, at + 16)?;
        let fs = read_u32(cat, at + 20)?;
        let sc = read_u32(cat, at + 24)?;
        let title = catalog_string(
            cat,
            pool_off,
            pool_size,
            read_u32(cat, at + 28)?,
            read_u32(cat, at + 32)?,
            true,
            "application title",
        )?;
        let publisher = catalog_string(
            cat,
            pool_off,
            pool_size,
            read_u32(cat, at + 36)?,
            read_u32(cat, at + 40)?,
            true,
            "publisher",
        )?;
        let icon = read_u32(cat, at + 44)?;
        let fl = read_u32(cat, at + 48)?;
        let lc = read_u32(cat, at + 52)?;
        let nacp = read_u32(cat, at + 56)?;
        let of = read_u32(cat, at + 60)?;
        let oc = read_u32(cat, at + 64)?;
        let selected = read_u32(cat, at + 68)?;
        if app_id == 0
            || platform > 2
            || flags & !3 != 0
            || read_u16(cat, at + 10)? != 0
            || cat[at + 72..at + 80].iter().any(|&b| b != 0)
            || prior_app.is_some_and(|p| p >= key)
            || pc == 0 && fp != NO_INDEX
            || pc != 0 && fp != next_program
            || sc == 0
            || fs != next_snapshot
            || fs
                .checked_add(sc)
                .map_or(true, |end| end as u64 > snapshot_count)
            || fl != if lc == 0 { 0 } else { next_local }
            || fl
                .checked_add(lc)
                .map_or(true, |end| end as u64 > local_count)
            || (oc == 0 && of != 0)
            || of
                .checked_add(oc)
                .map_or(true, |end| end as usize > media.len())
            || selected != NO_INDEX && selected as u64 >= snapshot_count
        {
            return Err(bad("invalid application Catalog record or partition"));
        }
        if pc > 0 {
            next_program = next_program
                .checked_add(pc)
                .ok_or_else(|| bad("Program partition overflow"))?;
        }
        next_snapshot += sc;
        next_local = next_local
            .checked_add(lc)
            .ok_or_else(|| bad("localized-entry partition overflow"))?;
        check_media_ref(media, icon, &[1, 2], "application icon")?;
        check_media_ref(media, nacp, &[4], "application NACP")?;
        for mi in of..of + oc {
            check_media_ref(media, mi, &[5], "opaque-source range")?;
        }
        app_rows.push(AppRow {
            key,
            flags,
            fp,
            pc,
            fs,
            sc,
            title,
            publisher,
            icon,
            fl,
            lc,
            nacp,
            of,
            oc,
            selected,
        });
        apps.push(SourceApplication {
            key,
            selected_identity: None,
            optional_extensions: Vec::new(),
            source_resources: Vec::new(),
            recipes: Vec::new(),
        });
        prior_app = Some(key);
    }
    if next_program as u64 != program_count
        || next_snapshot as u64 != snapshot_count
        || next_local as u64 != local_count
    {
        return Err(bad(
            "Catalog application ranges do not partition Program/Snapshot/Localized tables",
        ));
    }

    let mut programs = Vec::<SourceProgram>::new();
    let mut program_id_to_global = Vec::new();
    program_id_to_global
        .try_reserve_exact(program_count as usize)
        .map_err(|_| limit_error("unable to reserve Program rows"))?;
    let mut previous_program_key = None;
    let mut expected_program = 0u32;
    for (app_index, app) in app_rows.iter().enumerate() {
        let mut previous_id = None;
        for local in 0..app.pc {
            let i = app.fp + local;
            if i != expected_program {
                return Err(bad("Program rows do not match application ranges"));
            }
            let at = prog_off as usize + i as usize * PROGRAM_SIZE;
            let id = read_u64(cat, at)?;
            let row_app = read_u32(cat, at + 8)?;
            let kind = cat[at + 12];
            let flags = cat[at + 13];
            let has_cid = flags & 1 != 0;
            let content_id: [u8; 16] = cat[at + 16..at + 32].try_into().unwrap();
            let first = read_u32(cat, at + 32)?;
            let count = read_u32(cat, at + 36)?;
            let npdm = read_u32(cat, at + 40)?;
            if id == 0
                || row_app as usize != app_index
                || kind > 2
                || flags & !1 != 0
                || read_u16(cat, at + 14)? != 0
                || cat[at + 44..at + 64].iter().any(|&b| b != 0)
                || (!has_cid && content_id != [0; 16])
                || (has_cid && content_id == [0; 16])
                || count == 0
                || first
                    .checked_add(count)
                    .map_or(true, |end| end as u64 > snapshot_count)
                || previous_id.is_some_and(|v| v >= id)
            {
                return Err(bad("invalid Program Catalog record"));
            }
            check_media_ref(media, npdm, &[3], "legacy Program NPDM")?;
            let key = (app.key.0, app.key.1, id);
            if previous_program_key.is_some_and(|v| v >= key) {
                return Err(bad("Program table is not in canonical order"));
            }
            previous_id = Some(id);
            previous_program_key = Some(key);
            program_id_to_global.push(id);
            programs.push(SourceProgram {
                key: app.key,
                program_id: id,
                kind,
                content_id,
                has_content_id: has_cid,
                first_snapshot: first,
                snapshot_count: count,
                npdm,
            });
            expected_program += 1;
        }
    }
    if expected_program as u64 != program_count {
        return Err(bad("Program table has unowned records"));
    }

    let mut snapshots = Vec::<SourceSnapshot>::new();
    snapshots
        .try_reserve_exact(snapshot_count as usize)
        .map_err(|_| limit_error("unable to reserve Snapshot rows"))?;
    let mut identities = HashSet::new();
    let mut expected_snapshot = 0u32;
    let mut expected_dependency = 0u32;
    for app in &app_rows {
        let mut previous_key: Option<SnapshotSortKey> = None;
        for i in app.fs..app.fs + app.sc {
            if i != expected_snapshot {
                return Err(bad("Snapshot rows do not match application ranges"));
            }
            let at = snap_off as usize + i as usize * SNAPSHOT_SIZE;
            let program_index = read_u32(cat, at)?;
            let snapshot_type = cat[at + 4];
            let flags = cat[at + 5];
            let version = read_u32(cat, at + 8)?;
            let root = read_u32(cat, at + 12)?;
            let logical_size = read_u64(cat, at + 16)?;
            let display_version = catalog_string(
                cat,
                pool_off,
                pool_size,
                read_u32(cat, at + 24)?,
                read_u32(cat, at + 28)?,
                true,
                "display version",
            )?;
            if let Some(value) = &display_version {
                budget.charge(value.len() as u64, "source display versions")?;
            }
            let content_id: [u8; 16] = cat[at + 32..at + 48].try_into().unwrap();
            let first_dep = read_u32(cat, at + 48)?;
            let dep_n = read_u32(cat, at + 52)?;
            let identity: [u8; 32] = cat[at + 56..at + 88].try_into().unwrap();
            let build_id: [u8; 32] = cat[at + 88..at + 120].try_into().unwrap();
            let addon = flags & 2 != 0;
            if snapshot_type > 4
                || flags & !3 != 0
                || (addon
                    && (snapshot_type != SnapshotType::Dlc as u8
                        || program_index != NO_INDEX
                        || content_id[..8] == [0; 8]
                        || content_id[8..] != [0; 8]))
                || read_u16(cat, at + 6)? != 0
                || (flags & 1 != 0) != (build_id != [0; 32])
                || identity == [0; 32]
                || !identities.insert(identity)
                || first_dep != expected_dependency
                || first_dep
                    .checked_add(dep_n)
                    .map_or(true, |end| end as u64 > dep_count)
                || program_index != NO_INDEX && program_index as u64 >= program_count
                || (program_index == NO_INDEX && snapshot_type != SnapshotType::Dlc as u8)
            {
                return Err(bad("invalid Snapshot Catalog record"));
            }
            let program_id = if program_index == NO_INDEX {
                None
            } else {
                Some(program_id_to_global[program_index as usize])
            };
            let app_index_for_program = if program_index == NO_INDEX {
                None
            } else {
                let p = &programs[program_index as usize];
                Some(p.key)
            };
            if app_index_for_program.is_some_and(|key| key != app.key) {
                return Err(bad("Snapshot Program belongs to another application"));
            }
            let sort_key = (program_index, snapshot_type, version, content_id, build_id);
            if previous_key.is_some_and(|v| v >= sort_key) {
                return Err(bad("Snapshot rows are not in canonical order"));
            }
            previous_key = Some(sort_key);
            let mut dependencies = Vec::new();
            for di in first_dep..first_dep + dep_n {
                let d = dep_off as usize + di as usize * DEPENDENCY_SIZE;
                let kind = match cat[d] {
                    1 => DependencyKind::RequiredBase,
                    2 => DependencyKind::Requires,
                    3 => DependencyKind::Recommends,
                    _ => return Err(bad("invalid dependency kind")),
                };
                let dflags = cat[d + 1];
                let cid: [u8; 16] = cat[d + 4..d + 20].try_into().unwrap();
                let target_snapshot_identity: [u8; 32] = cat[d + 20..d + 52].try_into().unwrap();
                if dflags & !1 != 0
                    || read_u16(cat, d + 2)? != 0
                    || read_u32(cat, d + 52)? != 0
                    || (dflags & 1 == 0 && cid != [0; 16])
                    || (dflags & 1 != 0 && cid == [0; 16])
                    || target_snapshot_identity == [0; 32]
                {
                    return Err(bad("invalid dependency record"));
                }
                dependencies.push(DependencyInput {
                    kind,
                    content_id: if dflags & 1 != 0 { Some(cid) } else { None },
                    target_snapshot_identity,
                });
            }
            expected_dependency = expected_dependency
                .checked_add(dep_n)
                .ok_or_else(|| bad("dependency partition overflow"))?;
            let snapshot = FinalSnapshot {
                program_index,
                snapshot_type,
                version,
                root,
                logical_size,
                display_version,
                content_id,
                addon,
                build_id,
                identity,
                dependencies,
                metadata: None,
                meta_refs: None,
                npdm: NO_INDEX,
                required_firmware: None,
            };
            snapshots.push(SourceSnapshot {
                archive,
                key: app.key,
                program_id,
                snapshot,
            });
            expected_snapshot += 1;
        }
    }
    if expected_snapshot as u64 != snapshot_count || expected_dependency as u64 != dep_count {
        return Err(bad("Snapshot or Dependency table has unowned records"));
    }

    // Extensions are parsed after the owning rows exist. Payload bytes are
    // retained only for optional values that can be safely remapped.
    let mut optional_global = Vec::new();
    let mut app_extras: Vec<Vec<OptionalAppExtension>> = vec![Vec::new(); apps.len()];
    let mut app_recipes: Vec<Vec<Vec<u8>>> = vec![Vec::new(); apps.len()];
    let mut ext_keys = HashSet::new();
    let mut ext_payload_end = pool_end;
    for i in 0..ext_count as usize {
        let at = ext_off as usize + i * EXTENSION_SIZE;
        let type_id = read_u32(cat, at)?;
        let flags = read_u32(cat, at + 4)?;
        let owner_kind = read_u16(cat, at + 8)?;
        let owner_index = read_u32(cat, at + 12)?;
        let payload_off = read_u64(cat, at + 16)?;
        let payload_size = read_u64(cat, at + 24)?;
        if flags & !1 != 0
            || read_u16(cat, at + 10)? != 0
            || payload_size == 0
            || !ext_keys.insert((type_id, owner_kind, owner_index))
            || owner_kind > 2
            || (owner_kind == 0 && owner_index != 0)
            || (owner_kind == 1 && owner_index as usize >= apps.len())
            || (owner_kind == 2 && owner_index as u64 >= program_count)
        {
            return Err(bad("invalid or duplicate Catalog extension descriptor"));
        }
        let payload_expected = align_up(ext_payload_end, 8)?;
        if payload_off != payload_expected
            || payload_off
                .checked_add(payload_size)
                .map_or(true, |end| end > cat.len() as u64)
        {
            return Err(bad("Catalog extension payload placement is invalid"));
        }
        if cat[ext_payload_end as usize..payload_expected as usize]
            .iter()
            .any(|&b| b != 0)
        {
            return Err(bad("nonzero Catalog extension padding"));
        }
        ext_payload_end = payload_off + payload_size;
        let range = checked_range(cat, payload_off, payload_size)?;
        let payload = &cat[range];
        match type_id {
            HISTORY_EXTENSION => {
                if flags != 1 || owner_kind != 1 {
                    return Err(bad("invalid Snapshot Metadata History descriptor"));
                }
                parse_history_extension(
                    cat,
                    pool_off,
                    pool_size,
                    media,
                    app_rows[owner_index as usize].key,
                    app_rows[owner_index as usize].selected,
                    snapshots.as_mut_slice(),
                    payload,
                )?;
            }
            NATIVE_EXTENSION => {
                if flags != 1 || owner_kind != 1 {
                    return Err(bad("invalid Snapshot Native Metadata descriptor"));
                }
                parse_native_extension(
                    media,
                    app_rows[owner_index as usize].key,
                    snapshots.as_mut_slice(),
                    payload,
                )?;
            }
            0x0001_0001..=0x0001_0003 => {
                if flags != 0
                    || owner_kind != 1
                    || app_rows[owner_index as usize].selected == NO_INDEX
                {
                    return Err(bad(
                        "application discovery extension lacks an exact metadata source",
                    ));
                }
                validate_discovery_extension(type_id, payload)?;
                budget.charge(payload.len() as u64, "retained Catalog extensions")?;
                app_extras[owner_index as usize].push((
                    type_id,
                    flags,
                    owner_kind,
                    payload.to_vec(),
                ));
            }
            SOURCE_RESOURCES => {
                if flags != 1
                    || owner_kind != 1
                    || payload.len() < 16
                    || &payload[..4] != b"NXSR"
                    || read_u16(payload, 4)? != 1
                    || read_u16(payload, 6)? != 16
                    || read_u32(payload, 12)? != 0
                {
                    return Err(bad("invalid source resource descriptor"));
                }
                let count = read_u32(payload, 8)? as usize;
                if count == 0 || count > 4096 || payload.len() != 16 + count * 8 {
                    return Err(bad("invalid source resource count"));
                }
                budget.charge(count as u64 * 16, "source resource descriptors")?;
                let mut prior = None;
                for row in payload[16..].chunks_exact(8) {
                    let item = (read_u32(row, 0)?, read_u32(row, 4)?);
                    if item.0 > 1 || prior.is_some_and(|p| p >= item) {
                        return Err(bad("invalid or unsorted source resource"));
                    }
                    prior = Some(item);
                    apps[owner_index as usize].source_resources.push(item);
                }
            }
            RECIPE_EXTENSION => {
                if flags != 0 || owner_kind != 1 {
                    return Err(bad("invalid rebuild recipe descriptor"));
                }
                budget.charge(payload.len() as u64, "rebuild recipes")?;
                app_recipes[owner_index as usize] = decode_recipes(payload)?;
            }
            _ if flags & 1 != 0 => return Err(bad("unknown required Catalog extension")),
            _ => {
                budget.charge(payload.len() as u64, "retained Catalog extensions")?;
                optional_global.push((type_id, flags, owner_kind, owner_index, payload.to_vec()));
            }
        }
    }
    if ext_payload_end != cat.len() as u64 {
        return Err(bad("Catalog contains trailing or missing extension bytes"));
    }

    // Promote the single-snapshot legacy Program NPDM representation to its
    // exact snapshot association while rebuilding Catalog indexes.
    for program in &mut programs {
        if program.npdm != NO_INDEX {
            let snapshot = snapshots
                .get_mut(program.first_snapshot as usize)
                .ok_or_else(|| bad("legacy NPDM snapshot is missing"))?;
            if snapshot.snapshot.npdm != NO_INDEX {
                return Err(bad("NPDM is duplicated in legacy and snapshot metadata"));
            }
            snapshot.snapshot.npdm = program.npdm;
            program.npdm = NO_INDEX;
        }
    }

    // Attach current display metadata to its explicit source snapshot.
    for (ai, app) in app_rows.iter().enumerate() {
        let selected_identity = if app.selected == NO_INDEX {
            None
        } else {
            let selected = snapshots
                .get(app.selected as usize)
                .ok_or_else(|| bad("selected metadata snapshot index is out of range"))?;
            if selected.key != app.key || selected.snapshot.snapshot_type == SnapshotType::Dlc as u8
            {
                return Err(bad(
                    "selected metadata snapshot belongs to a different application or is DLC",
                ));
            }
            Some(selected.snapshot.identity)
        };
        let mut meta = metadata_refs_empty();
        meta.title = app.title.clone();
        meta.publisher = app.publisher.clone();
        meta.icon = app.icon;
        meta.nacp = app.nacp;
        if app.lc != 0 {
            for li in app.fl..app.fl + app.lc {
                let at = local_off as usize + li as usize * LOCAL_SIZE;
                let language = catalog_string(
                    cat,
                    pool_off,
                    pool_size,
                    read_u32(cat, at)?,
                    read_u32(cat, at + 4)?,
                    false,
                    "language tag",
                )?
                .ok_or_else(|| bad("language tag is absent"))?;
                // Same rule as the writer and the C++ reader.
                if !valid_language(&language) {
                    return Err(bad("localized language tag is not lowercase BCP 47"));
                }
                let title = catalog_string(
                    cat,
                    pool_off,
                    pool_size,
                    read_u32(cat, at + 8)?,
                    read_u32(cat, at + 12)?,
                    true,
                    "localized title",
                )?;
                let icon = read_u32(cat, at + 16)?;
                if read_u32(cat, at + 20)? != 0
                    || language.len() > 63
                    || !language
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    || meta
                        .localized
                        .last()
                        .is_some_and(|old| old.language.as_bytes() >= language.as_bytes())
                {
                    return Err(bad("invalid or unsorted localized Catalog entry"));
                }
                check_media_ref(media, icon, &[1, 2], "localized icon")?;
                if title.is_none() && icon == NO_INDEX {
                    return Err(bad("localized row has neither title nor icon"));
                }
                meta.localized.push(LocalizedRef {
                    language,
                    title,
                    icon,
                });
            }
        }
        if app.oc != 0 {
            meta.opaque.extend(app.of..app.of + app.oc);
        }
        if app.icon == NO_INDEX
            && app.nacp == NO_INDEX
            && app.title.is_none()
            && app.publisher.is_none()
            && meta.localized.is_empty()
            && meta.opaque.is_empty()
        {
            if app.selected != NO_INDEX {
                return Err(bad("metadata source is present without metadata"));
            }
        } else if app.selected == NO_INDEX {
            return Err(bad(
                "application metadata is not associated with a snapshot",
            ));
        }
        if app.flags & 1
            != snapshots[app.fs as usize..(app.fs + app.sc) as usize]
                .iter()
                .any(|s| s.snapshot.snapshot_type == SnapshotType::Base as u8) as u8
            || (app.flags >> 1) & 1
                != snapshots[app.fs as usize..(app.fs + app.sc) as usize]
                    .iter()
                    .any(|s| s.snapshot.snapshot_type == SnapshotType::Dlc as u8)
                    as u8
        {
            return Err(bad("application flags disagree with its snapshots"));
        }
        if let Some(id) = selected_identity {
            let selected = snapshots
                .iter_mut()
                .find(|s| s.key == app.key && s.snapshot.identity == id)
                .ok_or_else(|| bad("selected metadata identity is missing"))?;
            if selected.snapshot.meta_refs.replace(meta).is_some() {
                return Err(bad("duplicate metadata source association"));
            }
        }
        apps[ai].selected_identity = selected_identity;
        apps[ai].optional_extensions = std::mem::take(&mut app_extras[ai]);
        apps[ai].recipes = std::mem::take(&mut app_recipes[ai]);
    }
    // Extensions must cover every non-selected metadata-bearing snapshot and
    // execution row exactly once; validator semantics are reproduced here.
    for program in &programs {
        if program.snapshot_count == 0
            || program.first_snapshot as u64 + program.snapshot_count as u64 > snapshot_count
            || program.npdm != NO_INDEX && program.snapshot_count != 1
        {
            return Err(bad(
                "Program snapshot range or legacy NPDM association is ambiguous",
            ));
        }
        for si in program.first_snapshot..program.first_snapshot + program.snapshot_count {
            let snapshot = &snapshots[si as usize];
            if snapshot.key != program.key || snapshot.program_id != Some(program.program_id) {
                return Err(bad(
                    "Program snapshot range does not match its Snapshot rows",
                ));
            }
        }
    }
    for (snapshot_index, snapshot) in snapshots.iter().enumerate() {
        if let Some(id) = snapshot.program_id {
            let program = programs
                .iter()
                .find(|p| p.key == snapshot.key && p.program_id == id)
                .ok_or_else(|| bad("Snapshot references a missing Program"))?;
            if (snapshot_index as u32) < program.first_snapshot
                || (snapshot_index as u32) >= program.first_snapshot + program.snapshot_count
            {
                return Err(bad("Snapshot is outside its Program range"));
            }
        }
    }
    Ok((apps, programs, snapshots, optional_global))
}

#[allow(clippy::too_many_arguments)]
fn parse_history_extension(
    cat: &[u8],
    pool_off: u64,
    pool_size: u64,
    media: &[SourceMedia],
    key: (u8, u64),
    selected: u32,
    snapshots: &mut [SourceSnapshot],
    payload: &[u8],
) -> Result<(), WriterError> {
    if payload.len() < 24
        || &payload[..4] != b"NXSM"
        || read_u16(payload, 4)? != 1
        || read_u16(payload, 6)? != 24
        || read_u32(payload, 20)? != 0
    {
        return Err(bad("invalid Snapshot Metadata History header"));
    }
    let rows = read_u32(payload, 8)? as usize;
    let locals = read_u32(payload, 12)? as usize;
    let opaques = read_u32(payload, 16)? as usize;
    let expected_size = 24usize
        .checked_add(
            rows.checked_mul(48)
                .ok_or_else(|| bad("metadata history row size overflow"))?,
        )
        .and_then(|v| v.checked_add(locals.checked_mul(LOCAL_SIZE)?))
        .and_then(|v| v.checked_add(opaques.checked_mul(4)?))
        .ok_or_else(|| bad("metadata history size overflow"))?;
    if rows == 0
        || rows > 65536
        || locals > 16384
        || expected_size != payload.len()
        || selected == NO_INDEX
    {
        return Err(bad(
            "Snapshot Metadata History counts, size, or selected source are invalid",
        ));
    }
    let local_start = 24 + rows * 48;
    let opaque_start = local_start + locals * LOCAL_SIZE;
    let mut next_local = 0u32;
    let mut next_opaque = 0u32;
    let mut previous_snapshot = None;
    for ri in 0..rows {
        let at = 24 + ri * 48;
        let si = read_u32(payload, at)?;
        let snapshot = snapshots
            .get_mut(si as usize)
            .ok_or_else(|| bad("metadata history snapshot index is out of range"))?;
        let title = catalog_string(
            cat,
            pool_off,
            pool_size,
            read_u32(payload, at + 4)?,
            read_u32(payload, at + 8)?,
            true,
            "historical title",
        )?;
        let publisher = catalog_string(
            cat,
            pool_off,
            pool_size,
            read_u32(payload, at + 12)?,
            read_u32(payload, at + 16)?,
            true,
            "historical publisher",
        )?;
        let icon = read_u32(payload, at + 20)?;
        let nacp = read_u32(payload, at + 24)?;
        let first_local = read_u32(payload, at + 28)?;
        let local_n = read_u32(payload, at + 32)?;
        let first_opaque = read_u32(payload, at + 36)?;
        let opaque_n = read_u32(payload, at + 40)?;
        if read_u32(payload, at + 44)? != 0
            || snapshot.key != key
            || si == selected
            || previous_snapshot.is_some_and(|old| old >= si)
            || first_local != next_local
            || first_opaque != next_opaque
            || first_local
                .checked_add(local_n)
                .map_or(true, |end| end as usize > locals)
            || first_opaque
                .checked_add(opaque_n)
                .map_or(true, |end| end as usize > opaques)
        {
            return Err(bad("invalid or unsorted Snapshot Metadata History row"));
        }
        check_media_ref(media, icon, &[1, 2], "historical icon")?;
        check_media_ref(media, nacp, &[4], "historical NACP")?;
        let mut metadata = metadata_refs_empty();
        metadata.title = title;
        metadata.publisher = publisher;
        metadata.icon = icon;
        metadata.nacp = nacp;
        let mut last_language: Option<String> = None;
        for li in first_local..first_local + local_n {
            let lat = local_start + li as usize * LOCAL_SIZE;
            let language = catalog_string(
                cat,
                pool_off,
                pool_size,
                read_u32(payload, lat)?,
                read_u32(payload, lat + 4)?,
                false,
                "historical language",
            )?
            .ok_or_else(|| bad("historical language is absent"))?;
            let title = catalog_string(
                cat,
                pool_off,
                pool_size,
                read_u32(payload, lat + 8)?,
                read_u32(payload, lat + 12)?,
                true,
                "historical localized title",
            )?;
            let icon = read_u32(payload, lat + 16)?;
            if read_u32(payload, lat + 20)? != 0
                || language.len() > 63
                || !language
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                || last_language
                    .as_ref()
                    .is_some_and(|old| old.as_bytes() >= language.as_bytes())
            {
                return Err(bad("invalid historical localized metadata"));
            }
            check_media_ref(media, icon, &[1, 2], "historical localized icon")?;
            if title.is_none() && icon == NO_INDEX {
                return Err(bad("historical localized row has no content"));
            }
            last_language = Some(language.clone());
            metadata.localized.push(LocalizedRef {
                language,
                title,
                icon,
            });
        }
        for oi in first_opaque..first_opaque + opaque_n {
            let idx = read_u32(payload, opaque_start + oi as usize * 4)?;
            check_media_ref(media, idx, &[5], "historical opaque media")?;
            metadata.opaque.push(idx);
        }
        if !has_metadata(&metadata) || snapshot.snapshot.meta_refs.replace(metadata).is_some() {
            return Err(bad(
                "history row is empty or duplicates a snapshot metadata source",
            ));
        }
        next_local += local_n;
        next_opaque += opaque_n;
        previous_snapshot = Some(si);
    }
    if next_local as usize != locals || next_opaque as usize != opaques {
        return Err(bad(
            "Snapshot Metadata History arrays are not fully partitioned",
        ));
    }
    Ok(())
}

fn parse_native_extension(
    media: &[SourceMedia],
    key: (u8, u64),
    snapshots: &mut [SourceSnapshot],
    payload: &[u8],
) -> Result<(), WriterError> {
    if payload.len() < 32
        || &payload[..4] != b"NXNM"
        || read_u16(payload, 4)? != 1
        || read_u16(payload, 6)? != 16
        || read_u32(payload, 12)? != 0
    {
        return Err(bad("invalid Snapshot Native Metadata header"));
    }
    let count = read_u32(payload, 8)? as usize;
    if count == 0
        || count > 65536
        || 16usize.checked_add(
            count
                .checked_mul(16)
                .ok_or_else(|| bad("native metadata rows overflow"))?,
        ) != Some(payload.len())
    {
        return Err(bad("Snapshot Native Metadata count or size is invalid"));
    }
    let mut previous = None;
    let mut any = false;
    for i in 0..count {
        let at = 16 + i * 16;
        let si = read_u32(payload, at)?;
        let snapshot = snapshots
            .get_mut(si as usize)
            .ok_or_else(|| bad("native metadata snapshot index is out of range"))?;
        let npdm = read_u32(payload, at + 4)?;
        let firmware = read_u32(payload, at + 8)?;
        let flags = read_u32(payload, at + 12)?;
        if snapshot.key != key
            || previous.is_some_and(|old| old >= si)
            || flags & !1 != 0
            || (flags & 1 == 0 && firmware != 0)
        {
            return Err(bad("invalid or unsorted Snapshot Native Metadata row"));
        }
        check_media_ref(media, npdm, &[3], "snapshot NPDM")?;
        if npdm == NO_INDEX && flags == 0 {
            return Err(bad("empty Snapshot Native Metadata row"));
        }
        snapshot.snapshot.npdm = npdm;
        snapshot.snapshot.required_firmware = if flags & 1 != 0 { Some(firmware) } else { None };
        any = true;
        previous = Some(si);
    }
    if !any {
        return Err(bad("empty Snapshot Native Metadata extension"));
    }
    Ok(())
}

fn validate_discovery_extension(type_id: u32, payload: &[u8]) -> Result<(), WriterError> {
    match type_id {
        0x0001_0001 if payload.len() == 4 => Ok(()),
        0x0001_0002 => {
            if payload.len() < 3 {
                return Err(bad("truncated supported-language extension"));
            }
            let count = read_u16(payload, 0)? as usize;
            if count == 0 || count > 64 {
                return Err(bad("supported-language count is invalid"));
            }
            let mut at = 2usize;
            let mut prior: Option<&[u8]> = None;
            for _ in 0..count {
                let len = *payload
                    .get(at)
                    .ok_or_else(|| bad("truncated language tag length"))?
                    as usize;
                at += 1;
                let end = at
                    .checked_add(len)
                    .ok_or_else(|| bad("language tag size overflow"))?;
                let value = payload
                    .get(at..end)
                    .ok_or_else(|| bad("truncated language tag"))?;
                if !(1..=63).contains(&len)
                    || !value
                        .iter()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
                    || prior.is_some_and(|old| old >= value)
                {
                    return Err(bad("invalid or unsorted supported-language tag"));
                }
                prior = Some(value);
                at = end;
            }
            if at != payload.len() {
                return Err(bad("trailing supported-language bytes"));
            }
            Ok(())
        }
        0x0001_0003 => {
            if payload.len() < 4 {
                return Err(bad("truncated region extension"));
            }
            let count = read_u16(payload, 0)? as usize;
            if count == 0 || count > 249 || payload.len() != 2 + count * 2 {
                return Err(bad("region count or size is invalid"));
            }
            let mut prior: Option<&[u8]> = None;
            for i in 0..count {
                let region = &payload[2 + i * 2..4 + i * 2];
                if !region.iter().all(u8::is_ascii_uppercase)
                    || prior.is_some_and(|old| old >= region)
                {
                    return Err(bad("invalid or unsorted region code"));
                }
                prior = Some(region);
            }
            Ok(())
        }
        _ => Err(bad("invalid discovery extension payload")),
    }
}

fn visit_identity_tree(
    dir_id: u32,
    path: &[u8],
    depth: usize,
    archive: &SourceArchive,
    seen: &mut HashSet<(u8, u32)>,
    nodes: &mut Vec<IdentityNode>,
    logical_size: &mut u64,
) -> Result<(), WriterError> {
    if depth > MAX_DIRECTORY_DEPTH || !seen.insert((1, dir_id)) {
        return Err(bad(
            "snapshot directory tree is cyclic, shared internally, or too deep",
        ));
    }
    let directory = archive
        .directories
        .get(dir_id as usize)
        .ok_or_else(|| bad("snapshot root or child directory is out of range"))?;
    for entry in directory {
        let full = full_path(path, &entry.name)?;
        if entry.kind == 1 {
            nodes.push(IdentityNode {
                path: full.clone(),
                kind: 1,
                source_file: None,
            });
            visit_identity_tree(
                entry.target,
                &full,
                depth + 1,
                archive,
                seen,
                nodes,
                logical_size,
            )?;
        } else {
            let file = archive
                .files
                .get(entry.target as usize)
                .ok_or_else(|| bad("tree references missing File record"))?;
            // A File record may appear at one path per snapshot (SECURITY.md).
            if !seen.insert((2, entry.target)) {
                return Err(bad("File record repeated within one snapshot"));
            }
            *logical_size = logical_size
                .checked_add(file.size)
                .ok_or_else(|| bad("snapshot logical size overflow"))?;
            nodes.push(IdentityNode {
                path: full,
                kind: 2,
                source_file: Some(entry.target),
            });
        }
    }
    Ok(())
}

/// Cumulative snapshot traversal ceilings, the C++ reader's defaults
/// (`max_total_snapshot_nodes`, `max_total_snapshot_path_bytes`). Snapshots
/// may share directories, so without them a small Manifest shared by many
/// snapshots costs snapshots x nodes to verify.
const MAX_TOTAL_SNAPSHOT_NODES: u64 = 16_000_000;
const MAX_TOTAL_SNAPSHOT_PATH_BYTES: u64 = 512 * 1024 * 1024;

fn validate_snapshot_identities(archive: &SourceArchive) -> Result<(), WriterError> {
    validate_snapshot_identities_within(
        archive,
        MAX_TOTAL_SNAPSHOT_NODES,
        MAX_TOTAL_SNAPSHOT_PATH_BYTES,
    )
}

fn validate_snapshot_identities_within(
    archive: &SourceArchive,
    max_total_nodes: u64,
    max_total_path_bytes: u64,
) -> Result<(), WriterError> {
    let file_ids: Vec<u32> = (0..archive.files.len()).map(|i| i as u32).collect();
    let (mut total_nodes, mut total_paths) = (0u64, 0u64);
    for source in &archive.snapshots {
        let mut nodes = vec![IdentityNode {
            path: Vec::new(),
            kind: 1,
            source_file: None,
        }];
        let mut seen = HashSet::new();
        let mut logical_size = 0u64;
        visit_identity_tree(
            source.snapshot.root,
            &[],
            0,
            archive,
            &mut seen,
            &mut nodes,
            &mut logical_size,
        )?;
        total_nodes = total_nodes.saturating_add(nodes.len() as u64);
        total_paths = nodes
            .iter()
            .fold(total_paths, |t, n| t.saturating_add(n.path.len() as u64));
        if total_nodes > max_total_nodes || total_paths > max_total_path_bytes {
            return Err(limit_error(
                "cumulative snapshot tree nodes or path bytes exceed the reader limit",
            ));
        }
        if logical_size != source.snapshot.logical_size {
            return Err(bad(
                "Snapshot logical size disagrees with its Manifest tree",
            ));
        }
        let app = archive
            .applications
            .iter()
            .find(|app| app.key == source.key)
            .ok_or_else(|| bad("Snapshot application row is missing"))?;
        let computed = snapshot_identity(
            app.key.0,
            app.key.1,
            source.program_id,
            source.snapshot.snapshot_type,
            source.snapshot.version,
            source.snapshot.content_id,
            &mut nodes,
            &file_ids,
            &archive.files,
        )?;
        if computed != source.snapshot.identity {
            return Err(bad(
                "Snapshot canonical identity does not match its logical tree",
            ));
        }
    }
    Ok(())
}

fn parse_archive(
    file: &mut File,
    archive_id: u8,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
) -> Result<SourceArchive, WriterError> {
    cancelled(options)?;
    let header = parse_header(file, options)?;
    let media = parse_media(file, header, archive_id)?;
    let blocks = parse_blocks(file, header, options, budget, archive_id)?;
    let catalog = read_vec_at(
        file,
        header.catalog_offset,
        header.catalog_size,
        MAX_CATALOG,
        "Catalog",
    )?;
    let (applications, programs, snapshots, optional_global_extensions) =
        parse_catalog(&catalog, &media, archive_id, options, budget)?;
    drop(catalog);
    let (directories, files, block_refs) = parse_manifest(file, header, &blocks, options, budget)?;
    let parsed = SourceArchive {
        archive: archive_id,
        file_size: header.archive_size,
        generation: header.generation,
        slots_end: header.slots_end,
        active_slot: header.active_slot,
        tail_start: if header.media_size == 0 {
            header.catalog_offset
        } else {
            header.media_offset
        },
        applications,
        programs,
        snapshots,
        directories,
        files,
        block_refs,
        blocks,
        media,
        optional_global_extensions,
        inherited_files: HashMap::new(),
    };
    let resource_count: usize = parsed
        .applications
        .iter()
        .map(|a| a.source_resources.len())
        .sum();
    if resource_count > 4096 || (header.minor == 3) != (resource_count != 0) {
        return Err(bad("format version disagrees with source resources"));
    }
    let mut roots = HashSet::new();
    for app in &parsed.applications {
        for &(kind, root) in &app.source_resources {
            if !roots.insert(root) {
                return Err(bad("shared source resource root"));
            }
            let entries = parsed
                .directories
                .get(root as usize)
                .ok_or_else(|| bad("source resource root out of range"))?;
            if entries.is_empty() || entries.iter().any(|e| e.kind != 2) {
                return Err(bad("source resource must be a nonempty flat file set"));
            }
            if kind == 1 && (entries.len() != 1 || entries[0].name != "partition.hfs0") {
                return Err(bad("system update resource must contain partition.hfs0"));
            }
            let mut seen = HashSet::new();
            for e in entries {
                if !seen.insert(e.target) {
                    return Err(bad("repeated file in source resource"));
                }
            }
        }
    }
    if !roots.is_empty() {
        if parsed
            .snapshots
            .iter()
            .any(|s| roots.contains(&s.snapshot.root))
        {
            return Err(bad("source resource root belongs to a snapshot"));
        }
        if parsed
            .directories
            .iter()
            .flatten()
            .any(|e| e.kind == 1 && roots.contains(&e.target))
        {
            return Err(bad("source resource root is a child directory"));
        }
    }
    validate_snapshot_identities(&parsed)?;
    Ok(parsed)
}

fn decode_source_block(
    file: &mut File,
    block: &SourceBlock,
    frame: &mut Vec<u8>,
    logical: &mut Vec<u8>,
    decoder: &mut zstd::bulk::Decompressor<'static>,
    options: &WriterOptions,
) -> Result<usize, WriterError> {
    cancelled(options)?;
    let stored = usize::try_from(block.stored_size)
        .map_err(|_| limit_error("stored block exceeds address space"))?;
    let output_size = block.logical_size as usize;
    logical
        .try_reserve(output_size.saturating_sub(logical.len()))
        .map_err(|_| limit_error("unable to reserve decoded block"))?;
    // Existing bytes are overwritten by an exact read/decode before use. Keep
    // the length so equal-sized blocks do not zero-fill the same storage again.
    logical.resize(output_size, 0);
    match block.codec {
        0 => {
            if stored != output_size {
                return Err(bad("RAW source block has inconsistent sizes"));
            }
            // RAW needs no second full-block allocation or memory copy.
            read_exact_at(file, block.range.offset, logical)?;
        }
        1 => {
            frame
                .try_reserve(stored.saturating_sub(frame.len()))
                .map_err(|_| limit_error("unable to reserve source frame"))?;
            frame.resize(stored, 0);
            read_exact_at(file, block.range.offset, frame)?;
            let frame_size = zstd::zstd_safe::find_frame_compressed_size(frame)
                .map_err(|_| bad("invalid Zstandard frame in source archive"))?;
            let content_size = zstd::zstd_safe::get_frame_content_size(frame)
                .ok()
                .flatten();
            if frame_size != frame.len() || content_size != Some(block.logical_size as u64) {
                return Err(bad(
                    "source Zstandard payload is not one complete frame with its declared size",
                ));
            }
            let decoded = decoder.decompress_to_buffer(frame, logical).map_err(|_| {
                bad("source Zstandard frame cannot be decoded within the window limit")
            })?;
            if decoded != output_size {
                return Err(bad("source Zstandard block decoded to the wrong size"));
            }
        }
        _ => return Err(bad("unsupported source block codec")),
    }
    Ok(output_size)
}

fn same_file_structure(archive: &SourceArchive, a: &FileBuild, b: &FileBuild) -> bool {
    a.size == b.size
        && a.chunk_size_bytes == b.chunk_size_bytes
        && a.digest == b.digest
        && a.block_count == b.block_count
        && archive.block_refs[a.first_ref as usize..(a.first_ref + a.block_count) as usize]
            == archive.block_refs[b.first_ref as usize..(b.first_ref + b.block_count) as usize]
}

#[cfg(test)]
mod strict_cache_tests {
    use super::*;
    #[test]
    fn snapshot_traversal_is_bounded_across_snapshots() {
        // Two snapshots share one tree: each is small, together they count twice.
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/shared_snapshots.nx");
        let options = WriterOptions::default();
        let mut budget = MemoryBudget {
            current: 0,
            peak: 0,
            limit: options.max_memory_bytes,
        };
        let archive =
            parse_archive(&mut File::open(&path).unwrap(), 0, &options, &mut budget).unwrap();
        assert_eq!(archive.snapshots.len(), 2);
        let mut one = Vec::new();
        visit_identity_tree(
            archive.snapshots[0].snapshot.root,
            &[],
            0,
            &archive,
            &mut HashSet::new(),
            &mut one,
            &mut 0,
        )
        .unwrap();
        let per_snapshot = one.len() as u64 + 1;
        validate_snapshot_identities_within(&archive, 2 * per_snapshot, u64::MAX).unwrap();
        let error = validate_snapshot_identities_within(&archive, 2 * per_snapshot - 1, u64::MAX)
            .unwrap_err();
        assert_eq!(error.kind(), WriterErrorKind::ResourceLimit);
        let paths: u64 = one.iter().map(|n| n.path.len() as u64).sum();
        assert!(validate_snapshot_identities_within(&archive, u64::MAX, 2 * paths - 1).is_err());
        validate_snapshot_identities_within(&archive, u64::MAX, 2 * paths).unwrap();
    }
    #[test]
    fn strict_cache_requires_digest_structure_and_exact_reference_sequence() {
        let (empty, guard) =
            unique_temp(&std::env::temp_dir(), OsStr::new("nx-cache-test")).unwrap();
        drop(empty);
        let mut options = WriterOptions {
            chunk_size_bytes: 65536,
            ..WriterOptions::default()
        };
        let snapshot = SnapshotInput {
            addon_title_id: None,
            native: SnapshotNativeMetadata::default(),
            program_id: Some(1),
            snapshot_type: SnapshotType::Base,
            numeric_version: 0,
            content_id: None,
            build_id: None,
            display_version: None,
            dependencies: Vec::new(),
            metadata: None,
            root: DirectoryInput::new(vec![
                EntryInput::file("a", FileInput::from_bytes(vec![1; 131072])),
                EntryInput::file("b", FileInput::from_bytes(vec![2; 131072])),
            ]),
        };
        write_archive(
            ArchiveInput::new(vec![ApplicationInput {
                application_id: 1,
                platform: Platform::Switch1,
                programs: vec![ProgramInput {
                    program_id: 1,
                    kind: ProgramKind::Primary,
                    content_id: None,
                    npdm: None,
                }],
                snapshots: vec![snapshot],
            }]),
            &guard.path,
            &options,
        )
        .unwrap();
        let mut file = File::open(&guard.path).unwrap();
        let mut budget = MemoryBudget {
            current: 0,
            peak: 0,
            limit: options.max_memory_bytes,
        };
        let mut archive = parse_archive(&mut file, 0, &options, &mut budget).unwrap();
        archive.files.push(archive.files[0].clone());
        options.strict_file_cache_entries = 0;
        let baseline = verify_source_payloads(&mut file, &archive, &options).unwrap();
        options.strict_file_cache_entries = 128;
        let cached = verify_source_payloads(&mut file, &archive, &options).unwrap();
        assert_eq!(cached.files_reused, 1);
        assert_eq!(cached.logical_file_bytes_reused, 131072);
        assert!(cached.decompressed_bytes < baseline.decompressed_bytes);
        assert_eq!(cached.stored_bytes_verified, baseline.stored_bytes_verified);
        options.strict_file_cache_entries = 1;
        let evicted = verify_source_payloads(&mut file, &archive, &options).unwrap();
        assert_eq!(evicted.files_reused, 0);
        options.strict_file_cache_entries = 128;
        let last = archive.files.len() - 1;
        archive.files[last].first_ref = archive.files[1].first_ref;
        assert!(
            verify_source_payloads(&mut file, &archive, &options).is_err(),
            "same digest/size but different refs must not hit"
        );
        archive.files[last] = archive.files[0].clone();
        archive.files[last].digest[0] ^= 1;
        assert!(
            verify_source_payloads(&mut file, &archive, &options).is_err(),
            "different claimed digest must still fail"
        );
    }

    /// Same cases as tests/conformance/build_zstd_cases.py (C++ reader):
    /// a block is one complete frame with its exact content size, no
    /// dictionary, window at most 1 MiB.
    #[test]
    fn zstd_block_edge_cases_match_the_cxx_reader() {
        use std::io::Write;
        let (mut file, _guard) =
            unique_temp(&std::env::temp_dir(), OsStr::new("nx-zstd-cases")).unwrap();
        let mut decoder = zstd::bulk::Decompressor::new().unwrap();
        decoder
            .set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))
            .unwrap();
        let options = WriterOptions::default();
        let mut decode = |frame: &[u8], logical_size: u32| {
            file.set_len(0).unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(frame).unwrap();
            let block = SourceBlock {
                logical_size,
                stored_size: frame.len() as u32,
                codec: 1,
                checks: Vec::new(),
                range: SourceRange {
                    archive: 0,
                    offset: 0,
                    size: frame.len() as u64,
                },
            };
            let (mut stored, mut logical) = (Vec::new(), Vec::new());
            decode_source_block(
                &mut file,
                &block,
                &mut stored,
                &mut logical,
                &mut decoder,
                &options,
            )
            .map(|_| logical)
        };
        let data = vec![b'A'; 4096];
        let valid = zstd::bulk::compress(&data, 3).unwrap();
        assert_eq!(decode(&valid, 4096).unwrap(), data);
        // Declared size differs from the block's logical size.
        assert!(decode(&valid, 4095).is_err());
        assert!(decode(&valid, 4097).is_err());
        assert!(
            decode(&valid[..valid.len() - 1], 4096).is_err(),
            "truncated"
        );
        assert!(decode(&valid.repeat(2), 4096).is_err(), "concatenated");
        assert!(
            decode(&[&valid[..], b"trailing"].concat(), 4096).is_err(),
            "trailing bytes"
        );
        let skippable = [&[0x50, 0x2a, 0x4d, 0x18, 3, 0, 0, 0][..], b"abc"].concat();
        // Note: a skippable frame alone, declared as a 0-byte block, passes here
        // (the C++ reader refuses it); see docs/nup-audit-2026-11.md.
        assert!(decode(&skippable, 3).is_err(), "skippable frame only");
        assert!(
            decode(&[&skippable[..], &valid[..]].concat(), 4096).is_err(),
            "skippable prefix"
        );
        // A frame without its content size is refused even if it decodes.
        let mut no_size = zstd::bulk::Compressor::new(3).unwrap();
        no_size
            .set_parameter(zstd::zstd_safe::CParameter::ContentSizeFlag(false))
            .unwrap();
        assert!(
            decode(&no_size.compress(&data).unwrap(), 4096).is_err(),
            "no content size"
        );
        // Dictionary frames need state the format does not carry.
        let dictionary = b"abcdefg0123456789".repeat(100);
        let dictionary_data = b"abcdefg0123456789".repeat(256);
        let mut with_dictionary = zstd::bulk::Compressor::with_dictionary(3, &dictionary).unwrap();
        let frame = with_dictionary.compress(&dictionary_data).unwrap();
        assert!(
            decode(&frame, dictionary_data.len() as u32).is_err(),
            "dictionary"
        );
        // Window > 1 MiB: the one-shot decoder does not apply WindowLogMax to
        // single-segment frames; block logical size <= 1 MiB is checked at parse.
        // The same decoder still works after every refusal.
        assert_eq!(decode(&valid, 4096).unwrap(), data);
    }
}

/// Progress units of `verify_source_payloads`: every block, every Media and
/// every File it reads.
fn verification_units(archive: &SourceArchive) -> u64 {
    let blocks: u64 = archive.blocks.iter().map(|b| b.logical_size as u64).sum();
    let media: u64 = archive.media.iter().map(|m| m.size).sum();
    let files: u64 = archive.files.iter().map(|f| f.size).sum();
    blocks + media + files
}

/// File bytes `write_archive` reads from these inputs (an alias is not read).
fn input_units(input: &ArchiveInput) -> u64 {
    fn tree(dir: &DirectoryInput) -> u64 {
        dir.entries
            .iter()
            .map(|e| match &e.node {
                NodeInput::Directory(child) => tree(child),
                NodeInput::File(f) => {
                    if matches!(f.share, Some(crate::model::FileShare::Alias(_))) {
                        0
                    } else {
                        f.size
                    }
                }
            })
            .sum()
    }
    input
        .applications
        .iter()
        .flat_map(|a| &a.snapshots)
        .map(|s| tree(&s.root))
        .sum()
}

fn verify_source_payloads(
    file: &mut File,
    archive: &SourceArchive,
    options: &WriterOptions,
) -> Result<VerificationStats, WriterError> {
    let mut decoder = zstd::bulk::Decompressor::new()?;
    decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
    let mut frame = Vec::new();
    let mut logical = Vec::new();
    let mut verified = 0u64;
    let mut stats = VerificationStats::default();
    options.progress.add_total(verification_units(archive));
    // Each block is decoded once: its CRC-32C is checked the first time a file
    // hashes it, and blocks no file uses are checked at the end.
    let mut checked = vec![false; archive.blocks.len()];
    let check_block = |block_id: usize,
                       logical: &[u8],
                       verified: &mut u64,
                       checked: &mut [bool]|
     -> Result<(), WriterError> {
        let block = &archive.blocks[block_id];
        options.progress.add_done(block.logical_size as u64);
        if !checks_match(block, logical) {
            return Err(bad("source block CRC-32C mismatch"));
        }
        *verified = verified
            .checked_add(block.range.size)
            .ok_or_else(|| limit_error("verified byte count overflow"))?;
        checked[block_id] = true;
        Ok(())
    };
    for source_media in &archive.media {
        cancelled(options)?;
        if hash_range(file, source_media.range.offset, source_media.size, options)?
            != source_media.digest
        {
            return Err(bad("source Media payload SHA-256 mismatch"));
        }
        options.progress.add_done(source_media.size);
        verified = verified
            .checked_add(source_media.size)
            .ok_or_else(|| limit_error("verified byte count overflow"))?;
    }
    // Cache only indexes into already budgeted tables: <= 512 bytes of entries,
    // independent of file size/reference count. Exact reference equality is
    // mandatory; a digest hit alone never substitutes for verification.
    let capacity = (options.strict_file_cache_entries as usize)
        .min((options.max_memory_bytes / 16384) as usize);
    let mut cache = std::collections::VecDeque::<u32>::with_capacity(capacity);
    for (index, file_record) in archive.files.iter().enumerate() {
        cancelled(options)?;
        options.progress.add_done(file_record.size);
        if cache
            .iter()
            .any(|&i| same_file_structure(archive, file_record, &archive.files[i as usize]))
        {
            stats.files_reused += 1;
            stats.logical_file_bytes_reused += file_record.size;
            continue;
        }
        let mut hash = Sha256::new();
        for ri in file_record.first_ref..file_record.first_ref + file_record.block_count {
            let block_id = archive.block_refs[ri as usize] as usize;
            let block = &archive.blocks[block_id];
            decode_source_block(file, block, &mut frame, &mut logical, &mut decoder, options)?;
            if !checked[block_id] {
                check_block(block_id, &logical, &mut verified, &mut checked)?;
            }
            hash.update(&logical);
            stats.block_decodes += 1;
            if block.codec == 1 {
                stats.decompressed_bytes += logical.len() as u64;
            }
        }
        if hash.finish() != file_record.digest {
            return Err(bad("source logical File SHA-256 mismatch"));
        }
        stats.files_verified += 1;
        stats.logical_file_bytes_verified += file_record.size;
        if capacity != 0 {
            if cache.len() == capacity {
                cache.pop_front();
            }
            cache.push_back(index as u32);
            stats.cache_peak_entries = stats.cache_peak_entries.max(cache.len() as u16);
        }
    }
    for (block_id, block) in archive.blocks.iter().enumerate() {
        if checked[block_id] {
            continue;
        }
        let size =
            decode_source_block(file, block, &mut frame, &mut logical, &mut decoder, options)?;
        check_block(block_id, &logical, &mut verified, &mut checked)?;
        stats.block_decodes += 1;
        if block.codec == 1 {
            stats.decompressed_bytes += size as u64;
        }
    }
    stats.stored_bytes_verified = verified;
    Ok(stats)
}

struct MergedSnapshot {
    source_archive: u8,
    key: (u8, u64),
    program_id: Option<u64>,
    data: FinalSnapshot,
}

fn clone_parsed_snapshot(source: &FinalSnapshot) -> FinalSnapshot {
    FinalSnapshot {
        program_index: source.program_index,
        snapshot_type: source.snapshot_type,
        version: source.version,
        root: source.root,
        logical_size: source.logical_size,
        display_version: source.display_version.clone(),
        content_id: source.content_id,
        addon: source.addon,
        build_id: source.build_id,
        identity: source.identity,
        dependencies: source
            .dependencies
            .iter()
            .map(|d| DependencyInput {
                kind: d.kind,
                content_id: d.content_id,
                target_snapshot_identity: d.target_snapshot_identity,
            })
            .collect(),
        metadata: None,
        meta_refs: source.meta_refs.clone(),
        npdm: source.npdm,
        required_firmware: source.required_firmware,
    }
}

#[derive(Clone)]
struct OutputMedia {
    source: SourceMedia,
    data_offset: u64,
}

fn ranges_equal(
    sources: &mut [File],
    a: SourceRange,
    b: SourceRange,
    options: &WriterOptions,
) -> Result<bool, WriterError> {
    if a.size != b.size {
        return Ok(false);
    }
    let mut left = [0u8; 64 * 1024];
    let mut right = [0u8; 64 * 1024];
    let mut remain = a.size;
    let (mut pa, mut pb) = (a.offset, b.offset);
    while remain != 0 {
        cancelled(options)?;
        let n = remain.min(left.len() as u64) as usize;
        read_exact_at(&mut sources[a.archive as usize], pa, &mut left[..n])?;
        read_exact_at(&mut sources[b.archive as usize], pb, &mut right[..n])?;
        if left[..n] != right[..n] {
            return Ok(false);
        }
        pa += n as u64;
        pb += n as u64;
        remain -= n as u64;
    }
    Ok(true)
}

fn source_block_bytes(
    files: &mut [File],
    block: &SourceBlock,
    options: &WriterOptions,
) -> Result<Vec<u8>, WriterError> {
    let mut decoder = zstd::bulk::Decompressor::new()?;
    decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
    let mut frame = Vec::new();
    let mut logical = Vec::new();
    decode_source_block(
        &mut files[block.range.archive as usize],
        block,
        &mut frame,
        &mut logical,
        &mut decoder,
        options,
    )?;
    Ok(logical)
}

// The remapper keeps source ranges, collision candidates, compression state,
// and accounting separate so each buffer and file has explicit ownership.
#[allow(clippy::too_many_arguments)]
fn map_block(
    archive_id: u8,
    old_id: u32,
    archives: &[SourceArchive],
    files: &mut [File],
    block_maps: &mut [Vec<Option<u32>>],
    blocks_out: &mut Vec<SourceBlock>,
    candidates: &mut HashMap<(u32, u64), Vec<u32>>,
    stats: &mut RepackStats,
    options: &WriterOptions,
    block_encoder: &mut Option<BlockEncoder>,
    encoded_spool: &mut File,
    scratch_bytes: u64,
    budget: &mut MemoryBudget,
) -> Result<u32, WriterError> {
    if let Some(id) = block_maps[archive_id as usize][old_id as usize] {
        return Ok(id);
    }
    let mut source = archives[archive_id as usize]
        .blocks
        .get(old_id as usize)
        .ok_or_else(|| bad("Manifest block reference is out of range"))?
        .clone();
    if archive_id == 1 {
        options.progress.add_done(source.logical_size as u64);
    }
    let key = (source.logical_size, checks_key(&source.checks));
    let possible = candidates.get(&key);
    if archive_id == 1
        && (possible.is_some() || options.compression_mode == CompressionMode::ZstandardAuto)
    {
        let logical = source_block_bytes(files, &source, options)?;
        if !checks_match(&source, &logical) {
            return Err(bad("staged block CRC-32C mismatch"));
        }
        if let Some(possible) = possible {
            for &candidate_id in possible {
                let candidate = &blocks_out[candidate_id as usize];
                let old_logical = source_block_bytes(files, candidate, options)?;
                if old_logical == logical {
                    block_maps[archive_id as usize][old_id as usize] = Some(candidate_id);
                    stats.added_blocks_reused += 1;
                    return Ok(candidate_id);
                }
            }
        }
        if options.compression_mode == CompressionMode::ZstandardAuto {
            stats.added_logical_bytes_considered_for_compression = stats
                .added_logical_bytes_considered_for_compression
                .checked_add(logical.len() as u64)
                .ok_or_else(|| limit_error("compression input byte count overflow"))?;
            if block_encoder.is_none() {
                *block_encoder = Some(BlockEncoder::new(options, budget)?);
            }
            let mut timings = WriterTimings::default();
            let encoder = block_encoder.as_mut().unwrap();
            let encoded = encoder.encode(&logical, options, &mut timings)?;
            stats.compression_time = stats
                .compression_time
                .checked_add(timings.compression)
                .ok_or_else(|| limit_error("compression time overflow"))?;
            if crate::writer::keep_compressed(encoded, logical.len(), options) {
                let offset = encoded_spool.seek(SeekFrom::End(0))?;
                encoded_spool.write_all(&encoder.frame[..encoded])?;
                source.codec = 1;
                source.stored_size = u32::try_from(encoded)
                    .map_err(|_| limit_error("compressed frame size exceeds u32"))?;
                source.range = SourceRange {
                    archive: 2,
                    offset,
                    size: encoded as u64,
                };
                stats.added_logical_bytes_compressed = stats
                    .added_logical_bytes_compressed
                    .checked_add(logical.len() as u64)
                    .ok_or_else(|| limit_error("compressed byte count overflow"))?;
            }
            check_temporary_size(
                scratch_bytes
                    .checked_add(encoded_spool.metadata()?.len())
                    .ok_or_else(|| limit_error("temporary disk usage overflow"))?,
                options,
            )?;
        }
    }
    let new_id = u32::try_from(blocks_out.len())
        .map_err(|_| limit_error("output block index exceeds u32"))?;
    blocks_out
        .try_reserve(1)
        .map_err(|_| limit_error("unable to reserve output block"))?;
    blocks_out.push(source.clone());
    block_maps[archive_id as usize][old_id as usize] = Some(new_id);
    candidates.entry(key).or_default().push(new_id);
    if archive_id == 0 {
        stats.retained_stored_bytes_copied = stats
            .retained_stored_bytes_copied
            .checked_add(source.stored_size as u64)
            .ok_or_else(|| limit_error("retained byte counter overflow"))?;
    } else {
        stats.added_stored_bytes_copied = stats
            .added_stored_bytes_copied
            .checked_add(source.stored_size as u64)
            .ok_or_else(|| limit_error("added byte counter overflow"))?;
    }
    Ok(new_id)
}

// Explicit state avoids an extra mutable writer-context abstraction in this
// depth-bounded traversal.
#[allow(clippy::too_many_arguments)]
fn append_snapshot_tree(
    archive_id: u8,
    root: u32,
    archive: &SourceArchive,
    files_out: &mut Vec<FileBuild>,
    dirs_out: &mut Vec<DirectoryBuild>,
    refs_out: &mut Vec<u32>,
    archives: &[SourceArchive],
    sources: &mut [File],
    block_maps: &mut [Vec<Option<u32>>],
    blocks_out: &mut Vec<SourceBlock>,
    candidates: &mut HashMap<(u32, u64), Vec<u32>>,
    stats: &mut RepackStats,
    options: &WriterOptions,
    depth: usize,
    block_encoder: &mut Option<BlockEncoder>,
    encoded_spool: &mut File,
    scratch_bytes: u64,
    budget: &mut MemoryBudget,
) -> Result<u32, WriterError> {
    if depth > MAX_DIRECTORY_DEPTH {
        return Err(bad("Manifest tree exceeds maximum directory depth"));
    }
    let dir = archive
        .directories
        .get(root as usize)
        .ok_or_else(|| bad("snapshot root directory is missing"))?;
    let output_index = u32::try_from(dirs_out.len())
        .map_err(|_| limit_error("output directory index exceeds u32"))?;
    dirs_out.push(DirectoryBuild {
        entries: Vec::new(),
    });
    for entry in dir {
        cancelled(options)?;
        let (kind, target) = if entry.kind == 1 {
            (
                1,
                append_snapshot_tree(
                    archive_id,
                    entry.target,
                    archive,
                    files_out,
                    dirs_out,
                    refs_out,
                    archives,
                    sources,
                    block_maps,
                    blocks_out,
                    candidates,
                    stats,
                    options,
                    depth + 1,
                    block_encoder,
                    encoded_spool,
                    scratch_bytes,
                    budget,
                )?,
            )
        } else {
            let (file_archive_id, file_archive, file_index) =
                if let Some(&index) = archive.inherited_files.get(&entry.target) {
                    (0, &archives[0], index)
                } else {
                    (archive_id, archive, entry.target)
                };
            let source_file = file_archive
                .files
                .get(file_index as usize)
                .ok_or_else(|| bad("Manifest File record is missing"))?;
            let first_ref = u32::try_from(refs_out.len())
                .map_err(|_| limit_error("output block-reference index exceeds u32"))?;
            for ri in source_file.first_ref..source_file.first_ref + source_file.block_count {
                let old_block = file_archive.block_refs[ri as usize];
                let mapped = map_block(
                    file_archive_id,
                    old_block,
                    archives,
                    sources,
                    block_maps,
                    blocks_out,
                    candidates,
                    stats,
                    options,
                    block_encoder,
                    encoded_spool,
                    scratch_bytes,
                    budget,
                )?;
                refs_out.push(mapped);
            }
            let file_index = u32::try_from(files_out.len())
                .map_err(|_| limit_error("output file index exceeds u32"))?;
            let mut copied = source_file.clone();
            copied.first_ref = first_ref;
            files_out.push(copied);
            (2, file_index)
        };
        dirs_out[output_index as usize].entries.push(FinalEntry {
            name: entry.name.clone(),
            kind,
            target,
        });
    }
    Ok(output_index)
}

struct CombinedCatalog {
    apps: Vec<ApplicationBuild>,
    programs: Vec<ProgramBuild>,
    program_sources: Vec<(u8, u32)>,
    snapshots: Vec<MergedSnapshot>,
    extensions: Vec<CatalogExtension>,
}

fn source_application(
    archives: &[SourceArchive],
    key: (u8, u64),
    archive_id: u8,
) -> Option<&SourceApplication> {
    archives
        .get(archive_id as usize)?
        .applications
        .iter()
        .find(|app| app.key == key)
}

fn same_media(
    sources: &mut [File],
    a: &SourceMedia,
    b: &SourceMedia,
    options: &WriterOptions,
) -> Result<bool, WriterError> {
    Ok(a.media_type == b.media_type
        && a.size == b.size
        && a.digest == b.digest
        && ranges_equal(sources, a.range, b.range, options)?)
}

fn combine_catalog(
    archives: &[SourceArchive],
    plan: &RepackPlan,
    stats: &mut RepackStats,
    options: &WriterOptions,
) -> Result<CombinedCatalog, WriterError> {
    if archives
        .iter()
        .any(|archive| !archive.optional_global_extensions.is_empty())
    {
        return Err(invalid(
            "cannot safely remap an archive containing an unknown optional Catalog extension",
        ));
    }
    let removals: HashSet<[u8; 32]> = plan.remove_snapshots.iter().copied().collect();
    if removals.len() != plan.remove_snapshots.len() || removals.contains(&[0; 32]) {
        return Err(invalid("duplicate or zero snapshot removal identity"));
    }
    let source_ids: HashSet<[u8; 32]> = archives[0]
        .snapshots
        .iter()
        .map(|s| s.snapshot.identity)
        .collect();
    if removals.iter().any(|id| !source_ids.contains(id)) {
        return Err(invalid(
            "removal identity is not present in the source archive",
        ));
    }
    let mut selected_requests = HashMap::new();
    for selection in &plan.metadata_sources {
        let key = (selection.platform as u8, selection.application_id);
        if selection.application_id == 0
            || selection.snapshot_identity == [0; 32]
            || selected_requests
                .insert(key, selection.snapshot_identity)
                .is_some()
        {
            return Err(invalid("duplicate or invalid metadata-source selection"));
        }
    }
    let mut snapshots = Vec::<MergedSnapshot>::new();
    let mut identities = HashSet::new();
    let mut found_removals = HashSet::new();
    for archive in archives {
        for source in &archive.snapshots {
            let identity = source.snapshot.identity;
            if archive.archive == 0 && removals.contains(&identity) {
                found_removals.insert(identity);
                continue;
            }
            if !identities.insert(identity) {
                return Err(invalid(
                    "source and added snapshots have duplicate canonical identities",
                ));
            }
            snapshots.push(MergedSnapshot {
                source_archive: source.archive,
                key: source.key,
                program_id: source.program_id,
                data: clone_parsed_snapshot(&source.snapshot),
            });
        }
    }
    if found_removals.len() != removals.len() {
        return Err(invalid("a requested source snapshot was not removed"));
    }
    if snapshots.is_empty() {
        return Err(invalid(
            "repack cannot produce an archive with no snapshots",
        ));
    }
    for snapshot in &snapshots {
        for dependency in &snapshot.data.dependencies {
            let target = snapshots
                .iter()
                .find(|item| item.data.identity == dependency.target_snapshot_identity)
                .ok_or_else(|| invalid("repack would leave a dangling snapshot dependency"))?;
            if target.data.identity == snapshot.data.identity {
                return Err(invalid("snapshot cannot depend on itself"));
            }
            if target.key != snapshot.key {
                return Err(invalid(
                    "snapshot dependency targets a different application",
                ));
            }
            // A Program first brought by an update has no base of its own: it
            // depends on the application's base (Bayonetta 3 v1.2, Program 1).
            let own_base = |program| {
                snapshots.iter().any(|item| {
                    item.key == snapshot.key
                        && item.data.snapshot_type == SnapshotType::Base as u8
                        && item.program_id == program
                })
            };
            if dependency.kind == DependencyKind::RequiredBase
                && (target.data.snapshot_type != SnapshotType::Base as u8
                    || (snapshot.program_id.is_some()
                        && target.program_id.is_some()
                        && snapshot.program_id != target.program_id
                        && own_base(snapshot.program_id)))
            {
                return Err(invalid(
                    "required-base dependency does not target this program's base snapshot",
                ));
            }
            if let Some(cid) = dependency.content_id {
                if target.data.content_id != cid {
                    return Err(invalid(
                        "dependency Content ID disagrees with its target snapshot",
                    ));
                }
            }
        }
    }
    stats.removed_snapshots = u32::try_from(found_removals.len())
        .map_err(|_| limit_error("removed snapshot count overflow"))?;
    stats.added_snapshots =
        u32::try_from(snapshots.iter().filter(|s| s.source_archive == 1).count())
            .map_err(|_| limit_error("added snapshot count overflow"))?;
    stats.retained_snapshots =
        u32::try_from(snapshots.iter().filter(|s| s.source_archive == 0).count())
            .map_err(|_| limit_error("retained snapshot count overflow"))?;

    let mut app_keys = BTreeMap::<(u8, u64), Vec<usize>>::new();
    for (i, s) in snapshots.iter().enumerate() {
        app_keys.entry(s.key).or_default().push(i);
    }
    let mut program_defs = BTreeMap::<((u8, u64), u64), (&SourceProgram, u8)>::new();
    for archive in archives {
        for program in &archive.programs {
            let has_surviving_snapshot = snapshots
                .iter()
                .any(|s| s.key == program.key && s.program_id == Some(program.program_id));
            if !has_surviving_snapshot {
                continue;
            }
            let k = (program.key, program.program_id);
            if let Some((previous, _)) = program_defs.get(&k) {
                if previous.kind != program.kind
                    || previous.content_id != program.content_id
                    || previous.has_content_id != program.has_content_id
                {
                    return Err(invalid(
                        "program rows disagree across source and added snapshots",
                    ));
                }
            } else {
                program_defs.insert(k, (program, archive.archive));
            }
        }
    }
    let mut out_apps = Vec::new();
    let mut out_programs = Vec::new();
    let mut program_sources = Vec::new();
    let mut snapshots: Vec<Option<MergedSnapshot>> = snapshots.into_iter().map(Some).collect();
    let mut combined_snapshots = Vec::<MergedSnapshot>::new();
    let mut selected_identities = HashMap::new();
    let mut key_to_app_index = HashMap::new();
    for (app_index, key) in app_keys.keys().copied().enumerate() {
        cancelled(options)?;
        let app_index =
            u32::try_from(app_index).map_err(|_| limit_error("application index exceeds u32"))?;
        key_to_app_index.insert(key, app_index);
        let app_program_ids: Vec<u64> = program_defs
            .keys()
            .filter(|(k, _)| *k == key)
            .map(|(_, id)| *id)
            .collect();
        let first_program = if app_program_ids.is_empty() {
            NO_INDEX
        } else {
            u32::try_from(out_programs.len())
                .map_err(|_| limit_error("program index exceeds u32"))?
        };
        let mut program_indices = HashMap::new();
        for program_id in &app_program_ids {
            let (definition, source_id) = program_defs[&(key, *program_id)];
            let index = u32::try_from(out_programs.len())
                .map_err(|_| limit_error("program index exceeds u32"))?;
            program_indices.insert(*program_id, index);
            out_programs.push(ProgramBuild {
                app_index,
                program_id: *program_id,
                kind: definition.kind,
                content_id: definition.content_id,
                has_content_id: definition.has_content_id,
                first_snapshot: 0,
                snapshot_count: 0,
                npdm: NO_INDEX,
                npdm_input: None,
            });
            program_sources.push((source_id, definition.npdm));
        }
        let first_snapshot = u32::try_from(combined_snapshots.len())
            .map_err(|_| limit_error("snapshot index exceeds u32"))?;
        let mut app_snapshot_rows = app_keys[&key].clone();
        app_snapshot_rows.sort_by_key(|&i| {
            let s = snapshots[i].as_ref().unwrap();
            let program_index = s
                .program_id
                .and_then(|id| program_indices.get(&id).copied())
                .unwrap_or(NO_INDEX);
            (
                program_index,
                s.data.snapshot_type,
                s.data.version,
                s.data.content_id,
                s.data.build_id,
            )
        });
        for i in app_snapshot_rows {
            let mut s = snapshots[i].take().unwrap();
            s.data.program_index = match s.program_id {
                Some(id) => *program_indices.get(&id).ok_or_else(|| {
                    invalid("snapshot names a program absent from the resulting application")
                })?,
                None => NO_INDEX,
            };
            combined_snapshots.push(s);
        }
        let snapshot_count = u32::try_from(combined_snapshots.len())
            .map_err(|_| limit_error("snapshot count exceeds u32"))?
            - first_snapshot;
        for pi in first_program..if first_program == NO_INDEX {
            first_program
        } else {
            first_program + app_program_ids.len() as u32
        } {
            let program_id = out_programs[pi as usize].program_id;
            let first = combined_snapshots
                .iter()
                .position(|s| s.key == key && s.program_id == Some(program_id))
                .ok_or_else(|| invalid("resulting Program has no snapshots"))?
                as u32;
            let count = combined_snapshots
                .iter()
                .filter(|s| s.key == key && s.program_id == Some(program_id))
                .count() as u32;
            out_programs[pi as usize].first_snapshot = first;
            out_programs[pi as usize].snapshot_count = count;
        }
        let explicit = selected_requests.remove(&key);
        let selection = if let Some(identity) = explicit {
            Some(identity)
        } else {
            combined_snapshots
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.key == key
                        && s.data.snapshot_type != SnapshotType::Dlc as u8
                        && s.data.meta_refs.is_some()
                })
                .max_by_key(|(i, s)| (s.data.version, *i))
                .map(|(_, s)| s.data.identity)
        };
        if let Some(identity) = selection {
            let selected = combined_snapshots
                .iter()
                .find(|s| s.key == key && s.data.identity == identity)
                .ok_or_else(|| {
                    invalid(
                        "metadata source does not name a resulting snapshot of this application",
                    )
                })?;
            if selected.data.snapshot_type == SnapshotType::Dlc as u8
                || selected.data.meta_refs.is_none()
            {
                return Err(invalid(
                    "metadata source must name a metadata-bearing non-DLC snapshot",
                ));
            }
        } else if explicit.is_some() {
            unreachable!();
        }
        let selected_index = selection
            .and_then(|id| {
                combined_snapshots
                    .iter()
                    .position(|s| s.key == key && s.data.identity == id)
            })
            .map(|i| i as u32)
            .unwrap_or(NO_INDEX);
        let current_opaque_first = 0;
        let current_opaque_count = 0;
        out_apps.push(ApplicationBuild {
            application_id: key.1,
            platform: key.0,
            first_program,
            program_count: app_program_ids.len() as u32,
            first_snapshot,
            snapshot_count,
            selected_snapshot: selected_index,
            current_opaque_first,
            current_opaque_count,
        });
        selected_identities.insert(key, selection);
    }
    if !selected_requests.is_empty() {
        return Err(invalid(
            "metadata source selection names an application absent from the resulting archive",
        ));
    }
    if out_apps.len() > 4096 || out_programs.len() > 16384 || combined_snapshots.len() > 65536 {
        return Err(limit_error("repacked Catalog count exceeds NX 0.1 limits"));
    }

    let mut extensions = Vec::<CatalogExtension>::new();
    for (app_key, &app_index) in &key_to_app_index {
        let selection = selected_identities[app_key];
        if let Some(identity) = selection {
            let source_id = combined_snapshots
                .iter()
                .find(|s| s.key == *app_key && s.data.identity == identity)
                .map(|s| s.source_archive)
                .unwrap();
            if let Some(source_app) = source_application(archives, *app_key, source_id) {
                if source_app.selected_identity == Some(identity) {
                    for (type_id, flags, owner_kind, bytes) in &source_app.optional_extensions {
                        extensions.push(CatalogExtension {
                            type_id: *type_id,
                            flags: *flags,
                            owner_kind: *owner_kind,
                            owner: app_index,
                            bytes: bytes.clone(),
                        });
                    }
                }
            }
        }
    }
    if extensions.len() > 4096 {
        return Err(limit_error(
            "repacked Catalog extension count exceeds NX 0.1 limit",
        ));
    }
    Ok(CombinedCatalog {
        apps: out_apps,
        programs: out_programs,
        program_sources,
        snapshots: combined_snapshots,
        extensions,
    })
}

#[allow(clippy::too_many_arguments)]
fn add_media_reference(
    archive_id: u8,
    old_id: u32,
    archives: &[SourceArchive],
    sources: &mut [File],
    media_maps: &mut [Vec<Option<u32>>],
    output: &mut Vec<OutputMedia>,
    icon_candidates: &mut HashMap<(u16, u64, [u8; 32]), Vec<u32>>,
    options: &WriterOptions,
) -> Result<u32, WriterError> {
    let source = archives
        .get(archive_id as usize)
        .and_then(|a| a.media.get(old_id as usize))
        .ok_or_else(|| bad("Catalog references a missing Media record"))?
        .clone();
    if let Some(mapped) = media_maps[archive_id as usize][old_id as usize] {
        if source.media_type <= 2 {
            return Ok(mapped);
        }
        return Err(bad(
            "non-icon Media record has more than one owning reference",
        ));
    }
    if source.media_type <= 2 {
        let key = (source.media_type, source.size, source.digest);
        if let Some(candidates) = icon_candidates.get(&key) {
            for &candidate in candidates {
                if same_media(
                    sources,
                    &source,
                    &output[candidate as usize].source,
                    options,
                )? {
                    media_maps[archive_id as usize][old_id as usize] = Some(candidate);
                    return Ok(candidate);
                }
            }
        }
        icon_candidates.entry(key).or_default();
    }
    if output.len() >= 65535 {
        return Err(limit_error(
            "repacked Media record count exceeds NX 0.1 limit",
        ));
    }
    let id = u32::try_from(output.len()).map_err(|_| limit_error("Media index exceeds u32"))?;
    output.push(OutputMedia {
        source: source.clone(),
        data_offset: 0,
    });
    media_maps[archive_id as usize][old_id as usize] = Some(id);
    if source.media_type <= 2 {
        icon_candidates
            .entry((source.media_type, source.size, source.digest))
            .or_default()
            .push(id);
    }
    Ok(id)
}

fn remap_media_references(
    combined: &mut CombinedCatalog,
    archives: &[SourceArchive],
    sources: &mut [File],
    options: &WriterOptions,
) -> Result<Vec<OutputMedia>, WriterError> {
    let mut maps: Vec<Vec<Option<u32>>> =
        archives.iter().map(|a| vec![None; a.media.len()]).collect();
    let mut output = Vec::<OutputMedia>::new();
    let mut icon_candidates = HashMap::new();
    // Catalog opaque ranges must remain contiguous, so emit selected-source
    // opaque items first and in application order.
    for app_index in 0..combined.apps.len() {
        let selected_index = combined.apps[app_index].selected_snapshot;
        if selected_index == NO_INDEX {
            continue;
        }
        let selected = &combined.snapshots[selected_index as usize];
        let Some(meta) = selected.data.meta_refs.as_ref() else {
            continue;
        };
        if meta.opaque.is_empty() {
            continue;
        }
        let start =
            u32::try_from(output.len()).map_err(|_| limit_error("Media index exceeds u32"))?;
        for &old in &meta.opaque {
            let expected =
                u32::try_from(output.len()).map_err(|_| limit_error("Media index exceeds u32"))?;
            let mapped = add_media_reference(
                selected.source_archive,
                old,
                archives,
                sources,
                &mut maps,
                &mut output,
                &mut icon_candidates,
                options,
            )?;
            if mapped != expected {
                return Err(bad("selected opaque media range is not contiguous"));
            }
        }
        combined.apps[app_index].current_opaque_first = start;
        combined.apps[app_index].current_opaque_count = meta.opaque.len() as u32;
    }
    // Collect all remaining references in canonical table order.
    for (program_index, program) in combined.programs.iter_mut().enumerate() {
        let (archive_id, old) = combined.program_sources[program_index];
        if old != NO_INDEX {
            program.npdm = add_media_reference(
                archive_id,
                old,
                archives,
                sources,
                &mut maps,
                &mut output,
                &mut icon_candidates,
                options,
            )?;
        }
    }
    for snapshot in &mut combined.snapshots {
        let source_archive = snapshot.source_archive;
        if snapshot.data.npdm != NO_INDEX {
            snapshot.data.npdm = add_media_reference(
                source_archive,
                snapshot.data.npdm,
                archives,
                sources,
                &mut maps,
                &mut output,
                &mut icon_candidates,
                options,
            )?;
        }
        if let Some(meta) = snapshot.data.meta_refs.as_mut() {
            if meta.icon != NO_INDEX {
                meta.icon = add_media_reference(
                    source_archive,
                    meta.icon,
                    archives,
                    sources,
                    &mut maps,
                    &mut output,
                    &mut icon_candidates,
                    options,
                )?;
            }
            if meta.nacp != NO_INDEX {
                meta.nacp = add_media_reference(
                    source_archive,
                    meta.nacp,
                    archives,
                    sources,
                    &mut maps,
                    &mut output,
                    &mut icon_candidates,
                    options,
                )?;
            }
            for local in &mut meta.localized {
                if local.icon != NO_INDEX {
                    local.icon = add_media_reference(
                        source_archive,
                        local.icon,
                        archives,
                        sources,
                        &mut maps,
                        &mut output,
                        &mut icon_candidates,
                        options,
                    )?;
                }
            }
            for old in &mut meta.opaque {
                *old = match maps[source_archive as usize][*old as usize] {
                    Some(mapped) => mapped,
                    None => add_media_reference(
                        source_archive,
                        *old,
                        archives,
                        sources,
                        &mut maps,
                        &mut output,
                        &mut icon_candidates,
                        options,
                    )?,
                };
            }
        }
    }
    Ok(output)
}

fn layout_media(media: &mut [OutputMedia]) -> Result<(Vec<u8>, u64), WriterError> {
    if media.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let payload_start = align_up(
        32u64
            .checked_add(media.len() as u64 * MEDIA_RECORD_SIZE as u64)
            .ok_or_else(|| limit_error("Media table size overflow"))?,
        16,
    )?;
    let mut cursor = payload_start;
    for item in media.iter_mut() {
        cursor = align_up(cursor, 16)?;
        item.data_offset = cursor;
        cursor = cursor
            .checked_add(item.source.size)
            .ok_or_else(|| limit_error("Media region size overflow"))?;
    }
    if cursor > MAX_MEDIA_REGION {
        return Err(limit_error("repacked Media region exceeds 4 GiB"));
    }
    let header_len = 32 + media.len() * MEDIA_RECORD_SIZE;
    let mut bytes = vec![0u8; header_len];
    bytes[..4].copy_from_slice(b"NXMD");
    write_u16_at(&mut bytes, 4, 1);
    write_u16_at(&mut bytes, 6, 32);
    write_u64_at(&mut bytes, 8, cursor);
    write_u32_at(&mut bytes, 16, media.len() as u32);
    write_u16_at(&mut bytes, 20, MEDIA_RECORD_SIZE as u16);
    write_u32_at(&mut bytes, 24, 32);
    write_u32_at(&mut bytes, 28, payload_start as u32);
    for (i, item) in media.iter().enumerate() {
        let at = 32 + i * MEDIA_RECORD_SIZE;
        write_u16_at(&mut bytes, at, item.source.media_type);
        write_u64_at(&mut bytes, at + 4, item.source.size);
        write_u64_at(&mut bytes, at + 12, item.source.size);
        write_u64_at(&mut bytes, at + 20, item.data_offset);
        bytes[at + 28..at + 60].copy_from_slice(&item.source.digest);
    }
    Ok((bytes, cursor))
}

fn write_zeros<W: Write>(
    out: &mut W,
    position: &mut u64,
    target: u64,
    options: &WriterOptions,
) -> Result<(), WriterError> {
    if target < *position {
        return Err(bad("output section position moved backwards"));
    }
    let zeros = [0u8; 4096];
    while *position < target {
        cancelled(options)?;
        let n = (target - *position).min(zeros.len() as u64) as usize;
        out.write_all(&zeros[..n])?;
        *position += n as u64;
    }
    Ok(())
}

fn write_section<W: Write>(
    out: &mut W,
    position: &mut u64,
    bytes: &[u8],
    options: &WriterOptions,
) -> Result<(), WriterError> {
    cancelled(options)?;
    let end = position
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| limit_error("output section position overflow"))?;
    if end > options.max_archive_size {
        return Err(limit_error(
            "repacked archive exceeds configured size limit",
        ));
    }
    out.write_all(bytes)?;
    *position = end;
    Ok(())
}

fn write_range<W: Write>(
    out: &mut W,
    position: &mut u64,
    sources: &mut [File],
    range: SourceRange,
    options: &WriterOptions,
) -> Result<(), WriterError> {
    let source = sources
        .get_mut(range.archive as usize)
        .ok_or_else(|| bad("payload source handle is missing"))?;
    source.seek(SeekFrom::Start(range.offset))?;
    let mut remain = range.size;
    let mut buffer = [0u8; 64 * 1024];
    while remain != 0 {
        cancelled(options)?;
        let n = remain.min(buffer.len() as u64) as usize;
        source.read_exact(&mut buffer[..n])?;
        write_section(out, position, &buffer[..n], options)?;
        remain -= n as u64;
    }
    Ok(())
}

/// Predicted share of the file an append would leave unused: everything
/// before the append point except the retained blocks, against the file size
/// after the append (new blocks only; the small new tail is ignored).
fn append_would_waste(blocks: &[SourceBlock], append_start: u64, options: &WriterOptions) -> bool {
    let percent = u128::from(options.auto_compact_free_percent);
    if percent == 0 {
        return false;
    }
    let (mut retained, mut added) = (0u128, 0u128);
    for block in blocks {
        if block.range.archive == 0 {
            retained += u128::from(block.stored_size);
        } else {
            added += u128::from(block.stored_size) + 15;
        }
    }
    let start = u128::from(append_start);
    let free = start.saturating_sub(u128::from(DATA_START) + retained);
    free * 100 >= (start + added) * percent
}

#[derive(Clone, Copy)]
enum OutputMode {
    /// Fresh compact generation-0 archive in a temporary file.
    Compact,
    /// Append new blocks and a new tail to the source file itself; the
    /// caller commits `generation` into the free header slot.
    Append {
        start: u64,
        generation: u64,
        slot: usize,
    },
}

struct RepackWrite {
    archive_size: u64,
    bytes_written: u64,
    new_block_indexes: Vec<usize>,
    new_media_indexes: Vec<usize>,
    header: [u8; 256],
    tail_start: u64,
    free_bytes: u64,
}

#[allow(clippy::too_many_arguments)]
fn write_repacked_archive(
    output: &mut File,
    sources: &mut [File],
    catalog: &[u8],
    manifest: &[u8],
    media: &mut [OutputMedia],
    blocks: &mut [SourceBlock],
    options: &WriterOptions,
    temporary_base: u64,
    mode: OutputMode,
    decompress_blocks: bool,
) -> Result<RepackWrite, WriterError> {
    let (media_header, media_size) = layout_media(media)?;
    let (start, generation) = match mode {
        OutputMode::Compact => (0, 0),
        OutputMode::Append {
            start, generation, ..
        } => (start, generation),
    };
    let append = matches!(mode, OutputMode::Append { .. });
    let new_bytes: u64 = blocks
        .iter()
        .filter(|b| !append || b.range.archive != 0)
        .map(|b| {
            if decompress_blocks && b.codec == 1 {
                b.logical_size as u64 + 15
            } else {
                b.stored_size as u64 + 15
            }
        })
        .sum::<u64>()
        + media_size
        + catalog.len() as u64
        + manifest.len() as u64
        + blocks.len() as u64 * (BLOCK_SIZE as u64 + 64 * 4)
        + DATA_START;
    check_temporary_size(
        temporary_base
            .checked_add(new_bytes)
            .ok_or_else(|| limit_error("temporary disk usage overflow"))?,
        options,
    )?;
    output.seek(SeekFrom::Start(start))?;
    let mut out = BufWriter::with_capacity(1024 * 1024, &mut *output);
    let mut position = start;
    if !append {
        write_section(
            &mut out,
            &mut position,
            &[0u8; DATA_START as usize],
            options,
        )?;
    }
    let mut new_block_indexes = Vec::new();
    let mut previous_end = DATA_START;
    let mut stored_total = 0u64;
    let mut checks = Vec::<u32>::new();
    let mut first_checks = Vec::with_capacity(blocks.len());
    let mut decoder = None;
    let (mut frame, mut logical) = (Vec::new(), Vec::new());
    for (i, block) in blocks.iter_mut().enumerate() {
        if block.range.size != block.stored_size as u64 {
            return Err(bad(
                "stored payload range does not match output block descriptor",
            ));
        }
        let offset = if append && block.range.archive == 0 {
            // Retained blocks stay where they are.
            block.range.offset
        } else {
            let aligned = align_up(position, 16)?;
            write_zeros(&mut out, &mut position, aligned, options)?;
            new_block_indexes.push(i);
            let at = position;
            if decompress_blocks && block.codec == 1 {
                if decoder.is_none() {
                    let mut d = zstd::bulk::Decompressor::new()?;
                    d.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
                    decoder = Some(d);
                }
                decode_source_block(
                    &mut sources[block.range.archive as usize],
                    block,
                    &mut frame,
                    &mut logical,
                    decoder.as_mut().unwrap(),
                    options,
                )?;
                if !checks_match(block, &logical) {
                    return Err(bad("source block CRC-32C mismatch during RAW rewrite"));
                }
                block.codec = 0;
                block.stored_size = block.logical_size;
                write_section(&mut out, &mut position, &logical, options)?;
            } else {
                write_range(&mut out, &mut position, sources, block.range, options)?;
            }
            at
        };
        if offset < align_up(previous_end, 16)? || offset % 16 != 0 {
            return Err(bad("in-place edit would reorder or overlap Data blocks"));
        }
        block.range = SourceRange {
            archive: 0,
            offset,
            size: block.stored_size as u64,
        };
        previous_end = offset + block.stored_size as u64;
        stored_total += block.stored_size as u64;
        first_checks
            .push(u32::try_from(checks.len()).map_err(|_| limit_error("check count exceeds u32"))?);
        checks
            .try_reserve(block.checks.len())
            .map_err(|_| limit_error("unable to reserve integrity table"))?;
        checks.extend_from_slice(&block.checks);
    }
    let (data_offset, data_size) = if blocks.is_empty() {
        (0, 0)
    } else {
        (DATA_START, previous_end - DATA_START)
    };
    let tail_start = align_up(position.max(previous_end), 16)?;
    write_zeros(&mut out, &mut position, tail_start, options)?;
    let mut media_offset = 0u64;
    if !media.is_empty() {
        media_offset = position;
        write_section(&mut out, &mut position, &media_header, options)?;
        let payload_start = align_up((32 + media.len() * MEDIA_RECORD_SIZE) as u64, 16)?;
        write_zeros(
            &mut out,
            &mut position,
            media_offset + payload_start,
            options,
        )?;
        for item in media.iter() {
            let target = media_offset
                .checked_add(item.data_offset)
                .ok_or_else(|| limit_error("Media offset overflow"))?;
            write_zeros(&mut out, &mut position, target, options)?;
            write_range(&mut out, &mut position, sources, item.source.range, options)?;
        }
        if position != media_offset + media_size {
            return Err(bad("repacked Media length differs from its plan"));
        }
    }
    let catalog_offset = align_up(position, 16)?;
    write_zeros(&mut out, &mut position, catalog_offset, options)?;
    write_section(&mut out, &mut position, catalog, options)?;
    let manifest_offset = align_up(position, 16)?;
    write_zeros(&mut out, &mut position, manifest_offset, options)?;
    write_section(&mut out, &mut position, manifest, options)?;
    let mut index_offset = 0u64;
    let mut integrity_offset = 0u64;
    let mut index_hash = Sha256::new();
    if !blocks.is_empty() {
        index_offset = align_up(position, 16)?;
        write_zeros(&mut out, &mut position, index_offset, options)?;
        for (block, first_check) in blocks.iter().zip(&first_checks) {
            let mut record = [0u8; 24];
            record[..8].copy_from_slice(&block.range.offset.to_le_bytes());
            record[8..12].copy_from_slice(&block.stored_size.to_le_bytes());
            record[12..16].copy_from_slice(&block.logical_size.to_le_bytes());
            record[16] = block.codec;
            record[20..24].copy_from_slice(&first_check.to_le_bytes());
            index_hash.update(&record);
            write_section(&mut out, &mut position, &record, options)?;
        }
        integrity_offset = align_up(position, 16)?;
        write_zeros(&mut out, &mut position, integrity_offset, options)?;
        let mut chunk = Vec::with_capacity(64 * 1024);
        for group in checks.chunks(16 * 1024) {
            chunk.clear();
            for check in group {
                chunk.extend_from_slice(&check.to_le_bytes());
            }
            index_hash.update(&chunk);
            write_section(&mut out, &mut position, &chunk, options)?;
        }
    }
    let archive_size = position;
    if archive_size > MAX_ARCHIVE_SIZE || archive_size > options.max_archive_size {
        return Err(limit_error("repacked archive exceeds supported size"));
    }
    let extension_count = u32::from_le_bytes(catalog[40..44].try_into().unwrap());
    let extension_offset = u64::from_le_bytes(catalog[88..96].try_into().unwrap());
    let has_resources = (0..extension_count).any(|i| {
        let at = extension_offset as usize + i as usize * 32;
        u32::from_le_bytes(catalog[at..at + 4].try_into().unwrap()) == SOURCE_RESOURCES
    });
    let header = encode_header(&HeaderFields {
        source_resources: has_resources,
        archive_size,
        catalog_offset,
        catalog_size: catalog.len() as u64,
        media_offset,
        media_size,
        data_offset,
        data_size,
        manifest_offset,
        manifest_size: manifest.len() as u64,
        block_index_offset: index_offset,
        block_count: blocks.len() as u64,
        integrity_offset,
        integrity_size: checks.len() as u64 * 4,
        catalog_sha256: sha256(catalog),
        manifest_sha256: sha256(manifest),
        index_sha256: index_hash.finish(),
        generation,
    });
    out.flush()?;
    drop(out);
    if !append {
        for slot in HEADER_SLOTS {
            output.seek(SeekFrom::Start(slot))?;
            output.write_all(&header)?;
        }
    }
    output.flush()?;
    output.sync_all()?;
    let new_media_indexes = media
        .iter()
        .enumerate()
        .filter_map(|(i, m)| (m.source.range.archive == 1).then_some(i))
        .collect();
    Ok(RepackWrite {
        archive_size,
        bytes_written: archive_size - start,
        new_block_indexes,
        new_media_indexes,
        header,
        tail_start,
        free_bytes: tail_start.saturating_sub(DATA_START + stored_total),
    })
}

/// Make an appended generation visible: write its header into the slot the
/// current generation does not use, then synchronize.
fn commit_generation(file: &mut File, header: &[u8; 256], slot: usize) -> std::io::Result<()> {
    let mut page = [0u8; HEADER_PAGE_SIZE];
    page[..256].copy_from_slice(header);
    file.seek(SeekFrom::Start(HEADER_SLOTS[slot]))?;
    file.write_all(&page)?;
    file.sync_all()
}

/// Retire the older generations of a staged archive that no reader can have
/// open yet: copy the selected header page into the other slot. Readers then
/// see one generation and skip the older-generation checks at open. Bytes of
/// retired tails stay as free space (SPEC §1, §8); nothing they hold is read.
pub(crate) fn seal_staged_generations(
    path: &Path,
    options: &WriterOptions,
) -> Result<(), WriterError> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let header = parse_header(&mut file, options)?;
    let mut pages = [0u8; DATA_START as usize];
    read_exact_at(&mut file, 0, &mut pages)?;
    let (first, second) = pages.split_at(HEADER_PAGE_SIZE);
    let (active, other) = if header.active_slot == 0 {
        (first, second)
    } else {
        (second, first)
    };
    if active == other {
        return Ok(());
    }
    let selected: [u8; 256] = active[..256].try_into().unwrap();
    commit_generation(&mut file, &selected, 1 - header.active_slot)?;
    Ok(())
}

fn collect_tree_blocks(
    archive: &SourceArchive,
    dir_id: u32,
    found: &mut HashSet<u32>,
    depth: usize,
) -> Result<(), WriterError> {
    if depth > MAX_DIRECTORY_DEPTH {
        return Err(bad("tree exceeds maximum directory depth"));
    }
    let directory = archive
        .directories
        .get(dir_id as usize)
        .ok_or_else(|| bad("snapshot root directory is missing"))?;
    for entry in directory {
        if entry.kind == 1 {
            collect_tree_blocks(archive, entry.target, found, depth + 1)?;
        } else {
            let file = archive
                .files
                .get(entry.target as usize)
                .ok_or_else(|| bad("tree references missing File record"))?;
            for ri in file.first_ref..file.first_ref + file.block_count {
                found.insert(archive.block_refs[ri as usize]);
            }
        }
    }
    Ok(())
}

fn verify_new_output_payloads(
    file: &mut File,
    archive: &SourceArchive,
    block_indexes: &[usize],
    media_indexes: &[usize],
    options: &WriterOptions,
) -> Result<(), WriterError> {
    let mut decoder = zstd::bulk::Decompressor::new()?;
    decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
    let mut frame = Vec::new();
    let mut logical = Vec::new();
    for &index in block_indexes {
        let block = archive
            .blocks
            .get(index)
            .ok_or_else(|| bad("new output block index is out of range"))?;
        decode_source_block(file, block, &mut frame, &mut logical, &mut decoder, options)?;
        if !checks_match(block, &logical) {
            return Err(bad("new output block CRC-32C mismatch"));
        }
    }
    for &index in media_indexes {
        let item = archive
            .media
            .get(index)
            .ok_or_else(|| bad("new output Media index is out of range"))?;
        if hash_range(file, item.range.offset, item.size, options)? != item.digest {
            return Err(bad("new output Media SHA-256 mismatch"));
        }
    }
    Ok(())
}

/// Read and validate a source archive's authenticated metadata without
/// loading block payloads. STRICT additionally verifies all source payloads.
pub fn inspect_archive(
    path: impl AsRef<Path>,
    options: &WriterOptions,
    policy: IntegrityPolicy,
) -> Result<ArchiveSummary, WriterError> {
    inspect_archive_impl(path.as_ref(), options, policy, false).map(|(summary, _, _)| summary)
}

pub(crate) fn inspect_archive_with_space(
    path: &Path,
    options: &WriterOptions,
) -> Result<(ArchiveSummary, u64, Option<u64>), WriterError> {
    inspect_archive_impl(path, options, IntegrityPolicy::Fast, true)
}

fn inspect_archive_impl(
    path: &Path,
    options: &WriterOptions,
    policy: IntegrityPolicy,
    measure_space: bool,
) -> Result<(ArchiveSummary, u64, Option<u64>), WriterError> {
    validate_options(options)?;
    let mut file = File::open(path)?;
    let mut budget = MemoryBudget {
        current: 0,
        peak: 0,
        limit: options.max_memory_bytes,
    };
    let archive = parse_archive(&mut file, 0, options, &mut budget)?;
    if policy == IntegrityPolicy::Strict {
        verify_source_payloads(&mut file, &archive, options)?;
    }
    let mut applications = Vec::new();
    for app in &archive.applications {
        let platform = match app.key.0 {
            0 => Platform::Unknown,
            1 => Platform::Switch1,
            2 => Platform::Switch2,
            _ => return Err(bad("unsupported application platform")),
        };
        let programs = archive
            .programs
            .iter()
            .filter(|program| program.key == app.key)
            .map(|program| ProgramSummary {
                program_id: program.program_id,
                kind: match program.kind {
                    0 => ProgramKind::Primary,
                    1 => ProgramKind::Auxiliary,
                    _ => ProgramKind::Unknown,
                },
                content_id: program.has_content_id.then_some(program.content_id),
            })
            .collect();
        let snapshots = archive
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.key == app.key)
            .map(|snapshot| SnapshotSummary {
                dependencies: snapshot.snapshot.dependencies.clone(),
                identity: snapshot.snapshot.identity,
                program_id: snapshot.program_id,
                snapshot_type: match snapshot.snapshot.snapshot_type {
                    0 => SnapshotType::Base,
                    1 => SnapshotType::Update,
                    2 => SnapshotType::Dlc,
                    3 => SnapshotType::StandaloneUpdate,
                    _ => SnapshotType::Unknown,
                },
                numeric_version: snapshot.snapshot.version,
                content_id: (!snapshot.snapshot.addon && snapshot.snapshot.content_id != [0; 16])
                    .then_some(snapshot.snapshot.content_id),
                addon_title_id: snapshot.snapshot.addon.then(|| {
                    u64::from_le_bytes(snapshot.snapshot.content_id[..8].try_into().unwrap())
                }),
                build_id: (snapshot.snapshot.build_id != [0; 32])
                    .then_some(snapshot.snapshot.build_id),
                has_display_metadata: snapshot.snapshot.meta_refs.is_some(),
                nacp_digest: snapshot
                    .snapshot
                    .meta_refs
                    .as_ref()
                    .and_then(|m| archive.media.get(m.nacp as usize).map(|media| media.digest)),
            })
            .collect();
        let mut source_files = Vec::new();
        for &(kind, root) in &app.source_resources {
            for entry in &archive.directories[root as usize] {
                let file = &archive.files[entry.target as usize];
                budget.charge(
                    entry.name.len() as u64 + std::mem::size_of::<SourceFileSummary>() as u64,
                    "source file summary",
                )?;
                source_files.push(SourceFileSummary {
                    kind,
                    path: entry.name.clone(),
                    size: file.size,
                    digest: file.digest,
                });
            }
        }
        applications.push(ApplicationSummary {
            platform,
            application_id: app.key.1,
            metadata_snapshot_identity: app.selected_identity,
            programs,
            snapshots,
            recipes: app.recipes.clone(),
            source_files,
        });
    }
    let mut reclaimable = 0;
    let mut allocated = None;
    if measure_space {
        let mut packed_end = DATA_START;
        for block in &archive.blocks {
            packed_end = align_up(packed_end, 16)? + u64::from(block.stored_size);
        }
        let gaps = archive.tail_start.saturating_sub(align_up(packed_end, 16)?);
        let residue = file.metadata()?.len().saturating_sub(archive.file_size);
        reclaimable = gaps + residue;
        allocated = reclaim::allocated_bytes(&file);
    }
    Ok((ArchiveSummary { applications }, reclaimable, allocated))
}

/// Display information selected for one snapshot. Native control bytes remain
/// separate from the display title and optional user-provided artwork.
#[derive(Default)]
pub struct SnapshotDisplay {
    pub title: Option<String>,
    pub icon: Option<Vec<u8>>,
    pub nacp: Option<Vec<u8>>,
}

/// Read bounded display media, verifying each payload against its Media digest.
pub fn read_snapshot_display(
    path: impl AsRef<Path>,
    identity: [u8; 32],
    options: &WriterOptions,
    include_icon: bool,
) -> Result<SnapshotDisplay, WriterError> {
    validate_options(options)?;
    let mut file = File::open(path)?;
    let mut budget = MemoryBudget {
        current: 0,
        peak: 0,
        limit: options.max_memory_bytes,
    };
    let archive = parse_archive(&mut file, 0, options, &mut budget)?;
    let snapshot = archive
        .snapshots
        .iter()
        .find(|s| s.snapshot.identity == identity)
        .ok_or_else(|| invalid("snapshot identity is absent"))?;
    let Some(refs) = snapshot.snapshot.meta_refs.as_ref() else {
        return Ok(SnapshotDisplay::default());
    };
    let mut read = |index, cap| -> Result<Option<Vec<u8>>, WriterError> {
        if index == NO_INDEX {
            return Ok(None);
        }
        let media = archive
            .media
            .get(index as usize)
            .ok_or_else(|| bad("display media reference outside table"))?;
        if media.size > cap {
            return Err(limit_error("display media exceeds read limit"));
        }
        budget.charge(media.size, "display media")?;
        let mut bytes = vec![0; media.size as usize];
        read_exact_at(&mut file, media.range.offset, &mut bytes)?;
        if crate::hash::sha256(&bytes) != media.digest {
            return Err(bad("display media digest mismatch"));
        }
        Ok(Some(bytes))
    };
    Ok(SnapshotDisplay {
        title: refs.title.clone(),
        icon: if include_icon {
            read(refs.icon, 16 * 1024 * 1024)?
        } else {
            None
        },
        nacp: read(refs.nacp, 1024 * 1024)?,
    })
}

/// The control.nacp bytes a snapshot shows (its display metadata), checked
/// against their Media digest. `None` when the snapshot has no NACP.
pub fn read_snapshot_nacp(
    path: impl AsRef<Path>,
    identity: [u8; 32],
    options: &WriterOptions,
) -> Result<Option<Vec<u8>>, WriterError> {
    validate_options(options)?;
    let mut file = File::open(path)?;
    let mut budget = MemoryBudget {
        current: 0,
        peak: 0,
        limit: options.max_memory_bytes,
    };
    let archive = parse_archive(&mut file, 0, options, &mut budget)?;
    let snapshot = archive
        .snapshots
        .iter()
        .find(|s| s.snapshot.identity == identity)
        .ok_or_else(|| invalid("snapshot identity is absent"))?;
    let Some(refs) = snapshot.snapshot.meta_refs.as_ref() else {
        return Ok(None);
    };
    if refs.nacp == NO_INDEX {
        return Ok(None);
    }
    let media = archive
        .media
        .get(refs.nacp as usize)
        .ok_or_else(|| bad("NACP media reference outside the Media table"))?;
    if media.size > 1024 * 1024 {
        return Err(bad("NACP media is too large"));
    }
    let mut bytes = vec![0u8; media.size as usize];
    read_exact_at(&mut file, media.range.offset, &mut bytes)?;
    if crate::hash::sha256(&bytes) != media.digest {
        return Err(bad("NACP media digest mismatch"));
    }
    Ok(Some(bytes))
}

fn destination_is_source(source: &Path, destination: &Path) -> bool {
    source == destination
        || matches!(
            (source.canonicalize(), destination.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
}

// Fail immediately on contention. The lock is held by an independent file
// handle through verification and publication, even when compaction replaces
// the archive path or the transaction closes its input handles.
fn lock_archive(path: &Path) -> Result<same_file::Handle, WriterError> {
    let file = File::open(path)?;
    try_lock_archive(&file).map_err(|error| {
        WriterError::new(
            WriterErrorKind::Io,
            format!("cannot lock archive for editing: {error}"),
        )
    })?;
    let locked = same_file::Handle::from_file(file)?;
    if locked != same_file::Handle::from_path(path)? {
        return Err(WriterError::new(
            WriterErrorKind::Io,
            "archive was replaced while acquiring its edit lock; retry the edit",
        ));
    }
    Ok(locked)
}

#[cfg(unix)]
fn try_lock_archive(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor remains open for the call and the lock lifetime.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn try_lock_archive(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::{AsRawHandle, RawHandle};

    #[link(name = "kernel32")]
    extern "system" {
        fn LockFile(handle: RawHandle, low: u32, high: u32, len_low: u32, len_high: u32) -> i32;
    }

    // Lock one byte outside any valid NX archive so independent readers can
    // continue to read existing generations. Closing the handle releases it.
    // SAFETY: the handle remains open and the offset is split into two DWORDs.
    let locked = unsafe {
        LockFile(
            file.as_raw_handle(),
            MAX_ARCHIVE_SIZE as u32,
            (MAX_ARCHIVE_SIZE >> 32) as u32,
            1,
            0,
        )
    };
    if locked != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Repack `source` into `destination`, publishing only after structural
/// validation and new-content verification succeed. A no-op still validates
/// the source according to `policy`, then returns without creating a file.
/// A no-op is only valid when the destination already is the source file.
/// Distinct spellings of one existing path compare equal after
/// canonicalization; anything else is rebuilt and published normally.
pub fn repack_archive(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    plan: RepackPlan,
    options: &WriterOptions,
    policy: IntegrityPolicy,
) -> Result<RepackStats, WriterError> {
    repack_inner(
        source.as_ref(),
        destination.as_ref(),
        plan,
        options,
        policy,
        None,
        false,
        &CatalogEdit::default(),
    )
    .map(|s| s.repack)
}

/// Rewrite an archive as a compact generation-0 file, reclaiming the free
/// space left by in-place edits. Payloads are copied by default. With
/// `CompressionMode::RawOnly`, retained Zstandard blocks are decoded and
/// written as RAW without changing file chunks or logical snapshot identities.
pub fn compact_archive(
    path: impl AsRef<Path>,
    options: &WriterOptions,
    policy: IntegrityPolicy,
) -> Result<RepackStats, WriterError> {
    let path = path.as_ref();
    repack_inner(
        path,
        path,
        RepackPlan::new(),
        options,
        policy,
        None,
        true,
        &CatalogEdit::default(),
    )
    .map(|s| s.repack)
}

/// Combine independently validated native selections before staging, while
/// keeping one canonical Application/Program row per logical identity.
fn merge_native_input(output: &mut ArchiveInput, input: ArchiveInput) -> Result<(), WriterError> {
    output.source_resources.extend(input.source_resources);
    for mut addition in input.applications {
        if let Some(existing) = output.applications.iter_mut().find(|app| {
            app.platform == addition.platform && app.application_id == addition.application_id
        }) {
            for program in addition.programs.drain(..) {
                if let Some(previous) = existing
                    .programs
                    .iter()
                    .find(|p| p.program_id == program.program_id)
                {
                    if previous.kind != program.kind
                        || previous.content_id != program.content_id
                        || previous.npdm.is_some()
                        || program.npdm.is_some()
                    {
                        return Err(invalid("batch native Program definitions disagree"));
                    }
                } else {
                    existing.programs.push(program);
                }
            }
            existing.snapshots.append(&mut addition.snapshots);
        } else {
            output.applications.push(addition);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn repack_inner(
    source: &Path,
    destination: &Path,
    mut plan: RepackPlan,
    options: &WriterOptions,
    policy: IntegrityPolicy,
    native: Option<(&[NativeAddRequest], &crate::native::InventoryOptions)>,
    compact: bool,
    recipes: &CatalogEdit,
) -> Result<NativeAddStats, WriterError> {
    validate_options(options)?;
    let started = Instant::now();
    let source_path = source;
    // Same-file edits through a symlink publish to its target, preserving the
    // symlink itself. Distinct destinations retain their normal semantics.
    let in_place = destination_is_source(source_path, destination);
    let canonical_destination = if in_place {
        Some(source_path.canonicalize()?)
    } else {
        None
    };
    let destination_path = canonical_destination.as_deref().unwrap_or(destination);
    if source_path.file_name().is_none() || destination_path.file_name().is_none() {
        return Err(invalid("source and destination must name files"));
    }
    let parent = destination_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(WriterError::new(
            WriterErrorKind::Io,
            "destination parent directory does not exist",
        ));
    }
    let mut budget = MemoryBudget {
        current: 0,
        peak: 0,
        limit: options.max_memory_bytes,
    };
    let _edit_lock = if in_place {
        Some(lock_archive(destination_path)?)
    } else {
        None
    };
    let mut source_file = File::open(canonical_destination.as_deref().unwrap_or(source_path))?;
    let mut source_archive = parse_archive(&mut source_file, 0, options, &mut budget)?;
    let decompress_blocks = compact && options.compression_mode == CompressionMode::RawOnly;
    if decompress_blocks && source_archive.blocks.iter().any(|b| b.codec == 1) {
        // At most one <=1 MiB logical block, one bounded frame, an 8 MiB
        // decoder allowance and the output buffer are live during rewriting.
        budget.charge(12 * 1024 * 1024, "RAW rewrite decoder and block buffers")?;
    }
    if native.is_some() {
        budget.charge(
            4 * 1024 * 1024,
            "native transaction source verification buffers",
        )?;
    }
    let mut stats = RepackStats {
        archive_bytes: source_archive.file_size,
        ..RepackStats::default()
    };
    // Every step is declared now, so a frontend's bar only moves forward: the
    // source check, the new content (written, read back, then merged in) and
    // the check of the result.
    let added = input_units(&plan.additions);
    let checks = if policy == IntegrityPolicy::Strict {
        2 * verification_units(&source_archive) + added
    } else {
        0
    };
    options
        .progress
        .reserve(checks + added + added / 3 * 2 + added);
    if policy == IntegrityPolicy::Strict {
        stats.source_verification =
            verify_source_payloads(&mut source_file, &source_archive, options)?;
        stats.source_payload_bytes_verified = stats.source_verification.stored_bytes_verified;
    }
    let mut native_stats = NativeAddStats::default();
    let mut inherited = Vec::new();
    if let Some((requests, native_options)) = native {
        let mut additions = ArchiveInput::new(Vec::new());
        let mut families = HashSet::new();
        for request in requests {
            cancelled(options)?;
            let prior_memory = budget.current;
            let (input, files) = native_add::prepare(
                source_path,
                destination_path,
                &mut source_file,
                &source_archive,
                request,
                native_options,
                options,
                &mut budget,
                &mut native_stats,
            )?;
            // The native parser's bounded inspection buffers are gone. Only
            // streamed input descriptors and inline media survive this call.
            budget.current = prior_memory;
            budget.charge(
                native_add::staged_input_memory(&input),
                "native batch staged inputs",
            )?;
            let selection = native_add::PreparedInheritance::from_input(&input, files)?;
            if !families.insert(selection.identity_key()) {
                return Err(invalid("duplicate native content family in batch"));
            }
            merge_native_input(&mut additions, input)?;
            inherited.push(selection);
        }
        plan.additions = additions;
    }
    let has_additions = !plan.additions.applications.is_empty();
    let mut selected_applications = HashSet::new();
    let metadata_selection_is_unchanged = plan.metadata_sources.iter().all(|selection| {
        let key = (selection.platform as u8, selection.application_id);
        selected_applications.insert(key)
            && source_archive
                .applications
                .iter()
                .find(|application| application.key == key)
                .and_then(|application| application.selected_identity)
                == Some(selection.snapshot_identity)
    });
    if in_place
        && !compact
        && plan.remove_snapshots.is_empty()
        && !has_additions
        && recipes.add.is_empty()
        && recipes.remove.is_empty()
        && recipes.metadata.is_empty()
        && (plan.metadata_sources.is_empty() || metadata_selection_is_unchanged)
    {
        stats.retained_snapshots = source_archive.snapshots.len() as u32;
        stats.no_op = true;
        options.progress.settle();
        stats.elapsed = started.elapsed();
        native_stats.repack = stats;
        return Ok(native_stats);
    }

    // Source 3: the new icons, if metadata edits bring any.
    let mut icons = Vec::new();
    apply_metadata_edits(
        &mut source_archive,
        &recipes.metadata,
        3,
        &mut icons,
        &mut plan,
    )?;
    let mut scratch_guard: Option<TempGuard> = None;
    let mut scratch_file: Option<File> = None;
    let mut scratch_archive: Option<SourceArchive> = None;
    let mut scratch_writer_stats: Option<WriterStats> = None;
    if has_additions {
        let (empty, guard) = unique_temp(parent, OsStr::new(".nx-repack-input"))?;
        drop(empty);
        let mut scratch_options = options.clone();
        let held_memory = budget.current;
        if native.is_some() {
            scratch_options.max_memory_bytes = options.max_memory_bytes.saturating_sub(held_memory);
        }
        // Cross-archive deduplication must happen before compression. The
        // intermediate is RAW and is streamed into final compression only
        // after exact comparisons against retained frames.
        scratch_options.compression_mode = CompressionMode::RawOnly;
        scratch_options.verify_output_payloads = true;
        // The intermediate is deleted after the edit: no need to flush it.
        scratch_options.sync_output = false;
        let input = std::mem::replace(&mut plan.additions, ArchiveInput::new(Vec::new()));
        let writer_stats = write_archive(input, &guard.path, &scratch_options)?;
        if native.is_some() {
            native_stats.accounted_memory_bytes = held_memory
                .checked_add(writer_stats.accounted_memory_bytes)
                .ok_or_else(|| limit_error("native staging memory accounting overflow"))?;
        }
        let mut file = File::open(&guard.path)?;
        let mut parsed = parse_archive(&mut file, 1, options, &mut budget)?;
        if native.is_some() {
            native_stats.new_native_bytes_staged = writer_stats.input_bytes;
            for selection in inherited {
                native_add::inherit_files(
                    &source_archive,
                    &mut parsed,
                    selection,
                    &mut budget,
                    &mut native_stats,
                )?;
            }
        }
        scratch_archive = Some(parsed);
        scratch_file = Some(file);
        stats.added_input_deduplicated_blocks = writer_stats.deduplicated_blocks;
        scratch_writer_stats = Some(writer_stats);
        scratch_guard = Some(guard);
    }
    let mut archives = vec![source_archive];
    if let Some(additional) = scratch_archive {
        archives.push(additional);
    }
    let mut input_files = vec![source_file];
    if let Some(file) = scratch_file {
        input_files.push(file);
    } else {
        input_files.push(input_files[0].try_clone()?);
    }
    let (spool_file, _spool_guard) = unique_temp(parent, OsStr::new(".nx-repack-frames"))?;
    let mut encoded_spool = spool_file;
    input_files.push(encoded_spool.try_clone()?); // source id 2: frames compressed after dedup
    let (mut icon_file, _icon_guard) = unique_temp(parent, OsStr::new(".nx-repack-icons"))?;
    icon_file.write_all(&icons)?;
    icon_file.flush()?;
    input_files.push(icon_file); // source id 3: new icons

    let edit_plan = RepackPlan {
        remove_snapshots: plan.remove_snapshots,
        additions: ArchiveInput::new(Vec::new()),
        metadata_sources: plan.metadata_sources,
    };
    let mut combined = combine_catalog(&archives, &edit_plan, &mut stats, options)?;
    let mut media = remap_media_references(&mut combined, &archives, &mut input_files, options)?;
    let mut directories = Vec::<DirectoryBuild>::new();
    let mut files = Vec::<FileBuild>::new();
    let mut block_refs = Vec::<u32>::new();
    let mut output_blocks = Vec::<SourceBlock>::new();
    let mut block_maps: Vec<Vec<Option<u32>>> = archives
        .iter()
        .map(|a| vec![None; a.blocks.len()])
        .collect();
    let mut candidates = HashMap::<(u32, u64), Vec<u32>>::new();
    // Seed the dedup index with every retained source block before processing
    // additions, regardless of where an added version sorts in the Catalog.
    let mut retained_block_ids = HashSet::new();
    for snapshot in combined.snapshots.iter().filter(|s| s.source_archive == 0) {
        collect_tree_blocks(&archives[0], snapshot.data.root, &mut retained_block_ids, 0)?;
    }
    for app in &archives[0].applications {
        if combined
            .apps
            .iter()
            .any(|a| (a.platform, a.application_id) == app.key)
        {
            for &(_, root) in &app.source_resources {
                collect_tree_blocks(&archives[0], root, &mut retained_block_ids, 0)?;
            }
        }
    }
    let mut block_encoder = None;
    // HashSet iteration is randomized. Canonical block order is part of
    // deterministic output, so sort the live set before assigning new IDs.
    let mut retained_block_ids: Vec<u32> = retained_block_ids.into_iter().collect();
    retained_block_ids.sort_unstable();
    for id in retained_block_ids {
        map_block(
            0,
            id,
            &archives,
            &mut input_files,
            &mut block_maps,
            &mut output_blocks,
            &mut candidates,
            &mut stats,
            options,
            &mut block_encoder,
            &mut encoded_spool,
            archives.get(1).map_or(0, |a| a.file_size),
            &mut budget,
        )?;
    }
    let scratch_bytes = archives.get(1).map_or(0, |a| a.file_size);
    // The new content is merged block by block (see map_block).
    let scratch_units: u64 = archives
        .get(1)
        .map_or(0, |a| a.blocks.iter().map(|b| b.logical_size as u64).sum());
    options.progress.add_total(scratch_units);
    let merged_from = options.progress.done();
    for snapshot in &mut combined.snapshots {
        let archive = &archives[snapshot.source_archive as usize];
        let root = append_snapshot_tree(
            snapshot.source_archive,
            snapshot.data.root,
            archive,
            &mut files,
            &mut directories,
            &mut block_refs,
            &archives,
            &mut input_files,
            &mut block_maps,
            &mut output_blocks,
            &mut candidates,
            &mut stats,
            options,
            0,
            &mut block_encoder,
            &mut encoded_spool,
            scratch_bytes,
            &mut budget,
        )?;
        snapshot.data.root = root;
    }
    for (owner, app) in combined.apps.iter().enumerate() {
        let mut rows = Vec::new();
        // An add of a cart already in the archive brings the same firmware
        // again: keep one copy of an identical resource (kind and files).
        let mut seen = HashSet::new();
        for archive in &archives {
            for source_app in archive
                .applications
                .iter()
                .filter(|a| a.key == (app.platform, app.application_id))
            {
                for &(kind, root) in &source_app.source_resources {
                    let content: Vec<(String, u64, [u8; 32])> = archive.directories[root as usize]
                        .iter()
                        .map(|e| {
                            let file = &archive.files[e.target as usize];
                            (e.name.clone(), file.size, file.digest)
                        })
                        .collect();
                    if !seen.insert((kind, content)) {
                        continue;
                    }
                    let root = append_snapshot_tree(
                        archive.archive,
                        root,
                        archive,
                        &mut files,
                        &mut directories,
                        &mut block_refs,
                        &archives,
                        &mut input_files,
                        &mut block_maps,
                        &mut output_blocks,
                        &mut candidates,
                        &mut stats,
                        options,
                        0,
                        &mut block_encoder,
                        &mut encoded_spool,
                        scratch_bytes,
                        &mut budget,
                    )?;
                    rows.push((kind, root));
                }
            }
        }
        if !rows.is_empty() {
            rows.sort_unstable();
            combined
                .extensions
                .push(source_resource_extension(owner as u32, &rows));
        }
    }
    // Blocks no snapshot or resource uses are not merged.
    options
        .progress
        .add_done(scratch_units.saturating_sub(options.progress.done() - merged_from));
    let with_deferred;
    let recipes = match recipes.deferred {
        Some(make) => {
            let present: HashSet<(u64, [u8; 32])> =
                files.iter().map(|f| (f.size, f.digest)).collect();
            let mut add = recipes.add.clone();
            add.extend(make(&present)?);
            with_deferred = CatalogEdit {
                add,
                remove: recipes.remove.clone(),
                ..CatalogEdit::default()
            };
            &with_deferred
        }
        None => recipes,
    };
    let recipe_extensions = recipe_extensions(&combined.apps, &files, &archives[0], recipes)?;
    combined.extensions.extend(recipe_extensions);
    let final_snapshots: Vec<FinalSnapshot> =
        combined.snapshots.into_iter().map(|s| s.data).collect();
    let catalog = build_catalog(
        &combined.apps,
        &combined.programs,
        &final_snapshots,
        &combined.extensions,
        &mut budget,
    )?;
    let manifest = build_manifest(&directories, &files, &block_refs, options, &mut budget)?;
    // Same-file edits append in place unless a compaction was requested, or
    // the append would leave too much of the file unused.
    let auto_compact = in_place
        && !compact
        && append_would_waste(
            &output_blocks,
            archives[0].slots_end.max(archives[0].file_size),
            options,
        );
    let append = in_place && !compact && !auto_compact;
    stats.auto_compacted = auto_compact;
    let (mut output_file, mut output_guard) = if append {
        (
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(destination_path)?,
            None,
        )
    } else {
        let (file, guard) = unique_temp(parent, OsStr::new(".nx-repack-output"))?;
        (file, Some(guard))
    };
    let mode = if append {
        OutputMode::Append {
            start: align_up(archives[0].slots_end.max(archives[0].file_size), 16)?,
            slot: 1 - archives[0].active_slot,
            generation: archives[0]
                .generation
                .checked_add(1)
                .ok_or_else(|| limit_error("archive generation overflow"))?,
        }
    } else {
        OutputMode::Compact
    };
    let temporary_base = scratch_bytes
        .checked_add(encoded_spool.metadata()?.len())
        .ok_or_else(|| limit_error("temporary disk usage overflow"))?;
    let written = write_repacked_archive(
        &mut output_file,
        &mut input_files,
        &catalog,
        &manifest,
        &mut media,
        &mut output_blocks,
        options,
        temporary_base,
        mode,
        decompress_blocks,
    )?;
    let archive_size = written.archive_size;
    stats.output_bytes_written = written.bytes_written;
    stats.archive_bytes = archive_size;
    stats.in_place_append = append;
    stats.free_bytes = written.free_bytes;
    let scratch_size = scratch_writer_stats.as_ref().map_or(0, |s| s.archive_bytes);
    let staged_size = encoded_spool.metadata()?.len();
    let combined_temporary = scratch_size
        .checked_add(staged_size)
        .and_then(|v| v.checked_add(written.bytes_written))
        .ok_or_else(|| limit_error("temporary disk accounting overflow"))?;
    check_temporary_size(combined_temporary, options)?;
    stats.peak_temporary_disk_bytes = scratch_writer_stats
        .as_ref()
        .map_or(0, |s| s.peak_temporary_disk_bytes)
        .max(combined_temporary);
    let mut verify_budget = MemoryBudget {
        current: 0,
        peak: 0,
        limit: if native.is_some() {
            options.max_memory_bytes.saturating_sub(budget.current)
        } else {
            options.max_memory_bytes
        },
    };
    if append {
        // Check the new payloads before the generation becomes visible.
        let mut check = File::open(destination_path)?;
        let mut decoder = zstd::bulk::Decompressor::new()?;
        decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
        let (mut frame, mut logical) = (Vec::new(), Vec::new());
        for &index in &written.new_block_indexes {
            let block = &output_blocks[index];
            decode_source_block(
                &mut check,
                block,
                &mut frame,
                &mut logical,
                &mut decoder,
                options,
            )?;
            if !checks_match(block, &logical) {
                return Err(bad("appended block CRC-32C mismatch"));
            }
        }
        drop(check);
        #[cfg(test)]
        if native.is_some() {
            native_add::cancel_publication_test(options);
        }
        cancelled(options)?;
        let OutputMode::Append {
            generation, slot, ..
        } = mode
        else {
            unreachable!()
        };
        drop(input_files);
        drop(encoded_spool);
        drop(scratch_guard);
        drop(_spool_guard);
        commit_generation(&mut output_file, &written.header, slot)?;
        // Full structural re-parse of the committed generation. On failure,
        // invalidate the new slot so the previous generation is selected.
        let mut check = File::open(destination_path)?;
        let result = parse_archive(&mut check, 3, options, &mut verify_budget).and_then(|parsed| {
            if parsed.generation != generation {
                return Err(bad("committed generation was not selected"));
            }
            if policy == IntegrityPolicy::Strict {
                stats.output_verification = verify_source_payloads(&mut check, &parsed, options)?;
            }
            Ok(())
        });
        if let Err(error) = result {
            drop(check);
            commit_generation(&mut output_file, &[0u8; 256], slot)?;
            return Err(error);
        }
        drop(check);
        if stats.removed_snapshots != 0 && !options.cancellation.is_cancelled() {
            stats.reclaimed_disk_bytes = reclaim::reclaim(
                destination_path,
                output_file,
                _edit_lock,
                &written,
                &output_blocks,
                1 - slot,
            );
        }
    } else {
        // Re-parse the staged result and verify every new payload. STRICT also
        // verifies the complete output, including retained source blocks.
        let output_guard = output_guard.as_mut().expect("compact output has a guard");
        let mut output_check = File::open(&output_guard.path)?;
        let output_archive = parse_archive(&mut output_check, 3, options, &mut verify_budget)?;
        if policy == IntegrityPolicy::Strict {
            stats.output_verification =
                verify_source_payloads(&mut output_check, &output_archive, options)?;
        } else {
            verify_new_output_payloads(
                &mut output_check,
                &output_archive,
                &written.new_block_indexes,
                &written.new_media_indexes,
                options,
            )?;
        }
        if !source_path.exists() {
            return Err(WriterError::new(
                WriterErrorKind::Io,
                "source archive disappeared before repack publication",
            ));
        }
        #[cfg(test)]
        if native.is_some() {
            native_add::cancel_publication_test(options);
        }
        cancelled(options)?;
        // Windows replacement can fail while either the original source or the
        // staged destination is still open without FILE_SHARE_DELETE.
        drop(output_check);
        drop(output_file);
        drop(input_files);
        drop(encoded_spool);
        drop(scratch_guard);
        drop(_spool_guard);
        atomic_replace(&output_guard.path, destination_path)?;
        output_guard.disarm();
    }
    options.progress.settle();
    stats.elapsed = started.elapsed();
    native_stats.repack = stats;
    native_stats.accounted_memory_bytes = native_stats
        .accounted_memory_bytes
        .max(budget.peak + verify_budget.peak);
    Ok(native_stats)
}

/// Recipes made from the edited archive's `(size, SHA-256)` file set, once it
/// is staged and before it is published: a failure publishes nothing.
pub(crate) type DeferredRecipes<'a> =
    &'a dyn Fn(&HashSet<(u64, [u8; 32])>) -> Result<Vec<(Platform, u64, Vec<u8>)>, WriterError>;

/// Catalog-only changes made during a repack: rebuild recipes and metadata.
#[derive(Default)]
pub(crate) struct CatalogEdit<'a> {
    /// (platform, application ID, recipe bytes).
    pub add: Vec<(Platform, u64, Vec<u8>)>,
    /// More recipes to add, made from the staged archive (see `DeferredRecipes`).
    pub deferred: Option<DeferredRecipes<'a>>,
    /// Recipe checksums (`RecipeInfo::recipe_sha256`) to drop.
    pub remove: Vec<[u8; 32]>,
    /// Display metadata to set.
    pub metadata: Vec<MetadataEdit>,
}

/// New display metadata for one application. `None` keeps the current value.
/// A new title or icon replaces the localized ones, so it shows in every
/// language.
#[derive(Clone, Debug, Default)]
pub struct MetadataEdit {
    pub application_id: u64,
    pub title: Option<String>,
    pub publisher: Option<String>,
    /// PNG or JPEG bytes.
    pub icon: Option<Vec<u8>>,
}

/// Set display metadata in place. Payload blocks are kept; only the new icon
/// and a new tail are written.
pub fn edit_archive_metadata(
    archive: impl AsRef<Path>,
    edits: Vec<MetadataEdit>,
    options: &WriterOptions,
    policy: IntegrityPolicy,
) -> Result<RepackStats, WriterError> {
    let path = archive.as_ref();
    repack_inner(
        path,
        path,
        RepackPlan::new(),
        options,
        policy,
        None,
        false,
        &CatalogEdit {
            metadata: edits,
            ..CatalogEdit::default()
        },
    )
    .map(|s| s.repack)
}

fn check_metadata_text(text: &str, what: &str) -> Result<(), WriterError> {
    if text.is_empty() || text.len() > 4096 || text.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{what} is empty, too long or has control characters"
        )));
    }
    Ok(())
}

/// Apply metadata edits to the source catalog. New icons are laid out one
/// after another in `icons`, read back as source `icon_source`. The edited
/// snapshots are selected as their applications' metadata source.
fn apply_metadata_edits(
    archive: &mut SourceArchive,
    edits: &[MetadataEdit],
    icon_source: u8,
    icons: &mut Vec<u8>,
    plan: &mut RepackPlan,
) -> Result<(), WriterError> {
    for edit in edits {
        let mut apps = archive
            .applications
            .iter()
            .filter(|a| a.key.1 == edit.application_id);
        let app = apps
            .next()
            .ok_or_else(|| invalid("the application is not in the archive"))?;
        if apps.next().is_some() {
            return Err(invalid(
                "the application ID is in the archive for two platforms",
            ));
        }
        let key = app.key;
        let selected = app.selected_identity;
        let platform = match key.0 {
            1 => Platform::Switch1,
            2 => Platform::Switch2,
            _ => Platform::Unknown,
        };
        // The snapshot that shows the application now, or its newest executable one.
        let target = match selected {
            Some(identity) => archive
                .snapshots
                .iter()
                .position(|s| s.key == key && s.snapshot.identity == identity),
            None => archive
                .snapshots
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.key == key && s.snapshot.snapshot_type != SnapshotType::Dlc as u8
                })
                .max_by_key(|(i, s)| (s.snapshot.version, *i))
                .map(|(i, _)| i),
        }
        .ok_or_else(|| invalid("the application has no snapshot to carry metadata"))?;
        if let Some(title) = &edit.title {
            check_metadata_text(title, "title")?;
        }
        if let Some(publisher) = &edit.publisher {
            check_metadata_text(publisher, "publisher")?;
        }
        let icon = match &edit.icon {
            Some(bytes) => {
                let media_type =
                    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
                        MediaType::PngIcon
                    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
                        MediaType::JpegIcon
                    } else {
                        return Err(invalid("the icon must be PNG or JPEG"));
                    };
                if archive.media.len() >= 65535 {
                    return Err(limit_error("Media record count exceeds NX 0.1 limit"));
                }
                let offset = icons.len() as u64;
                icons.extend_from_slice(bytes);
                archive.media.push(SourceMedia {
                    media_type: media_type as u16,
                    size: bytes.len() as u64,
                    digest: crate::hash::sha256(bytes),
                    range: SourceRange {
                        archive: icon_source,
                        offset,
                        size: bytes.len() as u64,
                    },
                });
                Some((archive.media.len() - 1) as u32)
            }
            None => None,
        };
        let snapshot = &mut archive.snapshots[target].snapshot;
        let meta = snapshot.meta_refs.get_or_insert_with(metadata_refs_empty);
        if let Some(title) = &edit.title {
            meta.title = Some(title.clone());
            for local in &mut meta.localized {
                local.title = None;
            }
        }
        if let Some(publisher) = &edit.publisher {
            meta.publisher = Some(publisher.clone());
        }
        if let Some(icon) = icon {
            meta.icon = icon;
            for local in &mut meta.localized {
                local.icon = NO_INDEX;
            }
        }
        meta.localized
            .retain(|local| local.title.is_some() || local.icon != NO_INDEX);
        let identity = snapshot.identity;
        plan.metadata_sources
            .retain(|s| !(s.platform as u8 == key.0 && s.application_id == key.1));
        plan.metadata_sources.push(MetadataSourceSelection {
            platform,
            application_id: key.1,
            snapshot_identity: identity,
        });
    }
    Ok(())
}

// Payload: magic NXRC, u16 version 1, u16 header size 16, u32 count, u32 zero,
// then per recipe a u64 length, its bytes, and zero padding to 8 bytes.
// Recipes are sorted by their own checksum and unique.
fn decode_recipes(payload: &[u8]) -> Result<Vec<Vec<u8>>, WriterError> {
    if payload.len() < 16
        || &payload[..4] != b"NXRC"
        || read_u16(payload, 4)? != 1
        || read_u16(payload, 6)? != 16
        || read_u32(payload, 12)? != 0
    {
        return Err(bad("invalid rebuild recipe extension header"));
    }
    let count = read_u32(payload, 8)? as usize;
    if count == 0 || count > 4096 {
        return Err(bad("invalid rebuild recipe count"));
    }
    let mut at = 16usize;
    let mut recipes = Vec::with_capacity(count);
    let mut prior: Option<[u8; 32]> = None;
    for _ in 0..count {
        let length = read_u64(payload, at)?;
        let start = at + 8;
        let end = usize::try_from(length)
            .ok()
            .and_then(|n| start.checked_add(n))
            .filter(|&e| e <= payload.len())
            .ok_or_else(|| bad("rebuild recipe is truncated"))?;
        let bytes = &payload[start..end];
        let info = crate::recipe::recipe_info(bytes)
            .map_err(|e| bad(format!("embedded rebuild recipe is invalid: {}", e.message)))?;
        if prior.is_some_and(|p| p >= info.recipe_sha256) {
            return Err(bad("rebuild recipes are unsorted or duplicated"));
        }
        prior = Some(info.recipe_sha256);
        let padded = align_up(end as u64, 8)? as usize;
        if padded > payload.len() || payload[end..padded].iter().any(|&b| b != 0) {
            return Err(bad("invalid rebuild recipe padding"));
        }
        recipes.push(bytes.to_vec());
        at = padded;
    }
    if at != payload.len() {
        return Err(bad("rebuild recipe extension has trailing bytes"));
    }
    Ok(recipes)
}

fn encode_recipes(recipes: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"NXRC");
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(&(recipes.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for recipe in recipes {
        out.extend_from_slice(&(recipe.len() as u64).to_le_bytes());
        out.extend_from_slice(recipe);
        out.resize(out.len().div_ceil(8) * 8, 0);
    }
    out
}

/// Keep each application's recipes whose files all survive, add new ones,
/// and drop the ones asked for. A recipe never outlives a file it needs.
fn recipe_extensions(
    apps: &[ApplicationBuild],
    files: &[FileBuild],
    source: &SourceArchive,
    edit: &CatalogEdit,
) -> Result<Vec<CatalogExtension>, WriterError> {
    let present: HashSet<(u64, [u8; 32])> = files.iter().map(|f| (f.size, f.digest)).collect();
    let removed: HashSet<[u8; 32]> = edit.remove.iter().copied().collect();
    for (platform, id, bytes) in &edit.add {
        let info = crate::recipe::recipe_info(bytes)?;
        if !apps
            .iter()
            .any(|a| a.platform == *platform as u8 && a.application_id == *id)
        {
            return Err(invalid(
                "a rebuild recipe names an application absent from the archive",
            ));
        }
        if info.files.iter().any(|f| !present.contains(f)) {
            return Err(invalid(
                "a rebuild recipe needs a file that is not in the archive",
            ));
        }
    }
    let mut extensions = Vec::new();
    let mut total = 0u64;
    for (index, app) in apps.iter().enumerate() {
        let key = (app.platform, app.application_id);
        let mut kept = std::collections::BTreeMap::<[u8; 32], Vec<u8>>::new();
        let existing = source
            .applications
            .iter()
            .filter(|a| a.key == key)
            .flat_map(|a| a.recipes.iter());
        let added = edit
            .add
            .iter()
            .filter(|(p, id, _)| (*p as u8, *id) == key)
            .map(|(_, _, b)| b);
        for bytes in existing.chain(added) {
            let info = crate::recipe::recipe_info(bytes)?;
            if removed.contains(&info.recipe_sha256)
                || info.files.iter().any(|f| !present.contains(f))
            {
                continue;
            }
            kept.insert(info.recipe_sha256, bytes.clone());
        }
        if kept.is_empty() {
            continue;
        }
        let recipes: Vec<Vec<u8>> = kept.into_values().collect();
        let bytes = encode_recipes(&recipes);
        total += bytes.len() as u64;
        if total > MAX_EMBEDDED_RECIPE_BYTES {
            return Err(limit_error(
                "rebuild recipes are too large to keep inside the archive",
            ));
        }
        extensions.push(CatalogExtension {
            type_id: RECIPE_EXTENSION,
            flags: 0,
            owner_kind: 1,
            owner: index as u32,
            bytes,
        });
    }
    Ok(extensions)
}

/// Add rebuild recipes to an archive and drop others, in place. Nothing else
/// changes: payload blocks are kept, only a new tail is written.
pub fn edit_archive_recipes(
    archive: impl AsRef<Path>,
    add: Vec<(Platform, u64, Vec<u8>)>,
    remove: Vec<[u8; 32]>,
    options: &WriterOptions,
    policy: IntegrityPolicy,
) -> Result<RepackStats, WriterError> {
    let path = archive.as_ref();
    repack_inner(
        path,
        path,
        RepackPlan::new(),
        options,
        policy,
        None,
        false,
        &CatalogEdit {
            add,
            remove,
            ..CatalogEdit::default()
        },
    )
    .map(|s| s.repack)
}

#[cfg(test)]
mod source_resource_validation_tests {
    use super::*;

    fn image() -> Vec<u8> {
        let app = 0x0100_0000_0001_0000;
        let bytes = crate::native::synthetic::dxci(&[
            ("update", vec![("unknown.bin".into(), vec![7; 5000])]),
            (
                "secure",
                crate::native::synthetic::title_entries(
                    &crate::native::synthetic::TitleSpec::application(app, 0, 4),
                ),
            ),
        ]);
        let (mut source, source_guard) =
            unique_temp(&std::env::temp_dir(), OsStr::new("nx-resource-source")).unwrap();
        source.write_all(&bytes).unwrap();
        drop(source);
        let (output, output_guard) =
            unique_temp(&std::env::temp_dir(), OsStr::new("nx-resource-output")).unwrap();
        drop(output);
        let native = crate::native::InventoryOptions::default();
        let inventory =
            crate::native::inventory(std::slice::from_ref(&source_guard.path), &native).unwrap();
        crate::conversion::convert_inventory(
            &inventory,
            &output_guard.path,
            &native,
            IntegrityPolicy::Strict,
            &WriterOptions::default(),
        )
        .unwrap();
        std::fs::read(output_guard.path.clone()).unwrap()
    }

    // Re-sign metadata so rejection tests structure/ownership, not just hashes.
    fn resign(bytes: &mut [u8]) {
        let cat = read_u64(bytes, 24).unwrap() as usize;
        let len = read_u64(bytes, 32).unwrap() as usize;
        let digest = sha256(&bytes[cat..cat + len]);
        for slot in [0, 4096] {
            bytes[slot + 120..slot + 152].copy_from_slice(&digest);
            let crc = crc32c(&bytes[slot..slot + 252]);
            write_u32_at(bytes, slot + 252, crc);
        }
    }

    #[test]
    fn failed_deferred_recipes_publish_nothing() {
        let app = 0x0100_0000_0001_0000;
        let base = image();
        let patch = crate::native::synthetic::dxci(&[
            ("update", vec![("other.bin".into(), vec![9; 3000])]),
            (
                "secure",
                crate::native::synthetic::title_entries(
                    &crate::native::synthetic::TitleSpec::patch(app, 1, 9),
                ),
            ),
        ]);
        let (mut file, archive) =
            unique_temp(&std::env::temp_dir(), OsStr::new("nx-deferred-archive")).unwrap();
        file.write_all(&base).unwrap();
        drop(file);
        let (mut file, source) =
            unique_temp(&std::env::temp_dir(), OsStr::new("nx-deferred-patch")).unwrap();
        file.write_all(&patch).unwrap();
        drop(file);
        let request = NativeAddRequest::new(&source.path, app | 0x800, 1);
        let native = crate::native::InventoryOptions::default();
        let options = WriterOptions::default();
        let seen = std::cell::Cell::new(false);
        let fail = |known: &HashSet<(u64, [u8; 32])>| {
            // Called with the staged archive's files, before publication.
            seen.set(!known.is_empty());
            Err(invalid("recipe refused"))
        };
        let error = add_native_contents_with_recipes(
            &archive.path,
            &archive.path,
            std::slice::from_ref(&request),
            &native,
            &options,
            IntegrityPolicy::Fast,
            Some(&fail),
        )
        .unwrap_err();
        assert!(error.to_string().contains("recipe refused"), "{error}");
        assert!(seen.get());
        assert_eq!(std::fs::read(&archive.path).unwrap(), base);
        // Without the failing hook the same add succeeds.
        add_native_contents_with_recipes(
            &archive.path,
            &archive.path,
            std::slice::from_ref(&request),
            &native,
            &options,
            IntegrityPolicy::Fast,
            None,
        )
        .unwrap();
        assert_ne!(std::fs::read(&archive.path).unwrap(), base);
    }

    #[test]
    fn authenticated_invalid_resource_descriptors_reject() {
        let valid = image();
        let cat = read_u64(&valid, 24).unwrap() as usize;
        let table = cat + read_u64(&valid, cat + 88).unwrap() as usize;
        let count = read_u32(&valid, cat + 40).unwrap() as usize;
        let descriptor = (0..count)
            .map(|i| table + i * 32)
            .find(|&at| read_u32(&valid, at).unwrap() == SOURCE_RESOURCES)
            .unwrap();
        let payload = cat + read_u64(&valid, descriptor + 16).unwrap() as usize;
        let snapshot_table = cat + read_u64(&valid, cat + 64).unwrap() as usize;
        let snapshot_root = read_u32(&valid, snapshot_table + 12).unwrap();
        for case in 0..7 {
            let mut bytes = valid.clone();
            match case {
                0 => write_u32_at(&mut bytes, payload + 20, u32::MAX),
                1 => write_u32_at(&mut bytes, payload + 16, 2),
                2 => write_u32_at(&mut bytes, descriptor + 4, 0),
                3 => write_u32_at(&mut bytes, descriptor + 12, u32::MAX),
                4 => write_u32_at(&mut bytes, payload + 8, 4097),
                5 => write_u32_at(&mut bytes, payload + 20, snapshot_root),
                _ => {
                    for slot in [0, 4096] {
                        write_u16_at(&mut bytes, slot + 6, 2);
                    }
                }
            }
            resign(&mut bytes);
            let (mut file, guard) =
                unique_temp(&std::env::temp_dir(), OsStr::new("nx-resource-invalid")).unwrap();
            file.write_all(&bytes).unwrap();
            drop(file);
            assert!(
                inspect_archive(
                    &guard.path,
                    &WriterOptions::default(),
                    IntegrityPolicy::Fast
                )
                .is_err(),
                "case {case}"
            );
        }
    }
}
