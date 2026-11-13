use super::*;
use crate::{
    ApplicationInput, ArchiveInput, ArchiveSummary, DependencyInput, DependencyKind,
    IntegrityPolicy, ProgramInput, ProgramKind, RepackPlan, RepackStats, WriterOptions,
    WriterStats,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    io::Read,
    path::Path,
};

#[derive(Clone, Debug)]
pub struct ReceiptSnapshot {
    pub identity: [u8; 32],
    pub program: Evidence<u64>,
    pub build: Evidence<[u8; 32]>,
    pub module_build_ids: BTreeMap<String, Option<[u8; 32]>>,
    pub module_hash_checks: BTreeMap<String, u8>,
}
/// Independent audit artifact, deliberately not SnapshotMetadata.opaque and not
/// a new wire extension. Keep it with the NX to retain declared provenance.
#[derive(Clone, Debug)]
pub struct ImportReceipt {
    pub manifest: DeclarationManifest,
    pub snapshots: BTreeMap<String, ReceiptSnapshot>,
}
impl ImportReceipt {
    pub fn encode(&self, limits: &ImportLimits) -> Result<Vec<u8>, WriterError> {
        limits.validate()?;
        self.manifest.revalidated(limits)?;
        let cap = limits.max_inventory_bytes / 4;
        if self.manifest.bytes().len() as u64 * 2 + 32 > cap {
            return Err(limit("receipt manifest exceeds budget"));
        }
        fn push(out: &mut String, line: String, cap: u64) -> Result<(), WriterError> {
            if out.len() as u64 + line.len() as u64 > cap {
                return Err(limit("provenance receipt exceeds budget"));
            }
            out.push_str(&line);
            Ok(())
        }
        let mut text = format!(
            "NXPROVENANCE\t1\nmanifest\t{}\n",
            hex(self.manifest.bytes())
        );
        for (key, s) in &self.snapshots {
            if key.len() > 64 {
                return Err(invalid("oversized receipt key"));
            }
            push(
                &mut text,
                format!("snapshot\t{key}\t{}\n", hex(&s.identity)),
                cap,
            )?;
            for (field, value, origin) in [
                (
                    "program",
                    s.program
                        .value
                        .map(|p| format!("{p:016x}"))
                        .unwrap_or_else(|| "-".into()),
                    &s.program.origin,
                ),
                (
                    "build",
                    s.build.value.map(|b| hex(&b)).unwrap_or_else(|| "-".into()),
                    &s.build.origin,
                ),
            ] {
                let (kind, proof, location) = match origin {
                    Origin::Unknown => ("unknown", String::new(), String::new()),
                    Origin::UserDeclaration {
                        manifest_sha256,
                        field,
                    } => ("declared", hex(manifest_sha256), field.clone()),
                    Origin::NativeStructure { profile, location } => {
                        ("native-structure", profile.clone(), location.clone())
                    }
                    Origin::Derived {
                        exact_base_identity,
                        location,
                    } => ("derived", hex(exact_base_identity), location.clone()),
                };
                if proof.len() > 4096 || location.len() > limits.max_path_bytes * 2 + 1 {
                    return Err(limit("receipt fact bounds"));
                }
                push(
                    &mut text,
                    format!(
                        "fact\t{key}\t{field}\t{value}\t{kind}\t{}\t{}\n",
                        hex(proof.as_bytes()),
                        hex(location.as_bytes())
                    ),
                    cap,
                )?;
            }
            for (path, build) in &s.module_build_ids {
                portable_path(path, limits)?;
                push(
                    &mut text,
                    format!(
                        "module\t{key}\t{path}\t{}\t{}\n",
                        build.map(|b| hex(&b)).unwrap_or_else(|| "-".into()),
                        s.module_hash_checks.get(path).copied().unwrap_or(0)
                    ),
                    cap,
                )?;
            }
            if text.len() as u64 > limits.max_inventory_bytes / 4 {
                return Err(limit("provenance receipt exceeds budget"));
            }
        }
        Ok(text.into_bytes())
    }
    pub fn read(reader: impl Read, limits: &ImportLimits) -> Result<Self, WriterError> {
        limits.validate()?;
        let mut bytes = Vec::new();
        reader
            .take((limits.max_inventory_bytes / 4).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limits.max_inventory_bytes / 4 {
            return Err(limit("receipt byte budget"));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid("receipt UTF-8"))?;
        let mut lines = text.lines();
        if lines.next() != Some("NXPROVENANCE\t1") {
            return Err(unsupported("provenance receipt version"));
        }
        let manifest_line = lines
            .next()
            .ok_or_else(|| invalid("receipt manifest absent"))?;
        let raw = decode_hex(
            manifest_line
                .strip_prefix("manifest\t")
                .ok_or_else(|| invalid("receipt manifest record"))?,
            limits.max_manifest_bytes as usize,
        )?;
        let manifest = DeclarationManifest::read(raw.as_slice(), limits)?;
        let keys: BTreeSet<_> = manifest.snapshots.iter().map(|s| s.key.as_str()).collect();
        let mut result = Self {
            manifest: manifest.clone(),
            snapshots: BTreeMap::new(),
        };
        let mut seen = BTreeSet::new();
        let mut modules = 0;
        for line in lines {
            if line.len() > 65536 {
                return Err(limit("receipt line limit"));
            }
            let f: Vec<_> = line.split('\t').collect();
            let key = *f.get(1).ok_or_else(|| invalid("receipt scope missing"))?;
            if !keys.contains(key) {
                return Err(invalid("receipt references unknown declaration"));
            }
            if f[0] == "snapshot" && f.len() == 3 {
                let identity = unhex(f[2])?;
                if identity == [0; 32]
                    || result
                        .snapshots
                        .insert(
                            key.into(),
                            ReceiptSnapshot {
                                identity,
                                program: Evidence::unknown(),
                                build: Evidence::unknown(),
                                module_build_ids: BTreeMap::new(),
                                module_hash_checks: BTreeMap::new(),
                            },
                        )
                        .is_some()
                {
                    return Err(invalid("duplicate/zero receipt identity"));
                }
                continue;
            }
            let snapshot = result
                .snapshots
                .get_mut(key)
                .ok_or_else(|| invalid("fact precedes receipt snapshot"))?;
            match (f[0], f.len()) {
                ("fact", 7) => {
                    if !seen.insert((key.to_owned(), f[2].to_owned())) {
                        return Err(invalid("duplicate provenance fact"));
                    }
                    let proof = String::from_utf8(decode_hex(f[5], 4096)?)
                        .map_err(|_| invalid("fact proof UTF-8"))?;
                    let location =
                        String::from_utf8(decode_hex(f[6], limits.max_path_bytes * 2 + 1)?)
                            .map_err(|_| invalid("fact location UTF-8"))?;
                    let origin = match f[4] {
                        "unknown" if proof.is_empty() && location.is_empty() && f[3] == "-" => {
                            Origin::Unknown
                        }
                        "declared" => {
                            let digest = unhex(&proof)?;
                            if digest != manifest.sha256 {
                                return Err(invalid("receipt declaration digest mismatch"));
                            }
                            Origin::UserDeclaration {
                                manifest_sha256: digest,
                                field: location,
                            }
                        }
                        "native-structure" => Origin::NativeStructure {
                            profile: proof,
                            location,
                        },
                        "derived" => Origin::Derived {
                            exact_base_identity: unhex(&proof)?,
                            location,
                        },
                        _ => return Err(unsupported("provenance evidence kind")),
                    };
                    match f[2] {
                        "program" => {
                            let value = if f[3] == "-" {
                                None
                            } else {
                                let b = unhex::<8>(f[3])?;
                                let p = u64::from_be_bytes(b);
                                if p == 0 {
                                    return Err(invalid("zero receipt Program ID"));
                                }
                                Some(p)
                            };
                            snapshot.program = Evidence { value, origin };
                        }
                        "build" => {
                            let value = if f[3] == "-" {
                                None
                            } else {
                                Some(unhex(f[3])?)
                            };
                            snapshot.build = Evidence { value, origin };
                        }
                        _ => return Err(unsupported("provenance field")),
                    }
                }
                ("module", 5) => {
                    let checks = f[4]
                        .parse::<u8>()
                        .map_err(|_| invalid("module integrity count"))?;
                    if checks > 3 {
                        return Err(invalid("module integrity count exceeds three"));
                    }
                    snapshot.module_hash_checks.insert(f[2].into(), checks);
                    portable_path(f[2], limits)?;
                    modules += 1;
                    if modules > limits.max_nodes {
                        return Err(limit("receipt module budget"));
                    }
                    if snapshot
                        .module_build_ids
                        .insert(
                            f[2].into(),
                            if f[3] == "-" {
                                None
                            } else {
                                Some(unhex(f[3])?)
                            },
                        )
                        .is_some()
                    {
                        return Err(invalid("duplicate receipt module"));
                    }
                }
                _ => return Err(unsupported("receipt record")),
            }
        }
        if result.snapshots.len() != keys.len()
            || result.snapshots.keys().any(|k| {
                !seen.contains(&(k.clone(), "program".into()))
                    || !seen.contains(&(k.clone(), "build".into()))
            })
        {
            return Err(invalid("incomplete receipt"));
        }
        Ok(result)
    }
    /// Check bindings against authenticated NX indexes, after repack/source
    /// removal. A receipt records observations; it is not a signature of them.
    pub fn match_archive(&self, archive: &ArchiveSummary) -> Result<(), WriterError> {
        for d in &self.manifest.snapshots {
            let record = self
                .snapshots
                .get(&d.key)
                .ok_or_else(|| invalid("receipt scope missing"))?;
            let snapshot = archive
                .applications
                .iter()
                .find(|a| a.platform == d.platform && a.application_id == d.application_id)
                .and_then(|a| a.snapshots.iter().find(|s| s.identity == record.identity))
                .ok_or_else(|| invalid("receipt identity absent from NX"))?;
            if snapshot.program_id != record.program.value
                || snapshot.build_id != record.build.value
                || snapshot.snapshot_type != d.snapshot_type
                || snapshot.numeric_version != d.numeric_version
                || snapshot.content_id != d.content_id
                || snapshot.addon_title_id != d.addon_title_id
                || d.program_id
                    .is_some_and(|p| Some(p) != record.program.value)
                || d.build_id.is_some_and(|b| Some(b) != record.build.value)
            {
                return Err(invalid("receipt declaration/native facts disagree with NX"));
            }
        }
        Ok(())
    }
}
fn decode_hex(s: &str, cap: usize) -> Result<Vec<u8>, WriterError> {
    if s.len() % 2 != 0 || s.len() / 2 > cap || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("bounded receipt hex invalid"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| invalid("invalid receipt hex")))
        .collect()
}

