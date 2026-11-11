use super::secure::Root;
use super::structures::{validate_npdm, validate_nso, NsoFacts};
use super::*;
use crate::hash::Sha256;
use crate::{
    DependencyInput, DirectoryInput, EntryInput, FileInput, SnapshotInput, SnapshotNativeMetadata,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    sync::Arc,
};

#[derive(Clone, Debug)]
pub struct TreeCandidate {
    pub relative_root: String,
}

#[derive(Debug)]
pub struct ExecutableAudit {
    pub npdm: Option<structures::NpdmFacts>,
    pub modules: BTreeMap<String, NsoFacts>,
}
/// Inventory known executable structures only. This establishes no application,
/// platform, version, content type, dependency or snapshot completeness.
pub fn audit_executables(
    tree: &Path,
    profile: structures::NpdmProfile,
    limits: &ImportLimits,
    token: &crate::CancellationToken,
) -> Result<ExecutableAudit, WriterError> {
    limits.validate()?;
    cancelled(token)?;
    let root = Root::open(tree)?;
    let names = root.names(
        "exefs",
        limits
            .max_nodes
            .min((limits.max_inventory_bytes / 512) as usize),
    )?;
    let mut out = ExecutableAudit {
        npdm: None,
        modules: BTreeMap::new(),
    };
    let mut work = 0;
    for name in names {
        portable_path(&name, limits)?;
        cancelled(token)?;
        let mut file = root.open_relative(&format!("exefs/{name}"))?;
        let len = file.metadata()?.len();
        if !file.metadata()?.is_file() {
            return Err(unsupported("nested/special ExeFS in executable audit"));
        }
        if name == "main.npdm" {
            if len > 1024 * 1024 {
                return Err(limit("NPDM exceeds 1 MiB"));
            }
            let mut bytes = Vec::new();
            file.take(len + 1).read_to_end(&mut bytes)?;
            out.npdm = Some(validate_npdm(&bytes, profile)?);
            continue;
        }
        let mut magic = [0; 4];
        let got = file.read(&mut magic)?;
        if (got == 4 && &magic[..3] == b"NSO")
            || ["main", "sdk", "rtld"].contains(&name.as_str())
            || name.starts_with("subsdk")
        {
            let mut bounded = limits.clone();
            bounded.max_executable_bytes = limits.max_executable_bytes.saturating_sub(work);
            let facts = validate_nso(&mut file, len, &bounded, token)
                .map_err(|e| WriterError::new(e.kind(), format!("exefs/{name}: {e}")))?;
            work += facts.decoded_bytes;
            out.modules.insert(name, facts);
        }
    }
    if out.npdm.is_none() && profile != structures::NpdmProfile::Absent {
        return Err(invalid("audit profile requires missing NPDM"));
    }
    Ok(out)
}

/// Discover only the documented shapes, with an explicit finite directory walk.
/// A candidate has no inferred platform, application, type, version or ownership.
pub fn discover(package: &Path, limits: &ImportLimits) -> Result<Vec<TreeCandidate>, WriterError> {
    limits.validate()?;
    let root = Root::open(package)?;
    let mut result = Vec::new();
    let mut count = 0;
    fn walk(
        root: &Root,
        at: &str,
        level: usize,
        limits: &ImportLimits,
        count: &mut usize,
        out: &mut Vec<TreeCandidate>,
    ) -> Result<(), WriterError> {
        let names = root.names(
            at,
            limits
                .max_nodes
                .saturating_sub(*count)
                .min((limits.max_inventory_bytes / 512) as usize),
        )?;
        *count = count
            .checked_add(names.len())
            .ok_or_else(|| limit("discovery count overflow"))?;
        if *count > limits.max_nodes {
            return Err(limit("discovery node budget"));
        }
        if names
            .iter()
            .any(|n| ["exefs", "romfs", "data"].contains(&n.as_str()))
        {
            for n in names
                .iter()
                .filter(|n| ["exefs", "romfs", "data"].contains(&n.as_str()))
            {
                let p = if at.is_empty() {
                    n.clone()
                } else {
                    format!("{at}/{n}")
                };
                if !root.open_relative(&p)?.metadata()?.is_dir() {
                    return Err(invalid("content root is not a directory"));
                }
            }
            if out.len() == limits.max_snapshots {
                return Err(limit("candidate limit"));
            }
            out.push(TreeCandidate {
                relative_root: if at.is_empty() { ".".into() } else { at.into() },
            });
            return Ok(());
        }
        if level >= 3 {
            return Ok(());
        }
        for n in names {
            portable_path(&n, limits)?;
            if level == 0 && !["base", "updates", "dlc"].contains(&n.as_str()) {
                continue;
            }
            let p = if at.is_empty() {
                n.clone()
            } else {
                format!("{at}/{n}")
            };
            if root.open_relative(&p)?.metadata()?.is_dir() {
                walk(root, &p, level + 1, limits, count, out)?;
            }
        }
        Ok(())
    }
    walk(&root, "", 0, limits, &mut count, &mut result)?;
    Ok(result)
}

