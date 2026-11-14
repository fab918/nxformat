//! NUP v1 import (a55uka/nup SPEC.md, 1.0-draft).
//!
//! A NUP is decoded once into a private staging package: one complete
//! extracted tree per snapshot, its native control metadata, and generated
//! declarations. That package then goes through `convert_extracted`, so NUP
//! content gets exactly the validation any extracted tree gets (NPDM, NSO,
//! Build ID, dependencies) and nothing is trusted from NUP alone.
//!
//! Mapping:
//! - `base` -> base snapshot.
//! - `update` -> complete update snapshot that requires the base. The tree is
//!   materialized as native patching resolves it: the update ExeFS replaces
//!   the base ExeFS when present, the RomFS is the base RomFS shadowed by the
//!   update paths. Base files are hard-linked into the staged update tree when
//!   the filesystem allows it.
//! - `update` with `standalone_update` and no base -> standalone update.
//! - `addon` -> DLC snapshot stored by its add-on title ID (snapshot flag
//!   bit 1): a NUP has no NCA, so no native Content ID, and NX never invents
//!   one. The application is the title ID minus 0x1000, low 12 bits cleared.
//! - TLV NACP and icons -> native snapshot metadata (localized titles/icons).
use super::*;
use crate::hash::Sha256;
use crate::native::NACP_LANGUAGE_MAP;
use crate::WriterOptions;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub(super) const NONE: u32 = 0xffff_ffff;
pub(super) const SUPERBLOCK: usize = 0x80;
pub(super) const PACKAGE: usize = 0x40;
pub(super) const DIRECTORY: usize = 0x18;
pub(super) const FILE: usize = 0x50;

#[derive(Clone, Debug, Default)]
pub struct NupOptions {
    /// Caller-declared platform. NUP v1 does not encode a platform field.
    pub platform: Option<crate::Platform>,
    /// Leave add-ons out instead of failing. They are listed in the stage.
    pub skip_addons: bool,
    /// Gets the bytes to decode as its total and each decoded byte as done.
    pub progress: crate::ProgressCounter,
    /// Stage only ExeFS and metadata; RomFS and data files are read from the
    /// NUP while the archive is written (`convert_nup`), so nothing large is
    /// decoded to disk, and an update names its base's files without reading
    /// them again. Progress is then the writer's.
    pub stream: bool,
}

/// One NUP file read in place: its stored extent, codec and expected bytes.
#[derive(Debug)]
pub(crate) struct NupExtent {
    pub(crate) path: std::sync::Arc<PathBuf>,
    pub(crate) name: String,
    pub(crate) offset: u64,
    pub(crate) stored: u64,
    pub(crate) codec: u8,
    pub(crate) size: u64,
    pub(crate) sha256: [u8; 32],
}
impl NupExtent {
    /// The decoded bytes, checked for size and SHA-256 at the end.
    pub(crate) fn reader(
        &self,
        token: &crate::CancellationToken,
    ) -> Result<Box<dyn Read>, WriterError> {
        let mut file = File::open(self.path.as_ref())?;
        file.seek(SeekFrom::Start(self.offset))?;
        let stored = file.take(self.stored);
        let inner: Box<dyn Read> = if self.codec == 1 && self.size != 0 {
            Box::new(zstd::stream::read::Decoder::new(stored)?)
        } else {
            Box::new(stored)
        };
        Ok(Box::new(NupReader {
            inner,
            remaining: self.size,
            hash: Sha256::new(),
            expected: self.sha256,
            done: false,
            token: token.clone(),
            name: self.name.clone(),
        }))
    }
}
struct NupReader {
    inner: Box<dyn Read>,
    remaining: u64,
    hash: Sha256,
    expected: [u8; 32],
    done: bool,
    token: crate::CancellationToken,
    name: String,
}
impl Read for NupReader {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if b.is_empty() {
            return Ok(0);
        }
        if self.token.is_cancelled() {
            return Err(io::Error::other("import cancelled"));
        }
        let corrupt = |what: &str, name: &str| {
            io::Error::new(io::ErrorKind::InvalidData, format!("NUP: {name}: {what}"))
        };
        if self.remaining == 0 {
            if !self.done {
                let mut extra = [0u8; 1];
                let more = loop {
                    match self.inner.read(&mut extra) {
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        other => break other?,
                    }
                };
                if more != 0 {
                    return Err(corrupt("decodes past its size", &self.name));
                }
                if self.hash.clone().finish() != self.expected {
                    return Err(corrupt("size or SHA-256 mismatch", &self.name));
                }
                self.done = true;
            }
            return Ok(0);
        }
        let n = b.len().min(self.remaining.min(1 << 20) as usize);
        let got = self.inner.read(&mut b[..n])?;
        if got == 0 {
            return Err(corrupt("truncated", &self.name));
        }
        self.hash.update(&b[..got]);
        self.remaining -= got as u64;
        Ok(got)
    }
}

/// One add-on left out of the conversion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedAddon {
    pub title_id: u64,
    pub version: u32,
}

/// Decoded NUP ready for `convert_extracted`. The staging directory is removed
/// when this value is dropped.
pub struct NupStage {
    dir: StageDir,
    /// Streamed files per snapshot key: (path in the tree, source).
    pub(crate) external: super::External,
    pub manifest: DeclarationManifest,
    pub skipped_addons: Vec<SkippedAddon>,
    /// Staged file and metadata bytes (hard links not counted).
    pub staged_bytes: u64,
}
impl NupStage {
    pub fn package(&self) -> PathBuf {
        self.dir.path.join("pkg")
    }

    /// Keep decoded staging files inside the same temporary-disk budget as
    /// the writer's output and work files.
    pub fn remaining_writer_options(
        &self,
        options: &WriterOptions,
    ) -> Result<WriterOptions, WriterError> {
        let mut remaining = options.clone();
        remaining.max_temporary_disk_bytes = options
            .max_temporary_disk_bytes
            .checked_sub(self.staged_bytes)
            .ok_or_else(|| limit("NUP staging exceeds temporary disk budget"))?;
        Ok(remaining)
    }

    /// Files read from the NUP while writing: (path in its tree, size).
    pub fn streamed_files(&self) -> impl Iterator<Item = (&str, u64)> {
        self.external
            .files
            .values()
            .flatten()
            .map(|(path, file)| (path.as_str(), file.size()))
    }
    pub(crate) fn external(&self) -> &super::External {
        &self.external
    }
}

pub struct NupConversionStats {
    pub conversion: TreeConversionStats,
    pub skipped_addons: Vec<SkippedAddon>,
    pub staged_bytes: u64,
    /// Includes the decoded NUP stage and the writer's temporary files.
    pub peak_temporary_disk_bytes: u64,
}

/// Decode `nup` beside `destination`, then convert it to `destination`.
pub fn convert_nup(
    nup: &Path,
    destination: &Path,
    nup_options: &NupOptions,
    limits: &ImportLimits,
    options: &WriterOptions,
) -> Result<NupConversionStats, WriterError> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stage = stage_nup_with_options(nup, parent, nup_options, limits, options)?;
    let remaining = stage.remaining_writer_options(options)?;
    let conversion = super::convert::convert_extracted_external(
        &stage.package(),
        &stage.manifest,
        destination,
        limits,
        &remaining,
        &stage.external,
    )?;
    let peak_temporary_disk_bytes = stage
        .staged_bytes
        .checked_add(conversion.peak_temporary_disk_bytes)
        .ok_or_else(|| limit("NUP temporary disk accounting overflow"))?;
    Ok(NupConversionStats {
        conversion,
        skipped_addons: stage.skipped_addons.clone(),
        staged_bytes: stage.staged_bytes,
        peak_temporary_disk_bytes,
    })
}

/// Resolved tree entries (directories and files, after update overlay)
/// summed over every package: the import ceiling `ImportLimits::validate`
/// accepts, and what `pack-nup` already enforces after staging. Packages may
/// share directory nodes, so per-walk limits alone let a small TOC expand to
/// packages x nodes (NUP issue #27).
const MAX_RESOLVED_ENTRIES: usize = 1_000_000;

fn charge_entries(total: &mut usize, tree: &Tree, ceiling: usize) -> Result<(), WriterError> {
    *total = total
        .saturating_add(tree.dirs.len())
        .saturating_add(tree.files.len());
    if *total > ceiling {
        return Err(limit(
            "NUP packages resolve to more tree entries than the import ceiling",
        ));
    }
    Ok(())
}

struct StageDir {
    path: PathBuf,
}
impl StageDir {
    fn create(path: PathBuf) -> Result<Self, WriterError> {
        // Acquire cleanup ownership only after successful creation. A name
        // collision must never remove another operation's staging directory.
        fs::create_dir(&path)?;
        Ok(Self { path })
    }
}
impl Drop for StageDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn bad(message: &str) -> WriterError {
    invalid(&format!("NUP: {message}"))
}
fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

struct Package {
    ordinal: usize,
    title_id: u64,
    root: u32,
    exefs: u32,
    romfs: u32,
    version: u32,
    kind: u8,
    flags: u8,
    meta_offset: u64,
    meta_size: u32,
}
struct Directory {
    name: String,
    first_child: u32,
    child_count: u32,
}
#[derive(Clone)]
struct NupFile {
    name: String,
    size: u64,
    extent_offset: u64,
    extent_size: u64,
    sha256: [u8; 32],
    codec: u8,
}
struct Toc {
    packages: Vec<Package>,
    dirs: Vec<Directory>,
    files: Vec<NupFile>,
    children: Vec<u32>,
}

/// A resolved tree: relative path -> file index, plus every directory path.
#[derive(Clone, Default)]
struct Tree {
    files: BTreeMap<String, usize>,
    dirs: BTreeSet<String>,
}

fn name(pool: &[u8], offset: u32, length: u32, root: bool) -> Result<String, WriterError> {
    let end = (offset as u64)
        .checked_add(length as u64)
        .filter(|e| *e <= pool.len() as u64)
        .ok_or_else(|| bad("name outside the string pool"))?;
    let text = std::str::from_utf8(&pool[offset as usize..end as usize])
        .map_err(|_| bad("name is not UTF-8"))?;
    if !root
        && (text.is_empty()
            || text == "."
            || text == ".."
            || text.contains('/')
            || text.contains('\\')
            || text.contains('\0'))
    {
        return Err(bad("invalid entry name"));
    }
    Ok(text.to_owned())
}

fn package_kind(kind: u8) -> &'static str {
    match kind {
        0 => "base",
        1 => "update",
        2 => "addon",
        _ => "unknown",
    }
}

/// Keep useful package identities in structural errors: tools following the
/// official NUP reader cannot list them once the child table is malformed.
fn package_summary(packages: &[Package]) -> String {
    let mut entries: Vec<_> = packages
        .iter()
        .take(8)
        .map(|p| {
            format!(
                "#{} {} {:016x} (v{})",
                p.ordinal,
                package_kind(p.kind),
                p.title_id,
                p.version
            )
        })
        .collect();
    if packages.len() > entries.len() {
        entries.push(format!("and {} more", packages.len() - entries.len()));
    }
    entries.join(", ")
}

