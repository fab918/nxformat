//! Native conversion orchestration. Sources are read-only; only a complete,
//! revalidated inventory is accepted. Publication uses the existing writer.

use crate::native::{
    associate_with_cancellation, inspect_source, Inventory, InventoryOptions, SnapshotRef,
};
use crate::writer::{atomic_replace, seal_staged_generations, unique_temp};
use crate::*;
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct ConversionStats {
    pub initial: WriterStats,
    pub additions: Vec<RepackStats>,
    pub elapsed: Duration,
    /// Includes the current staged archive while a repack is staged.
    pub peak_temporary_disk_bytes: u64,
}

fn invalid(message: &str) -> WriterError {
    WriterError::new(WriterErrorKind::InvalidInput, message)
}

/// Pack bases and independent DLC, resolve exact dependency identities, then add
/// complete updates with the transactional repacker. No intermediate archive
/// is published at `destination`. Fast/Strict govern the staged NX readback;
/// native verification is specified separately by `inventory_options`.
pub fn convert_inventory(
    inventory: &Inventory,
    destination: &Path,
    inventory_options: &InventoryOptions,
    policy: IntegrityPolicy,
    options: &WriterOptions,
) -> Result<ConversionStats, WriterError> {
    convert_inventory_profiled(
        inventory,
        destination,
        inventory_options,
        policy,
        options,
        None,
    )
}

