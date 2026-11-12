//! External, Zstandard-compressed recipes for reversible extraction. No NX wire
//! extension: original headers, indexes, NCA/BKTR metadata and embedded package
//! recipes are literal data; game bytes reference the exported file ranges.
use super::*;
#[path = "restoration_metadata.rs"]
mod metadata;
#[path = "restoration_store.rs"]
mod store;
use crate::hash::Sha256;
use crate::import::{ImportLimits, SourceRoot};
use std::io::{self, BufReader};

pub const RESTORATION_FILE: &str = ".nxrestore.zst";
const MAGIC: &[u8; 8] = b"NXRSTR02";
const CHUNK: usize = 128 * 1024;
const MAX_TEXT: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) struct RestoreRange {
    pub file: u32,
    pub offset: u64,
    pub len: u64,
    pub path: String,
    pub file_offset: u64,
    /// Nonzero: reconstruct SHA-256 of this many exported file bytes.
    pub hash_size: u32,
    pub hash_offset: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RestorationMode {
    #[default]
    Compact,
    /// Preserve every stored source block; replay needs only a Zstd decoder.
    Portable,
}

#[derive(Clone, Debug, Default)]
pub struct RestorationStats {
    pub unchanged: bool,
    pub archive_bytes: u64,
    pub recipe_bytes: u64,
    pub referenced_bytes: u64,
    /// Original metadata and source-resource bytes staged during edited replay.
    /// Zero for recipe creation and exact unchanged restoration.
    pub metadata_staged_bytes: u64,
    pub files: usize,
    /// Recipe creation metrics; zero when replaying an existing recipe.
    pub compression_attempts: u64,
    pub exhaustive_searches: u32,
    pub literal_fallback_blocks: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    path: String,
    dir: bool,
    size: u64,
    digest: [u8; 32],
}

struct HashWrite<W> {
    inner: W,
    hash: Sha256,
}
impl<W: Write> Write for HashWrite<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
struct HashRead<R> {
    inner: R,
    hash: Sha256,
    remaining: u64,
}
impl<R: Read> Read for HashRead<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(bytes)?;
        if n as u64 > self.remaining {
            return Err(io::Error::other("restoration expansion exceeds budget"));
        }
        self.remaining -= n as u64;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
}
fn num(w: &mut impl Write, n: u64) -> Result<(), WriterError> {
    w.write_all(&n.to_le_bytes())?;
    Ok(())
}
fn number(r: &mut impl Read) -> Result<u64, WriterError> {
    let mut n = [0; 8];
    r.read_exact(&mut n)?;
    Ok(u64::from_le_bytes(n))
}
fn string(w: &mut impl Write, text: &str) -> Result<(), WriterError> {
    num(w, text.len() as u64)?;
    w.write_all(text.as_bytes())?;
    Ok(())
}
fn text(r: &mut impl Read, limit: u64) -> Result<String, WriterError> {
    let n = number(r)?;
    if n > limit {
        return Err(limit_error("restoration text exceeds budget"));
    }
    let mut bytes = vec![0; n as usize];
    r.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|_| bad("non-UTF8 restoration text"))
}
fn byte(r: &mut impl Read) -> Result<u8, WriterError> {
    let mut b = [0];
    r.read_exact(&mut b)?;
    Ok(b[0])
}
fn digest(r: &mut impl Read) -> Result<[u8; 32], WriterError> {
    let mut b = [0; 32];
    r.read_exact(&mut b)?;
    Ok(b)
}
fn hash_file(file: &mut File, options: &WriterOptions) -> Result<[u8; 32], WriterError> {
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut bytes = vec![0; CHUNK];
    loop {
        cancelled(options)?;
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(hash.finish())
}
fn nodes(root: &SourceRoot, options: &WriterOptions) -> Result<Vec<Node>, WriterError> {
    let mut out = Vec::new();
    let mut pending = vec![String::new()];
    let mut memory = 0;
    while let Some(parent) = pending.pop() {
        cancelled(options)?;
        for entry in root.list(&parent, options.max_entries as usize)? {
            if parent.is_empty() && entry.name == RESTORATION_FILE {
                continue;
            }
            let path = if parent.is_empty() {
                entry.name
            } else {
                format!("{parent}/{}", entry.name)
            };
            crate::import::portable_path(&path, &ImportLimits::default())?;
            memory += 256 + path.len() as u64 * 4;
            if out.len() >= options.max_entries as usize || memory > options.max_memory_bytes / 4 {
                return Err(limit_error(
                    "restoration file inventory exceeds memory budget",
                ));
            }
            let hash = if entry.dir {
                pending.push(path.clone());
                [0; 32]
            } else {
                hash_file(&mut root.open_relative(&path)?, options)?
            };
            out.push(Node {
                path,
                dir: entry.dir,
                size: if entry.dir { 0 } else { entry.size },
                digest: hash,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}
fn fingerprint(nodes: &[Node]) -> [u8; 32] {
    let mut hash = Sha256::new();
    for n in nodes {
        hash.update(&(n.path.len() as u64).to_le_bytes());
        hash.update(n.path.as_bytes());
        hash.update(&[u8::from(n.dir)]);
        hash.update(&n.size.to_le_bytes());
        hash.update(&n.digest);
    }
    hash.finish()
}

fn compression(bytes: &[u8], level: i32, window: u32) -> Result<Vec<u8>, WriterError> {
    let mut c = zstd::bulk::Compressor::new(level)?;
    c.set_parameter(zstd::zstd_safe::CParameter::WindowLog(window))?;
    c.set_parameter(zstd::zstd_safe::CParameter::ContentSizeFlag(true))?;
    c.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(false))?;
    c.set_parameter(zstd::zstd_safe::CParameter::DictIdFlag(false))?;
    Ok(c.compress(bytes)?)
}
/// Cache common encodings, but bound discovery work for the whole archive.
/// Foreign encodings may remain literal; exact bytes always win over size.
struct CompressionSearch {
    cached: Vec<(i32, u32)>,
    searches_left: u32,
    attempts: u64,
    exhaustive_searches: u32,
    literal_fallback_blocks: u64,
}
impl Default for CompressionSearch {
    fn default() -> Self {
        Self {
            cached: vec![(3, 20)],
            searches_left: 4,
            attempts: 0,
            exhaustive_searches: 0,
            literal_fallback_blocks: 0,
        }
    }
}
impl CompressionSearch {
    fn matches(
        &mut self,
        logical: &[u8],
        stored: &[u8],
        params: (i32, u32),
        options: &WriterOptions,
    ) -> Result<bool, WriterError> {
        cancelled(options)?;
        self.attempts += 1;
        Ok(compression(logical, params.0, params.1)? == stored)
    }
    fn find(
        &mut self,
        logical: &[u8],
        stored: &[u8],
        options: &WriterOptions,
    ) -> Result<Option<(i32, u32)>, WriterError> {
        for i in 0..self.cached.len() {
            let params = self.cached[i];
            if self.matches(logical, stored, params, options)? {
                self.cached.remove(i);
                self.cached.insert(0, params);
                return Ok(Some(params));
            }
        }
        if self.searches_left != 0 {
            self.searches_left -= 1;
            self.exhaustive_searches += 1;
            for level in [3, 1, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13] {
                for window in [20, 16, 17, 18, 19] {
                    let params = (level, window);
                    if self.cached.contains(&params) {
                        continue;
                    }
                    if self.matches(logical, stored, params, options)? {
                        self.cached.insert(0, params);
                        self.cached.truncate(4);
                        return Ok(Some(params));
                    }
                }
            }
        }
        self.literal_fallback_blocks += 1;
        Ok(None)
    }
}

// Native exclusive renames also work on removable filesystems without hard
// links. They atomically refuse an existing destination on every supported OS.
fn publish_new(source: &Path, destination: &Path) -> Result<(), WriterError> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())
            .map_err(|_| invalid("invalid output path"))?;
        let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| invalid("invalid output path"))?;
        if unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) }
            != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())
            .map_err(|_| invalid("invalid output path"))?;
        let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| invalid("invalid output path"))?;
        if unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn MoveFileExW(source: *const u16, destination: *const u16, flags: u32) -> i32;
        }
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 8) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        std::fs::hard_link(source, destination)?;
        std::fs::remove_file(source)?;
        Ok(())
    }
}