#[derive(Debug)]
pub struct TreeConversionStats {
    pub elapsed: std::time::Duration,
    pub decoded_executable_bytes: u64,
    pub initial: WriterStats,
    pub additions: Vec<RepackStats>,
    pub peak_temporary_disk_bytes: u64,
    pub source_bytes_compared: u64,
    pub receipt: ImportReceipt,
}

/// Result of `add_extracted`: the generations committed to the archive.
#[derive(Debug)]
pub struct TreeAdditionStats {
    pub elapsed: std::time::Duration,
    pub decoded_executable_bytes: u64,
    pub additions: Vec<RepackStats>,
    pub source_bytes_compared: u64,
    pub receipt: ImportReceipt,
}
fn targets(d: &SnapshotDeclaration) -> BTreeSet<&str> {
    let mut out: BTreeSet<_> = d.dependencies.iter().map(|(_, k)| k.as_str()).collect();
    if let Some(c) = &d.composition {
        out.insert(&c.base);
    }
    out
}
fn compose(
    mut overlay: TreeView,
    base: &TreeView,
    identity: [u8; 32],
    limits: &ImportLimits,
) -> Result<TreeView, WriterError> {
    let contract = overlay
        .declaration
        .composition
        .clone()
        .ok_or_else(|| invalid("composition missing"))?;
    if contract.exact_identity != identity
        || overlay.declaration.snapshot_type != SnapshotType::Update
        || overlay.declaration.platform != base.declaration.platform
        || overlay.declaration.application_id != base.declaration.application_id
        || overlay
            .declaration
            .program_id
            .is_some_and(|p| Some(p) != base.scope.program_id.value)
        || overlay.declaration.numeric_version <= base.declaration.numeric_version
        || base.declaration.snapshot_type == SnapshotType::Dlc
    {
        return Err(invalid("composition base identity/scope/version mismatch"));
    }
    let inherits_main = !overlay.files.contains_key("exefs/main");
    let inherits_npdm = !overlay.files.contains_key("exefs/main.npdm");
    let replacements = std::mem::take(&mut overlay.files);
    let dirs = std::mem::take(&mut overlay.directories);
    overlay.files = base.files.clone();
    overlay.directories = base.directories.clone();
    for path in &contract.remove {
        let prefix = format!("{path}/");
        if !overlay.files.contains_key(path) && !overlay.directories.contains(path) {
            return Err(invalid("tombstone does not identify an exact base path"));
        }
        overlay
            .files
            .retain(|p, _| p != path && !p.starts_with(&prefix));
        overlay
            .directories
            .retain(|p| p != path && !p.starts_with(&prefix));
    }
    for dir in dirs {
        if overlay.files.contains_key(&dir) {
            return Err(invalid(
                "file/directory replacement needs explicit deletion",
            ));
        }
        overlay.directories.insert(dir);
    }
    for (path, file) in replacements {
        if overlay.directories.contains(&path) {
            return Err(invalid(
                "directory/file replacement needs explicit deletion",
            ));
        }
        overlay.files.insert(path, file);
    }
    let mut folded = BTreeSet::new();
    for path in overlay.files.keys().chain(&overlay.directories) {
        if !folded.insert(path.to_ascii_lowercase()) {
            return Err(invalid("composed paths have case/type collision"));
        }
        let mut parent = path.as_str();
        while let Some((at, _)) = parent.rsplit_once('/') {
            if !overlay.directories.contains(at) || overlay.files.contains_key(at) {
                return Err(invalid("composition path parent is not a directory"));
            }
            parent = at;
        }
    }
    overlay.accounted_bytes = 1024 * 1024
        + overlay
            .files
            .values()
            .map(|f| f.descriptor_bytes())
            .sum::<u64>()
        + overlay
            .directories
            .iter()
            .map(|p| 1024 + p.len() as u64 * 4)
            .sum::<u64>();
    if overlay.files.len() + overlay.directories.len() > limits.max_nodes
        || overlay.accounted_bytes > limits.max_inventory_bytes
    {
        return Err(limit("composed inventory exceeds budget"));
    }
    overlay.declaration.complete = true;
    tree::validate_view(&mut overlay, limits)?;
    if overlay.scope.program_id.value != base.scope.program_id.value {
        return Err(invalid("composition changed Program association"));
    }
    if inherits_main && overlay.scope.build_id.value.is_some() {
        overlay.scope.build_id.origin = Origin::Derived {
            exact_base_identity: identity,
            location: "exefs/main".into(),
        };
    }
    if inherits_npdm && overlay.scope.program_id.value.is_some() {
        overlay.scope.program_id.origin = Origin::Derived {
            exact_base_identity: identity,
            location: "exefs/main.npdm".into(),
        };
    }
    Ok(overlay)
}

