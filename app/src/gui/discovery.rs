//! Lightweight source discovery on the command worker, never on the UI thread.
use super::{Content, Source, SourceKind};
use nxformat_packer::{import, native, CancellationToken, Platform, SnapshotType};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

fn bounded(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > limit {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

fn display_text(nacp: &[u8]) -> (String, String) {
    if nacp.len() < 0x4000 {
        return Default::default();
    }
    let text = |field: &[u8]| {
        let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
        String::from_utf8_lossy(&field[..end]).trim().to_owned()
    };
    (0..16)
        .find_map(|i| {
            let start = i * 0x300;
            let name = text(&nacp[start..start + 0x200]);
            (!name.is_empty()).then(|| (name, text(&nacp[start + 0x200..start + 0x300])))
        })
        .unwrap_or_default()
}

fn declared_source(
    path: &Path,
    selected: &BTreeSet<String>,
    declarations: &import::DeclarationManifest,
) -> Source {
    Source {
        path: path.to_owned(),
        kind: SourceKind::Extracted,
        tree_roots: selected.iter().cloned().collect(),
        supported: true,
        selected: true,
        contents: declarations
            .snapshots
            .iter()
            .filter(|s| selected.is_empty() || selected.contains(&s.root))
            .map(|s| Content {
                platform: format!("{:?}", s.platform),
                kind: match s.snapshot_type {
                    SnapshotType::Base => "base",
                    SnapshotType::Dlc => "dlc",
                    SnapshotType::StandaloneUpdate => "standalone",
                    _ => "update",
                }
                .into(),
                application: s.application_id,
                version: s.numeric_version,
                display: s
                    .presentation
                    .get("display-version")
                    .cloned()
                    .unwrap_or_default(),
                name: s.presentation.get("title").cloned().unwrap_or_default(),
                publisher: s.presentation.get("publisher").cloned().unwrap_or_default(),
            })
            .collect(),
        ..Default::default()
    }
}

fn extracted(
    path: &Path,
    selected: &BTreeSet<String>,
    token: &CancellationToken,
) -> Option<Source> {
    let has_restore_recipe = path.join(nxformat_packer::RESTORATION_FILE).is_file();
    let mut restoration_issue = None;
    let mut manifest_issue = None;
    if has_restore_recipe {
        match nxformat_packer::inspect_restoration(path, &Default::default()) {
            Ok(declarations) => {
                let mut source = declared_source(path, selected, &declarations);
                source.has_restore_recipe = true;
                return Some(source);
            }
            Err(error) => {
                restoration_issue = Some(format!(
                "Invalid restoration recipe: {error}. Choose normal tree import explicitly; exact restoration and recipe-only metadata will be unavailable."
            ));
            }
        }
    }
    if path
        .join(import::EXTRACTION_MANIFEST)
        .symlink_metadata()
        .is_ok()
    {
        match import::inspect_extraction_manifest(path, &import::ImportLimits::default()) {
            Ok(declarations) => {
                let mut source = declared_source(path, selected, &declarations);
                source.has_restore_recipe = has_restore_recipe;
                if let Some(issue) = restoration_issue {
                    source.restoration_invalid = true;
                    source.selected = false;
                    source.issue = issue;
                }
                return Some(source);
            }
            Err(error) => {
                manifest_issue = Some(format!("Invalid independent import manifest: {error}. Supply corrected explicit declarations; associations will not be guessed."));
            }
        }
    }
    // Use exactly the same native association planner as pack-tree, including
    // sibling CNMT, per-Program NACP and explicit component selections.
    if manifest_issue.is_none() && restoration_issue.is_none() {
        match import::plan_program_dump(
            path,
            &selected.iter().cloned().collect::<Vec<_>>(),
            &import::ImportLimits::default(),
            token,
        ) {
            Ok(Some(plan)) => {
                let mut source = declared_source(path, selected, &plan.manifest);
                for s in &plan.manifest.snapshots {
                    if source.icon.is_none() {
                        source.icon = s
                            .icons
                            .values()
                            .find_map(|p| bounded(&path.join(p), 16 * 1024 * 1024));
                    }
                }
                source.issue = "Extracted updates must contain complete materialized files; no missing base content is reconstructed.".into();
                return Some(source);
            }
            Err(error) => {
                return Some(Source {
                    path: path.to_owned(),
                    kind: SourceKind::Extracted,
                    issue: error.to_string(),
                    ..Default::default()
                })
            }
            Ok(None) => {}
        }
    }
    let mut candidates = match import::discover(path, &import::ImportLimits::default()) {
        Ok(candidates) => candidates,
        Err(error) if restoration_issue.is_some() => {
            return Some(Source {
                path: path.to_owned(),
                kind: SourceKind::Extracted,
                has_restore_recipe,
                restoration_invalid: true,
                issue: format!(
                    "{} Tree discovery failed: {error}",
                    restoration_issue.unwrap()
                ),
                ..Default::default()
            })
        }
        Err(_) => return None,
    };
    let package_has_base = candidates.iter().any(|c| c.relative_root == "base");
    if !selected.is_empty() {
        candidates.retain(|c| selected.contains(&c.relative_root));
    }
    if candidates.is_empty() {
        return restoration_issue.map(|issue| Source {
            path: path.to_owned(),
            kind: SourceKind::Extracted,
            has_restore_recipe,
            restoration_invalid: true,
            issue,
            ..Default::default()
        });
    }
    let mut source = Source {
        path: path.to_owned(),
        kind: SourceKind::Extracted,
        tree_roots: selected.iter().cloned().collect(),
        supported: true,
        selected: true,
        ..Default::default()
    };
    for c in candidates {
        let root = if c.relative_root == "." {
            path.to_owned()
        } else {
            path.join(&c.relative_root)
        };
        let kind = if c.relative_root.starts_with("updates/") {
            "update"
        } else if c.relative_root.starts_with("dlc/") {
            "dlc"
        } else {
            "base"
        };
        let npdm = bounded(&root.join("exefs/main.npdm"), 1024 * 1024);
        let program = npdm.and_then(|bytes| {
            [
                import::structures::NpdmProfile::MetaAcid2048V1,
                import::structures::NpdmProfile::MetaAcid3072V1,
            ]
            .iter()
            .find_map(|&profile| import::structures::validate_npdm(&bytes, profile).ok())
            .map(|n| n.program_id)
        });
        let (name, publisher) = bounded(&root.join("control.nacp"), 1024 * 1024)
            .map(|b| display_text(&b))
            .unwrap_or_default();
        // Root artwork is optional for extracted layouts. No recursive media search.
        if source.icon.is_none() && kind != "dlc" {
            source.icon = native::NACP_LANGUAGE_MAP.iter().find_map(|(language, _)| {
                bounded(&root.join(format!("icon_{language}.dat")), 16 * 1024 * 1024)
            });
        }
        // updates/<version>/ and dlc/<id>/<version>/: the folder names the version.
        let version = if kind == "base" {
            0
        } else {
            root.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0)
        };
        source.contents.push(Content {
            kind: kind.into(),
            application: program.unwrap_or(0),
            version,
            name,
            publisher,
            ..Default::default()
        });
    }
    // No base, one update: that update holds the whole game (standalone),
    // as in a dump whose base folder is empty. Its DLC may sit beside it.
    let has_base = source.contents.iter().any(|c| c.kind == "base");
    let mut updates = source.contents.iter_mut().filter(|c| c.kind == "update");
    if let (false, Some(update), None) =
        (has_base || package_has_base, updates.next(), updates.next())
    {
        if update.application != 0 {
            update.kind = "standalone".into();
        }
    }
    if !source
        .contents
        .iter()
        .any(|c| (c.kind == "base" || c.kind == "standalone") && c.application != 0)
    {
        source.supported = false;
        source.selected = false;
        let base = path.join("base");
        source.issue = if package_has_base && !has_base {
            "Add this dump's base folder to convert its updates and DLC."
        } else if base.is_dir() && !base.join("exefs/main.npdm").is_file() {
            "The base folder has no game in it (no exefs/main.npdm): this dump holds only updates or DLC. Add the base game to convert it."
        } else {
            "No base game found (no exefs/main.npdm). Add the base game, or use an import manifest for this layout."
        }
        .into();
    }
    source.has_restore_recipe = has_restore_recipe;
    if let Some(issue) = restoration_issue {
        source.restoration_invalid = true;
        source.selected = false;
        if !source.issue.is_empty() {
            source.issue.push(' ');
        }
        source.issue.push_str(&issue);
    }
    if let Some(issue) = manifest_issue {
        source.supported = false;
        source.selected = false;
        if !source.issue.is_empty() {
            source.issue.push(' ');
        }
        source.issue.push_str(&issue);
    }
    Some(source)
}

/// Resolve a selected canonical component to its enclosing extracted dump.
fn extracted_selection(path: &Path) -> Option<(PathBuf, Vec<String>)> {
    let mut program_container = None;
    for root in path.ancestors().skip(1).take(8) {
        let parts: Vec<_> = path
            .strip_prefix(root)
            .ok()?
            .iter()
            .map(|part| part.to_str())
            .collect::<Option<_>>()?;
        let id = |value: &str| value.len() == 16 && u64::from_str_radix(value, 16).is_ok();
        let known = match parts.as_slice() {
            ["base" | "updates" | "dlc"] => true,
            ["updates", version] => version.parse::<u32>().is_ok(),
            ["dlc", title] => id(title),
            ["dlc", title, version] => id(title) && version.parse::<u32>().is_ok(),
            _ => false,
        };
        if !known && !import::is_program_dump_component(&parts.join("/")) {
            continue;
        }
        let prefix = parts.join("/");
        let Ok(candidates) = import::discover(root, &import::ImportLimits::default()) else {
            continue;
        };
        let roots: Vec<_> = candidates
            .into_iter()
            .map(|c| c.relative_root)
            .filter(|candidate| {
                candidate == &prefix
                    || candidate
                        .strip_prefix(&prefix)
                        .is_some_and(|tail| tail.starts_with('/'))
            })
            .collect();
        if !roots.is_empty() {
            if parts.first() == Some(&"program") {
                if program_container.is_none() {
                    program_container = Some((root.to_owned(), roots));
                }
                continue;
            }
            return Some((root.to_owned(), roots));
        }
    }
    program_container
}

fn is_nup(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("nup"))
}

