//! Native additions inside the existing repack transaction. No base extraction.
use super::*;
use crate::native::container::{PartitionEntry, Source};
use crate::native::{self, ContentMetaType, InventoryOptions, Severity, SourceReport};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::{cell::Cell, rc::Rc};

/// Select exactly one native Patch/AddOnContent CNMT from a DNSP/DXCI source.
#[derive(Clone, Debug)]
pub struct NativeAddRequest {
    pub native_source: PathBuf,
    pub title_id: u64,
    pub numeric_version: u32,
    /// Required when the application has several base snapshots. This is an
    /// exact NX identity, not a title ID or a native content ID.
    pub base_snapshot_identity: Option<[u8; 32]>,
}

/// An exact native family produced by one selector. The staged archive may
/// contain many families, including versions with the same Program IDs.
pub(super) struct PreparedInheritance {
    pub key: (u8, u64),
    pub kind: u8,
    pub version: u32,
    pub content_id: [u8; 16],
    pub files: Vec<(String, u32)>,
}

impl PreparedInheritance {
    pub(super) fn from_input(
        input: &ArchiveInput,
        files: Vec<(String, u32)>,
    ) -> Result<Self, WriterError> {
        let [application] = input.applications.as_slice() else {
            return Err(invalid("native selector must contain one application"));
        };
        let Some(first) = application.snapshots.first() else {
            return Err(invalid("native selector has no snapshots"));
        };
        let content_id = first.content_id.unwrap_or([0; 16]);
        if application.snapshots.iter().any(|s| {
            s.snapshot_type != first.snapshot_type
                || s.numeric_version != first.numeric_version
                || s.content_id.unwrap_or([0; 16]) != content_id
        }) {
            return Err(invalid(
                "native selector contains multiple content families",
            ));
        }
        Ok(Self {
            key: (application.platform as u8, application.application_id),
            kind: first.snapshot_type as u8,
            version: first.numeric_version,
            content_id,
            files,
        })
    }

    pub(super) fn identity_key(&self) -> ((u8, u64), u8, u32, [u8; 16]) {
        (self.key, self.kind, self.version, self.content_id)
    }
}

/// Persistent input descriptors and inline media survive the native inventory
/// parser. Account for them separately from its temporary memory reservation.
pub(super) fn staged_input_memory(input: &ArchiveInput) -> u64 {
    fn directory_bytes(directory: &DirectoryInput) -> u64 {
        directory.entries.iter().fold(128u64, |total, entry| {
            total
                .saturating_add(512 + entry.name.len() as u64)
                .saturating_add(match &entry.node {
                    NodeInput::Directory(child) => directory_bytes(child),
                    NodeInput::File(_) => 512,
                })
        })
    }
    fn media_bytes(media: &MediaInput) -> u64 {
        media.size.saturating_add(256)
    }
    let mut bytes = input.source_resources.iter().fold(0u64, |n, r| {
        n.saturating_add(256 + directory_bytes(&r.root))
    });
    for app in &input.applications {
        bytes = bytes.saturating_add(1024 + app.programs.len() as u64 * 256);
        for program in &app.programs {
            if let Some(npdm) = &program.npdm {
                bytes = bytes.saturating_add(media_bytes(npdm));
            }
        }
        for snapshot in &app.snapshots {
            bytes = bytes.saturating_add(2048 + directory_bytes(&snapshot.root));
            if let Some(npdm) = &snapshot.native.npdm {
                bytes = bytes.saturating_add(media_bytes(npdm));
            }
            if let Some(metadata) = &snapshot.metadata {
                bytes = bytes.saturating_add(
                    metadata
                        .title
                        .as_ref()
                        .map_or(0, |s| s.len() as u64)
                        .saturating_add(metadata.publisher.as_ref().map_or(0, |s| s.len() as u64)),
                );
                for media in [&metadata.icon, &metadata.nacp].into_iter().flatten() {
                    bytes = bytes.saturating_add(media_bytes(media));
                }
                for localized in &metadata.localized {
                    bytes = bytes.saturating_add(256 + localized.language.len() as u64);
                    if let Some(title) = &localized.title {
                        bytes = bytes.saturating_add(title.len() as u64);
                    }
                    if let Some(icon) = &localized.icon {
                        bytes = bytes.saturating_add(media_bytes(icon));
                    }
                }
                for media in &metadata.opaque {
                    bytes = bytes.saturating_add(media_bytes(media));
                }
            }
        }
    }
    bytes
}

