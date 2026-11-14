use super::structures::NpdmProfile;
use super::*;
use crate::{DependencyKind, ProgramKind};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

#[derive(Clone, Debug)]
pub struct Composition {
    pub base: String,
    pub exact_identity: [u8; 32],
    pub remove: BTreeSet<String>,
}
#[derive(Clone, Debug)]
pub struct SnapshotDeclaration {
    pub key: String,
    pub root: String,
    pub platform: Platform,
    pub application_id: u64,
    pub program_id: Option<u64>,
    pub program_kind: ProgramKind,
    pub snapshot_type: SnapshotType,
    pub numeric_version: u32,
    pub complete: bool,
    pub npdm_profile: NpdmProfile,
    pub build_id: Option<[u8; 32]>,
    pub content_id: Option<[u8; 16]>,
    /// DLC add-on title ID, declared when no native Content ID exists.
    pub addon_title_id: Option<u64>,
    pub dependencies: Vec<(DependencyKind, String)>,
    pub composition: Option<Composition>,
    pub exclusions: BTreeSet<String>,
    /// Child `programN` directories represented by separate executable snapshots.
    /// They are excluded from this snapshot's logical file tree, not discarded.
    pub program_views: BTreeSet<String>,
    /// Explicit ancillary root directories preserved below `data/`.
    pub data_views: BTreeSet<String>,
    pub presentation: BTreeMap<String, String>,
    /// Package-relative exact `control.nacp` (0x4000 bytes), outside the tree.
    pub nacp: Option<String>,
    /// Native NACP language name -> package-relative PNG/JPEG icon.
    pub icons: BTreeMap<String, String>,
}
#[derive(Clone, Debug)]
pub struct DeclarationManifest {
    pub(crate) raw: Vec<u8>,
    pub sha256: [u8; 32],
    pub snapshots: Vec<SnapshotDeclaration>,
}
#[derive(Clone, Debug)]
pub struct ManifestSelection {
    pub manifest: DeclarationManifest,
    pub original_manifest_sha256: [u8; 32],
    pub omitted_snapshot_keys: Vec<String>,
    pub retained_dependency_keys: Vec<String>,
}
/// Switch 1 and Switch 2 add-on title IDs observed so far: the application ID
/// plus 0x1000 in the high bits and a nonzero index in the low 12 bits, for
/// example 010015100b514000 to 010015100b515001, as NUP and dumps name them.
pub fn addon_belongs_to(addon: u64, application_id: u64) -> bool {
    application_id & 0xfff == 0
        && addon & !0xfff == application_id.wrapping_add(0x1000)
        && addon & 0xfff != 0
}

impl SnapshotDeclaration {
    /// A file directly in this snapshot's root that is its declared
    /// `control.nacp` or icon (dumps keep them beside exefs/ and romfs/).
    /// Such files are metadata, not part of the tree.
    pub fn declares_root_metadata(&self, name: &str) -> bool {
        let path = if self.root == "." {
            name.to_owned()
        } else {
            format!("{}/{name}", self.root)
        };
        self.nacp.as_deref() == Some(path.as_str()) || self.icons.values().any(|i| *i == path)
    }
    /// Value that tells DLC of one application apart: the Content ID, or
    /// the add-on title ID as it is stored in the Catalog.
    pub fn dlc_scope(&self) -> Option<[u8; 16]> {
        self.content_id.or_else(|| {
            self.addon_title_id.map(|id| {
                let mut bytes = [0u8; 16];
                bytes[..8].copy_from_slice(&id.to_le_bytes());
                bytes
            })
        })
    }
}

