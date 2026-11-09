use crate::hash::{check_count, crc32c, sha256, slice_checks, Sha256};
use crate::model::*;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[path = "writer_parallel.rs"]
mod parallel;

const HEADER_SIZE: u64 = 256;
const CATALOG_HEADER_SIZE: u64 = 128;
const APP_SIZE: u64 = 80;
const PROGRAM_SIZE: u64 = 64;
const SNAPSHOT_SIZE: u64 = 120;
const LOCALIZED_SIZE: u64 = 24;
const DEPENDENCY_SIZE: u64 = 56;
const EXTENSION_SIZE: u64 = 32;
const MEDIA_HEADER_SIZE: u64 = 32;
const MEDIA_RECORD_SIZE: u64 = 64;
const MANIFEST_HEADER_SIZE: u64 = 96;
const DIRECTORY_SIZE: u64 = 16;
const ENTRY_SIZE: u64 = 24;
const FILE_SIZE: u64 = 64;
const BLOCK_SIZE: u64 = 24;
const NO_INDEX: u32 = u32::MAX;
const RAW_CHUNK_SIZE: u32 = 256 * 1024;
const VALID_CHUNK_SIZES: [u32; 5] = [
    64 * 1024,
    128 * 1024,
    RAW_CHUNK_SIZE,
    512 * 1024,
    1024 * 1024,
];
const MAX_ARCHIVE_SIZE: u64 = i64::MAX as u64;
// Conservative allowance for one single-threaded Zstandard context constrained
// to a <=1 MiB window and levels 1..=13. Charged once when that context is made.
const ZSTD_CONTEXT_MEMORY_ESTIMATE: u64 = 32 * 1024 * 1024;
const ZSTD_DECODER_MEMORY_ESTIMATE: u64 = 8 * 1024 * 1024;
pub(crate) const SOURCE_RESOURCES: u32 = 0x0001_0008;

fn source_resource_extension(owner: u32, rows: &[(u32, u32)]) -> CatalogExtension {
    let mut bytes = Vec::with_capacity(16 + rows.len() * 8);
    bytes.extend_from_slice(b"NXSR");
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    for &(kind, root) in rows {
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&root.to_le_bytes());
    }
    CatalogExtension {
        type_id: SOURCE_RESOURCES,
        flags: 1,
        owner_kind: 1,
        owner,
        bytes,
    }
}

const SNAPSHOT_NATIVE_METADATA: u32 = 0x0001_0005;
const SNAPSHOT_METADATA_HISTORY: u32 = 0x0001_0004;
const MAX_MEDIA_PAYLOAD: u64 = 64 * 1024 * 1024;
const MAX_MEDIA_REGION: u64 = 4 * 1024 * 1024 * 1024;
const MAX_CATALOG: u64 = 16 * 1024 * 1024;
const MAX_MANIFEST: u64 = 4 * 1024 * 1024 * 1024;
const MAX_PATH_POOL: u64 = 2 * 1024 * 1024 * 1024;
const MAX_DIRECTORY_DEPTH: usize = 256;
const DEDUP_BUCKET_ESTIMATE: u64 = 160;
const DEDUP_COLLISION_ESTIMATE: u64 = 32;

/// Stable error class suitable for a converter UI or command-line frontend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterErrorKind {
    InvalidInput,
    Integrity,
    ResourceLimit,
    Io,
    Cancelled,
}

/// Per-block codec policy. Zstandard falls back to RAW when its frame is not
/// smaller than the original logical block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompressionMode {
    /// Write all unique blocks as RAW. Explicit compaction also decodes
    /// retained compressed blocks and rewrites them as RAW.
    RawOnly,
    /// Try one independent Zstandard frame per unique block, then choose the
    /// smaller representation.
    ZstandardAuto,
}

/// Opt-in per-file sizing; no extension/content classification or reader work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockSizing {
    Fixed,
    Adaptive,
}

fn file_chunk_size(_size: u64, profile: FileReadProfile, options: &WriterOptions) -> u32 {
    if options.block_sizing == BlockSizing::Fixed {
        return options.chunk_size_bytes;
    }
    // Even short files retain the fallback without representative evidence.
    // Their single physical tail already has its exact short logical size.
    let selected = if profile.observations() >= 10_000
        && profile.largest_read() <= 64 * 1024
        && profile.contiguous_reads() as u128 * 100 <= profile.observations() as u128
    {
        64 * 1024
    } else {
        RAW_CHUNK_SIZE
    };
    selected.min(options.chunk_size_bytes)
}

fn input_chunk_size(file: &FileInput, options: &WriterOptions) -> u32 {
    if file.adaptive_disabled && options.block_sizing == BlockSizing::Adaptive {
        RAW_CHUNK_SIZE.min(options.chunk_size_bytes)
    } else {
        file_chunk_size(file.size, file.read_profile, options)
    }
}

/// Greedy benefit/extra-index selection, stable across source enumeration order.
/// A digest groups partition choices only; byte equality still uses the existing
/// bounded comparison during deduplication. Costs count every occurrence.
fn select_adaptive(
    input: &mut ArchiveInput,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
    depth: u64,
) -> Result<(), WriterError> {
    if options.block_sizing != BlockSizing::Adaptive {
        return Ok(());
    }
    budget.charge(
        depth
            .checked_mul((std::mem::size_of::<Sha256>() * 2 + 128) as u64)
            .ok_or_else(|| limit_error("adaptive selection overflow"))?,
        "adaptive selection scratch",
    )?;
    #[derive(Default)]
    struct Candidate {
        cost: u64,
        benefit: u128,
    }
    fn visit(
        dir: &mut DirectoryInput,
        parent: &Sha256,
        f: &mut impl FnMut(&mut FileInput, [u8; 41]) -> Result<(), WriterError>,
    ) -> Result<(), WriterError> {
        for entry in &mut dir.entries {
            let mut path = parent.clone();
            path.update(entry.name.as_bytes());
            path.update(&[0]);
            match &mut entry.node {
                NodeInput::Directory(child) => visit(child, &path, f)?,
                NodeInput::File(file) => {
                    let mut key = [0; 41];
                    if let Some(captured) = &file.captured {
                        key[0] = 1;
                        key[1..33].copy_from_slice(&captured.sha256);
                    } else {
                        key[1..33].copy_from_slice(&path.finish());
                    }
                    key[33..].copy_from_slice(&file.size.to_le_bytes());
                    f(file, key)?;
                }
            }
        }
        Ok(())
    }
    let mut groups = std::collections::BTreeMap::<[u8; 41], Candidate>::new();
    for app in &mut input.applications {
        for snapshot in &mut app.snapshots {
            visit(&mut snapshot.root, &Sha256::new(), &mut |file, key| {
                cancelled(options)?;
                if file.size == 0 {
                    return Ok(());
                }
                let small = file_chunk_size(file.size, file.read_profile, options) as u64;
                let base = RAW_CHUNK_SIZE.min(options.chunk_size_bytes) as u64;
                let extra = ((file.size - 1) / small).saturating_sub((file.size - 1) / base) * 96;
                if extra != 0 {
                    budget.charge(384, "adaptive candidate/rank/selection scratch")?;
                    let group = groups.entry(key).or_default();
                    group.cost = group
                        .cost
                        .checked_add(extra)
                        .ok_or_else(|| limit_error("adaptive cost overflow"))?;
                    group.benefit = group.benefit.saturating_add(
                        file.read_profile.observations() as u128 * (base - small) as u128,
                    );
                }
                Ok(())
            })?;
        }
    }
    let mut ranked: Vec<_> = groups.iter().collect();
    ranked.sort_by(|(ka, a), (kb, b)| {
        (b.benefit.saturating_mul(a.cost as u128))
            .cmp(&a.benefit.saturating_mul(b.cost as u128))
            .then_with(|| ka.cmp(kb))
    });
    let mut remaining = options.adaptive_index_growth_bytes;
    let mut selected = std::collections::BTreeSet::new();
    for (key, candidate) in ranked {
        if candidate.cost <= remaining {
            remaining -= candidate.cost;
            selected.insert(*key);
        }
    }
    for app in &mut input.applications {
        for snapshot in &mut app.snapshots {
            visit(&mut snapshot.root, &Sha256::new(), &mut |file, key| {
                cancelled(options)?;
                file.adaptive_disabled = groups.contains_key(&key) && !selected.contains(&key);
                Ok(())
            })?;
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct WriterError {
    kind: WriterErrorKind,
    message: String,
}

impl WriterError {
    pub(crate) fn new(kind: WriterErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn kind(&self) -> WriterErrorKind {
        self.kind
    }
    pub fn message(&self) -> &str {
        &self.message
    }
}
impl std::fmt::Display for WriterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for WriterError {}
impl From<io::Error> for WriterError {
    fn from(value: io::Error) -> Self {
        Self::new(WriterErrorKind::Io, value.to_string())
    }
}

/// Cloneable cancellation flag. Cancellation is checked between bounded reads,
/// archive sections, and immediately before atomic publication.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Cloneable progress counter for frontends: bytes expected and bytes done
/// so far. The writer adds every file and Media byte it reads; a caller adds
/// what it expects to read. Neither value is a format guarantee: re-reads
/// may push `done` past `total`.
#[derive(Clone, Debug, Default)]
pub struct ProgressCounter(Arc<(AtomicU64, AtomicU64, AtomicU64)>);
impl ProgressCounter {
    pub fn add_total(&self, bytes: u64) {
        // A step declared ahead by `reserve` is not counted twice.
        let reserve = &self.0 .2;
        let mut reserved = reserve.load(Ordering::Relaxed);
        loop {
            let used = reserved.min(bytes);
            match reserve.compare_exchange_weak(
                reserved,
                reserved - used,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.0 .0.fetch_add(bytes - used, Ordering::Relaxed);
                    return;
                }
                Err(now) => reserved = now,
            }
        }
    }
    /// Declares the bytes of later steps now, so the total does not grow (and
    /// a frontend's bar does not move back) when they start. Their own
    /// `add_total` calls are taken from this reserve.
    pub(crate) fn reserve(&self, bytes: u64) {
        self.0 .0.fetch_add(bytes, Ordering::Relaxed);
        self.0 .2.fetch_add(bytes, Ordering::Relaxed);
    }
    /// Counts what is left of the reserve as done, once the steps have run.
    pub(crate) fn settle(&self) {
        let left = self.0 .2.swap(0, Ordering::Relaxed);
        self.0 .1.fetch_add(left, Ordering::Relaxed);
    }
    pub fn add_done(&self, bytes: u64) {
        self.0 .1.fetch_add(bytes, Ordering::Relaxed);
    }
    pub fn total(&self) -> u64 {
        self.0 .0.load(Ordering::Relaxed)
    }
    pub fn done(&self) -> u64 {
        self.0 .1.load(Ordering::Relaxed)
    }
}

/// Writer resource ceilings. These may be lowered from the NX 0.1 format
/// limits, but never raised above them. `max_memory_bytes` caps conservative
/// cumulative accounting for tracked writer allocations; it is not an RSS cap
/// and does not include caller-owned buffers/streams. File content is streamed
/// in one of the five normative block sizes from 64 KiB through 1 MiB.
#[derive(Clone)]
pub struct WriterOptions {
    /// Native imports retain source-system partitions independently of recipes.
    pub preserve_firmware: bool,
    /// Display-only fallback for missing titles and artwork; never changes NACP.
    pub display_fallback: Option<DisplayFallback>,
    pub max_memory_bytes: u64,
    /// Estimated memory ceiling for the global block-deduplication index.
    pub max_dedup_index_bytes: u64,
    pub max_files: u32,
    pub max_directories: u32,
    pub max_entries: u32,
    pub max_path_component_bytes: u32,
    pub max_file_size: u64,
    pub max_archive_size: u64,
    /// Combined staging streams plus output temporary file, checked before growth.
    pub max_temporary_disk_bytes: u64,
    pub chunk_size_bytes: u32,
    /// Fixed chunk, or upper bound for adaptive choices. Unknown access uses
    /// min(256 KiB, this bound). The default remains Fixed.
    pub block_sizing: BlockSizing,
    /// Cap estimated extra reader index/reference storage from adaptive sizing.
    /// Planning counts file occurrences conservatively, ranks observed benefit
    /// per extra index byte, then retains a deterministic subset of candidates.
    pub adaptive_index_growth_bytes: u64,
    pub compression_mode: CompressionMode,
    /// Zstandard level, 1..=13. Measured on Mario Kart World and Pokopia
    /// (64 KiB blocks): decode time stays flat up to 13 and rises from 15,
    /// where more blocks also cross the threshold; 13 saves only ~0.03 %.
    pub zstd_compression_level: i32,
    /// Keep a Zstandard frame only when it saves at least this many basis
    /// points (1/100 %) of the logical block; otherwise store RAW so readers
    /// skip decompression for negligible gains. 0 keeps any smaller frame and
    /// disables the cheap RAW predictor. Default 2500 (25 %): decoding a
    /// 256 KiB block costs ~140-220 us almost regardless of its ratio, and
    /// 25 % is where the I/O saved on a SATA SSD (~64 KiB, ~170-220 us) first
    /// pays that back; on NVMe or a warm cache only larger gains are worth it.
    pub min_compression_savings_bp: u16,
    /// Chunk size for files whose first chunk keeps Zstandard, or 0 to keep
    /// the file's normal size. Default 64 KiB: on Mario Kart World and Pokopia
    /// it changed the archive size by under 0.1 % while a cache miss decodes
    /// up to 3x less. Incompressible files keep their larger RAW chunks, so
    /// the index grows only for the few compressed files.
    pub compressed_chunk_bytes: u32,
    /// Independent block compression workers: 1-8, or 0 (default) for
    /// automatic. Output is identical for any count: frames are written in
    /// block ID order. Automatic uses up to 4 (capped by the available cores)
    /// and drops workers until they fit `max_memory_bytes`; an explicit count
    /// that does not fit is an error. Synthetic game-like data gains ~2.2x
    /// from 1 to 2 workers and little past 4, where hashing/dedup on the
    /// caller thread becomes the limit.
    pub compression_workers: u8,
    /// Input/result credits, including results waiting for ordered output.
    /// Contexts and stacks are separately charged to max_memory_bytes.
    pub compression_queue_bytes: u64,
    /// Re-read payloads before publication as well as all serialized metadata.
    pub verify_output_payloads: bool,
    /// Flush the archive to disk before publishing it. Turned off only for
    /// temporary files that are deleted afterwards.
    pub sync_output: bool,
    /// Exact-structure Strict file verification cache; 0 disables it, maximum 128.
    pub strict_file_cache_entries: u16,
    /// Same-file edits normally append. When the space left unused by older
    /// generations would reach this percentage of the archive, the edit
    /// rewrites a compact archive instead. 0 never compacts automatically.
    /// Default 0.
    pub auto_compact_free_percent: u8,
    pub cancellation: CancellationToken,
    /// Counts the source bytes read, for a frontend progress bar.
    pub progress: ProgressCounter,
}
impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            preserve_firmware: true,
            display_fallback: None,
            max_memory_bytes: 512 * 1024 * 1024,
            max_dedup_index_bytes: 256 * 1024 * 1024,
            max_files: 1_000_000,
            max_directories: 1_000_000,
            max_entries: 2_000_000,
            max_path_component_bytes: 4096,
            max_file_size: MAX_ARCHIVE_SIZE,
            max_archive_size: MAX_ARCHIVE_SIZE,
            max_temporary_disk_bytes: 1024 * 1024 * 1024 * 1024,
            chunk_size_bytes: RAW_CHUNK_SIZE,
            block_sizing: BlockSizing::Fixed,
            adaptive_index_growth_bytes: 1024 * 1024,
            compression_mode: CompressionMode::ZstandardAuto,
            zstd_compression_level: 3,
            min_compression_savings_bp: 2500,
            compressed_chunk_bytes: 64 * 1024,
            compression_workers: 0,
            compression_queue_bytes: 32 * 1024 * 1024,
            verify_output_payloads: true,
            sync_output: true,
            strict_file_cache_entries: 128,
            auto_compact_free_percent: 0,
            cancellation: CancellationToken::default(),
            progress: ProgressCounter::default(),
        }
    }
}

/// Measurements returned by a successful write. `accounted_memory_bytes` is a
/// conservative cumulative estimate of writer-tracked allocations. It is not a
/// measured peak or process RSS, and excludes caller-owned input storage.
/// Temporary disk includes the staging streams and the output temporary file.
#[derive(Clone, Debug)]
pub struct WriterStats {
    pub input_bytes: u64,
    pub archive_bytes: u64,
    pub file_entries: u32,
    pub file_records: u32,
    pub directory_records: u32,
    pub block_count: u32,
    /// Input chunks confirmed byte-identical to an existing physical block.
    pub deduplicated_blocks: u64,
    /// Conservative estimate for the global block-deduplication index.
    pub dedup_index_bytes: u64,
    /// Total bytes in the internal block spool: logical bytes for exact
    /// duplicate checks plus compressed frames selected for archive storage.
    pub staged_block_bytes: u64,
    pub staged_logical_block_bytes: u64,
    pub staged_stored_block_bytes: u64,
    pub raw_block_count: u32,
    pub zstd_block_count: u32,
    pub media_records: u32,
    pub elapsed: Duration,
    pub input_bytes_per_second: u64,
    pub accounted_memory_bytes: u64,
    pub peak_temporary_disk_bytes: u64,
    pub input_read_hash_time: Duration,
    /// Payload reads only; excludes hashing, compression and spool I/O.
    pub input_read_time: Duration,
    /// Input file and block SHA-256 updates/finalization, excluding verification.
    pub input_hash_time: Duration,
    /// Logical blocks plus selected frames written to the deduplication spool.
    pub block_staging_write_time: Duration,
    pub compression_stage_time: Duration,
    pub compression_staging_read_time: Duration,
    pub compression_staging_read_bytes: u64,
    pub compression_queue_peak_bytes: u64,
    pub global_dedup_time: Duration,
    pub compression_time: Duration,
    pub archive_write_time: Duration,
    pub output_verification_time: Duration,
    pub output_sync_time: Duration,
    pub atomic_publication_time: Duration,
    /// Peak ZSTD_sizeof_CCtx reported by the compressor, excluding input/output
    /// buffers and allocator bookkeeping. Zero for RAW-only construction.
    pub compression_context_peak_bytes: u64,
}