/// Convert only explicitly declared complete materialized views, or the
/// replace/delete contract bound to an exact previously packed base identity.
/// Every payload is verified and compared before the one final publication.
pub fn convert_extracted(
    package: &Path,
    manifest: &DeclarationManifest,
    destination: &Path,
    limits: &ImportLimits,
    options: &WriterOptions,
) -> Result<TreeConversionStats, WriterError> {
    convert_extracted_external(
        package,
        manifest,
        destination,
        limits,
        options,
        &External::default(),
    )
}

/// What a container package supplies beside the declared tree.
#[derive(Default)]
pub(crate) struct External {
    /// Files read from elsewhere while writing: snapshot key -> (path, source).
    pub files: BTreeMap<String, Vec<(String, tree::SourceFile)>>,
    /// Control metadata of snapshots that have no NACP, by snapshot key.
    pub metadata: BTreeMap<String, PackageMetadata>,
    /// Exact metadata recovered from a restoration recipe, consumed per view.
    pub restored_metadata: std::cell::RefCell<BTreeMap<String, RestoredMetadata>>,
    pub source_resources: std::cell::RefCell<Vec<crate::SourceResourceInput>>,
    pub program_content_ids: BTreeMap<(u8, u64, u64), [u8; 16]>,
    pub retained_memory_bytes: u64,
}

pub(crate) struct RestoredMetadata {
    pub metadata: Option<crate::SnapshotMetadata>,
    pub display_version: Option<String>,
    pub native: crate::SnapshotNativeMetadata,
}

/// Title, publisher, display version and icons a package records itself,
/// when it carries no NACP (Switch 2 NUP packages).
#[derive(Clone, Debug, Default)]
pub(crate) struct PackageMetadata {
    pub title: Option<String>,
    pub publisher: Option<String>,
    pub display_version: Option<String>,
    pub icon: Option<Vec<u8>>,
    /// (BCP 47 tag, title, icon).
    pub localized: Vec<(String, Option<String>, Option<Vec<u8>>)>,
}