/// A nested Program is a separate snapshot with the same package version.
/// Auxiliary Programs introduced by an update may be standalone even when
/// the primary Program is an ordinary update.
fn is_program_view_child(
    parent: &SnapshotDeclaration,
    child: &str,
    other: &SnapshotDeclaration,
) -> bool {
    let index = child["program".len()..].parse::<u64>().unwrap_or(0);
    let expected = parent.program_id.map(|program| (program & !0xf) | index);
    let root = if parent.root == "." {
        child.to_owned()
    } else {
        format!("{}/{}", parent.root, child)
    };
    other.root == root
        && parent.program_id.is_some()
        && other.platform == parent.platform
        && other.application_id == parent.application_id
        && other.program_id == expected
        && other.program_id != parent.program_id
        && other.npdm_profile == parent.npdm_profile
        && other.npdm_profile != NpdmProfile::Absent
        && other.program_kind == ProgramKind::Auxiliary
        && other.numeric_version == parent.numeric_version
        && match parent.snapshot_type {
            SnapshotType::Base => other.snapshot_type == SnapshotType::Base,
            SnapshotType::Update | SnapshotType::StandaloneUpdate => matches!(
                other.snapshot_type,
                SnapshotType::Update | SnapshotType::StandaloneUpdate
            ),
            SnapshotType::Dlc | SnapshotType::Unknown => false,
        }
}