struct MemoryBudget {
    current: u64,
    peak: u64,
    limit: u64,
}
impl MemoryBudget {
    fn charge(&mut self, amount: u64, what: &str) -> Result<(), WriterError> {
        self.current = self
            .current
            .checked_add(amount)
            .ok_or_else(|| limit_error("estimated memory accounting overflow"))?;
        self.peak = self.peak.max(self.current);
        if self.current > self.limit {
            return Err(limit_error(format!(
                "writer memory estimate exceeds configured limit while accounting {what}"
            )));
        }
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> WriterError {
    WriterError::new(WriterErrorKind::InvalidInput, message)
}
fn limit_error(message: impl Into<String>) -> WriterError {
    WriterError::new(WriterErrorKind::ResourceLimit, message)
}
fn cancelled(options: &WriterOptions) -> Result<(), WriterError> {
    if options.cancellation.is_cancelled() {
        Err(WriterError::new(
            WriterErrorKind::Cancelled,
            "archive write cancelled",
        ))
    } else {
        Ok(())
    }
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub(crate) struct TempGuard {
    pub(crate) path: PathBuf,
    armed: bool,
}
impl TempGuard {
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn unique_temp(parent: &Path, prefix: &OsStr) -> Result<(File, TempGuard), WriterError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..128u64 {
        let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut name = OsString::from(prefix);
        name.push(format!(
            "-{}-{nanos:x}-{seq:x}-{attempt:x}.tmp",
            std::process::id()
        ));
        let path = parent.join(name);
        match OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((file, TempGuard { path, armed: true })),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(WriterError::new(
        WriterErrorKind::Io,
        "could not allocate a unique temporary file",
    ))
}

/// Zstandard levels used only to predict savings, never stored.
const PROBE_LEVEL: i32 = 1;
const SCAN_LEVEL: i32 = -5;
const PROBE_BYTES: usize = 4096;

/// Cheap RAW prediction shared by every encoder. Two predictors must both
/// show less than half of the savings threshold: three 4 KiB level-1 probes
/// (start, middle, end) estimate typical data well, and a full-block fast
/// pass sees every byte, so data that only compresses away from the probe
/// points (or repeats at long distance) still gets a full encode. A miss only
/// costs space: the block is stored RAW, never read incorrectly.
/// Measured on real Mario Kart World blocks: 2.6x cheaper decisions at 25 %
/// with under 1 % of qualifying blocks missed (all just above the threshold).
pub(crate) fn predicts_low_gain(
    bytes: &[u8],
    options: &WriterOptions,
    mut compress: impl FnMut(i32, &[u8]) -> Result<usize, WriterError>,
) -> Result<bool, WriterError> {
    let threshold = options.min_compression_savings_bp as u64;
    if threshold == 0 || bytes.len() < PROBE_BYTES * 4 {
        return Ok(false);
    }
    // savings(bp) < threshold / 2  <=>  (len - out) * 20000 < len * threshold
    let low =
        |out: usize, len: usize| (len.saturating_sub(out) as u64) * 20_000 < len as u64 * threshold;
    let mut probed = 0;
    for at in [
        0,
        bytes.len() / 2 - PROBE_BYTES / 2,
        bytes.len() - PROBE_BYTES,
    ] {
        probed += compress(PROBE_LEVEL, &bytes[at..at + PROBE_BYTES])?;
    }
    if !low(probed, PROBE_BYTES * 3) {
        return Ok(false);
    }
    Ok(low(compress(SCAN_LEVEL, bytes)?, bytes.len()))
}

/// RAW/Zstd choice shared by serial, parallel and repack paths.
pub(crate) fn keep_compressed(
    frame_len: usize,
    logical_len: usize,
    options: &WriterOptions,
) -> bool {
    frame_len < logical_len
        && ((logical_len - frame_len) as u64) * 10_000
            >= logical_len as u64 * options.min_compression_savings_bp as u64
}

fn validate_options(options: &WriterOptions) -> Result<(), WriterError> {
    cancelled(options)?;
    if options.compression_workers > 8 {
        return Err(invalid(
            "compression workers must be 0 (automatic) or 1 through 8",
        ));
    }
    if options.auto_compact_free_percent > 100 {
        return Err(invalid(
            "automatic compaction threshold must be 0 through 100",
        ));
    }
    if options.strict_file_cache_entries > 128 {
        return Err(invalid("Strict file cache cannot exceed 128 entries"));
    }
    if options.max_temporary_disk_bytes < HEADER_SIZE {
        return Err(invalid("temporary disk budget must allow a header"));
    }
    if !VALID_CHUNK_SIZES.contains(&options.chunk_size_bytes) {
        return Err(invalid("chunk size must be 64, 128, 256, 512, or 1024 KiB"));
    }
    if options.compressed_chunk_bytes != 0
        && (!VALID_CHUNK_SIZES.contains(&options.compressed_chunk_bytes)
            || options.compressed_chunk_bytes > options.chunk_size_bytes)
    {
        return Err(invalid(
            "compressed chunk size must be 0 or a valid chunk size up to the chunk size",
        ));
    }
    if options.compression_mode == CompressionMode::ZstandardAuto
        && !(1..=13).contains(&options.zstd_compression_level)
    {
        return Err(invalid(
            "Zstandard compression level must be from 1 through 13",
        ));
    }
    if options.min_compression_savings_bp > 5000 {
        return Err(invalid(
            "minimum compression savings cannot exceed 5000 basis points",
        ));
    }
    let codec_memory = if options.compression_mode == CompressionMode::ZstandardAuto {
        ZSTD_CONTEXT_MEMORY_ESTIMATE
            + zstd::zstd_safe::compress_bound(options.chunk_size_bytes as usize) as u64
            + if options.verify_output_payloads {
                ZSTD_DECODER_MEMORY_ESTIMATE
            } else {
                0
            }
    } else {
        0
    };
    if options.max_memory_bytes < options.chunk_size_bytes as u64 * 2 + codec_memory {
        return Err(invalid(
            "memory limit is too small for the configured chunk and codec buffers",
        ));
    }
    if options.max_files == 0
        || options.max_files > 33_554_432
        || options.max_directories == 0
        || options.max_directories > 16_777_216
        || options.max_entries == 0
        || options.max_entries > 50_331_648
        || options.max_path_component_bytes == 0
        || options.max_path_component_bytes > 4096
        || options.max_file_size == 0
        || options.max_file_size > MAX_ARCHIVE_SIZE
        || options.max_archive_size < HEADER_SIZE
        || options.max_archive_size > MAX_ARCHIVE_SIZE
    {
        return Err(invalid(
            "writer limits are zero or exceed the NX 0.1 normative limits",
        ));
    }
    Ok(())
}

#[derive(Default)]
struct InputCounts {
    file_entries: u64,
    directories: u64,
    entries: u64,
    input_bytes: u64,
    path_bytes: u64,
    estimated: u64,
    adaptive_index_growth: u64,
    maximum_depth: u64,
}
type SnapshotSortKey = (Option<usize>, u8, u32, [u8; 16], [u8; 32]);
fn count_tree(
    dir: &mut DirectoryInput,
    depth: usize,
    options: &WriterOptions,
    counts: &mut InputCounts,
) -> Result<(), WriterError> {
    cancelled(options)?;
    counts.maximum_depth = counts.maximum_depth.max(depth as u64 + 1);
    if depth > MAX_DIRECTORY_DEPTH {
        return Err(limit_error("directory nesting exceeds 256 levels"));
    }
    counts.directories = counts
        .directories
        .checked_add(1)
        .ok_or_else(|| limit_error("directory count overflow"))?;
    counts.estimated = counts
        .estimated
        .checked_add(64)
        .ok_or_else(|| limit_error("memory estimate overflow"))?;
    dir.entries
        .sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let mut prior: Option<&[u8]> = None;
    for entry in &mut dir.entries {
        cancelled(options)?;
        let name = entry.name.as_bytes();
        if name.is_empty()
            || name.len() > options.max_path_component_bytes as usize
            || name.len() > 4096
            || name.contains(&0)
            || name.contains(&b'/')
            || name.contains(&b'\\')
            || name == b"."
            || name == b".."
        {
            return Err(invalid(
                "path component is empty, too long, or contains a forbidden value",
            ));
        }
        if let Some(p) = prior {
            if p == name {
                return Err(invalid("duplicate name in one directory"));
            }
        }
        prior = Some(name);
        counts.entries = counts
            .entries
            .checked_add(1)
            .ok_or_else(|| limit_error("entry count overflow"))?;
        counts.path_bytes = counts
            .path_bytes
            .checked_add(name.len() as u64)
            .ok_or_else(|| limit_error("path pool size overflow"))?;
        counts.estimated = counts
            .estimated
            .checked_add(64 + name.len() as u64)
            .ok_or_else(|| limit_error("memory estimate overflow"))?;
        match &mut entry.node {
            NodeInput::Directory(child) => count_tree(child, depth + 1, options, counts)?,
            NodeInput::File(file) => {
                if !file.read_profile.fits(file.size) {
                    return Err(invalid("read profile exceeds the declared file extent"));
                }
                counts.estimated = counts
                    .estimated
                    .checked_add(std::mem::size_of::<FileReadProfile>() as u64)
                    .ok_or_else(|| limit_error("read-profile memory estimate overflow"))?;
                counts.file_entries = counts
                    .file_entries
                    .checked_add(1)
                    .ok_or_else(|| limit_error("file count overflow"))?;
                if options.compression_mode == CompressionMode::RawOnly
                    && file.size > options.max_archive_size
                {
                    return Err(limit_error(
                        "RAW file cannot fit within configured archive size limit",
                    ));
                }
                if file.size > options.max_file_size {
                    return Err(limit_error("file exceeds configured logical size limit"));
                }
                if file.size > MAX_ARCHIVE_SIZE {
                    return Err(limit_error("file exceeds NX 0.1 logical size limit"));
                }
                let blocks = if file.size == 0 {
                    0
                } else {
                    1 + (file.size - 1)
                        / file_chunk_size(file.size, file.read_profile, options) as u64
                };
                if options.block_sizing == BlockSizing::Adaptive && file.size != 0 {
                    let baseline =
                        1 + (file.size - 1) / RAW_CHUNK_SIZE.min(options.chunk_size_bytes) as u64;
                    // 96 bytes covers a block descriptor/digest/reference and
                    // reader-side index bookkeeping; allocator RSS is separate.
                    counts.adaptive_index_growth = counts
                        .adaptive_index_growth
                        .checked_add(blocks.saturating_sub(baseline).saturating_mul(96))
                        .ok_or_else(|| limit_error("adaptive index estimate overflow"))?;
                }
                if blocks > u32::MAX as u64 {
                    return Err(limit_error(
                        "one file exceeds the NX block-reference count limit",
                    ));
                }
                // An alias is not read: its bytes are the earlier file's.
                if !matches!(file.share, Some(crate::model::FileShare::Alias(_))) {
                    counts.input_bytes = counts
                        .input_bytes
                        .checked_add(file.size)
                        .ok_or_else(|| limit_error("input byte count overflow"))?;
                }
                counts.estimated = counts
                    .estimated
                    .checked_add(128)
                    .ok_or_else(|| limit_error("memory estimate overflow"))?;
            }
        }
    }
    if counts.directories > options.max_directories as u64
        || counts.entries > options.max_entries as u64
        || counts.file_entries > options.max_files as u64
    {
        return Err(limit_error(
            "input tree count exceeds configured writer limit",
        ));
    }
    Ok(())
}

fn check_optional_text(value: &Option<String>, max: usize, what: &str) -> Result<(), WriterError> {
    if let Some(s) = value {
        let b = s.as_bytes();
        if b.is_empty() || b.len() > max || b.contains(&0) {
            return Err(invalid(format!(
                "{what} is empty or exceeds its byte limit"
            )));
        }
    }
    Ok(())
}
fn valid_language(value: &str) -> bool {
    let b = value.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b.split(|x| *x == b'-').all(|part| {
            !part.is_empty()
                && part.len() <= 8
                && part
                    .iter()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}
fn media_is_icon(t: MediaType) -> bool {
    matches!(t, MediaType::PngIcon | MediaType::JpegIcon)
}
fn validate_media(m: &MediaInput, expected: Option<MediaType>) -> Result<(), WriterError> {
    if m.size == 0 || m.size > MAX_MEDIA_PAYLOAD {
        return Err(limit_error(
            "native media payload must be between 1 byte and 64 MiB",
        ));
    }
    if let Some(t) = expected {
        if m.media_type != t {
            return Err(invalid(
                "native media type does not match its Catalog field",
            ));
        }
    }
    Ok(())
}
fn validate_metadata(
    m: &mut SnapshotMetadata,
    counts: &mut InputCounts,
) -> Result<(), WriterError> {
    if !m.has_any() {
        return Err(invalid(
            "empty snapshot metadata object is not a source claim",
        ));
    }
    check_optional_text(&m.title, 4096, "title")?;
    check_optional_text(&m.publisher, 4096, "publisher")?;
    if let Some(media) = &m.icon {
        validate_media(media, None)?;
        if !media_is_icon(media.media_type) {
            return Err(invalid("default icon must be PNG or JPEG"));
        }
    }
    if let Some(media) = &m.nacp {
        validate_media(media, Some(MediaType::Nacp))?;
    }
    m.localized
        .sort_by(|a, b| a.language.as_bytes().cmp(b.language.as_bytes()));
    let mut prior: Option<&[u8]> = None;
    for l in &m.localized {
        if !valid_language(&l.language) {
            return Err(invalid(
                "localized language tag is invalid or not lowercase BCP 47 bytes",
            ));
        }
        if prior == Some(l.language.as_bytes()) {
            return Err(invalid("duplicate localized language tag in one snapshot"));
        }
        prior = Some(l.language.as_bytes());
        check_optional_text(&l.title, 4096, "localized title")?;
        if l.title.is_none() && l.icon.is_none() {
            return Err(invalid(
                "localized metadata entry has neither a title nor an icon",
            ));
        }
        if let Some(icon) = &l.icon {
            validate_media(icon, None)?;
            if !media_is_icon(icon.media_type) {
                return Err(invalid("localized icon must be PNG or JPEG"));
            }
        }
        counts.estimated = counts
            .estimated
            .checked_add(
                48 + l.language.len() as u64 + l.title.as_ref().map_or(0, |s| s.len() as u64),
            )
            .ok_or_else(|| limit_error("memory estimate overflow"))?;
    }
    for media in &m.opaque {
        validate_media(media, Some(MediaType::Opaque))?;
    }
    counts.estimated = counts
        .estimated
        .checked_add(96 + m.opaque.len() as u64 * 8)
        .ok_or_else(|| limit_error("memory estimate overflow"))?;
    Ok(())
}

fn nonzero16(v: &Option<[u8; 16]>, what: &str) -> Result<(), WriterError> {
    if v.is_some_and(|x| x == [0; 16]) {
        Err(invalid(format!(
            "{what} cannot be present with an all-zero identifier"
        )))
    } else {
        Ok(())
    }
}
fn nonzero32(v: &Option<[u8; 32]>, what: &str) -> Result<(), WriterError> {
    if v.is_some_and(|x| x == [0; 32]) {
        Err(invalid(format!(
            "{what} cannot be present with an all-zero identifier"
        )))
    } else {
        Ok(())
    }
}

fn preflight(
    input: &mut ArchiveInput,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
) -> Result<InputCounts, WriterError> {
    cancelled(options)?;
    if input.applications.is_empty() || input.applications.len() > 4096 {
        return Err(limit_error("archive must contain 1..4096 applications"));
    }
    let mut counts = InputCounts::default();
    input
        .applications
        .sort_by_key(|a| (a.platform as u8, a.application_id));
    let mut previous_app = None;
    for app in &mut input.applications {
        cancelled(options)?;
        if app.application_id == 0 {
            return Err(invalid("application ID must be nonzero"));
        }
        let key = (app.platform as u8, app.application_id);
        if previous_app == Some(key) {
            return Err(invalid("duplicate (platform, application ID)"));
        }
        previous_app = Some(key);
        if app.snapshots.is_empty() {
            return Err(invalid(
                "each application must contain at least one complete snapshot",
            ));
        }
        for program in &mut app.programs {
            if let Some(npdm) = program.npdm.take() {
                let mut matching = app
                    .snapshots
                    .iter_mut()
                    .filter(|s| s.program_id == Some(program.program_id));
                let snapshot = matching
                    .next()
                    .ok_or_else(|| invalid("Program NPDM has no snapshot"))?;
                if matching.next().is_some() || snapshot.native.npdm.is_some() {
                    return Err(invalid(
                        "Program NPDM is ambiguous; supply NPDM on each exact snapshot",
                    ));
                }
                snapshot.native.npdm = Some(npdm);
            }
        }
        app.programs.sort_by_key(|p| p.program_id);
        if app.programs.len() > 16384 {
            return Err(limit_error("program count exceeds NX 0.1 limit"));
        }
        let mut previous_program = None;
        for p in &mut app.programs {
            if p.program_id == 0 {
                return Err(invalid("Program ID must be source-provided and nonzero"));
            }
            if previous_program == Some(p.program_id) {
                return Err(invalid("duplicate Program ID within application"));
            }
            previous_program = Some(p.program_id);
            nonzero16(&p.content_id, "Program Content ID")?;
            if let Some(m) = &p.npdm {
                validate_media(m, Some(MediaType::Npdm))?;
            }
            counts.estimated = counts
                .estimated
                .checked_add(160)
                .ok_or_else(|| limit_error("memory estimate overflow"))?;
        }
        let positions: HashMap<u64, usize> = app
            .programs
            .iter()
            .enumerate()
            .map(|(i, p)| (p.program_id, i))
            .collect();
        let mut any_metadata = false;
        for s in &mut app.snapshots {
            cancelled(options)?;
            if s.snapshot_type != SnapshotType::Dlc {
                if let Some(fallback) = &options.display_fallback {
                    let metadata = s.metadata.get_or_insert_with(|| SnapshotMetadata {
                        title: None,
                        publisher: None,
                        icon: None,
                        nacp: None,
                        localized: Vec::new(),
                        opaque: Vec::new(),
                    });
                    let mut supplied = Vec::new();
                    if metadata
                        .title
                        .as_ref()
                        .map_or(true, |t| t.trim().is_empty())
                    {
                        if let Some(title) = &fallback.title {
                            check_optional_text(&Some(title.clone()), 4096, "fallback title")?;
                            if !title.trim().is_empty() {
                                metadata.title = Some(title.trim().to_owned());
                                supplied.push("title");
                            }
                        }
                    }
                    if metadata.icon.is_none() {
                        if let Some((kind, bytes)) = &fallback.icon {
                            if !matches!(kind, MediaType::PngIcon | MediaType::JpegIcon)
                                || bytes.len() > 16 * 1024 * 1024
                            {
                                return Err(invalid(
                                    "fallback icon must be PNG or JPEG, at most 16 MiB",
                                ));
                            }
                            metadata.icon = Some(MediaInput::new(
                                *kind,
                                bytes.len() as u64,
                                io::Cursor::new(bytes.clone()),
                            ));
                            supplied.push("icon");
                        }
                    }
                    if !supplied.is_empty() {
                        metadata.opaque.push(MediaInput::from_bytes(
                            MediaType::Opaque,
                            format!("nxformat:user-display:v1\n{}\n", supplied.join(","))
                                .into_bytes(),
                        ));
                    }
                    if !metadata.has_any() {
                        s.metadata = None;
                    }
                }
            }
            if let Some(npdm) = &s.native.npdm {
                if s.program_id.is_none() {
                    return Err(invalid("snapshot NPDM requires a real Program ID"));
                }
                validate_media(npdm, Some(MediaType::Npdm))?;
            }
            nonzero16(&s.content_id, "Snapshot Content ID")?;
            if let Some(addon) = s.addon_title_id {
                // NUP and extracted add-ons have a title ID but no NCA.
                if s.snapshot_type != SnapshotType::Dlc
                    || s.program_id.is_some()
                    || s.content_id.is_some()
                    || addon == 0
                {
                    return Err(invalid(
                        "an add-on title ID needs a DLC with no Program ID and no Content ID",
                    ));
                }
            }
            nonzero32(&s.build_id, "Snapshot Build ID")?;
            check_optional_text(&s.display_version, 256, "display version")?;
            if let Some(program_id) = s.program_id {
                if program_id == 0 || !positions.contains_key(&program_id) {
                    return Err(invalid(
                        "snapshot names a Program ID absent from its application",
                    ));
                }
            } else if s.snapshot_type != SnapshotType::Dlc {
                return Err(invalid("only DLC may omit a source-provided Program ID"));
            }
            if let Some(metadata) = &mut s.metadata {
                validate_metadata(metadata, &mut counts)?;
                any_metadata = true;
            }
            s.dependencies.sort_by_key(|d| {
                (
                    d.kind as u8,
                    d.target_snapshot_identity,
                    d.content_id.unwrap_or([0; 16]),
                )
            });
            for d in &s.dependencies {
                if d.target_snapshot_identity == [0; 32] {
                    return Err(invalid(
                        "dependency target snapshot identity cannot be zero",
                    ));
                }
                nonzero16(&d.content_id, "dependency Content ID")?;
            }
            if s.dependencies.windows(2).any(|w| {
                w[0].kind == w[1].kind
                    && w[0].target_snapshot_identity == w[1].target_snapshot_identity
                    && w[0].content_id == w[1].content_id
            }) {
                return Err(invalid("duplicate snapshot dependency"));
            }
            counts.estimated = counts
                .estimated
                .checked_add(192 + s.dependencies.len() as u64 * 64)
                .ok_or_else(|| limit_error("memory estimate overflow"))?;
            counts.estimated = counts
                .estimated
                .checked_add(128)
                .ok_or_else(|| limit_error("memory estimate overflow"))?;
        }
        app.snapshots.sort_by(|a, b| {
            let ap = a.program_id.and_then(|id| positions.get(&id).copied());
            let bp = b.program_id.and_then(|id| positions.get(&id).copied());
            match (ap, bp) {
                (Some(x), Some(y)) => x
                    .cmp(&y)
                    .then((a.snapshot_type as u8).cmp(&(b.snapshot_type as u8)))
                    .then(a.numeric_version.cmp(&b.numeric_version))
                    .then(stored_content_id(a).cmp(&stored_content_id(b)))
                    .then(
                        a.build_id
                            .unwrap_or([0; 32])
                            .cmp(&b.build_id.unwrap_or([0; 32])),
                    ),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a
                    .numeric_version
                    .cmp(&b.numeric_version)
                    .then(stored_content_id(a).cmp(&stored_content_id(b)))
                    .then(
                        a.build_id
                            .unwrap_or([0; 32])
                            .cmp(&b.build_id.unwrap_or([0; 32])),
                    ),
            }
        });
        let mut prior_key: Option<SnapshotSortKey> = None;
        let mut program_seen = vec![false; app.programs.len()];
        for s in &mut app.snapshots {
            cancelled(options)?;
            if let Some(npdm) = &s.native.npdm {
                if s.program_id.is_none() {
                    return Err(invalid("snapshot NPDM requires a real Program ID"));
                }
                validate_media(npdm, Some(MediaType::Npdm))?;
            }
            let pos = s.program_id.and_then(|id| positions.get(&id).copied());
            if let Some(i) = pos {
                program_seen[i] = true;
            }
            let key = (
                pos,
                s.snapshot_type as u8,
                s.numeric_version,
                stored_content_id(s),
                s.build_id.unwrap_or([0; 32]),
            );
            if prior_key == Some(key) {
                return Err(invalid("duplicate snapshot sort key within application"));
            }
            prior_key = Some(key);
            count_tree(&mut s.root, 0, options, &mut counts)?;
            if s.dependencies.len() > 32768 {
                return Err(limit_error(
                    "one snapshot dependency list exceeds Catalog limit",
                ));
            }
        }
        if program_seen.iter().any(|v| !*v) {
            return Err(invalid("Program row has no associated snapshot"));
        }
        if any_metadata {
            let has_non_dlc = app
                .snapshots
                .iter()
                .any(|s| s.snapshot_type != SnapshotType::Dlc && s.metadata.is_some());
            if !has_non_dlc {
                return Err(invalid("DLC-only metadata cannot be represented without a selected non-DLC Catalog source"));
            }
        }
        counts.estimated = counts
            .estimated
            .checked_add(app.application_id.to_le_bytes().len() as u64 + 128)
            .ok_or_else(|| limit_error("memory estimate overflow"))?;
    }
    let deps: u64 = input
        .applications
        .iter()
        .flat_map(|a| &a.snapshots)
        .map(|s| s.dependencies.len() as u64)
        .sum();
    if deps > 32768 {
        return Err(limit_error("archive dependency count exceeds NX 0.1 limit"));
    }
    if counts.estimated > options.max_memory_bytes {
        return Err(limit_error(
            "input metadata estimate exceeds configured memory limit",
        ));
    }
    budget.charge(
        counts.estimated,
        "caller input model and validated tree metadata",
    )?;
    budget.charge(
        RAW_CHUNK_SIZE as u64 * 2 + 64 * 1024 * 2,
        "bounded streaming and compare buffers",
    )?;
    Ok(counts)
}

struct TempEntry {
    name: String,
    target: TempTarget,
}
enum TempTarget {
    Directory(u32),
    SourceFile(u32),
}
struct TempDirectory {
    entries: Vec<TempEntry>,
}
struct SourceFile {
    size: u64,
    chunk_size_bytes: u32,
    reader: Box<dyn Read>,
    path: Vec<u8>,
    expected_digest: Option<[u8; 32]>,
    warm: Option<WarmUp>,
    share: Option<crate::model::FileShare>,
}
/// Reads the next small files in the background while the writer handles the
/// current one. Many small files are slow to read one at a time (first-read
/// latency per file): an extracted Switch 2 game read at 32 MB/s with one
/// thread and 565 MB/s with 32. The writer still does the real, checked read;
/// the threads only bring the bytes into the system cache, at most
/// `WINDOW` bytes ahead of the writer.
struct ReadAhead {
    jobs: Option<std::sync::mpsc::Sender<(usize, WarmUp)>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    position: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    next: usize,
    scheduled: u64,
    consumed: u64,
}
impl ReadAhead {
    const THREADS: usize = 16;
    const WINDOW: u64 = 128 * 1024 * 1024;
    /// Larger files already read fast sequentially.
    const LARGEST: u64 = 16 * 1024 * 1024;
    fn new() -> Self {
        Self {
            jobs: None,
            workers: Vec::new(),
            position: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            next: 0,
            scheduled: 0,
            consumed: 0,
        }
    }
    fn start(&mut self) {
        let (send, receive) = std::sync::mpsc::channel::<(usize, WarmUp)>();
        let receive = Arc::new(std::sync::Mutex::new(receive));
        for _ in 0..Self::THREADS {
            let receive = receive.clone();
            let position = self.position.clone();
            let stop = self.stop.clone();
            self.workers.push(std::thread::spawn(move || loop {
                let job = receive.lock().map(|r| r.recv());
                let Ok(Ok((index, warm))) = job else { return };
                // Skip files the writer has already reached.
                if !stop.load(Ordering::Relaxed) && index as u64 >= position.load(Ordering::Relaxed)
                {
                    warm();
                }
            }));
        }
        self.jobs = Some(send);
    }
    /// The writer is about to read `order[at]`: schedule the files after it.
    fn advance(&mut self, at: usize, order: &[usize], files: &[SourceFile]) {
        self.position.store(at as u64, Ordering::Relaxed);
        if self.next <= at {
            self.next = at + 1;
        }
        let window_end = self.consumed + Self::WINDOW;
        while self.next < order.len() && self.scheduled < window_end {
            let file = &files[order[self.next]];
            if let Some(warm) = file.warm.as_ref().filter(|_| file.size <= Self::LARGEST) {
                if self.jobs.is_none() {
                    self.start();
                }
                let _ = self
                    .jobs
                    .as_ref()
                    .expect("read-ahead started")
                    .send((self.next, warm.clone()));
                self.scheduled += file.size;
            }
            self.next += 1;
        }
    }
    fn finished(&mut self, file: &SourceFile) {
        if file.warm.is_some() && file.size <= Self::LARGEST {
            self.consumed += file.size;
        }
    }
}
impl Drop for ReadAhead {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.jobs = None;
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
struct IdentityNode {
    path: Vec<u8>,
    kind: u8,
    source_file: Option<u32>,
}
struct PreparedTree {
    directories: Vec<TempDirectory>,
    files: Vec<SourceFile>,
    nodes: Vec<IdentityNode>,
}

fn full_path(parent: &[u8], name: &str) -> Result<Vec<u8>, WriterError> {
    let len = parent
        .len()
        .checked_add(if parent.is_empty() { 0 } else { 1 })
        .and_then(|n| n.checked_add(name.len()))
        .ok_or_else(|| limit_error("logical path length overflow"))?;
    let mut path = Vec::new();
    path.try_reserve_exact(len)
        .map_err(|_| limit_error("unable to reserve path memory"))?;
    path.extend_from_slice(parent);
    if !parent.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name.as_bytes());
    Ok(path)
}
fn flatten_directory(
    dir: DirectoryInput,
    parent: &[u8],
    depth: usize,
    tree: &mut PreparedTree,
    budget: &mut MemoryBudget,
    options: &WriterOptions,
) -> Result<u32, WriterError> {
    cancelled(options)?;
    if depth > MAX_DIRECTORY_DEPTH {
        return Err(limit_error("directory nesting exceeds 256 levels"));
    }
    let local = u32::try_from(tree.directories.len())
        .map_err(|_| limit_error("directory index exceeds u32"))?;
    tree.directories
        .try_reserve(1)
        .map_err(|_| limit_error("unable to reserve directory table memory"))?;
    tree.directories.push(TempDirectory {
        entries: Vec::new(),
    });
    budget.charge(64, "directory record")?;
    let mut root = false;
    if parent.is_empty() && tree.nodes.is_empty() {
        root = true;
        tree.nodes.push(IdentityNode {
            path: Vec::new(),
            kind: 1,
            source_file: None,
        });
        budget.charge(64, "root identity record")?;
    }
    let mut entries = dir.entries;
    entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    for entry in entries {
        cancelled(options)?;
        let path = full_path(parent, &entry.name)?;
        let target = match entry.node {
            NodeInput::Directory(child) => {
                tree.nodes.push(IdentityNode {
                    path: path.clone(),
                    kind: 1,
                    source_file: None,
                });
                budget.charge(64 + path.len() as u64, "directory identity path")?;
                TempTarget::Directory(flatten_directory(
                    child,
                    &path,
                    depth + 1,
                    tree,
                    budget,
                    options,
                )?)
            }
            NodeInput::File(file) => {
                let source = u32::try_from(tree.files.len())
                    .map_err(|_| limit_error("snapshot file source index exceeds u32"))?;
                tree.files
                    .try_reserve(1)
                    .map_err(|_| limit_error("unable to reserve source-file table memory"))?;
                tree.files.push(SourceFile {
                    size: file.size,
                    chunk_size_bytes: input_chunk_size(&file, options),
                    expected_digest: file.captured.as_ref().map(|p| p.sha256),
                    reader: file.reader,
                    path: path.clone(),
                    warm: file.warm,
                    share: file.share,
                });
                tree.nodes.push(IdentityNode {
                    path: path.clone(),
                    kind: 2,
                    source_file: Some(source),
                });
                budget.charge(
                    96 + path.len() as u64,
                    "file identity path and source record",
                )?;
                TempTarget::SourceFile(source)
            }
        };
        tree.directories[local as usize].entries.push(TempEntry {
            name: entry.name,
            target,
        });
        budget.charge(48, "directory entry")?;
    }
    let _ = root;
    Ok(local)
}

struct BlockBuild {
    logical_size: u32,
    stored_size: u32,
    codec: u8,
    /// First entry of this block in the slice-CRC integrity table.
    first_check: u32,
    /// Final archive offset; zero until the block has been written.
    data_offset: u64,
}

/// The archive temporary file. Blocks are written once, in ID order,
/// straight into the Data region; no staging spool is copied afterwards.
struct Output {
    file: BufWriter<File>,
    position: u64,
    /// Number of leading block IDs already written.
    written: u32,
    /// CRC-32C of each stored payload, for the publication readback.
    stored_crcs: Vec<u32>,
}

impl Output {
    fn new(file: File) -> Self {
        Self {
            file: BufWriter::with_capacity(1024 * 1024, file),
            position: 0,
            written: 0,
            stored_crcs: Vec::new(),
        }
    }

    fn write(&mut self, bytes: &[u8], options: &WriterOptions) -> Result<(), WriterError> {
        write_bytes(&mut self.file, bytes, &mut self.position, options)
    }

    fn pad(&mut self, alignment: u64, options: &WriterOptions) -> Result<(), WriterError> {
        write_padding(&mut self.file, &mut self.position, alignment, options)
    }

    fn write_block(
        &mut self,
        id: u32,
        block: &mut BlockBuild,
        codec: u8,
        stored: &[u8],
        options: &WriterOptions,
        timings: &mut WriterTimings,
    ) -> Result<(), WriterError> {
        if id != self.written {
            return Err(WriterError::new(
                WriterErrorKind::Integrity,
                "blocks were not written in ID order",
            ));
        }
        let started = Instant::now();
        self.pad(16, options)?;
        check_temporary_size(
            self.position
                .checked_add(stored.len() as u64)
                .ok_or_else(|| limit_error("temporary disk accounting overflow"))?,
            options,
        )?;
        block.data_offset = self.position;
        block.codec = codec;
        block.stored_size =
            u32::try_from(stored.len()).map_err(|_| limit_error("stored block exceeds u32"))?;
        self.write(stored, options)?;
        if options.verify_output_payloads {
            self.stored_crcs.push(crc32c(stored));
        }
        self.written += 1;
        timings.block_staging_write += started.elapsed();
        Ok(())
    }

    /// Read already written bytes back (exact dedup comparison).
    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<(), WriterError> {
        let end = offset
            .checked_add(bytes.len() as u64)
            .filter(|&end| end <= self.position)
            .ok_or_else(|| {
                WriterError::new(WriterErrorKind::Integrity, "read past written data")
            })?;
        let buffered = self.file.buffer().len() as u64;
        let on_disk = self.position - buffered;
        if offset >= on_disk {
            // Still in the write buffer: copy it, no flush or syscall.
            let at = (offset - on_disk) as usize;
            bytes.copy_from_slice(&self.file.buffer()[at..at + bytes.len()]);
            return Ok(());
        }
        if end > on_disk {
            // Straddles disk and buffer (rare): make it all disk bytes.
            self.file.flush()?;
        }
        let file = self.file.get_mut();
        #[cfg(unix)]
        {
            // Positional IO leaves the buffered writer's cursor untouched.
            use std::os::unix::fs::FileExt;
            file.read_exact_at(bytes, offset)?;
        }
        #[cfg(not(unix))]
        {
            // The OS cursor must stay where the buffered bytes will be written.
            let resume = file.stream_position()?;
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(bytes)?;
            file.seek(SeekFrom::Start(resume))?;
        }
        Ok(())
    }
}

/// Decode a frame and compare it with its input before it is published.
fn verify_frame(
    frame: &[u8],
    input: &[u8],
    decoder: &mut Option<zstd::bulk::Decompressor<'static>>,
    scratch: &mut Vec<u8>,
) -> Result<(), WriterError> {
    if decoder.is_none() {
        let mut d = zstd::bulk::Decompressor::new()?;
        d.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
        *decoder = Some(d);
    }
    // The decoder overwrites the buffer; only newly grown bytes get zeroed.
    if scratch.len() < input.len() {
        scratch
            .try_reserve(input.len() - scratch.len())
            .map_err(|_| limit_error("unable to reserve frame verification buffer"))?;
    }
    scratch.resize(input.len(), 0);
    let decoded = decoder
        .as_mut()
        .expect("decoder was initialized")
        .decompress_to_buffer(frame, &mut scratch[..])
        .map_err(|_| {
            WriterError::new(
                WriterErrorKind::Integrity,
                "unable to decode a freshly compressed block",
            )
        })?;
    if decoded != input.len() || scratch[..] != *input {
        return Err(WriterError::new(
            WriterErrorKind::Integrity,
            "Zstandard frame does not round-trip to its input",
        ));
    }
    Ok(())
}

/// Everything that receives block bytes during tree processing.
struct DataSink {
    out: Output,
    /// Slice CRC-32C integrity table, in block order.
    checks: Vec<u32>,
    pipeline: Option<parallel::Pipeline>,
    decoder: Option<zstd::bulk::Decompressor<'static>>,
    frame: Vec<u8>,
    compare: Vec<u8>,
}

impl DataSink {
    /// Exact byte comparison of a new chunk with an existing block.
    fn block_equals(
        &mut self,
        id: u32,
        bytes: &[u8],
        blocks: &mut [BlockBuild],
        options: &WriterOptions,
        timings: &mut WriterTimings,
    ) -> Result<bool, WriterError> {
        if id >= self.out.written {
            if let Some(pipeline) = self.pipeline.as_mut() {
                pipeline.drain_through(id, &mut self.out, blocks, options, timings)?;
            }
        }
        let block = &blocks[id as usize];
        if id >= self.out.written || block.logical_size as usize != bytes.len() {
            return Err(WriterError::new(
                WriterErrorKind::Integrity,
                "dedup candidate was not written",
            ));
        }
        let (offset, stored, codec) = (block.data_offset, block.stored_size as usize, block.codec);
        // Reads and decodes overwrite these buffers; only grown bytes get zeroed.
        if self.compare.len() < bytes.len() {
            self.compare
                .try_reserve(bytes.len() - self.compare.len())
                .map_err(|_| limit_error("unable to reserve dedup comparison buffer"))?;
        }
        self.compare.resize(bytes.len(), 0);
        if codec == 0 {
            let mut compare = std::mem::take(&mut self.compare);
            self.out.read_at(offset, &mut compare)?;
            let equal = compare[..] == *bytes;
            self.compare = compare;
            return Ok(equal);
        }
        if self.frame.len() < stored {
            self.frame
                .try_reserve(stored - self.frame.len())
                .map_err(|_| limit_error("unable to reserve dedup frame buffer"))?;
        }
        self.frame.resize(stored, 0);
        let mut frame = std::mem::take(&mut self.frame);
        self.out.read_at(offset, &mut frame)?;
        if self.decoder.is_none() {
            let mut d = zstd::bulk::Decompressor::new()?;
            d.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
            self.decoder = Some(d);
        }
        let decoded = self
            .decoder
            .as_mut()
            .expect("decoder was initialized")
            .decompress_to_buffer(&frame[..], &mut self.compare[..])
            .map_err(|_| {
                WriterError::new(WriterErrorKind::Integrity, "unable to decode written block")
            })?;
        self.frame = frame;
        Ok(decoded == bytes.len() && self.compare[..] == *bytes)
    }

    fn finish(
        &mut self,
        blocks: &mut [BlockBuild],
        options: &WriterOptions,
        timings: &mut WriterTimings,
    ) -> Result<(), WriterError> {
        if let Some(mut pipeline) = self.pipeline.take() {
            pipeline.drain_through(u32::MAX, &mut self.out, blocks, options, timings)?;
        }
        if self.out.written as usize != blocks.len() {
            return Err(WriterError::new(
                WriterErrorKind::Integrity,
                "not every block reached the archive",
            ));
        }
        Ok(())
    }
}

/// Candidate-lookup key from the slice CRCs; equality is decided by bytes.
fn checks_key(checks: &[u32]) -> u64 {
    let mut h = 0x243f_6a88_85a3_08d3u64 ^ checks.len() as u64;
    for &c in checks {
        h = (h ^ c as u64)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .rotate_left(29);
    }
    h
}

struct BlockEncoder {
    compressor: zstd::bulk::Compressor<'static>,
    frame: Vec<u8>,
}

impl BlockEncoder {
    fn new(options: &WriterOptions, budget: &mut MemoryBudget) -> Result<Self, WriterError> {
        let frame_bound = zstd::zstd_safe::compress_bound(options.chunk_size_bytes as usize);
        budget.charge(
            ZSTD_CONTEXT_MEMORY_ESTIMATE + frame_bound as u64,
            "bounded Zstandard context and frame buffer",
        )?;
        let mut compressor = zstd::bulk::Compressor::new(options.zstd_compression_level)?;
        let window_log = options.chunk_size_bytes.trailing_zeros();
        compressor.set_parameter(zstd::zstd_safe::CParameter::WindowLog(window_log))?;
        compressor.set_parameter(zstd::zstd_safe::CParameter::ContentSizeFlag(true))?;
        compressor.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(false))?;
        compressor.set_parameter(zstd::zstd_safe::CParameter::DictIdFlag(false))?;
        let mut frame = Vec::new();
        frame
            .try_reserve_exact(frame_bound)
            .map_err(|_| limit_error("unable to reserve bounded Zstandard frame buffer"))?;
        Ok(Self { compressor, frame })
    }

    /// Returns the frame length, or `bytes.len()` (a RAW decision) when the
    /// cheap predictors show the block cannot reach the savings threshold.
    fn encode(
        &mut self,
        bytes: &[u8],
        options: &WriterOptions,
        timings: &mut WriterTimings,
    ) -> Result<usize, WriterError> {
        cancelled(options)?;
        let started = Instant::now();
        let compressor = &mut self.compressor;
        let frame = &mut self.frame;
        let skip = predicts_low_gain(bytes, options, |level, input| {
            compressor.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(level))?;
            compressor.set_pledged_src_size(Some(input.len() as u64))?;
            frame.clear();
            Ok(compressor.compress_to_buffer(input, frame)?)
        })?;
        self.compressor
            .set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(
                options.zstd_compression_level,
            ))?;
        if skip {
            timings.compression += started.elapsed();
            return Ok(bytes.len());
        }
        self.compressor
            .set_pledged_src_size(Some(bytes.len() as u64))?;
        self.frame.clear();
        let written = self.compressor.compress_to_buffer(bytes, &mut self.frame)?;
        timings.compression += started.elapsed();
        timings.compression_context_peak = timings
            .compression_context_peak
            .max(self.compressor.context_mut().sizeof() as u64);
        #[cfg(test)]
        CANCEL_DURING_COMPRESSION_TEST.with(|flag| {
            if flag.replace(false) {
                options.cancellation.cancel();
            }
        });
        cancelled(options)?;
        if written != self.frame.len() {
            return Err(WriterError::new(
                WriterErrorKind::Integrity,
                "Zstandard output length disagrees with its reusable buffer",
            ));
        }
        Ok(written)
    }
}

#[derive(Default)]
struct WriterTimings {
    compression_context_peak: u64,
    input_read_hash: Duration,
    input_read: Duration,
    input_hash: Duration,
    block_staging_write: Duration,
    compression_stage: Duration,
    compression_staging_read: Duration,
    compression_staging_read_bytes: u64,
    compression_queue_peak: u64,
    global_dedup: Duration,
    compression: Duration,
    archive_write: Duration,
    output_verification: Duration,
    output_sync: Duration,
    atomic_publication: Duration,
}

#[cfg(test)]
thread_local! {
    static CANCEL_DURING_COMPRESSION_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CANCEL_DURING_VERIFICATION_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[derive(Clone, Copy, Hash, Eq, PartialEq)]
struct BlockKey {
    logical_size: u32,
    digest: u64,
}
struct BlockBucket {
    first_id: u32,
    collision_ids: Vec<u32>,
}
#[derive(Default)]
struct BlockDedupIndex {
    buckets: HashMap<BlockKey, BlockBucket>,
    estimated_bytes: u64,
}
impl BlockDedupIndex {
    fn charge(
        &mut self,
        amount: u64,
        options: &WriterOptions,
        budget: &mut MemoryBudget,
    ) -> Result<(), WriterError> {
        let next = self
            .estimated_bytes
            .checked_add(amount)
            .ok_or_else(|| limit_error("deduplication index accounting overflow"))?;
        if next > options.max_dedup_index_bytes {
            return Err(limit_error(
                "estimated deduplication index exceeds configured limit",
            ));
        }
        budget.charge(amount, "global block-deduplication index")?;
        self.estimated_bytes = next;
        Ok(())
    }
}
#[derive(Clone)]
struct FileBuild {
    size: u64,
    digest: [u8; 32],
    chunk_size_bytes: u32,
    first_ref: u32,
    block_count: u32,
}
struct DirectoryBuild {
    entries: Vec<FinalEntry>,
}
struct FinalEntry {
    name: String,
    kind: u8,
    target: u32,
}
/// Bytes of `NxSnapshot.content_id`: the native Content ID, or an add-on
/// title ID little-endian followed by eight zero bytes.
pub(crate) fn stored_content_id(s: &SnapshotInput) -> [u8; 16] {
    match s.addon_title_id {
        Some(id) => {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&id.to_le_bytes());
            bytes
        }
        None => s.content_id.unwrap_or([0; 16]),
    }
}
struct FinalSnapshot {
    program_index: u32,
    snapshot_type: u8,
    version: u32,
    root: u32,
    logical_size: u64,
    display_version: Option<String>,
    content_id: [u8; 16],
    /// `content_id` holds an add-on title ID (snapshot flag bit 1).
    addon: bool,
    build_id: [u8; 32],
    identity: [u8; 32],
    dependencies: Vec<DependencyInput>,
    metadata: Option<SnapshotMetadata>,
    meta_refs: Option<MetadataRefs>,
    npdm: u32,
    required_firmware: Option<u32>,
}

#[derive(Clone)]
struct LocalizedRef {
    language: String,
    title: Option<String>,
    icon: u32,
}
#[derive(Clone)]
struct MetadataRefs {
    title: Option<String>,
    publisher: Option<String>,
    icon: u32,
    nacp: u32,
    localized: Vec<LocalizedRef>,
    opaque: Vec<u32>,
}

struct MediaBuild {
    media_type: u16,
    size: u64,
    digest: [u8; 32],
    spool_offset: u64,
    data_offset: u64,
}
struct MediaStage {
    records: Vec<MediaBuild>,
    icon_candidates: HashMap<(u16, [u8; 32]), Vec<u32>>,
    bytes: u64,
}
impl MediaStage {
    fn new() -> Self {
        Self {
            records: Vec::new(),
            icon_candidates: HashMap::new(),
            bytes: 0,
        }
    }
}

// These arguments are independent streams and accounting state; keeping them explicit
// avoids a mutable writer context that would obscure ownership and staging behavior.
#[allow(clippy::too_many_arguments)]
fn add_media(
    media: MediaInput,
    expected: Option<MediaType>,
    stage: &mut MediaStage,
    spool: &mut File,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
    peak_temp: &mut u64,
    data_spool_len: u64,
) -> Result<u32, WriterError> {
    validate_media(&media, expected)?;
    if media_is_icon(media.media_type) && expected.is_none() { /* icon fields may choose PNG or JPEG */
    } else if expected.is_some() && media.media_type != expected.unwrap() {
        return Err(invalid("native media type mismatch"));
    }
    let start = spool.seek(SeekFrom::End(0))?;
    let mut remaining = media.size;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut reader = media.reader;
    while remaining > 0 {
        cancelled(options)?;
        let n = remaining.min(buf.len() as u64) as usize;
        let next = stage
            .bytes
            .checked_add(n as u64)
            .and_then(|v| v.checked_add(data_spool_len))
            .ok_or_else(|| limit_error("temporary disk accounting overflow"))?;
        check_temporary_size(next, options)?;
        read_exact_uncounted(&mut *reader, &mut buf[..n], options)?;
        spool.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        remaining -= n as u64;
        stage.bytes = stage
            .bytes
            .checked_add(n as u64)
            .ok_or_else(|| limit_error("Media byte count overflow"))?;
        *peak_temp = (*peak_temp).max(
            data_spool_len
                .checked_add(stage.bytes)
                .ok_or_else(|| limit_error("temporary disk accounting overflow"))?,
        );
    }
    let mut extra = [0u8; 1];
    if reader.read(&mut extra)? != 0 {
        return Err(invalid(
            "native media stream contains bytes beyond its declared size",
        ));
    }
    let digest = hasher.finish();
    if media_is_icon(media.media_type) {
        let key = (media.media_type as u16, digest);
        if let Some(candidates) = stage.icon_candidates.get(&key) {
            for &id in candidates {
                let old = &stage.records[id as usize];
                if old.size == media.size
                    && compare_spool_ranges(spool, old.spool_offset, start, media.size, options)?
                {
                    spool.set_len(start)?;
                    spool.seek(SeekFrom::End(0))?;
                    stage.bytes = stage
                        .bytes
                        .checked_sub(media.size)
                        .ok_or_else(|| limit_error("Media spool accounting underflow"))?;
                    budget.charge(0, "shared icon")?;
                    return Ok(id);
                }
            }
        }
        stage
            .icon_candidates
            .entry(key)
            .or_default()
            .push(stage.records.len() as u32);
    }
    if stage.records.len() >= 65535 {
        return Err(limit_error("Media record count exceeds NX 0.1 limit"));
    }
    let id = stage.records.len() as u32;
    stage
        .records
        .try_reserve(1)
        .map_err(|_| limit_error("unable to reserve Media record"))?;
    stage.records.push(MediaBuild {
        media_type: media.media_type as u16,
        size: media.size,
        digest,
        spool_offset: start,
        data_offset: 0,
    });
    budget.charge(128, "Media record and index")?;
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
fn process_metadata(
    metadata: SnapshotMetadata,
    preprocessed_opaque: Option<Vec<u32>>,
    stage: &mut MediaStage,
    media_spool: &mut File,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
    peak_temp: &mut u64,
    data_spool_len: u64,
) -> Result<MetadataRefs, WriterError> {
    let mut refs = MetadataRefs {
        title: metadata.title,
        publisher: metadata.publisher,
        icon: NO_INDEX,
        nacp: NO_INDEX,
        localized: Vec::new(),
        opaque: Vec::new(),
    };
    if let Some(icon) = metadata.icon {
        refs.icon = add_media(
            icon,
            None,
            stage,
            media_spool,
            options,
            budget,
            peak_temp,
            data_spool_len,
        )?;
    }
    if let Some(nacp) = metadata.nacp {
        refs.nacp = add_media(
            nacp,
            Some(MediaType::Nacp),
            stage,
            media_spool,
            options,
            budget,
            peak_temp,
            data_spool_len,
        )?;
    }
    refs.localized
        .try_reserve_exact(metadata.localized.len())
        .map_err(|_| limit_error("unable to reserve localized metadata"))?;
    for local in metadata.localized {
        let icon = match local.icon {
            Some(m) => add_media(
                m,
                None,
                stage,
                media_spool,
                options,
                budget,
                peak_temp,
                data_spool_len,
            )?,
            None => NO_INDEX,
        };
        refs.localized.push(LocalizedRef {
            language: local.language,
            title: local.title,
            icon,
        });
    }
    if let Some(existing) = preprocessed_opaque {
        refs.opaque = existing;
    } else {
        refs.opaque
            .try_reserve_exact(metadata.opaque.len())
            .map_err(|_| limit_error("unable to reserve opaque metadata indexes"))?;
        for media in metadata.opaque {
            refs.opaque.push(add_media(
                media,
                Some(MediaType::Opaque),
                stage,
                media_spool,
                options,
                budget,
                peak_temp,
                data_spool_len,
            )?);
        }
    }
    Ok(refs)
}

#[allow(clippy::too_many_arguments)]
fn process_selected_opaque(
    snapshot: &mut FinalSnapshot,
    app: &mut ApplicationBuild,
    stage: &mut MediaStage,
    media_spool: &mut File,
    data_spool_len: u64,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
    peak_temp: &mut u64,
) -> Result<(), WriterError> {
    let Some(metadata) = snapshot.metadata.as_mut() else {
        return Ok(());
    };
    let opaque = std::mem::take(&mut metadata.opaque);
    let first = if opaque.is_empty() {
        0
    } else {
        u32::try_from(stage.records.len()).map_err(|_| limit_error("Media index exceeds u32"))?
    };
    let count = u32::try_from(opaque.len())
        .map_err(|_| limit_error("opaque metadata count exceeds u32"))?;
    let mut references = Vec::new();
    references
        .try_reserve_exact(opaque.len())
        .map_err(|_| limit_error("unable to reserve selected opaque metadata"))?;
    for media in opaque {
        references.push(add_media(
            media,
            Some(MediaType::Opaque),
            stage,
            media_spool,
            options,
            budget,
            peak_temp,
            data_spool_len,
        )?);
    }
    if count > 0 && references.windows(2).any(|w| w[1] != w[0] + 1) {
        return Err(invalid("selected opaque media records are not contiguous"));
    }
    app.current_opaque_first = first;
    app.current_opaque_count = count;
    snapshot.meta_refs = Some(MetadataRefs {
        title: metadata.title.take(),
        publisher: metadata.publisher.take(),
        icon: NO_INDEX,
        nacp: NO_INDEX,
        localized: Vec::new(),
        opaque: references,
    });
    Ok(())
}

fn finish_metadata(
    snapshot: &mut FinalSnapshot,
    stage: &mut MediaStage,
    media_spool: &mut File,
    data_spool_len: u64,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
    peak_temp: &mut u64,
) -> Result<(), WriterError> {
    let Some(metadata) = snapshot.metadata.take() else {
        return Ok(());
    };
    let previous = snapshot.meta_refs.take();
    let preprocessed = previous.as_ref().map(|m| m.opaque.clone());
    let mut refs = process_metadata(
        metadata,
        preprocessed,
        stage,
        media_spool,
        options,
        budget,
        peak_temp,
        data_spool_len,
    )?;
    if let Some(prior) = previous {
        refs.title = prior.title;
        refs.publisher = prior.publisher;
        refs.opaque = prior.opaque;
    }
    snapshot.meta_refs = Some(refs);
    Ok(())
}

fn align_up(value: u64, alignment: u64) -> Result<u64, WriterError> {
    value
        .checked_add(alignment - 1)
        .map(|v| v & !(alignment - 1))
        .ok_or_else(|| limit_error("section offset overflow"))
}
fn checked_table_bytes(count: usize, size: u64) -> Result<u64, WriterError> {
    (count as u64)
        .checked_mul(size)
        .ok_or_else(|| limit_error("table size overflow"))
}
fn write_u16_at(dst: &mut [u8], offset: usize, value: u16) {
    dst[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn write_u32_at(dst: &mut [u8], offset: usize, value: u32) {
    dst[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn write_u64_at(dst: &mut [u8], offset: usize, value: u64) {
    dst[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[derive(Default)]
struct StringPool {
    bytes: Vec<u8>,
    offsets: HashMap<String, (u32, u32)>,
}
impl StringPool {
    fn intern(
        &mut self,
        value: Option<&str>,
        budget: &mut MemoryBudget,
    ) -> Result<(u32, u32), WriterError> {
        let Some(value) = value else {
            return Ok((NO_INDEX, 0));
        };
        if let Some(pair) = self.offsets.get(value) {
            return Ok(*pair);
        }
        let off = u32::try_from(self.bytes.len())
            .map_err(|_| limit_error("Catalog string pool offset exceeds u32"))?;
        let len = u32::try_from(value.len())
            .map_err(|_| limit_error("Catalog string length exceeds u32"))?;
        self.bytes
            .try_reserve(value.len())
            .map_err(|_| limit_error("unable to reserve Catalog string pool"))?;
        self.offsets
            .try_reserve(1)
            .map_err(|_| limit_error("unable to reserve Catalog string index"))?;
        self.bytes.extend_from_slice(value.as_bytes());
        self.offsets.insert(value.to_owned(), (off, len));
        budget.charge(
            (value.len() as u64).saturating_mul(2).saturating_add(48),
            "interned Catalog string",
        )?;
        if self.bytes.len() as u64 > MAX_CATALOG {
            return Err(limit_error("Catalog string pool exceeds 16 MiB"));
        }
        Ok((off, len))
    }
}

struct EncodedLocale {
    language: (u32, u32),
    title: (u32, u32),
    icon: u32,
}
struct EncodedApp {
    title: (u32, u32),
    publisher: (u32, u32),
    icon: u32,
    nacp: u32,
    first_local: u32,
    local_count: u32,
    selected: u32,
}
struct EncodedSnapshot {
    display: (u32, u32),
    first_dependency: u32,
    dependency_count: u32,
}
struct HistoryRow {
    snapshot_index: u32,
    metadata: MetadataRefs,
}
struct HistoryPayload {
    type_id: u32,
    flags: u32,
    owner_kind: u16,
    owner: u32,
    bytes: Vec<u8>,
}

struct CatalogExtension {
    type_id: u32,
    flags: u32,
    owner_kind: u16,
    owner: u32,
    bytes: Vec<u8>,
}

fn build_catalog(
    apps: &[ApplicationBuild],
    programs: &[ProgramBuild],
    snapshots: &[FinalSnapshot],
    optional_extensions: &[CatalogExtension],
    budget: &mut MemoryBudget,
) -> Result<Vec<u8>, WriterError> {
    let mut strings = StringPool::default();
    let mut encoded_apps = Vec::new();
    let mut encoded_locales = Vec::<EncodedLocale>::new();
    let mut localized_first_counts = Vec::<(u32, u32)>::new();
    let mut ext_payloads = Vec::<HistoryPayload>::new();
    encoded_apps
        .try_reserve_exact(apps.len())
        .map_err(|_| limit_error("unable to reserve Catalog applications"))?;
    for (ai, app) in apps.iter().enumerate() {
        let current = if app.selected_snapshot == NO_INDEX {
            None
        } else {
            snapshots
                .get(app.selected_snapshot as usize)
                .and_then(|s| s.meta_refs.as_ref())
        };
        let title = strings.intern(current.and_then(|m| m.title.as_deref()), budget)?;
        let publisher = strings.intern(current.and_then(|m| m.publisher.as_deref()), budget)?;
        let icon = current.map_or(NO_INDEX, |m| m.icon);
        let nacp = current.map_or(NO_INDEX, |m| m.nacp);
        let first_local = u32::try_from(encoded_locales.len())
            .map_err(|_| limit_error("localized table index exceeds u32"))?;
        if let Some(metadata) = current {
            for local in &metadata.localized {
                let language = strings.intern(Some(&local.language), budget)?;
                let title = strings.intern(local.title.as_deref(), budget)?;
                encoded_locales.push(EncodedLocale {
                    language,
                    title,
                    icon: local.icon,
                });
            }
        }
        let local_count = u32::try_from(encoded_locales.len())
            .map_err(|_| limit_error("localized table count exceeds u32"))?
            - first_local;
        if encoded_locales.len() > 16384 {
            return Err(limit_error(
                "current Catalog localized table exceeds NX 0.1 limit",
            ));
        }
        localized_first_counts.push((first_local, local_count));
        encoded_apps.push(EncodedApp {
            title,
            publisher,
            icon,
            nacp,
            first_local,
            local_count,
            selected: app.selected_snapshot,
        });
        let mut history = Vec::<HistoryRow>::new();
        if app.selected_snapshot != NO_INDEX {
            for si in app.first_snapshot..app.first_snapshot + app.snapshot_count {
                if si == app.selected_snapshot {
                    continue;
                }
                if let Some(meta) = snapshots[si as usize].meta_refs.as_ref() {
                    history.push(HistoryRow {
                        snapshot_index: si,
                        metadata: meta.clone(),
                    });
                }
            }
        }
        if !history.is_empty() {
            let mut history_locals = Vec::<EncodedLocale>::new();
            let mut opaque_refs = Vec::<u32>::new();
            let mut rows = Vec::<[u32; 12]>::new();
            for row in &history {
                let m = &row.metadata;
                let title = strings.intern(m.title.as_deref(), budget)?;
                let publisher = strings.intern(m.publisher.as_deref(), budget)?;
                let first_local = u32::try_from(history_locals.len())
                    .map_err(|_| limit_error("history localized offset exceeds u32"))?;
                for local in &m.localized {
                    history_locals.push(EncodedLocale {
                        language: strings.intern(Some(&local.language), budget)?,
                        title: strings.intern(local.title.as_deref(), budget)?,
                        icon: local.icon,
                    });
                }
                let local_count = u32::try_from(history_locals.len())
                    .map_err(|_| limit_error("history localized count exceeds u32"))?
                    - first_local;
                let first_opaque = u32::try_from(opaque_refs.len())
                    .map_err(|_| limit_error("history opaque offset exceeds u32"))?;
                opaque_refs.extend_from_slice(&m.opaque);
                rows.push([
                    row.snapshot_index,
                    title.0,
                    title.1,
                    publisher.0,
                    publisher.1,
                    m.icon,
                    m.nacp,
                    first_local,
                    local_count,
                    first_opaque,
                    m.opaque.len() as u32,
                    0,
                ]);
            }
            if history.len() > 65536 || history_locals.len() > 16384 {
                return Err(limit_error(
                    "snapshot metadata history count exceeds NX 0.1 limit",
                ));
            }
            let size = 24u64
                .checked_add(rows.len() as u64 * 48)
                .and_then(|v| v.checked_add(history_locals.len() as u64 * 24))
                .and_then(|v| v.checked_add(opaque_refs.len() as u64 * 4))
                .ok_or_else(|| limit_error("metadata history size overflow"))?;
            let usize_size = usize::try_from(size)
                .map_err(|_| limit_error("metadata history exceeds host address space"))?;
            let mut payload = Vec::new();
            payload
                .try_reserve_exact(usize_size)
                .map_err(|_| limit_error("unable to reserve metadata history payload"))?;
            payload.resize(usize_size, 0);
            payload[..4].copy_from_slice(b"NXSM");
            write_u16_at(&mut payload, 4, 1);
            write_u16_at(&mut payload, 6, 24);
            write_u32_at(&mut payload, 8, rows.len() as u32);
            write_u32_at(&mut payload, 12, history_locals.len() as u32);
            write_u32_at(&mut payload, 16, opaque_refs.len() as u32);
            let mut cursor = 24usize;
            for row in rows {
                for value in row {
                    write_u32_at(&mut payload, cursor, value);
                    cursor += 4;
                }
            }
            for local in history_locals {
                write_u32_at(&mut payload, cursor, local.language.0);
                write_u32_at(&mut payload, cursor + 4, local.language.1);
                write_u32_at(&mut payload, cursor + 8, local.title.0);
                write_u32_at(&mut payload, cursor + 12, local.title.1);
                write_u32_at(&mut payload, cursor + 16, local.icon);
                cursor += 24;
            }
            for index in opaque_refs {
                write_u32_at(&mut payload, cursor, index);
                cursor += 4;
            }
            budget.charge(size, "Snapshot Metadata History extension payload")?;
            ext_payloads.push(HistoryPayload {
                type_id: SNAPSHOT_METADATA_HISTORY,
                flags: 1,
                owner_kind: 1,
                owner: ai as u32,
                bytes: payload,
            });
        }
    }
    for (ai, app) in apps.iter().enumerate() {
        let rows: Vec<_> = snapshots
            .iter()
            .enumerate()
            .skip(app.first_snapshot as usize)
            .take(app.snapshot_count as usize)
            .filter(|(_, s)| s.npdm != NO_INDEX || s.required_firmware.is_some())
            .collect();
        if !rows.is_empty() {
            let mut bytes = vec![0u8; 16 + rows.len() * 16];
            bytes[..4].copy_from_slice(b"NXNM");
            write_u16_at(&mut bytes, 4, 1);
            write_u16_at(&mut bytes, 6, 16);
            write_u32_at(&mut bytes, 8, rows.len() as u32);
            for (ri, (si, snapshot)) in rows.iter().enumerate() {
                let at = 16 + ri * 16;
                write_u32_at(&mut bytes, at, *si as u32);
                write_u32_at(&mut bytes, at + 4, snapshot.npdm);
                write_u32_at(&mut bytes, at + 8, snapshot.required_firmware.unwrap_or(0));
                write_u32_at(
                    &mut bytes,
                    at + 12,
                    snapshot.required_firmware.is_some() as u32,
                );
            }
            budget.charge(bytes.len() as u64, "Snapshot Native Metadata")?;
            ext_payloads.push(HistoryPayload {
                type_id: SNAPSHOT_NATIVE_METADATA,
                flags: 1,
                owner_kind: 1,
                owner: ai as u32,
                bytes,
            });
        }
    }
    for extension in optional_extensions {
        ext_payloads.push(HistoryPayload {
            type_id: extension.type_id,
            flags: extension.flags,
            owner_kind: extension.owner_kind,
            owner: extension.owner,
            bytes: extension.bytes.clone(),
        });
    }
    ext_payloads.sort_by_key(|e| (e.type_id, e.owner_kind, e.owner));
    let mut encoded_snapshots = Vec::new();
    encoded_snapshots
        .try_reserve_exact(snapshots.len())
        .map_err(|_| limit_error("unable to reserve Catalog snapshots"))?;
    let mut dep_count = 0u32;
    for snapshot in snapshots {
        let display = strings.intern(snapshot.display_version.as_deref(), budget)?;
        let first = dep_count;
        dep_count = dep_count
            .checked_add(snapshot.dependencies.len() as u32)
            .ok_or_else(|| limit_error("dependency count overflow"))?;
        encoded_snapshots.push(EncodedSnapshot {
            display,
            first_dependency: first,
            dependency_count: snapshot.dependencies.len() as u32,
        });
    }
    if dep_count > 32768 {
        return Err(limit_error("dependency table exceeds NX 0.1 limit"));
    }
    let app_count = apps.len();
    let program_count = programs.len();
    let snapshot_count = snapshots.len();
    let locale_count = encoded_locales.len();
    let dependency_count = dep_count;
    let extension_count = ext_payloads.len();
    if extension_count > 4096 {
        return Err(limit_error("extension count exceeds NX 0.1 limit"));
    }
    let mut cursor = CATALOG_HEADER_SIZE;
    let app_off = align_up(cursor, 8)?;
    cursor = app_off
        .checked_add(checked_table_bytes(app_count, APP_SIZE)?)
        .ok_or_else(|| limit_error("Catalog application range overflow"))?;
    let prog_off = if program_count > 0 {
        let o = align_up(cursor, 8)?;
        cursor = o
            .checked_add(checked_table_bytes(program_count, PROGRAM_SIZE)?)
            .ok_or_else(|| limit_error("Catalog program range overflow"))?;
        o
    } else {
        0
    };
    let snap_off = align_up(cursor, 8)?;
    cursor = snap_off
        .checked_add(checked_table_bytes(snapshot_count, SNAPSHOT_SIZE)?)
        .ok_or_else(|| limit_error("Catalog snapshot range overflow"))?;
    let local_off = if locale_count > 0 {
        let o = align_up(cursor, 8)?;
        cursor = o
            .checked_add(checked_table_bytes(locale_count, LOCALIZED_SIZE)?)
            .ok_or_else(|| limit_error("Catalog localized range overflow"))?;
        o
    } else {
        0
    };
    let dep_off = if dependency_count > 0 {
        let o = align_up(cursor, 8)?;
        cursor = o
            .checked_add(checked_table_bytes(
                dependency_count as usize,
                DEPENDENCY_SIZE,
            )?)
            .ok_or_else(|| limit_error("Catalog dependency range overflow"))?;
        o
    } else {
        0
    };
    let ext_off = if extension_count > 0 {
        let o = align_up(cursor, 8)?;
        cursor = o
            .checked_add(checked_table_bytes(extension_count, EXTENSION_SIZE)?)
            .ok_or_else(|| limit_error("Catalog extension range overflow"))?;
        o
    } else {
        0
    };
    let string_off = align_up(cursor, 8)?;
    cursor = string_off
        .checked_add(strings.bytes.len() as u64)
        .ok_or_else(|| limit_error("Catalog string range overflow"))?;
    let mut extension_offsets = Vec::new();
    for payload in &ext_payloads {
        cursor = align_up(cursor, 8)?;
        extension_offsets.push(cursor);
        cursor = cursor
            .checked_add(payload.bytes.len() as u64)
            .ok_or_else(|| limit_error("Catalog extension range overflow"))?;
    }
    if cursor > MAX_CATALOG {
        return Err(limit_error("Catalog exceeds 16 MiB"));
    }
    let total =
        usize::try_from(cursor).map_err(|_| limit_error("Catalog exceeds host address space"))?;
    budget.charge(cursor, "serialized Catalog section")?;
    let mut cat = Vec::new();
    cat.try_reserve_exact(total)
        .map_err(|_| limit_error("unable to reserve Catalog section"))?;
    cat.resize(total, 0);
    cat[..4].copy_from_slice(b"NXCT");
    write_u16_at(&mut cat, 4, 1);
    write_u16_at(&mut cat, 6, 128);
    write_u64_at(&mut cat, 8, cursor);
    write_u32_at(&mut cat, 20, app_count as u32);
    write_u32_at(&mut cat, 24, program_count as u32);
    write_u32_at(&mut cat, 28, snapshot_count as u32);
    write_u32_at(&mut cat, 32, locale_count as u32);
    write_u32_at(&mut cat, 36, dependency_count);
    write_u32_at(&mut cat, 40, extension_count as u32);
    write_u64_at(&mut cat, 48, app_off);
    write_u64_at(&mut cat, 56, prog_off);
    write_u64_at(&mut cat, 64, snap_off);
    write_u64_at(&mut cat, 72, local_off);
    write_u64_at(&mut cat, 80, dep_off);
    write_u64_at(&mut cat, 88, ext_off);
    write_u64_at(&mut cat, 96, string_off);
    write_u64_at(&mut cat, 104, strings.bytes.len() as u64);
    for (i, (app, meta)) in apps.iter().zip(&encoded_apps).enumerate() {
        let off = app_off as usize + i * APP_SIZE as usize;
        write_u64_at(&mut cat, off, app.application_id);
        cat[off + 8] = app.platform;
        let has_base = snapshots
            [app.first_snapshot as usize..(app.first_snapshot + app.snapshot_count) as usize]
            .iter()
            .any(|s| s.snapshot_type == SnapshotType::Base as u8);
        let has_dlc = snapshots
            [app.first_snapshot as usize..(app.first_snapshot + app.snapshot_count) as usize]
            .iter()
            .any(|s| s.snapshot_type == SnapshotType::Dlc as u8);
        cat[off + 9] = (has_base as u8) | ((has_dlc as u8) << 1);
        write_u32_at(&mut cat, off + 12, app.first_program);
        write_u32_at(&mut cat, off + 16, app.program_count);
        write_u32_at(&mut cat, off + 20, app.first_snapshot);
        write_u32_at(&mut cat, off + 24, app.snapshot_count);
        write_u32_at(&mut cat, off + 28, meta.title.0);
        write_u32_at(&mut cat, off + 32, meta.title.1);
        write_u32_at(&mut cat, off + 36, meta.publisher.0);
        write_u32_at(&mut cat, off + 40, meta.publisher.1);
        write_u32_at(&mut cat, off + 44, meta.icon);
        write_u32_at(&mut cat, off + 48, meta.first_local);
        write_u32_at(&mut cat, off + 52, meta.local_count);
        write_u32_at(&mut cat, off + 56, meta.nacp);
        write_u32_at(&mut cat, off + 60, app.current_opaque_first);
        write_u32_at(&mut cat, off + 64, app.current_opaque_count);
        write_u32_at(&mut cat, off + 68, meta.selected);
    }
    for (i, p) in programs.iter().enumerate() {
        let off = prog_off as usize + i * PROGRAM_SIZE as usize;
        write_u64_at(&mut cat, off, p.program_id);
        write_u32_at(&mut cat, off + 8, p.app_index);
        cat[off + 12] = p.kind;
        cat[off + 13] = p.has_content_id as u8;
        cat[off + 16..off + 32].copy_from_slice(&p.content_id);
        write_u32_at(&mut cat, off + 32, p.first_snapshot);
        write_u32_at(&mut cat, off + 36, p.snapshot_count);
        write_u32_at(&mut cat, off + 40, p.npdm);
    }
    for (i, (s, e)) in snapshots.iter().zip(&encoded_snapshots).enumerate() {
        let off = snap_off as usize + i * SNAPSHOT_SIZE as usize;
        write_u32_at(&mut cat, off, s.program_index);
        cat[off + 4] = s.snapshot_type;
        cat[off + 5] = (s.build_id != [0; 32]) as u8 | (s.addon as u8) << 1;
        write_u32_at(&mut cat, off + 8, s.version);
        write_u32_at(&mut cat, off + 12, s.root);
        write_u64_at(&mut cat, off + 16, s.logical_size);
        write_u32_at(&mut cat, off + 24, e.display.0);
        write_u32_at(&mut cat, off + 28, e.display.1);
        cat[off + 32..off + 48].copy_from_slice(&s.content_id);
        write_u32_at(&mut cat, off + 48, e.first_dependency);
        write_u32_at(&mut cat, off + 52, e.dependency_count);
        cat[off + 56..off + 88].copy_from_slice(&s.identity);
        cat[off + 88..off + 120].copy_from_slice(&s.build_id);
    }
    for (i, l) in encoded_locales.iter().enumerate() {
        let off = local_off as usize + i * LOCALIZED_SIZE as usize;
        write_u32_at(&mut cat, off, l.language.0);
        write_u32_at(&mut cat, off + 4, l.language.1);
        write_u32_at(&mut cat, off + 8, l.title.0);
        write_u32_at(&mut cat, off + 12, l.title.1);
        write_u32_at(&mut cat, off + 16, l.icon);
    }
    let mut dep_i = 0usize;
    for s in snapshots {
        for d in &s.dependencies {
            let off = dep_off as usize + dep_i * DEPENDENCY_SIZE as usize;
            cat[off] = d.kind as u8;
            let content = d.content_id.unwrap_or([0; 16]);
            cat[off + 1] = (d.content_id.is_some()) as u8;
            cat[off + 4..off + 20].copy_from_slice(&content);
            cat[off + 20..off + 52].copy_from_slice(&d.target_snapshot_identity);
            dep_i += 1;
        }
    }
    for (i, (payload, data_off)) in ext_payloads.iter().zip(&extension_offsets).enumerate() {
        let off = ext_off as usize + i * EXTENSION_SIZE as usize;
        write_u32_at(&mut cat, off, payload.type_id);
        write_u32_at(&mut cat, off + 4, payload.flags);
        write_u16_at(&mut cat, off + 8, payload.owner_kind);
        write_u32_at(&mut cat, off + 12, payload.owner);
        write_u64_at(&mut cat, off + 16, *data_off);
        write_u64_at(&mut cat, off + 24, payload.bytes.len() as u64);
        let at = *data_off as usize;
        cat[at..at + payload.bytes.len()].copy_from_slice(&payload.bytes);
    }
    let pool_at = string_off as usize;
    cat[pool_at..pool_at + strings.bytes.len()].copy_from_slice(&strings.bytes);
    let _ = localized_first_counts;
    Ok(cat)
}

struct EncodedEntry {
    name_offset: u64,
    name_len: u32,
    target: u32,
    kind: u8,
}
fn build_manifest(
    directories: &[DirectoryBuild],
    files: &[FileBuild],
    block_refs: &[u32],
    options: &WriterOptions,
    budget: &mut MemoryBudget,
) -> Result<Vec<u8>, WriterError> {
    if directories.is_empty() {
        return Err(invalid("Manifest requires at least one root directory"));
    }
    if directories.len() > 16_777_216
        || files.len() > 33_554_432
        || block_refs.len() > u32::MAX as usize
    {
        return Err(limit_error("Manifest table count exceeds NX 0.1 limit"));
    }
    let entry_count: usize = directories.iter().map(|d| d.entries.len()).sum();
    if entry_count > 50_331_648 {
        return Err(limit_error("Manifest Entry count exceeds NX 0.1 limit"));
    }
    let mut path_pool = StringPool::default();
    let mut entries = Vec::<EncodedEntry>::new();
    entries
        .try_reserve_exact(entry_count)
        .map_err(|_| limit_error("unable to reserve Manifest entry references"))?;
    for d in directories {
        for entry in &d.entries {
            let (off, len) = path_pool.intern(Some(&entry.name), budget)?;
            entries.push(EncodedEntry {
                name_offset: off as u64,
                name_len: len,
                target: entry.target,
                kind: entry.kind,
            });
        }
    }
    if path_pool.bytes.len() as u64 > MAX_PATH_POOL {
        return Err(limit_error("Manifest path pool exceeds 2 GiB"));
    }
    let mut cursor = MANIFEST_HEADER_SIZE;
    let dir_off = align_up(cursor, 8)?;
    cursor = dir_off
        .checked_add(checked_table_bytes(directories.len(), DIRECTORY_SIZE)?)
        .ok_or_else(|| limit_error("Manifest directory table overflow"))?;
    let file_off = if files.is_empty() {
        0
    } else {
        let o = align_up(cursor, 8)?;
        cursor = o
            .checked_add(checked_table_bytes(files.len(), FILE_SIZE)?)
            .ok_or_else(|| limit_error("Manifest file table overflow"))?;
        o
    };
    let entry_off = if entries.is_empty() {
        0
    } else {
        let o = align_up(cursor, 8)?;
        cursor = o
            .checked_add(checked_table_bytes(entries.len(), ENTRY_SIZE)?)
            .ok_or_else(|| limit_error("Manifest entry table overflow"))?;
        o
    };
    let reference_off = if block_refs.is_empty() {
        0
    } else {
        let o = align_up(cursor, 4)?;
        cursor = o
            .checked_add(checked_table_bytes(block_refs.len(), 4)?)
            .ok_or_else(|| limit_error("Manifest reference table overflow"))?;
        o
    };
    let path_off = align_up(cursor, 8)?;
    cursor = path_off
        .checked_add(path_pool.bytes.len() as u64)
        .ok_or_else(|| limit_error("Manifest path pool overflow"))?;
    if cursor > MAX_MANIFEST || cursor > options.max_memory_bytes {
        return Err(limit_error(
            "Manifest exceeds the supported section or memory budget",
        ));
    }
    budget.charge(cursor, "serialized Manifest section")?;
    let total =
        usize::try_from(cursor).map_err(|_| limit_error("Manifest exceeds host address space"))?;
    let mut manifest = Vec::new();
    manifest
        .try_reserve_exact(total)
        .map_err(|_| limit_error("unable to reserve Manifest section"))?;
    manifest.resize(total, 0);
    manifest[..4].copy_from_slice(b"NXMF");
    write_u16_at(&mut manifest, 4, 1);
    write_u16_at(&mut manifest, 6, 96);
    write_u64_at(&mut manifest, 8, cursor);
    write_u32_at(&mut manifest, 16, directories.len() as u32);
    write_u32_at(&mut manifest, 20, files.len() as u32);
    write_u32_at(&mut manifest, 24, entries.len() as u32);
    write_u32_at(&mut manifest, 28, block_refs.len() as u32);
    write_u64_at(&mut manifest, 32, dir_off);
    write_u64_at(&mut manifest, 40, file_off);
    write_u64_at(&mut manifest, 48, entry_off);
    write_u64_at(&mut manifest, 56, reference_off);
    write_u64_at(&mut manifest, 64, path_off);
    write_u64_at(&mut manifest, 72, path_pool.bytes.len() as u64);
    let mut entry_cursor = 0u32;
    for (i, d) in directories.iter().enumerate() {
        let off = dir_off as usize + i * DIRECTORY_SIZE as usize;
        write_u32_at(&mut manifest, off, entry_cursor);
        write_u32_at(&mut manifest, off + 4, d.entries.len() as u32);
        entry_cursor = entry_cursor
            .checked_add(d.entries.len() as u32)
            .ok_or_else(|| limit_error("Manifest entry cursor overflow"))?;
    }
    for (i, f) in files.iter().enumerate() {
        let off = file_off as usize + i * FILE_SIZE as usize;
        write_u64_at(&mut manifest, off, f.size);
        write_u32_at(&mut manifest, off + 8, f.first_ref);
        write_u32_at(&mut manifest, off + 12, f.block_count);
        manifest[off + 16] = f.chunk_size_bytes.trailing_zeros() as u8;
        manifest[off + 20..off + 52].copy_from_slice(&f.digest);
    }
    for (i, e) in entries.iter().enumerate() {
        let off = entry_off as usize + i * ENTRY_SIZE as usize;
        write_u64_at(
            &mut manifest,
            off,
            e.name_offset
                .checked_add(path_off)
                .ok_or_else(|| limit_error("Manifest name offset overflow"))?,
        );
        write_u32_at(&mut manifest, off + 8, e.name_len);
        write_u32_at(&mut manifest, off + 12, e.target);
        manifest[off + 16] = e.kind;
    }
    for (i, id) in block_refs.iter().enumerate() {
        write_u32_at(&mut manifest, reference_off as usize + i * 4, *id);
    }
    let pool_at = path_off as usize;
    manifest[pool_at..pool_at + path_pool.bytes.len()].copy_from_slice(&path_pool.bytes);
    Ok(manifest)
}

fn media_layout(stage: &mut MediaStage) -> Result<(Vec<u8>, u64, u64), WriterError> {
    if stage.records.is_empty() {
        return Ok((Vec::new(), 0, 0));
    }
    if stage.records.len() > 65535 {
        return Err(limit_error("Media record count exceeds NX 0.1 limit"));
    }
    let payload_start = align_up(
        MEDIA_HEADER_SIZE + stage.records.len() as u64 * MEDIA_RECORD_SIZE,
        16,
    )?;
    let mut cursor = payload_start;
    for record in &mut stage.records {
        cursor = align_up(cursor, 16)?;
        record.data_offset = cursor;
        cursor = cursor
            .checked_add(record.size)
            .ok_or_else(|| limit_error("Media region size overflow"))?;
    }
    if cursor > MAX_MEDIA_REGION {
        return Err(limit_error("Media region exceeds 4 GiB"));
    }
    let table_end =
        usize::try_from(MEDIA_HEADER_SIZE + stage.records.len() as u64 * MEDIA_RECORD_SIZE)
            .map_err(|_| limit_error("Media record table exceeds address space"))?;
    let mut header_records = Vec::new();
    header_records
        .try_reserve_exact(table_end)
        .map_err(|_| limit_error("unable to reserve Media descriptors"))?;
    header_records.resize(table_end, 0);
    header_records[..4].copy_from_slice(b"NXMD");
    write_u16_at(&mut header_records, 4, 1);
    write_u16_at(&mut header_records, 6, 32);
    write_u64_at(&mut header_records, 8, cursor);
    write_u32_at(&mut header_records, 16, stage.records.len() as u32);
    write_u16_at(&mut header_records, 20, 64);
    write_u32_at(&mut header_records, 24, 32);
    write_u32_at(&mut header_records, 28, payload_start as u32);
    for (i, r) in stage.records.iter().enumerate() {
        let off = 32 + i * 64;
        write_u16_at(&mut header_records, off, r.media_type);
        write_u64_at(&mut header_records, off + 4, r.size);
        write_u64_at(&mut header_records, off + 12, r.size);
        write_u64_at(&mut header_records, off + 20, r.data_offset);
        header_records[off + 28..off + 60].copy_from_slice(&r.digest);
    }
    Ok((header_records, payload_start, cursor))
}

fn check_temporary_size(size: u64, options: &WriterOptions) -> Result<(), WriterError> {
    if size > options.max_temporary_disk_bytes {
        return Err(limit_error(
            "combined temporary files exceed configured disk budget",
        ));
    }
    Ok(())
}

fn write_bytes<W: Write>(
    file: &mut W,
    bytes: &[u8],
    position: &mut u64,
    options: &WriterOptions,
) -> Result<(), WriterError> {
    cancelled(options)?;
    let end = position
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| limit_error("archive position overflow"))?;
    if end > options.max_archive_size {
        return Err(limit_error("output archive exceeds configured size limit"));
    }
    file.write_all(bytes)?;
    *position = end;
    Ok(())
}
fn write_padding<W: Write>(
    file: &mut W,
    position: &mut u64,
    alignment: u64,
    options: &WriterOptions,
) -> Result<(), WriterError> {
    let target = align_up(*position, alignment)?;
    let zeros = [0u8; 4096];
    while *position < target {
        let n = (target - *position).min(zeros.len() as u64) as usize;
        write_bytes(file, &zeros[..n], position, options)?;
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn copy_spool_range<W: Write>(
    source: &mut File,
    offset: u64,
    len: u64,
    destination: &mut W,
    position: &mut u64,
    options: &WriterOptions,
    staging_bytes: u64,
    peak_temp: &mut u64,
) -> Result<(), WriterError> {
    source.seek(SeekFrom::Start(offset))?;
    let mut remaining = len;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        cancelled(options)?;
        let n = remaining.min(buffer.len() as u64) as usize;
        source.read_exact(&mut buffer[..n])?;
        write_bytes(destination, &buffer[..n], position, options)?;
        remaining -= n as u64;
        *peak_temp = (*peak_temp).max(
            staging_bytes
                .checked_add(*position)
                .ok_or_else(|| limit_error("temporary disk accounting overflow"))?,
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_media_region<W: Write>(
    output: &mut W,
    position: &mut u64,
    media_offset: u64,
    header_records: &[u8],
    payload_start: u64,
    stage: &MediaStage,
    media_spool: &mut File,
    data_spool_len: u64,
    options: &WriterOptions,
    peak_temp: &mut u64,
) -> Result<(), WriterError> {
    write_bytes(output, header_records, position, options)?;
    let target = media_offset
        .checked_add(payload_start)
        .ok_or_else(|| limit_error("Media payload position overflow"))?;
    while *position < target {
        let zeros = [0u8; 256];
        let n = (target - *position).min(zeros.len() as u64) as usize;
        write_bytes(output, &zeros[..n], position, options)?;
    }
    let staging = data_spool_len
        .checked_add(stage.bytes)
        .ok_or_else(|| limit_error("temporary disk accounting overflow"))?;
    for r in &stage.records {
        let absolute = media_offset
            .checked_add(r.data_offset)
            .ok_or_else(|| limit_error("Media payload offset overflow"))?;
        if *position != absolute {
            return Err(invalid(
                "Media payload offsets are not contiguous/aligned as planned",
            ));
        }
        copy_spool_range(
            media_spool,
            r.spool_offset,
            r.size,
            output,
            position,
            options,
            staging,
            peak_temp,
        )?;
        if r.data_offset + r.size
            < stage
                .records
                .last()
                .map_or(0, |last| last.data_offset + last.size)
        {
            write_padding(output, position, 16, options)?;
        }
    }
    Ok(())
}

pub(crate) fn atomic_replace(source: &Path, destination: &Path) -> Result<(), WriterError> {
    replace_file(source, destination, true)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path, durable: bool) -> Result<(), WriterError> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new_name: *const u16, flags: u32) -> i32;
    }
    let mut src: Vec<u16> = source.as_os_str().encode_wide().collect();
    src.push(0);
    let mut dst: Vec<u16> = destination.as_os_str().encode_wide().collect();
    dst.push(0);
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let ok = unsafe {
        MoveFileExW(
            src.as_ptr(),
            dst.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | if durable { MOVEFILE_WRITE_THROUGH } else { 0 },
        )
    };
    if ok != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(5 | 32)) {
        // MoveFileEx can refuse a target still open by our own edit lock.
        // RenameInfoEx preserves that handle and keeps the lock through publish.
        return replace_open_windows_file(source, destination, durable);
    }
    Err(error.into())
}

#[cfg(windows)]
fn replace_open_windows_file(
    source: &Path,
    destination: &Path,
    durable: bool,
) -> Result<(), WriterError> {
    use std::os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, RawHandle},
    };

    #[repr(C)]
    struct RenameInfo {
        flags: u32,
        root: RawHandle,
        name_bytes: u32,
        name: [u16; 1],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn SetFileInformationByHandle(
            file: RawHandle,
            class: u32,
            info: *const std::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    let absolute = std::path::absolute(destination)?;
    let name: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    let name_bytes = name.len() * std::mem::size_of::<u16>();
    let name_offset = std::mem::offset_of!(RenameInfo, name);
    let size = (name_offset + name_bytes + 2).max(std::mem::size_of::<RenameInfo>());
    let mut storage = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<RenameInfo>();
    // SAFETY: storage is aligned for RenameInfo, includes the trailing UTF-16
    // name and terminator, and remains live until the synchronous call returns.
    unsafe {
        (*info).flags = 0x1 | 0x2; // REPLACE_IF_EXISTS | POSIX_SEMANTICS
        (*info).name_bytes =
            u32::try_from(name_bytes).map_err(|_| invalid("rename path is too long"))?;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            storage
                .as_mut_ptr()
                .cast::<u8>()
                .add(name_offset)
                .cast::<u16>(),
            name.len(),
        );
    }
    let file = OpenOptions::new()
        .access_mode(0x0001_0000) // DELETE
        .custom_flags(if durable { 0x8000_0000 } else { 0 }) // FILE_FLAG_WRITE_THROUGH
        .open(source)?;
    let size = u32::try_from(size).map_err(|_| invalid("rename path is too long"))?;
    // SAFETY: the handle and correctly laid out buffer remain valid.
    if unsafe { SetFileInformationByHandle(file.as_raw_handle(), 22, info.cast(), size) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path, durable: bool) -> Result<(), WriterError> {
    #[cfg(unix)]
    let parent = if durable {
        let path = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        Some(File::open(path)?)
    } else {
        None
    };

    fs::rename(source, destination)?;

    #[cfg(unix)]
    if let Some(parent) = parent {
        parent.sync_all().map_err(|error| {
            WriterError::new(
                WriterErrorKind::Io,
                format!("destination was published but its directory could not be synchronized: {error}"),
            )
        })?;
    }
    #[cfg(not(unix))]
    let _ = durable;
    Ok(())
}

/// Write a compact NX 0.2 archive to a same-directory temporary file and
/// publish it atomically. Validation and cancellation happen before publication.
/// A directory sync error is reported after the new file has been published.
pub fn write_archive(
    mut input: ArchiveInput,
    destination: impl AsRef<Path>,
    options: &WriterOptions,
) -> Result<WriterStats, WriterError> {
    validate_options(options)?;
    let started = Instant::now();
    let mut budget = MemoryBudget {
        current: 0,
        peak: 0,
        limit: options.max_memory_bytes,
    };
    let mut resource_counts = InputCounts::default();
    if input.source_resources.len() > 4096 {
        return Err(limit_error("too many source resources"));
    }
    for resource in &mut input.source_resources {
        if resource.kind > 1
            || resource.root.entries.is_empty()
            || resource
                .root
                .entries
                .iter()
                .any(|e| !matches!(e.node, NodeInput::File(_)))
        {
            return Err(invalid(
                "source resources must be nonempty flat file sets of a known kind",
            ));
        }
        if resource.kind == 1
            && (resource.root.entries.len() != 1
                || resource.root.entries[0].name != "partition.hfs0")
        {
            return Err(invalid(
                "system update resource must contain partition.hfs0",
            ));
        }
        if !input
            .applications
            .iter()
            .any(|a| a.platform == resource.platform && a.application_id == resource.application_id)
        {
            return Err(invalid("source resource application is absent"));
        }
        count_tree(&mut resource.root, 0, options, &mut resource_counts)?;
    }
    budget.charge(resource_counts.estimated, "source resource input metadata")?;
    let resources = std::mem::take(&mut input.source_resources);
    let (mut apps, mut programs, mut work, mut input_counts) =
        plan_inputs(input, options, &mut budget)?;
    input_counts.input_bytes = input_counts
        .input_bytes
        .checked_add(resource_counts.input_bytes)
        .ok_or_else(|| limit_error("source resource input size overflow"))?;
    input_counts.file_entries += resource_counts.file_entries;
    input_counts.directories += resource_counts.directories;
    input_counts.entries += resource_counts.entries;
    if input_counts.file_entries > options.max_files as u64
        || input_counts.directories > options.max_directories as u64
        || input_counts.entries > options.max_entries as u64
    {
        return Err(limit_error(
            "game and source resource records exceed configured limits",
        ));
    }
    // Progress counts the file bytes read below; NPDM and media are not
    // counted. The output readback at the end is counted too, as two thirds
    // of the input (measured: a 52 GB NUP reads in 110 s, its 25 GB archive
    // reads back in 85 s), spread over the stored bytes it reads. Frontends
    // only ever add to the total, so it stays fixed.
    let readback_estimate = if options.verify_output_payloads {
        input_counts.input_bytes / 3 * 2
    } else {
        0
    };
    options
        .progress
        .add_total(input_counts.input_bytes + readback_estimate);
    let destination = destination.as_ref();
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if destination.file_name().is_none() {
        return Err(invalid("destination must name a file"));
    }
    if !parent.is_dir() {
        return Err(WriterError::new(
            WriterErrorKind::Io,
            "destination parent directory does not exist",
        ));
    }
    // Keep temporary basenames short even for long valid destination names.
    // Blocks go straight into the archive file after the two header slots.
    let (output_file, mut output_guard) = unique_temp(parent, OsStr::new(".nx-output"))?;
    budget.charge(1024 * 1024, "buffered output storage")?;
    // Exact dedup compare and Zstandard re-decode scratch (one chunk + frame).
    budget.charge(
        options.chunk_size_bytes as u64 * 2
            + zstd::zstd_safe::compress_bound(options.chunk_size_bytes as usize) as u64,
        "dedup comparison buffers",
    )?;
    let mut data_spool = DataSink {
        out: Output::new(output_file),
        checks: Vec::new(),
        pipeline: None,
        decoder: None,
        frame: Vec::new(),
        compare: Vec::new(),
    };
    data_spool.out.write(&[0u8; DATA_START as usize], options)?;
    if options.compression_mode == CompressionMode::ZstandardAuto {
        let automatic = options.compression_workers == 0;
        let mut workers = if automatic {
            std::thread::available_parallelism().map_or(1, |n| n.get().min(4) as u8)
        } else {
            options.compression_workers
        };
        while workers > 1 {
            let mut trial = MemoryBudget {
                current: budget.current,
                peak: budget.peak,
                limit: budget.limit,
            };
            let sized = WriterOptions {
                compression_workers: workers,
                ..options.clone()
            };
            match parallel::Pipeline::new(&sized, &mut trial) {
                Ok(pipeline) => {
                    budget = trial;
                    data_spool.pipeline = Some(pipeline);
                    break;
                }
                // Automatic mode degrades to fewer workers, then to serial.
                Err(error) if automatic && error.kind() != WriterErrorKind::Cancelled => {
                    workers -= 1
                }
                Err(error) => return Err(error),
            }
        }
    }
    let (mut media_spool, _media_guard) = unique_temp(parent, OsStr::new(".nx-stage-media"))?;
    let mut media_stage = MediaStage::new();
    let mut peak_temp = 0u64;
    // Program NPDM records are emitted first; application opaque ranges are
    // staged next in selected-source order so each Catalog range is contiguous.
    for p in &mut programs {
        if let Some(npdm) = p.npdm_input.take() {
            p.npdm = add_media(
                npdm,
                Some(MediaType::Npdm),
                &mut media_stage,
                &mut media_spool,
                options,
                &mut budget,
                &mut peak_temp,
                0,
            )?;
        }
    }
    let mut directories = Vec::<DirectoryBuild>::new();
    let mut files = Vec::<FileBuild>::new();
    let mut block_refs = Vec::<u32>::new();
    let mut blocks = Vec::<BlockBuild>::new();
    let mut block_dedup = BlockDedupIndex::default();
    let mut block_buffer = Vec::<u8>::new();
    let mut block_encoder = None;
    let mut timings = WriterTimings::default();
    let mut deduplicated_blocks = 0u64;
    let mut same_path: HashMap<(u32, u32, Vec<u8>), Vec<u32>> = HashMap::new();
    let mut snapshots = Vec::<FinalSnapshot>::new();
    snapshots
        .try_reserve_exact(work.len())
        .map_err(|_| limit_error("unable to reserve Snapshot table"))?;
    let mut seen_identities = HashMap::<[u8; 32], u32>::new();
    // (application, snapshot type, version) -> identity, for pending targets.
    let mut written = HashMap::<(u32, u8, u32), [u8; 32]>::new();
    for mut snapshot in work.drain(..) {
        cancelled(options)?;
        let root = std::mem::replace(&mut snapshot.source.root, DirectoryInput::empty());
        let content_id = stored_content_id(&snapshot.source);
        let build_id = snapshot.source.build_id.unwrap_or([0; 32]);
        let app = &apps[snapshot.app_index as usize];
        let (root_index, logical_size, identity) = process_tree(
            root,
            snapshot.app_index,
            snapshot.program_index,
            app.platform,
            app.application_id,
            snapshot.source.program_id,
            snapshot.source.snapshot_type as u8,
            snapshot.source.numeric_version,
            content_id,
            &mut directories,
            &mut files,
            &mut block_refs,
            &mut blocks,
            &mut block_dedup,
            &mut block_buffer,
            &mut block_encoder,
            &mut same_path,
            &mut data_spool,
            options,
            &mut budget,
            &mut deduplicated_blocks,
            &mut timings,
        )?;
        let snapshot_index = u32::try_from(snapshots.len())
            .map_err(|_| limit_error("Snapshot index exceeds u32"))?;
        if seen_identities.insert(identity, snapshot_index).is_some() {
            return Err(invalid(
                "two snapshots have the same canonical logical identity",
            ));
        }
        let npdm = if let Some(npdm) = snapshot.source.native.npdm.take() {
            add_media(
                npdm,
                Some(MediaType::Npdm),
                &mut media_stage,
                &mut media_spool,
                options,
                &mut budget,
                &mut peak_temp,
                data_spool.out.position,
            )?
        } else {
            NO_INDEX
        };
        let mut source = snapshot.source;
        let mut resolved_any = false;
        for dependency in &mut source.dependencies {
            if let Some((kind, version)) = pending_dependency(&dependency.target_snapshot_identity)
            {
                dependency.target_snapshot_identity = *written
                    .get(&(snapshot.app_index, kind, version))
                    .ok_or_else(|| {
                        invalid("pending dependency names no snapshot written before it")
                    })?;
                resolved_any = true;
            }
        }
        if resolved_any {
            source.dependencies.sort_by_key(|d| {
                (
                    d.kind as u8,
                    d.target_snapshot_identity,
                    d.content_id.unwrap_or([0; 16]),
                )
            });
            if source.dependencies.windows(2).any(|w| w[0] == w[1]) {
                return Err(invalid("duplicate snapshot dependency"));
            }
        }
        if source.addon_title_id.is_none() {
            written.insert(
                (
                    snapshot.app_index,
                    source.snapshot_type as u8,
                    source.numeric_version,
                ),
                identity,
            );
        }
        snapshots.push(FinalSnapshot {
            program_index: snapshot.program_index,
            snapshot_type: source.snapshot_type as u8,
            version: source.numeric_version,
            root: root_index,
            logical_size,
            display_version: source.display_version,
            content_id,
            addon: source.addon_title_id.is_some(),
            build_id,
            identity,
            dependencies: source.dependencies,
            metadata: source.metadata,
            meta_refs: None,
            npdm,
            required_firmware: source.native.required_firmware,
        });
    }
    let mut resource_rows = vec![Vec::new(); apps.len()];
    for resource in resources {
        let owner = apps
            .iter()
            .position(|a| {
                a.platform == resource.platform as u8 && a.application_id == resource.application_id
            })
            .ok_or_else(|| invalid("source resource owner is absent"))?;
        // Same file/block writer, separate root and ownership. No synthetic
        // program/snapshot and no change to any game's snapshot identity.
        let marks = (
            directories.len(),
            files.len(),
            block_refs.len(),
            blocks.len(),
        );
        let (root, _, _) = process_tree(
            resource.root,
            owner as u32,
            NO_INDEX,
            resource.platform as u8,
            resource.application_id,
            None,
            SnapshotType::Unknown as u8,
            0,
            [0; 16],
            &mut directories,
            &mut files,
            &mut block_refs,
            &mut blocks,
            &mut block_dedup,
            &mut block_buffer,
            &mut block_encoder,
            &mut HashMap::new(),
            &mut data_spool,
            options,
            &mut budget,
            &mut deduplicated_blocks,
            &mut timings,
        )?;
        // Two sources of one game (two cart revisions) can carry the same
        // firmware. Its blocks deduplicate; drop the repeated records too when
        // they added no block, so no directory or block is left unreferenced.
        let content = |root: u32| -> Vec<(String, u64, [u8; 32])> {
            directories[root as usize]
                .entries
                .iter()
                .map(|e| {
                    let file = &files[e.target as usize];
                    (e.name.clone(), file.size, file.digest)
                })
                .collect()
        };
        let mine = content(root);
        let repeated = resource_rows[owner]
            .iter()
            .any(|&(kind, other)| kind == resource.kind && content(other) == mine);
        if repeated && blocks.len() == marks.3 {
            directories.truncate(marks.0);
            files.truncate(marks.1);
            block_refs.truncate(marks.2);
            continue;
        }
        resource_rows[owner].push((resource.kind, root));
    }
    let resource_extensions: Vec<_> = resource_rows
        .iter_mut()
        .enumerate()
        .filter_map(|(owner, rows)| {
            rows.sort_unstable();
            (!rows.is_empty()).then(|| source_resource_extension(owner as u32, rows))
        })
        .collect();
    data_spool.finish(&mut blocks, options, &mut timings)?;
    let data_spool_len = data_spool.out.position;
    let staged_logical_block_bytes = blocks.iter().try_fold(0u64, |sum, block| {
        sum.checked_add(block.logical_size as u64)
            .ok_or_else(|| limit_error("staged logical block byte count overflow"))
    })?;
    let staged_stored_block_bytes = blocks.iter().try_fold(0u64, |sum, block| {
        sum.checked_add(block.stored_size as u64)
            .ok_or_else(|| limit_error("staged stored block byte count overflow"))
    })?;
    let raw_block_count = u32::try_from(blocks.iter().filter(|block| block.codec == 0).count())
        .map_err(|_| limit_error("RAW block count exceeds u32"))?;
    let zstd_block_count = u32::try_from(blocks.iter().filter(|block| block.codec == 1).count())
        .map_err(|_| limit_error("Zstandard block count exceeds u32"))?;
    // Stage selected-source opaque media first, preserving each application's
    // opaque Media indexes as one contiguous Catalog range.
    for app in &mut apps {
        if app.selected_snapshot != NO_INDEX {
            process_selected_opaque(
                &mut snapshots[app.selected_snapshot as usize],
                app,
                &mut media_stage,
                &mut media_spool,
                data_spool_len,
                options,
                &mut budget,
                &mut peak_temp,
            )?;
        }
    }
    for app in &apps {
        let end = app
            .first_snapshot
            .checked_add(app.snapshot_count)
            .ok_or_else(|| limit_error("Snapshot range overflow"))?;
        for index in app.first_snapshot..end {
            finish_metadata(
                &mut snapshots[index as usize],
                &mut media_stage,
                &mut media_spool,
                data_spool_len,
                options,
                &mut budget,
                &mut peak_temp,
            )?;
        }
    }
    for (index, snapshot) in snapshots.iter().enumerate() {
        for dependency in &snapshot.dependencies {
            if let Some(target) = seen_identities.get(&dependency.target_snapshot_identity) {
                if *target as usize == index {
                    return Err(invalid("snapshot cannot depend on itself"));
                }
                if let Some(content_id) = dependency.content_id {
                    if content_id != snapshots[*target as usize].content_id {
                        return Err(invalid(
                            "dependency Content ID disagrees with the named in-archive snapshot",
                        ));
                    }
                }
            }
        }
    }
    let catalog = build_catalog(
        &apps,
        &programs,
        &snapshots,
        &resource_extensions,
        &mut budget,
    )?;
    let manifest = build_manifest(&directories, &files, &block_refs, options, &mut budget)?;
    let (media_header_records, media_payload_start, media_size) = media_layout(&mut media_stage)?;
    let media_present = !media_stage.records.is_empty();
    let block_count =
        u32::try_from(blocks.len()).map_err(|_| limit_error("block count exceeds u32"))?;
    let index_size = checked_table_bytes(blocks.len(), BLOCK_SIZE)?;
    let check_total = data_spool.checks.len() as u64;
    let integrity_size = check_total
        .checked_mul(4)
        .ok_or_else(|| limit_error("integrity table size overflow"))?;
    let data_end = data_spool.out.position;
    let (data_offset, data_size) = if blocks.is_empty() {
        (0, 0)
    } else {
        (DATA_START, data_end - DATA_START)
    };
    // The tail is packed after the Data region (§1).
    let mut planned = data_end;
    let mut place = |size: u64| -> Result<u64, WriterError> {
        let at = align_up(planned, 16)?;
        planned = at
            .checked_add(size)
            .ok_or_else(|| limit_error("planned archive overflows u64"))?;
        Ok(at)
    };
    let media_offset = if media_present { place(media_size)? } else { 0 };
    let catalog_offset = place(catalog.len() as u64)?;
    let manifest_offset = place(manifest.len() as u64)?;
    let (block_index_offset, integrity_offset) = if blocks.is_empty() {
        (0, 0)
    } else {
        (place(index_size)?, place(integrity_size)?)
    };
    let planned_size = planned;
    if planned_size > options.max_archive_size {
        return Err(limit_error("planned archive exceeds configured size limit"));
    }
    check_temporary_size(
        planned_size
            .checked_add(media_stage.bytes)
            .ok_or_else(|| limit_error("temporary disk accounting overflow"))?,
        options,
    )?;
    let archive_write_started = Instant::now();
    let out = &mut data_spool.out;
    if media_present {
        out.pad(16, options)?;
        if out.position != media_offset {
            return Err(invalid("Media region is not at its planned offset"));
        }
        write_media_region(
            &mut out.file,
            &mut out.position,
            media_offset,
            &media_header_records,
            media_payload_start,
            &media_stage,
            &mut media_spool,
            0,
            options,
            &mut peak_temp,
        )?;
    }
    out.pad(16, options)?;
    out.write(&catalog, options)?;
    out.pad(16, options)?;
    out.write(&manifest, options)?;
    let mut index_hash = Sha256::new();
    if !blocks.is_empty() {
        out.pad(16, options)?;
        if out.position != block_index_offset {
            return Err(invalid("block index is not at its planned offset"));
        }
        for block in &blocks {
            let mut record = [0u8; 24];
            record[..8].copy_from_slice(&block.data_offset.to_le_bytes());
            record[8..12].copy_from_slice(&block.stored_size.to_le_bytes());
            record[12..16].copy_from_slice(&block.logical_size.to_le_bytes());
            record[16] = block.codec;
            record[20..24].copy_from_slice(&block.first_check.to_le_bytes());
            index_hash.update(&record);
            out.write(&record, options)?;
        }
        out.pad(16, options)?;
        if out.position != integrity_offset {
            return Err(invalid("integrity table is not at its planned offset"));
        }
        let mut chunk = Vec::with_capacity(64 * 1024);
        for group in data_spool.checks.chunks(16 * 1024) {
            chunk.clear();
            for check in group {
                chunk.extend_from_slice(&check.to_le_bytes());
            }
            index_hash.update(&chunk);
            out.write(&chunk, options)?;
        }
    }
    let archive_size = out.position;
    if archive_size != planned_size {
        return Err(WriterError::new(
            WriterErrorKind::Integrity,
            "serialized archive size differs from the planned section layout",
        ));
    }
    if archive_size > MAX_ARCHIVE_SIZE {
        return Err(limit_error("archive exceeds INT64_MAX"));
    }
    peak_temp = peak_temp.max(
        archive_size
            .checked_add(media_stage.bytes)
            .ok_or_else(|| limit_error("temporary disk estimate overflow"))?,
    );
    let header = encode_header(&HeaderFields {
        source_resources: !resource_extensions.is_empty(),
        archive_size,
        catalog_offset,
        catalog_size: catalog.len() as u64,
        media_offset,
        media_size: if media_present { media_size } else { 0 },
        data_offset,
        data_size,
        manifest_offset,
        manifest_size: manifest.len() as u64,
        block_index_offset,
        block_count: blocks.len() as u64,
        integrity_offset,
        integrity_size,
        catalog_sha256: sha256(&catalog),
        manifest_sha256: sha256(&manifest),
        index_sha256: index_hash.finish(),
        generation: 0,
    });
    out.file.flush()?;
    for slot in HEADER_SLOTS {
        out.file.get_mut().seek(SeekFrom::Start(slot))?;
        out.file.get_mut().write_all(&header)?;
    }
    timings.archive_write = archive_write_started.elapsed();
    let verification_started = Instant::now();
    if options.verify_output_payloads {
        let largest = blocks
            .iter()
            .map(|b| b.stored_size as u64)
            .max()
            .unwrap_or(0);
        budget.charge(largest, "output readback buffer")?;
    }
    verify_written_output(
        out.file.get_mut(),
        &header,
        &media_stage,
        &blocks,
        &out.stored_crcs,
        options,
        readback_estimate,
    )?;
    timings.output_verification = verification_started.elapsed();
    let sync_started = Instant::now();
    if options.sync_output {
        data_spool.out.file.get_ref().sync_all()?;
    }
    timings.output_sync = sync_started.elapsed();
    drop(data_spool);
    cancelled(options)?;
    let publication_started = Instant::now();
    replace_file(&output_guard.path, destination, options.sync_output)?;
    timings.atomic_publication = publication_started.elapsed();
    output_guard.disarm();
    let elapsed = started.elapsed();
    let rate = if elapsed.as_nanos() == 0 {
        0
    } else {
        ((input_counts.input_bytes as u128 * 1_000_000_000) / elapsed.as_nanos())
            .min(u64::MAX as u128) as u64
    };
    Ok(WriterStats {
        input_bytes: input_counts.input_bytes,
        archive_bytes: archive_size,
        file_entries: input_counts.file_entries as u32,
        file_records: files.len() as u32,
        directory_records: directories.len() as u32,
        block_count,
        deduplicated_blocks,
        dedup_index_bytes: block_dedup.estimated_bytes,
        staged_block_bytes: 0,
        staged_logical_block_bytes,
        staged_stored_block_bytes,
        raw_block_count,
        zstd_block_count,
        media_records: media_stage.records.len() as u32,
        elapsed,
        input_bytes_per_second: rate,
        accounted_memory_bytes: budget.peak,
        peak_temporary_disk_bytes: peak_temp,
        input_read_hash_time: timings.input_read_hash,
        input_read_time: timings.input_read,
        input_hash_time: timings.input_hash,
        block_staging_write_time: timings.block_staging_write,
        compression_stage_time: timings.compression_stage,
        compression_staging_read_time: timings.compression_staging_read,
        compression_staging_read_bytes: timings.compression_staging_read_bytes,
        compression_queue_peak_bytes: timings.compression_queue_peak,
        global_dedup_time: timings.global_dedup,
        compression_time: timings.compression,
        archive_write_time: timings.archive_write,
        output_verification_time: timings.output_verification,
        output_sync_time: timings.output_sync,
        atomic_publication_time: timings.atomic_publication,
        compression_context_peak_bytes: timings.compression_context_peak,
    })
}

/// Dependency target for a snapshot written earlier in the same write, whose
/// identity is not known yet: the same application's snapshot of this type
/// and version. The writer replaces it with that snapshot's identity.
pub(crate) fn pending_dependency_target(snapshot_type: SnapshotType, version: u32) -> [u8; 32] {
    let mut target = [0u8; 32];
    target[..16].copy_from_slice(b"NX-PENDING-SNAP\0");
    target[16] = snapshot_type as u8;
    target[17..21].copy_from_slice(&version.to_le_bytes());
    target
}
fn pending_dependency(target: &[u8; 32]) -> Option<(u8, u32)> {
    (target[..16] == *b"NX-PENDING-SNAP\0" && target[21..] == [0; 11]).then(|| {
        (
            target[16],
            u32::from_le_bytes(target[17..21].try_into().unwrap()),
        )
    })
}

/// First byte of the Data region; the two header slots live below it.
pub(crate) const HEADER_PAGE_SIZE: usize = 4096;
pub(crate) const DATA_START: u64 = (2 * HEADER_PAGE_SIZE) as u64;
pub(crate) const PAGED_HEADERS: u16 = 1;
/// Offsets of header slot A and slot B (§1).
pub(crate) const HEADER_SLOTS: [u64; 2] = [0, HEADER_PAGE_SIZE as u64];

pub(crate) struct HeaderFields {
    pub source_resources: bool,
    pub archive_size: u64,
    pub catalog_offset: u64,
    pub catalog_size: u64,
    pub media_offset: u64,
    pub media_size: u64,
    pub data_offset: u64,
    pub data_size: u64,
    pub manifest_offset: u64,
    pub manifest_size: u64,
    pub block_index_offset: u64,
    pub block_count: u64,
    pub integrity_offset: u64,
    pub integrity_size: u64,
    pub catalog_sha256: [u8; 32],
    pub manifest_sha256: [u8; 32],
    pub index_sha256: [u8; 32],
    pub generation: u64,
}

pub(crate) fn encode_header(h: &HeaderFields) -> [u8; 256] {
    let mut header = [0u8; 256];
    header[..4].copy_from_slice(b"NXAR");
    write_u16_at(&mut header, 4, 0);
    write_u16_at(&mut header, 6, if h.source_resources { 3 } else { 2 });
    write_u32_at(&mut header, 8, 256);
    write_u16_at(&mut header, 12, PAGED_HEADERS);
    write_u64_at(&mut header, 16, h.archive_size);
    write_u64_at(&mut header, 24, h.catalog_offset);
    write_u64_at(&mut header, 32, h.catalog_size);
    write_u64_at(&mut header, 40, h.media_offset);
    write_u64_at(&mut header, 48, h.media_size);
    write_u64_at(&mut header, 56, h.data_offset);
    write_u64_at(&mut header, 64, h.data_size);
    write_u64_at(&mut header, 72, h.manifest_offset);
    write_u64_at(&mut header, 80, h.manifest_size);
    write_u64_at(&mut header, 88, h.block_index_offset);
    write_u64_at(&mut header, 96, h.block_count);
    write_u64_at(&mut header, 104, h.integrity_offset);
    write_u64_at(&mut header, 112, h.integrity_size);
    header[120..152].copy_from_slice(&h.catalog_sha256);
    header[152..184].copy_from_slice(&h.manifest_sha256);
    header[184..216].copy_from_slice(&h.index_sha256);
    write_u64_at(&mut header, 216, h.generation);
    write_u32_at(&mut header, 224, crate::hash::CHECK_SIZE as u32);
    let crc = crc32c(&header[..252]);
    write_u32_at(&mut header, 252, crc);
    header
}

// Read back the exact planned bytes before replacing an existing archive.
// This catches staging/copy corruption; conformance parsing stays independent.
fn hash_file_range(
    file: &mut File,
    offset: u64,
    len: u64,
    hash: &mut Sha256,
    options: &WriterOptions,
) -> Result<(), WriterError> {
    file.seek(SeekFrom::Start(offset))?;
    let mut remaining = len;
    let mut scratch = [0u8; 64 * 1024];
    while remaining != 0 {
        cancelled(options)?;
        let n = remaining.min(scratch.len() as u64) as usize;
        file.read_exact(&mut scratch[..n])?;
        hash.update(&scratch[..n]);
        remaining -= n as u64;
    }
    Ok(())
}
fn verify_written_output(
    file: &mut File,
    expected: &[u8; 256],
    media: &MediaStage,
    blocks: &[BlockBuild],
    stored_crcs: &[u32],
    options: &WriterOptions,
    readback_estimate: u64,
) -> Result<(), WriterError> {
    let get = |at: usize| u64::from_le_bytes(expected[at..at + 8].try_into().unwrap());
    let mut header = [0u8; 256];
    for slot in HEADER_SLOTS {
        file.seek(SeekFrom::Start(slot))?;
        file.read_exact(&mut header)?;
        if header != *expected {
            return Err(WriterError::new(
                WriterErrorKind::Integrity,
                "output header slot differs from the serialization plan",
            ));
        }
    }
    if file.metadata()?.len() != get(16) {
        return Err(WriterError::new(
            WriterErrorKind::Integrity,
            "output physical size differs from the serialization plan",
        ));
    }
    for (offset, length, digest) in [(get(24), get(32), 120usize), (get(72), get(80), 152usize)] {
        let mut hash = Sha256::new();
        hash_file_range(file, offset, length, &mut hash, options)?;
        if hash.finish().as_slice() != &expected[digest..digest + 32] {
            return Err(WriterError::new(
                WriterErrorKind::Integrity,
                "output section hash mismatch",
            ));
        }
    }
    let mut hash = Sha256::new();
    hash_file_range(file, get(88), get(96) * 24, &mut hash, options)?;
    hash_file_range(file, get(104), get(112), &mut hash, options)?;
    if hash.finish().as_slice() != &expected[184..216] {
        return Err(WriterError::new(
            WriterErrorKind::Integrity,
            "output index hash mismatch",
        ));
    }
    // Block payloads: one sequential re-read checked against the CRC-32C
    // taken when each stored payload was written (no decompression).
    if options.verify_output_payloads {
        if stored_crcs.len() != blocks.len() {
            return Err(WriterError::new(
                WriterErrorKind::Integrity,
                "output block readback is incomplete",
            ));
        }
        let largest = blocks.iter().map(|b| b.stored_size).max().unwrap_or(0);
        let mut buffer = vec![0u8; largest as usize];
        let stored_total: u64 = blocks.iter().map(|b| b.stored_size as u64).sum();
        let mut read = 0u64;
        let mut reported = 0u64;
        let mut report = |read: u64, last: bool| {
            let units = if last || stored_total == 0 {
                readback_estimate
            } else {
                (readback_estimate as u128 * read as u128 / stored_total as u128) as u64
            };
            if units > reported {
                options.progress.add_done(units - reported);
                reported = units;
            }
        };
        // Payloads are mostly contiguous: one buffered sequential read.
        let mut reader = io::BufReader::with_capacity(8 << 20, &mut *file);
        let mut position = u64::MAX;
        for (block, &crc) in blocks.iter().zip(stored_crcs) {
            cancelled(options)?;
            let stored = &mut buffer[..block.stored_size as usize];
            if position != block.data_offset {
                reader.seek(SeekFrom::Start(block.data_offset))?;
            }
            reader.read_exact(stored)?;
            position = block.data_offset + block.stored_size as u64;
            if crc32c(stored) != crc {
                return Err(WriterError::new(
                    WriterErrorKind::Integrity,
                    "output block payload differs from what was written",
                ));
            }
            read += block.stored_size as u64;
            report(read, false);
        }
        report(read, true);
    }
    // Media headers/descriptors are not covered by a section hash.
    if !media.records.is_empty() {
        let mut copy = [0u8; 64];
        let mut expected_header = [0u8; 32];
        expected_header[..4].copy_from_slice(b"NXMD");
        write_u16_at(&mut expected_header, 4, 1);
        write_u16_at(&mut expected_header, 6, 32);
        write_u64_at(&mut expected_header, 8, get(48));
        write_u32_at(&mut expected_header, 16, media.records.len() as u32);
        write_u16_at(&mut expected_header, 20, 64);
        write_u32_at(&mut expected_header, 24, 32);
        write_u32_at(
            &mut expected_header,
            28,
            align_up(32 + media.records.len() as u64 * 64, 16)? as u32,
        );
        file.seek(SeekFrom::Start(get(40)))?;
        file.read_exact(&mut copy[..32])?;
        if copy[..32] != expected_header {
            return Err(WriterError::new(
                WriterErrorKind::Integrity,
                "output Media header mismatch",
            ));
        }
        for (i, record) in media.records.iter().enumerate() {
            file.seek(SeekFrom::Start(get(40) + 32 + i as u64 * 64))?;
            file.read_exact(&mut copy)?;
            let mut planned = [0u8; 64];
            write_u16_at(&mut planned, 0, record.media_type);
            write_u64_at(&mut planned, 4, record.size);
            write_u64_at(&mut planned, 12, record.size);
            write_u64_at(&mut planned, 20, record.data_offset);
            planned[28..60].copy_from_slice(&record.digest);
            if copy != planned {
                return Err(WriterError::new(
                    WriterErrorKind::Integrity,
                    "output Media descriptor mismatch",
                ));
            }
            if options.verify_output_payloads {
                let mut hash = Sha256::new();
                hash_file_range(
                    file,
                    get(40) + record.data_offset,
                    record.size,
                    &mut hash,
                    options,
                )?;
                if hash.finish() != record.digest {
                    return Err(WriterError::new(
                        WriterErrorKind::Integrity,
                        "output Media payload hash mismatch",
                    ));
                }
            }
        }
    }
    #[cfg(test)]
    CANCEL_DURING_VERIFICATION_TEST.with(|flag| {
        if flag.replace(false) {
            options.cancellation.cancel();
        }
    });
    cancelled(options)?;
    Ok(())
}

// Check cancellation between fragmented reads, including Interrupted retries.
/// Reads file content and counts it as progress.
fn read_exact_cancelled(
    reader: &mut dyn Read,
    bytes: &mut [u8],
    options: &WriterOptions,
) -> Result<(), WriterError> {
    read_exact_inner(reader, bytes, options, true)
}
/// Reads NPDM and media: not part of the progress total.
fn read_exact_uncounted(
    reader: &mut dyn Read,
    bytes: &mut [u8],
    options: &WriterOptions,
) -> Result<(), WriterError> {
    read_exact_inner(reader, bytes, options, false)
}
fn read_exact_inner(
    reader: &mut dyn Read,
    mut bytes: &mut [u8],
    options: &WriterOptions,
    count: bool,
) -> Result<(), WriterError> {
    while !bytes.is_empty() {
        cancelled(options)?;
        match reader.read(bytes) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(n) => {
                if count {
                    options.progress.add_done(n as u64);
                }
                bytes = &mut bytes[n..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    cancelled(options)?;
    Ok(())
}
fn compare_spool_ranges<R: Read + Seek>(
    file: &mut R,
    a: u64,
    b: u64,
    len: u64,
    options: &WriterOptions,
) -> Result<bool, WriterError> {
    let mut left = [0u8; 64 * 1024];
    let mut right = [0u8; 64 * 1024];
    let mut done = 0;
    while done < len {
        cancelled(options)?;
        let n = (len - done).min(left.len() as u64) as usize;
        file.seek(SeekFrom::Start(a + done))?;
        file.read_exact(&mut left[..n])?;
        file.seek(SeekFrom::Start(b + done))?;
        file.read_exact(&mut right[..n])?;
        if left[..n] != right[..n] {
            file.seek(SeekFrom::End(0))?;
            return Ok(false);
        }
        done += n as u64;
    }
    file.seek(SeekFrom::End(0))?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn add_block(
    logical_size: u32,
    buffer: &[u8],
    sink: &mut DataSink,
    blocks: &mut Vec<BlockBuild>,
    index: &mut BlockDedupIndex,
    encoder: &mut Option<BlockEncoder>,
    budget: &mut MemoryBudget,
    options: &WriterOptions,
    timings: &mut WriterTimings,
) -> Result<(u32, bool), WriterError> {
    let bytes = &buffer[..logical_size as usize];
    let hash_started = Instant::now();
    let mark = sink.checks.len();
    sink.checks
        .try_reserve(check_count(bytes.len()))
        .map_err(|_| limit_error("unable to reserve integrity table"))?;
    slice_checks(bytes, &mut sink.checks);
    let key = BlockKey {
        logical_size,
        digest: checks_key(&sink.checks[mark..]),
    };
    timings.input_hash += hash_started.elapsed();
    let dedup_started = Instant::now();
    let candidates: Vec<u32> = index.buckets.get(&key).map_or_else(Vec::new, |bucket| {
        std::iter::once(bucket.first_id)
            .chain(bucket.collision_ids.iter().copied())
            .collect()
    });
    for id in candidates {
        cancelled(options)?;
        let block = &blocks[id as usize];
        let first = block.first_check as usize;
        let same_checks = block.logical_size == logical_size
            && sink.checks[first..first + (sink.checks.len() - mark)] == sink.checks[mark..];
        if same_checks && sink.block_equals(id, bytes, blocks, options, timings)? {
            sink.checks.truncate(mark);
            timings.global_dedup += dedup_started.elapsed();
            return Ok((id, true));
        }
    }
    timings.global_dedup += dedup_started.elapsed();

    let id = u32::try_from(blocks.len()).map_err(|_| limit_error("block count exceeds u32 IDs"))?;
    let first_check =
        u32::try_from(mark).map_err(|_| limit_error("integrity check count exceeds u32"))?;
    blocks
        .try_reserve(1)
        .map_err(|_| limit_error("unable to reserve block table"))?;
    let digest_collision = index.buckets.contains_key(&key);
    if digest_collision {
        index.charge(DEDUP_COLLISION_ESTIMATE, options, budget)?;
        let bucket = index.buckets.get_mut(&key).ok_or_else(|| {
            WriterError::new(WriterErrorKind::Integrity, "dedup bucket disappeared")
        })?;
        bucket
            .collision_ids
            .try_reserve(1)
            .map_err(|_| limit_error("unable to reserve hash-collision candidates"))?;
        bucket.collision_ids.push(id);
    } else {
        index.charge(DEDUP_BUCKET_ESTIMATE, options, budget)?;
        index
            .buckets
            .try_reserve(1)
            .map_err(|_| limit_error("unable to reserve block-deduplication index"))?;
        index.buckets.insert(
            key,
            BlockBucket {
                first_id: id,
                collision_ids: Vec::new(),
            },
        );
    }
    budget.charge(
        40 + 4 * check_count(bytes.len()) as u64,
        "block and integrity metadata",
    )?;
    blocks.push(BlockBuild {
        logical_size,
        stored_size: logical_size,
        codec: 0,
        first_check,
        data_offset: 0,
    });
    if options.compression_mode != CompressionMode::ZstandardAuto {
        sink.out
            .write_block(id, &mut blocks[id as usize], 0, bytes, options, timings)?;
    } else if let Some(pipeline) = sink.pipeline.as_mut() {
        let mut input = Vec::new();
        input
            .try_reserve_exact(bytes.len())
            .map_err(|_| limit_error("unable to reserve compression input"))?;
        input.extend_from_slice(bytes);
        pipeline.submit(id, input, &mut sink.out, blocks, options, timings)?;
    } else {
        if encoder.is_none() {
            *encoder = Some(BlockEncoder::new(options, budget)?);
        }
        let block_encoder = encoder.as_mut().expect("encoder was initialized");
        let frame_len = block_encoder.encode(bytes, options, timings)?;
        if keep_compressed(frame_len, bytes.len(), options) {
            let frame = &block_encoder.frame[..frame_len];
            if options.verify_output_payloads {
                let started = Instant::now();
                verify_frame(frame, bytes, &mut sink.decoder, &mut sink.compare)?;
                timings.output_verification += started.elapsed();
            }
            sink.out
                .write_block(id, &mut blocks[id as usize], 1, frame, options, timings)?;
        } else {
            sink.out
                .write_block(id, &mut blocks[id as usize], 0, bytes, options, timings)?;
        }
    }
    Ok((id, false))
}

#[allow(clippy::too_many_arguments)]
fn process_file(
    source: &mut SourceFile,
    app_index: u32,
    program_index: u32,
    data_spool: &mut DataSink,
    blocks: &mut Vec<BlockBuild>,
    files: &mut Vec<FileBuild>,
    block_refs: &mut Vec<u32>,
    dedup_index: &mut BlockDedupIndex,
    block_buffer: &mut Vec<u8>,
    block_encoder: &mut Option<BlockEncoder>,
    same_path: &mut HashMap<(u32, u32, Vec<u8>), Vec<u32>>,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
    deduplicated_blocks: &mut u64,
    timings: &mut WriterTimings,
) -> Result<u32, WriterError> {
    let block_count = if source.size == 0 {
        0
    } else {
        1 + (source.size - 1) / source.chunk_size_bytes as u64
    };
    let block_count_usize = usize::try_from(block_count)
        .map_err(|_| limit_error("file block-reference count exceeds host address space"))?;
    budget.charge(
        block_count
            .checked_mul(4)
            .ok_or_else(|| limit_error("file reference estimate overflow"))?,
        "per-file block reference scratch",
    )?;
    let mut ids = Vec::new();
    ids.try_reserve_exact(block_count_usize)
        .map_err(|_| limit_error("unable to reserve file block-reference scratch"))?;
    let mut remaining = source.size;
    if source.size > 0 && block_buffer.is_empty() {
        budget.charge(
            options.chunk_size_bytes as u64,
            "reusable logical input chunk buffer",
        )?;
        block_buffer
            .try_reserve_exact(options.chunk_size_bytes as usize)
            .map_err(|_| limit_error("unable to reserve reusable logical input chunk buffer"))?;
        block_buffer.resize(options.chunk_size_bytes as usize, 0);
    }
    let mut file_hash = Sha256::new();
    // Probe the first chunk: a file that keeps Zstandard there switches to
    // smaller chunks, so a cache miss decodes less. Deterministic per content.
    let small = options.compressed_chunk_bytes;
    if options.compression_mode == CompressionMode::ZstandardAuto
        && small != 0
        && source.chunk_size_bytes > small
        && source.size > small as u64
    {
        cancelled(options)?;
        let n = remaining.min(source.chunk_size_bytes as u64) as usize;
        let input_started = Instant::now();
        read_exact_cancelled(&mut *source.reader, &mut block_buffer[..n], options)?;
        timings.input_read += input_started.elapsed();
        let hash_started = Instant::now();
        file_hash.update(&block_buffer[..n]);
        timings.input_hash += hash_started.elapsed();
        timings.input_read_hash += input_started.elapsed();
        if block_encoder.is_none() {
            *block_encoder = Some(BlockEncoder::new(options, budget)?);
        }
        let encoder = block_encoder.as_mut().expect("encoder was initialized");
        let frame_len = encoder.encode(&block_buffer[..n], options, timings)?;
        if keep_compressed(frame_len, n, options) {
            source.chunk_size_bytes = small;
            let more = (1 + (source.size - 1) / small as u64) as usize;
            budget.charge(
                (more.saturating_sub(block_count_usize) * 4) as u64,
                "per-file block reference scratch",
            )?;
            ids.try_reserve_exact(more)
                .map_err(|_| limit_error("unable to reserve file block-reference scratch"))?;
        }
        let piece = source.chunk_size_bytes as usize;
        for at in (0..n).step_by(piece) {
            let len = piece.min(n - at);
            let (id, reused_block) = add_block(
                len as u32,
                &block_buffer[at..at + len],
                data_spool,
                blocks,
                dedup_index,
                block_encoder,
                budget,
                options,
                timings,
            )?;
            if reused_block {
                *deduplicated_blocks = deduplicated_blocks
                    .checked_add(1)
                    .ok_or_else(|| limit_error("deduplicated block count overflow"))?;
            }
            ids.push(id);
        }
        remaining -= n as u64;
    }
    while remaining > 0 {
        cancelled(options)?;
        let n = remaining.min(source.chunk_size_bytes as u64) as usize;
        let input_started = Instant::now();
        read_exact_cancelled(&mut *source.reader, &mut block_buffer[..n], options)?;
        timings.input_read += input_started.elapsed();
        let bytes = &block_buffer[..n];
        let hash_started = Instant::now();
        file_hash.update(bytes);
        timings.input_hash += hash_started.elapsed();
        timings.input_read_hash += input_started.elapsed();
        let (id, reused_block) = add_block(
            n as u32,
            bytes,
            data_spool,
            blocks,
            dedup_index,
            block_encoder,
            budget,
            options,
            timings,
        )?;
        if reused_block {
            *deduplicated_blocks = deduplicated_blocks
                .checked_add(1)
                .ok_or_else(|| limit_error("deduplicated block count overflow"))?;
        }
        ids.push(id);
        remaining -= n as u64;
    }
    let mut extra = [0u8; 1];
    if source.reader.read(&mut extra)? != 0 {
        return Err(invalid(
            "file stream contains bytes beyond its declared logical size",
        ));
    }
    cancelled(options)?;
    let hash_started = Instant::now();
    let digest = file_hash.finish();
    if source
        .expected_digest
        .is_some_and(|expected| expected != digest)
    {
        return Err(invalid(
            "captured profile SHA-256 differs from streamed file",
        ));
    }
    timings.input_hash += hash_started.elapsed();
    let key = (app_index, program_index, source.path.clone());
    let mut reused = None;
    if let Some(candidates) = same_path.get(&key) {
        for &candidate in candidates {
            let old = &files[candidate as usize];
            if old.size == source.size
                && old.chunk_size_bytes == source.chunk_size_bytes
                && old.digest == digest
                && old.block_count as usize == ids.len()
            {
                let old_start = old.first_ref as usize;
                let mut equal = true;
                for (n, &new_id) in ids.iter().enumerate() {
                    let old_id = block_refs[old_start + n];
                    let a = &blocks[old_id as usize];
                    let b = &blocks[new_id as usize];
                    if old_id != new_id || a.logical_size != b.logical_size {
                        equal = false;
                        break;
                    }
                }
                if equal {
                    reused = Some(candidate);
                    break;
                }
            }
        }
    }
    if let Some(id) = reused {
        return Ok(id);
    }
    if files.len() >= 33_554_432 {
        return Err(limit_error("File record count exceeds NX 0.1 limit"));
    }
    let id = u32::try_from(files.len()).map_err(|_| limit_error("File index exceeds u32"))?;
    let first_ref = u32::try_from(block_refs.len())
        .map_err(|_| limit_error("block reference table exceeds u32"))?;
    let block_count_u32 =
        u32::try_from(ids.len()).map_err(|_| limit_error("one file block count exceeds u32"))?;
    block_refs
        .try_reserve(ids.len())
        .map_err(|_| limit_error("unable to reserve block-reference table"))?;
    block_refs.extend_from_slice(&ids);
    files
        .try_reserve(1)
        .map_err(|_| limit_error("unable to reserve file table"))?;
    files.push(FileBuild {
        size: source.size,
        digest,
        chunk_size_bytes: source.chunk_size_bytes,
        first_ref,
        block_count: block_count_u32,
    });
    same_path.entry(key).or_default().push(id);
    budget.charge(128, "file record and same-path index")?;
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
fn snapshot_identity(
    platform: u8,
    application_id: u64,
    program_id: Option<u64>,
    snapshot_type: u8,
    version: u32,
    content_id: [u8; 16],
    nodes: &mut [IdentityNode],
    file_ids: &[u32],
    files: &[FileBuild],
) -> Result<[u8; 32], WriterError> {
    nodes.sort_by(|a, b| a.path.cmp(&b.path));
    if nodes.len() > u32::MAX as usize {
        return Err(limit_error("snapshot identity node count exceeds u32"));
    }
    if nodes.windows(2).any(|w| w[0].path == w[1].path) {
        return Err(invalid("logical tree contains a duplicate full path"));
    }
    let mut hash = Sha256::new();
    hash.update(b"NX-SNAPSHOT-ID\0");
    hash.update(&3u16.to_le_bytes());
    hash.update(&[platform]);
    hash.update(&application_id.to_le_bytes());
    hash.update(&program_id.unwrap_or(u64::MAX).to_le_bytes());
    hash.update(&[snapshot_type]);
    hash.update(&version.to_le_bytes());
    hash.update(&content_id);
    hash.update(&(nodes.len() as u32).to_le_bytes());
    for node in nodes {
        if node.path.len() > u32::MAX as usize {
            return Err(limit_error("logical full path exceeds u32"));
        }
        hash.update(&[node.kind]);
        hash.update(&(node.path.len() as u32).to_le_bytes());
        hash.update(&node.path);
        if let Some(source) = node.source_file {
            let id = *file_ids
                .get(source as usize)
                .ok_or_else(|| invalid("identity file source is out of range"))?;
            let file = files
                .get(id as usize)
                .ok_or_else(|| invalid("identity File ID is out of range"))?;
            hash.update(&file.size.to_le_bytes());
            hash.update(&file.digest);
        }
    }
    Ok(hash.finish())
}

#[allow(clippy::too_many_arguments)]
fn process_tree(
    root: DirectoryInput,
    app_index: u32,
    program_index: u32,
    platform: u8,
    application_id: u64,
    source_program_id: Option<u64>,
    snapshot_type: u8,
    version: u32,
    content_id: [u8; 16],
    directories: &mut Vec<DirectoryBuild>,
    files: &mut Vec<FileBuild>,
    block_refs: &mut Vec<u32>,
    blocks: &mut Vec<BlockBuild>,
    dedup_index: &mut BlockDedupIndex,
    block_buffer: &mut Vec<u8>,
    block_encoder: &mut Option<BlockEncoder>,
    same_path: &mut HashMap<(u32, u32, Vec<u8>), Vec<u32>>,
    data_spool: &mut DataSink,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
    deduplicated_blocks: &mut u64,
    timings: &mut WriterTimings,
) -> Result<(u32, u64, [u8; 32]), WriterError> {
    fn validate_profiles(
        dir: &DirectoryInput,
        path: &[u8],
        expected: &mut Option<[u8; 32]>,
        scope: (u8, u64, Option<u64>, u8, u32, [u8; 16]),
    ) -> Result<(), WriterError> {
        for entry in &dir.entries {
            let path = full_path(path, &entry.name)?;
            match &entry.node {
                NodeInput::Directory(child) => validate_profiles(child, &path, expected, scope)?,
                NodeInput::File(file) => {
                    if let Some(p) = &file.captured {
                        let s = &p.scope;
                        if (
                            s.platform,
                            s.application_id,
                            s.program_id,
                            s.snapshot_type,
                            s.numeric_version,
                            s.content_id,
                        ) != scope
                            || p.path.as_bytes() != path
                            || p.size != file.size
                            || expected.is_some_and(|id| id != s.snapshot_identity)
                        {
                            return Err(invalid("captured profile scope/path mismatch"));
                        }
                        *expected = Some(s.snapshot_identity);
                    }
                }
            }
        }
        Ok(())
    }
    let mut expected_identity = None;
    validate_profiles(
        &root,
        &[],
        &mut expected_identity,
        (
            platform,
            application_id,
            source_program_id,
            snapshot_type,
            version,
            content_id,
        ),
    )?;
    let mut tree = PreparedTree {
        directories: Vec::new(),
        files: Vec::new(),
        nodes: Vec::new(),
    };
    let local_root = flatten_directory(root, &[], 0, &mut tree, budget, options)?;
    if local_root != 0 {
        return Err(invalid("root directory was not assigned index zero"));
    }
    let mut file_ids = vec![NO_INDEX; tree.files.len()];
    let mut order: Vec<usize> = (0..tree.files.len()).collect();
    order.sort_by(|a, b| tree.files[*a].path.cmp(&tree.files[*b].path));
    let mut logical_size = 0u64;
    let mut ahead = ReadAhead::new();
    for position in 0..order.len() {
        let index = order[position];
        cancelled(options)?;
        ahead.advance(position, &order, &tree.files);
        let source = &mut tree.files[index];
        logical_size = logical_size
            .checked_add(source.size)
            .ok_or_else(|| limit_error("snapshot logical content size overflow"))?;
        if logical_size > MAX_ARCHIVE_SIZE {
            return Err(limit_error(
                "snapshot logical content size exceeds INT64_MAX",
            ));
        }
        let id = if let Some(crate::model::FileShare::Alias(slot)) = &source.share {
            let id = *slot
                .get()
                .ok_or_else(|| invalid("aliased file is not written earlier in this archive"))?;
            if files.get(id as usize).map(|f| f.size) != Some(source.size) {
                return Err(invalid("aliased file size differs from its written file"));
            }
            id
        } else {
            process_file(
                source,
                app_index,
                program_index,
                data_spool,
                blocks,
                files,
                block_refs,
                dedup_index,
                block_buffer,
                block_encoder,
                same_path,
                options,
                budget,
                deduplicated_blocks,
                timings,
            )?
        };
        if let Some(crate::model::FileShare::Publish(slot)) = &source.share {
            if slot.set(id).is_err() && slot.get() != Some(&id) {
                return Err(invalid("one shared file slot names two written files"));
            }
        }
        file_ids[index] = id;
        ahead.finished(&tree.files[index]);
    }
    drop(ahead);
    let global_base =
        u32::try_from(directories.len()).map_err(|_| limit_error("directory index exceeds u32"))?;
    let total_dirs = directories
        .len()
        .checked_add(tree.directories.len())
        .ok_or_else(|| limit_error("directory count overflow"))?;
    if total_dirs > 16_777_216 {
        return Err(limit_error("Manifest directory count exceeds NX 0.1 limit"));
    }
    let mut global_map = Vec::new();
    global_map
        .try_reserve_exact(tree.directories.len())
        .map_err(|_| limit_error("unable to reserve directory remap"))?;
    for i in 0..tree.directories.len() {
        global_map.push(
            global_base
                .checked_add(i as u32)
                .ok_or_else(|| limit_error("directory ID overflow"))?,
        );
    }
    for directory in tree.directories {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(directory.entries.len())
            .map_err(|_| limit_error("unable to reserve Manifest entries"))?;
        for entry in directory.entries {
            let (kind, target) = match entry.target {
                TempTarget::Directory(local) => (
                    1,
                    *global_map
                        .get(local as usize)
                        .ok_or_else(|| invalid("child directory index is invalid"))?,
                ),
                TempTarget::SourceFile(source) => (
                    2,
                    *file_ids
                        .get(source as usize)
                        .ok_or_else(|| invalid("file source index is invalid"))?,
                ),
            };
            entries.push(FinalEntry {
                name: entry.name,
                kind,
                target,
            });
        }
        directories.push(DirectoryBuild { entries });
    }
    let root_global = global_map[0];
    let identity = snapshot_identity(
        platform,
        application_id,
        source_program_id,
        snapshot_type,
        version,
        content_id,
        &mut tree.nodes,
        &file_ids,
        files,
    )?;
    if expected_identity.is_some_and(|expected| expected != identity) {
        return Err(invalid(
            "captured profile canonical snapshot identity mismatch",
        ));
    }
    Ok((root_global, logical_size, identity))
}

struct SnapshotWork {
    app_index: u32,
    program_index: u32,
    source: SnapshotInput,
}
struct ApplicationBuild {
    application_id: u64,
    platform: u8,
    first_program: u32,
    program_count: u32,
    first_snapshot: u32,
    snapshot_count: u32,
    selected_snapshot: u32,
    current_opaque_first: u32,
    current_opaque_count: u32,
}
struct ProgramBuild {
    app_index: u32,
    program_id: u64,
    kind: u8,
    content_id: [u8; 16],
    has_content_id: bool,
    first_snapshot: u32,
    snapshot_count: u32,
    npdm: u32,
    npdm_input: Option<MediaInput>,
}

type InputPlan = (
    Vec<ApplicationBuild>,
    Vec<ProgramBuild>,
    Vec<SnapshotWork>,
    InputCounts,
);

fn plan_inputs(
    mut input: ArchiveInput,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
) -> Result<InputPlan, WriterError> {
    let counts = preflight(&mut input, options, budget)?;
    // Build ID is checked separately because it is deliberately excluded from
    // canonical logical identity.
    for app in &input.applications {
        for snapshot in &app.snapshots {
            fn check(dir: &DirectoryInput, build: [u8; 32]) -> Result<(), WriterError> {
                for entry in &dir.entries {
                    match &entry.node {
                        NodeInput::Directory(child) => check(child, build)?,
                        NodeInput::File(file) => {
                            if file
                                .captured
                                .as_ref()
                                .is_some_and(|p| p.scope.build_id != build)
                            {
                                return Err(invalid("captured profile Build ID mismatch"));
                            }
                        }
                    }
                }
                Ok(())
            }
            check(&snapshot.root, snapshot.build_id.unwrap_or([0; 32]))?;
        }
    }
    select_adaptive(&mut input, options, budget, counts.maximum_depth)?;
    if input.applications.len() > 4096 {
        return Err(limit_error("application count exceeds NX 0.1 limit"));
    }
    let total_programs: u64 = input
        .applications
        .iter()
        .map(|a| a.programs.len() as u64)
        .sum();
    let total_snapshots: u64 = input
        .applications
        .iter()
        .map(|a| a.snapshots.len() as u64)
        .sum();
    if total_programs > 16384 || total_snapshots > 65536 {
        return Err(limit_error(
            "Program or Snapshot table exceeds NX 0.1 limit",
        ));
    }
    let mut apps = Vec::new();
    let mut programs = Vec::new();
    let mut snapshots = Vec::new();
    let mut program_positions: HashMap<(u32, u64), u32> = HashMap::new();
    for (ai, app) in input.applications.iter_mut().enumerate() {
        let app_index = ai as u32;
        let application_id = app.application_id;
        let platform = app.platform as u8;
        let program_count = app.programs.len() as u32;
        let first_program = if program_count == 0 {
            NO_INDEX
        } else {
            u32::try_from(programs.len()).map_err(|_| limit_error("Program index exceeds u32"))?
        };
        let source_programs = std::mem::take(&mut app.programs);
        for p in source_programs {
            let pi = u32::try_from(programs.len())
                .map_err(|_| limit_error("Program index exceeds u32"))?;
            program_positions.insert((app_index, p.program_id), pi);
            let cid = p.content_id.unwrap_or([0; 16]);
            programs.push(ProgramBuild {
                app_index,
                program_id: p.program_id,
                kind: p.kind as u8,
                content_id: cid,
                has_content_id: p.content_id.is_some(),
                first_snapshot: 0,
                snapshot_count: 0,
                npdm: NO_INDEX,
                npdm_input: p.npdm,
            });
        }
        let first_snapshot = u32::try_from(snapshots.len())
            .map_err(|_| limit_error("Snapshot index exceeds u32"))?;
        let selected_local = app
            .snapshots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.snapshot_type != SnapshotType::Dlc && s.metadata.is_some())
            .max_by_key(|(i, s)| (s.numeric_version, *i))
            .map(|(i, _)| i);
        let selected_snapshot = selected_local
            .map(|i| first_snapshot + i as u32)
            .unwrap_or(NO_INDEX);
        let snapshot_sources = std::mem::take(&mut app.snapshots);
        for s in snapshot_sources {
            let program_index = match s.program_id {
                Some(id) => *program_positions
                    .get(&(app_index, id))
                    .ok_or_else(|| invalid("snapshot Program ID map is missing"))?,
                None => NO_INDEX,
            };
            snapshots.push(SnapshotWork {
                app_index,
                program_index,
                source: s,
            });
        }
        let snapshot_count = u32::try_from(snapshots.len())
            .map_err(|_| limit_error("Snapshot count exceeds u32"))?
            - first_snapshot;
        apps.push(ApplicationBuild {
            application_id,
            platform,
            first_program,
            program_count,
            first_snapshot,
            snapshot_count,
            selected_snapshot,
            current_opaque_first: 0,
            current_opaque_count: 0,
        });
    }
    for (i, work) in snapshots.iter().enumerate() {
        if work.program_index != NO_INDEX {
            let p = programs
                .get_mut(work.program_index as usize)
                .ok_or_else(|| invalid("snapshot Program index is invalid"))?;
            if p.snapshot_count == 0 {
                p.first_snapshot = i as u32;
            } else if p.first_snapshot.checked_add(p.snapshot_count) != Some(i as u32) {
                return Err(invalid("Program snapshots are not contiguous"));
            }
            p.snapshot_count += 1;
        }
    }
    if programs.iter().any(|p| p.snapshot_count == 0) {
        return Err(invalid("Program row has no snapshots"));
    }
    Ok((apps, programs, snapshots, counts))
}

#[path = "repack.rs"]
mod repack;
pub use repack::{
    add_native_content, add_native_contents, compact_archive, edit_archive_metadata,
    edit_archive_recipes, inspect_archive, inspect_snapshot_files, inspect_source_resources,
    read_snapshot_display, read_snapshot_file_range, read_snapshot_nacp, repack_archive,
    ApplicationSummary, ArchiveSummary, IntegrityPolicy, MetadataEdit, MetadataSourceSelection,
    NativeAddRequest, NativeAddStats, ProgramSummary, RepackPlan, RepackStats,
    SnapshotBlockReference, SnapshotDisplay, SnapshotFileReference, SnapshotSummary,
    SourceFileSummary, VerificationStats, MAX_EMBEDDED_RECIPE_BYTES,
};
pub(crate) use repack::{
    add_native_contents_with_recipes, inspect_archive_with_space, seal_staged_generations,
    verify_extracted_source_files, DeferredRecipes,
};

#[cfg(test)]
mod regression_tests {
    #[test]
    fn display_fallback_preserves_identity_and_native_metadata() {
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nx-display-{}-{id}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let plain = dir.join("plain.nx");
        let custom = dir.join("custom.nx");
        let native = dir.join("native.nx");
        let options = WriterOptions {
            display_fallback: Some(DisplayFallback {
                title: Some("User title".into()),
                icon: Some((MediaType::JpegIcon, Arc::from(&b"artwork"[..]))),
            }),
            ..Default::default()
        };
        write_archive(
            test_archive(vec![42; 1024]),
            &plain,
            &WriterOptions::default(),
        )
        .unwrap();
        write_archive(test_archive(vec![42; 1024]), &custom, &options).unwrap();
        let identity = inspect_archive(&plain, &options, IntegrityPolicy::Strict)
            .unwrap()
            .applications[0]
            .snapshots[0]
            .identity;
        assert_eq!(
            identity,
            inspect_archive(&custom, &options, IntegrityPolicy::Strict)
                .unwrap()
                .applications[0]
                .snapshots[0]
                .identity
        );
        let display = read_snapshot_display(&custom, identity, &options, true).unwrap();
        assert_eq!(display.title.as_deref(), Some("User title"));
        assert_eq!(display.icon.as_deref(), Some(&b"artwork"[..]));
        assert!(display.nacp.is_none());
        let mut input = test_archive(vec![42; 1024]);
        input.applications[0].snapshots[0].metadata = Some(SnapshotMetadata {
            title: Some("Native title".into()),
            publisher: None,
            icon: Some(MediaInput::from_bytes(
                MediaType::PngIcon,
                b"native artwork".to_vec(),
            )),
            nacp: Some(MediaInput::from_bytes(MediaType::Nacp, vec![0; 0x4000])),
            localized: Vec::new(),
            opaque: Vec::new(),
        });
        write_archive(input, &native, &options).unwrap();
        let display = read_snapshot_display(&native, identity, &options, true).unwrap();
        assert_eq!(display.title.as_deref(), Some("Native title"));
        assert_eq!(display.icon.as_deref(), Some(&b"native artwork"[..]));
        assert_eq!(display.nacp.unwrap(), vec![0; 0x4000]);
        let compact = dir.join("compact.nx");
        repack_archive(
            &custom,
            &compact,
            RepackPlan::default(),
            &WriterOptions::default(),
            IntegrityPolicy::Strict,
        )
        .unwrap();
        assert_eq!(
            read_snapshot_display(&compact, identity, &options, true)
                .unwrap()
                .title
                .as_deref(),
            Some("User title")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn captured_profile_checks_scope_bytes_identity_build_and_atomicity() {
        use crate::read_profile::*;
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nx-captured-{}-{id}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("base.nx");
        let options = WriterOptions {
            block_sizing: BlockSizing::Adaptive,
            ..Default::default()
        };
        write_archive(
            test_archive(vec![42; 1024 * 1024]),
            &path,
            &WriterOptions::default(),
        )
        .unwrap();
        let summary = inspect_archive(&path, &options, IntegrityPolicy::Strict).unwrap();
        let mut observed = FileReadProfile::default();
        for _ in 0..10000 {
            observed.observe(0, 4096);
        }
        let scope = ProfileScope {
            platform: 1,
            application_id: 0x0100_0000_0000_9000,
            program_id: Some(0x0100_0000_0000_9001),
            snapshot_type: 0,
            numeric_version: 1,
            content_id: [0; 16],
            build_id: [0; 32],
            snapshot_identity: summary.applications[0].snapshots[0].identity,
        };
        let captured = CapturedFileProfile {
            scope,
            path: "payload.bin".into(),
            size: 1024 * 1024,
            sha256: sha256(&vec![42; 1024 * 1024]),
            profile: observed,
        };
        let attach = |profile: CapturedFileProfile, bytes: u8| {
            let mut input = test_archive(vec![bytes; 1024 * 1024]);
            CapturedReadProfiles {
                files: vec![profile],
            }
            .attach(&mut input, true)
            .unwrap();
            input
        };
        write_archive(attach(captured.clone(), 42), &path, &options).unwrap();
        let good = fs::read(&path).unwrap();
        for kind in 0..6 {
            let mut p = captured.clone();
            match kind {
                0 => p.scope.snapshot_identity[0] ^= 1,
                1 => p.sha256[0] ^= 1,
                2 => p.scope.build_id[0] ^= 1,
                3 => p.scope.numeric_version += 1,
                4 => p.scope.program_id = Some(5),
                _ => p.path = "missing".into(),
            }
            // Bypass attach to exercise the writer's own mandatory checks.
            let mut input = test_archive(vec![42; 1024 * 1024]);
            input.applications[0].snapshots[0].root.entries[0] = EntryInput::file(
                "payload.bin",
                FileInput::from_bytes(vec![42; 1024 * 1024]).with_captured_profile(p),
            );
            assert!(write_archive(input, &path, &options).is_err());
            assert_eq!(fs::read(&path).unwrap(), good);
        }
        assert!(write_archive(attach(captured.clone(), 43), &path, &options).is_err());
        let mut stale = test_archive(vec![42; 1024 * 1024]);
        stale.applications[0].snapshots[0].numeric_version += 1;
        assert!(CapturedReadProfiles {
            files: vec![captured]
        }
        .attach(&mut stale, true)
        .is_err());
        let repacked = dir.join("repacked.nx");
        repack_archive(
            &path,
            &repacked,
            RepackPlan::default(),
            &options,
            IntegrityPolicy::Strict,
        )
        .unwrap();
        assert_eq!(
            inspect_archive(&repacked, &options, IntegrityPolicy::Strict)
                .unwrap()
                .applications[0]
                .snapshots[0]
                .identity,
            summary.applications[0].snapshots[0].identity
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn adaptive_budget_keeps_useful_subset_deterministically() {
        fn make(reverse: bool) -> ArchiveInput {
            let mut input = test_archive(Vec::new());
            let mut high = FileReadProfile::default();
            let mut low = FileReadProfile::default();
            for _ in 0..20000 {
                high.observe(0, 4096);
            }
            for _ in 0..10000 {
                low.observe(0, 4096);
            }
            let mut entries = vec![
                EntryInput::file(
                    "a-low",
                    FileInput::from_bytes(vec![7; 1024 * 1024]).with_read_profile(low),
                ),
                EntryInput::file(
                    "z-high",
                    FileInput::from_bytes(vec![8; 1024 * 1024]).with_read_profile(high),
                ),
            ];
            if reverse {
                entries.reverse();
            }
            input.applications[0].snapshots[0].root = DirectoryInput::new(entries);
            input
        }
        let options = WriterOptions {
            block_sizing: BlockSizing::Adaptive,
            adaptive_index_growth_bytes: 12 * 96,
            ..Default::default()
        };
        for reverse in [false, true] {
            let mut input = make(reverse);
            let mut budget = MemoryBudget {
                current: 0,
                peak: 0,
                limit: 512 * 1024 * 1024,
            };
            select_adaptive(&mut input, &options, &mut budget, 2).unwrap();
            for entry in &input.applications[0].snapshots[0].root.entries {
                if let NodeInput::File(file) = &entry.node {
                    assert_eq!(
                        input_chunk_size(file, &options),
                        if entry.name == "z-high" {
                            64 * 1024
                        } else {
                            256 * 1024
                        }
                    );
                }
            }
        }
    }
    use super::*;

    #[test]
    fn adaptive_sizing_requires_observed_access_and_keeps_unknown_conservative() {
        let options = WriterOptions {
            block_sizing: BlockSizing::Adaptive,
            ..Default::default()
        };
        let mut profile = FileReadProfile::default();
        assert_eq!(file_chunk_size(1024 * 1024, profile, &options), 256 * 1024);
        for _ in 0..9_999 {
            profile.observe(0, 4096);
        }
        assert_eq!(file_chunk_size(1024 * 1024, profile, &options), 256 * 1024);
        profile.observe(0, 4096);
        assert_eq!(file_chunk_size(1024 * 1024, profile, &options), 64 * 1024);
        profile.observe(0, 1024 * 1024);
        assert_eq!(file_chunk_size(1024 * 1024, profile, &options), 256 * 1024);
        assert_eq!(file_chunk_size(1024, profile, &options), 256 * 1024);
        let fixed = WriterOptions::default();
        assert_eq!(file_chunk_size(1024, profile, &fixed), 256 * 1024);
        let mut sequential = FileReadProfile::default();
        for i in 0..10_000u64 {
            sequential.observe((i % 16) * 65536, 65536);
        }
        assert_eq!(
            file_chunk_size(1024 * 1024, sequential, &options),
            256 * 1024
        );
        assert!(!sequential.fits(10));
    }

    #[test]
    fn adaptive_index_growth_budget_falls_back_without_changing_identity() {
        let make = || {
            let mut input = test_archive(vec![42; 1024 * 1024]);
            let NodeInput::File(file) =
                &mut input.applications[0].snapshots[0].root.entries[0].node
            else {
                panic!()
            };
            for _ in 0..10_000 {
                file.read_profile.observe(0, 4096);
            }
            input
        };
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("nx-adaptive-budget-{}-{id}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let fixed = dir.join("fixed.nx");
        let adaptive = dir.join("adaptive.nx");
        write_archive(make(), &fixed, &WriterOptions::default()).unwrap();
        let options = WriterOptions {
            block_sizing: BlockSizing::Adaptive,
            adaptive_index_growth_bytes: 0,
            ..Default::default()
        };
        write_archive(make(), &adaptive, &options).unwrap();
        assert_eq!(fs::read(&fixed).unwrap(), fs::read(&adaptive).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn compressible_files_get_small_chunks_and_others_keep_raw_chunks() {
        let mut state = 0x1234_5678_9abc_def0u64;
        let noise: Vec<u8> = (0..600 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let text: Vec<u8> = b"mesh texture shader level actor "
            .iter()
            .copied()
            .cycle()
            .take(600 * 1024)
            .collect();
        let input = || {
            let mut input = test_archive(Vec::new());
            input.applications[0].snapshots[0].root = DirectoryInput::new(vec![
                EntryInput::file("noise", FileInput::from_bytes(noise.clone())),
                EntryInput::file("text", FileInput::from_bytes(text.clone())),
                EntryInput::file("tiny", FileInput::from_bytes(text[..60 * 1024].to_vec())),
            ]);
            input
        };
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nx-small-chunks-{}-{id}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let mut outputs = Vec::new();
        for (name, options) in [
            (
                "serial",
                WriterOptions {
                    compression_workers: 1,
                    compressed_chunk_bytes: 64 * 1024,
                    ..Default::default()
                },
            ),
            (
                "parallel",
                WriterOptions {
                    compression_workers: 4,
                    compressed_chunk_bytes: 64 * 1024,
                    ..Default::default()
                },
            ),
        ] {
            let path = dir.join(format!("{name}.nx"));
            write_archive(input(), &path, &options).unwrap();
            let info = inspect_archive(&path, &options, IntegrityPolicy::Strict).unwrap();
            let identity = info.applications[0].snapshots[0].identity;
            for file in
                inspect_snapshot_files(&path, identity, &options, IntegrityPolicy::Strict).unwrap()
            {
                let expected = if file.path == "text" {
                    64 * 1024
                } else {
                    256 * 1024
                };
                assert_eq!(file.chunk_size_bytes, expected, "{}", file.path);
            }
            outputs.push(fs::read(&path).unwrap());
        }
        assert_eq!(outputs[0], outputs[1], "worker count changed the archive");
        let off = dir.join("off.nx");
        let options = WriterOptions {
            compressed_chunk_bytes: 0,
            ..Default::default()
        };
        write_archive(input(), &off, &options).unwrap();
        let info = inspect_archive(&off, &options, IntegrityPolicy::Strict).unwrap();
        let identity = info.applications[0].snapshots[0].identity;
        assert!(
            inspect_snapshot_files(&off, identity, &options, IntegrityPolicy::Strict)
                .unwrap()
                .iter()
                .all(|f| f.chunk_size_bytes == 256 * 1024)
        );
        let bad = WriterOptions {
            compressed_chunk_bytes: 1024 * 1024,
            ..Default::default()
        };
        assert!(write_archive(input(), dir.join("bad.nx"), &bad).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn adaptive_mixed_chunks_keep_bytes_identity_dedup_parallelism_and_repack() {
        fn input() -> ArchiveInput {
            let mut input = test_archive(Vec::new());
            let mut profile = FileReadProfile::default();
            for _ in 0..10_000 {
                profile.observe(0, 4096);
            }
            let entries = vec![
                EntryInput::file("small", FileInput::from_bytes(vec![7; 123])),
                EntryInput::file(
                    "random",
                    FileInput::from_bytes(vec![8; 1024 * 1024 + 17]).with_read_profile(profile),
                ),
                EntryInput::file(
                    "copy",
                    FileInput::from_bytes(vec![8; 1024 * 1024 + 17]).with_read_profile(profile),
                ),
                EntryInput::file("unknown", FileInput::from_bytes(vec![9; 1024 * 1024 + 17])),
            ];
            input.applications[0].snapshots[0].root = DirectoryInput::new(entries);
            input
        }
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nx-adaptive-{}-{id}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let fixed = dir.join("fixed.nx");
        // Access-profile sizing only; content-based sizing is tested apart.
        let plain = WriterOptions {
            compressed_chunk_bytes: 0,
            ..Default::default()
        };
        write_archive(input(), &fixed, &plain).unwrap();
        let mut expected_bytes = None;
        for workers in [1, 4] {
            let options = WriterOptions {
                block_sizing: BlockSizing::Adaptive,
                compression_workers: workers,
                ..plain.clone()
            };
            let path = dir.join(format!("adaptive{workers}.nx"));
            let stats = write_archive(input(), &path, &options).unwrap();
            assert!(stats.deduplicated_blocks > 20);
            let info = inspect_archive(&path, &options, IntegrityPolicy::Strict).unwrap();
            assert_eq!(
                info,
                inspect_archive(&fixed, &options, IntegrityPolicy::Strict).unwrap()
            );
            let identity = info.applications[0].snapshots[0].identity;
            let files =
                inspect_snapshot_files(&path, identity, &options, IntegrityPolicy::Strict).unwrap();
            for file in files {
                assert_eq!(
                    file.chunk_size_bytes,
                    if file.path == "unknown" || file.path == "small" {
                        256 * 1024
                    } else {
                        64 * 1024
                    }
                );
            }
            let bytes = fs::read(&path).unwrap();
            if let Some(expected) = &expected_bytes {
                assert_eq!(&bytes, expected);
            } else {
                expected_bytes = Some(bytes);
            }
            let repacked = dir.join(format!("repack{workers}.nx"));
            repack_archive(
                &path,
                &repacked,
                RepackPlan::new(),
                &options,
                IntegrityPolicy::Strict,
            )
            .unwrap();
            assert_eq!(
                info,
                inspect_archive(&repacked, &options, IntegrityPolicy::Strict).unwrap()
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn experimental_levels_and_unbounded_worker_counts_are_not_product_options() {
        for level in [0, 14, 15, 19, 22] {
            let options = WriterOptions {
                zstd_compression_level: level,
                ..Default::default()
            };
            assert_eq!(
                validate_options(&options).unwrap_err().kind(),
                WriterErrorKind::InvalidInput
            );
        }
        for workers in [9, u8::MAX] {
            let options = WriterOptions {
                compression_workers: workers,
                ..Default::default()
            };
            assert_eq!(
                validate_options(&options).unwrap_err().kind(),
                WriterErrorKind::InvalidInput
            );
        }
    }

    fn test_archive(payload: Vec<u8>) -> ArchiveInput {
        let program_id = 0x0100_0000_0000_9001;
        ArchiveInput::new(vec![ApplicationInput {
            application_id: 0x0100_0000_0000_9000,
            platform: Platform::Switch1,
            programs: vec![ProgramInput {
                program_id,
                kind: ProgramKind::Primary,
                content_id: None,
                npdm: None,
            }],
            snapshots: vec![SnapshotInput {
                addon_title_id: None,
                native: Default::default(),
                program_id: Some(program_id),
                snapshot_type: SnapshotType::Base,
                numeric_version: 1,
                content_id: None,
                build_id: None,
                display_version: None,
                dependencies: Vec::new(),
                metadata: None,
                root: DirectoryInput::new(vec![EntryInput::file(
                    "payload.bin",
                    FileInput::from_bytes(payload),
                )]),
            }],
        }])
    }

    #[test]
    fn parallel_writer_is_deterministic_and_preserves_deduplication() {
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("nx-parallel-{}-{id}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let mut payload = vec![0x41; 64 * 1024 * 10 + 17];
        payload[64 * 1024..128 * 1024].fill(0x42);
        // One incompressible chunk also exercises the RAW result path.
        let mut state = 123u64;
        for byte in &mut payload[128 * 1024..192 * 1024] {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        let mut expected = None;
        for workers in [1, 2, 4, 8] {
            let options = WriterOptions {
                chunk_size_bytes: 64 * 1024,
                compression_workers: workers,
                ..Default::default()
            };
            let path = directory.join(format!("{workers}.nx"));
            let stats = write_archive(test_archive(payload.clone()), &path, &options).unwrap();
            assert!(stats.deduplicated_blocks >= 6);
            assert!(stats.raw_block_count > 0 && stats.zstd_block_count > 0);
            assert!(stats.compression_queue_peak_bytes <= options.compression_queue_bytes);
            let bytes = fs::read(&path).unwrap();
            if let Some(expected) = &expected {
                assert_eq!(&bytes, expected);
            } else {
                expected = Some(bytes);
            }
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn low_gain_predictor_skips_random_but_not_traps() {
        let mut state = 5u64;
        let mut random = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect()
        };
        let noise = random(256 * 1024);
        // Compresses 50 % but looks random to order-0 statistics.
        let half = random(128 * 1024);
        let repeated: Vec<u8> = half.iter().chain(half.iter()).copied().collect();
        // Random exactly where the three probes look, zeros elsewhere.
        let mut probe_trap = vec![0u8; 256 * 1024];
        for at in [0, 128 * 1024 - 2048, 256 * 1024 - 4096] {
            probe_trap[at..at + 4096].copy_from_slice(&random(4096));
        }
        let options = WriterOptions::default();
        let mut ctx = zstd::bulk::Compressor::new(3).unwrap();
        let mut predict = |b: &[u8]| {
            predicts_low_gain(b, &options, |level, input| {
                ctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(level))
                    .unwrap();
                Ok(ctx.compress(input).unwrap().len())
            })
            .unwrap()
        };
        assert!(predict(&noise));
        assert!(!predict(&repeated));
        assert!(!predict(&probe_trap));
        assert!(!predict(&vec![0x41; 256 * 1024]));
        let off = WriterOptions {
            min_compression_savings_bp: 0,
            ..Default::default()
        };
        assert!(!predicts_low_gain(&noise, &off, |_, _| unreachable!()).unwrap());
        // Serial and parallel encoders reach identical decisions and bytes.
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nx-predict-{}-{id}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let payload: Vec<u8> = [noise, repeated, probe_trap].concat();
        let mut first = None;
        for workers in [1, 4] {
            let o = WriterOptions {
                compression_workers: workers,
                ..Default::default()
            };
            let path = dir.join(format!("{workers}.nx"));
            let stats = write_archive(test_archive(payload.clone()), &path, &o).unwrap();
            assert_eq!((stats.raw_block_count, stats.zstd_block_count), (1, 2));
            inspect_archive(&path, &o, IntegrityPolicy::Strict).unwrap();
            let bytes = fs::read(&path).unwrap();
            match &first {
                Some(f) => assert_eq!(f, &bytes),
                None => first = Some(bytes),
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn minimum_compression_savings_stores_marginal_blocks_raw() {
        let any = WriterOptions {
            min_compression_savings_bp: 0,
            ..Default::default()
        };
        assert!(keep_compressed(99, 100, &any));
        assert!(!keep_compressed(100, 100, &any));
        assert!(keep_compressed(75, 100, &WriterOptions::default()));
        assert!(!keep_compressed(76, 100, &WriterOptions::default()));
        let three = WriterOptions {
            min_compression_savings_bp: 300,
            ..Default::default()
        };
        assert!(keep_compressed(97, 100, &three) && !keep_compressed(98, 100, &three));
        let too_high = WriterOptions {
            min_compression_savings_bp: 5001,
            ..Default::default()
        };
        assert!(validate_options(&too_high).is_err());

        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("nx-min-gain-{}-{id}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        // Chunk 0 compresses well; chunk 1 is random except ~2.3 % zeros.
        let mut payload = vec![0x41; 128 * 1024];
        let mut state = 99u64;
        for byte in &mut payload[64 * 1024 + 1500..] {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        payload[64 * 1024..64 * 1024 + 1500].fill(0);
        for workers in [1, 4] {
            for (bp, raw) in [(0u16, 0u32), (100, 0), (300, 1)] {
                let options = WriterOptions {
                    chunk_size_bytes: 64 * 1024,
                    compression_workers: workers,
                    min_compression_savings_bp: bp,
                    ..Default::default()
                };
                let path = directory.join(format!("{workers}-{bp}.nx"));
                let stats = write_archive(test_archive(payload.clone()), &path, &options).unwrap();
                assert_eq!(
                    (stats.raw_block_count, stats.zstd_block_count),
                    (raw, 2 - raw)
                );
                inspect_archive(&path, &options, IntegrityPolicy::Strict).unwrap();
            }
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parallel_failure_and_cancellation_keep_destination_and_clean_temporary_files() {
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("nx-parallel-errors-{}-{id}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("previous.nx");
        fs::write(&path, b"previous archive").unwrap();
        for scenario in 0..4 {
            let mut options = WriterOptions {
                compression_workers: 4,
                ..Default::default()
            };
            let expected = match scenario {
                0 => {
                    options.compression_queue_bytes = 1;
                    WriterErrorKind::InvalidInput
                }
                1 => {
                    options.max_memory_bytes = 64 * 1024 * 1024;
                    WriterErrorKind::ResourceLimit
                }
                2 => {
                    CANCEL_DURING_COMPRESSION_TEST.with(|flag| flag.set(true));
                    WriterErrorKind::Cancelled
                }
                _ => {
                    options.max_temporary_disk_bytes = 4096;
                    WriterErrorKind::ResourceLimit
                }
            };
            let error =
                write_archive(test_archive(vec![3; 1024 * 1024]), &path, &options).unwrap_err();
            assert_eq!(error.kind(), expected);
            assert_eq!(fs::read(&path).unwrap(), b"previous archive");
            assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        }
        fs::remove_dir_all(directory).unwrap();
    }

    struct CancelOnRead {
        cursor: io::Cursor<Vec<u8>>,
        token: CancellationToken,
    }
    impl Read for CancelOnRead {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.cursor.read(out)?;
            self.token.cancel();
            Ok(n)
        }
    }
    impl Seek for CancelOnRead {
        fn seek(&mut self, at: SeekFrom) -> io::Result<u64> {
            self.cursor.seek(at)
        }
    }
    #[test]
    fn comparison_cancellation_is_checked_between_bounded_reads() {
        let options = WriterOptions::default();
        let mut source = CancelOnRead {
            cursor: io::Cursor::new(vec![7; 512 * 1024]),
            token: options.cancellation.clone(),
        };
        let error =
            compare_spool_ranges(&mut source, 0, 256 * 1024, 256 * 1024, &options).unwrap_err();
        assert_eq!(error.kind(), WriterErrorKind::Cancelled);
    }

    #[test]
    fn dedup_candidates_require_exact_byte_equality() {
        let dir = std::env::temp_dir().join(format!("nx-dedup-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (file, _guard) = unique_temp(&dir, OsStr::new(".nx-dedup-test")).unwrap();
        let options = WriterOptions::default();
        let mut timings = WriterTimings::default();
        let mut sink = DataSink {
            out: Output::new(file),
            checks: Vec::new(),
            pipeline: None,
            decoder: None,
            frame: Vec::new(),
            compare: Vec::new(),
        };
        sink.out
            .write(&[0u8; DATA_START as usize], &options)
            .unwrap();
        let mut blocks = vec![BlockBuild {
            logical_size: 4,
            stored_size: 4,
            codec: 0,
            first_check: 0,
            data_offset: 0,
        }];
        sink.out
            .write_block(0, &mut blocks[0], 0, b"aaaa", &options, &mut timings)
            .unwrap();
        assert!(sink
            .block_equals(0, b"aaaa", &mut blocks, &options, &mut timings)
            .unwrap());
        // Same slot, same length, different bytes: never shared.
        assert!(!sink
            .block_equals(0, b"aaab", &mut blocks, &options, &mut timings)
            .unwrap());
        drop(sink);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compressor_emits_deterministic_independent_frames_with_declared_size() {
        let options = WriterOptions::default();
        let mut budget = MemoryBudget {
            current: 0,
            peak: 0,
            limit: options.max_memory_bytes,
        };
        let mut encoder = BlockEncoder::new(&options, &mut budget).unwrap();
        let mut timings = WriterTimings::default();
        let bytes = vec![0x71; 32 * 1024];
        let first_len = encoder.encode(&bytes, &options, &mut timings).unwrap();
        let first = encoder.frame[..first_len].to_vec();
        let second_len = encoder.encode(&bytes, &options, &mut timings).unwrap();
        assert_eq!(&encoder.frame[..second_len], first);
        assert_eq!(
            zstd::zstd_safe::find_frame_compressed_size(&first).unwrap(),
            first.len()
        );
        assert_eq!(
            zstd::zstd_safe::get_frame_content_size(&first).unwrap(),
            Some(bytes.len() as u64)
        );
        let mut decoder = zstd::bulk::Decompressor::new().unwrap();
        decoder
            .set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))
            .unwrap();
        let mut decoded = vec![0; bytes.len()];
        assert_eq!(
            decoder
                .decompress_to_buffer(&first, &mut decoded[..])
                .unwrap(),
            bytes.len()
        );
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn cancellation_is_observed_immediately_after_bounded_compression() {
        let options = WriterOptions::default();
        let mut budget = MemoryBudget {
            current: 0,
            peak: 0,
            limit: options.max_memory_bytes,
        };
        let mut encoder = BlockEncoder::new(&options, &mut budget).unwrap();
        let mut timings = WriterTimings::default();
        CANCEL_DURING_COMPRESSION_TEST.with(|flag| flag.set(true));
        let error = encoder
            .encode(&vec![0x42; 1024 * 1024], &options, &mut timings)
            .unwrap_err();
        assert_eq!(error.kind(), WriterErrorKind::Cancelled);
        assert!(timings.compression > Duration::ZERO);
    }

    #[test]
    fn cancellation_during_zstd_output_verification_preserves_destination() {
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "nx-writer-verify-cancel-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let destination = directory.join("archive.nx");
        fs::write(&destination, b"previous archive").unwrap();
        CANCEL_DURING_VERIFICATION_TEST.with(|flag| flag.set(true));
        let error = write_archive(
            test_archive(vec![0x2a; 64 * 1024]),
            &destination,
            &WriterOptions::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), WriterErrorKind::Cancelled);
        assert_eq!(fs::read(&destination).unwrap(), b"previous archive");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn read_ahead_warms_small_files_and_changes_nothing_in_the_archive() {
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("nx-read-ahead-{}-{id}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let small: Vec<Vec<u8>> = (0..400u32)
            .map(|i| (0..4096u32).map(|b| (b ^ i) as u8).collect())
            .collect();
        let large = vec![7u8; (ReadAhead::LARGEST + 1) as usize];
        let warmed = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let input = |warm: bool| {
            let mut entries: Vec<EntryInput> = small
                .iter()
                .enumerate()
                .map(|(i, bytes)| {
                    let name = format!("f{i:04}.bin");
                    let mut file = FileInput::from_bytes(bytes.clone());
                    if warm {
                        let log = warmed.clone();
                        let logged = name.clone();
                        file = file.with_warm_up(Arc::new(move || {
                            log.lock().unwrap().push(logged.clone())
                        }));
                    }
                    EntryInput::file(&name, file)
                })
                .collect();
            let mut big = FileInput::from_bytes(large.clone());
            if warm {
                let log = warmed.clone();
                big = big.with_warm_up(Arc::new(move || log.lock().unwrap().push("big".into())));
            }
            entries.push(EntryInput::file("big.bin", big));
            let mut archive = test_archive(Vec::new());
            archive.applications[0].snapshots[0].root = DirectoryInput::new(entries);
            archive
        };
        let plain = directory.join("plain.nx");
        let ahead = directory.join("ahead.nx");
        write_archive(input(false), &plain, &WriterOptions::default()).unwrap();
        write_archive(input(true), &ahead, &WriterOptions::default()).unwrap();
        // Same bytes: the read-ahead only warms the cache.
        assert_eq!(fs::read(&plain).unwrap(), fs::read(&ahead).unwrap());
        let warmed = warmed.lock().unwrap();
        // Small files are read ahead (some may be skipped once the writer
        // passes them); a file above LARGEST never is.
        assert!(!warmed.is_empty());
        assert!(!warmed.iter().any(|n| n == "big"));
        assert!(warmed.iter().all(|n| n.starts_with('f')));
        fs::remove_dir_all(directory).unwrap();
    }
}
