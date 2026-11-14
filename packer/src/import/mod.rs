//! Explicit complete-content import. Native NCA and extracted views feed the
//! existing writer; facts never masquerade as authenticated/display metadata.
mod convert;
mod dump;
pub use dump::{is_program_dump_component, plan_program_dump, ProgramDumpPlan};
mod manifest;
mod nup;
mod secure;
pub(crate) use secure::Root as SourceRoot;
pub mod structures;
pub mod synthetic;
pub mod synthetic_nup;
#[cfg(test)]
mod tests;
mod tree;
use crate::{CancellationToken, Platform, SnapshotType, WriterError, WriterErrorKind};
pub use convert::{
    add_extracted, convert_extracted, verify_extracted_sources, ImportReceipt, TreeAdditionStats,
    TreeConversionStats,
};
pub(crate) use convert::{convert_extracted_external, External, PackageMetadata, RestoredMetadata};
pub use manifest::{
    addon_belongs_to, Composition, DeclarationManifest, ManifestSelection, SnapshotDeclaration,
};
pub use nup::{
    check_nup, convert_nup, inspect_nup, stage_nup, stage_nup_with_options, NupCheck,
    NupConversionStats, NupInfo, NupOptions, NupPackageInfo, NupStage, SkippedAddon,
};
use std::path::PathBuf;
pub use tree::{
    audit_executables, discover, inspect_tree, ExecutableAudit, SourceFile, TreeCandidate, TreeView,
};

/// Independent declaration sidecar written by full filesystem extraction.
pub const EXTRACTION_MANIFEST: &str = "extracted.nximport";

/// Read explicit associations without requiring a restoration recipe. The
/// source root pins the directory and rejects symlinks/path traversal.
pub fn inspect_extraction_manifest(
    directory: impl AsRef<std::path::Path>,
    limits: &ImportLimits,
) -> Result<DeclarationManifest, WriterError> {
    let root = SourceRoot::open(directory.as_ref())?;
    DeclarationManifest::read(root.open_relative(EXTRACTION_MANIFEST)?, limits)
}

#[derive(Clone, Debug)]
pub struct ImportLimits {
    pub max_manifest_bytes: u64,
    pub max_snapshots: usize,
    pub max_nodes: usize,
    pub max_path_bytes: usize,
    pub max_depth: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_segment_bytes: u64,
    pub max_image_bytes: u64,
    pub max_executable_bytes: u64,
    pub max_inventory_bytes: u64,
    /// Re-read every source byte after writing and compare it with the
    /// archive (`--paranoid`). Off by default: the writer's streamed digests
    /// and output verification already cover the copied bytes.
    pub compare_source_bytes: bool,
}
impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_manifest_bytes: 1024 * 1024,
            max_snapshots: 1024,
            max_nodes: 100_000,
            max_path_bytes: 4096,
            max_depth: 64,
            max_file_bytes: 1 << 40,
            max_total_bytes: 4 << 40,
            max_segment_bytes: 256 * 1024 * 1024,
            max_image_bytes: 512 * 1024 * 1024,
            max_executable_bytes: 4 * 1024 * 1024 * 1024,
            max_inventory_bytes: 64 * 1024 * 1024,
            compare_source_bytes: false,
        }
    }
}
pub(crate) fn invalid(message: &str) -> WriterError {
    WriterError::new(WriterErrorKind::InvalidInput, message)
}
impl ImportLimits {
    pub const MAX_NODES: usize = 1_000_000;
    pub const MAX_INVENTORY_BYTES: u64 = 1024 * 1024 * 1024;