impl NativeAddRequest {
    pub fn new(source: impl Into<PathBuf>, title_id: u64, numeric_version: u32) -> Self {
        Self {
            native_source: source.into(),
            title_id,
            numeric_version,
            base_snapshot_identity: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct NativeAddStats {
    pub repack: RepackStats,
    pub added_snapshot_identity: [u8; 32],
    pub added_snapshot_identities: Vec<[u8; 32]>,
    pub base_snapshot_identities: Vec<[u8; 32]>,
    pub base_snapshot_identity: [u8; 32],
    pub required_snapshot_identity: [u8; 32],
    /// Native parser's requested logical bytes from the base NX, not extraction.
    pub base_native_logical_bytes_read: u64,
    /// Stored NX block bytes read by the one-block native metadata cache.
    pub base_native_stored_bytes_read: u64,
    pub new_source_inspection_bytes_read: u64,
    pub new_source_equivalence_bytes_read: u64,
    /// Native Strict sections skipped at its configured/memory-derived ceiling.
    /// NX Strict block/file verification is separate and is never skipped.
    pub native_verification_skipped: u32,
    pub new_native_bytes_staged: u64,
    pub inherited_files: u32,
    pub inherited_file_bytes: u64,
    /// Conservative allocation accounting, not measured resident memory.
    pub accounted_memory_bytes: u64,
}

/// Add native updates/DLCs in one transaction. All requests are validated
/// before publication, so a rejected member cannot leave a partial batch.
/// Dependencies must already exist in the retained NX (including any update
/// required by a DLC); this API does not synthesize dependencies within a batch.
pub fn add_native_contents(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    requests: &[NativeAddRequest],
    native_options: &InventoryOptions,
    options: &WriterOptions,
    policy: IntegrityPolicy,
) -> Result<NativeAddStats, WriterError> {
    add_native_contents_with_recipes(
        source.as_ref(),
        destination.as_ref(),
        requests,
        native_options,
        options,
        policy,
        None,
    )
}

/// `add_native_contents` that also keeps the recipes `recipes` makes from
/// the staged archive, in the same generation: both are published or neither.
pub(crate) fn add_native_contents_with_recipes(
    source: &Path,
    destination: &Path,
    requests: &[NativeAddRequest],
    native_options: &InventoryOptions,
    options: &WriterOptions,
    policy: IntegrityPolicy,
    recipes: Option<DeferredRecipes<'_>>,
) -> Result<NativeAddStats, WriterError> {
    if requests.is_empty() {
        return Err(invalid("native addition batch is empty"));
    }
    repack_inner(
        source,
        destination,
        RepackPlan::new(),
        options,
        policy,
        Some((requests, native_options)),
        false,
        &CatalogEdit {
            deferred: recipes,
            ..CatalogEdit::default()
        },
    )
}

/// Add one native update/DLC using only the retained NX and the new source.
/// Strict verifies all retained payloads; Fast authenticates indexes and
/// touched native blocks, and verifies new output.
pub fn add_native_content(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    request: &NativeAddRequest,
    native_options: &InventoryOptions,
    options: &WriterOptions,
    policy: IntegrityPolicy,
) -> Result<NativeAddStats, WriterError> {
    add_native_contents(
        source,
        destination,
        std::slice::from_ref(request),
        native_options,
        options,
        policy,
    )
}

struct Extent {
    start: u64,
    size: u64,
    file: u32,
}

// A flat native-file transport for the existing parser. A single decoded NX
// block is cached. The parser still validates every NCA's own bounds.
struct NativeReader<'a> {
    file: &'a mut File,
    archive: &'a SourceArchive,
    extents: Vec<Extent>,
    position: u64,
    size: u64,
    frame: Vec<u8>,
    logical: Vec<u8>,
    cached: Option<u32>,
    decoder: zstd::bulk::Decompressor<'static>,
    options: &'a WriterOptions,
    stored_read: u64,
}

impl Read for NativeReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_native(buffer).map_err(io::Error::other)
    }
}

impl NativeReader<'_> {
    fn read_native(&mut self, buffer: &mut [u8]) -> Result<usize, WriterError> {
        cancelled(self.options)?;
        if buffer.is_empty() || self.position == self.size {
            return Ok(0);
        }
        let at = self.extents.partition_point(|e| e.start <= self.position);
        let extent = self
            .extents
            .get(at.saturating_sub(1))
            .ok_or_else(|| bad("native NX read outside file ranges"))?;
        let record = &self.archive.files[extent.file as usize];
        let offset = self.position - extent.start;
        let ri = record.first_ref + (offset / record.chunk_size_bytes as u64) as u32;
        let id = self.archive.block_refs[ri as usize];
        if self.cached != Some(id) {
            let block = &self.archive.blocks[id as usize];
            decode_source_block(
                self.file,
                block,
                &mut self.frame,
                &mut self.logical,
                &mut self.decoder,
                self.options,
            )?;
            if !checks_match(block, &self.logical) {
                return Err(bad("base native NX block CRC-32C mismatch"));
            }
            self.stored_read += block.stored_size as u64;
            self.cached = Some(id);
        }
        let within = (offset % record.chunk_size_bytes as u64) as usize;
        let n = buffer
            .len()
            .min(self.logical.len() - within)
            .min((extent.size - offset).min(usize::MAX as u64) as usize);
        buffer[..n].copy_from_slice(&self.logical[within..within + n]);
        self.position += n as u64;
        Ok(n)
    }
}

impl Seek for NativeReader<'_> {
    fn seek(&mut self, offset: SeekFrom) -> io::Result<u64> {
        let next = match offset {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.position as i128 + n as i128,
            SeekFrom::End(n) => self.size as i128 + n as i128,
        };
        if next < 0 || next > self.size as i128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native NX seek out of range",
            ));
        }
        self.position = next as u64;
        Ok(self.position)
    }
}

