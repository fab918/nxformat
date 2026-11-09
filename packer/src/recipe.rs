//! Rebuild recipes: a small sidecar that rebuilds an original package (DNSP,
//! DXCI, or any other container whose files ended up in an NX) byte for byte.
//!
//! The recipe never stores game content. It describes the original file as a
//! sequence of segments covering it from byte 0 to its end:
//!
//! - **file**: the whole content of a file stored in the NX, found by its size
//!   and SHA-256 (for example one NCA);
//! - **fill**: a long run of one byte value (gamecard padding);
//! - **literal**: everything else (container headers, tickets, certificates,
//!   short gaps), kept in one Zstd frame.
//!
//! Files are matched by digest, not by snapshot or path, so a recipe survives
//! repacks and edits as long as the files it names are still in the archive.
//! Rebuilding checks each file digest and the SHA-256 of the whole result.
//! The NX format itself is not changed.
//!
//! Layout (little-endian), version 1:
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 8 | magic `NXRECIPE` |
//! | 8 | 2 | version, 1 |
//! | 10 | 2 | reserved, 0 |
//! | 12 | 4 | segment count |
//! | 16 | 8 | original size |
//! | 24 | 32 | original SHA-256 |
//! | 56 | 8 | literal bytes before compression |
//! | 64 | 4 | name length (UTF-8 original file name, a hint only) |
//! | 68 | 4 | reserved, 0 |
//! | 72 | n | name |
//! | | 48 × count | segments: kind u8 (0 literal, 1 fill, 2 file), fill byte u8, 6 zero bytes, length u64, digest (file) or zeros |
//! | | rest − 32 | one Zstd frame with all literal bytes in segment order |
//! | end − 32 | 32 | SHA-256 of every byte before it |

use crate::hash::Sha256;
use crate::native::{inspect_source, InventoryOptions};
use crate::writer::{atomic_replace, unique_temp};
use crate::{
    inspect_archive, inspect_snapshot_files, IntegrityPolicy, SnapshotFileReference, WriterError,
    WriterErrorKind, WriterOptions,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 8] = b"NXRECIPE";
const VERSION: u16 = 1;
const HEADER_BYTES: usize = 72;
const SEGMENT_BYTES: usize = 48;
/// A run of one byte value at least this long outside known files becomes a
/// fill segment instead of literal bytes.
const MIN_FILL: u64 = 64 * 1024;
const MAX_SEGMENTS: u32 = 1 << 20;
const MAX_NAME: u32 = 4096;
const IO_CHUNK: usize = 4 << 20;
const MAX_RECIPE_FILE: u64 = 1 << 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Segment {
    Literal(u64),
    Fill(u8, u64),
    File(u64, [u8; 32]),
}

