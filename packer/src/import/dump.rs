//! Associations for materialized dumps with separate `program/<index>` trees.
//! CNMT establishes title/type/version; validated NPDM establishes each Program.
//! Completeness of an extracted update remains the automatic import contract,
//! never a property inferred from a directory name or a CNMT content list.
use super::*;
use crate::native::{ContentMeta, ContentMetaType, ContentRecordType};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};
use structures::NpdmProfile;

pub struct ProgramDumpPlan {
    pub manifest: DeclarationManifest,
    pub notes: String,
}

/// Whether a relative selection belongs to a recognized extracted dump shape.
pub fn is_program_dump_component(path: &str) -> bool {
    location(path.strip_suffix("/program").unwrap_or(path)).is_ok()
}

#[derive(Clone)]
struct Location {
    group: String,
    kind: SnapshotType,
    version: u32,
    addon: Option<u64>,
    index: Option<u8>,
}

fn location(path: &str) -> Result<Location, WriterError> {
    let mut parts: Vec<_> = path.split('/').collect();
    let index = if parts.len() >= 2 && parts[parts.len() - 2] == "program" {
        let value = parts.pop().unwrap();
        let index = value
            .parse::<u8>()
            .ok()
            .filter(|i| *i <= 15 && value == i.to_string());
        parts.pop();
        Some(index.ok_or_else(|| invalid("invalid extracted Program index"))?)
    } else {
        None
    };
    let group = if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    };
    if parts.first() == Some(&"dlc") && parts.last() == Some(&"base") {
        parts.pop();
    }
    let id = |s: &str| {
        if s.len() == 16 {
            u64::from_str_radix(s, 16).ok()
        } else {
            None
        }
    };
    let (kind, version, addon) = match parts.as_slice() {
        [] | ["base"] | ["."] => (SnapshotType::Base, 0, None),
        ["updates", v] => (
            SnapshotType::Update,
            v.parse()
                .map_err(|_| invalid("invalid dump update version"))?,
            None,
        ),
        ["dlc", a] => (
            SnapshotType::Dlc,
            0,
            Some(id(a).ok_or_else(|| invalid("invalid dump add-on ID"))?),
        ),
        ["dlc", a, v] => (
            SnapshotType::Dlc,
            v.parse().map_err(|_| invalid("invalid dump DLC version"))?,
            Some(id(a).ok_or_else(|| invalid("invalid dump add-on ID"))?),
        ),
        _ => {
            return Err(invalid(&format!(
                "unrecognized extracted layout {path:?}; explicit manifest required"
            )))
        }
    };
    Ok(Location {
        group,
        kind,
        version,
        addon,
        index,
    })
}

