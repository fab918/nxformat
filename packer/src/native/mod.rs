//! Native inventory and complete snapshot planning (Phase 2B / Increment 2).
//!
//! Recognizes decrypted DNSP (PFS0) and DXCI (gamecard HFS0) containers by
//! their internal markers, validates every NCA as a `DNCA` plaintext unit,
//! reads CNMT content meta, Control `control.nacp` and the ExeFS Build ID,
//! and associates base, update and DLC records by their native IDs. It never
//! decrypts or uses file/directory names as identity. Inventory never writes;
//! [`crate::conversion`] streams the validated native files through the writer.
//!
//! The result is an [`Inventory`] for reporting plus an [`ImportPlan`] whose
//! snapshots map onto [`crate::ApplicationInput`] once the caller resolves the
//! dependency identities that only exist after the base has been packed.

pub(crate) mod container;
mod source_partition;
pub(crate) use source_partition::{source_resources, update_partition};
mod json;
mod nca;
pub(crate) use nca::parse_nacp_text;
pub mod synthetic;
mod tree;

use crate::model::{
    ApplicationInput, DependencyInput, DependencyKind, DirectoryInput, EntryInput, FileInput,
    Platform, ProgramInput, ProgramKind, SnapshotInput, SnapshotType,
};
use container::{parse_partition, u64_at, PartitionEntry, Source};
use nca::{header_marker, parse_nca, NcaRejection, ParsedNca, NCA_HEADER_SIZE};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How much native data the inventory authenticates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InventoryMode {
    /// Container tables, NCA headers and their FsHeader SHA-256 hashes, CNMT
    /// and Control metadata. Payload hash trees are not read.
    Fast,
    /// Fast plus the HierarchicalSha256/IVFC hash trees of every section the
    /// inventory reads metadata from (Meta, Control and ExeFS). This proves
    /// those sections are really plaintext. Program RomFS and Data payloads are
    /// still not hashed.
    Strict,
}

#[derive(Clone, Debug)]
pub struct InventoryOptions {
    pub mode: InventoryMode,
    pub max_partition_entries: u32,
    pub max_string_table_bytes: u64,
    /// Upper bound for one CNMT or RomFS file table.
    pub max_meta_bytes: u64,
    /// Strict mode skips (and reports) sections larger than this.
    pub max_strict_section_bytes: u64,
    pub max_sources: usize,
    pub max_directory_depth: u32,
}