/// Check every directory's child-slot range, even if that directory is not
/// reachable from a package root. Accept only an unreferenced, all-zero tail
/// as producer padding; unknown or overlapping slots are never interpreted as
/// valid entries. This does not change the NX archive format.
fn validate_child_table(
    dirs: &[Directory],
    children: &[u32],
    packages: &[Package],
) -> Result<(), WriterError> {
    let present = children.len() as u64;
    let declared: u64 = dirs.iter().map(|d| u64::from(d.child_count)).sum();
    let context = || {
        let difference = present.abs_diff(declared);
        let direction = if present >= declared {
            "extra"
        } else {
            "missing"
        };
        format!(
            "child table: {declared} slots declared, {present} present ({difference} {direction}, {} bytes); packages: {}",
            difference * 4,
            package_summary(packages)
        )
    };
    let mut ranges = Vec::with_capacity(dirs.len());
    for (index, dir) in dirs.iter().enumerate() {
        if dir.child_count == 0 {
            continue;
        }
        let first = u64::from(dir.first_child);
        let end = first + u64::from(dir.child_count);
        if end > present {
            return Err(bad(&format!(
                "{}; directory #{index} references child slots {first}..{end} outside the table",
                context()
            )));
        }
        ranges.push((first, end));
    }
    ranges.sort_unstable();
    let mut covered = 0u64;
    for (first, end) in ranges {
        if first != covered {
            let reason = if first < covered { "overlap" } else { "gap" };
            return Err(bad(&format!(
                "{}; child-slot ranges contain a {reason} at index {first}",
                context()
            )));
        }
        covered = end;
    }
    if covered != declared {
        return Err(bad(&format!(
            "{}; directory child-slot ranges do not match declared counts",
            context()
        )));
    }
    // Some producers leave harmless zero-filled slots immediately before the
    // string pool. All slots must be unreferenced and zero to accept them.
    if children[covered as usize..].iter().any(|&slot| slot != 0) {
        return Err(bad(&format!(
            "{}; unused child slots are not zero padding",
            context()
        )));
    }
    Ok(())
}

fn read_toc(file: &mut File, limits: &ImportLimits) -> Result<(Toc, u64), WriterError> {
    let physical = file.metadata()?.len();
    let mut sb = [0u8; SUPERBLOCK];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut sb)
        .map_err(|_| bad("file is shorter than its superblock"))?;
    if &sb[..4] != b"NUP\x01" {
        return Err(unsupported("not a NUP v1 file"));
    }
    let header_size = u16_at(&sb, 4) as u64;
    let toc_offset = u64_at(&sb, 0x08);
    let toc_size = u64_at(&sb, 0x10);
    let blob_offset = u64_at(&sb, 0x38);
    if header_size < SUPERBLOCK as u64
        || u16_at(&sb, 6) != 1
        || toc_offset != header_size
        || u64_at(&sb, 0x40) != physical
        || u32_at(&sb, 0x4c) != 0
        || blob_offset % 16 != 0
        || toc_offset
            .checked_add(toc_size)
            .map_or(true, |e| e > blob_offset)
        || blob_offset > physical
    {
        return Err(bad("superblock is invalid or unsupported"));
    }
    if toc_size > limits.max_inventory_bytes / 2 || toc_size < 0x30 {
        return Err(limit("NUP TOC exceeds the inventory budget"));
    }
    let mut toc = vec![0; toc_size as usize];
    file.seek(SeekFrom::Start(toc_offset))?;
    file.read_exact(&mut toc)?;
    if crate::hash::sha256(&toc) != sb[0x18..0x38] {
        return Err(bad("TOC SHA-256 mismatch"));
    }
    if &toc[..4] != b"TOC\x01" || u64_at(&toc, 0x28) != 0 {
        return Err(bad("TOC header is invalid"));
    }
    let package_count = u32_at(&toc, 4) as u64;
    let dir_count = u64_at(&toc, 0x08);
    let file_count = u64_at(&toc, 0x10);
    let pool_offset = u64_at(&toc, 0x18);
    let pool_size = u64_at(&toc, 0x20);
    if package_count != u32_at(&sb, 0x48) as u64 || package_count == 0 {
        return Err(bad("package count disagrees with the superblock"));
    }
    let nodes = dir_count.saturating_add(file_count);
    if nodes > limits.max_nodes as u64 || nodes >= NONE as u64 {
        return Err(limit("NUP node budget"));
    }
    let children_at = 0x30
        + package_count * PACKAGE as u64
        + dir_count * DIRECTORY as u64
        + file_count * FILE as u64;
    if children_at > pool_offset
        || (pool_offset - children_at) % 4 != 0
        || pool_offset
            .checked_add(pool_size)
            .map_or(true, |e| e > toc_size)
    {
        return Err(bad("TOC regions are out of bounds"));
    }
    let pool = &toc[pool_offset as usize..(pool_offset + pool_size) as usize];
    let mut at = 0x30usize;
    let mut packages = Vec::new();
    for ordinal in 0..package_count {
        let p = &toc[at..at + PACKAGE];
        if u16_at(p, 0x1a) != 0 || u32_at(p, 0x1c) != 0 || p[0x2c..].iter().any(|b| *b != 0) {
            return Err(bad("package reserved fields are not zero"));
        }
        packages.push(Package {
            ordinal: ordinal as usize,
            title_id: u64_at(p, 0),
            root: u32_at(p, 0x08),
            exefs: u32_at(p, 0x0c),
            romfs: u32_at(p, 0x10),
            version: u32_at(p, 0x14),
            kind: p[0x18],
            flags: p[0x19],
            meta_offset: u64_at(p, 0x20),
            meta_size: u32_at(p, 0x28),
        });
        at += PACKAGE;
    }
    let mut dirs = Vec::new();
    for _ in 0..dir_count {
        let d = &toc[at..at + DIRECTORY];
        if u64_at(d, 0x10) != 0 {
            return Err(bad("directory reserved field is not zero"));
        }
        dirs.push(Directory {
            name: name(pool, u32_at(d, 0), u32_at(d, 4), true)?,
            first_child: u32_at(d, 0x08),
            child_count: u32_at(d, 0x0c),
        });
        at += DIRECTORY;
    }
    let mut files = Vec::new();
    for _ in 0..file_count {
        let f = &toc[at..at + FILE];
        if f[0x41] != 0 || u16_at(f, 0x42) != 0 || f[0x44..].iter().any(|b| *b != 0) {
            return Err(bad("file reserved fields are not zero"));
        }
        let file = NupFile {
            name: name(pool, u32_at(f, 0), u32_at(f, 4), false)?,
            size: u64_at(f, 0x08),
            extent_offset: u64_at(f, 0x10),
            extent_size: u64_at(f, 0x18),
            sha256: f[0x20..0x40].try_into().unwrap(),
            codec: f[0x40],
        };
        let in_blob = file.extent_size == 0
            || (file.extent_offset >= blob_offset
                && file.extent_offset % 16 == 0
                && file
                    .extent_offset
                    .checked_add(file.extent_size)
                    .is_some_and(|e| e <= physical));
        let shape = match file.codec {
            0 => file.extent_size == file.size,
            1 => (file.size == 0) == (file.extent_size == 0),
            _ => return Err(unsupported("NUP codec other than raw/zstd")),
        };
        if !in_blob || !shape || file.size > limits.max_file_bytes {
            return Err(bad("file extent is invalid"));
        }
        files.push(file);
        at += FILE;
    }
    let children: Vec<u32> = toc[children_at as usize..pool_offset as usize]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    validate_child_table(&dirs, &children, &packages)?;
    Ok((
        Toc {
            packages,
            dirs,
            files,
            children,
        },
        physical,
    ))
}

impl Toc {
    /// Resolve a directory into `tree` under `prefix`. Every node may appear
    /// once per walk, so cycles and shared subtrees are rejected.
    fn walk(
        &self,
        dir: u32,
        prefix: &str,
        tree: &mut Tree,
        seen: &mut BTreeSet<u32>,
        depth: usize,
        limits: &ImportLimits,
    ) -> Result<(), WriterError> {
        let d = self
            .dirs
            .get(dir as usize)
            .ok_or_else(|| bad("directory index out of range"))?;
        if depth > limits.max_depth || !seen.insert(dir) {
            return Err(bad("directory tree is cyclic or too deep"));
        }
        let end = (d.first_child as u64) + d.child_count as u64;
        if end > self.children.len() as u64 {
            return Err(bad("children range out of bounds"));
        }
        let mut names = BTreeSet::new();
        for &node in &self.children[d.first_child as usize..end as usize] {
            let (child, is_dir) = if (node as usize) < self.dirs.len() {
                (self.dirs[node as usize].name.as_str(), true)
            } else {
                let i = node as usize - self.dirs.len();
                (
                    self.files
                        .get(i)
                        .ok_or_else(|| bad("child node out of range"))?
                        .name
                        .as_str(),
                    false,
                )
            };
            if is_dir && child.is_empty() {
                return Err(bad("directory child has an empty name"));
            }
            if !names.insert(child.to_ascii_lowercase()) {
                return Err(bad("duplicate or case-colliding sibling names"));
            }
            let path = if prefix.is_empty() {
                child.to_owned()
            } else {
                format!("{prefix}/{child}")
            };
            portable_path(&path, limits)?;
            if is_dir {
                tree.dirs.insert(path.clone());
                self.walk(node, &path, tree, seen, depth + 1, limits)?;
            } else {
                if !seen.insert(node) {
                    return Err(bad("file node listed twice"));
                }
                tree.files.insert(path, node as usize - self.dirs.len());
            }
        }
        Ok(())
    }

    /// Declared ExeFS/RomFS, optional `data/`, and separate `programN` views.
    /// Each nested executable is validated by the extracted-tree importer.
    fn package_tree(&self, p: &Package, limits: &ImportLimits) -> Result<Tree, WriterError> {
        let root = self
            .dirs
            .get(p.root as usize)
            .ok_or_else(|| bad("package root out of range"))?;
        // Extra content is allowed only in the canonical optional data tree.
        let end = root.first_child as u64 + root.child_count as u64;
        if end > self.children.len() as u64 {
            return Err(bad("root children out of bounds"));
        }
        let mut root_seen = BTreeSet::new();
        let mut data = None;
        let mut children = Vec::new();
        for &node in &self.children[root.first_child as usize..end as usize] {
            if !root_seen.insert(node) {
                return Err(bad("duplicate package root entry"));
            }
            if node != p.exefs && node != p.romfs {
                if let Some(dir) = self.dirs.get(node as usize) {
                    if dir.name == "data" && data.replace(node).is_none() {
                        continue;
                    }
                    if child_program_name(&dir.name) {
                        children.push((node, dir.name.clone()));
                        continue;
                    }
                }
                let name = if let Some(dir) = self.dirs.get(node as usize) {
                    format!("directory {:?}", dir.name)
                } else if let Some(file) = self.files.get(node as usize - self.dirs.len()) {
                    format!("file {:?}", file.name)
                } else {
                    format!("invalid node {node}")
                };
                return Err(unsupported(&format!(
                    "NUP package #{} ({:016x}) has unsupported root entry {name}; expected exefs, romfs, data or program1..program15",
                    p.ordinal, p.title_id
                )));
            }
        }
        if p.kind == 2 && !children.is_empty() {
            return Err(unsupported(
                "executable Program directories in DLC are unsupported",
            ));
        }
        let mut tree = Tree::default();
        let mut seen = BTreeSet::new();
        for (dir, logical) in [
            (p.exefs, "exefs"),
            (p.romfs, "romfs"),
            (data.unwrap_or(NONE), "data"),
        ] {
            if dir == NONE {
                continue;
            }
            if dir as usize >= self.dirs.len() || dir == p.root {
                return Err(bad("package content directory index is invalid"));
            }
            tree.dirs.insert(logical.into());
            self.walk(dir, logical, &mut tree, &mut seen, 1, limits)?;
        }
        for (dir, name) in children {
            if !tree.dirs.insert(name.clone()) {
                return Err(bad("duplicate child Program directory"));
            }
            self.walk(dir, &name, &mut tree, &mut seen, 1, limits)?;
        }
        Ok(tree)
    }
}