impl PackageMetadata {
    /// Native snapshot metadata without a NACP; icons that are not PNG or
    /// JPEG, or are too large, are left out. The display version is kept even
    /// when there is no title or icon (an update records only its version).
    pub(crate) fn snapshot_metadata(
        &self,
        limits: &ImportLimits,
    ) -> (Option<crate::SnapshotMetadata>, Option<String>, u64) {
        let mut held = 0u64;
        let mut media = |bytes: &Option<Vec<u8>>| {
            let bytes = bytes.as_ref()?;
            let kind = if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
                crate::MediaType::PngIcon
            } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
                crate::MediaType::JpegIcon
            } else {
                return None;
            };
            if bytes.len() as u64 > limits.max_image_bytes {
                return None;
            }
            held = held.saturating_add(bytes.len() as u64);
            Some(crate::MediaInput::from_bytes(kind, bytes.clone()))
        };
        let text = |t: &Option<String>| {
            t.as_ref()
                .map(|t| t.trim_end_matches('\0').to_owned())
                .filter(|t| !t.is_empty() && t.len() <= 4096 && !t.chars().any(char::is_control))
        };
        let mut localized = Vec::new();
        for (tag, title, icon) in &self.localized {
            let title = text(title);
            let icon = media(icon);
            if title.is_some() || icon.is_some() {
                localized.push(crate::LocalizedMetadata {
                    language: tag.clone(),
                    title,
                    icon,
                });
            }
        }
        let metadata = crate::SnapshotMetadata {
            title: text(&self.title),
            publisher: text(&self.publisher),
            icon: media(&self.icon),
            nacp: None,
            localized,
            opaque: Vec::new(),
        };
        (
            metadata.has_any().then_some(metadata),
            text(&self.display_version).filter(|v| v.len() <= 256),
            held,
        )
    }
}

/// As `convert_extracted`, plus files of each declared tree that are read from
/// elsewhere while writing (a NUP's RomFS): snapshot key -> (path, source).
pub(crate) fn convert_extracted_external(
    package: &Path,
    manifest: &DeclarationManifest,
    destination: &Path,
    limits: &ImportLimits,
    options: &WriterOptions,
    external: &External,
) -> Result<TreeConversionStats, WriterError> {
    let result = convert_inner(
        package,
        manifest,
        destination,
        false,
        limits,
        options,
        external,
    );
    if result.is_err() && options.cancellation.is_cancelled() {
        return Err(WriterError::new(
            WriterErrorKind::Cancelled,
            "tree conversion cancelled",
        ));
    }
    let r = result?;
    Ok(TreeConversionStats {
        elapsed: r.elapsed,
        decoded_executable_bytes: r.decoded_executable_bytes,
        initial: r.initial.ok_or_else(|| invalid("no complete views"))?,
        additions: r.additions,
        peak_temporary_disk_bytes: r.peak_temporary_disk_bytes,
        source_bytes_compared: r.source_bytes_compared,
        receipt: r.receipt,
    })
}

/// Add explicitly declared complete extracted trees (for example a Switch 2
/// update folder) to an existing archive, in place: every new snapshot is
/// staged, verified and committed as a new archive generation. The archive
/// keeps its existing snapshots; a declared scope already present is refused.
pub fn add_extracted(
    archive: &Path,
    package: &Path,
    manifest: &DeclarationManifest,
    limits: &ImportLimits,
    options: &WriterOptions,
) -> Result<TreeAdditionStats, WriterError> {
    if !archive.is_file() {
        return Err(invalid("archive to extend does not exist"));
    }
    // A version the archive already holds is refused, even with other bytes:
    // one base, and one update or DLC per version.
    let existing = crate::inspect_archive(archive, options, IntegrityPolicy::Fast)?;
    let kind = |t: SnapshotType| match t {
        SnapshotType::Base => 0,
        SnapshotType::Dlc => 2,
        _ => 1,
    };
    for d in &manifest.snapshots {
        let taken = existing
            .applications
            .iter()
            .filter(|a| a.platform == d.platform && a.application_id == d.application_id)
            .flat_map(|a| &a.snapshots)
            .any(|s| {
                kind(s.snapshot_type) == kind(d.snapshot_type)
                    && (kind(d.snapshot_type) == 0
                        || (s.numeric_version == d.numeric_version
                            && s.addon_title_id == d.addon_title_id
                            && (d.content_id.is_none() || s.content_id == d.content_id)))
            });
        if taken {
            return Err(invalid(&format!(
                "{}: the archive already holds this version",
                d.key
            )));
        }
    }
    let result = convert_inner(
        package,
        manifest,
        archive,
        true,
        limits,
        options,
        &External::default(),
    );
    if result.is_err() && options.cancellation.is_cancelled() {
        return Err(WriterError::new(
            WriterErrorKind::Cancelled,
            "tree addition cancelled",
        ));
    }
    let r = result?;
    Ok(TreeAdditionStats {
        elapsed: r.elapsed,
        decoded_executable_bytes: r.decoded_executable_bytes,
        additions: r.additions,
        source_bytes_compared: r.source_bytes_compared,
        receipt: r.receipt,
    })
}