/// One physical NX file interval of game data is reconstructed from an exported
/// range. Unmapped bytes (including NCA headers, hash tables and BKTR buckets)
/// are retained as literals in the compressed recipe.
fn ranges_for(
    path: &Path,
    archive: &SourceArchive,
    jobs: &[([u8; 32], String)],
    budget: &MemoryBudget,
    options: &WriterOptions,
) -> Result<HashMap<u32, Vec<RestoreRange>>, WriterError> {
    let mut all: HashMap<u32, Vec<RestoreRange>> = HashMap::new();
    let mut memory = 0u64;
    for (identity, prefix) in jobs {
        let snapshot = archive
            .snapshots
            .iter()
            .find(|s| s.snapshot.identity == *identity)
            .ok_or_else(|| bad("restoration snapshot absent"))?;
        let native = !snapshot.snapshot.addon && snapshot.snapshot.content_id != [0; 16];
        let ranges = if native {
            native_add::extraction::collect_native_ranges(path, archive, snapshot, budget, options)?
        } else {
            let mut nodes = Vec::new();
            let mut total = 0;
            visit_identity_tree(
                snapshot.snapshot.root,
                &[],
                0,
                archive,
                &mut HashSet::new(),
                &mut nodes,
                &mut total,
            )?;
            nodes
                .into_iter()
                .filter_map(|n| n.source_file.map(|file| (n, file)))
                .map(|(n, file)| {
                    Ok(RestoreRange {
                        file,
                        offset: 0,
                        len: archive.files[file as usize].size,
                        path: String::from_utf8(n.path).map_err(|_| bad("non-UTF8 tree path"))?,
                        file_offset: 0,
                        hash_size: 0,
                        hash_offset: 0,
                    })
                })
                .collect::<Result<Vec<_>, WriterError>>()?
        };
        for mut r in ranges {
            r.path = format!("{prefix}/{}", r.path);
            memory += 128 + r.path.len() as u64 * 2;
            if memory > options.max_memory_bytes / 8 {
                return Err(limit_error("restoration ranges exceed memory budget"));
            }
            all.entry(r.file).or_default().push(r);
        }
    }
    for ranges in all.values_mut() {
        ranges.sort_by_key(|r| (r.offset, std::cmp::Reverse(r.len)));
        let original = std::mem::take(ranges);
        let mut index = 0;
        let mut at = original.first().map_or(0, |r| r.offset);
        let mut best: Option<&RestoreRange> = None;
        while index < original.len() || best.is_some_and(|r| r.offset + r.len > at) {
            while index < original.len() && original[index].offset <= at {
                let r = &original[index];
                if best.map_or(true, |b| r.offset + r.len > b.offset + b.len) {
                    best = Some(r);
                }
                index += 1;
            }
            if let Some(r) = best.filter(|r| r.offset + r.len > at) {
                let mut segment = r.clone();
                segment.offset = at;
                segment.len = r.offset + r.len - at;
                if segment.hash_size == 0 {
                    segment.file_offset += at - r.offset;
                } else {
                    let delta = r.hash_offset as u64 + at - r.offset;
                    segment.file_offset += delta / 32 * r.hash_size as u64;
                    segment.hash_offset = (delta % 32) as u8;
                }
                at += segment.len;
                ranges.push(segment);
            } else {
                best = None;
                if let Some(r) = original.get(index) {
                    at = r.offset;
                }
            }
        }
    }
    Ok(all)
}

#[derive(Debug)]
enum Piece {
    Literal(u64, u64),
    File(String, u64, u64),
    Hash(String, u64, u32, u8, u64),
}
fn pieces(ranges: &[RestoreRange], offset: u64, len: u64) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut at = offset;
    let end = offset + len;
    // Interval sweep: overlapping views can reference the same base bytes.
    // Prefer the longest currently covering interval, without consuming a
    // range merely because another snapshot described it first.
    let mut index = ranges.partition_point(|r| r.offset + r.len <= offset);
    let mut best: Option<&RestoreRange> = None;
    while at < end {
        while index < ranges.len() && ranges[index].offset <= at {
            let r = &ranges[index];
            if best.map_or(true, |b| r.offset + r.len > b.offset + b.len) {
                best = Some(r);
            }
            index += 1;
        }
        if let Some(r) = best.filter(|r| r.offset + r.len > at) {
            let n = (end - at).min(r.offset + r.len - at);
            if r.hash_size != 0 {
                let delta = r.hash_offset as u64 + at - r.offset;
                out.push(Piece::Hash(
                    r.path.clone(),
                    r.file_offset + delta / 32 * r.hash_size as u64,
                    r.hash_size,
                    (delta % 32) as u8,
                    n,
                ));
            } else {
                out.push(Piece::File(
                    r.path.clone(),
                    r.file_offset + at - r.offset,
                    n,
                ));
            }
            at += n;
        } else {
            let n = ranges.get(index).map_or(end, |r| end.min(r.offset)) - at;
            out.push(Piece::Literal(at - offset, n));
            at += n;
            best = None;
        }
    }
    out
}
fn copy_n(
    r: &mut impl Read,
    w: &mut impl Write,
    len: u64,
    options: &WriterOptions,
) -> Result<(), WriterError> {
    let mut buffer = vec![0; CHUNK];
    let mut left = len;
    while left > 0 {
        cancelled(options)?;
        let n = left.min(CHUNK as u64) as usize;
        r.read_exact(&mut buffer[..n])?;
        w.write_all(&buffer[..n])?;
        left -= n as u64;
    }
    Ok(())
}

/// Create a standalone Zstd frame inside the extraction root. `jobs` are the
/// exact snapshot identities and relative directories exported by the caller;
/// `manifest` describes the complete extracted views for edited-file repacks.
pub fn create_restoration(
    archive_path: impl AsRef<Path>,
    directory: impl AsRef<Path>,
    jobs: &[([u8; 32], String)],
    manifest: &str,
    options: &WriterOptions,
) -> Result<RestorationStats, WriterError> {
    create_restoration_with_mode(
        archive_path,
        directory,
        jobs,
        manifest,
        RestorationMode::Compact,
        options,
    )
}