/// Native update resolution: the update ExeFS replaces the base ExeFS when
/// present; RomFS paths of the update shadow base paths.
fn overlay(base: &Tree, update: &Tree) -> Result<Tree, WriterError> {
    // Replace each Program's whole ExeFS independently: removed modules must
    // not leak in from its base, and sibling Programs must remain untouched.
    let replaced: Vec<_> = update
        .dirs
        .iter()
        .filter(|path| {
            path.as_str() == "exefs"
                || nested_program_file(path).is_some_and(|(_, relative)| relative == "exefs")
        })
        .collect();
    let keep = |path: &str| {
        !replaced.iter().any(|root| {
            path == root.as_str()
                || path
                    .strip_prefix(root.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let mut out = Tree::default();
    out.dirs
        .extend(base.dirs.iter().filter(|p| keep(p)).cloned());
    for (path, index) in &base.files {
        if keep(path) {
            out.files.insert(path.clone(), *index | BASE_BIT);
        }
    }
    for dir in &update.dirs {
        if out.files.contains_key(dir) {
            return Err(bad("update directory shadows a base file"));
        }
        out.dirs.insert(dir.clone());
    }
    for (path, index) in &update.files {
        if out.dirs.contains(path) {
            return Err(bad("update file shadows a base directory"));
        }
        out.files.insert(path.clone(), *index);
    }
    Ok(out)
}

/// A Switch 2 Program may reuse the legacy RomFS, but sibling executable
/// Programs from that NX base are not implicitly Ounce Programs.
fn overlay_upgrade(base: &Tree, update: &Tree, upgraded: bool) -> Result<Tree, WriterError> {
    if !upgraded {
        return overlay(base, update);
    }
    let inherited = Tree {
        files: base
            .files
            .iter()
            .filter(|(path, _)| nested_program_file(path).is_none())
            .map(|(path, index)| (path.clone(), *index))
            .collect(),
        dirs: base
            .dirs
            .iter()
            .filter(|path| !child_program_name(path.split('/').next().unwrap_or("")))
            .cloned()
            .collect(),
    };
    overlay(&inherited, update)
}

/// Select the legacy update whose materialized file view the edition inherits.
fn edition_legacy_parent<'a>(
    toc: &'a Toc,
    p: &Package,
    plan: &NupUpgradePlan,
) -> Option<&'a Package> {
    toc.packages
        .iter()
        .filter(|candidate| {
            candidate.kind == 1
                && !plan.includes(candidate)
                && candidate.version <= p.version
                && package_role(candidate, true)
                    .is_ok_and(|(_, owner)| owner == plan.legacy_application)
        })
        .max_by_key(|candidate| candidate.version)
}

/// Resolve an Ounce upgrade against the latest applicable legacy update, not
/// just the original v0 base. The base/update bit cannot represent an inherited
/// file from a third NUP package, so the upgraded view references its original
/// TOC extent directly. Stream mode still avoids staging the RomFS bytes.
fn resolve_nup_update(
    toc: &Toc,
    p: &Package,
    base: &Tree,
    own: &Tree,
    upgrade: Option<&NupUpgradePlan>,
    limits: &ImportLimits,
) -> Result<Tree, WriterError> {
    let Some(plan) = upgrade.filter(|plan| plan.includes(p)) else {
        return overlay(base, own);
    };
    let Some(legacy) = edition_legacy_parent(toc, p, plan) else {
        return overlay_upgrade(base, own, true);
    };
    let legacy_view = overlay(base, &toc.package_tree(legacy, limits)?)?;
    let mut upgraded = overlay_upgrade(&legacy_view, own, true)?;
    for index in upgraded.files.values_mut() {
        *index &= !BASE_BIT;
    }
    Ok(upgraded)
}
/// Marks a file inherited from the base tree, to be hard-linked.
const BASE_BIT: usize = 1 << (usize::BITS - 1);

#[derive(Default)]
struct Meta {
    nacp: Option<Vec<u8>>,
    icon: Option<Vec<u8>>,
    localized_icons: Vec<(String, Vec<u8>)>,
    title: Option<String>,
    publisher: Option<String>,
    display_version: Option<String>,
    localized_titles: Vec<(String, String)>,
}

/// Records of a localized TLV: u8 tag length, tag, u32 data length, data.
fn localized_records(body: &[u8], what: &str) -> Result<Vec<(String, Vec<u8>)>, WriterError> {
    let mut out = Vec::new();
    let mut tags = BTreeSet::new();
    let mut i = 0;
    while i < body.len() {
        let n = body[i] as usize;
        let tag = body
            .get(i + 1..i + 1 + n)
            .and_then(|t| std::str::from_utf8(t).ok())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| bad(&format!("localized {what} tag is invalid")))?;
        let len = body
            .get(i + 1 + n..i + 5 + n)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
            .ok_or_else(|| bad(&format!("localized {what} is truncated")))?;
        let data = body
            .get(i + 5 + n..i + 5 + n + len)
            .ok_or_else(|| bad(&format!("localized {what} is truncated")))?;
        if !tags.insert(tag.to_owned()) {
            return Err(bad(&format!("duplicate localized {what} tag")));
        }
        out.push((tag.to_owned(), data.to_vec()));
        i += 5 + n + len;
    }
    Ok(out)
}

/// The native BCP 47 tag of a NUP language tag, if it names a NACP language.
fn native_tag(tag: &str) -> Option<&'static str> {
    let tag = tag.replace('_', "-");
    NACP_LANGUAGE_MAP
        .iter()
        .find(|(_, t)| t.eq_ignore_ascii_case(&tag))
        .map(|(_, t)| *t)
}

impl Meta {
    /// Title, publisher and icons for a package that carries no NACP.
    fn package_metadata(&self) -> Option<super::PackageMetadata> {
        let mut localized: BTreeMap<&'static str, (Option<String>, Option<Vec<u8>>)> =
            BTreeMap::new();
        for (tag, title) in &self.localized_titles {
            if let Some(tag) = native_tag(tag) {
                localized.entry(tag).or_default().0 = Some(title.clone());
            }
        }
        for (tag, icon) in &self.localized_icons {
            if let Some(tag) = native_tag(tag) {
                localized.entry(tag).or_default().1 = Some(icon.clone());
            }
        }
        let m = super::PackageMetadata {
            title: self.title.clone(),
            publisher: self.publisher.clone(),
            display_version: self.display_version.clone(),
            icon: self.icon.clone(),
            localized: localized
                .into_iter()
                .map(|(tag, (title, icon))| (tag.to_owned(), title, icon))
                .collect(),
        };
        (m.title.is_some()
            || m.icon.is_some()
            || m.display_version.is_some()
            || !m.localized.is_empty())
        .then_some(m)
    }
}

fn read_meta(file: &mut File, p: &Package, physical: u64) -> Result<Meta, WriterError> {
    let meta = Meta::default();
    if p.meta_offset == 0 && p.meta_size == 0 {
        return Ok(meta);
    }
    if p.meta_offset
        .checked_add(p.meta_size as u64)
        .map_or(true, |e| e > physical)
        || p.meta_size > 64 * 1024 * 1024
    {
        return Err(bad("meta block is out of bounds"));
    }
    let mut block = vec![0; p.meta_size as usize];
    file.seek(SeekFrom::Start(p.meta_offset))?;
    file.read_exact(&mut block)?;
    parse_meta(&block, meta)
}

/// Meta TLV records as stored: (type, payload).
fn meta_records(block: &[u8]) -> Result<Vec<(u16, &[u8])>, WriterError> {
    let mut records = Vec::new();
    let mut at = 0usize;
    while at + 8 <= block.len() {
        let kind = u16_at(block, at);
        let size = u32_at(block, at + 4) as usize;
        let body = block
            .get(at + 8..at + 8 + size)
            .ok_or_else(|| bad("meta record overruns its block"))?;
        records.push((kind, body));
        at += 8 + size;
    }
    Ok(records)
}

/// Upstream nup PR #20 wrote every meta TLV type one too low (NPDM = 0).
/// Same rule as nup PR #32: a type 0 record, or a first 0x0007 record that
/// is not the 32-byte build ID, marks such a block. Types up to 0x0009
/// are then read one higher.
fn shifted_meta(records: &[(u16, &[u8])]) -> bool {
    records.iter().any(|&(kind, _)| kind == 0)
        || records
            .iter()
            .find(|&&(kind, _)| kind == 0x0007)
            .is_some_and(|&(_, body)| body.len() != 32)
}

fn parse_meta(block: &[u8], mut meta: Meta) -> Result<Meta, WriterError> {
    let records = meta_records(block)?;
    let shifted = shifted_meta(&records);
    for (kind, body) in records {
        let kind = if shifted && kind <= 0x0009 {
            kind + 1
        } else {
            kind
        };
        match kind {
            0x0002 if body.len() == 0x4000 => meta.nacp = Some(body.to_vec()),
            0x0002 => return Err(bad("NACP record is not 0x4000 bytes")),
            0x0003 => meta.icon = Some(body.to_vec()),
            0x0004 => meta.display_version = Some(String::from_utf8_lossy(body).into_owned()),
            0x0005 => meta.title = Some(String::from_utf8_lossy(body).into_owned()),
            0x0006 => meta.publisher = Some(String::from_utf8_lossy(body).into_owned()),
            0x0009 => {
                meta.localized_titles = localized_records(body, "title")?
                    .into_iter()
                    .map(|(tag, data)| (tag, String::from_utf8_lossy(&data).into_owned()))
                    .collect();
            }
            0x000a => meta.localized_icons = localized_records(body, "icon")?,
            _ => {}
        }
    }
    Ok(meta)
}

/// NPDM key profile from the ACID magic position (0x200 or 0x300).
fn npdm_profile(npdm: &[u8]) -> &'static str {
    if npdm.len() >= 0x80 && &npdm[..4] == b"META" {
        let acid = u32_at(npdm, 0x78) as usize;
        for (at, profile) in [(0x200, "meta-acid2048-v1"), (0x300, "meta-acid3072-v1")] {
            if npdm.get(acid + at..acid + at + 4) == Some(b"ACID") {
                return profile;
            }
        }
    }
    "none"
}

/// The extra executable views used by multi-Program extracted packages.
/// A directory name is only a locator; the Program ID comes from its NPDM.
fn child_program_name(name: &str) -> bool {
    let Some(index) = name
        .strip_prefix("program")
        .and_then(|s| s.parse::<u8>().ok())
    else {
        return false;
    };
    (1..=15).contains(&index) && name == format!("program{index}")
}
fn nested_program_file(path: &str) -> Option<(&str, &str)> {
    let (name, relative) = path.split_once('/')?;
    child_program_name(name).then_some((name, relative))
}
fn executable_file(path: &str) -> bool {
    path.starts_with("exefs/")
        || nested_program_file(path).is_some_and(|(_, relative)| relative.starts_with("exefs/"))
}