/// How much of each source file a read-ahead touches.
const PREFETCH_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SourceFile {
    pub(crate) logical_path: String,
    pub(crate) size: u64,
    pub(crate) digest: Option<[u8; 32]>,
    root: Option<Arc<Root>>,
    source_path: String,
    native_path: Option<PathBuf>,
    offset: u64,
    /// Read from a NUP package and decoded on the fly (not staged on disk).
    nup: Option<Arc<super::nup::NupExtent>>,
    /// Same bytes as a file of an earlier snapshot of the same write.
    share: Option<crate::model::FileShare>,
}
impl SourceFile {
    /// A file stored in a NUP package: decoded while the writer reads it and
    /// checked against the package's SHA-256. `share` names it from another
    /// snapshot of the same write without reading it twice.
    pub(crate) fn nup(
        extent: Arc<super::nup::NupExtent>,
        share: Option<crate::model::FileShare>,
    ) -> Self {
        Self {
            logical_path: String::new(),
            size: extent.size,
            digest: Some(extent.sha256),
            root: None,
            source_path: String::new(),
            native_path: None,
            offset: 0,
            nup: Some(extent),
            share,
        }
    }
    pub(crate) fn descriptor_bytes(&self) -> u64 {
        1024 + self.logical_path.len() as u64 * 4 + self.source_path.len() as u64 * 2
    }
    /// Metadata location for audits/conformance, not a bypass of pinned reads.
    pub fn host_extent(&self) -> (PathBuf, u64) {
        if let Some(n) = &self.nup {
            return (n.path.as_ref().clone(), n.offset);
        }
        (
            self.root
                .as_ref()
                .map(|r| super::join(&r.path, &self.source_path))
                .unwrap_or_else(|| self.native_path.clone().unwrap_or_default()),
            self.offset,
        )
    }
    pub fn to_writer_file(&self, token: &crate::CancellationToken) -> FileInput {
        self.input(token)
    }
    /// Bounded structural probe. Full digest verification belongs to the
    /// complete stream consumed by the writer, not to an arbitrary short range.
    pub fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<(), WriterError> {
        if output.len() > 1024 * 1024
            || offset > self.size
            || output.len() as u64 > self.size - offset
        {
            return Err(invalid("source probe exceeds bounded extent"));
        }
        let mut f = self.file()?;
        f.seek(SeekFrom::Start(self.offset + offset))?;
        f.read_exact(output)?;
        Ok(())
    }
    pub fn path(&self) -> &str {
        &self.logical_path
    }
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn sha256(&self) -> Option<[u8; 32]> {
        self.digest
    }
    pub(crate) fn native_extent(path: String, source: PathBuf, offset: u64, size: u64) -> Self {
        Self {
            logical_path: path,
            size,
            digest: None,
            root: None,
            source_path: String::new(),
            native_path: Some(source),
            offset,
            nup: None,
            share: None,
        }
    }
    fn file(&self) -> Result<File, WriterError> {
        if let Some(n) = &self.nup {
            // Positioned raw access exists for stored (codec 0) files only.
            if n.codec != 0 {
                return Err(invalid("compressed NUP file has no positioned access"));
            }
            let mut file = File::open(n.path.as_ref())?;
            file.seek(SeekFrom::Start(n.offset))?;
            return Ok(file);
        }
        let mut file = if let Some(root) = &self.root {
            root.open_relative(&self.source_path)?
        } else {
            File::open(
                self.native_path
                    .as_ref()
                    .ok_or_else(|| invalid("source missing"))?,
            )?
        };
        if !file.metadata()?.is_file()
            || file.metadata()?.len()
                < self
                    .offset
                    .checked_add(self.size)
                    .ok_or_else(|| invalid("source extent overflow"))?
        {
            return Err(invalid("source size/type changed"));
        }
        if self.root.is_some() && file.metadata()?.len() != self.size {
            return Err(invalid("source file changed length"));
        }
        file.seek(SeekFrom::Start(self.offset))?;
        Ok(file)
    }
    /// Opens the file and reads its start so that the comparison that follows
    /// finds it warm (disk, cache, antivirus scan). Errors are left to it.
    pub(crate) fn prefetch(&self) {
        if self.nup.is_some() {
            return;
        }
        if let Ok(file) = self.file() {
            let _ = std::io::copy(&mut file.take(PREFETCH_BYTES), &mut std::io::sink());
        }
    }
    pub(crate) fn open_reader(
        &self,
        token: &crate::CancellationToken,
    ) -> Result<CheckedReader, WriterError> {
        if self.nup.is_some() {
            return Err(invalid("a NUP file is read by the writer only"));
        }
        Ok(CheckedReader {
            file: self.file()?,
            remaining: self.size,
            expected: self.digest,
            hash: Sha256::new(),
            finished: false,
            whole: self.root.is_some(),
            size: self.size,
            token: token.clone(),
        })
    }
    fn input(&self, token: &crate::CancellationToken) -> FileInput {
        use crate::model::FileShare;
        if let Some(FileShare::Alias(slot)) = &self.share {
            return FileInput::alias(self.size, slot.clone());
        }
        let publish = match &self.share {
            Some(FileShare::Publish(slot)) => Some(slot.clone()),
            _ => None,
        };
        let input = self.read_input(token);
        match publish {
            Some(slot) => input.with_publish(slot),
            None => input,
        }
    }
    fn read_input(&self, token: &crate::CancellationToken) -> FileInput {
        // Lazy opening bounds descriptors independently of the number of files.
        struct Lazy {
            source: SourceFile,
            reader: Option<Box<dyn Read>>,
            done: bool,
            token: crate::CancellationToken,
        }
        impl Read for Lazy {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                if self.done {
                    return Ok(0);
                }
                if self.reader.is_none() {
                    self.reader = Some(match &self.source.nup {
                        Some(n) => n.reader(&self.token).map_err(io::Error::other)?,
                        None => Box::new(
                            self.source
                                .open_reader(&self.token)
                                .map_err(io::Error::other)?,
                        ),
                    });
                }
                let n = self.reader.as_mut().unwrap().read(b)?;
                if n == 0 && !b.is_empty() {
                    // End of file: release the descriptor now, not when the
                    // writer drops this input.
                    self.reader = None;
                    self.done = true;
                }
                Ok(n)
            }
        }
        let warm = self.clone();
        FileInput::new(
            self.size,
            Lazy {
                source: self.clone(),
                reader: None,
                done: false,
                token: token.clone(),
            },
        )
        .with_warm_up(std::sync::Arc::new(move || warm.warm_up()))
    }
    /// Reads the extent and discards it (see `FileInput::warm`). Errors are
    /// left to the real, checked read.
    fn warm_up(&self) {
        if let Some(n) = &self.nup {
            // The stored (maybe compressed) bytes: the decode then reads cache.
            let Ok(mut file) = File::open(n.path.as_ref()) else {
                return;
            };
            if file.seek(SeekFrom::Start(n.offset)).is_ok() {
                let _ = io::copy(&mut file.take(n.stored), &mut io::sink());
            }
            return;
        }
        let Ok(file) = self.file() else { return };
        let mut left = file.take(self.size);
        let _ = io::copy(&mut left, &mut io::sink());
    }
}
pub(crate) struct CheckedReader {
    file: File,
    remaining: u64,
    expected: Option<[u8; 32]>,
    hash: Sha256,
    finished: bool,
    whole: bool,
    size: u64,
    token: crate::CancellationToken,
}
impl Read for CheckedReader {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if b.is_empty() {
            return Ok(0);
        }
        if self.token.is_cancelled() {
            return Err(io::Error::other("import cancelled"));
        }
        if self.remaining == 0 {
            if !self.finished {
                if self.whole && self.file.metadata()?.len() != self.size {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "source length changed",
                    ));
                }
                let actual = self.hash.clone().finish();
                if self.expected.is_some_and(|d| d != actual) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "source SHA-256 changed after inventory",
                    ));
                }
                self.finished = true;
            }
            return Ok(0);
        }
        let n = b.len().min(self.remaining.min(65536) as usize);
        let got = self.file.read(&mut b[..n])?;
        if got == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source truncated",
            ));
        }
        if self.expected.is_some() {
            self.hash.update(&b[..got]);
        }
        self.remaining -= got as u64;
        Ok(got)
    }
}