impl Segment {
    fn len(self) -> u64 {
        match self {
            Segment::Literal(n) | Segment::Fill(_, n) | Segment::File(n, _) => n,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RecipeStats {
    pub source_bytes: u64,
    /// Bytes rebuilt from files stored in the NX.
    pub referenced_bytes: u64,
    pub fill_bytes: u64,
    /// Bytes kept in the recipe before compression.
    pub literal_bytes: u64,
    pub recipe_bytes: u64,
    pub segments: usize,
    /// Package entries whose bytes are not in the NX and were kept as literal
    /// bytes (for example a gamecard's system update partition).
    pub unmatched_entries: Vec<(String, u64)>,
    pub elapsed: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct RebuildStats {
    pub bytes: u64,
    pub sha256: [u8; 32],
    pub elapsed: Duration,
}

fn invalid(message: impl Into<String>) -> WriterError {
    WriterError::new(WriterErrorKind::InvalidInput, message.into())
}

fn integrity(message: impl Into<String>) -> WriterError {
    WriterError::new(WriterErrorKind::Integrity, message.into())
}

fn cancelled(options: &WriterOptions) -> Result<(), WriterError> {
    if options.cancellation.is_cancelled() {
        return Err(WriterError::new(WriterErrorKind::Cancelled, "cancelled"));
    }
    Ok(())
}

/// Every file of every snapshot, by (size, SHA-256).
fn archive_files(
    archive: &Path,
    options: &WriterOptions,
) -> Result<HashMap<(u64, [u8; 32]), SnapshotFileReference>, WriterError> {
    let summary = inspect_archive(archive, options, IntegrityPolicy::Fast)?;
    let mut files = HashMap::new();
    for application in &summary.applications {
        if !application.source_files.is_empty() {
            for (_, resource) in crate::inspect_source_resources(
                archive,
                application.platform,
                application.application_id,
                options,
            )? {
                for file in resource {
                    files.entry((file.size, file.digest)).or_insert(file);
                }
            }
        }
        for snapshot in &application.snapshots {
            for file in
                inspect_snapshot_files(archive, snapshot.identity, options, IntegrityPolicy::Fast)?
            {
                files.entry((file.size, file.digest)).or_insert(file);
            }
        }
    }
    Ok(files)
}

/// The (size, SHA-256) of every file in `archive`, as [`finish_recipe`] needs.
pub fn archive_digests(
    archive: &Path,
    options: &WriterOptions,
) -> Result<std::collections::HashSet<(u64, [u8; 32])>, WriterError> {
    Ok(archive_files(archive, options)?.into_keys().collect())
}

/// Splits bytes outside known files into literal and fill segments.
#[derive(Default)]
struct GapScanner {
    literal: u64,
    run_byte: u8,
    run: u64,
}

impl GapScanner {
    fn push_literal(out: &mut Vec<Segment>, n: u64) {
        if n == 0 {
            return;
        }
        if let Some(Segment::Literal(last)) = out.last_mut() {
            *last += n;
        } else {
            out.push(Segment::Literal(n));
        }
    }

    fn flush_run(&mut self, out: &mut Vec<Segment>) {
        if self.run >= MIN_FILL {
            Self::push_literal(out, self.literal);
            self.literal = 0;
            out.push(Segment::Fill(self.run_byte, self.run));
        } else {
            self.literal += self.run;
        }
        self.run = 0;
    }

    fn feed(&mut self, bytes: &[u8], out: &mut Vec<Segment>) {
        let mut at = 0;
        while at < bytes.len() {
            if self.run != 0 {
                let same = bytes[at..]
                    .iter()
                    .position(|&b| b != self.run_byte)
                    .unwrap_or(bytes.len() - at);
                self.run += same as u64;
                at += same;
                if at == bytes.len() {
                    break;
                }
                self.flush_run(out);
            }
            self.run_byte = bytes[at];
            self.run = 1;
            at += 1;
        }
    }

    fn finish(&mut self, out: &mut Vec<Segment>) {
        self.flush_run(out);
        Self::push_literal(out, self.literal);
        self.literal = 0;
    }
}

/// Hashing writer for the recipe file itself.
struct Hashed<W: Write> {
    inner: W,
    hash: Sha256,
    bytes: u64,
}

impl<W: Write> Write for Hashed<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Write a recipe that rebuilds `source` from the files of `archive`. The
/// recipe is published atomically at `recipe`.
pub fn create_recipe(
    source: &Path,
    archive: &Path,
    recipe: &Path,
    options: &WriterOptions,
) -> Result<RecipeStats, WriterError> {
    let (bytes, stats) = build_recipe(source, archive, options)?;
    let parent = recipe
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (out, mut guard) = unique_temp(parent, OsStr::new(".nxrecipe"))?;
    let mut out = BufWriter::new(out);
    out.write_all(&bytes)?;
    let inner = out.into_inner().map_err(|e| e.into_error())?;
    inner.sync_all()?;
    drop(inner);
    atomic_replace(&guard.path, recipe)?;
    guard.disarm();
    Ok(stats)
}

/// Build, in memory, a recipe that rebuilds `source` from the files of `archive`.
pub fn build_recipe(
    source: &Path,
    archive: &Path,
    options: &WriterOptions,
) -> Result<(Vec<u8>, RecipeStats), WriterError> {
    let scan = scan_source(source, options)?;
    let known = archive_files(archive, options)?;
    finish_recipe(&scan, &known.into_keys().collect(), options)
}

/// One stretch of a scanned source.
#[derive(Clone, Debug)]
enum Part {
    /// Bytes outside the package entries: literal or fill.
    Gap(Segment),
    /// A package entry, kept as a file of the NX when one has its digest.
    Entry(u64, [u8; 32], String),
}

/// The one full read of a source a recipe needs: the whole-file SHA-256,
/// each package entry's SHA-256 and the gaps between entries. It does not
/// need the archive, so it runs while the conversion reads the same source.
#[derive(Clone, Debug)]
pub struct SourceScan {
    source: std::path::PathBuf,
    size: u64,
    digest: [u8; 32],
    parts: Vec<Part>,
    started: Instant,
}

/// Read `source` once for its recipe (see [`SourceScan`]).
pub fn scan_source(source: &Path, options: &WriterOptions) -> Result<SourceScan, WriterError> {
    let started = Instant::now();
    let report = inspect_source(source, &InventoryOptions::default());
    let mut units: Vec<_> = report
        .units
        .iter()
        .filter(|u| u.size != 0)
        .map(|u| (u.offset, u.size, u.name.clone()))
        .collect();
    if let Some(part) = crate::native::update_partition(source, &InventoryOptions::default())
        .map_err(|e| invalid(e.to_string()))?
    {
        units.push((
            part.offset,
            part.size,
            "system-update-partition.hfs0".to_owned(),
        ));
    }
    if units.is_empty() {
        return Err(invalid("source has no recognised package entries"));
    }
    units.sort();
    if units.windows(2).any(|w| w[0].0 + w[0].1 > w[1].0) {
        return Err(invalid("source package entries overlap"));
    }
    let mut file = crate::source_file::open(source)?;
    let size = file.len()?;
    if units.last().is_some_and(|u| u.0 + u.1 > size) {
        return Err(invalid("source package entry extends past the file"));
    }

    // Hash the whole source and each entry; classify the gaps. The
    // whole-file hash runs on a second thread, so both hashes cost one.
    let mut parts = Vec::new();
    let mut gaps = Vec::new();
    let mut gap = GapScanner::default();
    let mut position = 0u64;
    let mut next = 0usize;
    let (to_hasher, hasher_input) = mpsc::sync_channel::<(Vec<u8>, usize)>(4);
    let (to_reader, free_buffers) = mpsc::channel::<Vec<u8>>();
    for _ in 0..4 {
        to_reader.send(vec![0u8; IO_CHUNK]).unwrap();
    }
    let hasher = std::thread::spawn(move || {
        let mut whole = Sha256::new();
        for (buf, n) in hasher_input {
            whole.update(&buf[..n]);
            if to_reader.send(buf).is_err() {
                break;
            }
        }
        whole.finish()
    });
    let mut pass = || -> Result<(), WriterError> {
        while position < size {
            cancelled(options)?;
            let (end, entry) = match units.get(next) {
                Some(&(offset, _, _)) if position < offset => (offset, None),
                Some(&(offset, length, _)) => (offset + length, Some(next)),
                None => (size, None),
            };
            let mut hash = entry.map(|_| Sha256::new());
            while position < end {
                let mut buf = free_buffers
                    .recv()
                    .map_err(|_| invalid("hash thread stopped"))?;
                let n = (end - position).min(buf.len() as u64) as usize;
                file.read_exact(&mut buf[..n])?;
                match &mut hash {
                    Some(h) => h.update(&buf[..n]),
                    None => gap.feed(&buf[..n], &mut gaps),
                }
                position += n as u64;
                to_hasher
                    .send((buf, n))
                    .map_err(|_| invalid("hash thread stopped"))?;
            }
            if let (Some(index), Some(h)) = (entry, hash) {
                gap.finish(&mut gaps);
                parts.extend(gaps.drain(..).map(Part::Gap));
                let (_, length, name) = &units[index];
                parts.push(Part::Entry(*length, h.finish(), name.clone()));
                next += 1;
            }
        }
        gap.finish(&mut gaps);
        parts.extend(gaps.drain(..).map(Part::Gap));
        Ok(())
    };
    let passed = pass();
    drop(to_hasher);
    let digest = hasher.join().map_err(|_| invalid("hash thread panicked"))?;
    passed?;
    Ok(SourceScan {
        source: source.to_owned(),
        size,
        digest,
        parts,
        started,
    })
}

/// Write the recipe of a scanned source. `known` holds the (size, SHA-256)
/// of the archive's files; only the literal ranges of the source are read.
pub fn finish_recipe(
    scan: &SourceScan,
    known: &std::collections::HashSet<(u64, [u8; 32])>,
    options: &WriterOptions,
) -> Result<(Vec<u8>, RecipeStats), WriterError> {
    let mut segments = Vec::new();
    let mut unmatched = Vec::new();
    for part in &scan.parts {
        match part {
            Part::Gap(Segment::Literal(n)) => GapScanner::push_literal(&mut segments, *n),
            Part::Gap(segment) => segments.push(*segment),
            Part::Entry(length, digest, name) => {
                if known.contains(&(*length, *digest)) {
                    segments.push(Segment::File(*length, *digest));
                } else {
                    GapScanner::push_literal(&mut segments, *length);
                    unmatched.push((name.clone(), *length));
                }
            }
        }
    }
    let size = scan.size;
    let digest = scan.digest;
    let referenced: u64 = segments
        .iter()
        .filter(|s| matches!(s, Segment::File(..)))
        .map(|s| s.len())
        .sum();
    if referenced == 0 {
        return Err(invalid(
            "no package entry of the source is stored in the archive",
        ));
    }
    let literal: u64 = segments
        .iter()
        .filter(|s| matches!(s, Segment::Literal(_)))
        .map(|s| s.len())
        .sum();
    if segments.len() > MAX_SEGMENTS as usize {
        return Err(invalid("recipe needs too many segments"));
    }
    let mut file = crate::source_file::open(&scan.source)?;
    if file.len()? != size {
        return Err(integrity("source changed while writing the recipe"));
    }

    // Write the recipe, reading only the literal ranges again.
    let name = scan
        .source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = &name.as_bytes()[..name.len().min(MAX_NAME as usize)];
    let mut out = Hashed {
        inner: Vec::new(),
        hash: Sha256::new(),
        bytes: 0,
    };
    let mut header = [0u8; HEADER_BYTES];
    header[..8].copy_from_slice(MAGIC);
    header[8..10].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&(segments.len() as u32).to_le_bytes());
    header[16..24].copy_from_slice(&size.to_le_bytes());
    header[24..56].copy_from_slice(&digest);
    header[56..64].copy_from_slice(&literal.to_le_bytes());
    header[64..68].copy_from_slice(&(name.len() as u32).to_le_bytes());
    out.write_all(&header)?;
    out.write_all(name)?;
    for segment in &segments {
        let mut row = [0u8; SEGMENT_BYTES];
        match *segment {
            Segment::Literal(n) => row[8..16].copy_from_slice(&n.to_le_bytes()),
            Segment::Fill(b, n) => {
                row[0] = 1;
                row[1] = b;
                row[8..16].copy_from_slice(&n.to_le_bytes());
            }
            Segment::File(n, d) => {
                row[0] = 2;
                row[8..16].copy_from_slice(&n.to_le_bytes());
                row[16..48].copy_from_slice(&d);
            }
        }
        out.write_all(&row)?;
    }
    // Headers are tiny and compress well at a high level; a large literal
    // area (unmatched entries) uses the fast default level.
    let level = if literal <= 64 << 20 { 19 } else { 3 };
    let mut encoder = zstd::stream::write::Encoder::new(&mut out, level)?;
    encoder.set_pledged_src_size(Some(literal))?;
    let mut offset = 0u64;
    for segment in &segments {
        if let Segment::Literal(n) = *segment {
            cancelled(options)?;
            file.seek(SeekFrom::Start(offset))?;
            let copied = io::copy(&mut (&mut file).take(n), &mut encoder)?;
            if copied != n {
                return Err(integrity("source changed while writing the recipe"));
            }
        }
        offset += segment.len();
    }
    encoder.finish()?;
    let trailer = std::mem::replace(&mut out.hash, Sha256::new()).finish();
    out.write_all(&trailer)?;
    let recipe_bytes = out.bytes;
    let bytes = out.inner;
    Ok((
        bytes,
        RecipeStats {
            source_bytes: size,
            referenced_bytes: referenced,
            fill_bytes: size - referenced - literal,
            literal_bytes: literal,
            recipe_bytes,
            segments: segments.len(),
            unmatched_entries: unmatched,
            elapsed: scan.started.elapsed(),
        },
    ))
}

struct Parsed {
    size: u64,
    digest: [u8; 32],
    name: String,
    segments: Vec<Segment>,
    /// Start and length of the Zstd frame in the recipe file.
    frame: (u64, u64),
}

fn parse_recipe(bytes: &[u8]) -> Result<Parsed, WriterError> {
    let total = bytes.len() as u64;
    if total < (HEADER_BYTES + 32) as u64 {
        return Err(invalid("recipe is truncated"));
    }
    // Check the trailer before trusting any field.
    let (body, trailer) = bytes.split_at(bytes.len() - 32);
    if crate::hash::sha256(body) != trailer {
        return Err(integrity("recipe checksum mismatch"));
    }
    let header = &bytes[..HEADER_BYTES];
    let u32_at = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().unwrap());
    if &header[..8] != MAGIC {
        return Err(invalid("not an NX recipe"));
    }
    if u16::from_le_bytes([header[8], header[9]]) != VERSION
        || header[10..12] != [0, 0]
        || u32_at(68) != 0
    {
        return Err(invalid("unsupported recipe version"));
    }
    let count = u32_at(12);
    let name_length = u32_at(64);
    if count == 0 || count > MAX_SEGMENTS || name_length > MAX_NAME {
        return Err(invalid("recipe limits exceeded"));
    }
    let tables = HEADER_BYTES as u64 + name_length as u64 + count as u64 * SEGMENT_BYTES as u64;
    if tables > total - 32 {
        return Err(invalid("recipe is truncated"));
    }
    let name = &bytes[HEADER_BYTES..HEADER_BYTES + name_length as usize];
    let rows = &bytes[HEADER_BYTES + name_length as usize..tables as usize];
    let mut segments = Vec::with_capacity(count as usize);
    let (mut covered, mut literal) = (0u64, 0u64);
    for row in rows.chunks_exact(SEGMENT_BYTES) {
        let n = u64::from_le_bytes(row[8..16].try_into().unwrap());
        let zero_tail = row[16..].iter().all(|&b| b == 0);
        let segment = match row[0] {
            0 if row[1] == 0 && zero_tail => Segment::Literal(n),
            1 if zero_tail => Segment::Fill(row[1], n),
            2 if row[1] == 0 => Segment::File(n, row[16..48].try_into().unwrap()),
            _ => return Err(invalid("unknown recipe segment")),
        };
        if n == 0 || row[2..8].iter().any(|&b| b != 0) {
            return Err(invalid("invalid recipe segment"));
        }
        if let Segment::Literal(n) = segment {
            literal = literal
                .checked_add(n)
                .ok_or_else(|| invalid("recipe size overflows"))?;
        }
        covered = covered
            .checked_add(n)
            .ok_or_else(|| invalid("recipe size overflows"))?;
        segments.push(segment);
    }
    if covered != u64_at(16) || literal != u64_at(56) {
        return Err(invalid("recipe segments do not cover the original size"));
    }
    Ok(Parsed {
        size: covered,
        digest: header[24..56].try_into().unwrap(),
        name: String::from_utf8_lossy(name).into_owned(),
        segments,
        frame: (tables, total - 32 - tables),
    })
}

/// A recipe file read whole: recipes are small (headers and tickets).
fn read_recipe_file(recipe: &Path) -> Result<Vec<u8>, WriterError> {
    let length = std::fs::metadata(recipe)?.len();
    if length > MAX_RECIPE_FILE {
        return Err(invalid("recipe file is too large"));
    }
    Ok(std::fs::read(recipe)?)
}

/// What a recipe rebuilds, and which NX files it needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipeInfo {
    /// Original file name (a hint for user interfaces).
    pub name: String,
    pub size: u64,
    pub sha256: [u8; 32],
    /// Checksum of the recipe itself: identifies one recipe.
    pub recipe_sha256: [u8; 32],
    pub recipe_bytes: u64,
    /// (size, SHA-256) of every NX file it needs, sorted and unique.
    pub files: Vec<(u64, [u8; 32])>,
}

/// Check a recipe's own checksum and layout, and describe it.
pub fn recipe_info(bytes: &[u8]) -> Result<RecipeInfo, WriterError> {
    let parsed = parse_recipe(bytes)?;
    let mut files: Vec<(u64, [u8; 32])> = parsed
        .segments
        .iter()
        .filter_map(|s| match *s {
            Segment::File(n, d) => Some((n, d)),
            _ => None,
        })
        .collect();
    files.sort_unstable();
    files.dedup();
    Ok(RecipeInfo {
        name: parsed.name,
        size: parsed.size,
        sha256: parsed.digest,
        recipe_sha256: bytes[bytes.len() - 32..].try_into().unwrap(),
        recipe_bytes: bytes.len() as u64,
        files,
    })
}

/// Original file name stored in a recipe (a hint for user interfaces).
pub fn recipe_source_name(recipe: &Path) -> Result<String, WriterError> {
    Ok(parse_recipe(&read_recipe_file(recipe)?)?.name)
}

/// Replay a recipe into `sink`, checking every file digest and the SHA-256 of
/// the whole result. Returns the number of bytes and the result's SHA-256.
fn replay(
    archive: &Path,
    recipe: &[u8],
    sink: &mut dyn Write,
    options: &WriterOptions,
) -> Result<(u64, [u8; 32]), WriterError> {
    let parsed = parse_recipe(recipe)?;
    let known = archive_files(archive, options)?;
    for segment in &parsed.segments {
        if let Segment::File(n, d) = *segment {
            if !known.contains_key(&(n, d)) {
                return Err(invalid(
                    "a file named by the recipe is no longer in the archive",
                ));
            }
        }
    }
    let frame = &recipe[parsed.frame.0 as usize..(parsed.frame.0 + parsed.frame.1) as usize];
    let mut literals = zstd::stream::read::Decoder::new(frame)?;
    let mut nx = File::open(archive)?;
    let mut decoder = zstd::bulk::Decompressor::new()?;
    decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
    let mut whole = Sha256::new();
    let mut stored = Vec::new();
    let mut logical = Vec::new();
    let mut buf = vec![0u8; IO_CHUNK];
    let mut written = 0u64;
    let mut emit = |bytes: &[u8], whole: &mut Sha256| -> Result<(), WriterError> {
        whole.update(bytes);
        sink.write_all(bytes)?;
        Ok(())
    };
    for segment in &parsed.segments {
        cancelled(options)?;
        match *segment {
            Segment::Literal(n) => {
                let mut left = n;
                while left != 0 {
                    let k = left.min(buf.len() as u64) as usize;
                    literals
                        .read_exact(&mut buf[..k])
                        .map_err(|_| integrity("recipe literal bytes are truncated"))?;
                    emit(&buf[..k], &mut whole)?;
                    left -= k as u64;
                }
            }
            Segment::Fill(b, n) => {
                let k = (n.min(buf.len() as u64)) as usize;
                buf[..k].fill(b);
                let mut left = n;
                while left != 0 {
                    let k = left.min(buf.len() as u64) as usize;
                    emit(&buf[..k], &mut whole)?;
                    left -= k as u64;
                }
            }
            Segment::File(n, d) => {
                let reference = &known[&(n, d)];
                let mut hash = Sha256::new();
                let mut produced = 0u64;
                for block in &reference.blocks {
                    stored.resize(block.stored_size as usize, 0);
                    nx.seek(SeekFrom::Start(block.offset))?;
                    nx.read_exact(&mut stored)?;
                    let bytes: &[u8] = match block.codec {
                        0 => &stored,
                        1 => {
                            logical.resize(block.logical_size as usize, 0);
                            let got = decoder
                                .decompress_to_buffer(&stored, &mut logical[..])
                                .map_err(|_| integrity("NX block does not decode"))?;
                            if got != logical.len() {
                                return Err(integrity("NX block size mismatch"));
                            }
                            &logical
                        }
                        _ => return Err(invalid("unknown NX block codec")),
                    };
                    if bytes.len() as u64 != block.logical_size as u64 {
                        return Err(integrity("NX block size mismatch"));
                    }
                    hash.update(bytes);
                    produced += bytes.len() as u64;
                    emit(bytes, &mut whole)?;
                }
                if produced != n || hash.finish() != d {
                    return Err(integrity("NX file digest mismatch"));
                }
            }
        }
        written += segment.len();
    }
    let mut extra = [0u8; 1];
    if literals.read(&mut extra)? != 0 {
        return Err(integrity("recipe literal area is longer than its segments"));
    }
    let digest = whole.finish();
    if written != parsed.size || digest != parsed.digest {
        return Err(integrity(
            "rebuilt file does not match the original SHA-256",
        ));
    }
    Ok((written, digest))
}

/// Rebuild the original package at `output` and publish it atomically only
/// when its SHA-256 matches the recipe.
pub fn rebuild_from_recipe(
    archive: &Path,
    recipe: &Path,
    output: &Path,
    options: &WriterOptions,
) -> Result<RebuildStats, WriterError> {
    let started = Instant::now();
    if output.exists() {
        return Err(invalid("output already exists"));
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (file, mut guard) = unique_temp(parent, OsStr::new(".nxrebuild"))?;
    let mut sink = BufWriter::with_capacity(IO_CHUNK, file);
    let (bytes, sha256) = replay(archive, &read_recipe_file(recipe)?, &mut sink, options)?;
    let file = sink.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    drop(file);
    atomic_replace(&guard.path, output)?;
    guard.disarm();
    Ok(RebuildStats {
        bytes,
        sha256,
        elapsed: started.elapsed(),
    })
}

/// Replay a recipe without writing anything: proves the archive still
/// rebuilds the original byte for byte.
pub fn check_recipe(
    archive: &Path,
    recipe: &Path,
    options: &WriterOptions,
) -> Result<RebuildStats, WriterError> {
    let started = Instant::now();
    let (bytes, sha256) = replay(
        archive,
        &read_recipe_file(recipe)?,
        &mut io::sink(),
        options,
    )?;
    Ok(RebuildStats {
        bytes,
        sha256,
        elapsed: started.elapsed(),
    })
}

/// Default recipe path for a source converted into `archive`: `GAME.nxrecipe`
/// for one source, `GAME.<source name>.nxrecipe` for several.
pub fn recipe_path(archive: &Path, source: &Path, several: bool) -> std::path::PathBuf {
    if several {
        let name = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        archive.with_extension(format!("{name}.nxrecipe"))
    } else {
        archive.with_extension("nxrecipe")
    }
}

/// A recipe kept inside an NX archive.
#[derive(Clone, Debug)]
pub struct EmbeddedRecipe {
    pub platform: crate::Platform,
    pub application_id: u64,
    pub info: RecipeInfo,
    pub bytes: Vec<u8>,
}

/// Every rebuild recipe kept inside `archive`.
pub fn embedded_recipes(
    archive: &Path,
    options: &WriterOptions,
) -> Result<Vec<EmbeddedRecipe>, WriterError> {
    let summary = inspect_archive(archive, options, IntegrityPolicy::Fast)?;
    let mut out = Vec::new();
    for application in summary.applications {
        for bytes in application.recipes {
            out.push(EmbeddedRecipe {
                platform: application.platform,
                application_id: application.application_id,
                info: recipe_info(&bytes)?,
                bytes,
            });
        }
    }
    Ok(out)
}

/// The application whose snapshots hold every file a recipe needs.
fn recipe_owner(
    archive: &Path,
    info: &RecipeInfo,
    options: &WriterOptions,
) -> Result<(crate::Platform, u64), WriterError> {
    let summary = inspect_archive(archive, options, IntegrityPolicy::Fast)?;
    for application in &summary.applications {
        let mut files: std::collections::HashSet<_> = application
            .source_files
            .iter()
            .map(|file| (file.size, file.digest))
            .collect();
        for snapshot in &application.snapshots {
            for file in
                inspect_snapshot_files(archive, snapshot.identity, options, IntegrityPolicy::Fast)?
            {
                files.insert((file.size, file.digest));
            }
        }
        if info.files.iter().all(|f| files.contains(f)) {
            return Ok((application.platform, application.application_id));
        }
    }
    Err(invalid(
        "no game in the archive holds every file of this recipe",
    ))
}

/// Keep recipe bytes (made by `build_recipe` or read from an `.nxrecipe`)
/// inside `archive`. Only a new archive tail is written.
pub fn embed_recipes(
    archive: &Path,
    recipes: Vec<Vec<u8>>,
    options: &WriterOptions,
) -> Result<(), WriterError> {
    let mut add = Vec::with_capacity(recipes.len());
    for bytes in recipes {
        let info = recipe_info(&bytes)?;
        let (platform, application_id) = recipe_owner(archive, &info, options)?;
        add.push((platform, application_id, bytes));
    }
    crate::edit_archive_recipes(archive, add, Vec::new(), options, IntegrityPolicy::Fast)?;
    Ok(())
}

/// Drop recipes (by their own checksum) from `archive`.
pub fn remove_recipes(
    archive: &Path,
    recipes: Vec<[u8; 32]>,
    options: &WriterOptions,
) -> Result<(), WriterError> {
    crate::edit_archive_recipes(archive, Vec::new(), recipes, options, IntegrityPolicy::Fast)?;
    Ok(())
}

/// Rebuild the original file of an embedded recipe at `output`, published
/// atomically only when its SHA-256 matches.
pub fn rebuild_embedded(
    archive: &Path,
    recipe: &[u8],
    output: &Path,
    options: &WriterOptions,
) -> Result<RebuildStats, WriterError> {
    let started = Instant::now();
    if output.exists() {
        return Err(invalid("output already exists"));
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (file, mut guard) = unique_temp(parent, OsStr::new(".nxrebuild"))?;
    let mut sink = BufWriter::with_capacity(IO_CHUNK, file);
    let (bytes, sha256) = replay(archive, recipe, &mut sink, options)?;
    let file = sink.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    drop(file);
    atomic_replace(&guard.path, output)?;
    guard.disarm();
    Ok(RebuildStats {
        bytes,
        sha256,
        elapsed: started.elapsed(),
    })
}

/// Replay an embedded recipe without writing anything.
pub fn check_embedded(
    archive: &Path,
    recipe: &[u8],
    options: &WriterOptions,
) -> Result<RebuildStats, WriterError> {
    let started = Instant::now();
    let (bytes, sha256) = replay(archive, recipe, &mut io::sink(), options)?;
    Ok(RebuildStats {
        bytes,
        sha256,
        elapsed: started.elapsed(),
    })
}