fn nup(path: PathBuf, token: &CancellationToken) -> Source {
    let mut source = Source {
        kind: SourceKind::Nup,
        path,
        ..Default::default()
    };
    match import::inspect_nup(&source.path, &import::ImportLimits::for_nup(), token) {
        Ok(info) => {
            source.size = info.size;
            source.supported = true;
            source.selected = true;
            source.nup_platform = (info.platform != Platform::Unknown).then_some(info.platform);
            source.icon = info.icon;
            let title = info.title.unwrap_or_default();
            let publisher = info.publisher.unwrap_or_default();
            source.contents = info
                .packages
                .into_iter()
                .map(|package| {
                    let kind = match package.kind {
                        SnapshotType::Base => "base",
                        SnapshotType::Update => "update",
                        SnapshotType::StandaloneUpdate => "standalone",
                        SnapshotType::Dlc => "dlc",
                        SnapshotType::Unknown => "other",
                    };
                    Content {
                        kind: kind.into(),
                        application: info.application_id,
                        version: package.version,
                        display: package.display_version.unwrap_or_default(),
                        name: title.clone(),
                        publisher: publisher.clone(),
                        ..Default::default()
                    }
                })
                .collect();
        }
        Err(error) => source.issue = error.to_string(),
    }
    source
}

fn collect_nups(
    dir: &Path,
    depth: u32,
    options: &native::InventoryOptions,
    paths: &mut Vec<PathBuf>,
    token: &CancellationToken,
) -> Result<(), String> {
    if depth >= options.max_directory_depth {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        if token.is_cancelled() {
            return Err("Cancelled".into());
        }
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            collect_nups(&entry.path(), depth + 1, options, paths, token)?;
        } else if kind.is_file() && is_nup(&entry.path()) {
            if paths.len() >= options.max_sources {
                return Err("Too many NUP sources.".into());
            }
            paths.push(entry.path());
        }
    }
    Ok(())
}