#[derive(Clone)]
pub struct TreeView {
    pub(crate) scope: ContentScope,
    pub(crate) declaration: SnapshotDeclaration,
    pub(crate) modules: BTreeMap<String, NsoFacts>,
    pub(crate) files: BTreeMap<String, SourceFile>,
    pub(crate) directories: BTreeSet<String>,
    pub(crate) accounted_bytes: u64,
    pub(crate) decoded_executable_bytes: u64,
    pub(crate) token: crate::CancellationToken,
    pub(crate) watch: NamespaceWatch,
}
#[derive(Clone)]
pub(crate) struct NamespaceWatch {
    root: Arc<Root>,
    selected: String,
    expected: [u8; 32],
}
fn namespace_row(hash: &mut Sha256, path: &str, entry: &super::secure::Entry) {
    hash.update(&(path.len() as u32).to_le_bytes());
    hash.update(path.as_bytes());
    hash.update(&[u8::from(entry.dir)]);
    hash.update(&if entry.dir { 0 } else { entry.size }.to_le_bytes());
    // Ordinary rewrites, including same-size ones, change the write time.
    hash.update(&if entry.dir { 0 } else { entry.mtime }.to_le_bytes());
}
impl NamespaceWatch {
    pub(crate) fn check(
        &self,
        limits: &ImportLimits,
        token: &crate::CancellationToken,
    ) -> Result<(), WriterError> {
        fn walk(
            root: &Root,
            source: &str,
            logical: &str,
            hash: &mut Sha256,
            count: &mut (usize, u64),
            limits: &ImportLimits,
            token: &crate::CancellationToken,
        ) -> Result<(), WriterError> {
            cancelled(token)?;
            let entries = root.list(
                source,
                limits
                    .max_nodes
                    .saturating_sub(count.0)
                    .min((limits.max_inventory_bytes.saturating_sub(count.1) / 1024) as usize),
            )?;
            let scratch = entries.len() as u64 * 1024;
            count.1 += scratch;
            for entry in entries {
                let name = entry.name.clone();
                count.0 += 1;
                if count.0 > limits.max_nodes {
                    return Err(limit("namespace verification budget"));
                }
                let path = if logical.is_empty() {
                    name.clone()
                } else {
                    format!("{logical}/{name}")
                };
                portable_path(&path, limits)?;
                let at = if source.is_empty() {
                    name
                } else {
                    format!("{source}/{name}")
                };
                namespace_row(hash, &path, &entry);
                if entry.dir {
                    walk(root, &at, &path, hash, count, limits, token)?;
                }
            }
            count.1 -= scratch;
            Ok(())
        }
        let mut hash = Sha256::new();
        walk(
            &self.root,
            &self.selected,
            "",
            &mut hash,
            &mut (0, 0),
            limits,
            token,
        )?;
        if hash.finish() != self.expected {
            return Err(invalid("source namespace changed during conversion"));
        }
        Ok(())
    }
}
impl TreeView {
    pub fn completeness(&self) -> Completeness {
        if !self.declaration.complete {
            Completeness::UnresolvedPartial
        } else if let Some(c) = &self.declaration.composition {
            Completeness::ComposedExactBase {
                identity: c.exact_identity,
                contract: "replace-delete-v1",
            }
        } else {
            Completeness::DeclaredComplete
        }
    }
    pub fn scope(&self) -> &ContentScope {
        &self.scope
    }
    pub fn declaration(&self) -> &SnapshotDeclaration {
        &self.declaration
    }
    pub fn modules(&self) -> &BTreeMap<String, NsoFacts> {
        &self.modules
    }
    pub fn accounted_bytes(&self) -> u64 {
        self.accounted_bytes
    }