fn package_role(p: &Package, has_base: bool) -> Result<(crate::SnapshotType, u64), WriterError> {
    use crate::SnapshotType;
    match (p.kind, p.flags) {
        (0, 0) => Ok((SnapshotType::Base, p.title_id)),
        (1, 0 | 1) => {
            let suffix = p.title_id & 0xfff;
            if suffix != 0 && !(0x800..=0x80f).contains(&suffix) {
                return Err(bad(&format!(
                    "update title ID {:016x} has an invalid patch/program index",
                    p.title_id
                )));
            }
            if suffix > 0x800 && p.title_id >> 56 != 4 {
                return Err(bad(
                    "indexed patch title IDs require a native Switch 2 application",
                ));
            }
            let standalone = p.flags == 1;
            if standalone == has_base {
                return Err(unsupported(if standalone {
                    "standalone update beside a base package"
                } else {
                    "update without a base package and without the standalone flag"
                }));
            }
            let kind = if standalone {
                SnapshotType::StandaloneUpdate
            } else {
                SnapshotType::Update
            };
            Ok((kind, p.title_id & !0xfff))
        }
        (2, 0) => {
            let application = (p.title_id & !0xfff)
                .checked_sub(0x1000)
                .filter(|_| p.title_id & 0xfff != 0)
                .ok_or_else(|| bad("add-on title ID has no add-on index"))?;
            Ok((SnapshotType::Dlc, application))
        }
        (0..=2, _) => Err(bad("unknown package flags")),
        _ => Err(unsupported("reserved NUP package kind")),
    }
}

/// Report which actual package caused a cross-application rejection. Reading
/// its NPDM is bounded to 1 MiB and happens only on the error path.
fn package_application_error(
    path: &Path,
    toc: &Toc,
    p: &Package,
    observed: u64,
    expected: u64,
    limits: &ImportLimits,
    token: &crate::CancellationToken,
) -> WriterError {
    let mut reason = format!(
        "package #{} ({} title ID {:016x}, version {}, flags {}) belongs to application {:016x}, expected {:016x}",
        p.ordinal,
        package_kind(p.kind),
        p.title_id,
        p.version,
        p.flags,
        observed,
        expected
    );
    match nup_program_id(path, toc, p, limits, token) {
        Ok(Some((program, _))) => reason.push_str(&format!("; NPDM Program ID {program:016x}")),
        Ok(None) => {}
        Err(error) => reason.push_str(&format!("; NPDM unavailable: {error}")),
    }
    bad(&reason)
}

/// Switch 2 can launch an Ounce Program belonging to an NX application. The
/// upper ID byte changes from 01 to 04; a Switch 2 Edition's low 12 bits are
/// the upgrade add-on index. This is a native Program relationship, not a
/// license to merge unrelated NUP application IDs.
fn ounce_application(nx_application: u64) -> Option<u64> {
    ((nx_application >> 56) == 1 && nx_application & 0xfff == 0)
        .then_some((nx_application & 0x00ff_ffff_ffff_ffff) | 0x0400_0000_0000_0000)
}

fn ounce_program_of(nx_program: u64, ounce_program: u64) -> bool {
    ounce_application(nx_program)
        .is_some_and(|converted| (converted & !0xfff) == (ounce_program & !0xfff))
}

#[derive(Clone, Debug)]
struct NupUpgradePlan {
    legacy_application: u64,
    /// The authoritative Program ID read from Switch 2 NPDM, not a TOC guess.
    ounce_program: u64,
    /// Exact TOC record positions, since legacy and Ounce packages can share
    /// a title ID and numeric version without sharing a native Program.
    packages: BTreeSet<usize>,
}

impl NupUpgradePlan {
    fn includes(&self, package: &Package) -> bool {
        self.packages.contains(&package.ordinal)
    }
}

fn nup_program_id(
    path: &Path,
    toc: &Toc,
    package: &Package,
    limits: &ImportLimits,
    token: &crate::CancellationToken,
) -> Result<Option<(u64, super::structures::NpdmProfile)>, WriterError> {
    if package.exefs == NONE {
        return Ok(None);
    }
    let mut tree = Tree::default();
    toc.walk(
        package.exefs,
        "",
        &mut tree,
        &mut BTreeSet::new(),
        1,
        limits,
    )?;
    let Some(&index) = tree.files.get("main.npdm") else {
        return Ok(None);
    };
    let file = &toc.files[index];
    if file.size > 1024 * 1024 {
        return Err(limit("NUP NPDM exceeds 1 MiB"));
    }
    let mut bytes = Vec::new();
    NupExtent {
        path: std::sync::Arc::new(path.to_owned()),
        name: file.name.clone(),
        offset: file.extent_offset,
        stored: file.extent_size,
        codec: file.codec,
        size: file.size,
        sha256: file.sha256,
    }
    .reader(token)?
    .read_to_end(&mut bytes)?;
    let profile = match npdm_profile(&bytes) {
        "meta-acid2048-v1" => super::structures::NpdmProfile::MetaAcid2048V1,
        "meta-acid3072-v1" => super::structures::NpdmProfile::MetaAcid3072V1,
        _ => return Ok(None),
    };
    let program = super::structures::validate_npdm(&bytes, profile)?.program_id;
    // An explicit patch index is authoritative. Unindexed 0x800 packages
    // remain compatible with legacy-to-Ounce transitions evidenced by NPDM.
    if package.kind == 1 && (package.title_id & 0xfff) > 0x800 {
        let expected = (package.title_id & !0xfff) | (package.title_id & 0xf);
        if program != expected || profile != super::structures::NpdmProfile::MetaAcid3072V1 {
            return Err(bad(&format!(
                "indexed update package #{} ({:016x}) NPDM Program {:016x} does not match declared Program {:016x}",
                package.ordinal, package.title_id, program, expected
            )));
        }
    }
    Ok(Some((program, profile)))
}

/// Detect only an evidenced NX -> Ounce Program transition. A matching TOC
/// title alone is insufficient; the executable NPDM must prove the Program ID.
fn nup_upgrade_plan(
    path: &Path,
    toc: &Toc,
    limits: &ImportLimits,
    token: &crate::CancellationToken,
) -> Result<Option<NupUpgradePlan>, WriterError> {
    let base = toc.packages.iter().find(|p| p.kind == 0);
    let base_application = if let Some(base) = base {
        let Some(ounce_app) = ounce_application(base.title_id) else {
            return Ok(None);
        };
        let Some((base_program, _)) = nup_program_id(path, toc, base, limits, token)? else {
            return Ok(None);
        };
        if base_program != base.title_id {
            return Ok(None);
        }
        Some((base.title_id, ounce_app))
    } else {
        None
    };
    // Native Ounce bases are handled by their own Program-family association.
    // A matching legacy DLC must not turn their ordinary indexed updates into
    // standalone NX -> Ounce upgrades.
    if base.is_some() && base_application.is_none() {
        return Ok(None);
    }
    let mut upgrade = None;
    let mut legacy_application = None;
    let mut packages = BTreeSet::new();
    for p in toc.packages.iter().filter(|p| p.kind == 1) {
        cancelled(token)?;
        let (_, declared_application) = package_role(p, base.is_some())?;
        let observed = nup_program_id(path, toc, p, limits, token)?;
        let Some((program, profile)) = observed else {
            if base_application.is_some_and(|(_, ounce)| declared_application == ounce) {
                return Err(unsupported(
                    "Switch 2 update requires an identifiable NPDM Program",
                ));
            }
            continue;
        };
        let candidate_application = if let Some((legacy, ounce)) = base_application {
            if declared_application != legacy && declared_application != ounce {
                continue;
            }
            if !ounce_program_of(legacy, program) {
                if declared_application == ounce {
                    return Err(bad(
                        "Switch 2 update NPDM does not match the NX base Program family",
                    ));
                }
                continue;
            }
            legacy
        } else {
            if profile != super::structures::NpdmProfile::MetaAcid3072V1
                || program >> 56 != 4
                || program & 0xfff == 0
            {
                continue;
            }
            let legacy = (program & 0x00ff_ffff_ffff_f000) | 0x0100_0000_0000_0000;
            if declared_application != legacy
                && ounce_application(legacy) != Some(declared_application)
            {
                continue;
            }
            let expected_dlc = legacy.wrapping_add(0x1000 | (program & 0xfff));
            if !toc
                .packages
                .iter()
                .any(|addon| addon.kind == 2 && addon.title_id == expected_dlc)
            {
                continue;
            }
            legacy
        };
        if profile != super::structures::NpdmProfile::MetaAcid3072V1 {
            if base_application.is_some() {
                return Err(bad(
                    "Switch 2 update NPDM does not match the NX base Program family",
                ));
            }
            continue;
        }
        if legacy_application.is_some_and(|previous| previous != candidate_application) {
            return Err(unsupported(
                "multiple independent Switch 2 applications require separate imports",
            ));
        }
        legacy_application = Some(candidate_application);
        if let Some(previous) = upgrade {
            if previous != program {
                return Err(unsupported(
                    "multiple Switch 2 upgrade Program IDs need explicit association",
                ));
            }
        }
        upgrade = Some(program);
        packages.insert(p.ordinal);
    }
    Ok(upgrade.map(|ounce_program| NupUpgradePlan {
        legacy_application: legacy_application.unwrap(),
        ounce_program,
        packages,
    }))
}

/// Some real Switch 2 NUPs contain one `base` TOC record per native Program,
/// rather than one base record with nested `programN` directories. Accept this
/// only when every base has a verified Ounce NPDM in the *same* Program family.
/// This is an import association; it does not change the NX on-disk format.
#[derive(Debug)]
struct NupMultiBasePlan {
    application: u64,
    primary_program: u64,
    ounce_application: u64,
    programs: BTreeMap<u64, usize>,
}

impl NupMultiBasePlan {
    fn program(&self, p: &Package) -> Option<u64> {
        match p.kind {
            0 => self
                .programs
                .contains_key(&p.title_id)
                .then_some(p.title_id),
            1 => {
                let suffix = p.title_id & 0xfff;
                if p.title_id & !0xfff != self.ounce_application
                    || (suffix != 0 && !(0x800..=0x80f).contains(&suffix))
                {
                    return None;
                }
                let program = self.ounce_application | (suffix & 0xf);
                self.programs.contains_key(&program).then_some(program)
            }
            _ => None,
        }
    }

    fn owns(&self, p: &Package) -> bool {
        if p.kind != 2 {
            return self.program(p).is_some();
        }
        if p.flags != 0 || p.title_id & 0xfff == 0 {
            return false;
        }
        let Some(owner) = (p.title_id & !0xfff).checked_sub(0x1000) else {
            return false;
        };
        owner == self.ounce_application
            || owner == ((self.ounce_application & 0x00ff_ffff_ffff_ffff) | 0x0100_0000_0000_0000)
    }
}