fn snapshot_entries(
    archive: &SourceArchive,
    snapshot: &SourceSnapshot,
) -> Result<Vec<(String, u32)>, WriterError> {
    let mut result = Vec::new();
    for entry in &archive.directories[snapshot.snapshot.root as usize] {
        if entry.kind != 2 || !entry.name.ends_with(".nca") {
            return Err(invalid("native base must contain a flat, complete NCA set"));
        }
        result.push((entry.name.clone(), entry.target));
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn inspect_base(
    path: &Path,
    file: &mut File,
    archive: &SourceArchive,
    base: &SourceSnapshot,
    native_options: &InventoryOptions,
    options: &WriterOptions,
    stats: &mut NativeAddStats,
    metadata_budget: &Rc<Cell<u64>>,
) -> Result<SourceReport, WriterError> {
    let files = snapshot_entries(archive, base)?;
    if files.len() > native_options.max_partition_entries as usize {
        return Err(limit_error(
            "native base file count exceeds configured limit",
        ));
    }
    let mut entries = Vec::new();
    let mut extents = Vec::new();
    let mut size = 0u64;
    for (name, id) in files {
        let length = archive.files[id as usize].size;
        entries.push((
            None,
            PartitionEntry {
                name,
                offset: size,
                size: length,
            },
        ));
        extents.push(Extent {
            start: size,
            size: length,
            file: id,
        });
        size = size
            .checked_add(length)
            .ok_or_else(|| bad("native file ranges overflow"))?;
    }
    let mut decoder = zstd::bulk::Decompressor::new()?;
    decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
    let mut reader = NativeReader {
        file,
        archive,
        extents,
        size,
        position: 0,
        frame: Vec::new(),
        logical: Vec::new(),
        cached: None,
        decoder,
        options,
        stored_read: 0,
    };
    let mut src = Source::new(&mut reader, size).with_metadata_budget(metadata_budget.clone());
    let mut report = SourceReport {
        path: path.to_owned(),
        size,
        format: native::SourceFormat::Unrecognized,
        status: native::SourceStatus::Rejected,
        units: Vec::new(),
        metas: Vec::new(),
        tree: None,
        system_update_bytes: None,
        issues: Vec::new(),
        bytes_read: 0,
    };
    let result = native::inspect_entries(&mut src, &mut report, native_options, entries);
    report.bytes_read = src.bytes_read;
    stats.base_native_logical_bytes_read += src.bytes_read;
    drop(src);
    stats.base_native_stored_bytes_read += reader.stored_read;
    cancelled(options)?;
    result.map_err(|e| {
        if e.kind == native::NativeErrorKind::ResourceLimit {
            limit_error(format!("base NX native metadata: {e}"))
        } else {
            bad(format!("base NX native metadata: {e}"))
        }
    })?;
    stats.native_verification_skipped += report
        .issues
        .iter()
        .filter(|i| i.code == native::IssueCode::VerificationSkipped)
        .count() as u32;
    Ok(report)
}

fn is_primary(archive: &SourceArchive, snapshot: &SourceSnapshot) -> bool {
    snapshot.program_id.is_some_and(|id| {
        archive.programs.iter().any(|p| {
            p.key == snapshot.key && p.program_id == id && p.kind == ProgramKind::Primary as u8
        })
    })
}

fn select_base_family(
    archive: &SourceArchive,
    key: (u8, u64),
    identity: Option<[u8; 32]>,
) -> Result<Vec<&SourceSnapshot>, WriterError> {
    let all: Vec<_> = archive
        .snapshots
        .iter()
        .filter(|s| s.key == key && s.snapshot.snapshot_type == SnapshotType::Base as u8)
        .collect();
    let group = if let Some(id) = identity {
        let s = all
            .iter()
            .find(|s| s.snapshot.identity == id)
            .ok_or_else(|| invalid("native base identity is absent"))?;
        (s.snapshot.version, s.snapshot.content_id)
    } else {
        let groups: std::collections::BTreeSet<_> = all
            .iter()
            .map(|s| (s.snapshot.version, s.snapshot.content_id))
            .collect();
        if groups.len() != 1 {
            return Err(invalid("native base version is missing or ambiguous"));
        }
        *groups.first().unwrap()
    };
    let family: Vec<_> = all
        .into_iter()
        .filter(|s| (s.snapshot.version, s.snapshot.content_id) == group)
        .collect();
    if family.iter().filter(|s| is_primary(archive, s)).count() != 1 {
        return Err(invalid("native base has no unique primary Program"));
    }
    Ok(family)
}

fn validate_family(
    archive: &SourceArchive,
    family: &[&SourceSnapshot],
    plan: &native::PlannedApplication,
    kind: SnapshotType,
    bases: Option<&[&SourceSnapshot]>,
) -> Result<(), WriterError> {
    let expected: Vec<_> = plan
        .snapshots
        .iter()
        .filter(|s| s.snapshot_type == kind)
        .collect();
    if expected.len() != family.len() {
        return Err(bad("native Program snapshot family is incomplete"));
    }
    let anchor = snapshot_entries(archive, family[0])?;
    for expected in expected {
        let rows: Vec<_> = family
            .iter()
            .filter(|s| s.program_id == expected.program_id)
            .collect();
        if rows.len() != 1 {
            return Err(bad("native Program snapshot selection is ambiguous"));
        }
        let row = rows[0];
        let program = archive
            .programs
            .iter()
            .find(|p| p.key == row.key && Some(p.program_id) == row.program_id)
            .ok_or_else(|| bad("native Program row absent"))?;
        let expected_kind = if expected.program_id_offset == Some(0) {
            ProgramKind::Primary
        } else {
            ProgramKind::Auxiliary
        };
        if row.snapshot.build_id != expected.build_id.unwrap_or([0; 32])
            || program.kind != expected_kind as u8
        {
            return Err(bad(
                "native Program Build ID/kind disagrees with executable",
            ));
        }
        let files = snapshot_entries(archive, row)?;
        if files.len() != anchor.len()
            || files.iter().any(|(name, index)| {
                !anchor.iter().any(|(n, id)| {
                    name == n
                        && same_file_structure(
                            archive,
                            &archive.files[*id as usize],
                            &archive.files[*index as usize],
                        )
                })
            })
        {
            return Err(bad(
                "native Program family does not share the complete native root",
            ));
        }
        if let Some(bases) = bases {
            let base = bases
                .iter()
                .find(|b| b.program_id == row.program_id)
                .ok_or_else(|| bad("required Program base is absent"))?;
            if !row.snapshot.dependencies.iter().any(|d| {
                d.kind == DependencyKind::RequiredBase
                    && d.target_snapshot_identity == base.snapshot.identity
            }) {
                return Err(bad("native Program required-base identity mismatch"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare(
    source_path: &Path,
    destination: &Path,
    file: &mut File,
    archive: &SourceArchive,
    request: &NativeAddRequest,
    native_options: &InventoryOptions,
    options: &WriterOptions,
    budget: &mut MemoryBudget,
    stats: &mut NativeAddStats,
) -> Result<(ArchiveInput, Vec<(String, u32)>), WriterError> {
    cancelled(options)?;
    let virtual_source = crate::source_file::is_virtual(&request.native_source);
    let native_path = if virtual_source {
        request.native_source.clone()
    } else {
        request.native_source.canonicalize()?
    };
    if !virtual_source && !native_path.is_file() {
        return Err(invalid("native addition must be a DNSP/DXCI file"));
    }
    if destination == request.native_source
        || destination.canonicalize().ok().as_ref() == Some(&native_path)
    {
        return Err(invalid("destination is the native addition source"));
    }
    // Bound existing native parser allocations independently of game/file size.
    // These caps may lower caller ceilings but never raise them.
    let mut limits = native_options.clone();
    limits.max_meta_bytes = limits.max_meta_bytes.min(options.max_memory_bytes / 64);
    limits.max_strict_section_bytes = limits
        .max_strict_section_bytes
        .min(options.max_memory_bytes / 16);
    let reserve = limits
        .max_meta_bytes
        .checked_mul(8)
        .and_then(|v| v.checked_add(limits.max_strict_section_bytes.checked_mul(2)?))
        .and_then(|v| v.checked_add(limits.max_partition_entries as u64 * 2048 + 34 * 1024 * 1024))
        .ok_or_else(|| limit_error("native memory reservation overflow"))?;
    budget.charge(reserve, "bounded native metadata and NX decoder")?;
    let metadata_budget = Rc::new(Cell::new(limits.max_meta_bytes * 8));
    let mut addition =
        native::inspect_source_bounded(&native_path, &limits, metadata_budget.clone());
    stats.new_source_inspection_bytes_read += addition.bytes_read;
    stats.native_verification_skipped += addition
        .issues
        .iter()
        .filter(|i| i.code == native::IssueCode::VerificationSkipped)
        .count() as u32;
    cancelled(options)?;
    if addition
        .issues
        .iter()
        .any(|i| i.code == native::IssueCode::ResourceLimit)
    {
        return Err(limit_error("new native source exceeds metadata limits"));
    }
    if addition
        .issues
        .iter()
        .any(|i| i.severity == Severity::Error)
    {
        return Err(invalid(
            "new native source contains invalid or incomplete content",
        ));
    }
    addition.metas.retain(|m| {
        m.meta.title_id == request.title_id && m.meta.version == request.numeric_version
    });
    if addition.metas.len() != 1 || !addition.metas[0].plannable {
        return Err(invalid(
            "native title/version selector is missing, ambiguous or unsupported",
        ));
    }
    let target = &addition.metas[0].meta;
    let kind = match target.meta_type {
        ContentMetaType::Patch => SnapshotType::Update,
        ContentMetaType::AddOnContent => SnapshotType::Dlc,
        _ => {
            return Err(invalid(
                "native addition must select a Patch or AddOnContent CNMT",
            ))
        }
    };
    let key = (Platform::Switch1 as u8, target.application_id);
    let family = select_base_family(archive, key, request.base_snapshot_identity)?;
    let base = *family
        .iter()
        .find(|s| is_primary(archive, s))
        .ok_or_else(|| invalid("native primary base is absent"))?;
    stats.base_snapshot_identity = base.snapshot.identity;
    stats.base_snapshot_identities = family.iter().map(|s| s.snapshot.identity).collect();
    let base_report = inspect_base(
        source_path,
        file,
        archive,
        base,
        &limits,
        options,
        stats,
        &metadata_budget,
    )?;
    if base_report
        .issues
        .iter()
        .any(|i| i.severity == Severity::Error)
        || base_report.metas.len() != 1
    {
        return Err(bad(
            "base NX does not contain one validated native application",
        ));
    }
    let meta = &base_report.metas[0];
    if meta.meta.meta_type != ContentMetaType::Application
        || meta.meta.application_id != key.1
        || meta.meta.title_id != key.1
        || meta.meta.version != base.snapshot.version
        || meta.meta.meta_content_id != Some(base.snapshot.content_id)
        || meta.units.len() != base_report.units.len()
    {
        return Err(bad(
            "base NX catalog disagrees with complete native content",
        ));
    }
    if kind == SnapshotType::Update && meta.meta.patch_id != Some(request.title_id) {
        return Err(invalid("update title disagrees with native base Patch ID"));
    }
    let base_plan = native::associate(vec![base_report.clone()]).plan();
    if !base_plan.blocked.is_empty() || base_plan.applications.len() != 1 {
        return Err(invalid(
            "native base Program associations are incomplete or ambiguous",
        ));
    }
    validate_family(
        archive,
        &family,
        &base_plan.applications[0],
        SnapshotType::Base,
        None,
    )?;
    if archive.programs.iter().filter(|p| p.key == key).count() != family.len() {
        return Err(invalid(
            "native application contains unrepresented Program associations",
        ));
    }
    // A title/version already present must never be silently replaced, even if
    // a caller supplies a new Meta ID or a divergent executable.
    if archive.snapshots.iter().any(|s| {
        s.key == key
            && s.snapshot.snapshot_type == kind as u8
            && s.snapshot.version == request.numeric_version
            && (kind != SnapshotType::Dlc
                || s.snapshot.content_id == target.meta_content_id.unwrap_or([0; 16]))
    }) {
        return Err(invalid("native snapshot version is already present"));
    }
    if kind == SnapshotType::Dlc {
        for old in archive.snapshots.iter().filter(|s| {
            s.key == key
                && s.snapshot.snapshot_type == SnapshotType::Dlc as u8
                && s.snapshot.version == request.numeric_version
        }) {
            let report = inspect_base(
                source_path,
                file,
                archive,
                old,
                &limits,
                options,
                stats,
                &metadata_budget,
            )?;
            if report.issues.iter().any(|i| i.severity == Severity::Error)
                || report.metas.len() != 1
                || report.metas[0].meta.meta_content_id != Some(old.snapshot.content_id)
                || report.metas[0].meta.meta_type != ContentMetaType::AddOnContent
                || report.metas[0].meta.application_id != key.1
                || report.metas[0].meta.version != old.snapshot.version
                || report.metas[0].units.len() != report.units.len()
            {
                return Err(bad("retained DLC catalog/native view disagrees"));
            }
            if report.metas[0].meta.title_id == request.title_id {
                return Err(invalid("DLC title/version is already present"));
            }
        }
    }
    let minimum = target.required_application_version.unwrap_or(0);
    let plan = if kind == SnapshotType::Update {
        native::associate(vec![base_report.clone(), addition]).plan_with_file_comparison(
            &mut |a, b| {
                let candidate = if a.source == native_path { a } else { b };
                existing_native_file(file, archive, key, candidate, options, stats)
                    .map(|index| index.is_some())
                    .map_err(io::Error::other)
            },
        )
    } else {
        native::associate(vec![addition]).plan()
    };
    // A cancelled comparison is carried through the planner's IO diagnostic.
    // Preserve cancellation as the public transaction result, not InvalidInput.
    cancelled(options)?;
    if !plan.blocked.is_empty() {
        return Err(invalid(format!(
            "native addition cannot form a complete supported snapshot: {}",
            native::BlockedContent::describe(&plan.blocked)
        )));
    }
    if plan.applications.len() != 1 {
        return Err(invalid(
            "native addition cannot form a complete supported snapshot",
        ));
    }
    let mut app = plan.applications.into_iter().next().unwrap();
    app.snapshots.retain(|s| s.snapshot_type == kind);
    if app.snapshots.is_empty() {
        return Err(invalid("native addition selection is not unique"));
    }
    let required = if kind == SnapshotType::Update {
        base
    } else {
        let candidates: Vec<_> = archive
            .snapshots
            .iter()
            .filter(|s| {
                s.key == key
                    && is_primary(archive, s)
                    && matches!(s.snapshot.snapshot_type, 0 | 1)
                    && s.snapshot.version >= minimum
                    && (s.snapshot.identity == base.snapshot.identity
                        || (s.snapshot.snapshot_type == SnapshotType::Update as u8
                            && s.snapshot.dependencies.iter().any(|d| {
                                d.kind == DependencyKind::RequiredBase
                                    && d.target_snapshot_identity == base.snapshot.identity
                            })))
            })
            .collect();
        // The lowest version that fits; on a tie the base, which the update
        // also needs (a base may carry its update's version number).
        let lowest = candidates
            .iter()
            .map(|s| (s.snapshot.version, s.snapshot.snapshot_type))
            .min()
            .ok_or_else(|| {
                invalid(format!(
                    "DLC needs game version {minimum} or later, which is not in the archive"
                ))
            })?;
        let selected: Vec<_> = candidates
            .into_iter()
            .filter(|s| (s.snapshot.version, s.snapshot.snapshot_type) == lowest)
            .collect();
        if selected.len() != 1 {
            return Err(invalid("DLC required snapshot is ambiguous"));
        }
        selected[0]
    };
    if required.snapshot.snapshot_type == SnapshotType::Update as u8 {
        validate_required_update(
            source_path,
            file,
            archive,
            base,
            &base_report,
            required,
            &limits,
            options,
            stats,
            &metadata_budget,
        )?;
    }
    stats.required_snapshot_identity = required.snapshot.identity;
    let mut inherited = if kind == SnapshotType::Update {
        snapshot_entries(archive, base)?
    } else {
        Vec::new()
    };
    let mut fresh_files = Vec::new();
    for candidate in app.snapshots[0]
        .files
        .iter()
        .filter(|f| f.source == native_path)
    {
        if let Some(index) = existing_native_file(file, archive, key, candidate, options, stats)? {
            inherited.push((candidate.name.clone(), index));
        } else {
            fresh_files.push(candidate.clone());
        }
    }
    // One RAW staging stream for each new NCA, regardless of Program count.
    // The complete shared native root is attached to every Program after staging.
    let anchor = app
        .snapshots
        .iter()
        .position(|s| s.program_id_offset == Some(0))
        .unwrap_or(0);
    for (i, snapshot) in app.snapshots.iter_mut().enumerate() {
        snapshot.files = if i == anchor {
            std::mem::take(&mut fresh_files).into()
        } else {
            Vec::new().into()
        };
    }
    let mut input = app.to_application_input(&|_| true, &|reference| {
        family
            .iter()
            .find(|s| s.program_id == reference.program_id)
            .map(|s| s.snapshot.identity)
    })?;
    if kind == SnapshotType::Dlc {
        input.snapshots[0].dependencies = vec![DependencyInput {
            kind: DependencyKind::Requires,
            content_id: None,
            target_snapshot_identity: required.snapshot.identity,
        }];
    }
    let mut input = ArchiveInput::new(vec![input]);
    if options.preserve_firmware {
        input.source_resources =
            native::source_resources(&native_path, Platform::Switch1, key.1, &limits)
                .map_err(|e| invalid(e.to_string()))?;
    }
    Ok((input, inherited))
}

#[allow(clippy::too_many_arguments)]
fn validate_required_update(
    path: &Path,
    file: &mut File,
    archive: &SourceArchive,
    base: &SourceSnapshot,
    base_report: &SourceReport,
    required: &SourceSnapshot,
    native_options: &InventoryOptions,
    options: &WriterOptions,
    stats: &mut NativeAddStats,
    metadata_budget: &Rc<Cell<u64>>,
) -> Result<(), WriterError> {
    let mut report = inspect_base(
        path,
        file,
        archive,
        required,
        native_options,
        options,
        stats,
        metadata_budget,
    )?;
    if report.issues.iter().any(|i| i.severity == Severity::Error) {
        return Err(bad("required update contains invalid native content"));
    }
    report
        .metas
        .retain(|m| m.meta.meta_content_id == Some(required.snapshot.content_id));
    if report.metas.len() != 1
        || report.metas[0].meta.meta_type != ContentMetaType::Patch
        || report.metas[0].meta.version != required.snapshot.version
        || report.metas[0].meta.title_id != base_report.metas[0].meta.patch_id.unwrap_or(0)
        || !required.snapshot.dependencies.iter().any(|d| {
            d.kind == DependencyKind::RequiredBase
                && d.target_snapshot_identity == base.snapshot.identity
        })
    {
        return Err(bad("required update identity/native dependency mismatch"));
    }
    let actual = snapshot_entries(archive, required)?;
    for (name, index) in snapshot_entries(archive, base)? {
        let other = actual
            .iter()
            .find(|(n, _)| *n == name)
            .ok_or_else(|| bad("required update omits a base native file"))?
            .1;
        if archive.files[other as usize].digest != archive.files[index as usize].digest
            || archive.files[other as usize].size != archive.files[index as usize].size
        {
            return Err(bad("required update changed its native base content"));
        }
    }
    let plan = native::associate(vec![base_report.clone(), report]).plan_with_file_comparison(
        &mut |a, _b| {
            let left = snapshot_entries(archive, base)
                .map_err(io::Error::other)?
                .into_iter()
                .find(|(n, _)| *n == a.name)
                .ok_or_else(|| io::Error::other("shared base file absent"))?
                .1;
            let right = snapshot_entries(archive, required)
                .map_err(io::Error::other)?
                .into_iter()
                .find(|(n, _)| *n == a.name)
                .ok_or_else(|| io::Error::other("shared update file absent"))?
                .1;
            Ok(same_file_structure(
                archive,
                &archive.files[left as usize],
                &archive.files[right as usize],
            ))
        },
    );
    let selected = plan.applications.first().and_then(|a| {
        a.snapshots.iter().find(|s| {
            s.snapshot_type == SnapshotType::Update && s.program_id == required.program_id
        })
    });
    let selected = selected.ok_or_else(|| bad("required update is not a complete native view"))?;
    if !plan.blocked.is_empty()
        || selected.program_id != required.program_id
        || selected.build_id.unwrap_or([0; 32]) != required.snapshot.build_id
        || selected.files.len() != actual.len()
        || selected.files.iter().any(|f| {
            !actual.iter().any(|(name, index)| {
                *name == f.name && archive.files[*index as usize].size == f.size
            })
        })
    {
        return Err(bad(
            "required update catalog disagrees with complete native content",
        ));
    }
    let family: Vec<_> = archive
        .snapshots
        .iter()
        .filter(|s| {
            s.key == required.key
                && s.snapshot.snapshot_type == SnapshotType::Update as u8
                && s.snapshot.version == required.snapshot.version
                && s.snapshot.content_id == required.snapshot.content_id
        })
        .collect();
    let bases = select_base_family(archive, required.key, Some(base.snapshot.identity))?;
    validate_family(
        archive,
        &family,
        &plan.applications[0],
        SnapshotType::Update,
        Some(&bases),
    )?;
    Ok(())
}

fn existing_native_file(
    file: &mut File,
    archive: &SourceArchive,
    key: (u8, u64),
    candidate: &native::PlannedFile,
    options: &WriterOptions,
    stats: &mut NativeAddStats,
) -> Result<Option<u32>, WriterError> {
    let mut indices = std::collections::BTreeSet::new();
    for snapshot in archive.snapshots.iter().filter(|s| s.key == key) {
        for entry in &archive.directories[snapshot.snapshot.root as usize] {
            if entry.kind == 2 && entry.name == candidate.name {
                indices.insert(entry.target);
            }
        }
    }
    let Some(&index) = indices.first() else {
        return Ok(None);
    };
    let record = &archive.files[index as usize];
    if record.size != candidate.size
        || indices.iter().any(|&id| {
            archive.files[id as usize].digest != record.digest
                || archive.files[id as usize].size != record.size
        })
    {
        return Err(invalid("ambiguous retained native filename"));
    }
    // Every retained definition must agree. Exact reference structure proves the
    // same bytes; equal declared digests alone do not establish equivalence.
    for &candidate_index in &indices {
        if candidate_index != index
            && same_file_structure(archive, record, &archive.files[candidate_index as usize])
        {
            continue;
        }
        let mut decoder = zstd::bulk::Decompressor::new()?;
        decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
        let mut reader = NativeReader {
            file: &mut *file,
            archive,
            extents: vec![Extent {
                start: 0,
                size: record.size,
                file: candidate_index,
            }],
            position: 0,
            size: record.size,
            frame: Vec::new(),
            logical: Vec::new(),
            cached: None,
            decoder,
            options,
            stored_read: 0,
        };
        let mut input = crate::source_file::open(&candidate.source)?;
        input.seek(SeekFrom::Start(candidate.offset))?;
        let mut left = [0u8; 65536];
        let mut right = [0u8; 65536];
        let mut done = 0;
        while done < record.size {
            cancelled(options)?;
            let n = (record.size - done).min(left.len() as u64) as usize;
            let result = reader.read_exact(&mut left[..n]);
            cancelled(options)?;
            result?;
            input.read_exact(&mut right[..n])?;
            stats.new_source_equivalence_bytes_read += n as u64;
            stats.base_native_logical_bytes_read += n as u64;
            if left[..n] != right[..n] {
                return Err(invalid(
                    "new source reuses a native filename with different bytes",
                ));
            }
            done += n as u64;
        }
        stats.base_native_stored_bytes_read += reader.stored_read;
    }
    Ok(Some(index))
}

/// Full extracted-tree equivalence using the existing bounded NX transport.
/// Parse once and reuse the decoder; no new archive reader/index implementation.
/// Source files the read-ahead threads may open before the comparison reaches them.
const PREFETCH_WINDOW: usize = 256;

pub(crate) fn verify_extracted_source_files(
    path: &Path,
    views: &[([u8; 32], &crate::import::TreeView)],
    options: &WriterOptions,
) -> Result<u64, WriterError> {
    validate_options(options)?;
    let mut budget = MemoryBudget {
        current: 0,
        peak: 0,
        limit: options.max_memory_bytes,
    };
    let mut file = File::open(path)?;
    let archive = parse_archive(&mut file, 0, options, &mut budget)?;
    verify_source_payloads(&mut file, &archive, options)?;
    budget.charge(
        4 * 1024 * 1024 + 2 * 65536,
        "tree byte comparison scratch and decoder",
    )?;
    let mut decoder = zstd::bulk::Decompressor::new()?;
    decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
    let mut original = [0; 65536];
    let mut packed = [0; 65536];
    let mut compared = 0u64;
    for (identity, view) in views {
        cancelled(options)?;
        let snapshot = archive
            .snapshots
            .iter()
            .find(|s| s.snapshot.identity == *identity)
            .ok_or_else(|| invalid("tree verification snapshot absent"))?;
        let old_budget = budget.current;
        budget.charge(
            view.files
                .keys()
                .chain(&view.directories)
                .map(|p| 128 + p.len() as u64 * 2)
                .sum(),
            "tree verification nodes",
        )?;
        let mut nodes = Vec::new();
        visit_identity_tree(
            snapshot.snapshot.root,
            &[],
            0,
            &archive,
            &mut HashSet::new(),
            &mut nodes,
            &mut 0,
        )?;
        if nodes.len() != view.files.len() + view.directories.len() {
            return Err(invalid("archived tree node count differs from source"));
        }
        // Opening a small source file costs milliseconds when it is cold:
        // helper threads open the next files while this one compares.
        let ahead: Vec<_> = nodes
            .iter()
            .filter(|n| n.source_file.is_some())
            .filter_map(|n| std::str::from_utf8(&n.path).ok())
            .filter_map(|p| view.files.get(p))
            .collect();
        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let helpers = std::thread::available_parallelism().map_or(1, |n| n.get().clamp(2, 8));
        decoder = std::thread::scope(|scope| {
            for _ in 0..helpers {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= ahead.len() {
                        return;
                    }
                    // Stay within PREFETCH_WINDOW files of the comparison.
                    while i > done.load(Ordering::Acquire) + PREFETCH_WINDOW {
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    ahead[i].prefetch();
                });
            }
            struct Stop<'a>(&'a AtomicBool);
            impl Drop for Stop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let _stop = Stop(&stop);
            for node in nodes {
                let logical =
                    std::str::from_utf8(&node.path).map_err(|_| invalid("tree path UTF-8"))?;
                if let Some(index) = node.source_file {
                    done.fetch_add(1, Ordering::Release);
                    let source = view
                        .files
                        .get(logical)
                        .ok_or_else(|| invalid("unexpected archived tree file"))?;
                    let record = &archive.files[index as usize];
                    // Inventory digests exist only where content continuity was
                    // needed (ExeFS); the byte comparison below is the proof.
                    if record.size != source.size
                        || source.digest.is_some_and(|d| d != record.digest)
                    {
                        return Err(invalid("archived/source file size or digest differs"));
                    }
                    let mut input = source.open_reader(&options.cancellation)?;
                    let mut reader = NativeReader {
                        file: &mut file,
                        archive: &archive,
                        extents: vec![Extent {
                            start: 0,
                            size: record.size,
                            file: index,
                        }],
                        position: 0,
                        size: record.size,
                        frame: Vec::new(),
                        logical: Vec::new(),
                        cached: None,
                        decoder,
                        options,
                        stored_read: 0,
                    };
                    let mut left = record.size;
                    while left != 0 {
                        cancelled(options)?;
                        let n = left.min(original.len() as u64) as usize;
                        input.read_exact(&mut original[..n])?;
                        reader.read_exact(&mut packed[..n])?;
                        if original[..n] != packed[..n] {
                            return Err(invalid("full source/NX byte comparison failed"));
                        }
                        compared = compared
                            .checked_add(n as u64)
                            .ok_or_else(|| limit_error("comparison byte count overflow"))?;
                        left -= n as u64;
                    }
                    let mut extra = [0];
                    if input.read(&mut extra)? != 0 {
                        return Err(invalid("source length changed after packing"));
                    }
                    decoder = reader.decoder;
                } else if !logical.is_empty() && !view.directories.contains(logical) {
                    return Err(invalid("unexpected archived directory"));
                }
            }
            Ok::<_, WriterError>(decoder)
        })?;
        budget.current = old_budget;
    }
    Ok(compared)
}

/// Read an authenticated logical file range through the existing NX indexes.
/// Only touched blocks are decoded and verified. Caller output is unchanged on
/// failure; the temporary range and bounded decoder are memory-budgeted.
pub fn read_snapshot_file_range(
    path: impl AsRef<Path>,
    identity: [u8; 32],
    logical_path: &str,
    offset: u64,
    output: &mut [u8],
    options: &WriterOptions,
) -> Result<(), WriterError> {
    validate_options(options)?;
    cancelled(options)?;
    let mut budget = MemoryBudget {
        current: 0,
        peak: 0,
        limit: options.max_memory_bytes,
    };
    let mut file = File::open(path)?;
    let archive = parse_archive(&mut file, 0, options, &mut budget)?;
    budget.charge(
        output.len() as u64 + 4 * 1024 * 1024,
        "logical file range and NX decoder",
    )?;
    let snapshot = archive
        .snapshots
        .iter()
        .find(|s| s.snapshot.identity == identity)
        .ok_or_else(|| invalid("snapshot identity is absent"))?;
    let mut directory = snapshot.snapshot.root;
    let mut parts = logical_path.split('/').peekable();
    let mut index = None;
    while let Some(part) = parts.next() {
        let entry = archive.directories[directory as usize]
            .iter()
            .find(|e| e.name == part)
            .ok_or_else(|| invalid("snapshot path is absent"))?;
        if parts.peek().is_none() {
            if entry.kind != 2 {
                return Err(invalid("snapshot path is not a file"));
            }
            index = Some(entry.target);
        } else {
            if entry.kind != 1 {
                return Err(invalid("snapshot parent is not a directory"));
            }
            directory = entry.target;
        }
    }
    let index = index.ok_or_else(|| invalid("empty snapshot path"))?;
    let record = &archive.files[index as usize];
    if offset
        .checked_add(output.len() as u64)
        .map_or(true, |end| end > record.size)
    {
        return Err(invalid("snapshot read exceeds file bounds"));
    }
    let mut decoder = zstd::bulk::Decompressor::new()?;
    decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
    let mut reader = NativeReader {
        file: &mut file,
        archive: &archive,
        extents: vec![Extent {
            start: 0,
            size: record.size,
            file: index,
        }],
        position: offset,
        size: record.size,
        frame: Vec::new(),
        logical: Vec::new(),
        cached: None,
        decoder,
        options,
        stored_read: 0,
    };
    let mut temporary = vec![0u8; output.len()];
    let result = reader.read_exact(&mut temporary);
    cancelled(options)?;
    result?;
    output.copy_from_slice(&temporary);
    Ok(())
}

pub(super) fn inherit_files(
    source: &SourceArchive,
    staged: &mut SourceArchive,
    group: PreparedInheritance,
    budget: &mut MemoryBudget,
    stats: &mut NativeAddStats,
) -> Result<(), WriterError> {
    let members: Vec<usize> = staged
        .snapshots
        .iter()
        .enumerate()
        .filter(|(_, snapshot)| {
            snapshot.key == group.key
                && snapshot.snapshot.snapshot_type == group.kind
                && snapshot.snapshot.version == group.version
                && snapshot.snapshot.content_id == group.content_id
        })
        .map(|(index, _)| index)
        .collect();
    let Some(&anchor) = members
        .iter()
        .find(|&&index| is_primary(staged, &staged.snapshots[index]))
        .or_else(|| members.first())
    else {
        return Err(invalid("staged native content family is missing"));
    };
    let root = staged.snapshots[anchor].snapshot.root as usize;
    for (name, index) in group.files {
        if staged.directories[root].iter().any(|e| e.name == name) {
            return Err(invalid(
                "native base and addition contain ambiguous NCA names",
            ));
        }
        budget.charge(256 + name.len() as u64, "inherited native file reference")?;
        let record = source.files[index as usize].clone();
        let target = u32::try_from(staged.files.len())
            .map_err(|_| limit_error("native file count overflow"))?;
        stats.inherited_files += 1;
        stats.inherited_file_bytes += record.size;
        staged.files.push(record);
        staged.inherited_files.insert(target, index);
        staged.directories[root].push(SourceEntry {
            name,
            kind: 2,
            target,
        });
    }
    staged.directories[root].sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let root = root as u32;
    let mut replacements = HashMap::new();
    for i in members {
        staged.snapshots[i].snapshot.root = root;
        let snapshot = &staged.snapshots[i];
        let mut nodes = vec![IdentityNode {
            path: Vec::new(),
            kind: 1,
            source_file: None,
        }];
        let mut logical_size = 0;
        visit_identity_tree(
            root,
            &[],
            0,
            staged,
            &mut HashSet::new(),
            &mut nodes,
            &mut logical_size,
        )?;
        let file_ids: Vec<_> = (0..staged.files.len()).map(|i| i as u32).collect();
        let identity = snapshot_identity(
            snapshot.key.0,
            snapshot.key.1,
            snapshot.program_id,
            snapshot.snapshot.snapshot_type,
            snapshot.snapshot.version,
            snapshot.snapshot.content_id,
            &mut nodes,
            &file_ids,
            &staged.files,
        )?;
        replacements.insert(snapshot.snapshot.identity, identity);
        staged.snapshots[i].snapshot.identity = identity;
        staged.snapshots[i].snapshot.logical_size = logical_size;
        stats.added_snapshot_identities.push(identity);
    }
    for app in &mut staged.applications {
        if let Some(new) = app.selected_identity.and_then(|id| replacements.get(&id)) {
            app.selected_identity = Some(*new);
        }
    }
    stats.added_snapshot_identity = staged.snapshots[anchor].snapshot.identity;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static CANCEL_NATIVE_PUBLICATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn cancel_publication_test(options: &WriterOptions) {
    CANCEL_NATIVE_PUBLICATION.with(|flag| {
        if flag.replace(false) {
            options.cancellation.cancel();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_retained_same_name_requires_bytes_or_exact_references() {
        let (handle, guard) =
            unique_temp(&std::env::temp_dir(), OsStr::new("nx-retained-proof")).unwrap();
        drop(handle);
        let options = WriterOptions::default();
        let snapshots = (0..2)
            .map(|version| SnapshotInput {
                addon_title_id: None,
                native: SnapshotNativeMetadata::default(),
                program_id: Some(1),
                snapshot_type: SnapshotType::Base,
                numeric_version: version,
                content_id: None,
                build_id: None,
                display_version: None,
                dependencies: Vec::new(),
                metadata: None,
                root: DirectoryInput::new(vec![EntryInput::file(
                    "same.nca",
                    FileInput::from_bytes(vec![version as u8 + 1; 65536]),
                )]),
            })
            .collect();
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
                snapshots,
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
        let (mut original, original_guard) =
            unique_temp(&std::env::temp_dir(), OsStr::new("nx-retained-original")).unwrap();
        original.write_all(&vec![1; 65536]).unwrap();
        drop(original);
        let candidate = native::PlannedFile {
            name: "same.nca".into(),
            source: original_guard.path.clone(),
            offset: 0,
            size: 65536,
        };
        assert_eq!(archive.files.len(), 2);
        // Simulate equal declared file digests with distinct valid block payloads.
        // The native comparison must not silently choose the first definition.
        archive.files[1].digest = archive.files[0].digest;
        assert!(existing_native_file(
            &mut file,
            &archive,
            (Platform::Switch1 as u8, 1),
            &candidate,
            &options,
            &mut NativeAddStats::default()
        )
        .is_err());
        archive.files[1] = archive.files[0].clone();
        assert!(existing_native_file(
            &mut file,
            &archive,
            (Platform::Switch1 as u8, 1),
            &candidate,
            &options,
            &mut NativeAddStats::default()
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn cancellation_during_native_staging_and_before_publication_preserves_both_archives() {
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("nx-native-cancel-{}-{id}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let base_source = directory.join("base.nsp");
        std::fs::write(
            &base_source,
            native::synthetic::dnsp(&native::synthetic::title_entries(
                &native::synthetic::TitleSpec::application(0x0100_0000_0001_0000, 0, 1),
            )),
        )
        .unwrap();
        let inventory = native::associate(vec![native::inspect_source(
            &base_source,
            &InventoryOptions::default(),
        )]);
        let input = inventory.plan().applications[0]
            .to_application_input(&|_| true, &|_| None)
            .unwrap();
        let source = directory.join("base.nx");
        write_archive(
            ArchiveInput::new(vec![input]),
            &source,
            &WriterOptions::default(),
        )
        .unwrap();
        std::fs::remove_file(base_source).unwrap();
        let before = std::fs::read(&source).unwrap();
        let update = directory.join("update.nsp");
        std::fs::write(
            &update,
            native::synthetic::dnsp(&native::synthetic::title_entries(
                &native::synthetic::TitleSpec::patch(0x0100_0000_0001_0000, 1, 10),
            )),
        )
        .unwrap();
        let request = NativeAddRequest::new(update, 0x0100_0000_0001_0800, 1);
        let destination = directory.join("destination.nx");
        std::fs::write(&destination, b"old destination").unwrap();
        for final_phase in [false, true] {
            if final_phase {
                CANCEL_NATIVE_PUBLICATION.with(|flag| flag.set(true));
            } else {
                CANCEL_DURING_VERIFICATION_TEST.with(|flag| flag.set(true));
            }
            let error = add_native_content(
                &source,
                &destination,
                &request,
                &InventoryOptions::default(),
                &WriterOptions::default(),
                IntegrityPolicy::Strict,
            )
            .unwrap_err();
            assert_eq!(error.kind(), WriterErrorKind::Cancelled);
            assert_eq!(std::fs::read(&source).unwrap(), before);
            assert_eq!(std::fs::read(&destination).unwrap(), b"old destination");
            assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 3);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