impl Default for InventoryOptions {
    fn default() -> Self {
        Self {
            mode: InventoryMode::Fast,
            max_partition_entries: 4096,
            max_string_table_bytes: 1024 * 1024,
            max_meta_bytes: 4 * 1024 * 1024,
            max_strict_section_bytes: 1024 * 1024 * 1024,
            max_sources: 100_000,
            max_directory_depth: 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeErrorKind {
    Io,
    Truncated,
    Invalid,
    Unsupported,
    ResourceLimit,
}

#[derive(Debug)]
pub struct NativeError {
    pub kind: NativeErrorKind,
    pub message: String,
}

impl NativeError {
    fn new(kind: NativeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub(crate) fn truncated(m: impl Into<String>) -> Self {
        Self::new(NativeErrorKind::Truncated, m)
    }
    pub(crate) fn invalid(m: impl Into<String>) -> Self {
        Self::new(NativeErrorKind::Invalid, m)
    }
    pub(crate) fn unsupported(m: impl Into<String>) -> Self {
        Self::new(NativeErrorKind::Unsupported, m)
    }
    pub(crate) fn limit(m: impl Into<String>) -> Self {
        Self::new(NativeErrorKind::ResourceLimit, m)
    }
}

impl From<io::Error> for NativeError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Self::truncated(e.to_string())
        } else {
            Self::new(NativeErrorKind::Io, e.to_string())
        }
    }
}

impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for NativeError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssueCode {
    Unrecognized,
    NotDecrypted,
    EncryptedSection,
    CompressedUnsupported,
    StandaloneNcaUnsupported,
    ExtractedTreeUnsupported,
    Switch2Unsupported,
    UnknownGeneration,
    UnsupportedMetaType,
    Truncated,
    InvalidStructure,
    ResourceLimit,
    Io,
    MissingContent,
    ContentMismatch,
    UnreferencedContent,
    DuplicateContent,
    AmbiguousVersion,
    MissingBase,
    MissingApplication,
    VerificationFailed,
    VerificationSkipped,
    MetadataUnavailable,
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub severity: Severity,
    pub code: IssueCode,
    pub message: String,
    /// Index into [`Inventory::sources`].
    pub source: Option<usize>,
    pub title_id: Option<u64>,
}

impl Issue {
    fn new(severity: Severity, code: IssueCode, message: impl Into<String>) -> Self {
        Self {
            severity,
            code,
            message: message.into(),
            source: None,
            title_id: None,
        }
    }
    fn from_error(e: &NativeError, context: &str) -> Self {
        let code = match e.kind {
            NativeErrorKind::Io => IssueCode::Io,
            NativeErrorKind::Truncated => IssueCode::Truncated,
            NativeErrorKind::Invalid => IssueCode::InvalidStructure,
            NativeErrorKind::Unsupported => IssueCode::MetadataUnavailable,
            NativeErrorKind::ResourceLimit => IssueCode::ResourceLimit,
        };
        Self::new(Severity::Error, code, format!("{context}: {}", e.message))
    }
    fn title(mut self, title_id: u64) -> Self {
        self.title_id = Some(title_id);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceFormat {
    /// PFS0 package whose NCAs all carry the DNCA marker.
    Dnsp,
    /// Gamecard image with the DXCI header marker.
    Dxci,
    /// PFS0 package or gamecard whose NCAs are not marked decrypted.
    EncryptedPackage,
    /// PFS0 package containing NCZ-compressed units.
    CompressedPackage,
    StandaloneNca,
    ExtractedTree,
    Unrecognized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceStatus {
    /// At least one content meta can be planned.
    Supported,
    /// Recognized, but this increment cannot plan it (reason in issues).
    Unsupported,
    /// Structurally invalid, encrypted or truncated.
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NcaContentType {
    Program,
    Meta,
    Control,
    Manual,
    Data,
    PublicData,
}

impl NcaContentType {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Program,
            1 => Self::Meta,
            2 => Self::Control,
            3 => Self::Manual,
            4 => Self::Data,
            5 => Self::PublicData,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct NcaSection {
    pub index: u8,
    /// Byte range relative to the NCA start.
    pub start: u64,
    pub end: u64,
    pub fs_type: u8,
    pub hash_type: u8,
    pub encryption_type: u8,
    /// The section has a BKTR patch table (update RomFS over a base).
    pub has_patch_info: bool,
    /// The decrypter flattened a sparse section (marker `DSPF` at FS header
    /// 0x1D0). Nintendo ships some bases sparse: only the regions their
    /// update reuses are present, the holes hold no data, and the content is
    /// only read through that update. Its hashes do not cover the holes.
    pub sparse_flattened: bool,
}

#[derive(Clone, Debug)]
pub struct NcaSummary {
    pub marker: &'static str,
    pub content_type: NcaContentType,
    pub distribution: u8,
    pub program_id: u64,
    pub content_index: u32,
    pub sdk_addon_version: u32,
    pub key_generation: u8,
    pub rights_id: [u8; 16],
    pub sections: Vec<NcaSection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnitKind {
    Nca,
    CompressedNca,
    /// Ticket, certificate, XML, image or other non-NCA package entry.
    Auxiliary,
}

#[derive(Clone, Debug)]
pub struct ContentUnit {
    /// Native package entry name.
    pub name: String,
    /// Gamecard partition (`secure`, `normal`, ...), absent for DNSP.
    pub partition: Option<String>,
    pub offset: u64,
    pub size: u64,
    /// Content ID parsed from the native 32-hex-digit entry name.
    pub content_id: Option<[u8; 16]>,
    pub kind: UnitKind,
    pub nca: Option<NcaSummary>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentMetaType {
    Application,
    Patch,
    AddOnContent,
    Delta,
    DataPatch,
    Other(u8),
}

impl ContentMetaType {
    fn from_u8(v: u8) -> Self {
        match v {
            0x80 => Self::Application,
            0x81 => Self::Patch,
            0x82 => Self::AddOnContent,
            0x83 => Self::Delta,
            0x84 => Self::DataPatch,
            other => Self::Other(other),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentRecordType {
    Meta,
    Program,
    Data,
    Control,
    HtmlDocument,
    LegalInformation,
    DeltaFragment,
    Other(u8),
}

impl ContentRecordType {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Meta,
            1 => Self::Program,
            2 => Self::Data,
            3 => Self::Control,
            4 => Self::HtmlDocument,
            5 => Self::LegalInformation,
            6 => Self::DeltaFragment,
            other => Self::Other(other),
        }
    }
    fn matches(self, nca: NcaContentType) -> bool {
        use NcaContentType as N;
        matches!(
            (self, nca),
            (Self::Meta, N::Meta)
                | (Self::Program, N::Program)
                | (Self::Control, N::Control)
                | (Self::HtmlDocument | Self::LegalInformation, N::Manual)
                | (Self::Data | Self::DeltaFragment, N::Data | N::PublicData)
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentRecord {
    pub hash: [u8; 32],
    pub content_id: [u8; 16],
    pub size: u64,
    pub record_type: ContentRecordType,
    pub id_offset: u8,
}

/// CNMT `ContentMetaPlatform` (offset 0x0D, 17.0.0+; reserved zero before).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeGeneration {
    /// Value 0, documented as `Nx`.
    Switch1,
    /// Value 1, documented as `Ounce` ([S2]). Recognized but not supported.
    Switch2,
    UnknownMarker(u8),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerificationState {
    NotChecked,
    Verified,
    NotApplicable,
    SkippedSizeLimit,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct ControlTitle {
    pub language: String,
    pub name: Option<String>,
    pub publisher: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ControlMetadata {
    pub display_version: Option<String>,
    pub titles: Vec<ControlTitle>,
    /// `(language, size)` of each root `icon_{Language}.dat`.
    pub icons: Vec<(String, u64)>,
    /// Invalid text fields or unsupported title-table encodings left unprojected.
    pub invalid_strings: Vec<String>,
    /// Exact source bytes, including every localized publisher.
    pub nacp_bytes: Vec<u8>,
    pub nacp_digest: [u8; 32],
    /// Validated native icon names and bounded absolute source ranges.
    pub icon_ranges: Vec<(String, u64, u64, crate::MediaType)>,
}

/// Verified per-Program native facts, including non-primary CNMT records.
#[derive(Clone, Debug)]
pub struct NativeProgram {
    pub content_id: [u8; 16],
    pub program_id: u64,
    pub id_offset: u8,
    pub build_id: Option<[u8; 32]>,
    /// The ExeFS is a flattened sparse section: the executable is only in the
    /// update, so the base carries no Build ID (see `NcaSection::sparse_flattened`).
    pub sparse: bool,
}

#[derive(Clone, Debug)]
pub struct NativeControl {
    pub program_id: u64,
    pub id_offset: u8,
    pub metadata: ControlMetadata,
}

#[derive(Clone, Debug)]
pub struct ContentMeta {
    pub title_id: u64,
    pub version: u32,
    pub meta_type: ContentMetaType,
    pub platform_byte: u8,
    pub generation: NativeGeneration,
    /// Owning application: the title itself for Application, the extended
    /// header's ApplicationId for Patch and AddOnContent.
    pub application_id: u64,
    pub patch_id: Option<u64>,
    pub required_application_version: Option<u32>,
    pub required_system_version: Option<u32>,
    pub contents: Vec<ContentRecord>,
    pub meta_content_id: Option<[u8; 16]>,
    pub control: Option<ControlMetadata>,
    pub controls: Vec<NativeControl>,
    pub build_id: Option<[u8; 32]>,
    /// Program IDs from the Program NCA headers referenced by this meta.
    pub program_ids: Vec<u64>,
    pub programs: Vec<NativeProgram>,
    /// A referenced Program NCA carries BKTR patch tables.
    pub requires_base_romfs: bool,
    pub verification: VerificationState,
}

/// Per-meta resolution of CNMT records to package units.
#[derive(Clone, Debug)]
pub struct ResolvedMeta {
    pub meta: ContentMeta,
    pub meta_unit: usize,
    /// Units required by the meta, meta unit first.
    pub units: Vec<usize>,
    pub plannable: bool,
}

#[derive(Clone, Debug)]
pub struct TreeSummary {
    /// ACI0 program ID from `exefs/main.npdm`; absent for RomFS-only trees.
    pub program_id: Option<u64>,
    pub npdm_name: Option<String>,
    pub npdm_flags: Option<u8>,
    pub main_build_id: Option<[u8; 32]>,
    pub exefs_files: Vec<String>,
    pub nso_build_ids: Vec<(String, [u8; 32])>,
    pub has_romfs: bool,
}

#[derive(Clone, Debug)]
pub struct SourceReport {
    pub path: PathBuf,
    pub size: u64,
    pub format: SourceFormat,
    pub status: SourceStatus,
    pub units: Vec<ContentUnit>,
    pub metas: Vec<ResolvedMeta>,
    pub tree: Option<TreeSummary>,
    /// Size of the nonempty gamecard system-update partition, including its HFS0 metadata.
    pub system_update_bytes: Option<u64>,
    pub issues: Vec<Issue>,
    pub bytes_read: u64,
}

#[derive(Clone, Debug)]
pub struct MetaRef {
    pub source: usize,
    pub meta: usize,
    pub title_id: u64,
    pub version: u32,
}

#[derive(Clone, Debug)]
pub struct InventoryApplication {
    pub platform: Platform,
    pub application_id: u64,
    pub bases: Vec<MetaRef>,
    pub updates: Vec<MetaRef>,
    pub dlc: Vec<MetaRef>,
}

#[derive(Clone, Debug, Default)]
pub struct InventoryStats {
    pub elapsed: Duration,
    pub files_examined: u64,
    pub files_ignored: u64,
    pub bytes_read: u64,
}

#[derive(Clone, Debug)]
pub struct Inventory {
    pub sources: Vec<SourceReport>,
    pub applications: Vec<InventoryApplication>,
    /// Cross-source issues (duplicates, missing bases, ...).
    pub issues: Vec<Issue>,
    pub stats: InventoryStats,
}

fn parse_content_id(name: &str) -> Option<[u8; 16]> {
    let hex = name.get(..32)?;
    let rest = &name[32..];
    if !(rest == ".nca" || rest == ".cnmt.nca" || rest == ".ncz" || rest == ".cnmt.ncz") {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(id)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Inspect one file or extracted-tree directory.
pub fn inspect_source(path: &Path, options: &InventoryOptions) -> SourceReport {
    inspect_source_inner(path, options, None)
}

pub(crate) fn inspect_source_bounded(
    path: &Path,
    options: &InventoryOptions,
    budget: std::rc::Rc<std::cell::Cell<u64>>,
) -> SourceReport {
    inspect_source_inner(path, options, Some(budget))
}

fn inspect_source_inner(
    path: &Path,
    options: &InventoryOptions,
    metadata_budget: Option<std::rc::Rc<std::cell::Cell<u64>>>,
) -> SourceReport {
    let mut report = SourceReport {
        path: path.to_owned(),
        size: 0,
        format: SourceFormat::Unrecognized,
        status: SourceStatus::Rejected,
        units: Vec::new(),
        metas: Vec::new(),
        tree: None,
        system_update_bytes: None,
        issues: Vec::new(),
        bytes_read: 0,
    };
    if path.is_dir() {
        if tree::is_extracted_tree(path) {
            report.format = SourceFormat::ExtractedTree;
            match tree::summarize_tree(path) {
                Ok(summary) => {
                    report.status = SourceStatus::Unsupported;
                    report.issues.push(Issue::new(
                        Severity::Error,
                        IssueCode::ExtractedTreeUnsupported,
                        "extracted ExeFS/RomFS tree has no CNMT or Control metadata: \
                         snapshot type, numeric version and Switch generation are not \
                         established natively; directory names are not authoritative",
                    ));
                    report.tree = Some(summary);
                }
                Err(e) => report.issues.push(Issue::from_error(&e, "extracted tree")),
            }
        }
        return report;
    }
    let result = crate::source_file::open(path)
        .and_then(|f| f.len().map(|len| (f, len)))
        .map_err(NativeError::from)
        .and_then(|(file, len)| {
            report.size = len;
            let mut src = Source::new(file, len);
            if let Some(budget) = metadata_budget {
                src = src.with_metadata_budget(budget);
            }
            let r = inspect_file(&mut src, &mut report, options);
            report.bytes_read = src.bytes_read;
            r
        });
    if let Err(e) = result {
        report.status = SourceStatus::Rejected;
        report.issues.push(Issue::from_error(&e, "source"));
    }
    report
}

/// Whether two package entries hold the same bytes.
fn same_unit_bytes(
    src: &mut Source,
    a: &ContentUnit,
    b: &ContentUnit,
) -> Result<bool, NativeError> {
    if a.size != b.size {
        return Ok(false);
    }
    let mut x = vec![0u8; 65536];
    let mut y = vec![0u8; 65536];
    let mut done = 0u64;
    while done < a.size {
        let n = (a.size - done).min(x.len() as u64) as usize;
        src.read_exact_at(a.offset + done, &mut x[..n])?;
        src.read_exact_at(b.offset + done, &mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
        done += n as u64;
    }
    Ok(true)
}

fn inspect_file(
    src: &mut Source,
    report: &mut SourceReport,
    options: &InventoryOptions,
) -> Result<(), NativeError> {
    let mut head = vec![0u8; src.len.min(0x400) as usize];
    src.read_exact_at(0, &mut head)?;
    let entries: Vec<(Option<String>, PartitionEntry)>;
    if head.len() >= 16 && &head[..4] == b"PFS0" {
        entries = parse_partition(src, 0, src.len, false, options)?
            .into_iter()
            .map(|e| (None, e))
            .collect();
        report.format = SourceFormat::Dnsp;
    } else if head.len() >= 0x140 && &head[0x100..0x104] == b"DXCI" {
        report.format = SourceFormat::Dxci;
        // 0x138 is the root HFS0 header size; partitions follow it, so they
        // are bounded by the image size.
        let root_off = u64_at(&head, 0x130);
        let mut all = Vec::new();
        for part in parse_partition(src, root_off, src.len, true, options)? {
            let inner = parse_partition(src, part.offset, part.offset + part.size, true, options)
                .map_err(|e| {
                NativeError::new(e.kind, format!("partition {}: {}", part.name, e.message))
            })?;
            if part.name == "update" {
                if !inner.is_empty() {
                    report.system_update_bytes = Some(part.size);
                    report.issues.push(Issue::new(
                        Severity::Info,
                        IssueCode::UnreferencedContent,
                        format!(
                            "gamecard update partition holds {} system-update entries separate from game content",
                            inner.len()
                        ),
                    ));
                }
                continue;
            }
            all.extend(inner.into_iter().map(|e| (Some(part.name.clone()), e)));
        }
        entries = all;
    } else if head.len() >= 0x104 && &head[0x100..0x104] == b"HEAD" {
        report.format = SourceFormat::EncryptedPackage;
        report.issues.push(Issue::new(
            Severity::Error,
            IssueCode::NotDecrypted,
            "gamecard image has the original HEAD marker, not DXCI; decryption is out of scope",
        ));
        return Ok(());
    } else if head.len() >= 0x204 && header_marker(&head[..0x400.min(head.len())]).is_some() {
        report.format = SourceFormat::StandaloneNca;
        report.status = SourceStatus::Unsupported;
        report.issues.push(Issue::new(
            Severity::Error,
            IssueCode::StandaloneNcaUnsupported,
            "standalone NCA: owner, version and complete content set cannot be demonstrated \
             without its package; not planned in this increment",
        ));
        return Ok(());
    } else {
        report.issues.push(Issue::new(
            Severity::Info,
            IssueCode::Unrecognized,
            "no DNSP, DXCI or NCA marker",
        ));
        return Ok(());
    }

    inspect_entries(src, report, options, entries)
}

// The same native validation is used for container ranges and existing NX
// files. Transport offsets supply no native identity or completeness evidence.
pub(crate) fn inspect_entries(
    src: &mut Source,
    report: &mut SourceReport,
    options: &InventoryOptions,
    entries: Vec<(Option<String>, PartitionEntry)>,
) -> Result<(), NativeError> {
    if entries.len() > options.max_partition_entries as usize {
        return Err(NativeError::limit(
            "native file count exceeds configured limit",
        ));
    }
    // Classify package entries from their bytes.
    let mut parsed: Vec<Option<ParsedNca>> = Vec::with_capacity(entries.len());
    let mut not_decrypted = 0usize;
    let mut compressed = 0usize;
    for (partition, entry) in entries {
        let mut kind = UnitKind::Auxiliary;
        let mut nca = None;
        if entry.size >= NCA_HEADER_SIZE {
            let mut marker = [0u8; 4];
            src.read_exact_at(entry.offset + 0x200, &mut marker)?;
            let looks_nca = matches!(&marker, b"DNCA" | b"NCA3" | b"NCA2" | b"NCA0");
            let named_nca = entry.name.ends_with(".nca") || entry.name.ends_with(".ncz");
            if looks_nca || named_nca {
                let mut ncz = [0u8; 8];
                if entry.size >= 0x4008 {
                    src.read_exact_at(entry.offset + 0x4000, &mut ncz)?;
                }
                if &ncz == b"NCZSECTN" {
                    kind = UnitKind::CompressedNca;
                    compressed += 1;
                } else {
                    kind = UnitKind::Nca;
                    match parse_nca(src, entry.offset, entry.size) {
                        Ok(p) => nca = Some(p),
                        Err(NcaRejection::NotDecrypted(m)) => {
                            not_decrypted += 1;
                            report.issues.push(Issue::new(
                                Severity::Error,
                                IssueCode::NotDecrypted,
                                format!("{}: {m}", entry.name),
                            ));
                        }
                        Err(NcaRejection::EncryptedSection(m)) => {
                            not_decrypted += 1;
                            report.issues.push(Issue::new(
                                Severity::Error,
                                IssueCode::EncryptedSection,
                                format!("{}: {m}", entry.name),
                            ));
                        }
                        Err(NcaRejection::Other(e)) => {
                            report.issues.push(Issue::from_error(&e, &entry.name))
                        }
                    }
                }
            }
        }
        report.units.push(ContentUnit {
            content_id: parse_content_id(&entry.name),
            name: entry.name,
            partition,
            offset: entry.offset,
            size: entry.size,
            kind,
            nca: nca.as_ref().map(|p| p.summary.clone()),
        });
        parsed.push(nca);
    }
    if compressed != 0 {
        report.format = SourceFormat::CompressedPackage;
        report.status = SourceStatus::Unsupported;
        report.issues.push(Issue::new(
            Severity::Error,
            IssueCode::CompressedUnsupported,
            format!("{compressed} NCZ-compressed units; decompression is not implemented"),
        ));
        return Ok(());
    }
    if not_decrypted != 0 {
        report.format = SourceFormat::EncryptedPackage;
        return Ok(());
    }

    // Resolve each Meta NCA.
    // DXCI partitions have independent name tables. Never silently pick one
    // of several units with the same native filename.
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, unit) in report.units.iter().enumerate() {
        by_name.entry(unit.name.clone()).or_default().push(i);
    }
    // Early game cards also copy the control NCA and the CNMT into the
    // `normal` partition (New Super Mario Bros. U Deluxe). Byte-identical
    // copies are one unit: keep the `secure` one. Copies that differ stay
    // ambiguous and are refused below.
    let mut copy = vec![false; report.units.len()];
    for group in by_name.values_mut().filter(|g| g.len() > 1) {
        let keep = group
            .iter()
            .copied()
            .find(|&i| report.units[i].partition.as_deref() == Some("secure"))
            .unwrap_or(group[0]);
        let mut same = true;
        for &other in group.iter().filter(|&&i| i != keep) {
            if !same_unit_bytes(src, &report.units[keep], &report.units[other])? {
                same = false;
                break;
            }
        }
        if same {
            for &other in group.iter().filter(|&&i| i != keep) {
                copy[other] = true;
            }
            group.retain(|&i| i == keep);
        }
    }
    let mut referenced = copy.clone();
    for meta_unit in 0..report.units.len() {
        src.check_metadata_budget()?;
        if copy[meta_unit] {
            continue;
        }
        let Some(nca) = &parsed[meta_unit] else {
            continue;
        };
        if nca.summary.content_type != NcaContentType::Meta {
            continue;
        }
        referenced[meta_unit] = true;
        let unit_name = report.units[meta_unit].name.clone();
        let mut meta = match nca::read_content_meta(src, nca, options) {
            Ok(m) => m,
            Err(e) => {
                report.issues.push(Issue::from_error(&e, &unit_name));
                continue;
            }
        };
        meta.meta_content_id = report.units[meta_unit].content_id;
        let mut plannable = true;
        let mut issues = Vec::new();
        let mut verification = Vec::new();
        if options.mode == InventoryMode::Strict {
            verification.push(verify_unit(src, nca, options));
        }
        match meta.generation {
            NativeGeneration::Switch1 => {}
            NativeGeneration::Switch2 => {
                plannable = false;
                issues.push(Issue::new(
                    Severity::Error,
                    IssueCode::Switch2Unsupported,
                    "CNMT platform is Ounce (Switch 2); Switch 2 association rules are not \
                     verified, so this content is reported but not planned",
                ));
            }
            NativeGeneration::UnknownMarker(v) => {
                plannable = false;
                issues.push(Issue::new(
                    Severity::Error,
                    IssueCode::UnknownGeneration,
                    format!("CNMT platform byte {v} is undocumented"),
                ));
            }
        }
        if !matches!(
            meta.meta_type,
            ContentMetaType::Application | ContentMetaType::Patch | ContentMetaType::AddOnContent
        ) {
            plannable = false;
            issues.push(Issue::new(
                Severity::Warning,
                IssueCode::UnsupportedMetaType,
                format!("content meta type {:?} is not imported", meta.meta_type),
            ));
        }
        if nca.summary.program_id != meta.title_id {
            issues.push(Issue::new(
                Severity::Warning,
                IssueCode::ContentMismatch,
                format!(
                    "Meta NCA program ID {:016x} differs from CNMT title ID {:016x}",
                    nca.summary.program_id, meta.title_id
                ),
            ));
        }
        let mut units = vec![meta_unit];
        let mut control_associations = std::collections::HashSet::new();
        for record in meta.contents.clone() {
            if record.record_type == ContentRecordType::Meta {
                continue;
            }
            let name = format!("{}.nca", hex(&record.content_id));
            let Some(candidates) = by_name.get(&name) else {
                if record.record_type == ContentRecordType::DeltaFragment {
                    issues.push(Issue::new(
                        Severity::Info,
                        IssueCode::MissingContent,
                        format!("delta fragment {name} absent (not needed to load)"),
                    ));
                } else {
                    plannable = false;
                    issues.push(Issue::new(
                        Severity::Error,
                        IssueCode::MissingContent,
                        format!(
                            "{:?} content {name} listed by CNMT is missing",
                            record.record_type
                        ),
                    ));
                }
                continue;
            };
            if candidates.len() != 1 {
                plannable = false;
                issues.push(Issue::new(
                    Severity::Error,
                    IssueCode::ContentMismatch,
                    format!("{name}: multiple units with this content ID across partitions"),
                ));
                continue;
            }
            let ui = candidates[0];
            referenced[ui] = true;
            let Some(unit) = &parsed[ui] else {
                plannable = false;
                issues.push(Issue::new(
                    Severity::Error,
                    IssueCode::InvalidStructure,
                    format!("{name} referenced by CNMT is not a valid DNCA unit"),
                ));
                continue;
            };
            if unit.summary.content_type == NcaContentType::Meta
                || !record.record_type.matches(unit.summary.content_type)
            {
                plannable = false;
                issues.push(Issue::new(
                    Severity::Error,
                    IssueCode::ContentMismatch,
                    format!(
                        "{name}: CNMT {:?} of {} bytes, unit is {:?} of {} bytes",
                        record.record_type,
                        record.size,
                        unit.summary.content_type,
                        report.units[ui].size
                    ),
                ));
                continue;
            }
            if record.size != report.units[ui].size {
                // The DNCA header agrees with its package entry (checked by
                // parse_nca); the CNMT still describes the original unit.
                issues.push(Issue::new(
                    Severity::Warning,
                    IssueCode::ContentMismatch,
                    format!(
                        "{name}: CNMT lists {} bytes, DNCA unit is {} bytes; the source tool rewrote this unit, so CNMT size and hash cannot authenticate it",
                        record.size,
                        report.units[ui].size
                    ),
                ));
            }
            if !units.contains(&ui) {
                units.push(ui);
            }
            // Ambiguity is a native association fact even if NACP is unreadable.
            if record.record_type == ContentRecordType::Control
                && !control_associations.insert((unit.summary.program_id, record.id_offset))
            {
                plannable = false;
                issues.push(Issue::new(
                    Severity::Error,
                    IssueCode::ContentMismatch,
                    "ambiguous Control NCA Program/offset association",
                ));
            }
            match record.record_type {
                ContentRecordType::Program => {
                    if !meta.program_ids.contains(&unit.summary.program_id) {
                        meta.program_ids.push(unit.summary.program_id);
                    }
                    if unit.summary.sections.iter().any(|s| s.has_patch_info) {
                        meta.requires_base_romfs = true;
                    }
                    // Inventory each executable and its explicit native association.
                    let sparse_exefs = unit
                        .summary
                        .sections
                        .iter()
                        .any(|s| s.fs_type == 1 && s.sparse_flattened);
                    let build_id = if sparse_exefs {
                        // The executable lives in the update: nothing to read here.
                        issues.push(Issue::new(
                            Severity::Warning,
                            IssueCode::MetadataUnavailable,
                            format!("{name}: the base executable is sparse; it runs only with its update"),
                        ));
                        None
                    } else {
                        match nca::read_build_id(src, unit, options) {
                            Ok(id) => id,
                            Err(e) => {
                                plannable = false;
                                issues.push(Issue::from_error(&e, &format!("{name} ExeFS")));
                                None
                            }
                        }
                    };
                    meta.programs.push(NativeProgram {
                        content_id: record.content_id,
                        program_id: unit.summary.program_id,
                        id_offset: record.id_offset,
                        build_id,
                        sparse: sparse_exefs,
                    });
                    if record.id_offset == 0 {
                        meta.build_id = build_id;
                    }
                }
                ContentRecordType::Control => match nca::read_control(src, unit, options) {
                    Ok(c) => {
                        for field in &c.invalid_strings {
                            issues.push(Issue::new(
                                Severity::Warning,
                                IssueCode::MetadataUnavailable,
                                if field == "unsupported TitlesDataFormat" {
                                    format!("{name}: unsupported NACP title table format; titles and publishers left absent, native bytes retained")
                                } else {
                                    format!("{name}: control.nacp {field} is not UTF-8; left absent")
                                },
                            ));
                        }
                        meta.controls.push(NativeControl {
                            program_id: unit.summary.program_id,
                            id_offset: record.id_offset,
                            metadata: c,
                        });
                    }
                    Err(e) => issues
                        .push(Issue::from_error(&e, &format!("{name} control.nacp")).warning()),
                },
                _ => {}
            }
            if options.mode == InventoryMode::Strict
                && matches!(
                    record.record_type,
                    ContentRecordType::Program | ContentRecordType::Control
                )
            {
                verification.push(verify_unit(src, unit, options));
            }
        }
        let primary = meta
            .programs
            .iter()
            .find(|p| p.id_offset == 0)
            .map_or(meta.application_id, |p| p.program_id);
        meta.control = meta
            .controls
            .iter()
            .find(|c| c.program_id == primary && c.id_offset == 0)
            .map(|c| c.metadata.clone());
        meta.verification = combine_verification(&verification, options.mode);
        match &meta.verification {
            VerificationState::Failed(m) => {
                plannable = false;
                issues.push(Issue::new(
                    Severity::Error,
                    IssueCode::VerificationFailed,
                    m.clone(),
                ));
            }
            VerificationState::SkippedSizeLimit => issues.push(Issue::new(
                Severity::Warning,
                IssueCode::VerificationSkipped,
                "a section exceeds max_strict_section_bytes and was not hashed",
            )),
            _ => {}
        }
        for issue in issues {
            report.issues.push(issue.title(meta.title_id));
        }
        report.metas.push(ResolvedMeta {
            meta,
            meta_unit,
            units,
            plannable,
        });
    }
    for (i, unit) in report.units.iter().enumerate() {
        if unit.kind == UnitKind::Nca && !referenced[i] && parsed[i].is_some() {
            report.issues.push(Issue::new(
                Severity::Warning,
                IssueCode::UnreferencedContent,
                format!("{} is not referenced by any CNMT", unit.name),
            ));
        }
    }
    src.check_metadata_budget()?;
    if report.metas.is_empty() {
        report.issues.push(Issue::new(
            Severity::Error,
            IssueCode::InvalidStructure,
            "package has no usable content meta",
        ));
        report.status = SourceStatus::Rejected;
    } else if report.metas.iter().any(|m| m.plannable) {
        report.status = SourceStatus::Supported;
    } else {
        report.status = SourceStatus::Unsupported;
    }
    Ok(())
}

impl Issue {
    fn warning(mut self) -> Self {
        self.severity = Severity::Warning;
        self
    }
}

/// Strict check of the sections the inventory read.
fn verify_unit(src: &mut Source, nca: &ParsedNca, options: &InventoryOptions) -> VerificationState {
    let mut states = Vec::new();
    for (i, s) in nca.summary.sections.iter().enumerate() {
        let state = match (nca.summary.content_type, s.fs_type, s.hash_type) {
            // Its hashes describe the full section, holes included.
            _ if s.sparse_flattened => Ok(VerificationState::NotApplicable),
            (NcaContentType::Meta | NcaContentType::Program, 1, 2) => {
                nca.verify_partition(src, i, options)
            }
            (NcaContentType::Control, 0, 3) => nca.verify_romfs(src, i, options),
            _ => Ok(VerificationState::NotApplicable),
        };
        states.push(state.unwrap_or_else(|e| VerificationState::Failed(e.message)));
    }
    combine_verification(&states, InventoryMode::Strict)
}

fn combine_verification(states: &[VerificationState], mode: InventoryMode) -> VerificationState {
    if mode == InventoryMode::Fast {
        return VerificationState::NotChecked;
    }
    if let Some(f) = states
        .iter()
        .find(|s| matches!(s, VerificationState::Failed(_)))
    {
        return f.clone();
    }
    if states.contains(&VerificationState::SkippedSizeLimit) {
        return VerificationState::SkippedSizeLimit;
    }
    if states.contains(&VerificationState::Verified) {
        VerificationState::Verified
    } else {
        VerificationState::NotApplicable
    }
}

fn platform_of(g: NativeGeneration) -> Platform {
    match g {
        NativeGeneration::Switch1 => Platform::Switch1,
        NativeGeneration::Switch2 => Platform::Switch2,
        NativeGeneration::UnknownMarker(_) => Platform::Unknown,
    }
}

/// Walk files and directories. Directories are scanned recursively in sorted
/// order; a directory with `exefs/main.npdm` or `romfs/` is reported as one
/// extracted tree and not descended into. Symbolic links are not followed.
pub fn inventory(roots: &[PathBuf], options: &InventoryOptions) -> io::Result<Inventory> {
    let started = Instant::now();
    let mut paths = Vec::new();
    let mut ignored = 0u64;
    for root in roots {
        collect(root, 0, options, &mut paths, &mut ignored)?;
    }
    let mut sources = Vec::with_capacity(paths.len());
    for path in &paths {
        sources.push(inspect_source(path, options));
    }
    let mut inv = associate(sources);
    inv.stats.elapsed = started.elapsed();
    inv.stats.files_examined = paths.len() as u64;
    inv.stats.files_ignored = ignored;
    inv.stats.bytes_read = inv.sources.iter().map(|s| s.bytes_read).sum();
    Ok(inv)
}

fn collect(
    path: &Path,
    depth: u32,
    options: &InventoryOptions,
    out: &mut Vec<PathBuf>,
    ignored: &mut u64,
) -> io::Result<()> {
    if crate::source_file::is_virtual(path) {
        out.push(path.to_owned());
        return Ok(());
    }
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        *ignored += 1;
        return Ok(());
    }
    if out.len() >= options.max_sources {
        return Err(io::Error::other("inventory exceeds max_sources"));
    }
    if meta.is_file() {
        if looks_like_source(path)? {
            out.push(path.to_owned());
        } else {
            *ignored += 1;
        }
        return Ok(());
    }
    if tree::is_extracted_tree(path) {
        out.push(path.to_owned());
        return Ok(());
    }
    if depth >= options.max_directory_depth {
        *ignored += 1;
        return Ok(());
    }
    let mut children: Vec<PathBuf> = fs::read_dir(path)?
        .map(|e| e.map(|e| e.path()))
        .collect::<io::Result<_>>()?;
    children.sort();
    for child in children {
        collect(&child, depth + 1, options, out, ignored)?;
    }
    Ok(())
}

/// Cheap sniff so unrelated files are not reported. Detection itself is done
/// again, fully, by [`inspect_source`].
fn looks_like_source(path: &Path) -> io::Result<bool> {
    let mut f = File::open(path)?;
    let mut head = [0u8; 0x204];
    let n = read_up_to(&mut f, &mut head)?;
    let head = &head[..n];
    Ok((n >= 4 && &head[..4] == b"PFS0")
        || (n >= 0x104 && matches!(&head[0x100..0x104], b"DXCI" | b"HEAD"))
        || (n >= 0x204 && matches!(&head[0x200..0x204], b"DNCA" | b"NCA3" | b"NCA2" | b"NCA0")))
}

fn read_up_to(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Compare the native units backing two candidate duplicate content metas.
/// This deliberately runs only for duplicate identities. CNMT content IDs,
/// sizes and hashes alone do not prove equality of rewritten DNCA bytes.
fn identical_native_units(
    left_source: &SourceReport,
    left_meta: &ResolvedMeta,
    right_source: &SourceReport,
    right_meta: &ResolvedMeta,
    cancellation: Option<&crate::CancellationToken>,
) -> io::Result<bool> {
    if left_meta.units.len() != right_meta.units.len() {
        return Ok(false);
    }

    let contextual = |source: &SourceReport, error: io::Error| {
        io::Error::new(error.kind(), format!("{}: {error}", source.path.display()))
    };
    let mut left_file =
        crate::source_file::open(&left_source.path).map_err(|e| contextual(left_source, e))?;
    let mut right_file =
        crate::source_file::open(&right_source.path).map_err(|e| contextual(right_source, e))?;
    let mut left_buf = [0u8; 64 * 1024];
    let mut right_buf = [0u8; 64 * 1024];

    for (&left_index, &right_index) in left_meta.units.iter().zip(&right_meta.units) {
        let Some(left) = left_source.units.get(left_index) else {
            return Ok(false);
        };
        let Some(right) = right_source.units.get(right_index) else {
            return Ok(false);
        };

        if left.name != right.name || left.kind != right.kind || left.size != right.size {
            return Ok(false);
        }

        if left_source.path == right_source.path && left.offset == right.offset {
            continue;
        }

        left_file
            .seek(SeekFrom::Start(left.offset))
            .map_err(|e| contextual(left_source, e))?;
        right_file
            .seek(SeekFrom::Start(right.offset))
            .map_err(|e| contextual(right_source, e))?;

        let mut remaining = left.size;
        while remaining != 0 {
            if cancellation.is_some_and(crate::CancellationToken::is_cancelled) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "duplicate comparison cancelled",
                ));
            }
            let n = remaining.min(left_buf.len() as u64) as usize;
            left_file
                .read_exact(&mut left_buf[..n])
                .map_err(|e| contextual(left_source, e))?;
            right_file
                .read_exact(&mut right_buf[..n])
                .map_err(|e| contextual(right_source, e))?;
            if left_buf[..n] != right_buf[..n] {
                return Ok(false);
            }
            remaining -= n as u64;
        }
    }
    Ok(true)
}

/// Group content metas by `(platform, application_id)` and report duplicates,
/// ambiguous versions and missing relationships.
pub fn associate(sources: Vec<SourceReport>) -> Inventory {
    associate_inner(sources, None)
}

// Preserve the public inventory API while allowing conversion's new payload IO
// to observe its existing cancellation token at each bounded read.
pub(crate) fn associate_with_cancellation(
    sources: Vec<SourceReport>,
    cancellation: &crate::CancellationToken,
) -> Inventory {
    associate_inner(sources, Some(cancellation))
}

fn associate_inner(
    sources: Vec<SourceReport>,
    cancellation: Option<&crate::CancellationToken>,
) -> Inventory {
    let mut issues = Vec::new();
    let mut apps: BTreeMap<(u8, u64), InventoryApplication> = BTreeMap::new();
    let mut seen: BTreeMap<(u8, u64, u32, u8), Vec<MetaRef>> = BTreeMap::new();
    for (si, s) in sources.iter().enumerate() {
        for (mi, m) in s.metas.iter().enumerate() {
            let meta = &m.meta;
            let platform = platform_of(meta.generation);
            let r = MetaRef {
                source: si,
                meta: mi,
                title_id: meta.title_id,
                version: meta.version,
            };
            let type_code = match meta.meta_type {
                ContentMetaType::Application => 0,
                ContentMetaType::Patch => 1,
                ContentMetaType::AddOnContent => 2,
                _ => continue,
            };
            seen.entry((type_code, meta.title_id, meta.version, platform as u8))
                .or_default()
                .push(r.clone());
            let app = apps
                .entry((platform as u8, meta.application_id))
                .or_insert_with(|| InventoryApplication {
                    platform,
                    application_id: meta.application_id,
                    bases: Vec::new(),
                    updates: Vec::new(),
                    dlc: Vec::new(),
                });
            match type_code {
                0 => app.bases.push(r),
                1 => app.updates.push(r),
                _ => app.dlc.push(r),
            }
        }
    }
    let meta_of = |r: &MetaRef| &sources[r.source].metas[r.meta];
    for refs in seen.values() {
        if refs.len() < 2 {
            continue;
        }
        let first = meta_of(&refs[0]);
        let matching_records = refs.iter().skip(1).all(|r| {
            let m = meta_of(r);
            m.meta.meta_content_id == first.meta.meta_content_id
                && m.meta.meta_type == first.meta.meta_type
                && m.meta.application_id == first.meta.application_id
                && m.meta.platform_byte == first.meta.platform_byte
                && m.meta.patch_id == first.meta.patch_id
                && m.meta.required_application_version == first.meta.required_application_version
                && m.meta.required_system_version == first.meta.required_system_version
                && m.meta.contents.len() == first.meta.contents.len()
                && m.meta
                    .contents
                    .iter()
                    .zip(&first.meta.contents)
                    .all(|(a, b)| {
                        a.content_id == b.content_id
                            && a.size == b.size
                            && a.hash == b.hash
                            && a.record_type == b.record_type
                            && a.id_offset == b.id_offset
                    })
        });
        // Check every declaration before opening payloads: a cheap mismatch in
        // a later candidate must not force a large earlier byte comparison.
        let mut comparison_error = None;
        let identical = matching_records
            && refs.iter().skip(1).all(|r| {
                match identical_native_units(
                    &sources[refs[0].source],
                    first,
                    &sources[r.source],
                    meta_of(r),
                    cancellation,
                ) {
                    Ok(equal) => equal,
                    Err(error) => {
                        comparison_error = Some((r.source, error.to_string()));
                        false
                    }
                }
            });
        let paths: Vec<String> = refs
            .iter()
            .map(|r| sources[r.source].path.display().to_string())
            .collect();
        let mut issue = if let Some((_, message)) = &comparison_error {
            Issue::new(
                Severity::Error,
                IssueCode::Io,
                format!("failed to compare duplicate native units: {message}"),
            )
        } else if identical {
            Issue::new(
                Severity::Warning,
                IssueCode::DuplicateContent,
                format!(
                    "identical content meta v{} found {} times: {}",
                    refs[0].version,
                    refs.len(),
                    paths.join(" | ")
                ),
            )
        } else {
            Issue::new(
                Severity::Error,
                IssueCode::AmbiguousVersion,
                format!(
                    "same title and version v{} with different contents: {}",
                    refs[0].version,
                    paths.join(" | ")
                ),
            )
        };
        issue.source = Some(
            comparison_error
                .as_ref()
                .map_or(refs[1].source, |(source, _)| *source),
        );
        issues.push(issue.title(refs[0].title_id));
    }
    for app in apps.values() {
        let mut base_versions: Vec<u32> = app.bases.iter().map(|r| r.version).collect();
        base_versions.sort_unstable();
        base_versions.dedup();
        if base_versions.len() > 1 {
            issues.push(
                Issue::new(
                    Severity::Warning,
                    IssueCode::AmbiguousVersion,
                    format!("several base versions present: {base_versions:?}"),
                )
                .title(app.application_id),
            );
        }
        for u in &app.updates {
            if app.bases.is_empty() {
                // Absence of BKTR alone cannot prove a standalone update
                // contains every native content unit needed by the loader.
                let mut i = Issue::new(
                    Severity::Error,
                    IssueCode::MissingBase,
                    format!(
                        "update v{} has no base application in the inventory",
                        u.version
                    ),
                );
                i.source = Some(u.source);
                issues.push(i.title(u.title_id));
            }
            for b in &app.bases {
                if let Some(pid) = meta_of(b).meta.patch_id {
                    if pid != u.title_id {
                        let mut i = Issue::new(
                            Severity::Warning,
                            IssueCode::ContentMismatch,
                            format!(
                                "base declares patch ID {pid:016x}, update title is {:016x}",
                                u.title_id
                            ),
                        );
                        i.source = Some(u.source);
                        issues.push(i.title(u.title_id));
                    }
                }
            }
        }
        let max_version = app
            .bases
            .iter()
            .chain(&app.updates)
            .map(|r| r.version)
            .max();
        for d in &app.dlc {
            let m = &meta_of(d).meta;
            let mut issue = None;
            if app.bases.is_empty() {
                issue = Some(Issue::new(
                    Severity::Warning,
                    IssueCode::MissingApplication,
                    format!(
                        "DLC {:016x} belongs to application {:016x}, which is not in the inventory",
                        d.title_id, app.application_id
                    ),
                ));
            } else if let (Some(req), Some(max)) = (m.required_application_version, max_version) {
                if req > max {
                    issue = Some(Issue::new(
                        Severity::Warning,
                        IssueCode::MissingBase,
                        format!("DLC requires application v{req}; newest present is v{max}"),
                    ));
                }
            }
            if let Some(mut i) = issue {
                i.source = Some(d.source);
                issues.push(i.title(d.title_id));
            }
        }
    }
    Inventory {
        sources,
        applications: apps.into_values().collect(),
        issues,
        stats: InventoryStats::default(),
    }
}

// ---------------------------------------------------------------------------
// Intermediate representation shared by inventory and conversion.

type ProgramSelection = (
    Option<u64>,
    Option<u8>,
    Option<[u8; 32]>,
    Option<SnapshotRef>,
);

/// Native reference to another planned snapshot. It becomes a 32-byte NX
/// snapshot identity only after that snapshot's file hashes are known.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotRef {
    pub platform: Platform,
    pub application_id: u64,
    pub snapshot_type: SnapshotType,
    pub title_id: u64,
    pub numeric_version: u32,
    pub program_id: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct PlannedFile {
    /// Native package entry name, kept as the logical file name.
    pub name: String,
    pub source: PathBuf,
    pub offset: u64,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct PlannedSnapshot {
    pub program_id_offset: Option<u8>,
    pub native_contents: Arc<[ContentRecord]>,
    pub snapshot_type: SnapshotType,
    pub title_id: u64,
    pub numeric_version: u32,
    pub program_id: Option<u64>,
    /// The Meta NCA content ID.
    pub content_id: Option<[u8; 16]>,
    pub build_id: Option<[u8; 32]>,
    pub display_version: Option<String>,
    pub requires: Option<SnapshotRef>,
    /// Immutable complete native view shared by Programs of the same version.
    pub files: Arc<[PlannedFile]>,
    pub control: Option<ControlMetadata>,
    pub metadata_source: PathBuf,
    pub required_firmware: Option<u32>,
}

impl PlannedSnapshot {
    pub fn reference(&self, platform: Platform, application_id: u64) -> SnapshotRef {
        SnapshotRef {
            platform,
            application_id,
            snapshot_type: self.snapshot_type,
            title_id: self.title_id,
            numeric_version: self.numeric_version,
            program_id: self.program_id,
        }
    }

    /// Build a writer snapshot that streams each file from its source range.
    /// `dependency` must be the packed identity of [`Self::requires`].
    pub fn to_snapshot_input(&self, dependency: Option<[u8; 32]>) -> io::Result<SnapshotInput> {
        let dependencies = match (&self.requires, dependency) {
            (None, _) => Vec::new(),
            (Some(_), Some(identity)) => vec![DependencyInput {
                kind: if self.snapshot_type == SnapshotType::Dlc {
                    DependencyKind::Requires
                } else {
                    DependencyKind::RequiredBase
                },
                content_id: None,
                target_snapshot_identity: identity,
            }],
            (Some(_), None) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "required native snapshot identity is not resolved",
                ))
            }
        };
        let mut entries = Vec::with_capacity(self.files.len());
        for f in self.files.iter() {
            let mut file = crate::source_file::open(&f.source)?;
            file.seek(SeekFrom::Start(f.offset))?;
            entries.push(EntryInput::file(
                f.name.clone(),
                FileInput::new(f.size, file.take(f.size)),
            ));
        }
        Ok(SnapshotInput {
            addon_title_id: None,
            native: crate::SnapshotNativeMetadata {
                npdm: None,
                required_firmware: self.required_firmware,
            },
            program_id: self.program_id,
            snapshot_type: self.snapshot_type,
            numeric_version: self.numeric_version,
            content_id: self.content_id,
            build_id: self.build_id,
            display_version: self.display_version.clone(),
            dependencies,
            metadata: self
                .control
                .as_ref()
                .map(|c| c.to_snapshot_metadata(&self.metadata_source))
                .transpose()?,
            root: DirectoryInput::new(entries),
        })
    }
}

#[derive(Clone, Debug)]
pub struct PlannedApplication {
    pub platform: Platform,
    pub application_id: u64,
    pub programs: Vec<(u64, ProgramKind)>,
    pub snapshots: Vec<PlannedSnapshot>,
}

impl PlannedApplication {
    /// Map selected snapshots onto writer input. `resolve` returns the packed
    /// identity of a required snapshot, typically from `inspect_archive`.
    pub fn to_application_input(
        &self,
        include: &dyn Fn(&PlannedSnapshot) -> bool,
        resolve: &dyn Fn(&SnapshotRef) -> Option<[u8; 32]>,
    ) -> io::Result<ApplicationInput> {
        let mut snapshots = Vec::new();
        for s in self.snapshots.iter().filter(|s| include(s)) {
            let dep = s.requires.as_ref().and_then(resolve);
            snapshots.push(s.to_snapshot_input(dep)?);
        }
        Ok(ApplicationInput {
            application_id: self.application_id,
            platform: self.platform,
            programs: self
                .programs
                .iter()
                .filter(|(program_id, _)| {
                    snapshots
                        .iter()
                        .any(|snapshot| snapshot.program_id == Some(*program_id))
                })
                .map(|&(program_id, kind)| ProgramInput {
                    program_id,
                    kind,
                    content_id: None,
                    npdm: None,
                })
                .collect(),
            snapshots,
        })
    }
}

#[derive(Clone, Debug)]
pub struct BlockedContent {
    pub source: PathBuf,
    pub title_id: u64,
    pub meta_type: ContentMetaType,
    pub version: u32,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ImportPlan {
    pub applications: Vec<PlannedApplication>,
    pub blocked: Vec<BlockedContent>,
}

impl BlockedContent {
    /// One line per blocked content: what it is and why, for error messages.
    pub fn describe(blocked: &[BlockedContent]) -> String {
        blocked
            .iter()
            .map(|b| {
                let name = b.source.file_name().unwrap_or(b.source.as_os_str());
                format!(
                    "{} ({:016x} v{}): {}",
                    name.to_string_lossy(),
                    b.title_id,
                    b.version,
                    b.reasons.join("; ")
                )
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

impl Inventory {
    pub fn error_count(&self) -> usize {
        self.all_issues()
            .filter(|i| i.severity == Severity::Error)
            .count()
    }

    pub fn all_issues(&self) -> impl Iterator<Item = &Issue> {
        self.sources
            .iter()
            .flat_map(|s| s.issues.iter())
            .chain(self.issues.iter())
    }

    pub fn to_json(&self) -> String {
        json::inventory_json(self)
    }

    /// Build the converter plan. Only Switch 1 Application/Patch/AddOnContent
    /// metas whose content set resolved completely are planned; everything
    /// else is listed in [`ImportPlan::blocked`] with its reasons.
    pub fn plan(&self) -> ImportPlan {
        self.plan_with_file_comparison(&mut |a, b| identical_planned_files(a, b, None))
    }

    pub(crate) fn plan_with_file_comparison(
        &self,
        compare: &mut dyn FnMut(&PlannedFile, &PlannedFile) -> io::Result<bool>,
    ) -> ImportPlan {
        let mut plan = ImportPlan::default();
        let issue_for = |source: usize, title: u64| -> Vec<String> {
            self.sources[source]
                .issues
                .iter()
                .chain(self.issues.iter().filter(|i| i.source == Some(source)))
                .filter(|i| i.severity == Severity::Error && i.title_id == Some(title))
                .map(|i| i.message.clone())
                .collect()
        };
        let ambiguous: Vec<u64> = self
            .issues
            .iter()
            // A failed duplicate comparison must block every candidate, including
            // the first copy, not only the source carrying the IO diagnostic.
            .filter(|i| {
                matches!(i.code, IssueCode::AmbiguousVersion | IssueCode::Io)
                    && i.severity == Severity::Error
            })
            .filter_map(|i| i.title_id)
            .collect();
        for app in &self.applications {
            let mut planned = PlannedApplication {
                platform: app.platform,
                application_id: app.application_id,
                programs: Vec::new(),
                snapshots: Vec::new(),
            };
            let mut taken: Vec<(u64, u32, SnapshotType)> = Vec::new();
            let block = |r: &MetaRef, reasons: Vec<String>, plan: &mut ImportPlan| {
                let m = &self.sources[r.source].metas[r.meta].meta;
                plan.blocked.push(BlockedContent {
                    source: self.sources[r.source].path.clone(),
                    title_id: m.title_id,
                    meta_type: m.meta_type,
                    version: m.version,
                    reasons,
                });
            };
            let base_versions: Vec<u32> = {
                let mut v: Vec<u32> = app.bases.iter().map(|r| r.version).collect();
                v.sort_unstable();
                v.dedup();
                v
            };
            let groups = [
                (&app.bases, SnapshotType::Base),
                (&app.updates, SnapshotType::Update),
                (&app.dlc, SnapshotType::Dlc),
            ];
            for (refs, snapshot_type) in groups {
                for r in refs {
                    let resolved = &self.sources[r.source].metas[r.meta];
                    let m = &resolved.meta;
                    let mut reasons = issue_for(r.source, m.title_id);
                    if !resolved.plannable && reasons.is_empty() {
                        reasons.push("content meta is not plannable".into());
                    }
                    let declared: std::collections::BTreeSet<_> =
                        m.program_ids.iter().copied().collect();
                    let observed: std::collections::BTreeSet<_> =
                        m.programs.iter().map(|p| p.program_id).collect();
                    if declared != observed {
                        reasons.push("Program ID inventory disagrees with native records".into());
                    }
                    let mut offsets = std::collections::BTreeSet::new();
                    let mut ids = std::collections::BTreeSet::new();
                    if snapshot_type == SnapshotType::Dlc && !m.programs.is_empty() {
                        reasons.push("executable DLC native associations are not validated".into());
                    }
                    for program in &m.programs {
                        if !offsets.insert(program.id_offset) || !ids.insert(program.program_id) {
                            reasons.push("ambiguous duplicate Program ID or ID offset".into());
                        }
                        // A sparse base has no executable of its own; its update brings it.
                        let sparse_base = program.sparse && snapshot_type == SnapshotType::Base;
                        if (program.build_id.is_none() && !sparse_base)
                            || program.build_id == Some([0; 32])
                        {
                            reasons.push(
                                "Program executable/Build ID is missing, zero or unsupported"
                                    .into(),
                            );
                        }
                    }
                    if app.platform != Platform::Switch1 {
                        reasons.push("only Switch 1 content is planned in this increment".into());
                    }
                    if ambiguous.contains(&m.title_id) {
                        reasons.push(
                            "duplicate title/version equivalence could not be established".into(),
                        );
                    }
                    let key = (m.title_id, m.version, snapshot_type);
                    if taken.contains(&key) {
                        // Identical duplicate already planned from an earlier source.
                        continue;
                    }
                    let mut requires = None;
                    let mut files = self.planned_files(r.source, &resolved.units);
                    if snapshot_type == SnapshotType::Update {
                        match base_versions.as_slice() {
                            [] => reasons.push(
                                "update without base: completeness cannot be established".into(),
                            ),
                            [_] => {
                                // Reuse exactly the base that was accepted in this
                                // plan, not a potentially rejected duplicate source.
                                if let Some(base) = planned
                                    .snapshots
                                    .iter()
                                    .find(|s| s.snapshot_type == SnapshotType::Base)
                                {
                                    let accepted = app.bases.iter().find_map(|r| {
                                        let source = &self.sources[r.source];
                                        let native = &source.metas[r.meta].meta;
                                        (source.path == base.metadata_source
                                            && native.meta_content_id == base.content_id
                                            && native.version == base.numeric_version)
                                            .then_some(native)
                                    });
                                    if accepted.and_then(|b| b.patch_id) != Some(m.title_id) {
                                        reasons.push(
                                            "update title disagrees with accepted Base Patch ID"
                                                .into(),
                                        );
                                    }
                                    for f in base.files.iter() {
                                        if let Some(index) =
                                            files.iter().position(|g| g.name == f.name)
                                        {
                                            let id = parse_content_id(&f.name);
                                            let left = base
                                                .native_contents
                                                .iter()
                                                .find(|c| Some(c.content_id) == id);
                                            let right = m
                                                .contents
                                                .iter()
                                                .find(|c| Some(c.content_id) == id);
                                            if left.is_none() || left != right {
                                                reasons.push(format!("conflicting base/update CNMT association for {}", f.name));
                                            } else {
                                                match compare(f, &files[index]) {
                                                    Ok(true) => files[index] = f.clone(),
                                                    Ok(false) => reasons.push(format!(
                                                        "conflicting native bytes for {}",
                                                        f.name
                                                    )),
                                                    Err(e) => reasons.push(format!(
                                                        "IO comparing shared native {}: {e}",
                                                        f.name
                                                    )),
                                                }
                                            }
                                        } else {
                                            files.push(f.clone());
                                        }
                                    }
                                    requires =
                                        Some(base.reference(app.platform, app.application_id));
                                } else {
                                    reasons.push("required base could not be planned".into());
                                }
                            }
                            _ => reasons.push(
                                "several base versions are present; the required base is ambiguous"
                                    .into(),
                            ),
                        }
                    }
                    if snapshot_type == SnapshotType::Dlc && !app.bases.is_empty() {
                        let minimum = m.required_application_version.unwrap_or(0);
                        if let Some(target) = planned
                            .snapshots
                            .iter()
                            .filter(|s| {
                                s.snapshot_type != SnapshotType::Dlc
                                    && s.program_id_offset == Some(0)
                                    && s.numeric_version >= minimum
                            })
                            .min_by_key(|s| (s.numeric_version, s.snapshot_type))
                        {
                            requires = Some(target.reference(app.platform, app.application_id));
                        } else {
                            reasons.push(format!("DLC requires application version {minimum}, absent from accepted snapshots"));
                        }
                    }
                    if !reasons.is_empty() {
                        block(r, reasons, &mut plan);
                        continue;
                    }
                    files.sort_by(|a, b| a.name.cmp(&b.name));
                    let bases: Vec<_> = planned
                        .snapshots
                        .iter()
                        .filter(|s| s.snapshot_type == SnapshotType::Base)
                        .cloned()
                        .collect();
                    let selections: Vec<ProgramSelection> = if snapshot_type == SnapshotType::Dlc {
                        vec![(None, None, None, requires)]
                    } else if snapshot_type == SnapshotType::Base {
                        if !m.programs.iter().any(|p| p.id_offset == 0) {
                            block(
                                r,
                                vec!["complete primary Program association is absent".into()],
                                &mut plan,
                            );
                            continue;
                        }
                        m.programs
                            .iter()
                            .map(|p| (Some(p.program_id), Some(p.id_offset), p.build_id, None))
                            .collect()
                    } else {
                        // An update may bring a Program the base does not have
                        // (Bayonetta 3 v1.2 adds Program 1). Such a new Program
                        // depends on the primary base, like the update itself.
                        let primary = bases.iter().find(|b| b.program_id_offset == Some(0));
                        if m.programs.iter().any(|p| {
                            let known = bases.iter().any(|b| {
                                b.program_id_offset == Some(p.id_offset)
                                    && b.program_id == Some(p.program_id)
                            });
                            let new = p.id_offset != 0
                                && primary.is_some()
                                && !bases.iter().any(|b| {
                                    b.program_id_offset == Some(p.id_offset)
                                        || b.program_id == Some(p.program_id)
                                });
                            !known && !new
                        }) {
                            block(r, vec!["update Program ID differs or its offset disagrees with accepted base association".into()], &mut plan);
                            continue;
                        }
                        let added = m
                            .programs
                            .iter()
                            .filter(|p| {
                                !bases
                                    .iter()
                                    .any(|b| b.program_id_offset == Some(p.id_offset))
                            })
                            .map(|p| {
                                (
                                    Some(p.program_id),
                                    Some(p.id_offset),
                                    p.build_id,
                                    primary.map(|b| b.reference(app.platform, app.application_id)),
                                )
                            });
                        bases
                            .iter()
                            .map(|b| {
                                let replacement = m
                                    .programs
                                    .iter()
                                    .find(|p| Some(p.id_offset) == b.program_id_offset);
                                (
                                    b.program_id,
                                    b.program_id_offset,
                                    replacement.and_then(|p| p.build_id).or(b.build_id),
                                    Some(b.reference(app.platform, app.application_id)),
                                )
                            })
                            .chain(added)
                            .collect()
                    };
                    if selections.is_empty() {
                        block(
                            r,
                            vec!["complete executable Program association is absent".into()],
                            &mut plan,
                        );
                        continue;
                    }
                    // Program kind is application-wide in the existing Catalog.
                    // Do not silently reinterpret a Program's primary role between Bases.
                    if selections.iter().any(|(id, offset, _, _)| {
                        planned.programs.iter().any(|(old_id, kind)| {
                            Some(*old_id) == *id
                                && (*kind == ProgramKind::Primary) != (*offset == Some(0))
                        })
                    }) {
                        block(r, vec!["changing primary Program roles across native Base versions is unsupported".into()], &mut plan);
                        continue;
                    }
                    taken.push(key);
                    let files: Arc<[PlannedFile]> = files.into();
                    let native_contents: Arc<[ContentRecord]> = m.contents.clone().into();
                    for (program_id, program_id_offset, build_id, requires) in selections {
                        if let Some(pid) = program_id {
                            let kind = if program_id_offset == Some(0) {
                                ProgramKind::Primary
                            } else {
                                ProgramKind::Auxiliary
                            };
                            if !planned.programs.iter().any(|p| p.0 == pid) {
                                planned.programs.push((pid, kind));
                            }
                        }
                        let control = m
                            .controls
                            .iter()
                            .find(|c| {
                                Some(c.program_id) == program_id
                                    && Some(c.id_offset) == program_id_offset
                            })
                            .map(|c| c.metadata.clone());
                        let display_version =
                            control.as_ref().and_then(|c| c.display_version.clone());
                        planned.snapshots.push(PlannedSnapshot {
                            program_id_offset,
                            native_contents: native_contents.clone(),
                            snapshot_type,
                            title_id: m.title_id,
                            numeric_version: m.version,
                            program_id,
                            content_id: m.meta_content_id,
                            build_id,
                            control,
                            metadata_source: self.sources[r.source].path.clone(),
                            required_firmware: m.required_system_version,
                            display_version,
                            requires,
                            files: files.clone(),
                        });
                    }
                }
            }
            if !planned.snapshots.is_empty() {
                planned.programs.sort_unstable_by_key(|p| p.0);
                plan.applications.push(planned);
            }
        }
        plan
    }

    fn planned_files(&self, source: usize, units: &[usize]) -> Vec<PlannedFile> {
        let s = &self.sources[source];
        units
            .iter()
            .map(|&u| PlannedFile {
                name: s.units[u].name.clone(),
                source: s.path.clone(),
                offset: s.units[u].offset,
                size: s.units[u].size,
            })
            .collect()
    }
}

/// Equality of already-resolved native ranges, used only for shared filenames.
pub(crate) fn identical_planned_files(
    a: &PlannedFile,
    b: &PlannedFile,
    cancellation: Option<&crate::CancellationToken>,
) -> io::Result<bool> {
    if a.size != b.size {
        return Ok(false);
    }
    if a.source == b.source && a.offset == b.offset {
        return Ok(true);
    }
    let mut left = crate::source_file::open(&a.source)?;
    let mut right = crate::source_file::open(&b.source)?;
    left.seek(SeekFrom::Start(a.offset))?;
    right.seek(SeekFrom::Start(b.offset))?;
    let mut x = [0u8; 65536];
    let mut y = [0u8; 65536];
    let mut remaining = a.size;
    while remaining != 0 {
        if cancellation.is_some_and(crate::CancellationToken::is_cancelled) {
            return Err(io::Error::other("shared native comparison cancelled"));
        }
        let n = remaining.min(x.len() as u64) as usize;
        left.read_exact(&mut x[..n])?;
        right.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
        remaining -= n as u64;
    }
    Ok(true)
}

/// Verified native NACP order -> NX BCP-47 language identifiers.
/// Sources and unsupported newer NACP layouts: docs/CLI.md.
pub const NACP_LANGUAGE_MAP: [(&str, &str); 16] = [
    ("AmericanEnglish", "en-us"),
    ("BritishEnglish", "en-gb"),
    ("Japanese", "ja"),
    ("French", "fr"),
    ("German", "de"),
    ("LatinAmericanSpanish", "es-419"),
    ("Spanish", "es"),
    ("Italian", "it"),
    ("Dutch", "nl"),
    ("CanadianFrench", "fr-ca"),
    ("Portuguese", "pt"),
    ("Russian", "ru"),
    ("Korean", "ko"),
    ("TraditionalChinese", "zh-hant"),
    ("SimplifiedChinese", "zh-hans"),
    ("BrazilianPortuguese", "pt-br"),
];

impl ControlMetadata {
    fn to_snapshot_metadata(&self, source: &Path) -> io::Result<crate::SnapshotMetadata> {
        let mut localized = Vec::new();
        for (native, language) in NACP_LANGUAGE_MAP {
            let title = self.titles.iter().find(|t| t.language == native);
            let icon = self.icon_ranges.iter().find(|(l, _, _, _)| l == native);
            let icon = if let Some((_, offset, size, kind)) = icon {
                let mut reader = crate::source_file::open(source)?;
                reader.seek(SeekFrom::Start(*offset))?;
                Some(crate::MediaInput::new(*kind, *size, reader.take(*size)))
            } else {
                None
            };
            let name = title.and_then(|t| t.name.clone());
            if name.is_some() || icon.is_some() {
                localized.push(crate::LocalizedMetadata {
                    language: language.into(),
                    title: name,
                    icon,
                });
            }
        }
        // Explicit display fallback: first populated native entry in NACP order.
        // All other titles/icons remain localized, all publishers remain in NACP.
        let default = self.titles.first();
        let icon = if let Some((_, offset, size, kind)) = default.and_then(|t| {
            self.icon_ranges
                .iter()
                .find(|(l, _, _, _)| l == &t.language)
        }) {
            let mut reader = crate::source_file::open(source)?;
            reader.seek(SeekFrom::Start(*offset))?;
            Some(crate::MediaInput::new(*kind, *size, reader.take(*size)))
        } else {
            None
        };
        Ok(crate::SnapshotMetadata {
            title: default.and_then(|t| t.name.clone()),
            publisher: default.and_then(|t| t.publisher.clone()),
            icon,
            nacp: Some(crate::MediaInput::from_bytes(
                crate::MediaType::Nacp,
                self.nacp_bytes.clone(),
            )),
            localized,
            opaque: Vec::new(),
        })
    }
}

#[cfg(test)]
mod duplicate_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn strict_partition_verification_bounds_scratch_by_actual_data_size() {
        let entries = synthetic::title_entries(&synthetic::TitleSpec::application(
            0x0100_0000_0001_0000,
            0,
            1,
        ));
        let mut bytes = entries
            .into_iter()
            .find(|(name, _)| name.ends_with(".cnmt.nca"))
            .unwrap()
            .1;
        // A valid one-block small PartitionFS with an enormous declared hash
        // block must not allocate that declared block size (2 GiB).
        bytes[0x428..0x42c].copy_from_slice(&0x8000_0000u32.to_le_bytes());
        let digest = crate::hash::sha256(&bytes[0x400..0x600]);
        bytes[0x280..0x2a0].copy_from_slice(&digest);
        let length = bytes.len() as u64;
        let mut source = Source::new(std::io::Cursor::new(bytes), length);
        let parsed = parse_nca(&mut source, 0, length).ok().unwrap();
        assert_eq!(
            parsed
                .verify_partition(&mut source, 0, &InventoryOptions::default())
                .unwrap(),
            VerificationState::Verified
        );
    }

    #[test]
    fn native_metadata_quota_is_cumulative_across_sources() {
        let bytes = synthetic::title_entries(&synthetic::TitleSpec::application(
            0x0100_0000_0001_0000,
            0,
            1,
        ))
        .into_iter()
        .find(|(name, _)| name.ends_with(".cnmt.nca"))
        .unwrap()
        .1;
        let length = bytes.len() as u64;
        let mut first = Source::new(std::io::Cursor::new(bytes.clone()), length);
        let parsed = parse_nca(&mut first, 0, length).ok().unwrap();
        let size = parsed
            .partition(&mut first, &InventoryOptions::default())
            .unwrap()
            .1[0]
            .size;
        let budget = std::rc::Rc::new(std::cell::Cell::new(size * 8));
        first = first.with_metadata_budget(budget.clone());
        nca::read_content_meta(&mut first, &parsed, &InventoryOptions::default()).unwrap();
        assert_eq!(budget.get(), 0);
        let mut second =
            Source::new(std::io::Cursor::new(bytes), length).with_metadata_budget(budget);
        let error =
            nca::read_content_meta(&mut second, &parsed, &InventoryOptions::default()).unwrap_err();
        assert_eq!(error.kind, NativeErrorKind::ResourceLimit);
        assert!(second.check_metadata_budget().is_err());
    }

    #[test]
    fn duplicate_streaming_observes_cancellation() {
        let bytes = synthetic::dnsp(&synthetic::title_entries(
            &synthetic::TitleSpec::application(0x0100_0000_0001_0000, 0, 1),
        ));
        let (mut a, a_guard) = crate::writer::unique_temp(
            &std::env::temp_dir(),
            std::ffi::OsStr::new("nx-duplicate-a"),
        )
        .unwrap();
        let (mut b, b_guard) = crate::writer::unique_temp(
            &std::env::temp_dir(),
            std::ffi::OsStr::new("nx-duplicate-b"),
        )
        .unwrap();
        a.write_all(&bytes).unwrap();
        b.write_all(&bytes).unwrap();
        drop(a);
        drop(b);
        let reports = vec![
            inspect_source(&a_guard.path, &InventoryOptions::default()),
            inspect_source(&b_guard.path, &InventoryOptions::default()),
        ];
        let token = crate::CancellationToken::default();
        token.cancel();
        let error = identical_native_units(
            &reports[0],
            &reports[0].metas[0],
            &reports[1],
            &reports[1].metas[0],
            Some(&token),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let inv = associate_with_cancellation(reports, &token);
        assert_eq!(inv.issues[0].code, IssueCode::Io);
        assert!(inv.plan().applications.is_empty());
        assert_eq!(inv.plan().blocked.len(), 2);
    }
}