    pub fn files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.values()
    }
    pub(crate) fn writer_snapshot(
        &self,
        dependencies: Vec<DependencyInput>,
    ) -> Result<SnapshotInput, WriterError> {
        if !self.declaration.complete {
            return Err(invalid("partial tree cannot be passed to the writer"));
        }
        let mut built: BTreeMap<&str, DirectoryInput> = self
            .directories
            .iter()
            .map(|p| (p.as_str(), DirectoryInput::empty()))
            .collect();
        built.insert("", DirectoryInput::empty());
        for (path, file) in &self.files {
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            built
                .get_mut(parent)
                .ok_or_else(|| invalid("missing source parent"))?
                .entries
                .push(EntryInput::file(name, file.input(&self.token)));
        }
        for path in self.directories.iter().rev() {
            let dir = built
                .remove(path.as_str())
                .ok_or_else(|| invalid("duplicate source directory"))?;
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            built
                .get_mut(parent)
                .ok_or_else(|| invalid("missing directory parent"))?
                .entries
                .push(EntryInput::directory(name, dir));
        }
        let root = built
            .remove("")
            .ok_or_else(|| invalid("missing source root"))?;
        Ok(SnapshotInput {
            addon_title_id: self.declaration.addon_title_id,
            native: SnapshotNativeMetadata {
                npdm: self.files.get("exefs/main.npdm").map(|f| {
                    crate::MediaInput::new(
                        crate::MediaType::Npdm,
                        f.size,
                        f.input(&self.token).reader,
                    )
                }),
                required_firmware: None,
            },
            program_id: self.scope.program_id.value,
            snapshot_type: self.declaration.snapshot_type,
            numeric_version: self.declaration.numeric_version,
            content_id: self.declaration.content_id,
            build_id: self.scope.build_id.value,
            display_version: None,
            dependencies,
            metadata: None,
            root,
        })
    }
}
fn declared<T>(value: T, manifest: [u8; 32], field: &str) -> Evidence<T> {
    Evidence {
        value: Some(value),
        origin: Origin::UserDeclaration {
            manifest_sha256: manifest,
            field: field.into(),
        },
    }
}