    /// NUP inspection and conversion share the same bounded inventory profile.
    pub fn for_nup() -> Self {
        Self {
            max_nodes: Self::MAX_NODES,
            max_inventory_bytes: Self::MAX_INVENTORY_BYTES,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), WriterError> {
        if self.max_manifest_bytes == 0
            || self.max_manifest_bytes > 16 * 1024 * 1024
            || self.max_snapshots == 0
            || self.max_snapshots > 4096
            || self.max_nodes == 0
            || self.max_nodes > Self::MAX_NODES
            || self.max_depth == 0
            || self.max_depth > 64
            || self.max_path_bytes == 0
            || self.max_path_bytes > 4096
            || self.max_inventory_bytes < 1024 * 1024
            || self.max_inventory_bytes > Self::MAX_INVENTORY_BYTES
            || self.max_file_bytes > i64::MAX as u64
            || self.max_total_bytes > i64::MAX as u64
            || self.max_segment_bytes > 1024 * 1024 * 1024
            || self.max_image_bytes > u32::MAX as u64
            || self.max_executable_bytes > i64::MAX as u64
        {
            return Err(limit("unsupported import resource limits"));
        }
        Ok(())
    }
}
pub(crate) fn unsupported(message: &str) -> WriterError {
    invalid(&format!("unsupported import profile: {message}"))
}
pub(crate) fn limit(message: &str) -> WriterError {
    WriterError::new(WriterErrorKind::ResourceLimit, message)
}
pub(crate) fn cancelled(token: &CancellationToken) -> Result<(), WriterError> {
    if token.is_cancelled() {
        Err(WriterError::new(
            WriterErrorKind::Cancelled,
            "import cancelled",
        ))
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Origin {
    Unknown,
    UserDeclaration {
        manifest_sha256: [u8; 32],
        field: String,
    },
    NativeStructure {
        profile: String,
        location: String,
    },
    Derived {
        exact_base_identity: [u8; 32],
        location: String,
    },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Evidence<T> {
    pub value: Option<T>,
    pub origin: Origin,
}
impl<T> Evidence<T> {
    pub fn unknown() -> Self {
        Self {
            value: None,
            origin: Origin::Unknown,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentLayout {
    NcaUnits,
    ExtractedExeFsRomFs,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    NativeNca,
    ExtractedTree,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Completeness {
    DeclaredComplete,
    UnresolvedPartial,
    ComposedExactBase {
        identity: [u8; 32],
        contract: &'static str,
    },
}

/// Common scope, independent of storage and platform generation heuristics.
#[derive(Clone, Debug)]
pub struct ContentScope {
    pub source_kind: SourceKind,
    pub layout: ContentLayout,
    pub platform: Evidence<Platform>,
    pub application_id: Evidence<u64>,
    pub program_id: Evidence<u64>,
    pub snapshot_type: Evidence<SnapshotType>,
    pub numeric_version: Evidence<u32>,
    pub build_id: Evidence<[u8; 32]>,
}

/// Adapter preserves native planning/Control/dependency semantics by delegating
/// their writer mapping, rather than introducing another archive engine.
pub enum CompleteContent<'a> {
    NativeNca(&'a crate::native::PlannedSnapshot),
    ExtractedTree(&'a TreeView),
}
impl CompleteContent<'_> {
    pub fn layout(&self) -> ContentLayout {
        match self {
            Self::NativeNca(_) => ContentLayout::NcaUnits,
            Self::ExtractedTree(_) => ContentLayout::ExtractedExeFsRomFs,
        }
    }
    pub fn source_kind(&self) -> SourceKind {
        match self {
            Self::NativeNca(_) => SourceKind::NativeNca,
            Self::ExtractedTree(_) => SourceKind::ExtractedTree,
        }
    }
    pub fn files(&self) -> Vec<SourceFile> {
        match self {
            Self::NativeNca(s) => s
                .files
                .iter()
                .map(|f| {
                    SourceFile::native_extent(f.name.clone(), f.source.clone(), f.offset, f.size)
                })
                .collect(),
            Self::ExtractedTree(t) => t.files.values().cloned().collect(),
        }
    }
    pub fn to_snapshot_input(
        &self,
        native_dependency: Option<[u8; 32]>,
    ) -> Result<crate::SnapshotInput, WriterError> {
        match self {
            Self::NativeNca(s) => Ok(s.to_snapshot_input(native_dependency)?),
            Self::ExtractedTree(t) => {
                if !t.declaration.dependencies.is_empty() || t.declaration.composition.is_some() {
                    return Err(invalid(
                        "relationships require conversion-time exact identity resolution",
                    ));
                }
                t.writer_snapshot(Vec::new())
            }
        }
    }
}

pub(crate) fn portable_path(path: &str, limits: &ImportLimits) -> Result<(), WriterError> {
    if path.is_empty()
        || path.len() > limits.max_path_bytes
        || path.split('/').count() > limits.max_depth
    {
        return Err(invalid("empty, long or deep import path"));
    }
    for part in path.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.len() > 240
            || !part.is_ascii()
            || part.ends_with('.')
            || part.ends_with(' ')
            || part
                .bytes()
                .any(|b| b < 32 || b == 127 || b"\\:<>\"|?*".contains(&b))
        {
            return Err(unsupported("unsafe or non-ASCII portable path"));
        }
        let stem = part.split('.').next().unwrap().to_ascii_uppercase();
        if ["CON", "PRN", "AUX", "NUL", "CLOCK$"].contains(&stem.as_str())
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            return Err(invalid("Windows reserved path component"));
        }
    }
    Ok(())
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    const H: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(H[(b >> 4) as usize] as char);
        out.push(H[(b & 15) as usize] as char);
    }
    out
}
pub(crate) fn unhex<const N: usize>(s: &str) -> Result<[u8; N], WriterError> {
    if s.len() != N * 2 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("invalid hexadecimal identity"));
    }
    let mut out = [0; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| invalid("invalid hex"))?;
    }
    Ok(out)
}
pub(crate) fn join(root: &std::path::Path, path: &str) -> PathBuf {
    path.split('/').fold(root.to_owned(), |p, c| p.join(c))
}