fn nup_multi_base_plan(
    path: &Path,
    toc: &Toc,
    limits: &ImportLimits,
    token: &crate::CancellationToken,
) -> Result<Option<NupMultiBasePlan>, WriterError> {
    let bases: Vec<_> = toc.packages.iter().filter(|p| p.kind == 0).collect();
    if bases.len() <= 1 {
        return Ok(None);
    }
    let family = bases[0].title_id & !0xfff;
    if family >> 56 != 4 {
        return Err(unsupported(
            "multiple base NUP packages require one verified Switch 2 Program family",
        ));
    }
    let mut programs = BTreeMap::new();
    for p in bases {
        cancelled(token)?;
        if p.flags != 0 || p.title_id & !0xfff != family || p.title_id & 0xfff > 15 {
            return Err(bad(&format!(
                "base package #{} ({:016x}) is not a Program in the same Switch 2 application",
                p.ordinal, p.title_id
            )));
        }
        let Some((program, profile)) = nup_program_id(path, toc, p, limits, token)? else {
            return Err(unsupported(&format!(
                "base package #{} ({:016x}) needs a validated exefs/main.npdm",
                p.ordinal, p.title_id
            )));
        };
        if profile != super::structures::NpdmProfile::MetaAcid3072V1 || program != p.title_id {
            return Err(bad(&format!(
                "base package #{} ({:016x}) NPDM Program ID does not match its TOC title ID",
                p.ordinal, p.title_id
            )));
        }
        if programs.insert(program, p.ordinal).is_some() {
            return Err(bad("duplicate Switch 2 base Program ID"));
        }
    }
    if !programs.contains_key(&family) {
        return Err(unsupported("multi-base NUP has no Program index 0 anchor"));
    }
    let legacy = (family & 0x00ff_ffff_ffff_ffff) | 0x0100_0000_0000_0000;
    let mut has_legacy_addon = false;
    let mut has_native_addon = false;
    let mut upgrade_index = None;
    let provisional = NupMultiBasePlan {
        application: family,
        primary_program: family,
        ounce_application: family,
        programs,
    };
    for p in &toc.packages {
        cancelled(token)?;
        if !provisional.owns(p) {
            return Err(bad(&format!(
                "NUP package #{} ({:016x}) belongs to another application or has no matching base Program",
                p.ordinal, p.title_id
            )));
        }
        if p.kind == 1 {
            if p.flags != 0 {
                return Err(unsupported(
                    "standalone update beside multiple base Programs",
                ));
            }
            if let Some((program, profile)) = nup_program_id(path, toc, p, limits, token)? {
                if profile != super::structures::NpdmProfile::MetaAcid3072V1
                    || Some(program) != provisional.program(p)
                {
                    return Err(bad(&format!(
                        "update package #{} ({:016x}) NPDM does not match its Switch 2 Program index",
                        p.ordinal, p.title_id
                    )));
                }
            }
        }
        if p.kind == 2 && (p.title_id & !0xfff).checked_sub(0x1000) == Some(family) {
            has_native_addon = true;
        }
        if p.kind == 2 && (p.title_id & !0xfff).checked_sub(0x1000) == Some(legacy) {
            has_legacy_addon = true;
            let index = p.title_id & 0xfff;
            if (1..=15).contains(&index) {
                let program = family | index;
                if !provisional.programs.contains_key(&program) {
                    return Err(bad(&format!(
                        "Switch 2 upgrade DLC {:016x} references absent Program index {index}",
                        p.title_id
                    )));
                }
                if upgrade_index
                    .replace(program)
                    .is_some_and(|previous| previous != program)
                {
                    return Err(unsupported(
                        "ambiguous Switch 2 upgrade DLC Program indices",
                    ));
                }
            }
        }
    }
    if has_legacy_addon && has_native_addon {
        return Err(unsupported(
            "mixed legacy and native Switch 2 DLC namespaces require explicit associations",
        ));
    }
    Ok(Some(NupMultiBasePlan {
        application: if has_legacy_addon { legacy } else { family },
        primary_program: upgrade_index.unwrap_or(family),
        ..provisional
    }))
}

/// An indexed update beside a base must resolve to that exact Program. A
/// matching family ID alone cannot make Program 1 an update of Program 0.
/// Standalone updates are resolved independently by their own NPDM.
fn validate_indexed_updates(
    path: &Path,
    toc: &Toc,
    multi: Option<&NupMultiBasePlan>,
    upgrade: Option<&NupUpgradePlan>,
    limits: &ImportLimits,
    token: &crate::CancellationToken,
) -> Result<(), WriterError> {
    for p in toc
        .packages
        .iter()
        .filter(|p| p.kind == 1 && (p.title_id & 0xfff) > 0x800)
    {
        cancelled(token)?;
        if p.flags == 1 {
            // With no base to inherit, a standalone indexed update must
            // carry its own structurally validated Program identity.
            if nup_program_id(path, toc, p, limits, token)?.is_none() {
                return Err(bad(&format!(
                    "standalone indexed update package #{} ({:016x}) requires a validated exefs/main.npdm",
                    p.ordinal, p.title_id
                )));
            }
            continue;
        }
        if multi.is_some_and(|plan| plan.program(p).is_some())
            || upgrade.is_some_and(|plan| plan.includes(p))
        {
            continue;
        }
        return Err(bad(&format!(
            "indexed Switch 2 update package #{} ({:016x}) has no verified matching base Program",
            p.ordinal, p.title_id
        )));
    }
    Ok(())
}

pub struct NupPackageInfo {
    pub kind: crate::SnapshotType,
    pub version: u32,
    pub display_version: Option<String>,
}

pub struct NupInfo {
    pub application_id: u64,
    pub platform: crate::Platform,
    pub size: u64,
    pub title: Option<String>,
    pub publisher: Option<String>,
    pub icon: Option<Vec<u8>>,
    pub packages: Vec<NupPackageInfo>,
}

/// Inspect the TOC and display metadata without staging or decoding game data.
/// Only an optional main.npdm is decoded for platform detection.
pub fn inspect_nup(
    path: &Path,
    limits: &ImportLimits,
    token: &crate::CancellationToken,
) -> Result<NupInfo, WriterError> {
    use crate::{Platform, SnapshotType};
    limits.validate()?;
    cancelled(token)?;
    let mut file = File::open(path)?;
    let (toc, size) = read_toc(&mut file, limits)?;
    if toc.packages.len() > limits.max_snapshots {
        return Err(limit("NUP snapshot budget"));
    }
    let mut bases = toc.packages.iter().filter(|p| p.kind == 0 && p.flags == 0);
    let base = bases.next();
    let multi = nup_multi_base_plan(path, &toc, limits, token)?;
    let main = base
        .or_else(|| {
            toc.packages
                .iter()
                .filter(|p| p.kind == 1 && p.flags == 1)
                .max_by_key(|p| p.version)
        })
        .ok_or_else(|| bad("no base or standalone update package to convert"))?;
    let upgrade = nup_upgrade_plan(path, &toc, limits, token)?;
    validate_indexed_updates(path, &toc, multi.as_ref(), upgrade.as_ref(), limits, token)?;
    let (_, observed_application) = package_role(main, base.is_some())?;
    let application_id = multi
        .as_ref()
        .map(|plan| plan.application)
        .or_else(|| upgrade.as_ref().map(|plan| plan.legacy_application))
        .unwrap_or(observed_application);
    let platform = if multi.is_some() || upgrade.is_some() || application_id >> 56 == 0x04 {
        Platform::Switch2
    } else if application_id >> 56 == 0x01
        && nup_program_id(path, &toc, main, limits, token)?
            .is_some_and(|(_, profile)| profile == super::structures::NpdmProfile::MetaAcid2048V1)
    {
        Platform::Switch1
    } else {
        Platform::Unknown
    };
    let mut info = NupInfo {
        application_id,
        platform,
        size,
        title: None,
        publisher: None,
        icon: None,
        packages: Vec::new(),
    };
    let mut ordered: Vec<_> = toc.packages.iter().collect();
    ordered.sort_by_key(|p| (p.kind, p.version));
    let mut metadata_bytes = 0u64;
    for p in ordered {
        cancelled(token)?;
        let (mut kind, owner) = package_role(p, base.is_some())?;
        if upgrade.as_ref().is_some_and(|plan| plan.includes(p)) {
            kind = SnapshotType::StandaloneUpdate;
        } else if multi.as_ref().is_some_and(|plan| plan.owns(p)) {
            // Several NUP base Programs and their legacy upgrade DLC may have
            // different title ID prefixes but share one verified application.
        } else if owner != application_id {
            return Err(package_application_error(
                path,
                &toc,
                p,
                owner,
                application_id,
                limits,
                token,
            ));
        }
        let display_version = if kind != SnapshotType::Dlc {
            metadata_bytes += u64::from(p.meta_size);
            if metadata_bytes > limits.max_inventory_bytes {
                return Err(limit("NUP display metadata exceeds the inspection budget"));
            }
            let mut meta = read_meta(&mut file, p, size)?;
            if let Some(nacp) = &meta.nacp {
                let (titles, version, _) = crate::native::parse_nacp_text(nacp);
                if let Some(title) = titles.first() {
                    meta.title = title.name.clone().or(meta.title);
                    meta.publisher = title.publisher.clone().or(meta.publisher);
                }
                meta.display_version = version.or(meta.display_version);
            }
            let title = meta.title.or_else(|| {
                meta.localized_titles
                    .into_iter()
                    .next()
                    .map(|(_, title)| title)
            });
            if title.as_ref().is_some_and(|title| !title.trim().is_empty()) {
                info.title = title;
            }
            if meta
                .publisher
                .as_ref()
                .is_some_and(|publisher| !publisher.trim().is_empty())
            {
                info.publisher = meta.publisher;
            }
            // Prefer the base artwork so the editor can restore it after a custom selection.
            if info.icon.is_none() {
                info.icon = meta
                    .icon
                    .or_else(|| {
                        meta.localized_icons
                            .into_iter()
                            .next()
                            .map(|(_, icon)| icon)
                    })
                    .filter(|icon| icon.len() <= 16 * 1024 * 1024);
            }
            meta.display_version
        } else {
            None
        };
        info.packages.push(NupPackageInfo {
            kind,
            version: p.version,
            display_version,
        });
    }
    Ok(info)
}

/// Decode one extent into `out`, checking size and SHA-256.
fn decode(
    source: &mut File,
    f: &NupFile,
    out: &Path,
    buffer: &mut [u8],
    token: &crate::CancellationToken,
    budget: &mut StageBudget,
    progress: &crate::ProgressCounter,
) -> Result<(), WriterError> {
    let mut target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)?;
    source.seek(SeekFrom::Start(f.extent_offset))?;
    let stored = (&mut *source).take(f.extent_size);
    let mut reader: Box<dyn Read + '_> = if f.codec == 1 && f.size != 0 {
        Box::new(zstd::stream::read::Decoder::new(stored)?)
    } else {
        Box::new(stored)
    };
    let mut hash = Sha256::new();
    let mut written = 0u64;
    loop {
        cancelled(token)?;
        let n = match reader.read(buffer) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(bad(&format!("{}: decode failed: {e}", f.name))),
        };
        if n == 0 {
            break;
        }
        written = written
            .checked_add(n as u64)
            .ok_or_else(|| limit("NUP decoded size overflow"))?;
        progress.add_done(n as u64);
        if written > f.size {
            return Err(bad(&format!("{}: decodes past its size", f.name)));
        }
        hash.update(&buffer[..n]);
        budget.charge(n as u64)?;
        target.write_all(&buffer[..n])?;
    }
    if written != f.size || hash.finish() != f.sha256 {
        return Err(bad(&format!("{}: size or SHA-256 mismatch", f.name)));
    }
    target.flush()?;
    Ok(())
}

fn hex16(v: u64) -> String {
    format!("{v:016x}")
}

struct StageBudget {
    used: u64,
    limit: u64,
}
impl StageBudget {
    fn charge(&mut self, bytes: u64) -> Result<(), WriterError> {
        self.used = self
            .used
            .checked_add(bytes)
            .filter(|&n| n <= self.limit)
            .ok_or_else(|| limit("NUP staging exceeds temporary disk budget"))?;
        Ok(())
    }
    fn write(&mut self, path: &Path, bytes: &[u8]) -> Result<(), WriterError> {
        self.charge(bytes.len() as u64)?;
        fs::write(path, bytes)?;
        Ok(())
    }
}