struct InnerStats {
    elapsed: std::time::Duration,
    decoded_executable_bytes: u64,
    initial: Option<WriterStats>,
    additions: Vec<RepackStats>,
    peak_temporary_disk_bytes: u64,
    source_bytes_compared: u64,
    receipt: ImportReceipt,
}
fn context(e: WriterError, where_: &str) -> WriterError {
    WriterError::new(e.kind(), format!("{where_}: {e}"))
}
fn convert_inner(
    package: &Path,
    manifest: &DeclarationManifest,
    destination: &Path,
    in_place: bool,
    limits: &ImportLimits,
    options: &WriterOptions,
    external: &External,
) -> Result<InnerStats, WriterError> {
    let started = std::time::Instant::now();
    cancelled(&options.cancellation)?;
    if !options.verify_output_payloads {
        return Err(invalid(
            "extracted conversion requires full output verification",
        ));
    }
    let manifest = manifest.revalidated(limits)?;
    let package_guard = std::sync::Arc::new(super::secure::Root::open(package)?);
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let destination_real = if destination.exists() {
        destination.canonicalize()?
    } else {
        parent.canonicalize()?.join(
            destination
                .file_name()
                .ok_or_else(|| invalid("missing destination filename"))?,
        )
    };
    if destination_real.starts_with(&package_guard.path) {
        return Err(invalid("destination lies inside the source package"));
    }
    let mut pending: BTreeMap<_, _> = manifest
        .snapshots
        .iter()
        .map(|s| (s.key.clone(), s.clone()))
        .collect();
    // Reject cycles before reading large payloads or creating a temporary.
    let mut todo = pending.clone();
    let mut done = BTreeSet::new();
    while !todo.is_empty() {
        let ready: Vec<_> = todo
            .iter()
            .filter(|(_, s)| targets(s).iter().all(|k| done.contains(*k)))
            .map(|(k, _)| k.clone())
            .collect();
        if ready.is_empty() {
            return Err(invalid("cyclic declared dependencies"));
        }
        for k in ready {
            todo.remove(&k);
            done.insert(k);
        }
    }
    let mut views = BTreeMap::<String, TreeView>::new();
    let mut identities = BTreeMap::<String, [u8; 32]>::new();
    let mut input_memory = manifest.bytes().len() as u64 * 8
        + manifest.snapshots.len() as u64 * 4096
        + external.retained_memory_bytes;
    let mut total_work = 0u64;
    let mut total_bytes = 0u64;
    let mut initial = None;
    let mut additions = Vec::new();
    let mut peak = 0;
    let mut watches = Vec::new();
    let mut program_roles = BTreeMap::new();
    let mut primary = BTreeMap::new();
    let mut scopes = BTreeSet::new();
    // A new archive is built in a stage published at the end; an existing
    // archive is extended in place, one committed generation at a time.
    let mut stage = if in_place {
        None
    } else {
        let (file, stage) = crate::writer::unique_temp(parent, OsStr::new(".nx-tree"))
            .map_err(|e| context(e, "allocate conversion stage"))?;
        drop(file);
        Some(stage)
    };
    let work: std::path::PathBuf = match &stage {
        Some(s) => s.path.clone(),
        None => destination_real.clone(),
    };
    while !pending.is_empty() {
        cancelled(&options.cancellation)?;
        // One write per pass. A dependency on a snapshot of the same pass is
        // resolved by the writer, so a base, its updates and its DLC are
        // written together (and an update can name the base's files without
        // reading them again). A composition needs its base identity first.
        let mut ready: Vec<String> = Vec::new();
        loop {
            let more: Vec<String> = pending
                .iter()
                .filter(|(k, s)| {
                    !ready.contains(*k)
                        && targets(s).iter().all(|t| {
                            identities.contains_key(*t)
                                || (s.composition.is_none() && ready.iter().any(|r| r == t))
                        })
                })
                .map(|(k, _)| k.clone())
                .collect();
            if more.is_empty() {
                break;
            }
            ready.extend(more);
        }
        let mut grouped =
            BTreeMap::<(u8, u64), (BTreeMap<u64, ProgramKind>, Vec<crate::SnapshotInput>)>::new();
        for key in &ready {
            let declaration = pending
                .remove(key)
                .ok_or_else(|| invalid("pending declaration missing"))?;
            let mut remaining = limits.clone();
            remaining.max_inventory_bytes = limits
                .max_inventory_bytes
                .checked_sub(input_memory)
                .ok_or_else(|| limit("global import inventory budget"))?;
            remaining.max_executable_bytes = limits.max_executable_bytes.saturating_sub(total_work);
            remaining.max_total_bytes = limits.max_total_bytes.saturating_sub(total_bytes);
            let mut view = tree::inspect_pinned_with(
                package_guard.clone(),
                declaration.clone(),
                manifest.sha256,
                &remaining,
                &options.cancellation,
                external
                    .files
                    .get(key)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )?;
            watches.push(view.watch.clone());
            let inspected_work = view.decoded_executable_bytes;
            let mut new_work = inspected_work;
            if let Some(c) = &declaration.composition {
                remaining.max_executable_bytes = remaining
                    .max_executable_bytes
                    .checked_sub(inspected_work)
                    .ok_or_else(|| limit("composition validation work budget"))?;
                view = compose(
                    view,
                    views
                        .get(&c.base)
                        .ok_or_else(|| invalid("composition base missing"))?,
                    identities[&c.base],
                    &remaining,
                )?;
                new_work = new_work
                    .checked_add(view.decoded_executable_bytes)
                    .ok_or_else(|| limit("composition work overflow"))?;
            }
            input_memory = input_memory
                .checked_add(view.accounted_bytes)
                .ok_or_else(|| limit("inventory memory overflow"))?;
            if input_memory > limits.max_inventory_bytes {
                return Err(limit("global import inventory budget"));
            }
            total_work = total_work
                .checked_add(new_work)
                .ok_or_else(|| limit("executable work overflow"))?;
            total_bytes = total_bytes
                .checked_add(view.files.values().map(|f| f.size).sum::<u64>())
                .ok_or_else(|| limit("source size overflow"))?;
            if total_bytes > limits.max_total_bytes || total_work > limits.max_executable_bytes {
                return Err(limit("global source/validation work budget"));
            }
            let mut deps = declaration.dependencies.clone();
            if let Some(c) = &declaration.composition {
                let kind = if views[&c.base].declaration.snapshot_type == SnapshotType::Base {
                    DependencyKind::RequiredBase
                } else {
                    DependencyKind::Requires
                };
                if let Some((explicit, _)) = deps.iter().find(|(_, k)| k == &c.base) {
                    if *explicit != kind {
                        return Err(invalid("composition dependency kind conflict"));
                    }
                } else {
                    deps.push((kind, c.base.clone()));
                }
            }
            let mut resolved = Vec::new();
            for (kind, target) in deps {
                let parent = &views[&target];
                if parent.declaration.platform != declaration.platform
                    || parent.declaration.application_id != declaration.application_id
                    || (kind == DependencyKind::RequiredBase
                        && (parent.declaration.snapshot_type != SnapshotType::Base
                            || parent.scope.program_id.value != view.scope.program_id.value))
                {
                    return Err(invalid(
                        "dependency application/Program/type association conflict",
                    ));
                }
                let target_snapshot_identity = match identities.get(&target) {
                    Some(id) => *id,
                    // Written earlier in this same pass (see above).
                    None if parent.declaration.addon_title_id.is_none() => {
                        crate::writer::pending_dependency_target(
                            parent.declaration.snapshot_type,
                            parent.declaration.numeric_version,
                            parent.scope.program_id.value,
                        )
                    }
                    None => return Err(invalid("dependency on an add-on of the same pass")),
                };
                resolved.push(DependencyInput {
                    kind,
                    content_id: parent.declaration.content_id,
                    target_snapshot_identity,
                });
            }
            let group = grouped
                .entry((declaration.platform as u8, declaration.application_id))
                .or_default();
            if let Some(program) = view.scope.program_id.value {
                let app = (declaration.platform as u8, declaration.application_id);
                if !scopes.insert((app, Some(program), declaration.numeric_version, None)) {
                    return Err(invalid("duplicate declared Program/version scope"));
                }
                if program_roles
                    .insert((app, program), declaration.program_kind)
                    .is_some_and(|k| k != declaration.program_kind)
                {
                    return Err(invalid("Program role changed between versions"));
                }
                if declaration.program_kind == ProgramKind::Primary
                    && primary.insert(app, program).is_some_and(|p| p != program)
                {
                    return Err(invalid("application declares multiple primary Programs"));
                }
                if group
                    .0
                    .insert(program, declaration.program_kind)
                    .is_some_and(|k| k != declaration.program_kind)
                {
                    return Err(invalid("conflicting Program roles"));
                }
            } else if !scopes.insert((
                (declaration.platform as u8, declaration.application_id),
                None,
                declaration.numeric_version,
                declaration.dlc_scope(),
            )) {
                return Err(invalid("duplicate DLC scope"));
            }
            let mut snapshot = view.writer_snapshot(resolved)?;
            if let Some(restored) = external.restored_metadata.borrow_mut().remove(key) {
                snapshot.metadata = restored.metadata;
                snapshot.display_version = restored.display_version;
                snapshot.native.required_firmware = restored.native.required_firmware;
                if snapshot.native.npdm.is_none() {
                    snapshot.native.npdm = restored.native.npdm;
                }
            } else {
                let control = match control_metadata(&package_guard, &declaration, limits)? {
                    Some((metadata, version, bytes)) => Some((Some(metadata), version, bytes)),
                    None => external
                        .metadata
                        .get(key)
                        .map(|m| m.snapshot_metadata(limits)),
                };
                if let Some((metadata, display_version, bytes)) = control {
                    input_memory = input_memory
                        .checked_add(bytes)
                        .ok_or_else(|| limit("inventory memory overflow"))?;
                    if input_memory > limits.max_inventory_bytes {
                        return Err(limit("global import inventory budget"));
                    }
                    snapshot.metadata = metadata;
                    snapshot.display_version = display_version;
                }
            }
            group.1.push(snapshot);
            views.insert(key.clone(), view);
        }
        let applications = grouped
            .into_iter()
            .map(|(key, (programs, mut snapshots))| {
                // A Program's snapshots are contiguous, oldest first: a base
                // is written before the updates that name its files. DLC last.
                snapshots.sort_by_key(|s| {
                    (
                        s.program_id.is_none(),
                        s.program_id,
                        s.numeric_version,
                        s.snapshot_type,
                    )
                });
                (key, (programs, snapshots))
            })
            .map(
                |((platform, application_id), (programs, snapshots))| ApplicationInput {
                    platform: if platform == 1 {
                        Platform::Switch1
                    } else {
                        Platform::Switch2
                    },
                    application_id,
                    programs: programs
                        .into_iter()
                        .map(|(program_id, kind)| ProgramInput {
                            program_id,
                            kind,
                            content_id: external
                                .program_content_ids
                                .get(&(platform, application_id, program_id))
                                .copied(),
                            npdm: None,
                        })
                        .collect(),
                    snapshots,
                },
            )
            .collect();
        let mut bounded = options.clone();
        bounded.max_memory_bytes = options
            .max_memory_bytes
            .checked_sub(input_memory)
            .ok_or_else(|| limit("combined import/writer memory budget"))?;
        if initial.is_none() && !in_place {
            let mut input = ArchiveInput::new(applications);
            // Resources may belong to an application first written in a later
            // dependency pass. Only attach them once their owner is present.
            let mut held_resources = external.source_resources.borrow_mut();
            let resources = std::mem::take(&mut *held_resources);
            for resource in resources {
                if input.applications.iter().any(|a| {
                    a.platform == resource.platform && a.application_id == resource.application_id
                }) {
                    input.source_resources.push(resource);
                } else {
                    held_resources.push(resource);
                }
            }
            drop(held_resources);
            let stats = crate::write_archive(input, &work, &bounded)
                .map_err(|e| context(e, "write initial tree"))?;
            peak = stats.peak_temporary_disk_bytes;
            initial = Some(stats);
        } else {
            // The staged archive is temporary disk; an archive extended in
            // place is the caller's own file.
            let bytes = if in_place {
                0
            } else {
                std::fs::metadata(&work)?.len()
            };
            bounded.max_temporary_disk_bytes = options
                .max_temporary_disk_bytes
                .checked_sub(bytes)
                .ok_or_else(|| limit("combined staged/repack disk budget"))?;
            let mut input = ArchiveInput::new(applications);
            let mut held_resources = external.source_resources.borrow_mut();
            let resources = std::mem::take(&mut *held_resources);
            for resource in resources {
                if input.applications.iter().any(|a| {
                    a.platform == resource.platform && a.application_id == resource.application_id
                }) {
                    input.source_resources.push(resource);
                } else {
                    held_resources.push(resource);
                }
            }
            drop(held_resources);
            let stats = crate::repack_archive(
                &work,
                &work,
                RepackPlan {
                    remove_snapshots: Vec::new(),
                    additions: input,
                    metadata_sources: Vec::new(),
                },
                &bounded,
                IntegrityPolicy::Strict,
            )?;
            peak = peak.max(bytes + stats.peak_temporary_disk_bytes);
            additions.push(stats);
        }
        let summary = crate::inspect_archive(&work, &bounded, IntegrityPolicy::Fast)?;
        for key in ready {
            let v = &views[&key];
            let d = &v.declaration;
            let mut matches = summary
                .applications
                .iter()
                .filter(|a| a.platform == d.platform && a.application_id == d.application_id)
                .flat_map(|a| &a.snapshots)
                .filter(|s| {
                    s.program_id == v.scope.program_id.value
                        && s.numeric_version == d.numeric_version
                        && s.snapshot_type == d.snapshot_type
                        && s.content_id == d.content_id
                        && s.addon_title_id == d.addon_title_id
                });
            let id = matches
                .next()
                .ok_or_else(|| invalid("written snapshot scope absent"))?
                .identity;
            if matches.next().is_some() {
                return Err(invalid("ambiguous written snapshot scope"));
            }
            identities.insert(key, id);
        }
    }
    let mut bounded = options.clone();
    bounded.max_memory_bytes = options
        .max_memory_bytes
        .checked_sub(input_memory)
        .ok_or_else(|| limit("combined verification memory budget"))?;
    if !external.restored_metadata.borrow().is_empty()
        || !external.source_resources.borrow().is_empty()
    {
        return Err(invalid(
            "restoration metadata or resources have no written owner",
        ));
    }
    let pairs: Vec<_> = views.iter().map(|(key, v)| (identities[key], v)).collect();
    let mut verification_limits = limits.clone();
    verification_limits.max_inventory_bytes =
        limits.max_inventory_bytes.min(bounded.max_memory_bytes);
    // The writer already hashed every source byte as it streamed it and has
    // re-read the staged output against those digests. A second full source
    // read is opt-in; the cheap listing-based namespace check after writing
    // still rejects any ordinary change made since the inventory.
    let compared = if limits.compare_source_bytes {
        crate::writer::verify_extracted_source_files(&work, &pairs, &bounded)
            .map_err(|e| context(e, "byte comparison"))?
    } else {
        0
    };
    for watch in &watches {
        watch.check(&verification_limits, &options.cancellation)?;
    }
    let receipt = ImportReceipt {
        manifest: manifest.clone(),
        snapshots: views
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    ReceiptSnapshot {
                        identity: identities[k],
                        program: v.scope.program_id.clone(),
                        build: v.scope.build_id.clone(),
                        module_build_ids: v
                            .modules
                            .iter()
                            .map(|(p, m)| (p.clone(), m.build_id))
                            .collect(),
                        module_hash_checks: v
                            .modules
                            .iter()
                            .map(|(p, m)| (p.clone(), m.segment_hashes_checked))
                            .collect(),
                    },
                )
            })
            .collect(),
    };
    receipt.match_archive(&crate::inspect_archive(
        &work,
        &bounded,
        IntegrityPolicy::Fast,
    )?)?;
    receipt.encode(&verification_limits)?;
    cancelled(&options.cancellation)?;
    if let Some(stage) = stage.as_mut() {
        crate::writer::atomic_replace(&stage.path, destination)
            .map_err(|e| context(e, "final publication"))?;
        stage.disarm();
    }
    Ok(InnerStats {
        elapsed: started.elapsed(),
        decoded_executable_bytes: total_work,
        initial,
        additions,
        peak_temporary_disk_bytes: peak,
        source_bytes_compared: compared,
        receipt,
    })
}