pub fn inspect_tree(
    package: &Path,
    manifest: &DeclarationManifest,
    key: &str,
    limits: &ImportLimits,
    token: &crate::CancellationToken,
) -> Result<TreeView, WriterError> {
    let manifest = manifest.revalidated(limits)?;
    let declaration = manifest
        .snapshots
        .iter()
        .find(|s| s.key == key)
        .ok_or_else(|| invalid("unknown declared tree"))?
        .clone();
    inspect_declared(package, declaration, manifest.sha256, limits, token)
}
pub(crate) fn inspect_declared(
    package: &Path,
    declaration: SnapshotDeclaration,
    manifest: [u8; 32],
    limits: &ImportLimits,
    token: &crate::CancellationToken,
) -> Result<TreeView, WriterError> {
    inspect_pinned(
        Arc::new(Root::open(package)?),
        declaration,
        manifest,
        limits,
        token,
    )
}
pub(crate) fn inspect_pinned(
    root: Arc<Root>,
    declaration: SnapshotDeclaration,
    manifest: [u8; 32],
    limits: &ImportLimits,
    token: &crate::CancellationToken,
) -> Result<TreeView, WriterError> {
    inspect_pinned_with(root, declaration, manifest, limits, token, &[])
}
/// As `inspect_pinned`, plus `external` files of the tree that are not on
/// disk (a NUP's RomFS, read while writing): (path in the tree, source).
/// Their directories must be on disk; ExeFS must be on disk.
pub(crate) fn inspect_pinned_with(
    root: Arc<Root>,
    declaration: SnapshotDeclaration,
    manifest: [u8; 32],
    limits: &ImportLimits,
    token: &crate::CancellationToken,
    external: &[(String, SourceFile)],
) -> Result<TreeView, WriterError> {
    if limits.max_depth > 64
        || limits.max_inventory_bytes < 1024 * 1024
        || limits.max_path_bytes > 4096
    {
        return Err(limit("unsupported inventory depth/memory/path limits"));
    }
    cancelled(token)?;
    let selected = if declaration.root == "." {
        String::new()
    } else {
        declaration.root.clone()
    };
    let mut view = TreeView {
        scope: ContentScope {
            source_kind: SourceKind::ExtractedTree,
            layout: ContentLayout::ExtractedExeFsRomFs,
            platform: declared(declaration.platform, manifest, "platform"),
            application_id: declared(declaration.application_id, manifest, "application-id"),
            program_id: declaration
                .program_id
                .map(|p| declared(p, manifest, "program-id"))
                .unwrap_or_else(Evidence::unknown),
            snapshot_type: declared(declaration.snapshot_type, manifest, "snapshot-type"),
            numeric_version: declared(declaration.numeric_version, manifest, "numeric-version"),
            build_id: Evidence::unknown(),
        },
        declaration,
        modules: BTreeMap::new(),
        files: BTreeMap::new(),
        directories: BTreeSet::new(),
        accounted_bytes: 1024 * 1024,
        decoded_executable_bytes: 0,
        token: token.clone(),
        watch: NamespaceWatch {
            root: root.clone(),
            selected: selected.clone(),
            expected: [0; 32],
        },
    };
    let mut folded = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    let mut total = 0u64;
    #[allow(clippy::too_many_arguments)]
    fn scan(
        root: &Arc<Root>,
        source: &str,
        logical: &str,
        view: &mut TreeView,
        folded: &mut BTreeSet<String>,
        excluded: &mut BTreeSet<String>,
        total: &mut u64,
        limits: &ImportLimits,
        namespace: &mut Sha256,
        buffer: &mut [u8],
    ) -> Result<(), WriterError> {
        cancelled(&view.token)?;
        let max = limits
            .max_nodes
            .saturating_sub(folded.len())
            .min(((limits.max_inventory_bytes - view.accounted_bytes) / 1024) as usize);
        let entries = root.list(source, max)?;
        let scratch = (entries.len() * 1024) as u64;
        view.accounted_bytes += scratch;
        for entry in entries {
            let name = entry.name.clone();
            let path = if logical.is_empty() {
                name.clone()
            } else {
                format!("{logical}/{name}")
            };
            portable_path(&path, limits)?;
            if !folded.insert(path.to_ascii_lowercase()) {
                return Err(invalid("case-colliding source paths"));
            }
            let at = if source.is_empty() {
                name
            } else {
                format!("{source}/{name}")
            };
            namespace_row(namespace, &path, &entry);
            let charge = 1024 + path.len() as u64 * 4 + at.len() as u64 * 2;
            view.accounted_bytes = view
                .accounted_bytes
                .checked_add(charge)
                .ok_or_else(|| limit("inventory memory overflow"))?;
            if view.accounted_bytes > limits.max_inventory_bytes || folded.len() > limits.max_nodes
            {
                return Err(limit("tree inventory node/memory budget"));
            }
            if logical.is_empty()
                && !entry.dir
                && view.declaration.declares_root_metadata(&entry.name)
            {
                continue;
            }
            if view.declaration.exclusions.contains(&path) {
                if entry.dir || path == "exefs/main" || path == "exefs/main.npdm" {
                    return Err(invalid("exclusion is not an allowed regular artifact"));
                }
                excluded.insert(path);
                continue;
            }
            if logical.is_empty() && !["exefs", "romfs", "data"].contains(&path.as_str()) {
                return Err(unsupported(
                    "unknown root entry; declare an explicit artifact exclusion",
                ));
            }
            if entry.dir {
                if logical == "exefs" {
                    return Err(unsupported(
                        "nested ExeFS requires explicit separate Program views",
                    ));
                }
                view.directories.insert(path.clone());
                scan(
                    root, &at, &path, view, folded, excluded, total, limits, namespace, buffer,
                )?;
            } else {
                let size = entry.size;
                *total = total
                    .checked_add(size)
                    .ok_or_else(|| limit("source byte overflow"))?;
                if size > limits.max_file_bytes || *total > limits.max_total_bytes {
                    return Err(limit("tree file/total size budget"));
                }
                // Only ExeFS is structurally validated from this inventory, so
                // only it needs content continuity up to the writer's read.
                // Everything else is read once, by the writer; ordinary
                // changes are caught by the size/write-time namespace check.
                let digest = if logical == "exefs" {
                    let mut file = root.open_relative(&at)?;
                    if file.metadata()?.len() != size {
                        return Err(invalid("source changed during inventory"));
                    }
                    let mut hash = Sha256::new();
                    let mut left = size;
                    while left != 0 {
                        cancelled(&view.token)?;
                        let n = left.min(buffer.len() as u64) as usize;
                        file.read_exact(&mut buffer[..n])?;
                        hash.update(&buffer[..n]);
                        left -= n as u64;
                    }
                    let mut extra = [0];
                    if file.read(&mut extra)? != 0 {
                        return Err(invalid("source grew during inventory"));
                    }
                    Some(hash.finish())
                } else {
                    None
                };
                view.files.insert(
                    path.clone(),
                    SourceFile {
                        logical_path: path,
                        size,
                        digest,
                        root: Some(root.clone()),
                        source_path: at,
                        native_path: None,
                        offset: 0,
                        nup: None,
                        share: None,
                    },
                );
            }
        }
        view.accounted_bytes -= scratch;
        Ok(())
    }
    let mut namespace = Sha256::new();
    let mut buffer = vec![0; 65536];
    scan(
        &root,
        &selected,
        "",
        &mut view,
        &mut folded,
        &mut excluded,
        &mut total,
        limits,
        &mut namespace,
        &mut buffer,
    )?;
    view.watch.expected = namespace.finish();
    for (path, file) in external {
        cancelled(token)?;
        portable_path(path, limits)?;
        let (parent, _) = path
            .rsplit_once('/')
            .ok_or_else(|| invalid("streamed file at the tree root"))?;
        let top = path.split('/').next().unwrap_or_default();
        if !["romfs", "data"].contains(&top) {
            return Err(invalid("streamed file outside romfs/ and data/"));
        }
        if !view.directories.contains(parent) {
            return Err(invalid("streamed file has no staged parent directory"));
        }
        if !folded.insert(path.to_ascii_lowercase()) {
            return Err(invalid("case-colliding source paths"));
        }
        view.accounted_bytes = view
            .accounted_bytes
            .checked_add(1024 + path.len() as u64 * 4)
            .ok_or_else(|| limit("inventory memory overflow"))?;
        total = total
            .checked_add(file.size)
            .ok_or_else(|| limit("source byte overflow"))?;
        if view.accounted_bytes > limits.max_inventory_bytes
            || folded.len() > limits.max_nodes
            || file.size > limits.max_file_bytes
            || total > limits.max_total_bytes
        {
            return Err(limit("tree file/total size budget"));
        }
        let mut file = file.clone();
        file.logical_path = path.clone();
        view.files.insert(path.clone(), file);
    }
    if excluded != view.declaration.exclusions {
        return Err(invalid("declared exclusion is absent"));
    }
    if view.files.is_empty()
        && (view.declaration.complete
            || view
                .declaration
                .composition
                .as_ref()
                .map_or(true, |c| c.remove.is_empty()))
    {
        return Err(invalid("tree contains no content"));
    }
    validate_view(&mut view, limits)?;
    Ok(view)
}

