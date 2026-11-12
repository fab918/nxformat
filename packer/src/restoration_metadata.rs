//! Recover immutable metadata from the literal regions of a verified recipe.
//! Original ranges are mapped to a compact store; media and resource blocks are
//! retained independently, and are checked before the new archive is published.
use super::*;
use crate::import::{DeclarationManifest, External, RestoredMetadata};
use std::collections::BTreeMap;

pub(super) struct Recovered {
    pub external: External,
    pub selected: Vec<(Platform, u64, String)>,
}

struct RegionReader {
    path: std::path::PathBuf,
    file: Option<File>,
    offset: u64,
    remaining: u64,
    digest: [u8; 32],
    hash: Option<Sha256>,
    options: WriterOptions,
}
impl Read for RegionReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        cancelled(&self.options).map_err(io::Error::other)?;
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            if let Some(hash) = self.hash.take() {
                if hash.finish() != self.digest {
                    return Err(io::Error::other("retained native media SHA-256 mismatch"));
                }
            }
            self.file = None;
            return Ok(0);
        }
        if self.file.is_none() {
            let mut file = File::open(&self.path)?;
            file.seek(SeekFrom::Start(self.offset))?;
            self.file = Some(file);
        }
        let n = bytes
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        self.file.as_mut().unwrap().read_exact(&mut bytes[..n])?;
        self.remaining -= n as u64;
        self.hash.as_mut().unwrap().update(&bytes[..n]);
        if self.remaining == 0 {
            if self.hash.take().unwrap().finish() != self.digest {
                return Err(io::Error::other("retained native media SHA-256 mismatch"));
            }
            self.file = None;
        }
        Ok(n)
    }
}

struct ResourceReader {
    path: std::path::PathBuf,
    file: Option<File>,
    blocks: Vec<SourceBlock>,
    size: u64,
    chunk: u32,
    position: u64,
    digest: [u8; 32],
    hash: Option<Sha256>,
    frame: Vec<u8>,
    logical: Vec<u8>,
    decoder: Option<zstd::bulk::Decompressor<'static>>,
    cached: Option<u64>,
    options: WriterOptions,
}
impl Read for ResourceReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        cancelled(&self.options).map_err(io::Error::other)?;
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.position == self.size {
            if let Some(hash) = self.hash.take() {
                if hash.finish() != self.digest {
                    return Err(io::Error::other(
                        "retained source resource SHA-256 mismatch",
                    ));
                }
            }
            return Ok(0);
        }
        if self.file.is_none() {
            self.file = Some(File::open(&self.path)?);
            let mut decoder = zstd::bulk::Decompressor::new()?;
            decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(20))?;
            self.decoder = Some(decoder);
        }
        let index = (self.position / self.chunk as u64) as usize;
        let block = self
            .blocks
            .get(index)
            .ok_or_else(|| io::Error::other("retained resource block is absent"))?;
        if self.cached != Some(block.range.offset) {
            decode_source_block(
                self.file.as_mut().unwrap(),
                block,
                &mut self.frame,
                &mut self.logical,
                self.decoder.as_mut().unwrap(),
                &self.options,
            )
            .map_err(io::Error::other)?;
            if !checks_match(block, &self.logical) {
                return Err(io::Error::other("retained resource data is unavailable or corrupt; regenerate the extraction recipe before editing"));
            }
            self.cached = Some(block.range.offset);
        }
        let within = (self.position % self.chunk as u64) as usize;
        let n = bytes
            .len()
            .min(self.logical.len() - within)
            .min((self.size - self.position).min(usize::MAX as u64) as usize);
        bytes[..n].copy_from_slice(&self.logical[within..within + n]);
        self.hash.as_mut().unwrap().update(&bytes[..n]);
        self.position += n as u64;
        if self.position == self.size {
            if self.hash.take().unwrap().finish() != self.digest {
                return Err(io::Error::other(
                    "retained source resource SHA-256 mismatch",
                ));
            }
            self.file = None;
            self.decoder = None;
            self.frame = Vec::new();
            self.logical = Vec::new();
        }
        Ok(n)
    }
}

fn platform(value: u8) -> Result<Platform, WriterError> {
    match value {
        1 => Ok(Platform::Switch1),
        2 => Ok(Platform::Switch2),
        _ => Err(bad("unknown retained platform")),
    }
}