fn read(
    root: &SourceRoot,
    path: &str,
    budget: &mut u64,
    ceiling: u64,
) -> Result<Vec<u8>, WriterError> {
    let file = root.open_relative(path)?;
    let len = file.metadata()?.len();
    if !file.metadata()?.is_file() || len > 1024 * 1024 || len > ceiling.saturating_sub(*budget) {
        return Err(limit("extracted metadata budget exceeded"));
    }
    let mut bytes = Vec::new();
    file.take(len + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != len {
        return Err(invalid("extracted metadata changed while reading"));
    }
    *budget += len;
    Ok(bytes)
}

/// Returns None for the older direct ExeFS/RomFS layout. Selection limits
/// stored content, while sibling CNMT may still establish shared associations.
pub fn plan_program_dump(
    package: &Path,
    selected: &[String],
    limits: &ImportLimits,
    token: &CancellationToken,
) -> Result<Option<ProgramDumpPlan>, WriterError> {
    let candidates = discover(package, limits)?;
    if !candidates
        .iter()
        .any(|c| c.relative_root.split('/').any(|p| p == "program"))
    {
        return Ok(None);
    }
    let root = SourceRoot::open(package)?;
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut locations = BTreeMap::new();
    for c in &candidates {
        let loc = match location(&c.relative_root) {
            Ok(loc) => loc,
            Err(_) if !selected.is_empty() && !selected.contains(&c.relative_root) => continue,
            Err(error) => return Err(error),
        };
        groups
            .entry(loc.group.clone())
            .or_default()
            .push(c.relative_root.clone());
        locations.insert(c.relative_root.clone(), loc);
    }
    for s in selected {
        if !locations.contains_key(s) {
            return Err(invalid("selected extracted tree is absent"));
        }
    }
    if !selected.is_empty() {
        groups.retain(|_, roots| roots.iter().any(|root| selected.contains(root)));
    }
    let mut budget = 0;
    let mut metas: BTreeMap<String, ContentMeta> = BTreeMap::new();
    for (group, trees) in &groups {
        cancelled(token)?;
        let mut paths: BTreeSet<_> = trees.iter().map(|p| format!("{p}/meta")).collect();
        paths.insert(if group == "." {
            "meta".to_owned()
        } else {
            format!("{group}/meta")
        });
        let mut descriptor = None;
        for path in paths {
            let parent = path.strip_suffix("/meta").unwrap_or("");
            if !root
                .names(parent, limits.max_nodes)?
                .iter()
                .any(|name| name == "meta")
            {
                continue;
            }
            let entries = root.list(&path, limits.max_nodes)?;
            for entry in entries {
                if entry.dir || !entry.name.ends_with(".cnmt") {
                    continue;
                }
                let bytes = read(
                    &root,
                    &format!("{path}/{}", entry.name),
                    &mut budget,
                    limits.max_inventory_bytes / 4,
                )?;
                if descriptor.as_ref().is_some_and(|old| old != &bytes) {
                    return Err(invalid(
                        "conflicting CNMT descriptors in one extracted package",
                    ));
                }
                descriptor = Some(bytes);
            }
        }
        let bytes = descriptor.ok_or_else(|| {
            invalid(&format!(
                "{group}: separate Program layout requires a shared CNMT descriptor"
            ))
        })?;
        let meta = crate::native::nca::parse_cnmt(&bytes)
            .map_err(|e| invalid(&format!("{group}: {e}")))?;
        let loc = &locations[&trees[0]];
        let wanted = match loc.kind {
            SnapshotType::Base => ContentMetaType::Application,
            SnapshotType::Update => ContentMetaType::Patch,
            _ => ContentMetaType::AddOnContent,
        };
        if meta.meta_type != wanted
            || (loc.kind != SnapshotType::Base && meta.version != loc.version)
            || loc.addon.is_some_and(|a| a != meta.title_id)
        {
            return Err(invalid(&format!(
                "{group}: directory role/version/ID conflicts with CNMT"
            )));
        }
        if loc.kind == SnapshotType::Dlc && !addon_belongs_to(meta.title_id, meta.application_id) {
            return Err(invalid("CNMT add-on ID does not belong to its application"));
        }
        if meta.platform_byte > 1 {
            return Err(unsupported("unknown extracted CNMT platform marker"));
        }
        if meta.meta_type == ContentMetaType::Patch {
            let native_family =
                (meta.application_id & 0x00ff_ffff_ffff_f000) | 0x0400_0000_0000_0000;
            if meta.title_id != (meta.application_id | 0x800)
                && meta.title_id != (native_family | 0x800)
            {
                return Err(invalid("CNMT patch title does not match its application"));
            }
        }
        metas.insert(group.clone(), meta);
    }
    let chosen: Vec<_> = candidates
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.relative_root))
        .collect();
    let mut application = None;
    let mut programs = BTreeMap::new();
    let mut roots = BTreeMap::new();
    for c in &chosen {
        cancelled(token)?;
        let loc = &locations[&c.relative_root];
        let meta = &metas[&loc.group];
        if application.is_some_and(|a| a != meta.application_id) {
            return Err(invalid(
                "extracted packages belong to different applications",
            ));
        }
        application = Some(meta.application_id);
        if loc.kind == SnapshotType::Dlc {
            if root
                .names(&c.relative_root, limits.max_nodes)?
                .iter()
                .any(|n| n == "exefs")
            {
                return Err(unsupported("executable extracted DLC"));
            }
            continue;
        }
        let bytes = read(
            &root,
            &format!("{}/exefs/main.npdm", c.relative_root),
            &mut budget,
            limits.max_inventory_bytes / 4,
        )?;
        let (program, profile, name) = [
            (NpdmProfile::MetaAcid2048V1, "meta-acid2048-v1"),
            (NpdmProfile::MetaAcid3072V1, "meta-acid3072-v1"),
        ]
        .into_iter()
        .find_map(|(profile, name)| {
            structures::validate_npdm(&bytes, profile)
                .ok()
                .map(|p| (p.program_id, profile, name))
        })
        .ok_or_else(|| invalid("invalid extracted Program NPDM"))?;
        let index = loc.index.unwrap_or((program & 0xf) as u8);
        let app = meta.application_id;
        let native_family = (app & 0x00ff_ffff_ffff_f000) | 0x0400_0000_0000_0000;
        let identity_matches = (program == (app | u64::from(index))
            && ((app >> 56 == 1 && profile == NpdmProfile::MetaAcid2048V1)
                || (app >> 56 == 4 && profile == NpdmProfile::MetaAcid3072V1)))
            || (app >> 56 == 1
                && program == (native_family | u64::from(index))
                && profile == NpdmProfile::MetaAcid3072V1);
        if app & 0xfff != 0
            || !identity_matches
            || !meta
                .contents
                .iter()
                .any(|r| r.record_type == ContentRecordType::Program && r.id_offset == index)
        {
            return Err(invalid(&format!(
                "{}: NPDM Program {program:016x} conflicts with CNMT application/index",
                c.relative_root
            )));
        }
        if roots
            .insert(
                (program, loc.kind as u8, meta.version),
                c.relative_root.clone(),
            )
            .is_some()
        {
            return Err(invalid("duplicate extracted Program/type/version"));
        }
        programs.insert(c.relative_root.clone(), (program, profile, name));
    }
    let application = application.ok_or_else(|| invalid("no selected extracted packages"))?;
    if programs.is_empty() {
        return Err(invalid("DLC alone cannot establish an executable dump"));
    }
    let ounce: BTreeSet<_> = programs
        .values()
        .filter(|(p, _, _)| p >> 56 == 4)
        .map(|(p, _, _)| *p)
        .collect();
    let upgrade_programs: BTreeSet<_> = ounce
        .iter()
        .copied()
        .filter(|p| {
            metas.values().any(|m| {
                m.meta_type == ContentMetaType::AddOnContent
                    && m.title_id == application + 0x1000 + (p & 0xfff)
            })
        })
        .collect();
    let primary = if upgrade_programs.len() == 1 {
        *upgrade_programs.iter().next().unwrap()
    } else if upgrade_programs.len() > 1 {
        return Err(invalid("ambiguous extracted upgrade Programs"));
    } else if ounce.contains(&((application & 0x00ff_ffff_ffff_f000) | 0x0400_0000_0000_0000)) {
        (application & 0x00ff_ffff_ffff_f000) | 0x0400_0000_0000_0000
    } else if ounce.len() == 1 {
        *ounce.iter().next().unwrap()
    } else if ounce.is_empty() && programs.values().any(|(p, _, _)| *p == application) {
        application
    } else {
        return Err(invalid("no unambiguous primary extracted Program"));
    };
    let platform = if ounce.is_empty() {
        "switch1"
    } else {
        "switch2"
    };
    let keys: BTreeMap<_, _> = chosen
        .iter()
        .enumerate()
        .map(|(i, c)| (c.relative_root.clone(), format!("dump-{i}")))
        .collect();
    let mut text = "NXIMPORT\t1\n".to_owned();
    let mut records = Vec::new();
    for c in &chosen {
        cancelled(token)?;
        let loc = &locations[&c.relative_root];
        let meta = &metas[&loc.group];
        let key = &keys[&c.relative_root];
        let (program, role, profile) =
            if let Some((program, _, profile)) = programs.get(&c.relative_root) {
                (
                    format!("{program:016x}"),
                    if *program == primary {
                        "primary"
                    } else {
                        "auxiliary"
                    },
                    *profile,
                )
            } else {
                ("-".to_owned(), "none", "none")
            };
        let base = programs.get(&c.relative_root).and_then(|(program, _, _)| {
            roots
                .iter()
                .filter(|((p, kind, v), _)| {
                    p == program && *kind == SnapshotType::Base as u8 && *v <= meta.version
                })
                .max_by_key(|((_, _, v), _)| *v)
                .map(|(_, root)| root)
        });
        let kind = match loc.kind {
            SnapshotType::Base => "base",
            SnapshotType::Dlc => "dlc",
            _ if base.is_some() => "update",
            _ => "standalone-update",
        };
        text.push_str(&format!("snapshot\t{key}\t{}\t{platform}\t{application:016x}\t{program}\t{role}\t{kind}\t{}\tcomplete\tmaterialized-v1\t{profile}\n", c.relative_root, meta.version));
        // Dependencies and presentation records follow all snapshot declarations.
        if kind == "update" {
            records.push(format!(
                "dependency\t{key}\trequired-base\t{}\n",
                keys[base.unwrap()]
            ));
        }
        if loc.kind == SnapshotType::Dlc {
            records.push(format!("addon\t{key}\t{:016x}\n", meta.title_id));
            if let Some(required) = meta.required_application_version.filter(|v| *v != 0) {
                let parent = chosen
                    .iter()
                    .filter(|c| {
                        programs
                            .get(&c.relative_root)
                            .is_some_and(|(p, _, _)| *p == primary)
                    })
                    .filter(|c| metas[&locations[&c.relative_root].group].version >= required)
                    .min_by_key(|c| metas[&locations[&c.relative_root].group].version)
                    .ok_or_else(|| invalid("DLC requires an absent application version"))?;
                records.push(format!(
                    "dependency\t{key}\trequires\t{}\n",
                    keys[&parent.relative_root]
                ));
            }
        }
        let names = root.names(&c.relative_root, limits.max_nodes)?;
        for name in names
            .iter()
            .filter(|n| matches!(n.as_str(), "control" | "logo" | "legal" | "meta"))
        {
            records.push(format!("data-view\t{key}\t{name}\n"));
        }
        if loc.kind != SnapshotType::Dlc {
            let control = if names.iter().any(|n| n == "control") {
                format!("{}/control", c.relative_root)
            } else {
                c.relative_root.clone()
            };
            let control_names = root.names(&control, limits.max_nodes)?;
            if control_names.iter().any(|n| n == "control.nacp") {
                let path = format!("{control}/control.nacp");
                let bytes = read(&root, &path, &mut budget, limits.max_inventory_bytes / 4)?;
                if bytes.len() != 0x4000 {
                    return Err(invalid("extracted NACP has invalid size"));
                }
                records.push(format!("nacp\t{key}\t{path}\n"));
                let (titles, version, _) = crate::native::parse_nacp_text(&bytes);
                for (field, value) in [
                    ("title", titles.first().and_then(|t| t.name.clone())),
                    (
                        "publisher",
                        titles.first().and_then(|t| t.publisher.clone()),
                    ),
                    ("display-version", version),
                ] {
                    if let Some(value) = value.filter(|v| !v.is_empty()) {
                        records.push(format!("{field}\t{key}\t{value}\n"));
                    }
                }
                for (language, _) in crate::native::NACP_LANGUAGE_MAP {
                    let name = format!("icon_{language}.dat");
                    if control_names.contains(&name) {
                        records.push(format!("icon\t{key}\t{language}\t{control}/{name}\n"));
                    }
                }
            }
        }
    }
    for record in records {
        text.push_str(&record);
    }
    let manifest = DeclarationManifest::read(text.as_bytes(), limits)?;
    Ok(Some(ProgramDumpPlan { manifest, notes: "OBSERVED: application, snapshot types and numeric versions from CNMT; Program identities and profiles from validated NPDM.\nPRESUMED: selected extracted updates are complete materialized trees, as required by automatic pack-tree; CNMT alone does not prove completeness or authenticate extracted payloads. No base files are composed or guessed.\n".to_owned() }))
}

#[cfg(test)]
mod tests;