pub(crate) fn validate_view(view: &mut TreeView, limits: &ImportLimits) -> Result<(), WriterError> {
    if view.declaration.snapshot_type == SnapshotType::Dlc {
        if view.directories.contains("exefs") {
            return Err(unsupported("executable extracted DLC associations"));
        }
        return Ok(());
    }
    view.modules.clear();
    view.decoded_executable_bytes = 0;
    let mut program = None;
    if let Some(npdm) = view.files.get("exefs/main.npdm") {
        if npdm.size > 1024 * 1024 {
            return Err(limit("NPDM exceeds 1 MiB"));
        }
        let mut bytes = Vec::new();
        npdm.open_reader(&view.token)?.read_to_end(&mut bytes)?;
        let facts = validate_npdm(&bytes, view.declaration.npdm_profile)?;
        if view
            .declaration
            .program_id
            .is_some_and(|p| p != facts.program_id)
        {
            return Err(invalid(&format!(
                "Program ID conflict: declared {:?}, native {:016x}",
                view.declaration.program_id, facts.program_id
            )));
        }
        program = Some(facts.program_id);
        view.scope.program_id = Evidence {
            value: program,
            origin: Origin::NativeStructure {
                profile: format!("{:?}", facts.profile),
                location: npdm.source_path.clone(),
            },
        };
    } else if view.declaration.complete
        && view.declaration.npdm_profile != structures::NpdmProfile::Absent
    {
        return Err(invalid("declared NPDM profile requires main.npdm"));
    }
    if view.declaration.complete && program.is_none() && view.scope.program_id.value.is_none() {
        return Err(invalid("executable tree lacks a Program ID"));
    }
    for (path, file) in &view.files {
        if !path.starts_with("exefs/") || path == "exefs/main.npdm" {
            continue;
        }
        let mut source = file.file()?;
        let mut magic = [0; 4];
        let got = source.read(&mut magic)?;
        let name = path.strip_prefix("exefs/").unwrap();
        if (got == 4 && &magic == b"META") || name.ends_with(".npdm") {
            return Err(unsupported(
                "additional NPDM/Program associations require separate views",
            ));
        }
        if (got == 4 && &magic[..3] == b"NSO")
            || ["main", "rtld", "sdk"].contains(&name)
            || name.starts_with("subsdk")
        {
            let mut bounded = limits.clone();
            bounded.max_executable_bytes = limits
                .max_executable_bytes
                .saturating_sub(view.decoded_executable_bytes);
            let facts = validate_nso(&mut source, file.size, &bounded, &view.token)?;
            view.decoded_executable_bytes += facts.decoded_bytes;
            view.modules.insert(path.clone(), facts);
            // Bind native observations to the same full bytes the writer will use.
            let mut checked = file.open_reader(&view.token)?;
            io::copy(&mut checked, &mut io::sink())?;
        }
    }
    let Some(main) = view.modules.get("exefs/main") else {
        return if view.declaration.complete {
            Err(invalid(
                "complete executable view lacks a validated main NSO",
            ))
        } else {
            Ok(())
        };
    };
    if view
        .declaration
        .build_id
        .is_some_and(|b| Some(b) != main.build_id)
    {
        return Err(invalid(&format!(
            "Build ID conflict: declared {}, native {}",
            hex(&view.declaration.build_id.unwrap()),
            main.build_id
                .map(|b| hex(&b))
                .unwrap_or_else(|| "unknown".into())
        )));
    }
    view.scope.build_id = Evidence {
        value: main.build_id,
        origin: Origin::NativeStructure {
            profile: "nso0-v0-raw-lz4-v1".into(),
            location: view.files["exefs/main"].source_path.clone(),
        },
    };
    if view.scope.build_id.value.is_none() {
        view.scope.build_id.origin = Origin::Unknown;
    }
    Ok(())
}