/// Read a declared NACP and its icons into native snapshot metadata, with the
/// same projection as native conversion: every NACP title and every icon is
/// localized, and the first populated NACP entry is the display fallback.
/// Returns the metadata, the NACP display version and the bytes held.
pub(crate) fn control_metadata(
    root: &super::secure::Root,
    d: &SnapshotDeclaration,
    limits: &ImportLimits,
) -> Result<Option<(crate::SnapshotMetadata, Option<String>, u64)>, WriterError> {
    let Some(path) = &d.nacp else {
        return Ok(None);
    };
    let read = |path: &str, cap: u64, what: &str| -> Result<Vec<u8>, WriterError> {
        let mut bytes = Vec::new();
        root.open_relative(path)?
            .take(cap.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > cap {
            return Err(limit(&format!("{what} exceeds its byte limit")));
        }
        Ok(bytes)
    };
    let nacp = read(path, 0x4000, "control.nacp")?;
    if nacp.len() != 0x4000 {
        return Err(invalid("declared control.nacp is not 0x4000 bytes"));
    }
    let (titles, display_version, _) = crate::native::parse_nacp_text(&nacp);
    let mut held = nacp.len() as u64;
    let mut icons = BTreeMap::new();
    for (language, path) in &d.icons {
        let bytes = read(path, limits.max_image_bytes, "icon")?;
        let kind = if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
            crate::MediaType::PngIcon
        } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            crate::MediaType::JpegIcon
        } else {
            return Err(invalid("declared icon is neither PNG nor JPEG"));
        };
        held = held.saturating_add(bytes.len() as u64 * 2);
        icons.insert(language.as_str(), (kind, bytes));
    }
    let media = |entry: Option<&(crate::MediaType, Vec<u8>)>| {
        entry.map(|(kind, bytes)| crate::MediaInput::from_bytes(*kind, bytes.clone()))
    };
    let mut localized = Vec::new();
    for (native, language) in crate::native::NACP_LANGUAGE_MAP {
        let title = titles
            .iter()
            .find(|t| t.language == native)
            .and_then(|t| t.name.clone());
        let icon = media(icons.get(native));
        if title.is_some() || icon.is_some() {
            localized.push(crate::LocalizedMetadata {
                language: language.into(),
                title,
                icon,
            });
        }
    }
    let default = titles.first();
    Ok(Some((
        crate::SnapshotMetadata {
            title: default.and_then(|t| t.name.clone()),
            publisher: default.and_then(|t| t.publisher.clone()),
            icon: media(default.and_then(|t| icons.get(t.language.as_str()))),
            nacp: Some(crate::MediaInput::from_bytes(crate::MediaType::Nacp, nacp)),
            localized,
            opaque: Vec::new(),
        },
        display_version,
        held,
    )))
}