pub(super) fn recover(
    capsule: &Path,
    store: &super::store::Store,
    declarations: &DeclarationManifest,
    options: &WriterOptions,
) -> Result<Recovered, WriterError> {
    let mut file = File::open(capsule)?;
    let mut budget = MemoryBudget {
        current: store.memory_bytes(),
        peak: 0,
        limit: options.max_memory_bytes / 2,
    };
    let archive = parse_archive(&mut store.reader(&mut file), 0, options, &mut budget)?;
    if !archive.optional_global_extensions.is_empty()
        || archive
            .applications
            .iter()
            .any(|a| !a.optional_extensions.is_empty())
    {
        return Err(invalid(
            "edited restoration cannot preserve unrecognized optional catalog extensions",
        ));
    }
    let mut external = External::default();
    let mut selected = Vec::new();
    let by_id: BTreeMap<_, _> = archive
        .snapshots
        .iter()
        .map(|s| (crate::import::hex(&s.snapshot.identity), s))
        .collect();
    let mut media = |index: u32| -> Result<Option<MediaInput>, WriterError> {
        if index == NO_INDEX {
            return Ok(None);
        }
        let m = archive
            .media
            .get(index as usize)
            .ok_or_else(|| bad("retained media index is invalid"))?;
        let kind = match m.media_type {
            1 => MediaType::PngIcon,
            2 => MediaType::JpegIcon,
            3 => MediaType::Npdm,
            4 => MediaType::Nacp,
            5 => MediaType::Opaque,
            _ => return Err(bad("unknown retained media type")),
        };
        budget.charge(1024, "retained media descriptors")?;
        Ok(Some(MediaInput::new(
            kind,
            m.size,
            RegionReader {
                path: capsule.to_owned(),
                file: None,
                offset: store.translate(m.range.offset, m.size)?,
                remaining: m.size,
                digest: m.digest,
                hash: Some(Sha256::new()),
                options: options.clone(),
            },
        )))
    };
    for d in &declarations.snapshots {
        let snapshot = if let Some(s) = by_id.get(&d.key) {
            *s
        } else {
            let mut matches = archive.snapshots.iter().filter(|s| {
                s.key == (d.platform as u8, d.application_id)
                    && s.program_id == d.program_id
                    && s.snapshot.snapshot_type == d.snapshot_type as u8
                    && s.snapshot.version == d.numeric_version
            });
            let found = matches
                .next()
                .ok_or_else(|| bad("retained snapshot metadata owner is absent"))?;
            if matches.next().is_some() {
                return Err(bad("ambiguous retained metadata owner"));
            }
            found
        };
        if snapshot.key != (d.platform as u8, d.application_id)
            || snapshot.program_id != d.program_id
            || snapshot.snapshot.version != d.numeric_version
            || snapshot.snapshot.snapshot_type != d.snapshot_type as u8
        {
            return Err(bad("retained metadata scope disagrees with declarations"));
        }
        let metadata = if let Some(refs) = &snapshot.snapshot.meta_refs {
            let mut localized = Vec::new();
            for l in &refs.localized {
                localized.push(LocalizedMetadata {
                    language: l.language.clone(),
                    title: l.title.clone(),
                    icon: media(l.icon)?,
                });
            }
            let mut opaque = Vec::new();
            for &index in &refs.opaque {
                opaque.push(media(index)?.ok_or_else(|| bad("absent retained opaque metadata"))?);
            }
            Some(SnapshotMetadata {
                title: refs.title.clone(),
                publisher: refs.publisher.clone(),
                icon: media(refs.icon)?,
                nacp: media(refs.nacp)?,
                localized,
                opaque,
            })
        } else {
            None
        };
        let program = archive
            .programs
            .iter()
            .find(|p| p.key == snapshot.key && Some(p.program_id) == snapshot.program_id);
        let npdm = if snapshot.snapshot.npdm != NO_INDEX {
            snapshot.snapshot.npdm
        } else {
            program.map_or(NO_INDEX, |p| p.npdm)
        };
        external.restored_metadata.borrow_mut().insert(
            d.key.clone(),
            RestoredMetadata {
                metadata,
                display_version: snapshot.snapshot.display_version.clone(),
                native: SnapshotNativeMetadata {
                    npdm: media(npdm)?,
                    required_firmware: snapshot.snapshot.required_firmware,
                },
            },
        );
        if let Some(app) = archive.applications.iter().find(|a| a.key == snapshot.key) {
            if app.selected_identity == Some(snapshot.snapshot.identity) {
                selected.push((d.platform, d.application_id, d.key.clone()));
            }
        }
    }
    for program in &archive.programs {
        if program.has_content_id {
            external.program_content_ids.insert(
                (program.key.0, program.key.1, program.program_id),
                program.content_id,
            );
        }
    }
    for app in &archive.applications {
        if !app.source_resources.is_empty()
            && !declarations
                .snapshots
                .iter()
                .any(|d| (d.platform as u8, d.application_id) == app.key)
        {
            return Err(bad("retained resource owner has no exported snapshot"));
        }
        for &(kind, root) in &app.source_resources {
            let mut entries = Vec::new();
            for entry in &archive.directories[root as usize] {
                let record = &archive.files[entry.target as usize];
                let mut blocks = Vec::new();
                for i in 0..record.block_count {
                    let block = &archive.blocks
                        [archive.block_refs[(record.first_ref + i) as usize] as usize];
                    budget.charge(
                        128 + block.checks.len() as u64 * 4,
                        "retained resource block descriptors",
                    )?;
                    let mut block = block.clone();
                    block.range.offset =
                        store.translate(block.range.offset, block.stored_size as u64)?;
                    blocks.push(block);
                }
                entries.push(EntryInput::file(
                    entry.name.clone(),
                    FileInput::new(
                        record.size,
                        ResourceReader {
                            path: capsule.to_owned(),
                            file: None,
                            blocks,
                            size: record.size,
                            chunk: record.chunk_size_bytes,
                            position: 0,
                            digest: record.digest,
                            hash: Some(Sha256::new()),
                            frame: Vec::new(),
                            logical: Vec::new(),
                            decoder: None,
                            cached: None,
                            options: options.clone(),
                        },
                    ),
                ));
            }
            external
                .source_resources
                .borrow_mut()
                .push(SourceResourceInput {
                    platform: platform(app.key.0)?,
                    application_id: app.key.1,
                    kind,
                    root: DirectoryInput { entries },
                });
        }
    }
    // Reserve every retained descriptor conservatively while the tree writer
    // is active; resource payloads and native media themselves stay streamed.
    external.retained_memory_bytes = budget.current.saturating_add(8 * 1024 * 1024);
    Ok(Recovered { external, selected })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    const APP: u64 = 0x0400_0000_0010_0000;

    fn snapshot(version: u32, bytes: &[u8], base: Option<[u8; 32]>) -> SnapshotInput {
        let npdm = crate::import::synthetic::npdm(
            APP,
            crate::import::structures::NpdmProfile::MetaAcid3072V1,
        );
        let icon = |n| {
            MediaInput::from_bytes(
                MediaType::JpegIcon,
                vec![0xff, 0xd8, 0xff, 0xe0, version as u8, n],
            )
        };
        let mut nacp = vec![0; 0x4000];
        let title = format!("Native title {version}");
        nacp[..title.len()].copy_from_slice(title.as_bytes());
        let display = format!("NACP {version}");
        nacp[0x3060..0x3060 + display.len()].copy_from_slice(display.as_bytes());
        SnapshotInput {
            native: SnapshotNativeMetadata {
                npdm: Some(MediaInput::from_bytes(MediaType::Npdm, npdm.clone())),
                required_firmware: Some(0x1234 + version),
            },
            program_id: Some(APP),
            snapshot_type: if version == 0 {
                SnapshotType::Base
            } else {
                SnapshotType::Update
            },
            numeric_version: version,
            content_id: None,
            addon_title_id: None,
            build_id: Some([3 + version as u8; 32]),
            display_version: Some(format!("Display {version}")),
            dependencies: base
                .into_iter()
                .map(|id| DependencyInput {
                    kind: DependencyKind::RequiredBase,
                    content_id: None,
                    target_snapshot_identity: id,
                })
                .collect(),
            metadata: Some(SnapshotMetadata {
                title: Some(format!("Override {version}")),
                publisher: Some("Publisher".into()),
                icon: Some(icon(1)),
                nacp: Some(MediaInput::from_bytes(MediaType::Nacp, nacp)),
                localized: vec![
                    LocalizedMetadata {
                        language: "en-us".into(),
                        title: Some(format!("English {version}")),
                        icon: Some(icon(1)),
                    },
                    LocalizedMetadata {
                        language: "fr-fr".into(),
                        title: Some(format!("French {version}")),
                        icon: Some(icon(2)),
                    },
                ],
                opaque: vec![MediaInput::from_bytes(
                    MediaType::Opaque,
                    vec![version as u8; 256],
                )],
            }),
            root: DirectoryInput::new(vec![
                EntryInput::directory(
                    "exefs",
                    DirectoryInput::new(vec![
                        EntryInput::file(
                            "main",
                            FileInput::from_bytes(crate::import::synthetic::nso(
                                3 + version as u8,
                                false,
                            )),
                        ),
                        EntryInput::file("main.npdm", FileInput::from_bytes(npdm)),
                    ]),
                ),
                EntryInput::directory(
                    "romfs",
                    DirectoryInput::new(vec![EntryInput::file(
                        "shared.bin",
                        FileInput::from_bytes(bytes.to_vec()),
                    )]),
                ),
            ]),
        }
    }
    fn application(s: SnapshotInput) -> ApplicationInput {
        ApplicationInput {
            application_id: APP,
            platform: Platform::Switch2,
            programs: vec![ProgramInput {
                program_id: APP,
                kind: ProgramKind::Primary,
                content_id: Some([17; 16]),
                npdm: None,
            }],
            snapshots: vec![s],
        }
    }
    type MetadataFingerprint = (BTreeMap<u32, String>, u32, Vec<(u32, u64, [u8; 32])>);
    fn fingerprint(path: &Path, options: &WriterOptions) -> MetadataFingerprint {
        let mut f = File::open(path).unwrap();
        let mut budget = MemoryBudget {
            current: 0,
            peak: 0,
            limit: options.max_memory_bytes,
        };
        let a = parse_archive(&mut f, 0, options, &mut budget).unwrap();
        let media = |id| {
            if id == NO_INDEX {
                None
            } else {
                Some((a.media[id as usize].media_type, a.media[id as usize].digest))
            }
        };
        let mut snapshots = BTreeMap::new();
        for s in &a.snapshots {
            let m = s.snapshot.meta_refs.as_ref().unwrap();
            let localized: Vec<_> = m
                .localized
                .iter()
                .map(|l| (&l.language, &l.title, media(l.icon)))
                .collect();
            let opaque: Vec<_> = m.opaque.iter().map(|&id| media(id)).collect();
            snapshots.insert(
                s.snapshot.version,
                format!(
                    "{:?}",
                    (
                        &m.title,
                        &m.publisher,
                        media(m.icon),
                        media(m.nacp),
                        localized,
                        opaque,
                        media(s.snapshot.npdm),
                        s.snapshot.required_firmware,
                        &s.snapshot.display_version,
                        s.snapshot.build_id,
                        a.programs[0].content_id
                    )
                ),
            );
        }
        let selected = a
            .snapshots
            .iter()
            .find(|s| Some(s.snapshot.identity) == a.applications[0].selected_identity)
            .unwrap()
            .snapshot
            .version;
        let mut resources = Vec::new();
        for &(kind, root) in &a.applications[0].source_resources {
            for entry in &a.directories[root as usize] {
                let f = &a.files[entry.target as usize];
                resources.push((kind, f.size, f.digest));
            }
        }
        (snapshots, selected, resources)
    }
    #[test]
    fn edited_roundtrip_preserves_all_native_metadata_and_shared_resources() {
        let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("nx-native-metadata-{}-{id}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let mut state = 91u64;
        let data: Vec<u8> = (0..262144)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let options = WriterOptions {
            compression_mode: CompressionMode::RawOnly,
            ..Default::default()
        };
        let original = dir.join("original.nx");
        let mut input = ArchiveInput::new(vec![application(snapshot(0, &data, None))]);
        input.source_resources.push(SourceResourceInput {
            platform: Platform::Switch2,
            application_id: APP,
            kind: 1,
            root: DirectoryInput::new(vec![EntryInput::file(
                "partition.hfs0",
                FileInput::from_bytes(data.clone()),
            )]),
        });
        write_archive(input, &original, &options).unwrap();
        let base = inspect_archive(&original, &options, IntegrityPolicy::Strict)
            .unwrap()
            .applications[0]
            .snapshots[0]
            .identity;
        let plan = RepackPlan {
            remove_snapshots: vec![],
            additions: ArchiveInput::new(vec![application(snapshot(1, &data, Some(base)))]),
            metadata_sources: vec![MetadataSourceSelection {
                platform: Platform::Switch2,
                application_id: APP,
                snapshot_identity: base,
            }],
        };
        repack_archive(
            &original,
            &original,
            plan,
            &options,
            IntegrityPolicy::Strict,
        )
        .unwrap();
        let before = fingerprint(&original, &options);
        assert_eq!(before.1, 0);
        assert_eq!(before.2.len(), 1);
        for portable in [false, true] {
            let dump = dir.join(if portable { "portable-dump" } else { "dump" });
            crate::cli::run(
                {
                    let mut args = vec![
                        "extract".into(),
                        original.to_string_lossy().into_owned(),
                        dump.to_string_lossy().into_owned(),
                    ];
                    if portable {
                        args.push("--portable-restore".into());
                    }
                    args
                },
                None,
                None,
            )
            .unwrap();
            std::fs::write(
                dump.join("base/romfs/shared.bin"),
                b"a resized mod; source resource must retain original bytes",
            )
            .unwrap();
            let modified = dir.join(if portable {
                "portable-modified.nx"
            } else {
                "modified.nx"
            });
            super::super::restore_extraction(&dump, &modified, &options).unwrap();
            inspect_archive(&modified, &options, IntegrityPolicy::Strict).unwrap();
            assert_eq!(fingerprint(&modified, &options), before);
            let resources =
                inspect_source_resources(&modified, Platform::Switch2, APP, &options).unwrap();
            assert_eq!(resources[0].1[0].digest, sha256(&data));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