pub fn create_restoration_with_mode(
    archive_path: impl AsRef<Path>,
    directory: impl AsRef<Path>,
    jobs: &[([u8; 32], String)],
    manifest: &str,
    mode: RestorationMode,
    options: &WriterOptions,
) -> Result<RestorationStats, WriterError> {
    validate_options(options)?;
    let archive_path = archive_path.as_ref();
    let directory = directory.as_ref();
    let destination = directory.join(RESTORATION_FILE);
    if destination.exists() {
        return Err(invalid("restoration recipe already exists"));
    }
    let root = SourceRoot::open(directory)?;
    let inventory = nodes(&root, options)?;
    let mut nx = File::open(archive_path)?;
    let original_hash = hash_file(&mut nx, options)?;
    let mut budget = MemoryBudget {
        current: 0,
        peak: 0,
        limit: options.max_memory_bytes,
    };
    let archive = parse_archive(&mut nx, 0, options, &mut budget)?;
    if budget.current + options.max_memory_bytes / 3 > options.max_memory_bytes {
        return Err(limit_error("restoration parser exceeds memory budget"));
    }
    let all = if mode == RestorationMode::Compact {
        ranges_for(archive_path, &archive, jobs, &budget, options)?
    } else {
        HashMap::new()
    };
    let mut reps: HashMap<u32, (u32, u64)> = HashMap::new();
    let mut files: Vec<_> = all.keys().copied().collect();
    files.sort_unstable();
    for file in files {
        let ranges = &all[&file];
        let f = &archive.files[file as usize];
        for i in 0..f.block_count {
            let offset = i as u64 * f.chunk_size_bytes as u64;
            let id = archive.block_refs[(f.first_ref + i) as usize];
            let range_index = ranges.partition_point(|r| r.offset + r.len <= offset);
            if ranges.get(range_index).is_some_and(|r| {
                r.offset < offset + archive.blocks[id as usize].logical_size as u64
            }) {
                reps.entry(id).or_insert((file, offset));
            }
        }
    }
    for app in &archive.applications {
        for &(_, root) in &app.source_resources {
            for entry in &archive.directories[root as usize] {
                let record = &archive.files[entry.target as usize];
                for i in 0..record.block_count {
                    reps.remove(&archive.block_refs[(record.first_ref + i) as usize]);
                }
            }
        }
    }
    // Level 13 with an 8 MiB history needs substantially more workspace
    // than a decoder. Reserve it along with both retained inventories.
    let inventory_memory: u64 = inventory
        .iter()
        .map(|n| 256 + n.path.len() as u64 * 4)
        .sum();
    let range_memory: u64 = all
        .values()
        .flat_map(|v| v.iter())
        .map(|r| 128 + r.path.len() as u64 * 2)
        .sum();
    if budget
        .current
        .saturating_add(inventory_memory)
        .saturating_add(range_memory)
        .saturating_add(128 * 1024 * 1024)
        > options.max_memory_bytes
    {
        return Err(limit_error(
            "Zstd level 13 restoration workspace exceeds memory budget",
        ));
    }
    let (file, mut guard) = unique_temp(
        directory
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
        OsStr::new(".nxrestore"),
    )?;
    let mut encoder = zstd::stream::write::Encoder::new(file, 13)?;
    encoder.window_log(23)?;
    encoder.include_checksum(true)?;
    let mut out = HashWrite {
        inner: encoder,
        hash: Sha256::new(),
    };
    out.write_all(MAGIC)?;
    num(&mut out, archive.file_size)?;
    out.write_all(&original_hash)?;
    string(&mut out, manifest)?;
    num(&mut out, inventory.len() as u64)?;
    out.write_all(&fingerprint(&inventory))?;
    let path_ids: HashMap<_, _> = inventory
        .iter()
        .enumerate()
        .map(|(i, n)| (n.path.as_str(), i as u32))
        .collect();
    for n in &inventory {
        string(&mut out, &n.path)?;
        out.write_all(&[u8::from(n.dir)])?;
        num(&mut out, n.size)?;
    }
    let mut ordered: Vec<_> = archive.blocks.iter().enumerate().collect();
    ordered.sort_by_key(|(_, b)| b.range.offset);
    let mut position = 0;
    let mut stored = Vec::new();
    let mut logical = Vec::new();
    let mut decoder = zstd::bulk::Decompressor::new()?;
    decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
    let mut compression_search = CompressionSearch::default();
    let mut referenced = 0;
    options.progress.add_total(archive.file_size);
    for (id, block) in ordered {
        cancelled(options)?;
        if block.range.offset < position {
            return Err(bad("overlapping restoration source blocks"));
        }
        if block.range.offset > position {
            out.write_all(&[0])?;
            num(&mut out, block.range.offset - position)?;
            nx.seek(SeekFrom::Start(position))?;
            copy_n(&mut nx, &mut out, block.range.offset - position, options)?;
            options.progress.add_done(block.range.offset - position);
        }
        decode_source_block(
            &mut nx,
            block,
            &mut stored,
            &mut logical,
            &mut decoder,
            options,
        )?;
        if !checks_match(block, &logical) {
            return Err(bad("restoration source block integrity mismatch"));
        }
        // decode_source_block does not retain stored RAW data in `stored`.
        let mut original = vec![0; block.stored_size as usize];
        nx.seek(SeekFrom::Start(block.range.offset))?;
        nx.read_exact(&mut original)?;
        let parts = reps
            .get(&(id as u32))
            .map(|(file, off)| pieces(&all[file], *off, block.logical_size as u64));
        let referenced_here = parts.as_ref().map_or(0, |p| {
            p.iter()
                .map(|p| match p {
                    Piece::File(_, _, n) | Piece::Hash(_, _, _, _, n) => *n,
                    _ => 0,
                })
                .sum::<u64>()
        });
        let encoding = if referenced_here > 128 && block.stored_size > 128 {
            if block.codec == 0 {
                Some((0, 0))
            } else {
                compression_search.find(&logical, &original, options)?
            }
        } else {
            None
        };
        if let (Some(parts), Some((level, window))) = (parts, encoding) {
            out.write_all(&[1])?;
            out.write_all(&[level as u8, window as u8])?;
            num(&mut out, block.logical_size as u64)?;
            num(&mut out, block.stored_size as u64)?;
            num(&mut out, parts.len() as u64)?;
            for p in parts {
                match p {
                    Piece::Literal(offset, n) => {
                        out.write_all(&[0])?;
                        num(&mut out, n)?;
                        out.write_all(&logical[offset as usize..(offset + n) as usize])?;
                    }
                    Piece::File(path, offset, n) => {
                        out.write_all(&[1])?;
                        num(&mut out, n)?;
                        let id = path_ids.get(path.as_str()).ok_or_else(|| {
                            invalid(format!(
                                "restoration source file missing from extraction: {path}"
                            ))
                        })?;
                        out.write_all(&id.to_le_bytes())?;
                        num(&mut out, offset)?;
                    }
                    Piece::Hash(path, offset, size, begin, len) => {
                        out.write_all(&[2])?;
                        num(&mut out, len)?;
                        let id = path_ids.get(path.as_str()).ok_or_else(|| {
                            invalid(format!(
                                "restoration source file missing from extraction: {path}"
                            ))
                        })?;
                        out.write_all(&id.to_le_bytes())?;
                        num(&mut out, offset)?;
                        out.write_all(&size.to_le_bytes())?;
                        out.write_all(&[begin])?;
                    }
                }
            }
            referenced += referenced_here;
        } else {
            out.write_all(&[0])?;
            num(&mut out, original.len() as u64)?;
            out.write_all(&original)?;
        }
        position = block.range.offset + block.stored_size as u64;
        options.progress.add_done(block.stored_size as u64);
        if out.inner.get_ref().metadata()?.len() > options.max_temporary_disk_bytes {
            return Err(limit_error("restoration recipe exceeds disk budget"));
        }
    }
    if position < archive.file_size {
        out.write_all(&[0])?;
        num(&mut out, archive.file_size - position)?;
        nx.seek(SeekFrom::Start(position))?;
        copy_n(&mut nx, &mut out, archive.file_size - position, options)?;
        options.progress.add_done(archive.file_size - position);
    }
    out.write_all(&[255])?;
    let checksum = out.hash.finish();
    out.inner.write_all(&checksum)?;
    let file = out.inner.finish()?;
    file.sync_all()?;
    let size = file.metadata()?.len();
    drop(file);
    if size > options.max_temporary_disk_bytes {
        return Err(limit_error("restoration recipe exceeds disk budget"));
    }
    // Refuse publication if extraction files changed during recipe creation.
    if hash_file(&mut nx, options)? != original_hash || nodes(&root, options)? != inventory {
        return Err(bad(
            "extracted files changed while creating restoration recipe",
        ));
    }
    publish_new(&guard.path, &destination)?;
    guard.disarm();
    Ok(RestorationStats {
        unchanged: true,
        archive_bytes: archive.file_size,
        recipe_bytes: size,
        referenced_bytes: referenced,
        metadata_staged_bytes: 0,
        files: inventory.iter().filter(|n| !n.dir).count(),
        compression_attempts: compression_search.attempts,
        exhaustive_searches: compression_search.exhaustive_searches,
        literal_fallback_blocks: compression_search.literal_fallback_blocks,
    })
}