fn copy_inherited(
    from: &Path,
    out: &Path,
    size: u64,
    buffer: &mut [u8],
    token: &crate::CancellationToken,
    budget: &mut StageBudget,
) -> Result<(), WriterError> {
    let mut source = File::open(from)?;
    let mut target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)?;
    let mut remaining = size;
    while remaining != 0 {
        cancelled(token)?;
        let n = remaining.min(buffer.len() as u64) as usize;
        source.read_exact(&mut buffer[..n])?;
        budget.charge(n as u64)?;
        target.write_all(&buffer[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

/// Decode `nup` under `parent`, bounded by `max_total_bytes`. Conversion
/// callers should use `stage_nup_with_options` to include their writer budget.
pub fn stage_nup(
    nup: &Path,
    parent: &Path,
    nup_options: &NupOptions,
    limits: &ImportLimits,
    token: &crate::CancellationToken,
) -> Result<NupStage, WriterError> {
    stage_nup_bounded(
        nup,
        parent,
        nup_options,
        limits,
        token,
        limits.max_total_bytes,
        MAX_RESOLVED_ENTRIES,
    )
}

/// Decode under both the import and writer temporary-disk limits.
pub fn stage_nup_with_options(
    nup: &Path,
    parent: &Path,
    nup_options: &NupOptions,
    limits: &ImportLimits,
    options: &WriterOptions,
) -> Result<NupStage, WriterError> {
    stage_nup_bounded(
        nup,
        parent,
        nup_options,
        limits,
        &options.cancellation,
        options.max_temporary_disk_bytes,
        MAX_RESOLVED_ENTRIES,
    )
}

fn stage_nup_bounded(
    nup: &Path,
    parent: &Path,
    nup_options: &NupOptions,
    limits: &ImportLimits,
    token: &crate::CancellationToken,
    max_staging_bytes: u64,
    max_entries: usize,
) -> Result<NupStage, WriterError> {
    let platform = match nup_options.platform {
        Some(crate::Platform::Switch1) => "switch1",
        Some(crate::Platform::Switch2) => "switch2",
        _ => {
            return Err(invalid(
                "NUP import requires an explicit Switch 1 or Switch 2 platform",
            ))
        }
    };
    limits.validate()?;
    cancelled(token)?;
    let mut source = File::open(nup)?;
    let (toc, physical) = read_toc(&mut source, limits)?;
    // Bound all package-level NPDM inspection before resolving associations.
    if toc.packages.len() > limits.max_snapshots {
        return Err(limit("NUP package count exceeds the snapshot budget"));
    }
    let upgrade = nup_upgrade_plan(nup, &toc, limits, token)?;
    let multi = nup_multi_base_plan(nup, &toc, limits, token)?;
    validate_indexed_updates(nup, &toc, multi.as_ref(), upgrade.as_ref(), limits, token)?;
    let has_base = toc.packages.iter().any(|p| p.kind == 0);
    let mut owner = None;
    for p in &toc.packages {
        if p.kind == 2 && nup_options.skip_addons {
            continue;
        }
        let (_, declared) = package_role(p, has_base)?;
        let application = if let Some(plan) = multi.as_ref() {
            plan.application
        } else {
            upgrade
                .as_ref()
                .filter(|plan| plan.includes(p))
                .map_or(declared, |plan| plan.legacy_application)
        };
        if let Some(previous) = owner.filter(|previous| *previous != application) {
            return Err(package_application_error(
                nup,
                &toc,
                p,
                application,
                previous,
                limits,
                token,
            ));
        }
        owner = Some(application);
    }
    if (upgrade.is_some() || multi.is_some()) && platform != "switch2" {
        return Err(unsupported(
            "Switch 2 Program import requires --platform switch2",
        ));
    }
    // Decoded bytes written to the stage. Streamed, only ExeFS is staged.
    let stage_ceiling = if nup_options.stream {
        limits.max_executable_bytes
    } else {
        limits.max_total_bytes
    };
    let mut budget = StageBudget {
        used: 0,
        limit: max_staging_bytes.min(stage_ceiling),
    };
    // Reject declared expansion before creating or writing the staging tree.
    // Count every selected package occurrence; skipped DLC consumes no stage.
    let mut declared = 0u64;
    let mut snapshots = 0usize;
    for p in &toc.packages {
        cancelled(token)?;
        if p.kind == 2 && nup_options.skip_addons {
            continue;
        }
        let tree = toc.package_tree(p, limits)?;
        snapshots = snapshots.saturating_add(
            1 + tree
                .dirs
                .iter()
                .filter(|name| child_program_name(name))
                .count(),
        );
        if snapshots > limits.max_snapshots {
            return Err(limit("NUP snapshot budget"));
        }
        if tree
            .files
            .get("exefs/main.npdm")
            .is_some_and(|&i| toc.files[i].size > 1024 * 1024)
            || tree.files.iter().any(|(path, &i)| {
                nested_program_file(path).is_some_and(|(_, relative)| relative == "exefs/main.npdm")
                    && toc.files[i].size > 1024 * 1024
            })
        {
            return Err(limit("NUP NPDM exceeds 1 MiB"));
        }
        for (path, index) in &tree.files {
            if nup_options.stream && !executable_file(path) {
                continue;
            }
            declared = declared
                .checked_add(toc.files[*index].size)
                .filter(|&n| n <= budget.limit)
                .ok_or_else(|| limit("NUP declared expansion exceeds staging budget"))?;
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = StageDir::create(parent.join(format!(".nx-nup-{nanos:x}-{}", std::process::id())))?;
    let package_root = dir.path.join("pkg");
    fs::create_dir(&package_root)?;

    let base = toc.packages.iter().find(|p| p.kind == 0);
    let mut skipped = Vec::new();
    let mut text = "NXIMPORT\t1\n".to_owned();
    let mut base_tree = None;
    // A separate base tree and staging path for each Switch 2 Program.
    let mut program_bases: BTreeMap<u64, (Tree, String)> = BTreeMap::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut keys = BTreeSet::new();
    let mut auxiliary_bases: BTreeMap<(u64, String), (u64, String)> = BTreeMap::new();
    // Base first, so updates can link to its staged files.
    let mut order: Vec<&Package> = toc.packages.iter().collect();
    order.sort_by_key(|p| (p.kind != 0, p.version));
    let mut counted = 0usize;
    for p in &order {
        if (p.kind == 2 && nup_options.skip_addons) || nup_options.stream {
            continue;
        }
        let own = toc.package_tree(p, limits)?;
        charge_entries(&mut counted, &own, max_entries)?;
        nup_options
            .progress
            .add_total(own.files.values().map(|&i| toc.files[i].size).sum());
    }
    // Streamed: the small staged files are not part of the writer's bar.
    let decode_progress = if nup_options.stream {
        crate::ProgressCounter::default()
    } else {
        nup_options.progress.clone()
    };
    let nup_path = std::sync::Arc::new(nup.to_path_buf());
    let mut external: BTreeMap<String, Vec<(String, super::tree::SourceFile)>> = BTreeMap::new();
    let mut metadata: BTreeMap<String, super::PackageMetadata> = BTreeMap::new();
    let mut extents: BTreeMap<usize, std::sync::Arc<NupExtent>> = BTreeMap::new();
    // One slot per base file named by an update, shared by both snapshots.
    let mut slots: BTreeMap<usize, std::sync::Arc<std::sync::OnceLock<u32>>> = BTreeMap::new();
    let mut resolved = 0usize;
    for p in order {
        cancelled(token)?;
        if (p.kind, p.flags) == (2, 0) && nup_options.skip_addons {
            skipped.push(SkippedAddon {
                title_id: p.title_id,
                version: p.version,
            });
            continue;
        }
        let (snapshot_type, original_application) = package_role(p, base.is_some())?;
        let is_upgrade = upgrade.as_ref().is_some_and(|plan| plan.includes(p));
        let application = if let Some(plan) = multi.as_ref() {
            plan.application
        } else if is_upgrade {
            upgrade.as_ref().unwrap().legacy_application
        } else {
            original_application
        };
        if let Some(b) = base.filter(|_| multi.is_none()) {
            if application != b.title_id {
                return Err(bad("update belongs to another application"));
            }
        }
        let multi_program = multi.as_ref().and_then(|plan| plan.program(p));
        let (kind, key) = if is_upgrade {
            ("standalone-update", format!("upgrade-{}", p.version))
        } else if let Some(program) = multi_program {
            match snapshot_type {
                crate::SnapshotType::Base => ("base", format!("base-program-{}", hex16(program))),
                crate::SnapshotType::Update => (
                    "update",
                    format!("update-program-{}-{}", hex16(program), p.version),
                ),
                _ => return Err(bad("unexpected Switch 2 multi-base package kind")),
            }
        } else {
            match snapshot_type {
                crate::SnapshotType::Base => ("base", "base".to_owned()),
                crate::SnapshotType::Update => ("update", format!("update-{}", p.version)),
                crate::SnapshotType::StandaloneUpdate => {
                    ("standalone-update", format!("update-{}", p.version))
                }
                crate::SnapshotType::Dlc => {
                    ("dlc", format!("dlc-{}-{}", hex16(p.title_id), p.version))
                }
                crate::SnapshotType::Unknown => {
                    return Err(unsupported("unknown NUP snapshot type"))
                }
            }
        };
        if !keys.insert(key.clone()) {
            return Err(bad("two updates share one version"));
        }
        let own = toc.package_tree(p, limits)?;
        let base_for_update = multi_program.and_then(|id| program_bases.get(&id));
        let tree = if p.kind == 1 && multi.is_some() {
            let (base_tree, _) = base_for_update
                .ok_or_else(|| bad("Switch 2 update has no matching staged base Program"))?;
            overlay(base_tree, &own)?
        } else {
            match (p.kind, &base_tree) {
                (1, Some(b)) => resolve_nup_update(&toc, p, b, &own, upgrade.as_ref(), limits)?,
                _ => own,
            }
        };
        // Before any directory or stream entry is created for this package.
        charge_entries(&mut resolved, &tree, max_entries)?;
        let root = format!("trees/{key}");
        let root_path = package_root.join(&root);
        fs::create_dir_all(&root_path)?;
        for d in &tree.dirs {
            fs::create_dir_all(root_path.join(d))?;
        }
        for (path, index) in &tree.files {
            if nup_options.stream && !executable_file(path) {
                let file = index & !BASE_BIT;
                let f = &toc.files[file];
                let extent = extents
                    .entry(file)
                    .or_insert_with(|| {
                        std::sync::Arc::new(NupExtent {
                            path: nup_path.clone(),
                            name: f.name.clone(),
                            offset: f.extent_offset,
                            stored: f.extent_size,
                            codec: f.codec,
                            size: f.size,
                            sha256: f.sha256,
                        })
                    })
                    .clone();
                let share = if index & BASE_BIT != 0 {
                    Some(crate::model::FileShare::Alias(
                        slots.entry(file).or_default().clone(),
                    ))
                } else if kind == "base" {
                    Some(crate::model::FileShare::Publish(
                        slots.entry(file).or_default().clone(),
                    ))
                } else {
                    None
                };
                let (owner, logical) = match nested_program_file(path) {
                    Some((child, relative)) => (format!("{key}-{child}"), relative.to_owned()),
                    None => (key.clone(), path.clone()),
                };
                external
                    .entry(owner)
                    .or_default()
                    .push((logical, super::tree::SourceFile::nup(extent, share)));
                continue;
            }
            let out = root_path.join(path);
            let size = toc.files[index & !BASE_BIT].size;
            if budget.used.saturating_add(size) > budget.limit {
                return Err(limit("NUP files to stage exceed the staging byte budget"));
            }
            if index & BASE_BIT != 0 {
                let inherited_key = base_for_update.map_or("base", |(_, key)| key.as_str());
                let from = package_root.join("trees").join(inherited_key).join(path);
                if fs::hard_link(&from, &out).is_err() {
                    copy_inherited(
                        &from,
                        &out,
                        toc.files[index & !BASE_BIT].size,
                        &mut buffer,
                        token,
                        &mut budget,
                    )?;
                }
                continue;
            }
            decode(
                &mut source,
                &toc.files[*index],
                &out,
                &mut buffer,
                token,
                &mut budget,
                &decode_progress,
            )?;
        }
        let dlc = kind == "dlc";
        let (profile, primary_program) = match fs::read(root_path.join("exefs/main.npdm")) {
            Ok(_) if dlc => return Err(unsupported("executable NUP add-on")),
            Ok(npdm) => {
                let profile = npdm_profile(&npdm);
                let validated = match profile {
                    "meta-acid2048-v1" => Some(super::structures::NpdmProfile::MetaAcid2048V1),
                    "meta-acid3072-v1" => Some(super::structures::NpdmProfile::MetaAcid3072V1),
                    _ => None,
                };
                let program = match validated {
                    Some(profile) => super::structures::validate_npdm(&npdm, profile)?.program_id,
                    None => application,
                };
                (profile, program)
            }
            Err(_) => ("none", application),
        };
        if let Some(expected) = multi_program {
            if primary_program != expected || profile != "meta-acid3072-v1" {
                return Err(bad(&format!(
                    "Switch 2 package #{} ({:016x}) must use its verified Program {:016x}",
                    p.ordinal, p.title_id, expected
                )));
            }
        }
        if !dlc && !is_upgrade && primary_program != application {
            // Standalone native Switch 2 Programs may use the low add-on bits,
            // but an NPDM must never replace an unrelated declared application.
            let native_standalone = kind == "standalone-update"
                && application >> 56 == 4
                && primary_program & !0xfff == application
                && profile == "meta-acid3072-v1";
            let native_multi_base = multi
                .as_ref()
                .is_some_and(|plan| plan.program(p) == Some(primary_program))
                && profile == "meta-acid3072-v1";
            if !native_standalone && !native_multi_base {
                return Err(bad("NPDM Program does not match the declared application"));
            }
        }
        if is_upgrade
            && upgrade
                .as_ref()
                .is_some_and(|plan| plan.ounce_program != primary_program)
        {
            return Err(bad(
                "upgraded Program does not match the native Switch 2 NPDM",
            ));
        }
        let (program, role) = if dlc {
            ("-".to_owned(), "none")
        } else {
            (
                hex16(primary_program),
                if multi
                    .as_ref()
                    .is_some_and(|plan| primary_program != plan.primary_program)
                    || (upgrade.is_some() && !is_upgrade)
                {
                    "auxiliary"
                } else {
                    "primary"
                },
            )
        };
        text.push_str(&format!(
            "snapshot\t{key}\t{root}\t{platform}\t{}\t{program}\t{role}\t{kind}\t{}\tcomplete\tmaterialized-v1\t{profile}\n",
            hex16(application),
            p.version
        ));
        if dlc {
            text.push_str(&format!("addon\t{key}\t{}\n", hex16(p.title_id)));
        }
        if kind == "update" {
            let parent = base_for_update.map_or("base", |(_, key)| key.as_str());
            text.push_str(&format!("dependency\t{key}\trequired-base\t{parent}\n"));
        } else if is_upgrade && base.is_some() {
            let parent = edition_legacy_parent(&toc, p, upgrade.as_ref().unwrap()).map_or_else(
                || "base".to_owned(),
                |parent| format!("update-{}", parent.version),
            );
            text.push_str(&format!("dependency\t{key}\trequires\t{parent}\n"));
        }
        if !dlc {
            for child in tree.dirs.iter().filter(|name| child_program_name(name)) {
                let nested = format!("{root}/{child}");
                let npdm = fs::read(package_root.join(&nested).join("exefs/main.npdm"))
                    .map_err(|_| unsupported("nested Program has no exefs/main.npdm"))?;
                let (child_profile, profile_name) = match npdm_profile(&npdm) {
                    "meta-acid2048-v1" => (
                        super::structures::NpdmProfile::MetaAcid2048V1,
                        "meta-acid2048-v1",
                    ),
                    "meta-acid3072-v1" => (
                        super::structures::NpdmProfile::MetaAcid3072V1,
                        "meta-acid3072-v1",
                    ),
                    _ => return Err(unsupported("nested Program has an unsupported NPDM")),
                };
                let program = super::structures::validate_npdm(&npdm, child_profile)?.program_id;
                let index: u64 = child["program".len()..]
                    .parse()
                    .map_err(|_| bad("invalid nested Program index"))?;
                if program != ((primary_program & !0xf) | index)
                    || program == primary_program
                    || profile_name != profile
                {
                    return Err(unsupported(
                        "nested Program belongs to a different application; explicit association required",
                    ));
                }
                let family = (primary_program & !0xf, child.clone());
                let base_program = auxiliary_bases.get(&family);
                if base_program.is_some_and(|(id, _)| *id != program) {
                    return Err(bad("nested Program ID changed between package versions"));
                }
                let (aux_kind, dependency) = if kind == "base" {
                    ("base", None)
                } else if kind == "update" {
                    match base_program {
                        Some((_, base_key)) => ("update", Some(base_key.clone())),
                        None => ("standalone-update", None),
                    }
                } else {
                    ("standalone-update", None)
                };
                let child_key = format!("{key}-{child}");
                text.push_str(&format!(
                    "snapshot\t{child_key}\t{nested}\t{platform}\t{}\t{}\tauxiliary\t{aux_kind}\t{}\tcomplete\tmaterialized-v1\t{profile_name}\n",
                    hex16(application), hex16(program), p.version
                ));
                text.push_str(&format!("program-view\t{key}\t{child}\n"));
                if let Some(base_key) = dependency {
                    text.push_str(&format!(
                        "dependency\t{child_key}\trequired-base\t{base_key}\n"
                    ));
                }
                if kind == "base" {
                    auxiliary_bases.insert(family, (program, child_key));
                }
            }
        }
        let meta = read_meta(&mut source, p, physical)?;
        if !dlc && meta.nacp.is_none() {
            // Switch 2 packages have no NACP: keep their own title and icons.
            if let Some(m) = meta.package_metadata() {
                metadata.insert(key.clone(), m);
            }
        }
        if let (false, Some(nacp)) = (dlc, &meta.nacp) {
            let at = format!("meta/{key}");
            fs::create_dir_all(package_root.join(&at))?;
            budget.write(&package_root.join(&at).join("control.nacp"), nacp)?;
            text.push_str(&format!("nacp\t{key}\t{at}/control.nacp\n"));
            let mut icons: BTreeMap<&str, &[u8]> = BTreeMap::new();
            for (tag, bytes) in &meta.localized_icons {
                if let Some((native, _)) = NACP_LANGUAGE_MAP.iter().find(|(_, t)| t == tag) {
                    icons.insert(native, bytes);
                }
            }
            if icons.is_empty() {
                // A single icon belongs to the display-fallback language.
                if let (Some(icon), Some(first)) =
                    (&meta.icon, crate::native::parse_nacp_text(nacp).0.first())
                {
                    if let Some((native, _)) =
                        NACP_LANGUAGE_MAP.iter().find(|(n, _)| *n == first.language)
                    {
                        icons.insert(native, icon);
                    }
                }
            }
            for (native, bytes) in icons {
                let path = format!("{at}/icon_{native}.img");
                budget.write(&package_root.join(&path), bytes)?;
                text.push_str(&format!("icon\t{key}\t{native}\t{path}\n"));
            }
        }
        if kind == "base" {
            if let Some(program) = multi_program {
                program_bases.insert(program, (tree, key));
            } else {
                base_tree = Some(tree);
            }
        }
    }
    if keys.is_empty() {
        return Err(bad("no base or update package to convert"));
    }
    cancelled(token)?;
    let manifest = DeclarationManifest::read(text.as_bytes(), limits)?;
    Ok(NupStage {
        dir,
        external: super::External {
            files: external,
            metadata,
            ..Default::default()
        },
        manifest,
        skipped_addons: skipped,
        staged_bytes: budget.used,
    })
}

/// Picks the NX snapshot that matches one NUP package.
type SnapshotFilter = Box<dyn Fn(&crate::SnapshotSummary) -> bool>;

#[allow(clippy::too_many_arguments)]
fn check_nup_view_files(
    toc: &Toc,
    archive: &Path,
    snapshot: &crate::SnapshotSummary,
    files: &BTreeMap<String, usize>,
    options: &WriterOptions,
    what: &str,
    out: &mut NupCheck,
    nup_files: &mut BTreeSet<usize>,
    read_back: &mut BTreeSet<(u32, usize, u64)>,
    work: &mut Vec<(crate::SnapshotFileReference, [u8; 32], String)>,
) -> Result<(), WriterError> {
    let actual = crate::inspect_snapshot_files(
        archive,
        snapshot.identity,
        options,
        crate::IntegrityPolicy::Fast,
    )?;
    let mut by_path: BTreeMap<String, crate::SnapshotFileReference> =
        actual.into_iter().map(|f| (f.path.clone(), f)).collect();
    for (path, index) in files {
        let f = &toc.files[index & !BASE_BIT];
        let stored = by_path
            .remove(path)
            .ok_or_else(|| bad(&format!("{what}: {path} is missing from the archive")))?;
        if stored.size != f.size || stored.digest != f.sha256 {
            return Err(bad(&format!("{what}: {path} differs from the NUP")));
        }
        out.files += 1;
        nup_files.insert(index & !BASE_BIT);
        let key = (
            stored.blocks.first().map(|b| b.index).unwrap_or(u32::MAX),
            stored.blocks.len(),
            stored.size,
        );
        if read_back.insert(key) {
            work.push((stored, f.sha256, format!("{what}: {path}")));
        }
    }
    if let Some(extra) = by_path.keys().next() {
        return Err(bad(&format!("{what}: {extra} is not in the NUP")));
    }
    Ok(())
}

/// Result of `check_nup`.
#[derive(Clone, Debug, Default)]
pub struct NupCheck {
    /// NX snapshots matched to NUP packages.
    pub snapshots: usize,
    /// Files compared by path, size and SHA-256, over every snapshot.
    pub files: u64,
    /// Stored files read back from the NX and hashed (each once).
    pub files_read: u64,
    /// Decoded bytes read back and hashed.
    pub bytes_verified: u64,
    /// NUP files decoded and hashed against the NUP's own table (each once).
    pub nup_files_read: u64,
    /// Snapshots whose NACP equals the NUP's.
    pub nacp_checked: usize,
}

/// Check that `archive` holds exactly what `nup` holds: one snapshot per NUP
/// package (base, updates as native patching resolves them, add-ons), each
/// with the same file paths, and every file's bytes read back from the NX,
/// decoded and hashed against the NUP's SHA-256. Each NUP file is decoded and
/// hashed too, so the NUP's bytes, not only its table, match the archive.
/// NACPs and the update's required base are compared too. Progress counts the
/// bytes read from both files.
pub fn check_nup(
    nup: &Path,
    archive: &Path,
    skip_addons: bool,
    limits: &ImportLimits,
    options: &WriterOptions,
) -> Result<NupCheck, WriterError> {
    let limits = limits.clone();
    let mut source = File::open(nup)?;
    let (toc, physical) = read_toc(&mut source, &limits)?;
    if toc.packages.len() > limits.max_snapshots {
        return Err(limit("NUP has more packages than the snapshot budget"));
    }
    let upgrade = nup_upgrade_plan(nup, &toc, &limits, &options.cancellation)?;
    let multi = nup_multi_base_plan(nup, &toc, &limits, &options.cancellation)?;
    validate_indexed_updates(
        nup,
        &toc,
        multi.as_ref(),
        upgrade.as_ref(),
        &limits,
        &options.cancellation,
    )?;
    let summary = crate::inspect_archive(archive, options, crate::IntegrityPolicy::Fast)?;
    let base = toc.packages.iter().find(|p| p.kind == 0);
    let base_program = match base {
        Some(package) => {
            nup_program_id(nup, &toc, package, &limits, &options.cancellation)?.map(|(id, _)| id)
        }
        None => None,
    };
    let mut order: Vec<&Package> = toc.packages.iter().collect();
    order.sort_by_key(|p| (p.kind != 0, p.version));
    let mut out = NupCheck::default();
    let mut matched = BTreeSet::new();
    let mut base_tree = None;
    let mut base_identity = None;
    let mut program_bases: BTreeMap<u64, (Tree, [u8; 32])> = BTreeMap::new();
    let mut auxiliary_bases = BTreeMap::new();
    // (first block index, block count) of files already read back.
    let mut read_back = BTreeSet::new();
    let mut work: Vec<(crate::SnapshotFileReference, [u8; 32], String)> = Vec::new();
    let mut nup_files = BTreeSet::new();
    let mut resolved = 0usize;
    for p in order {
        cancelled(&options.cancellation)?;
        if p.kind == 2 && skip_addons {
            continue;
        }
        let own = toc.package_tree(p, &limits)?;
        charge_entries(&mut resolved, &own, MAX_RESOLVED_ENTRIES)?;
        let is_upgrade = upgrade.as_ref().is_some_and(|plan| plan.includes(p));
        let (tree, mut application, wanted): (Tree, u64, SnapshotFilter) = match p.kind {
            0 => (
                own,
                p.title_id,
                Box::new(|s| s.snapshot_type == SnapshotType::Base),
            ),
            1 => {
                let tree = if let Some(plan) = multi.as_ref() {
                    let program = plan
                        .program(p)
                        .ok_or_else(|| bad("Switch 2 update has no matching base Program"))?;
                    let (base, _) = program_bases
                        .get(&program)
                        .ok_or_else(|| bad("Switch 2 update base Program has not been staged"))?;
                    overlay(base, &own)?
                } else {
                    match &base_tree {
                        Some(b) => resolve_nup_update(&toc, p, b, &own, upgrade.as_ref(), &limits)?,
                        None => own,
                    }
                };
                let version = p.version;
                (
                    tree,
                    if is_upgrade {
                        upgrade.as_ref().unwrap().legacy_application
                    } else {
                        p.title_id & !0xfff
                    },
                    Box::new(move |s| {
                        (if is_upgrade {
                            s.snapshot_type == SnapshotType::StandaloneUpdate
                        } else {
                            matches!(
                                s.snapshot_type,
                                SnapshotType::Update | SnapshotType::StandaloneUpdate
                            )
                        }) && s.numeric_version == version
                    }),
                )
            }
            _ => {
                let (title, version) = (p.title_id, p.version);
                (
                    own,
                    (p.title_id & !0xfff).wrapping_sub(0x1000),
                    Box::new(move |s| {
                        s.snapshot_type == SnapshotType::Dlc
                            && s.addon_title_id == Some(title)
                            && s.numeric_version == version
                    }),
                )
            }
        };
        if let Some(plan) = multi.as_ref() {
            application = plan.application;
        }
        let what = match p.kind {
            0 => "base".to_owned(),
            1 => format!("update v{}", p.version),
            _ => format!("add-on {:016x} v{}", p.title_id, p.version),
        };
        let app = summary
            .applications
            .iter()
            .find(|a| a.application_id == application)
            .ok_or_else(|| {
                bad(&format!(
                    "{what}: the archive has no application {application:016x}"
                ))
            })?;
        let expected_program = if p.kind == 2 {
            None
        } else if is_upgrade {
            upgrade.as_ref().map(|plan| plan.ounce_program)
        } else {
            nup_program_id(nup, &toc, p, &limits, &options.cancellation)?
                .map(|(id, _)| id)
                .or_else(|| multi.as_ref().and_then(|plan| plan.program(p)))
                .or(if p.kind == 1 { base_program } else { None })
                .or(Some(application))
        };
        let mut found = app
            .snapshots
            .iter()
            .filter(|s| wanted(s) && (p.kind == 2 || s.program_id == expected_program));
        let snapshot = found
            .next()
            .ok_or_else(|| bad(&format!("{what}: missing from the archive")))?;
        if found.next().is_some() {
            return Err(bad(&format!("{what}: several matching snapshots")));
        }
        matched.insert(snapshot.identity);
        out.snapshots += 1;
        if p.kind == 1 {
            let dependency_identity = if is_upgrade {
                match upgrade
                    .as_ref()
                    .and_then(|plan| edition_legacy_parent(&toc, p, plan))
                {
                    Some(legacy) => Some(
                        app.snapshots
                            .iter()
                            .find(|s| {
                                s.program_id == base_program
                                    && s.snapshot_type == SnapshotType::Update
                                    && s.numeric_version == legacy.version
                            })
                            .ok_or_else(|| bad("legacy parent update is missing from the archive"))?
                            .identity,
                    ),
                    None => base_identity,
                }
            } else if let Some(plan) = multi.as_ref() {
                let program = plan
                    .program(p)
                    .ok_or_else(|| bad("update has no base Program"))?;
                Some(
                    program_bases
                        .get(&program)
                        .ok_or_else(|| bad("update base Program is missing from the archive"))?
                        .1,
                )
            } else {
                base_identity
            };
            if let Some(b) = dependency_identity {
                if !snapshot.dependencies.iter().any(|d| {
                    d.target_snapshot_identity == b
                        && d.kind
                            == (if is_upgrade {
                                crate::DependencyKind::Requires
                            } else {
                                crate::DependencyKind::RequiredBase
                            })
                }) {
                    return Err(bad(&format!("{what}: wrong or absent base association")));
                }
            }
        }
        let meta = read_meta(&mut source, p, physical)?;
        if let (true, Some(nacp)) = (p.kind != 2, &meta.nacp) {
            let stored = crate::read_snapshot_nacp(archive, snapshot.identity, options)?;
            if stored.as_deref() != Some(nacp.as_slice()) {
                return Err(bad(&format!("{what}: NACP differs from the NUP")));
            }
            out.nacp_checked += 1;
        }
        let mut parts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        for (path, index) in &tree.files {
            let (part, logical) = match nested_program_file(path) {
                Some((child, relative)) => (child.to_owned(), relative.to_owned()),
                None => (String::new(), path.clone()),
            };
            parts.entry(part).or_default().insert(logical, *index);
        }
        check_nup_view_files(
            &toc,
            archive,
            snapshot,
            &parts.remove("").unwrap_or_default(),
            options,
            &what,
            &mut out,
            &mut nup_files,
            &mut read_back,
            &mut work,
        )?;
        for (child, files) in parts {
            let index: u64 = child["program".len()..]
                .parse()
                .map_err(|_| bad("invalid child Program index"))?;
            let program = (snapshot.program_id.unwrap_or(application) & !0xf) | index;
            let mut found = app.snapshots.iter().filter(|s| {
                s.program_id == Some(program)
                    && s.numeric_version == p.version
                    && match p.kind {
                        0 => s.snapshot_type == SnapshotType::Base,
                        1 => matches!(
                            s.snapshot_type,
                            SnapshotType::Update | SnapshotType::StandaloneUpdate
                        ),
                        _ => false,
                    }
            });
            let auxiliary = found.next().ok_or_else(|| {
                bad(&format!(
                    "{what}: missing auxiliary {child} ({program:016x})"
                ))
            })?;
            if found.next().is_some() {
                return Err(bad(&format!("{what}: ambiguous auxiliary {child}")));
            }
            if p.kind == 1 {
                if let Some(base) = auxiliary_bases.get(&program) {
                    if !auxiliary
                        .dependencies
                        .iter()
                        .any(|d| d.target_snapshot_identity == *base)
                    {
                        return Err(bad(&format!("{what}: {child} does not require its base")));
                    }
                }
            }
            matched.insert(auxiliary.identity);
            out.snapshots += 1;
            check_nup_view_files(
                &toc,
                archive,
                auxiliary,
                &files,
                options,
                &format!("{what}/{child}"),
                &mut out,
                &mut nup_files,
                &mut read_back,
                &mut work,
            )?;
            if p.kind == 0 {
                auxiliary_bases.insert(program, auxiliary.identity);
            }
        }
        if p.kind == 0 {
            if multi.is_some() {
                let program = expected_program.ok_or_else(|| bad("base Program has no NPDM"))?;
                program_bases.insert(program, (tree, snapshot.identity));
            } else {
                base_tree = Some(tree);
                base_identity = Some(snapshot.identity);
            }
        }
    }
    if base.is_none() && out.snapshots == 0 {
        return Err(bad("no package to check"));
    }
    if let Some(extra) = summary
        .applications
        .iter()
        .flat_map(|a| &a.snapshots)
        .find(|s| !matched.contains(&s.identity))
    {
        return Err(bad(&format!(
            "the archive holds a snapshot that is not in the NUP ({:?} v{})",
            extra.snapshot_type, extra.numeric_version
        )));
    }
    // Read every stored file back, in archive order, and hash its bytes.
    work.sort_by_key(|(f, _, _)| f.blocks.first().map(|b| b.offset).unwrap_or(0));
    options.progress.add_total(
        work.iter().map(|(f, _, _)| f.size).sum::<u64>()
            + nup_files.iter().map(|&i| toc.files[i].size).sum::<u64>(),
    );
    // The NUP's own bytes, in NUP order, against its table.
    let path = std::sync::Arc::new(nup.to_path_buf());
    let mut by_offset: Vec<usize> = nup_files.into_iter().collect();
    by_offset.sort_by_key(|&i| toc.files[i].extent_offset);
    let mut buffer = vec![0u8; 1 << 20];
    for i in by_offset {
        let f = &toc.files[i];
        let mut reader = NupExtent {
            path: path.clone(),
            name: f.name.clone(),
            offset: f.extent_offset,
            stored: f.extent_size,
            codec: f.codec,
            size: f.size,
            sha256: f.sha256,
        }
        .reader(&options.cancellation)?;
        loop {
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            options.progress.add_done(n as u64);
        }
        out.nup_files_read += 1;
    }
    let mut nx = io::BufReader::with_capacity(8 << 20, File::open(archive)?);
    let mut stored = Vec::new();
    let mut decoder = zstd::bulk::Decompressor::new()?;
    let mut logical = Vec::new();
    let mut position = u64::MAX;
    for (f, expected, name) in &work {
        let mut hash = Sha256::new();
        let mut size = 0u64;
        for b in &f.blocks {
            cancelled(&options.cancellation)?;
            stored.resize(b.stored_size as usize, 0);
            if position != b.offset {
                nx.seek(SeekFrom::Start(b.offset))?;
            }
            nx.read_exact(&mut stored)?;
            position = b.offset + b.stored_size as u64;
            let bytes: &[u8] = match b.codec {
                0 => &stored,
                1 => {
                    logical.resize(b.logical_size as usize, 0);
                    let n = decoder
                        .decompress_to_buffer(&stored, &mut logical[..])
                        .map_err(|e| bad(&format!("{name}: block {}: {e}", b.index)))?;
                    &logical[..n]
                }
                _ => return Err(bad(&format!("{name}: unknown block codec"))),
            };
            if bytes.len() != b.logical_size as usize {
                return Err(bad(&format!("{name}: block {} has a wrong size", b.index)));
            }
            hash.update(bytes);
            size += bytes.len() as u64;
            options.progress.add_done(bytes.len() as u64);
        }
        if size != f.size || hash.finish() != *expected {
            return Err(bad(&format!(
                "{name}: the archive bytes differ from the NUP"
            )));
        }
        out.files_read += 1;
        out.bytes_verified += size;
    }
    Ok(out)
}

#[cfg(test)]
#[path = "nup_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "nup_hostile_tests.rs"]
mod hostile_tests;