pub fn scan(paths: Vec<PathBuf>, token: &CancellationToken) -> Result<Vec<Source>, String> {
    let mut result = Vec::new();
    let mut packages = Vec::new();
    let mut nups = Vec::new();
    let mut extracted_dumps = BTreeMap::<PathBuf, Option<BTreeSet<String>>>::new();
    let options = native::InventoryOptions::default();
    for path in paths {
        if token.is_cancelled() {
            return Err("Cancelled".into());
        }
        if path.is_dir() {
            let path = std::path::absolute(&path).map_err(|e| e.to_string())?;
            if path.join(nxformat_packer::RESTORATION_FILE).is_file()
                || path
                    .join(import::EXTRACTION_MANIFEST)
                    .symlink_metadata()
                    .is_ok()
            {
                extracted_dumps.insert(path, None);
                continue;
            }
            if let Some((root, selected)) = extracted_selection(&path) {
                if let Some(roots) = extracted_dumps
                    .entry(root)
                    .or_insert_with(|| Some(BTreeSet::new()))
                {
                    roots.extend(selected);
                }
                continue;
            }
            if import::discover(&path, &import::ImportLimits::default())
                .is_ok_and(|c| !c.is_empty())
            {
                extracted_dumps.insert(path, None);
                continue;
            }
            collect_nups(&path, 0, &options, &mut nups, token)?;
            packages.push(path);
            continue;
        } else if is_nup(&path) {
            nups.push(path);
            continue;
        }
        packages.push(path);
    }
    for (path, selected) in extracted_dumps {
        if token.is_cancelled() {
            return Err("Cancelled".into());
        }
        if let Some(source) = extracted(&path, &selected.unwrap_or_default(), token) {
            result.push(source);
        }
    }
    nups.sort();
    nups.dedup();
    for path in nups {
        if token.is_cancelled() {
            return Err("Cancelled".into());
        }
        result.push(nup(path, token));
    }
    if !packages.is_empty() {
        let inventory = native::inventory(&packages, &options).map_err(|e| e.to_string())?;
        for s in inventory.sources {
            if token.is_cancelled() {
                return Err("Cancelled".into());
            }
            let supported = s.status == native::SourceStatus::Supported;
            let mut source = Source {
                path: s.path.clone(),
                size: s.size,
                firmware_bytes: s.system_update_bytes.unwrap_or_default(),
                supported,
                selected: supported,
                issue: s
                    .issues
                    .iter()
                    .filter(|i| i.severity == native::Severity::Error)
                    .map(|i| i.message.clone())
                    .collect::<Vec<_>>()
                    .join(" · "),
                ..Default::default()
            };
            for m in s.metas {
                let kind = match m.meta.meta_type {
                    native::ContentMetaType::Application => "base",
                    native::ContentMetaType::Patch => "update",
                    native::ContentMetaType::AddOnContent => "dlc",
                    _ => continue,
                };
                let mut content = Content {
                    kind: kind.into(),
                    application: m.meta.application_id,
                    version: m.meta.version,
                    platform: format!("{:?}", m.meta.generation),
                    ..Default::default()
                };
                if let Some(control) = m.meta.control {
                    let title = control
                        .titles
                        .iter()
                        .find(|t| t.name.as_ref().is_some_and(|s| !s.trim().is_empty()));
                    content.name = title.and_then(|t| t.name.clone()).unwrap_or_default();
                    content.publisher = title.and_then(|t| t.publisher.clone()).unwrap_or_default();
                    content.display = control.display_version.unwrap_or_default();
                    if source.icon.is_none() && kind != "dlc" {
                        let language = control
                            .titles
                            .iter()
                            .find(|t| t.name.as_ref().is_some_and(|s| !s.trim().is_empty()))
                            .map(|t| t.language.as_str());
                        let range = control
                            .icon_ranges
                            .iter()
                            .find(|r| Some(r.0.as_str()) == language)
                            .or_else(|| control.icon_ranges.first());
                        if let Some((_, offset, size, _)) = range {
                            if *size <= 16 * 1024 * 1024 {
                                if let Ok(mut file) = File::open(&s.path) {
                                    if file.seek(SeekFrom::Start(*offset)).is_ok() {
                                        let mut bytes = vec![0; *size as usize];
                                        if file.read_exact(&mut bytes).is_ok() {
                                            source.icon = Some(bytes);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                source.contents.push(content);
            }
            result.push(source);
        }
    }
    Ok(result)
}