/// Read bounded declaration hints for source discovery. The complete recipe
/// checksum and all references are verified by `restore_extraction` before any
/// output is published; these hints alone are never used as verification.
pub fn inspect_restoration(
    directory: impl AsRef<Path>,
    options: &WriterOptions,
) -> Result<crate::import::DeclarationManifest, WriterError> {
    let root = SourceRoot::open(directory.as_ref())?;
    let mut decoder = zstd::stream::read::Decoder::new(root.open_relative(RESTORATION_FILE)?)?;
    decoder.window_log_max(23)?;
    let mut header = [0; 48];
    decoder.read_exact(&mut header)?;
    if &header[..8] != MAGIC {
        return Err(bad("unsupported restoration recipe"));
    }
    let manifest = text(&mut decoder, MAX_TEXT.min(options.max_memory_bytes / 8))?;
    let limits = ImportLimits {
        max_manifest_bytes: MAX_TEXT,
        max_nodes: ImportLimits::MAX_NODES,
        max_inventory_bytes: (options.max_memory_bytes / 4).min(ImportLimits::MAX_INVENTORY_BYTES),
        ..Default::default()
    };
    crate::import::DeclarationManifest::read(manifest.as_bytes(), &limits)
}

/// Restore unchanged extractions to the exact original NX, including all its
/// embedded DNSP/DXCI recipes. Edited trees use the saved explicit declarations
/// and the existing materialized-tree writer, preserving their relative paths.
/// Edited restoration stages only the original metadata and source resources in
/// a compact file, in addition to the materialized writer's temporary space.
/// All temporary files are removed on error.
pub fn restore_extraction(
    directory: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    options: &WriterOptions,
) -> Result<RestorationStats, WriterError> {
    validate_options(options)?;
    let directory = directory.as_ref();
    let destination = destination.as_ref();
    if destination.exists() {
        return Err(invalid("restoration output already exists"));
    }
    let root = SourceRoot::open(directory)?;
    let recipe = root.open_relative(RESTORATION_FILE)?;
    let recipe_bytes = recipe.metadata()?.len();
    let mut decoder = zstd::stream::read::Decoder::new(BufReader::new(recipe))?;
    decoder.window_log_max(23)?;
    let mut input = HashRead {
        inner: decoder,
        hash: Sha256::new(),
        remaining: options.max_memory_bytes,
    };
    let mut magic = [0; 8];
    input.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(bad("unsupported restoration recipe"));
    }
    let original_size = number(&mut input)?;
    let original_digest = digest(&mut input)?;
    input.remaining = original_size
        .saturating_mul(16)
        .saturating_add(options.max_memory_bytes);
    if original_size > options.max_archive_size {
        return Err(limit_error("restored NX exceeds disk budget"));
    }
    let manifest = text(&mut input, MAX_TEXT.min(options.max_memory_bytes / 8))?;
    let count = number(&mut input)?;
    if count > options.max_entries as u64 || count * 256 > options.max_memory_bytes / 4 {
        return Err(limit_error("restoration inventory exceeds memory budget"));
    }
    let expected_fingerprint = digest(&mut input)?;
    let mut expected = Vec::new();
    let mut memory = 0;
    for _ in 0..count {
        let path = text(&mut input, 4096)?;
        crate::import::portable_path(&path, &ImportLimits::default())?;
        memory += 256 + path.len() as u64 * 4;
        if memory > options.max_memory_bytes / 4 {
            return Err(limit_error("restoration path budget exceeded"));
        }
        if path == RESTORATION_FILE {
            return Err(bad("restoration recipe cannot reference itself"));
        }
        let kind = byte(&mut input)?;
        if kind > 1 {
            return Err(bad("invalid restoration node kind"));
        }
        let size = number(&mut input)?;
        let digest = [0; 32];
        if expected.last().is_some_and(|n: &Node| n.path >= path) {
            return Err(bad("duplicate or unordered restoration path"));
        }
        expected.push(Node {
            path,
            dir: kind == 1,
            size,
            digest,
        });
    }
    let current = nodes(&root, options)?;
    let unchanged = fingerprint(&current) == expected_fingerprint;
    if unchanged && original_size > options.max_temporary_disk_bytes {
        return Err(limit_error("restored NX exceeds disk budget"));
    }
    let mut metadata_store = store::Store::new(original_size);
    let operation_prefix = 48
        + 8
        + manifest.len() as u64
        + 8
        + 32
        + expected
            .iter()
            .map(|n| 8 + n.path.len() as u64 + 1 + 8)
            .sum::<u64>();
    let files = current.iter().filter(|n| !n.dir).count();
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (file, guard) = unique_temp(parent, OsStr::new(".nxrestore-output"))?;
    let mut output = file;
    let mut whole = Sha256::new();
    let mut produced = 0u64;
    let mut referenced = 0u64;
    let mut buffer = vec![0; CHUNK];
    let mut cached_file: Option<(usize, File)> = None;
    options.progress.add_total(original_size);
    loop {
        cancelled(options)?;
        match byte(&mut input)? {
            255 => break,
            0 => {
                let n = number(&mut input)?;
                if n == 0 || n > original_size.saturating_sub(produced) {
                    return Err(bad("restoration literal exceeds original size"));
                }
                let mut left = n;
                while left > 0 {
                    cancelled(options)?;
                    let k = left.min(CHUNK as u64) as usize;
                    input.read_exact(&mut buffer[..k])?;
                    if unchanged {
                        whole.update(&buffer[..k]);
                    }
                    if unchanged {
                        output.write_all(&buffer[..k])?;
                    } else {
                        metadata_store.metadata(
                            &mut output,
                            produced + n - left,
                            &buffer[..k],
                            options,
                        )?;
                    }
                    left -= k as u64;
                }
                produced += n;
                options.progress.add_done(n);
            }
            1 => {
                let level = byte(&mut input)?;
                let window = byte(&mut input)?;
                if level > 13 || (level != 0 && !(16..=20).contains(&window)) {
                    return Err(bad("invalid restoration codec"));
                }
                let logical_size = number(&mut input)?;
                let stored_size = number(&mut input)?;
                if logical_size == 0
                    || logical_size > 1024 * 1024
                    || logical_size > options.max_memory_bytes / 16
                    || stored_size == 0
                    || stored_size > logical_size + 64 * 1024
                    || stored_size > original_size.saturating_sub(produced)
                {
                    return Err(bad("invalid restoration block size"));
                }
                let count = number(&mut input)?;
                if count == 0 || count > logical_size {
                    return Err(bad("invalid restoration piece count"));
                }
                let mut logical = vec![0; logical_size as usize];
                let mut at = 0;
                for _ in 0..count {
                    let kind = byte(&mut input)?;
                    let len = number(&mut input)?;
                    if len == 0 || len > logical_size - at {
                        return Err(bad("restoration piece exceeds block"));
                    }
                    let bytes = &mut logical[at as usize..(at + len) as usize];
                    match kind {
                        0 => input.read_exact(bytes)?,
                        1 | 2 => {
                            let mut id = [0; 4];
                            input.read_exact(&mut id)?;
                            let id = u32::from_le_bytes(id) as usize;
                            let node = expected
                                .get(id)
                                .filter(|n| !n.dir)
                                .ok_or_else(|| bad("restoration references an undeclared file"))?;
                            let offset = number(&mut input)?;
                            let mut hash_block_size = 0;
                            let (size, begin) = if kind == 2 {
                                let mut size = [0; 4];
                                input.read_exact(&mut size)?;
                                let size = u32::from_le_bytes(size);
                                let begin = byte(&mut input)?;
                                hash_block_size = size as usize;
                                if size < 512
                                    || !size.is_power_of_two()
                                    || size > 16 * 1024 * 1024
                                    || size as u64 > options.max_memory_bytes / 16
                                    || begin >= 32
                                {
                                    return Err(bad("invalid derived restoration hash"));
                                }
                                (
                                    (begin as u64 + len)
                                        .div_ceil(32)
                                        .checked_mul(size as u64)
                                        .ok_or_else(|| bad("derived hash range overflow"))?,
                                    begin,
                                )
                            } else {
                                (len, 0)
                            };
                            if offset.checked_add(size).map_or(true, |e| e > node.size) {
                                return Err(bad("restoration reference out of bounds"));
                            }
                            if unchanged {
                                if cached_file.as_ref().map(|(old, _)| *old) != Some(id) {
                                    cached_file = Some((id, root.open_relative(&node.path)?));
                                }
                                let file = &mut cached_file.as_mut().unwrap().1;
                                file.seek(SeekFrom::Start(offset))?;
                                if kind == 2 {
                                    let mut block = vec![0; hash_block_size];
                                    let mut written = 0;
                                    let mut start = begin as usize;
                                    while written < bytes.len() {
                                        cancelled(options)?;
                                        file.read_exact(&mut block)?;
                                        let hash = sha256(&block);
                                        let n = (bytes.len() - written).min(32 - start);
                                        bytes[written..written + n]
                                            .copy_from_slice(&hash[start..start + n]);
                                        start = 0;
                                        written += n;
                                    }
                                } else {
                                    file.read_exact(bytes)?;
                                }
                            }
                            referenced += len;
                        }
                        _ => return Err(bad("invalid restoration piece")),
                    }
                    at += len;
                }
                if at != logical_size {
                    return Err(bad("incomplete restoration block"));
                }
                if unchanged {
                    let stored = if level == 0 {
                        logical
                    } else {
                        compression(&logical, level as i32, window as u32)?
                    };
                    if stored.len() as u64 != stored_size {
                        return Err(bad("restoration block SHA-256 mismatch (files changed or incompatible Zstd encoder)"));
                    }
                    whole.update(&stored);
                    output.write_all(&stored)?;
                }
                produced += stored_size;
                options.progress.add_done(stored_size);
            }
            _ => return Err(bad("invalid restoration operation")),
        }
    }
    let checksum = input.hash.finish();
    let mut expected_checksum = [0; 32];
    input.inner.read_exact(&mut expected_checksum)?;
    let mut extra = [0];
    if input.inner.read(&mut extra)? != 0
        || checksum != expected_checksum
        || produced != original_size
    {
        return Err(bad("restoration recipe checksum or length mismatch"));
    }
    if unchanged {
        output.set_len(original_size)?;
    }
    let mut modified_output = None;
    if unchanged {
        if whole.finish() != original_digest {
            return Err(bad("restored NX SHA-256 differs from original"));
        }
        output.sync_all()?;
        drop(output);
        // Validate the archive before publishing, as well as its exact bytes.
        inspect_archive(&guard.path, options, IntegrityPolicy::Strict)?;
    } else {
        let limits = ImportLimits {
            max_manifest_bytes: MAX_TEXT.min(options.max_memory_bytes / 8),
            max_snapshots: 4096,
            max_nodes: (options.max_entries as usize).min(ImportLimits::MAX_NODES),
            max_inventory_bytes: (options.max_memory_bytes / 2)
                .min(ImportLimits::MAX_INVENTORY_BYTES),
            ..ImportLimits::default()
        };
        let declarations = crate::import::DeclarationManifest::read(manifest.as_bytes(), &limits)?;
        // Never silently ignore files added outside a declared snapshot root.
        for node in &current {
            if !node.dir
                && !declarations
                    .snapshots
                    .iter()
                    .any(|s| s.root == "." || node.path.starts_with(&format!("{}/", s.root)))
            {
                return Err(invalid(
                    "edited extraction has files outside its declared snapshot roots",
                ));
            }
        }
        metadata_store.sort();
        let resource_ranges = store::resource_ranges(&guard.path, &metadata_store, options)?;
        store::collect_resources(
            &root,
            &mut metadata_store,
            &mut output,
            &resource_ranges,
            operation_prefix,
            checksum,
            options,
        )?;
        drop(resource_ranges);
        drop(output);
        let recovered = metadata::recover(&guard.path, &metadata_store, &declarations, options)?;
        let mut bounded = options.clone();
        bounded.max_temporary_disk_bytes = options
            .max_temporary_disk_bytes
            .checked_sub(metadata_store.bytes)
            .ok_or_else(|| {
                limit_error("metadata store and modified NX exceed temporary disk budget")
            })?;
        let (file, candidate) = unique_temp(parent, OsStr::new(".nxrestore-modified"))?;
        drop(file);
        let result = crate::import::convert_extracted_external(
            directory,
            &declarations,
            &candidate.path,
            &limits,
            &bounded,
            &recovered.external,
        )?;
        let mut plan = RepackPlan::new();
        for (platform, application_id, key) in recovered.selected {
            let id = result
                .receipt
                .snapshots
                .get(&key)
                .ok_or_else(|| bad("written native metadata source is absent"))?
                .identity;
            plan.metadata_sources.push(MetadataSourceSelection {
                platform,
                application_id,
                snapshot_identity: id,
            });
        }
        if !plan.metadata_sources.is_empty() {
            // This private candidate only needs a metadata tail. Never make a
            // full payload copy just to restore the selected metadata source.
            bounded.auto_compact_free_percent = 0;
            bounded.max_temporary_disk_bytes = bounded
                .max_temporary_disk_bytes
                .saturating_sub(std::fs::metadata(&candidate.path)?.len());
            repack_archive(
                &candidate.path,
                &candidate.path,
                plan,
                &bounded,
                IntegrityPolicy::Strict,
            )?;
        }
        modified_output = Some(candidate);
    }
    cancelled(options)?;
    let candidate = modified_output.as_ref().map_or(&guard.path, |g| &g.path);
    publish_new(candidate, destination)?;
    let archive_bytes = std::fs::metadata(destination)?.len();
    Ok(RestorationStats {
        unchanged,
        archive_bytes,
        recipe_bytes,
        referenced_bytes: referenced,
        metadata_staged_bytes: metadata_store.bytes,
        files,
        compression_attempts: 0,
        exhaustive_searches: 0,
        literal_fallback_blocks: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        dir: std::path::PathBuf,
        original: std::path::PathBuf,
        extracted: std::path::PathBuf,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    fn fixture() -> Fixture {
        let id = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nx-restore-{}-{id}", std::process::id()));
        let source = dir.join("source");
        crate::import::synthetic::executable_tree(&source, 0x0400_0000_0010_0000, 3).unwrap();
        let mut state = 12345u64;
        let noise: Vec<u8> = (0..262144)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        std::fs::write(source.join("romfs/random.bin"), noise).unwrap();
        let original = dir.join("original.nx");
        crate::cli::run(
            vec![
                "pack-tree".into(),
                source.to_string_lossy().into_owned(),
                original.to_string_lossy().into_owned(),
                "--platform".into(),
                "switch2".into(),
                "--no-trail".into(),
            ],
            None,
            None,
        )
        .unwrap();
        let extracted = dir.join("extracted");
        crate::cli::run(
            vec![
                "extract".into(),
                original.to_string_lossy().into_owned(),
                extracted.to_string_lossy().into_owned(),
            ],
            None,
            None,
        )
        .unwrap();
        Fixture {
            dir,
            original,
            extracted,
        }
    }
    #[test]
    fn compressed_tree_roundtrip_and_one_resized_mod_keep_paths_and_other_files() {
        let f = fixture();
        let options = WriterOptions::default();
        let recipe = std::fs::read(f.extracted.join(RESTORATION_FILE)).unwrap();
        assert_eq!(&recipe[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
        assert!((recipe.len() as u64) < std::fs::metadata(&f.original).unwrap().len() / 2);
        let restored = f.dir.join("restored.nx");
        let result = restore_extraction(&f.extracted, &restored, &options).unwrap();
        assert!(result.unchanged);
        assert!(result.referenced_bytes > 200000);
        assert_eq!(
            std::fs::read(&restored).unwrap(),
            std::fs::read(&f.original).unwrap()
        );
        let random = std::fs::read(f.extracted.join("base/romfs/random.bin")).unwrap();
        std::fs::write(
            f.extracted.join("base/romfs/data.bin"),
            b"a resized modification",
        )
        .unwrap();
        let modified = f.dir.join("modified.nx");
        crate::cli::run(
            vec![
                "pack-tree".into(),
                f.extracted.to_string_lossy().into_owned(),
                modified.to_string_lossy().into_owned(),
                "--no-trail".into(),
                "--tree-root".into(),
                "base".into(),
            ],
            None,
            None,
        )
        .unwrap();
        let summary = inspect_archive(&modified, &options, IntegrityPolicy::Strict).unwrap();
        let output = f.dir.join("mod-tree");
        extract_tree(
            &modified,
            summary.applications[0].snapshots[0].identity,
            &output,
            &options,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(output.join("romfs/data.bin")).unwrap(),
            b"a resized modification"
        );
        assert_eq!(
            std::fs::read(output.join("romfs/random.bin")).unwrap(),
            random
        );
        assert_eq!(
            std::fs::read(output.join("exefs/main")).unwrap(),
            std::fs::read(f.extracted.join("base/exefs/main")).unwrap()
        );
        assert!(output.join("romfs/empty").is_dir());
    }
    #[test]
    fn missing_exported_file_returns_error_without_publishing_recipe() {
        let f = fixture();
        let options = WriterOptions::default();
        let manifest = inspect_restoration(&f.extracted, &options).unwrap();
        let summary = inspect_archive(&f.original, &options, IntegrityPolicy::Strict).unwrap();
        std::fs::remove_file(f.extracted.join(RESTORATION_FILE)).unwrap();
        std::fs::remove_file(f.extracted.join("base/romfs/random.bin")).unwrap();
        let error = create_restoration(
            &f.original,
            &f.extracted,
            &[(summary.applications[0].snapshots[0].identity, "base".into())],
            std::str::from_utf8(manifest.bytes()).unwrap(),
            &options,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("restoration source file missing from extraction: base/romfs/random.bin"));
        assert!(!f.extracted.join(RESTORATION_FILE).exists());
        assert!(f.extracted.join("base/exefs/main").is_file());
        assert!(!std::fs::read_dir(&f.dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".nxrestore")));
    }

    #[test]
    fn edited_restore_uses_compact_metadata_and_cleans_up_on_disk_limit() {
        let f = fixture();
        let original_size = std::fs::metadata(&f.original).unwrap().len();
        let options = WriterOptions {
            max_temporary_disk_bytes: original_size,
            ..Default::default()
        };
        // The same quota can restore the unchanged archive exactly.
        let exact = f.dir.join("exact.nx");
        assert!(
            restore_extraction(&f.extracted, &exact, &options)
                .unwrap()
                .unchanged
        );
        std::fs::write(f.extracted.join("base/romfs/random.bin"), b"modified").unwrap();
        let compact = f.dir.join("compact.nx");
        let stats = restore_extraction(&f.extracted, &compact, &options).unwrap();
        assert!(!stats.unchanged);
        assert!(stats.metadata_staged_bytes < original_size / 4);
        eprintln!("compact metadata benchmark: original_bytes={original_size} new_nx_bytes={} metadata_staged_bytes={}", stats.archive_bytes, stats.metadata_staged_bytes);
        inspect_archive(&compact, &options, IntegrityPolicy::Strict).unwrap();
        let modified = f.dir.join("modified.nx");
        let options = WriterOptions {
            max_temporary_disk_bytes: stats.metadata_staged_bytes,
            ..options
        };
        assert!(restore_extraction(&f.extracted, &modified, &options).is_err());
        assert!(!modified.exists());
        assert_eq!(
            std::fs::read(f.extracted.join("base/romfs/random.bin")).unwrap(),
            b"modified"
        );
        assert!(f.extracted.join(RESTORATION_FILE).is_file());
        assert!(!std::fs::read_dir(&f.dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".nxrestore")));
    }

    #[test]
    fn edited_large_compact_and_portable_recipes_fit_one_output_plus_small_overhead() {
        let f = fixture();
        let mut state = 12345u64;
        let noise: Vec<u8> = (0..16 * 1024 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        std::fs::write(f.dir.join("source/romfs/random.bin"), &noise).unwrap();
        drop(noise);
        let original = f.dir.join("large.nx");
        crate::cli::run(
            vec![
                "pack-tree".into(),
                f.dir.join("source").to_string_lossy().into_owned(),
                original.to_string_lossy().into_owned(),
                "--platform".into(),
                "switch2".into(),
                "--no-trail".into(),
                "--no-compress".into(),
            ],
            None,
            None,
        )
        .unwrap();
        let original_size = std::fs::metadata(&original).unwrap().len();
        assert!(original_size > 16 * 1024 * 1024);
        for portable in [false, true] {
            let dump = f.dir.join(if portable {
                "large-portable"
            } else {
                "large-compact"
            });
            let mut args = vec![
                "extract".into(),
                original.to_string_lossy().into_owned(),
                dump.to_string_lossy().into_owned(),
            ];
            if portable {
                args.push("--portable-restore".into());
            }
            crate::cli::run(args, None, None).unwrap();
            std::fs::write(dump.join("base/romfs/data.bin"), b"one mod").unwrap();
            let options = WriterOptions {
                max_temporary_disk_bytes: original_size + 128 * 1024,
                ..Default::default()
            };
            let output = f.dir.join(if portable {
                "large-portable-mod.nx"
            } else {
                "large-compact-mod.nx"
            });
            let stats = restore_extraction(&dump, &output, &options).unwrap();
            assert!(!stats.unchanged);
            assert!(stats.archive_bytes > 16 * 1024 * 1024);
            assert!(stats.metadata_staged_bytes < 128 * 1024);
            assert!(
                stats.archive_bytes + stats.metadata_staged_bytes
                    <= options.max_temporary_disk_bytes
            );
            inspect_archive(&output, &options, IntegrityPolicy::Strict).unwrap();
            eprintln!("large compact-store benchmark: portable={portable} original_bytes={original_size} new_nx_bytes={} metadata_staged_bytes={} temporary_budget={}", stats.archive_bytes, stats.metadata_staged_bytes, options.max_temporary_disk_bytes);
        }
    }

    #[test]
    fn unknown_snapshot_survives_exact_restore_and_blocks_modified_restore() {
        let f = fixture();
        let options = WriterOptions::default();
        let unknown = SnapshotInput {
            program_id: Some(0x0400_0000_0010_0000),
            snapshot_type: SnapshotType::Unknown,
            numeric_version: 0,
            content_id: None,
            addon_title_id: None,
            build_id: None,
            display_version: None,
            dependencies: vec![],
            metadata: None,
            native: Default::default(),
            root: DirectoryInput::new(vec![EntryInput::file(
                "opaque.bin",
                FileInput::from_bytes(b"must survive".to_vec()),
            )]),
        };
        repack_archive(
            &f.original,
            &f.original,
            RepackPlan {
                remove_snapshots: vec![],
                metadata_sources: vec![],
                additions: ArchiveInput::new(vec![ApplicationInput {
                    application_id: 0x0400_0000_0010_0000,
                    platform: Platform::Switch2,
                    programs: vec![ProgramInput {
                        program_id: 0x0400_0000_0010_0000,
                        kind: ProgramKind::Primary,
                        content_id: None,
                        npdm: None,
                    }],
                    snapshots: vec![unknown],
                }]),
            },
            &options,
            IntegrityPolicy::Strict,
        )
        .unwrap();
        let dump = f.dir.join("with-unknown");
        crate::cli::run(
            vec![
                "extract".into(),
                f.original.to_string_lossy().into_owned(),
                dump.to_string_lossy().into_owned(),
            ],
            None,
            None,
        )
        .unwrap();
        let exact = f.dir.join("with-unknown-exact.nx");
        restore_extraction(&dump, &exact, &options).unwrap();
        assert_eq!(
            std::fs::read(&exact).unwrap(),
            std::fs::read(&f.original).unwrap()
        );
        let summary = inspect_archive(&exact, &options, IntegrityPolicy::Strict).unwrap();
        assert_eq!(summary.applications[0].snapshots.len(), 2);
        assert!(summary.applications[0]
            .snapshots
            .iter()
            .any(|s| s.snapshot_type == SnapshotType::Unknown));
        std::fs::write(dump.join("base/romfs/data.bin"), b"edited").unwrap();
        let modified = f.dir.join("with-unknown-modified.nx");
        let error = restore_extraction(&dump, &modified, &options).unwrap_err();
        assert!(error
            .to_string()
            .contains("snapshots absent from the extracted declarations"));
        assert!(!modified.exists());
        assert!(dump.join(RESTORATION_FILE).is_file());
    }

    #[test]
    fn invalid_recipe_needs_explicit_opt_out_for_normal_tree_rebuild() {
        let f = fixture();
        let recipe = f.extracted.join(RESTORATION_FILE);
        std::fs::write(&recipe, b"corrupt recipe").unwrap();
        let output = f.dir.join("normal.nx");
        let args = vec![
            "pack-tree".into(),
            f.extracted.to_string_lossy().into_owned(),
            output.to_string_lossy().into_owned(),
            "--platform".into(),
            "switch2".into(),
            "--no-trail".into(),
        ];
        let error = crate::cli::run(args.clone(), None, None).unwrap_err();
        assert!(error.to_string().contains("--ignore-restore"));
        assert!(!output.exists());
        let mut args = args;
        args.push("--ignore-restore".into());
        crate::cli::run(args, None, None).unwrap();
        inspect_archive(&output, &WriterOptions::default(), IntegrityPolicy::Strict).unwrap();
        assert_eq!(std::fs::read(recipe).unwrap(), b"corrupt recipe");
    }

    #[test]
    fn require_restore_reports_failure_and_keeps_extracted_files() {
        let f = fixture();
        let output = f.dir.join("required");
        let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = messages.clone();
        let result = crate::cli::run(
            vec![
                "extract".into(),
                f.original.to_string_lossy().into_owned(),
                output.to_string_lossy().into_owned(),
                "--require-restore".into(),
                "--temp-mib".into(),
                "0".into(),
            ],
            Some(Arc::new(move |_, line| {
                captured.lock().unwrap().push(line.to_owned())
            })),
            None,
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("required restoration recipe"));
        assert!(output.join("base/exefs/main").is_file());
        assert!(!output.join(RESTORATION_FILE).exists());
        assert!(messages
            .lock()
            .unwrap()
            .iter()
            .any(|s| s == "@RESTORE_STATUS\tfailed"));
        let ok = f.dir.join("required-ok");
        let captured = messages.clone();
        crate::cli::run(
            vec![
                "extract".into(),
                f.original.to_string_lossy().into_owned(),
                ok.to_string_lossy().into_owned(),
                "--require-restore".into(),
            ],
            Some(Arc::new(move |_, line| {
                captured.lock().unwrap().push(line.to_owned())
            })),
            None,
        )
        .unwrap();
        assert!(ok.join(RESTORATION_FILE).is_file());
        assert!(messages
            .lock()
            .unwrap()
            .iter()
            .any(|s| s == "@RESTORE_STATUS\tcreated"));
        let conflict = f.dir.join("conflicting-flags");
        assert!(crate::cli::run(
            vec![
                "extract".into(),
                f.original.to_string_lossy().into_owned(),
                conflict.to_string_lossy().into_owned(),
                "--require-restore".into(),
                "--no-restore".into()
            ],
            None,
            None
        )
        .is_err());
        assert!(!conflict.exists());
    }

    #[test]
    fn checksum_truncation_and_existing_outputs_never_publish_or_overwrite() {
        let f = fixture();
        let options = WriterOptions::default();
        let path = f.extracted.join(RESTORATION_FILE);
        let good = std::fs::read(&path).unwrap();
        let mut decoded = zstd::stream::decode_all(&good[..]).unwrap();
        *decoded.last_mut().unwrap() ^= 1;
        std::fs::write(&path, zstd::stream::encode_all(&decoded[..], 13).unwrap()).unwrap();
        let output = f.dir.join("bad.nx");
        assert!(restore_extraction(&f.extracted, &output, &options).is_err());
        assert!(!output.exists());
        std::fs::write(&path, &good[..good.len() - 5]).unwrap();
        assert!(restore_extraction(&f.extracted, &output, &options).is_err());
        assert!(!output.exists());
        std::fs::write(&path, good).unwrap();
        std::fs::write(&output, b"keep existing").unwrap();
        assert!(restore_extraction(&f.extracted, &output, &options).is_err());
        assert_eq!(std::fs::read(output).unwrap(), b"keep existing");
        assert!(!std::fs::read_dir(&f.dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".nxrestore-output")));
    }
    #[test]
    fn disk_limits_cancellation_and_untracked_files_fail_without_output() {
        let f = fixture();
        let output = f.dir.join("bad.nx");
        let options = WriterOptions {
            max_temporary_disk_bytes: 1,
            ..Default::default()
        };
        assert!(restore_extraction(&f.extracted, &output, &options).is_err());
        assert!(!output.exists());
        assert!(crate::cli::run(
            vec![
                "pack-tree".into(),
                f.extracted.to_string_lossy().into_owned(),
                output.to_string_lossy().into_owned(),
                "--temp-mib".into(),
                "0".into()
            ],
            None,
            None
        )
        .is_err());
        assert!(!output.exists());
        let options = WriterOptions::default();
        options.cancellation.cancel();
        assert!(restore_extraction(&f.extracted, &output, &options).is_err());
        assert!(!output.exists());
        std::fs::write(f.extracted.join("untracked.bin"), b"must not be discarded").unwrap();
        assert!(restore_extraction(&f.extracted, &output, &WriterOptions::default()).is_err());
        assert!(!output.exists());
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_supply_restoration_data() {
        let f = fixture();
        let target = f.extracted.join("base/romfs/data.bin");
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&f.original, &target).unwrap();
        let output = f.dir.join("bad.nx");
        assert!(restore_extraction(&f.extracted, &output, &WriterOptions::default()).is_err());
        assert!(!output.exists());
    }
    #[test]
    fn optional_recipe_failure_and_opt_out_keep_valid_extractions() {
        let f = fixture();
        for (name, flags, warning) in [
            ("low-memory", vec!["--memory-mib", "64"], true),
            ("low-disk", vec!["--temp-mib", "0"], true),
            ("disabled", vec!["--no-restore", "--temp-mib", "0"], false),
        ] {
            let output = f.dir.join(name);
            let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured = messages.clone();
            let mut args = vec![
                "extract".into(),
                f.original.to_string_lossy().into_owned(),
                output.to_string_lossy().into_owned(),
            ];
            args.extend(flags.into_iter().map(String::from));
            crate::cli::run(
                args,
                Some(Arc::new(move |_, line| {
                    captured.lock().unwrap().push(line.to_owned())
                })),
                None,
            )
            .unwrap();
            assert_eq!(
                std::fs::read(output.join("base/romfs/random.bin")).unwrap(),
                std::fs::read(f.extracted.join("base/romfs/random.bin")).unwrap()
            );
            assert!(output.join("base/exefs/main").is_file());
            assert!(!output.join(RESTORATION_FILE).exists());
            let expected_status = if warning {
                "@RESTORE_STATUS\tfailed"
            } else {
                "@RESTORE_STATUS\tdisabled"
            };
            assert!(messages
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == expected_status));
            assert_eq!(
                messages
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|m| m.contains("Restoration recipe not created")),
                warning
            );
        }
        assert!(crate::cli::run(
            vec![
                "contents".into(),
                f.original.to_string_lossy().into_owned(),
                "--no-restore".into()
            ],
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn compression_discovery_is_cached_bounded_and_never_approximate() {
        let options = WriterOptions::default();
        let mut search = CompressionSearch::default();
        for value in 0..64u8 {
            let logical = vec![value; 4096];
            let mut foreign = zstd::bulk::Compressor::new(3).unwrap();
            foreign
                .set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))
                .unwrap();
            let stored = foreign.compress(&logical).unwrap();
            assert!(search.find(&logical, &stored, &options).unwrap().is_none());
        }
        assert_eq!(search.exhaustive_searches, 4);
        assert_eq!(search.literal_fallback_blocks, 64);
        assert!(search.attempts <= 4 * 65 + 64);
        let logical = crate::import::synthetic::nso(3, false);
        let stored = compression(&logical, 3, 20).unwrap();
        let attempts = search.attempts;
        let params = search.find(&logical, &stored, &options).unwrap().unwrap();
        assert_eq!(compression(&logical, params.0, params.1).unwrap(), stored);
        assert_eq!(search.attempts, attempts + 1);
        // A newly discovered nondefault encoder is tried first on reuse.
        let mut fresh = CompressionSearch::default();
        let stored = compression(&logical, 13, 16).unwrap();
        let params = fresh.find(&logical, &stored, &options).unwrap().unwrap();
        assert_eq!(compression(&logical, params.0, params.1).unwrap(), stored);
        let attempts = fresh.attempts;
        assert_eq!(
            fresh.find(&logical, &stored, &options).unwrap(),
            Some(params)
        );
        assert_eq!(fresh.attempts, attempts + 1);
    }
    #[test]
    fn portable_recipe_uses_no_encoder_and_replays_exact_original_bytes() {
        let f = fixture();
        let portable = f.dir.join("portable");
        crate::cli::run(
            vec![
                "extract".into(),
                f.original.to_string_lossy().into_owned(),
                portable.to_string_lossy().into_owned(),
                "--portable-restore".into(),
            ],
            None,
            None,
        )
        .unwrap();
        let decoded =
            zstd::stream::decode_all(File::open(portable.join(RESTORATION_FILE)).unwrap()).unwrap();
        let mut reader = std::io::Cursor::new(decoded);
        let mut header = [0; 48];
        reader.read_exact(&mut header).unwrap();
        text(&mut reader, MAX_TEXT).unwrap();
        let count = number(&mut reader).unwrap();
        digest(&mut reader).unwrap();
        for _ in 0..count {
            text(&mut reader, 4096).unwrap();
            byte(&mut reader).unwrap();
            number(&mut reader).unwrap();
        }
        loop {
            match byte(&mut reader).unwrap() {
                255 => break,
                0 => {
                    let len = number(&mut reader).unwrap();
                    reader.seek(SeekFrom::Current(len as i64)).unwrap();
                }
                _ => panic!("portable recipe contains encoder-dependent operations"),
            }
        }
        let restored = f.dir.join("portable-restored.nx");
        restore_extraction(&portable, &restored, &WriterOptions::default()).unwrap();
        assert_eq!(
            std::fs::read(restored).unwrap(),
            std::fs::read(&f.original).unwrap()
        );
        std::fs::write(portable.join("base/romfs/random.bin"), b"portable edit").unwrap();
        let edited = f.dir.join("portable-edited.nx");
        let stats = restore_extraction(
            &portable,
            &edited,
            &WriterOptions {
                max_temporary_disk_bytes: std::fs::metadata(&f.original).unwrap().len(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!stats.unchanged);
        assert!(stats.metadata_staged_bytes < std::fs::metadata(&f.original).unwrap().len() / 4);
        inspect_archive(&edited, &WriterOptions::default(), IntegrityPolicy::Strict).unwrap();
        assert!(crate::cli::run(
            vec![
                "extract".into(),
                f.original.to_string_lossy().into_owned(),
                f.dir.join("invalid").to_string_lossy().into_owned(),
                "--portable-restore".into(),
                "--no-restore".into()
            ],
            None,
            None
        )
        .is_err());
    }
}