/// Same native conversion with optional exact-version VFS performance profiles.
/// All supplied files must match a staged snapshot; no unknown profile is ignored.
pub fn convert_inventory_profiled(
    inventory: &Inventory,
    destination: &Path,
    inventory_options: &InventoryOptions,
    policy: IntegrityPolicy,
    options: &WriterOptions,
    profiles: Option<&crate::read_profile::CapturedReadProfiles>,
) -> Result<ConversionStats, WriterError> {
    let started = Instant::now();
    if options.cancellation.is_cancelled() {
        return Err(WriterError::new(
            WriterErrorKind::Cancelled,
            "conversion cancelled",
        ));
    }
    if inventory.sources.is_empty() {
        return Err(invalid("empty native inventory"));
    }
    let destination_real = destination.canonicalize().ok();
    for source in &inventory.sources {
        if destination == source.path
            || destination_real
                .as_ref()
                .is_some_and(|d| source.path.canonicalize().ok().as_ref() == Some(d))
        {
            return Err(invalid("destination is a native source"));
        }
    }
    // Do not trust mutable/public IR, or metadata cached before a source edit.
    let fresh = associate_with_cancellation(
        inventory
            .sources
            .iter()
            .map(|s| inspect_source(&s.path, inventory_options))
            .collect(),
        &options.cancellation,
    );
    if options.cancellation.is_cancelled() {
        return Err(WriterError::new(
            WriterErrorKind::Cancelled,
            "conversion cancelled",
        ));
    }
    let plan = fresh.plan_with_file_comparison(&mut |a, b| {
        crate::native::identical_planned_files(a, b, Some(&options.cancellation))
    });
    if options.cancellation.is_cancelled() {
        return Err(WriterError::new(
            WriterErrorKind::Cancelled,
            "conversion cancelled",
        ));
    }
    if !plan.blocked.is_empty() {
        return Err(invalid(&format!(
            "native content cannot be converted: {}",
            crate::native::BlockedContent::describe(&plan.blocked)
        )));
    }
    if fresh.error_count() != 0
        || fresh.sources.iter().any(|s| s.metas.is_empty())
        || plan.applications.is_empty()
    {
        return Err(invalid(
            "inventory contains rejected, incomplete or ambiguous native content",
        ));
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (file, mut stage) = unique_temp(parent, OsStr::new(".nx-convert"))?;
    drop(file);
    let independent = |s: &crate::native::PlannedSnapshot| s.requires.is_none();
    let input = plan
        .applications
        .iter()
        .map(|a| a.to_application_input(&independent, &|_| None))
        .collect::<Result<Vec<_>, _>>()?;
    let mut matched = std::collections::BTreeSet::new();
    let mut input = ArchiveInput::new(input);
    if options.preserve_firmware {
        for source in &fresh.sources {
            let resources = crate::native::source_resources(
                &source.path,
                crate::Platform::Switch1,
                0,
                inventory_options,
            )
            .map_err(|e| invalid(&e.to_string()))?;
            if resources.is_empty() {
                continue;
            }
            // The firmware belongs to a game this archive holds: the cart's
            // base game first, then any game one of its metas names. The first
            // meta can be a DLC or an update of a game that is not packed.
            let packed = |id: u64| {
                input
                    .applications
                    .iter()
                    .any(|a| a.platform == crate::Platform::Switch1 && a.application_id == id)
            };
            let owner = source
                .metas
                .iter()
                .filter(|m| m.meta.meta_type == crate::native::ContentMetaType::Application)
                .chain(source.metas.iter())
                .map(|m| m.meta.application_id)
                .find(|&id| packed(id))
                .ok_or_else(|| {
                    invalid(&format!(
                        "{}: the cart firmware belongs to no game in this archive; use --omit-firmware --no-recipe",
                        source.path.display()
                    ))
                })?;
            input
                .source_resources
                .extend(resources.into_iter().map(|mut r| {
                    r.application_id = owner;
                    r
                }));
        }
    }
    if let Some(profiles) = profiles {
        matched.extend(profiles.attach(&mut input, false)?);
    }
    let initial = write_archive(input, &stage.path, options)?;
    let mut peak = initial.peak_temporary_disk_bytes;
    let mut additions = Vec::new();
    // First resolve references to already-written bases; only DLC requiring
    // an update needs the second pass, after that update's identity exists.
    for update_target in [false, true] {
        let include = |s: &crate::native::PlannedSnapshot| {
            s.requires
                .as_ref()
                .is_some_and(|r| (r.snapshot_type == SnapshotType::Update) == update_target)
        };
        if !plan
            .applications
            .iter()
            .any(|a| a.snapshots.iter().any(&include))
        {
            continue;
        }
        let summary = inspect_archive(&stage.path, options, policy)?;
        if profiles.is_none() {
            // A native update's planned view lists its base files too. The
            // native add path borrows them from the staged base instead of
            // storing them a second time (Aggelos base+update: 339 MB -> 227 MB).
            let mut done = std::collections::BTreeSet::new();
            let mut requests = Vec::new();
            for a in &plan.applications {
                for s in a.snapshots.iter().filter(|s| include(s)) {
                    if !done.insert((s.metadata_source.clone(), s.title_id, s.numeric_version)) {
                        continue;
                    }
                    let base = s
                        .requires
                        .as_ref()
                        .filter(|r| r.snapshot_type == SnapshotType::Base)
                        .and_then(|r| {
                            summary
                                .applications
                                .iter()
                                .find(|x| {
                                    x.platform == r.platform && x.application_id == r.application_id
                                })
                                .and_then(|x| {
                                    x.snapshots.iter().find(|y| {
                                        y.snapshot_type == r.snapshot_type
                                            && y.program_id == r.program_id
                                            && y.numeric_version == r.numeric_version
                                    })
                                })
                                .map(|y| y.identity)
                        });
                    requests.push(crate::NativeAddRequest {
                        native_source: s.metadata_source.clone(),
                        title_id: s.title_id,
                        numeric_version: s.numeric_version,
                        base_snapshot_identity: base,
                    });
                }
            }
            // Every addition of this pass goes into one archive write.
            let staged_bytes = std::fs::metadata(&stage.path)?.len();
            let mut remaining = options.clone();
            remaining.max_temporary_disk_bytes = options
                .max_temporary_disk_bytes
                .checked_sub(staged_bytes)
                .ok_or_else(|| {
                    WriterError::new(
                        WriterErrorKind::ResourceLimit,
                        "conversion staging exceeds disk budget",
                    )
                })?;
            let stats = crate::add_native_contents(
                &stage.path,
                &stage.path,
                &requests,
                inventory_options,
                &remaining,
                policy,
            )?
            .repack;
            peak = peak.max(staged_bytes + stats.peak_temporary_disk_bytes);
            additions.push(stats);
            continue;
        }
        let resolve = |reference: &SnapshotRef| {
            summary
                .applications
                .iter()
                .find(|a| {
                    a.platform == reference.platform && a.application_id == reference.application_id
                })
                .and_then(|a| {
                    a.snapshots.iter().find(|s| {
                        s.snapshot_type == reference.snapshot_type
                            && s.program_id == reference.program_id
                            && s.numeric_version == reference.numeric_version
                    })
                })
                .map(|s| s.identity)
        };
        let input = plan
            .applications
            .iter()
            .filter(|a| a.snapshots.iter().any(&include))
            .map(|a| a.to_application_input(&include, &resolve))
            .collect::<Result<Vec<_>, _>>()?;
        let mut input = ArchiveInput::new(input);
        if let Some(profiles) = profiles {
            matched.extend(profiles.attach(&mut input, false)?);
        }
        let staged_bytes = std::fs::metadata(&stage.path)?.len();
        let mut remaining = options.clone();
        remaining.max_temporary_disk_bytes = options
            .max_temporary_disk_bytes
            .checked_sub(staged_bytes)
            .ok_or_else(|| {
                WriterError::new(
                    WriterErrorKind::ResourceLimit,
                    "conversion staging exceeds disk budget",
                )
            })?;
        let stats = repack_archive(
            &stage.path,
            &stage.path,
            RepackPlan {
                remove_snapshots: Vec::new(),
                additions: input,
                metadata_sources: Vec::new(),
            },
            &remaining,
            policy,
        )?;
        peak = peak.max(staged_bytes + stats.peak_temporary_disk_bytes);
        additions.push(stats);
    }
    if options.cancellation.is_cancelled() {
        return Err(WriterError::new(
            WriterErrorKind::Cancelled,
            "conversion cancelled before publication",
        ));
    }
    if profiles.is_some_and(|profiles| matched.len() != profiles.files.len()) {
        return Err(invalid(
            "captured profiles contain a missing/incompatible native version",
        ));
    }
    // Updates were appended to the private stage as new generations: publish
    // only the last one, so readers never check an older one at open.
    if !additions.is_empty() {
        seal_staged_generations(&stage.path, options)?;
    }
    atomic_replace(&stage.path, destination)?;
    stage.disarm();
    Ok(ConversionStats {
        initial,
        additions,
        elapsed: started.elapsed(),
        peak_temporary_disk_bytes: peak,
    })
}