/// Strict archive verification plus byte-for-byte comparison with the receipt's
/// original source views. Composition uses the same resolver as conversion.
/// This does not authenticate a user-supplied receipt.
pub fn verify_extracted_sources(
    archive: &Path,
    package: &Path,
    receipt: &ImportReceipt,
    limits: &ImportLimits,
    options: &WriterOptions,
) -> Result<u64, WriterError> {
    let receipt = ImportReceipt::read(receipt.encode(limits)?.as_slice(), limits)?;
    let summary = crate::inspect_archive(archive, options, IntegrityPolicy::Fast)?;
    receipt.match_archive(&summary)?;
    drop(summary);
    let mut pending: BTreeMap<_, _> = receipt
        .manifest
        .snapshots
        .iter()
        .map(|d| (d.key.clone(), d.clone()))
        .collect();
    let mut views = BTreeMap::<String, TreeView>::new();
    let mut memory = receipt.manifest.bytes().len() as u64 * 8 + pending.len() as u64 * 4096;
    let mut work = 0u64;
    let mut bytes = 0u64;
    let mut watches = Vec::new();
    while !pending.is_empty() {
        cancelled(&options.cancellation)?;
        let ready: Vec<_> = pending
            .iter()
            .filter(|(_, d)| targets(d).iter().all(|k| views.contains_key(*k)))
            .map(|(k, _)| k.clone())
            .collect();
        if ready.is_empty() {
            return Err(invalid("unresolved verification dependency closure"));
        }
        for key in ready {
            pending.remove(&key);
            let mut remaining = limits.clone();
            remaining.max_inventory_bytes = limits
                .max_inventory_bytes
                .checked_sub(memory)
                .ok_or_else(|| limit("verification inventory budget"))?;
            remaining.max_executable_bytes = limits.max_executable_bytes.saturating_sub(work);
            remaining.max_total_bytes = limits.max_total_bytes.saturating_sub(bytes);
            let mut view = tree::inspect_tree(
                package,
                &receipt.manifest,
                &key,
                &remaining,
                &options.cancellation,
            )?;
            watches.push(view.watch.clone());
            work = work
                .checked_add(view.decoded_executable_bytes)
                .ok_or_else(|| limit("verification work overflow"))?;
            if let Some(c) = &view.declaration.composition {
                remaining.max_executable_bytes = limits.max_executable_bytes.saturating_sub(work);
                let base_key = c.base.clone();
                view = compose(
                    view,
                    &views[&base_key],
                    receipt.snapshots[&base_key].identity,
                    &remaining,
                )?;
                work = work
                    .checked_add(view.decoded_executable_bytes)
                    .ok_or_else(|| limit("verification composition work overflow"))?;
            }
            bytes = bytes
                .checked_add(view.files.values().map(|f| f.size).sum::<u64>())
                .ok_or_else(|| limit("verification source size overflow"))?;
            if bytes > limits.max_total_bytes || work > limits.max_executable_bytes {
                return Err(limit("verification source/work budget"));
            }
            let record = &receipt.snapshots[&key];
            if view.scope.program_id.value != record.program.value
                || view.scope.build_id.value != record.build.value
                || view
                    .modules
                    .iter()
                    .map(|(p, m)| (p.clone(), m.build_id))
                    .collect::<BTreeMap<_, _>>()
                    != record.module_build_ids
            {
                return Err(invalid("source executable facts differ from receipt"));
            }
            memory = memory
                .checked_add(view.accounted_bytes)
                .ok_or_else(|| limit("verification memory overflow"))?;
            if memory > limits.max_inventory_bytes {
                return Err(limit("verification inventory budget"));
            }
            views.insert(key, view);
        }
    }
    let mut bounded = options.clone();
    bounded.max_memory_bytes = options
        .max_memory_bytes
        .checked_sub(memory)
        .ok_or_else(|| limit("combined verification memory budget"))?;
    let mut watch_limits = limits.clone();
    watch_limits.max_inventory_bytes = limits.max_inventory_bytes.min(bounded.max_memory_bytes);
    for w in &watches {
        w.check(&watch_limits, &options.cancellation)?;
    }
    let pairs: Vec<_> = views
        .iter()
        .map(|(k, v)| (receipt.snapshots[k].identity, v))
        .collect();
    let compared = crate::writer::verify_extracted_source_files(archive, &pairs, &bounded)?;
    for w in &watches {
        w.check(&watch_limits, &options.cancellation)?;
    }
    Ok(compared)
}