impl DeclarationManifest {
    /// Explicit space-saving export; dependency closure is retained and history
    /// removal is reported. Partial views require explicit composition closure.
    /// Exact content identities are checked during conversion, never guessed here.
    pub fn select_latest_complete(
        &self,
        dlc_keys: &BTreeSet<String>,
        limits: &ImportLimits,
    ) -> Result<ManifestSelection, WriterError> {
        let original = self.revalidated(limits)?;
        let mut latest = BTreeMap::new();
        let mut chosen = BTreeSet::new();
        let mut scopes = BTreeSet::new();
        for s in &original.snapshots {
            if s.snapshot_type == SnapshotType::Dlc {
                if dlc_keys.contains(&s.key) {
                    chosen.insert(s.key.clone());
                }
                continue;
            }
            let program = s.program_id.ok_or_else(|| {
                invalid("latest-only selection requires explicit Program associations")
            })?;
            let key = (s.platform as u8, s.application_id, program);
            if !scopes.insert((key, s.numeric_version)) {
                return Err(invalid("ambiguous declared Program version"));
            }
            if let Some((version, _)) = latest.get(&key) {
                if *version == s.numeric_version {
                    return Err(invalid("ambiguous latest Program version"));
                }
                if *version > s.numeric_version {
                    continue;
                }
            }
            latest.insert(key, (s.numeric_version, s.key.clone()));
        }
        for key in dlc_keys {
            if !original
                .snapshots
                .iter()
                .any(|s| s.key == *key && s.snapshot_type == SnapshotType::Dlc)
            {
                return Err(invalid("selected DLC key is absent/not DLC"));
            }
        }
        chosen.extend(latest.values().map(|(_, k)| k.clone()));
        if chosen.is_empty() {
            return Err(invalid("no declared complete version selected"));
        }
        let wanted = chosen.clone();
        loop {
            let before = chosen.len();
            for s in &original.snapshots {
                if chosen.contains(&s.key) {
                    chosen.extend(s.dependencies.iter().map(|(_, k)| k.clone()));
                    if let Some(c) = &s.composition {
                        chosen.insert(c.base.clone());
                    }
                    for child in &s.program_views {
                        let auxiliary = original
                            .snapshots
                            .iter()
                            .find(|other| is_program_view_child(s, child, other))
                            .ok_or_else(|| invalid("missing selected auxiliary Program"))?;
                        chosen.insert(auxiliary.key.clone());
                    }
                }
            }
            if chosen.len() == before {
                break;
            }
        }
        // Resolve the selected dependency graph before returning a plan. The
        // manifest alone cannot authenticate a nonzero exact NX identity;
        // convert_extracted must match it against the packed parent.
        let mut done = BTreeSet::new();
        while done.len() < original.snapshots.len() {
            let before = done.len();
            for s in &original.snapshots {
                if done.contains(&s.key) {
                    continue;
                }
                let deps_ready = s.dependencies.iter().all(|(_, k)| done.contains(k));
                if let Some(c) = &s.composition {
                    let parent = original
                        .snapshots
                        .iter()
                        .find(|p| p.key == c.base)
                        .ok_or_else(|| invalid("missing composition parent"))?;
                    if s.snapshot_type != SnapshotType::Update
                        || parent.snapshot_type == SnapshotType::Dlc
                        || s.platform != parent.platform
                        || s.application_id != parent.application_id
                        || s.program_id != parent.program_id
                        || s.program_kind != parent.program_kind
                        || s.numeric_version <= parent.numeric_version
                    {
                        return Err(invalid("unresolvable composition scope/version"));
                    }
                    if !done.contains(&c.base) {
                        continue;
                    }
                } else if !s.complete {
                    return Err(invalid("unresolved partial version"));
                }
                if deps_ready {
                    done.insert(s.key.clone());
                }
            }
            if before == done.len() {
                return Err(invalid("unresolved/cyclic composition dependency closure"));
            }
        }
        let mut text = "NXIMPORT\t1\n".to_owned();
        let raw = std::str::from_utf8(original.bytes()).map_err(|_| invalid("manifest UTF-8"))?;
        for line in raw.lines().skip(1) {
            let key = line
                .split('\t')
                .nth(1)
                .ok_or_else(|| invalid("manifest scope"))?;
            if chosen.contains(key) {
                text.push_str(line);
                text.push('\n');
            }
        }
        Ok(ManifestSelection {
            manifest: Self::read(text.as_bytes(), limits)?,
            original_manifest_sha256: original.sha256,
            omitted_snapshot_keys: original
                .snapshots
                .iter()
                .filter(|s| !chosen.contains(&s.key))
                .map(|s| s.key.clone())
                .collect(),
            retained_dependency_keys: chosen.difference(&wanted).cloned().collect(),
        })
    }
    pub fn read(reader: impl Read, limits: &ImportLimits) -> Result<Self, WriterError> {
        limits.validate()?;
        let mut raw = Vec::new();
        reader
            .take(
                limits
                    .max_manifest_bytes
                    .min(limits.max_inventory_bytes / 8)
                    .saturating_add(1),
            )
            .read_to_end(&mut raw)?;
        if raw.len() as u64
            > limits
                .max_manifest_bytes
                .min(limits.max_inventory_bytes / 8)
        {
            return Err(limit("manifest exceeds byte budget"));
        }
        let text = std::str::from_utf8(&raw).map_err(|_| invalid("manifest is not UTF-8"))?;
        if raw.len() as u64 * 8
            + text.lines().filter(|l| l.starts_with("snapshot\t")).count() as u64 * 4096
            > limits.max_inventory_bytes
        {
            return Err(limit("manifest decoded metadata budget"));
        }
        let mut lines = text.lines();
        if lines.next() != Some("NXIMPORT\t1") {
            return Err(unsupported("expected NXIMPORT version 1"));
        }
        let mut snapshots = Vec::new();
        let mut keys = BTreeMap::new();
        for line in lines {
            if line.is_empty() || line.len() > 16384 {
                return Err(invalid("empty/oversized manifest record"));
            }
            let f: Vec<_> = line.split('\t').collect();
            if f[0] == "snapshot" && f.len() == 12 {
                if snapshots.len() >= limits.max_snapshots {
                    return Err(limit("too many declared snapshots"));
                }
                if f[1].len() > 64
                    || f[1].is_empty()
                    || !f[1]
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err(invalid("invalid snapshot key"));
                }
                if keys.insert(f[1].to_owned(), snapshots.len()).is_some() {
                    return Err(invalid("duplicate snapshot key"));
                }
                if f[2] != "." {
                    portable_path(f[2], limits)?;
                }
                let platform = match f[3] {
                    "switch1" => Platform::Switch1,
                    "switch2" => Platform::Switch2,
                    _ => return Err(unsupported("explicit platform required")),
                };
                let app = u64::from_be_bytes(unhex::<8>(f[4])?);
                if app == 0 || f[4].len() != 16 {
                    return Err(invalid("application ID must be nonzero hex16"));
                }
                let program = if f[5] == "-" {
                    None
                } else {
                    let p = u64::from_be_bytes(unhex::<8>(f[5])?);
                    if p == 0 || f[5].len() != 16 {
                        return Err(invalid("Program ID must be nonzero hex16"));
                    }
                    Some(p)
                };
                let kind = match f[6] {
                    "primary" => ProgramKind::Primary,
                    "auxiliary" => ProgramKind::Auxiliary,
                    "none" if program.is_none() => ProgramKind::Unknown,
                    _ => return Err(invalid("invalid Program role")),
                };
                let ty = match f[7] {
                    "base" => SnapshotType::Base,
                    "update" => SnapshotType::Update,
                    "dlc" => SnapshotType::Dlc,
                    "standalone-update" => SnapshotType::StandaloneUpdate,
                    _ => return Err(unsupported("unknown snapshot type")),
                };
                let version = f[8]
                    .parse::<u32>()
                    .map_err(|_| invalid("invalid opaque numeric version"))?;
                let complete = match f[9] {
                    "complete" => true,
                    "partial" => false,
                    _ => return Err(invalid("explicit completeness required")),
                };
                if f[10] != "materialized-v1" {
                    return Err(unsupported("unknown extracted layout contract"));
                }
                let profile = match f[11] {
                    "none" => NpdmProfile::Absent,
                    "meta-acid2048-v1" => NpdmProfile::MetaAcid2048V1,
                    "meta-acid3072-v1" => NpdmProfile::MetaAcid3072V1,
                    _ => return Err(unsupported("unknown NPDM profile")),
                };
                snapshots.push(SnapshotDeclaration {
                    key: f[1].into(),
                    root: f[2].into(),
                    platform,
                    application_id: app,
                    program_id: program,
                    program_kind: kind,
                    snapshot_type: ty,
                    numeric_version: version,
                    complete,
                    npdm_profile: profile,
                    build_id: None,
                    content_id: None,
                    addon_title_id: None,
                    dependencies: Vec::new(),
                    composition: None,
                    exclusions: BTreeSet::new(),
                    program_views: BTreeSet::new(),
                    data_views: BTreeSet::new(),
                    presentation: BTreeMap::new(),
                    nacp: None,
                    icons: BTreeMap::new(),
                });
                continue;
            }
            let i = *keys
                .get(*f.get(1).ok_or_else(|| invalid("missing record scope"))?)
                .ok_or_else(|| invalid("record precedes its snapshot declaration"))?;
            let s = &mut snapshots[i];
            match (f[0], f.len()) {
                ("build", 3) => {
                    if s.build_id.replace(unhex(f[2])?).is_some() {
                        return Err(invalid("duplicate Build ID declaration"));
                    }
                }
                ("content", 3) => {
                    if s.content_id.replace(unhex(f[2])?).is_some() {
                        return Err(invalid("duplicate Content ID declaration"));
                    }
                }
                ("addon", 3) => {
                    let id = u64::from_be_bytes(unhex::<8>(f[2])?);
                    if f[2].len() != 16 || s.addon_title_id.replace(id).is_some() {
                        return Err(invalid("invalid or duplicate add-on title ID"));
                    }
                }
                ("dependency", 4) => {
                    let k = match f[2] {
                        "required-base" => DependencyKind::RequiredBase,
                        "requires" => DependencyKind::Requires,
                        _ => return Err(unsupported("dependency kind")),
                    };
                    if s.dependencies.iter().any(|d| d.1 == f[3]) {
                        return Err(invalid("duplicate dependency"));
                    }
                    s.dependencies.push((k, f[3].into()));
                }
                ("compose", 5) if f[2] == "replace-delete-v1" => {
                    if s.composition
                        .replace(Composition {
                            base: f[3].into(),
                            exact_identity: unhex(f[4])?,
                            remove: BTreeSet::new(),
                        })
                        .is_some()
                    {
                        return Err(invalid("duplicate composition"));
                    }
                }
                ("remove", 3) => {
                    portable_path(f[2], limits)?;
                    let c = s
                        .composition
                        .as_mut()
                        .ok_or_else(|| invalid("remove without composition"))?;
                    if !c.remove.insert(f[2].into()) {
                        return Err(invalid("duplicate tombstone"));
                    }
                }
                ("exclude", 3) => {
                    portable_path(f[2], limits)?;
                    if f[2].rsplit('/').next() != Some(".listing") {
                        return Err(unsupported(
                            "v1 artifact exclusions are exact .listing files only",
                        ));
                    }
                    if !s.exclusions.insert(f[2].into()) {
                        return Err(invalid("duplicate exclusion"));
                    }
                }
                ("data-view", 3) => {
                    if !matches!(f[2], "control" | "logo" | "legal" | "meta")
                        || !s.data_views.insert(f[2].to_owned())
                    {
                        return Err(invalid("invalid or duplicate ancillary data view"));
                    }
                }
                ("program-view", 3) => {
                    let name = f[2];
                    let index = name
                        .strip_prefix("program")
                        .and_then(|n| n.parse::<u8>().ok());
                    if !matches!(index, Some(1..=15))
                        || name != format!("program{}", index.unwrap())
                        || !s.program_views.insert(name.to_owned())
                    {
                        return Err(invalid("invalid or duplicate child Program view"));
                    }
                }
                ("title" | "publisher" | "display-version", 3) => {
                    if f[2].is_empty()
                        || f[2].len() > 4096
                        || f[2].chars().any(char::is_control)
                        || s.presentation.insert(f[0].into(), f[2].into()).is_some()
                    {
                        return Err(invalid("invalid/duplicate presentation declaration"));
                    }
                }
                ("nacp", 3) => {
                    portable_path(f[2], limits)?;
                    if s.nacp.replace(f[2].into()).is_some() {
                        return Err(invalid("duplicate NACP declaration"));
                    }
                }
                ("icon", 4) => {
                    portable_path(f[3], limits)?;
                    if !crate::native::NACP_LANGUAGE_MAP
                        .iter()
                        .any(|(native, _)| *native == f[2])
                    {
                        return Err(unsupported("icon language is not a native NACP language"));
                    }
                    if s.icons.insert(f[2].into(), f[3].into()).is_some() {
                        return Err(invalid("duplicate icon language"));
                    }
                }
                _ => return Err(unsupported("unknown declaration record")),
            }
        }
        if snapshots.is_empty() {
            return Err(invalid("manifest has no snapshots"));
        }
        for s in &snapshots {
            for child in &s.program_views {
                if s.program_kind == ProgramKind::Unknown
                    || snapshots
                        .iter()
                        .filter(|other| is_program_view_child(s, child, other))
                        .count()
                        != 1
                {
                    return Err(invalid(
                        "child Program view needs exactly one matching auxiliary snapshot/version",
                    ));
                }
            }
            if (s.nacp.is_some() || !s.icons.is_empty())
                && (s.snapshot_type == SnapshotType::Dlc || s.nacp.is_none())
            {
                return Err(invalid(
                    "control metadata needs an executable snapshot and an explicit NACP",
                ));
            }
            if s.snapshot_type != SnapshotType::Dlc && s.program_kind == ProgramKind::Unknown {
                return Err(invalid(
                    "executable Program role must be explicitly declared",
                ));
            }
            if s.complete == s.composition.is_some() {
                return Err(invalid(
                    "partial source requires composition; complete source must not compose",
                ));
            }
            if let Some(addon) = s.addon_title_id {
                if s.snapshot_type != SnapshotType::Dlc
                    || s.content_id.is_some()
                    || !addon_belongs_to(addon, s.application_id)
                {
                    return Err(invalid(
                        "an add-on title ID needs a DLC without Content ID, in its application's add-on range",
                    ));
                }
            }
            if s.snapshot_type == SnapshotType::Dlc
                && (s.program_id.is_some()
                    || (s.content_id.is_none() && s.addon_title_id.is_none())
                    || !s.complete
                    || s.npdm_profile != NpdmProfile::Absent
                    || s.program_kind != ProgramKind::Unknown
                    || s.build_id.is_some())
            {
                return Err(unsupported(
                    "DLC must be complete, non-executable and have a Content ID or add-on title ID",
                ));
            }
            if s.snapshot_type != SnapshotType::Dlc
                && s.program_id.is_none()
                && s.npdm_profile == NpdmProfile::Absent
            {
                return Err(invalid(
                    "Program ID requires a declaration or explicit NPDM profile",
                ));
            }
            if s.dependencies
                .iter()
                .any(|(_, k)| k == &s.key || !keys.contains_key(k))
                || s.composition.as_ref().is_some_and(|c| {
                    c.base == s.key || !keys.contains_key(&c.base) || c.exact_identity == [0; 32]
                })
            {
                return Err(invalid("missing/self/zero dependency or composition base"));
            }
        }
        Ok(Self {
            sha256: crate::hash::sha256(&raw),
            raw,
            snapshots,
        })
    }
    pub fn bytes(&self) -> &[u8] {
        &self.raw
    }
    pub(crate) fn revalidated(&self, limits: &ImportLimits) -> Result<Self, WriterError> {
        Self::read(self.raw.as_slice(), limits)
    }
}
