use nxformat_packer::{
    compact_archive, inspect_archive, inspect_source_resources, read_snapshot_nacp, repack_archive,
    repack_archive_with_source_removals, write_archive, ApplicationInput, ArchiveInput,
    DependencyInput, DependencyKind, DirectoryInput, EntryInput, FileInput, IntegrityPolicy,
    LocalizedMetadata, MetadataSourceSelection, Platform, ProgramInput, ProgramKind, RepackPlan,
    SnapshotInput, SnapshotMetadata, SnapshotType, SourceResourceInput, SourceResourceRemoval,
    WriterErrorKind, WriterOptions,
};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("nx-repack-test-{}-{id}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
#[test]
fn concurrent_compaction_is_rejected_before_touching_the_archive() {
    use std::os::fd::AsRawFd;

    let temp = TempDir::new();
    let archive = temp.path().join("archive.nx");
    let fixture = include_bytes!("../../tests/fixtures/appended_generation.nx");
    fs::write(&archive, fixture).unwrap();

    let locked = File::open(&archive).unwrap();
    // SAFETY: locked owns the descriptor for the duration of the test.
    let result = unsafe { libc::flock(locked.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(result, 0);
    let options = WriterOptions::default();
    let error = compact_archive(&archive, &options, IntegrityPolicy::Fast).unwrap_err();
    assert_eq!(error.kind(), WriterErrorKind::Io);
    assert!(error
        .to_string()
        .contains("cannot lock archive for editing"));
    assert_eq!(fs::read(&archive).unwrap(), fixture);

    drop(locked);
    compact_archive(&archive, &options, IntegrityPolicy::Fast).unwrap();
    assert_ne!(fs::read(&archive).unwrap(), fixture);
}

#[test]
fn snapshot_with_display_metadata_but_no_nacp_can_be_listed() {
    let archive = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/raw_only.nx");
    let options = WriterOptions::default();
    let summary = inspect_archive(&archive, &options, IntegrityPolicy::Fast).unwrap();
    let snapshot = &summary.applications[0].snapshots[0];
    assert!(snapshot.has_display_metadata);
    assert!(read_snapshot_nacp(&archive, snapshot.identity, &options)
        .unwrap()
        .is_none());
}

const APP: u64 = 0x0100_0000_0000_1234;
const PROGRAM: u64 = 0x0100_0000_0000_1235;

fn tree(entries: Vec<EntryInput>) -> DirectoryInput {
    DirectoryInput::new(entries)
}

#[test]
fn symlink_in_place_edits_publish_to_target_and_preserve_link_on_errors() {
    let temp = TempDir::new();
    let target_dir = temp.path().join("target");
    fs::create_dir(&target_dir).unwrap();
    let target = target_dir.join("game.nx");
    let alias = temp.path().join("alias.nx");
    let options = WriterOptions::default();
    write_archive(base_input(vec![0x55; 262144]), &target, &options).unwrap();
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(&target, &alias);
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(&target, &alias);
    if let Err(error) = result {
        if error.kind() == std::io::ErrorKind::PermissionDenied
            || error.raw_os_error() == Some(1314)
        {
            eprintln!("SKIP symlink creation is unavailable: {error}");
            return;
        }
        panic!("symlink creation: {error}");
    }
    let base = inspect_archive(&target, &options, IntegrityPolicy::Strict)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    for version in 1..=2 {
        let mut plan = RepackPlan::new();
        plan.additions = single_update_input(base, version, vec![version as u8; 262144]);
        let source = if version == 1 { &alias } else { &target };
        repack_archive(source, &alias, plan, &options, IntegrityPolicy::Strict).unwrap();
        assert!(fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            inspect_archive(&target, &options, IntegrityPolicy::Strict)
                .unwrap()
                .applications[0]
                .snapshots
                .len(),
            version as usize + 1
        );
    }
    let before = fs::read(&target).unwrap();
    for cancel in [true, false] {
        let mut options = WriterOptions::default();
        if cancel {
            options.cancellation.cancel();
        } else {
            options.max_temporary_disk_bytes = 256;
        }
        let mut plan = RepackPlan::new();
        plan.additions = single_update_input(base, 3, vec![7; 524288]);
        assert!(repack_archive(&alias, &alias, plan, &options, IntegrityPolicy::Strict).is_err());
        assert_eq!(fs::read(&target).unwrap(), before);
        assert!(fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink());
    }
    let other = temp.path().join("other.nx");
    fs::write(&other, b"other target").unwrap();
    let distinct = temp.path().join("distinct.nx");
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&other, &distinct).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&other, &distinct).unwrap();
    repack_archive(
        &target,
        &distinct,
        RepackPlan::new(),
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert!(!fs::symlink_metadata(&distinct)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(other).unwrap(), b"other target");
    assert_eq!(fs::read(target).unwrap(), before);
}

fn base_input(base_bytes: Vec<u8>) -> ArchiveInput {
    ArchiveInput::new(vec![ApplicationInput {
        application_id: APP,
        platform: Platform::Switch1,
        programs: vec![ProgramInput {
            program_id: PROGRAM,
            kind: ProgramKind::Primary,
            content_id: None,
            npdm: None,
        }],
        snapshots: vec![SnapshotInput {
            addon_title_id: None,
            native: Default::default(),
            program_id: Some(PROGRAM),
            snapshot_type: SnapshotType::Base,
            numeric_version: 0,
            content_id: None,
            build_id: Some([0x31; 32]),
            display_version: Some("1.0".into()),
            dependencies: vec![],
            metadata: Some(SnapshotMetadata {
                title: Some("Synthetic base".into()),
                publisher: Some("NX tests".into()),
                icon: None,
                nacp: None,
                localized: vec![LocalizedMetadata {
                    language: "en-us".into(),
                    title: Some("Synthetic base".into()),
                    icon: None,
                }],
                opaque: vec![],
            }),
            root: tree(vec![EntryInput::file(
                "base.bin",
                FileInput::from_bytes(base_bytes),
            )]),
        }],
    }])
}

fn addition_input(base_identity: [u8; 32], same_block: Vec<u8>) -> ArchiveInput {
    ArchiveInput::new(vec![ApplicationInput {
        application_id: APP,
        platform: Platform::Switch1,
        programs: vec![ProgramInput {
            program_id: PROGRAM,
            kind: ProgramKind::Primary,
            content_id: None,
            npdm: None,
        }],
        snapshots: vec![
            SnapshotInput {
                addon_title_id: None,
                native: Default::default(),
                program_id: Some(PROGRAM),
                snapshot_type: SnapshotType::Update,
                numeric_version: 1,
                content_id: None,
                build_id: Some([0x32; 32]),
                display_version: Some("1.1".into()),
                dependencies: vec![DependencyInput {
                    kind: DependencyKind::RequiredBase,
                    content_id: None,
                    target_snapshot_identity: base_identity,
                }],
                metadata: None,
                root: tree(vec![
                    EntryInput::file("same.bin", FileInput::from_bytes(same_block)),
                    EntryInput::file("update.bin", FileInput::from_bytes(vec![0x44; 256 * 1024])),
                ]),
            },
            SnapshotInput {
                addon_title_id: None,
                native: Default::default(),
                program_id: None,
                snapshot_type: SnapshotType::Dlc,
                numeric_version: 7,
                content_id: Some([0x55; 16]),
                build_id: None,
                display_version: None,
                dependencies: vec![],
                metadata: None,
                root: tree(vec![EntryInput::file(
                    "dlc.bin",
                    FileInput::from_bytes(vec![0x77; 64 * 1024]),
                )]),
            },
        ],
    }])
}

fn single_update_input(base_identity: [u8; 32], version: u32, bytes: Vec<u8>) -> ArchiveInput {
    ArchiveInput::new(vec![ApplicationInput {
        application_id: APP,
        platform: Platform::Switch1,
        programs: vec![ProgramInput {
            program_id: PROGRAM,
            kind: ProgramKind::Primary,
            content_id: None,
            npdm: None,
        }],
        snapshots: vec![SnapshotInput {
            addon_title_id: None,
            native: Default::default(),
            program_id: Some(PROGRAM),
            snapshot_type: SnapshotType::Update,
            numeric_version: version,
            content_id: None,
            build_id: Some([0x30 + version as u8; 32]),
            display_version: None,
            dependencies: vec![DependencyInput {
                kind: DependencyKind::RequiredBase,
                content_id: None,
                target_snapshot_identity: base_identity,
            }],
            metadata: None,
            root: tree(vec![EntryInput::file(
                "replacement.bin",
                FileInput::from_bytes(bytes),
            )]),
        }],
    }])
}

/// Field of the selected (highest generation) NX 0.2 header slot.
fn header_u64(path: &Path, field: u64) -> u64 {
    let a = read_u64_at(path, 216);
    let b = read_u64_at(path, 4096 + 216);
    let slot = if b > a { 4096 } else { 0 };
    read_u64_at(path, slot + field)
}

fn read_u64_at(path: &Path, offset: u64) -> u64 {
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut b = [0u8; 8];
    file.read_exact(&mut b).unwrap();
    u64::from_le_bytes(b)
}

fn read_u32_at(path: &Path, offset: u64) -> u32 {
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut b = [0u8; 4];
    file.read_exact(&mut b).unwrap();
    u32::from_le_bytes(b)
}

fn block_frames(path: &Path) -> Vec<Vec<u8>> {
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(88)).unwrap();
    let mut b = [0u8; 8];
    file.read_exact(&mut b).unwrap();
    let index = u64::from_le_bytes(b);
    file.seek(SeekFrom::Start(96)).unwrap();
    file.read_exact(&mut b).unwrap();
    let count = u64::from_le_bytes(b);
    let mut out = Vec::new();
    for i in 0..count {
        file.seek(SeekFrom::Start(index + i * 24)).unwrap();
        let mut d = [0u8; 24];
        file.read_exact(&mut d).unwrap();
        let offset = u64::from_le_bytes(d[..8].try_into().unwrap());
        let size = u32::from_le_bytes(d[8..12].try_into().unwrap()) as usize;
        let mut frame = vec![0; size];
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.read_exact(&mut frame).unwrap();
        out.push(frame);
    }
    out
}

#[test]
fn add_update_and_unassociated_dlc_reuses_old_zstd_frame_and_compacts() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let output = temp.path().join("edited.nx");
    let base_options = WriterOptions {
        chunk_size_bytes: 64 * 1024,
        ..WriterOptions::default()
    };
    let base_bytes = vec![0x22; 64 * 1024];
    write_archive(base_input(base_bytes.clone()), &source, &base_options).unwrap();
    let before = inspect_archive(&source, &base_options, IntegrityPolicy::Strict).unwrap();
    let base_identity = before.applications[0].snapshots[0].identity;
    let original_frame = block_frames(&source).remove(0);
    let options = WriterOptions {
        chunk_size_bytes: 256 * 1024,
        ..WriterOptions::default()
    };
    let plan = RepackPlan {
        remove_snapshots: vec![],
        additions: addition_input(base_identity, base_bytes.clone()),
        metadata_sources: vec![],
    };
    let stats = repack_archive(&source, &output, plan, &options, IntegrityPolicy::Fast).unwrap();
    let repeated = temp.path().join("edited-again.nx");
    let repeated_stats = repack_archive(
        &source,
        &repeated,
        RepackPlan {
            remove_snapshots: vec![],
            additions: addition_input(base_identity, base_bytes.clone()),
            metadata_sources: vec![],
        },
        &options,
        IntegrityPolicy::Fast,
    )
    .unwrap();
    assert_eq!(
        repeated_stats.added_blocks_reused,
        stats.added_blocks_reused
    );
    assert_eq!(fs::read(&repeated).unwrap(), fs::read(&output).unwrap());
    assert_eq!(stats.retained_snapshots, 1);
    assert_eq!(stats.added_snapshots, 2);
    assert_eq!(stats.added_blocks_reused, 1);
    assert_eq!(
        stats.retained_stored_bytes_copied,
        original_frame.len() as u64
    );
    assert!(stats.added_logical_bytes_compressed >= 256 * 1024);
    let after = inspect_archive(&output, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(after.applications[0].snapshots.len(), 3);
    assert_eq!(
        after.applications[0].metadata_snapshot_identity,
        Some(base_identity)
    );
    assert_eq!(
        after.applications[0]
            .snapshots
            .iter()
            .find(|s| s.snapshot_type == SnapshotType::Dlc)
            .unwrap()
            .program_id,
        None
    );
    assert_eq!(
        block_frames(&output)[0],
        original_frame,
        "retained stored Zstandard bytes changed"
    );
    let block_count = header_u64(&output, 96);
    assert_eq!(
        block_count, 3,
        "unreferenced or duplicate physical blocks remain"
    );

    let update = after.applications[0]
        .snapshots
        .iter()
        .find(|s| s.snapshot_type == SnapshotType::Update)
        .unwrap()
        .identity;
    let in_place = repack_archive(
        &output,
        &output,
        RepackPlan {
            remove_snapshots: vec![update],
            additions: ArchiveInput::new(vec![]),
            metadata_sources: vec![],
        },
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert_eq!(in_place.removed_snapshots, 1);
    let final_summary = inspect_archive(&output, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(final_summary.applications[0].snapshots.len(), 2);
    assert!(final_summary.applications[0]
        .snapshots
        .iter()
        .any(|s| s.snapshot_type == SnapshotType::Dlc));
    assert_eq!(
        header_u64(&output, 96),
        2,
        "removed update left an orphan block"
    );

    let dlc_id = final_summary.applications[0]
        .snapshots
        .iter()
        .find(|snapshot| snapshot.snapshot_type == SnapshotType::Dlc)
        .unwrap()
        .identity;
    repack_archive(
        &output,
        &output,
        RepackPlan {
            remove_snapshots: vec![dlc_id],
            additions: ArchiveInput::new(vec![]),
            metadata_sources: vec![],
        },
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert_eq!(
        header_u64(&output, 96),
        1,
        "removed DLC left orphan storage"
    );
    assert_eq!(
        inspect_archive(&output, &options, IntegrityPolicy::Strict)
            .unwrap()
            .applications[0]
            .snapshots
            .len(),
        1
    );

    repack_archive(
        &output,
        &output,
        RepackPlan {
            remove_snapshots: vec![],
            additions: single_update_input(base_identity, 2, vec![0x66; 128 * 1024]),
            metadata_sources: vec![],
        },
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    let repeated_edit = inspect_archive(&output, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(repeated_edit.applications[0].snapshots.len(), 2);
    assert_eq!(
        header_u64(&output, 96),
        2,
        "repeated edits accumulated dead blocks"
    );
    assert_eq!(block_frames(&output)[0], original_frame);
}

#[test]
fn no_op_does_not_change_source_and_failed_edit_preserves_destination() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let destination = temp.path().join("destination.nx");
    write_archive(
        base_input(vec![0x11; 128]),
        &source,
        &WriterOptions::default(),
    )
    .unwrap();
    let original = fs::read(&source).unwrap();
    let selected_identity =
        inspect_archive(&source, &WriterOptions::default(), IntegrityPolicy::Fast)
            .unwrap()
            .applications[0]
            .snapshots[0]
            .identity;
    let stats = repack_archive(
        &source,
        &source,
        RepackPlan::new(),
        &WriterOptions::default(),
        IntegrityPolicy::Fast,
    )
    .unwrap();
    assert!(stats.no_op);
    assert_eq!(fs::read(&source).unwrap(), original);
    let explicit_same_source = repack_archive(
        &source,
        &destination,
        RepackPlan {
            remove_snapshots: vec![],
            additions: ArchiveInput::new(vec![]),
            metadata_sources: vec![MetadataSourceSelection {
                platform: Platform::Switch1,
                application_id: APP,
                snapshot_identity: selected_identity,
            }],
        },
        &WriterOptions::default(),
        IntegrityPolicy::Fast,
    )
    .unwrap();
    assert!(!explicit_same_source.no_op);
    assert!(destination.exists());
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(
        inspect_archive(
            &destination,
            &WriterOptions::default(),
            IntegrityPolicy::Strict
        )
        .unwrap()
        .applications[0]
            .metadata_snapshot_identity,
        Some(selected_identity)
    );
    fs::write(&destination, b"existing destination").unwrap();
    let error = repack_archive(
        &source,
        &destination,
        RepackPlan {
            remove_snapshots: vec![[0x99; 32]],
            additions: ArchiveInput::new(vec![]),
            metadata_sources: vec![],
        },
        &WriterOptions::default(),
        IntegrityPolicy::Fast,
    )
    .unwrap_err();
    assert_eq!(error.kind(), WriterErrorKind::InvalidInput);
    assert_eq!(fs::read(&destination).unwrap(), b"existing destination");
}

#[test]
fn unchanged_repack_to_other_path_publishes_a_valid_destination() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let absent = temp.path().join("absent.nx");
    let existing = temp.path().join("existing.nx");
    let options = WriterOptions::default();
    write_archive(base_input(vec![0x11; 4096]), &source, &options).unwrap();
    let original = fs::read(&source).unwrap();
    let summary = inspect_archive(&source, &options, IntegrityPolicy::Strict).unwrap();

    let to_absent = repack_archive(
        &source,
        &absent,
        RepackPlan::new(),
        &options,
        IntegrityPolicy::Fast,
    )
    .unwrap();
    assert!(!to_absent.no_op);
    assert_eq!(
        inspect_archive(&absent, &options, IntegrityPolicy::Strict).unwrap(),
        summary
    );

    fs::write(&existing, b"stale destination").unwrap();
    let to_existing = repack_archive(
        &source,
        &existing,
        RepackPlan::new(),
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert!(!to_existing.no_op);
    assert_eq!(
        inspect_archive(&existing, &options, IntegrityPolicy::Strict).unwrap(),
        summary
    );
    assert_eq!(fs::read(&source).unwrap(), original);

    // Another spelling of the source path still names the same file.
    let respelled = temp.path().join(".").join("source.nx");
    let same = repack_archive(
        &source,
        &respelled,
        RepackPlan::new(),
        &options,
        IntegrityPolicy::Fast,
    )
    .unwrap();
    assert!(same.no_op);
    assert_eq!(fs::read(&source).unwrap(), original);
}

fn with_title(mut input: ArchiveInput, title: &str) -> ArchiveInput {
    input.applications[0].snapshots[0].metadata = Some(SnapshotMetadata {
        title: Some(title.into()),
        publisher: Some("NX tests".into()),
        icon: None,
        nacp: None,
        localized: vec![],
        opaque: vec![],
    });
    input
}

fn add_only(source: &Path, destination: &Path, additions: ArchiveInput) {
    repack_archive(
        source,
        destination,
        RepackPlan {
            remove_snapshots: vec![],
            additions,
            metadata_sources: vec![],
        },
        &WriterOptions::default(),
        IntegrityPolicy::Strict,
    )
    .unwrap();
}

#[test]
fn appended_update_metadata_is_selected_by_version_unless_explicitly_pinned() {
    let temp = TempDir::new();
    let source = temp.path().join("base.nx");
    let updated = temp.path().join("updated.nx");
    let pinned = temp.path().join("pinned.nx");
    let options = WriterOptions::default();
    write_archive(base_input(vec![0x11; 1024]), &source, &options).unwrap();
    let base_identity = inspect_archive(&source, &options, IntegrityPolicy::Strict)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;

    add_only(
        &source,
        &updated,
        with_title(
            single_update_input(base_identity, 2, vec![0x22; 1024]),
            "Synthetic update v2",
        ),
    );
    let summary = inspect_archive(&updated, &options, IntegrityPolicy::Strict).unwrap();
    let application = &summary.applications[0];
    let update_identity = application
        .snapshots
        .iter()
        .find(|s| s.snapshot_type == SnapshotType::Update)
        .unwrap()
        .identity;
    assert_eq!(
        application.metadata_snapshot_identity,
        Some(update_identity)
    );

    // A newer update without display metadata is retained but cannot become
    // the metadata source; v2 stays selected.
    let without_metadata = temp.path().join("v3-no-metadata.nx");
    add_only(
        &updated,
        &without_metadata,
        single_update_input(base_identity, 3, vec![0x23; 1024]),
    );
    let v3 = inspect_archive(&without_metadata, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(v3.applications[0].snapshots.len(), 3);
    assert_eq!(
        v3.applications[0].metadata_snapshot_identity,
        Some(update_identity)
    );

    repack_archive(
        &updated,
        &pinned,
        RepackPlan {
            remove_snapshots: vec![],
            additions: ArchiveInput::new(vec![]),
            metadata_sources: vec![MetadataSourceSelection {
                platform: Platform::Switch1,
                application_id: APP,
                snapshot_identity: base_identity,
            }],
        },
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert_eq!(
        inspect_archive(&pinned, &options, IntegrityPolicy::Strict)
            .unwrap()
            .applications[0]
            .metadata_snapshot_identity,
        Some(base_identity)
    );

    // A later edit without an explicit selection re-applies the default rule
    // and keeps the older update.
    let readded = temp.path().join("readded.nx");
    add_only(
        &pinned,
        &readded,
        with_title(
            single_update_input(base_identity, 4, vec![0x24; 1024]),
            "Synthetic update v4",
        ),
    );
    let after = inspect_archive(&readded, &options, IntegrityPolicy::Strict).unwrap();
    let v4_identity = after.applications[0]
        .snapshots
        .iter()
        .find(|s| s.numeric_version == 4)
        .unwrap()
        .identity;
    assert_eq!(
        after.applications[0].metadata_snapshot_identity,
        Some(v4_identity)
    );
    assert!(after.applications[0]
        .snapshots
        .iter()
        .any(|s| s.identity == update_identity));
}

fn source_resource_fixture(duplicates: bool) -> ArchiveInput {
    let mut input = base_input(vec![0x55; 4096]);
    input.source_resources.push(SourceResourceInput {
        platform: Platform::Switch1,
        application_id: APP,
        kind: 0,
        root: tree(vec![
            EntryInput::file("old.rec", FileInput::from_bytes(vec![0x22; 128])),
            EntryInput::file("shared.rec", FileInput::from_bytes(vec![0x55; 4096])),
        ]),
    });
    input.source_resources.push(SourceResourceInput {
        platform: Platform::Switch1,
        application_id: APP,
        kind: 0,
        root: tree(vec![EntryInput::file(
            if duplicates { "old.rec" } else { "new.rec" },
            FileInput::from_bytes(if duplicates {
                vec![0x22; 128]
            } else {
                vec![0x44; 128]
            }),
        )]),
    });
    input
}

fn resource_selector(path: &Path, filename: &str) -> SourceResourceRemoval {
    let resources =
        inspect_source_resources(path, Platform::Switch1, APP, &WriterOptions::default()).unwrap();
    let file = resources
        .iter()
        .flat_map(|(_, files)| files)
        .find(|file| file.path == filename)
        .unwrap();
    SourceResourceRemoval {
        platform: Platform::Switch1,
        application_id: APP,
        kind: 0,
        filename: filename.to_string(),
        size: file.size,
        sha256: file.digest,
    }
}

fn remaining_source_names(path: &Path) -> Vec<String> {
    let mut names: Vec<_> =
        inspect_source_resources(path, Platform::Switch1, APP, &WriterOptions::default())
            .unwrap()
            .into_iter()
            .flat_map(|(_, files)| files.into_iter().map(|file| file.path))
            .collect();
    names.sort();
    names
}

#[test]
fn source_resource_removal_is_atomic_with_snapshot_removal_and_keeps_shared_blocks() {
    let temp = TempDir::new();
    let base = temp.path().join("base.nx");
    let source = temp.path().join("with-update.nx");
    let destination = temp.path().join("result.nx");
    let options = WriterOptions::default();
    write_archive(source_resource_fixture(false), &base, &options).unwrap();
    let base_id = inspect_archive(&base, &options, IntegrityPolicy::Strict)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    add_only(
        &base,
        &source,
        single_update_input(base_id, 1, vec![9; 256]),
    );
    let update_id = inspect_archive(&source, &options, IntegrityPolicy::Fast)
        .unwrap()
        .applications[0]
        .snapshots
        .iter()
        .find(|snapshot| snapshot.snapshot_type == SnapshotType::Update)
        .unwrap()
        .identity;
    let old = resource_selector(&source, "old.rec");
    let mut plan = RepackPlan::new();
    plan.remove_snapshots.push(update_id);
    let stats = repack_archive_with_source_removals(
        &source,
        &destination,
        plan,
        vec![old],
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert_eq!(stats.removed_snapshots, 1);
    assert_eq!(
        remaining_source_names(&source),
        vec!["new.rec", "old.rec", "shared.rec"]
    );
    assert_eq!(
        remaining_source_names(&destination),
        vec!["new.rec", "shared.rec"]
    );
    let after = inspect_archive(&destination, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(after.applications[0].snapshots.len(), 1);
    assert_eq!(after.applications[0].snapshots[0].identity, base_id);

    // Removing the final file of each root also removes its resource descriptor.
    let new = resource_selector(&destination, "new.rec");
    let in_place = repack_archive_with_source_removals(
        &destination,
        &destination,
        RepackPlan::new(),
        vec![new],
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert!(!in_place.no_op);
    assert_eq!(remaining_source_names(&destination), vec!["shared.rec"]);
    let shared = resource_selector(&destination, "shared.rec");
    repack_archive_with_source_removals(
        &destination,
        &destination,
        RepackPlan::new(),
        vec![shared],
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert!(remaining_source_names(&destination).is_empty());
    compact_archive(&destination, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(
        inspect_archive(&destination, &options, IntegrityPolicy::Strict)
            .unwrap()
            .applications[0]
            .snapshots[0]
            .identity,
        base_id
    );
}

#[test]
fn source_resource_removal_rejects_missing_ambiguous_and_duplicate_selectors_without_publishing() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let destination = temp.path().join("destination.nx");
    let options = WriterOptions::default();
    write_archive(source_resource_fixture(true), &source, &options).unwrap();
    fs::write(&destination, b"preserve me").unwrap();
    let selected = resource_selector(&source, "old.rec");
    let mut invalid = selected.clone();
    invalid.sha256 = [0; 32];
    let mut system = selected.clone();
    system.kind = 1;
    for removals in [
        vec![selected.clone()],
        vec![invalid],
        vec![selected.clone(), selected],
        vec![system],
    ] {
        let error = repack_archive_with_source_removals(
            &source,
            &destination,
            RepackPlan::new(),
            removals,
            &options,
            IntegrityPolicy::Fast,
        )
        .unwrap_err();
        assert_eq!(error.kind(), WriterErrorKind::InvalidInput);
        assert_eq!(fs::read(&destination).unwrap(), b"preserve me");
    }
}

#[test]
fn source_resource_removal_does_not_bypass_snapshot_dependency_validation() {
    let temp = TempDir::new();
    let base = temp.path().join("base.nx");
    let source = temp.path().join("source.nx");
    let options = WriterOptions::default();
    write_archive(source_resource_fixture(false), &base, &options).unwrap();
    let base_id = inspect_archive(&base, &options, IntegrityPolicy::Fast)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    add_only(
        &base,
        &source,
        single_update_input(base_id, 1, vec![3; 256]),
    );
    let original = fs::read(&source).unwrap();
    let mut plan = RepackPlan::new();
    plan.remove_snapshots.push(base_id);
    let error = repack_archive_with_source_removals(
        &source,
        &source,
        plan,
        vec![resource_selector(&source, "old.rec")],
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap_err();
    assert_eq!(error.kind(), WriterErrorKind::InvalidInput);
    assert_eq!(fs::read(&source).unwrap(), original);
}

#[test]
fn source_resource_removal_requires_every_selector_field_and_never_partially_publishes() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let destination = temp.path().join("destination.nx");
    let options = WriterOptions::default();
    write_archive(source_resource_fixture(false), &source, &options).unwrap();
    let original = fs::read(&source).unwrap();
    let selected = resource_selector(&source, "old.rec");
    let mut wrong = Vec::new();
    let mut item = selected.clone();
    item.platform = Platform::Switch2;
    wrong.push(item);
    let mut item = selected.clone();
    item.application_id += 1;
    wrong.push(item);
    let mut item = selected.clone();
    item.filename = "missing.rec".into();
    wrong.push(item);
    let mut item = selected.clone();
    item.size += 1;
    wrong.push(item);
    let mut item = selected.clone();
    item.sha256[0] ^= 1;
    wrong.push(item);
    for policy in [IntegrityPolicy::Fast, IntegrityPolicy::Strict] {
        for in_place in [false, true] {
            for absent in &wrong {
                fs::write(&destination, b"KEEP").unwrap();
                let output = if in_place { &source } else { &destination };
                let error = repack_archive_with_source_removals(
                    &source,
                    output,
                    RepackPlan::new(),
                    vec![selected.clone(), absent.clone()],
                    &options,
                    policy,
                )
                .unwrap_err();
                assert_eq!(error.kind(), WriterErrorKind::InvalidInput);
                assert_eq!(fs::read(&source).unwrap(), original);
                assert_eq!(fs::read(&destination).unwrap(), b"KEEP");
            }
        }
    }
}

#[test]
fn source_resource_removal_preserves_blocks_referenced_by_games_and_extras() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let destination = temp.path().join("destination.nx");
    let options = WriterOptions::default();
    let mut input = source_resource_fixture(false);
    input.extra_files = Some(tree(vec![EntryInput::file(
        "keep.bin",
        FileInput::from_bytes(vec![0x22; 128]),
    )]));
    write_archive(input, &source, &options).unwrap();
    assert_eq!(header_u64(&source, 96), 3);
    let removals = vec![
        resource_selector(&source, "old.rec"),
        resource_selector(&source, "shared.rec"),
    ];
    repack_archive_with_source_removals(
        &source,
        &destination,
        RepackPlan::new(),
        removals,
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert_eq!(remaining_source_names(&destination), vec!["new.rec"]);
    // Both removed files share their blocks with surviving trees. Strict
    // verification hashes all File records, including the retained extra.
    assert_eq!(header_u64(&destination, 96), 3);
    inspect_archive(&destination, &options, IntegrityPolicy::Strict).unwrap();
    compact_archive(&destination, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(header_u64(&destination, 96), 3);
}

#[test]
fn empty_source_resource_removals_keep_existing_no_op_and_output_bytes() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let plain = temp.path().join("plain.nx");
    let selected = temp.path().join("selected.nx");
    let options = WriterOptions::default();
    write_archive(source_resource_fixture(false), &source, &options).unwrap();
    let original = fs::read(&source).unwrap();
    let stats = repack_archive_with_source_removals(
        &source,
        &source,
        RepackPlan::new(),
        vec![],
        &options,
        IntegrityPolicy::Fast,
    )
    .unwrap();
    assert!(stats.no_op);
    assert_eq!(fs::read(&source).unwrap(), original);
    repack_archive(
        &source,
        &plain,
        RepackPlan::new(),
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    repack_archive_with_source_removals(
        &source,
        &selected,
        RepackPlan::new(),
        vec![],
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert_eq!(fs::read(&plain).unwrap(), fs::read(&selected).unwrap());
}

#[test]
fn configured_source_resource_limits_are_enforced() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let options = WriterOptions::default();
    write_archive(base_input(vec![0x33; 128]), &source, &options).unwrap();

    let limits = [
        WriterOptions {
            max_archive_size: fs::metadata(&source).unwrap().len() - 1,
            ..options.clone()
        },
        WriterOptions {
            max_file_size: 64,
            ..options.clone()
        },
        WriterOptions {
            max_path_component_bytes: 4,
            ..options.clone()
        },
    ];
    for limited in limits {
        let error = inspect_archive(&source, &limited, IntegrityPolicy::Fast).unwrap_err();
        assert_eq!(error.kind(), WriterErrorKind::ResourceLimit);
    }
}

#[test]
fn configured_source_record_count_limits_are_enforced_and_preserve_destination() {
    let temp = TempDir::new();
    let base = temp.path().join("base.nx");
    let source = temp.path().join("source.nx");
    let destination = temp.path().join("destination.nx");
    let options = WriterOptions::default();
    write_archive(base_input(vec![0x33; 128]), &base, &options).unwrap();
    let base_identity = inspect_archive(&base, &options, IntegrityPolicy::Fast)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    // Two snapshots: two root directories, two entries and two distinct files.
    add_only(
        &base,
        &source,
        single_update_input(base_identity, 1, vec![0x34; 128]),
    );
    inspect_archive(
        &source,
        &WriterOptions {
            max_files: 2,
            max_directories: 2,
            max_entries: 2,
            ..options.clone()
        },
        IntegrityPolicy::Fast,
    )
    .unwrap();

    let limits = [
        WriterOptions {
            max_files: 1,
            ..options.clone()
        },
        WriterOptions {
            max_directories: 1,
            ..options.clone()
        },
        WriterOptions {
            max_entries: 1,
            ..options.clone()
        },
    ];
    fs::write(&destination, b"keep me").unwrap();
    for limited in limits {
        let error = inspect_archive(&source, &limited, IntegrityPolicy::Fast).unwrap_err();
        assert_eq!(error.kind(), WriterErrorKind::ResourceLimit);
        let error = repack_archive(
            &source,
            &destination,
            RepackPlan::new(),
            &limited,
            IntegrityPolicy::Strict,
        )
        .unwrap_err();
        assert_eq!(error.kind(), WriterErrorKind::ResourceLimit);
        assert_eq!(fs::read(&destination).unwrap(), b"keep me");
    }
    let leftovers: Vec<_> = fs::read_dir(temp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !matches!(name.as_str(), "base.nx" | "source.nx" | "destination.nx"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
}

#[test]
fn repeated_add_remove_cycles_leave_no_orphan_blocks() {
    let temp = TempDir::new();
    let archive = temp.path().join("cycle.nx");
    // Appends only: automatic compaction is covered separately.
    let options = WriterOptions {
        auto_compact_free_percent: 0,
        ..WriterOptions::default()
    };
    write_archive(base_input(vec![0x51; 64 * 1024]), &archive, &options).unwrap();
    let base_bytes = fs::read(&archive).unwrap();
    let base_identity = inspect_archive(&archive, &options, IntegrityPolicy::Strict)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    assert_eq!(header_u64(&archive, 96), 1);
    for cycle in 1..=4u8 {
        // Each update shares one block with the base and adds one unique block.
        let mut bytes = vec![0x51; 64 * 1024];
        bytes.extend(std::iter::repeat(0x60 + cycle).take(64 * 1024));
        let added = repack_archive(
            &archive,
            &archive,
            RepackPlan {
                remove_snapshots: vec![],
                additions: single_update_input(base_identity, cycle.into(), bytes),
                metadata_sources: vec![],
            },
            &options,
            IntegrityPolicy::Fast,
        )
        .unwrap();
        assert_eq!(added.added_snapshots, 1);
        assert_eq!(header_u64(&archive, 96), 2, "cycle {cycle} add");
        let update = inspect_archive(&archive, &options, IntegrityPolicy::Strict)
            .unwrap()
            .applications[0]
            .snapshots
            .iter()
            .find(|s| s.snapshot_type == SnapshotType::Update)
            .unwrap()
            .identity;
        repack_archive(
            &archive,
            &archive,
            RepackPlan {
                remove_snapshots: vec![update],
                additions: ArchiveInput::new(vec![]),
                metadata_sources: vec![],
            },
            &options,
            IntegrityPolicy::Strict,
        )
        .unwrap();
        assert_eq!(header_u64(&archive, 96), 1, "cycle {cycle} remove");
        // In-place edits append; compaction returns the canonical base bytes.
        assert!(fs::metadata(&archive).unwrap().len() > base_bytes.len() as u64);
        let compacted = compact_archive(&archive, &options, IntegrityPolicy::Strict).unwrap();
        assert!(!compacted.in_place_append);
        assert_eq!(
            fs::read(&archive).unwrap(),
            base_bytes,
            "cycle {cycle} did not compact to the canonical base archive"
        );
    }
}

#[test]
fn same_file_edit_compacts_when_the_append_would_waste_space() {
    let temp = TempDir::new();
    let archive = temp.path().join("auto.nx");
    let options = WriterOptions {
        compressed_chunk_bytes: 64 * 1024,
        ..WriterOptions::default()
    };
    // Distinct 64 KiB chunks so the update's unique block is a large share.
    let base: Vec<u8> = (0..4 * 64 * 1024u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    write_archive(base_input(base.clone()), &archive, &options).unwrap();
    let base_bytes = fs::read(&archive).unwrap();
    let base_identity = inspect_archive(&archive, &options, IntegrityPolicy::Strict)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    let mut update = base.clone();
    update.extend(std::iter::repeat(0x77).take(64 * 1024));
    // Adding leaves only the old tail unused: a plain append.
    let added = repack_archive(
        &archive,
        &archive,
        RepackPlan {
            remove_snapshots: vec![],
            additions: single_update_input(base_identity, 1, update),
            metadata_sources: vec![],
        },
        &options,
        IntegrityPolicy::Fast,
    )
    .unwrap();
    assert!(added.in_place_append && !added.auto_compacted);
    let update_identity = inspect_archive(&archive, &options, IntegrityPolicy::Strict)
        .unwrap()
        .applications[0]
        .snapshots
        .iter()
        .find(|s| s.snapshot_type == SnapshotType::Update)
        .unwrap()
        .identity;
    let remove = || RepackPlan {
        remove_snapshots: vec![update_identity],
        additions: ArchiveInput::new(vec![]),
        metadata_sources: vec![],
    };
    // With the threshold off, removing appends and keeps the waste.
    let kept = temp.path().join("kept.nx");
    fs::copy(&archive, &kept).unwrap();
    let appended =
        repack_archive(&kept, &kept, remove(), &options, IntegrityPolicy::Strict).unwrap();
    assert!(appended.in_place_append && !appended.auto_compacted);
    assert!(appended.free_bytes * 100 >= appended.archive_bytes * 20);
    compact_archive(&kept, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(fs::read(&kept).unwrap(), base_bytes);
    // Callers can still opt in to automatic compaction.
    let automatic = WriterOptions {
        auto_compact_free_percent: 20,
        ..options.clone()
    };
    let removed = repack_archive(
        &archive,
        &archive,
        remove(),
        &automatic,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert!(removed.auto_compacted && !removed.in_place_append);
    assert_eq!(fs::read(&archive).unwrap(), base_bytes);
    let bad = WriterOptions {
        auto_compact_free_percent: 101,
        ..options
    };
    assert_eq!(
        compact_archive(&archive, &bad, IntegrityPolicy::Fast)
            .unwrap_err()
            .kind(),
        WriterErrorKind::InvalidInput
    );
}

#[test]
fn strict_detects_corrupt_old_payload_before_publication() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let destination = temp.path().join("destination.nx");
    write_archive(
        base_input(vec![0x22; 64 * 1024]),
        &source,
        &WriterOptions::default(),
    )
    .unwrap();
    let identity = inspect_archive(&source, &WriterOptions::default(), IntegrityPolicy::Fast)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    let data_offset = header_u64(&source, 56);
    let mut file = File::options().write(true).open(&source).unwrap();
    file.seek(SeekFrom::Start(data_offset)).unwrap();
    file.write_all(&[0xff]).unwrap();
    file.sync_all().unwrap();
    fs::write(&destination, b"keep me").unwrap();
    let result = repack_archive(
        &source,
        &destination,
        RepackPlan {
            remove_snapshots: vec![],
            additions: ArchiveInput::new(vec![]),
            metadata_sources: vec![MetadataSourceSelection {
                platform: Platform::Switch1,
                application_id: APP,
                snapshot_identity: identity,
            }],
        },
        &WriterOptions::default(),
        IntegrityPolicy::Strict,
    );
    assert!(result.is_err());
    assert_eq!(fs::read(&destination).unwrap(), b"keep me");
}

#[test]
fn strict_verifies_unreferenced_physical_blocks_before_compaction() {
    let temp = TempDir::new();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("shared_with_orphan_block.nx");
    let source = temp.path().join("corrupt-orphan.nx");
    let destination = temp.path().join("destination.nx");
    fs::copy(&fixture, &source).unwrap();
    fs::write(&destination, b"preserve destination").unwrap();

    let index_offset = header_u64(&source, 88);
    let block_count = header_u64(&source, 96);
    let manifest_offset = header_u64(&source, 72);
    let reference_count = read_u32_at(&source, manifest_offset + 28);
    let reference_offset = read_u64_at(&source, manifest_offset + 56);
    let mut file = File::open(&source).unwrap();
    let mut referenced = std::collections::HashSet::new();
    for index in 0..reference_count as u64 {
        file.seek(SeekFrom::Start(
            manifest_offset + reference_offset + index * 4,
        ))
        .unwrap();
        let mut value = [0u8; 4];
        file.read_exact(&mut value).unwrap();
        referenced.insert(u32::from_le_bytes(value));
    }
    drop(file);
    let orphan = (0..block_count)
        .map(|index| index as u32)
        .find(|index| !referenced.contains(index))
        .expect("fixture must contain an unreferenced physical block");
    assert_eq!(block_count - referenced.len() as u64, 1);
    let data_offset = read_u64_at(&source, index_offset + u64::from(orphan) * 24);
    let mut file = File::options()
        .read(true)
        .write(true)
        .open(&source)
        .unwrap();
    file.seek(SeekFrom::Start(data_offset)).unwrap();
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(data_offset)).unwrap();
    file.write_all(&[byte[0] ^ 0xff]).unwrap();
    file.sync_all().unwrap();

    let error = repack_archive(
        &source,
        &destination,
        RepackPlan::new(),
        &WriterOptions::default(),
        IntegrityPolicy::Strict,
    )
    .unwrap_err();
    assert_eq!(error.kind(), WriterErrorKind::Integrity);
    assert!(error.to_string().contains("source block CRC-32C mismatch"));
    assert_eq!(fs::read(&destination).unwrap(), b"preserve destination");
}

#[test]
fn rust_parser_accepts_the_independent_valid_fixture_corpus() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("tests")
        .join("fixtures");
    let names = [
        "minimal_valid.nx",
        "raw_only.nx",
        "nested_vfs.nx",
        "zstd_single.nx",
        "multi_block_file.nx",
        "mixed_raw_zstd.nx",
        "shared_blocks.nx",
        "shared_with_orphan_block.nx",
        "shared_snapshots.nx",
        "deleted_between_versions.nx",
        "multiple_programs.nx",
        "metadata_snapshot_tie.nx",
        "localized_metadata_icons.nx",
        "switch2_localized_icons.nx",
        "jpeg_icon.nx",
        "same_application_id_cross_platform.nx",
        "dlc_without_program.nx",
        "dlc_with_program.nx",
        "distinct_dlc_same_tree.nx",
        "dlc_only_application.nx",
        "empty_files.nx",
    ];
    for name in names {
        let summary = inspect_archive(
            fixtures.join(name),
            &WriterOptions::default(),
            IntegrityPolicy::Fast,
        )
        .unwrap_or_else(|error| panic!("Rust repack parser rejected {name}: {error}"));
        assert!(!summary.applications.is_empty(), "{name}");
    }
}

#[test]
fn removing_a_required_base_or_the_last_snapshot_fails_without_publication() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let with_update = temp.path().join("with-update.nx");
    let destination = temp.path().join("destination.nx");
    write_archive(
        base_input(vec![0x22; 64 * 1024]),
        &source,
        &WriterOptions::default(),
    )
    .unwrap();
    let base_identity = inspect_archive(&source, &WriterOptions::default(), IntegrityPolicy::Fast)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    repack_archive(
        &source,
        &with_update,
        RepackPlan {
            remove_snapshots: vec![],
            additions: addition_input(base_identity, vec![0x22; 64 * 1024]),
            metadata_sources: vec![],
        },
        &WriterOptions::default(),
        IntegrityPolicy::Fast,
    )
    .unwrap();
    fs::write(&destination, b"preserve me").unwrap();
    let base_removal = repack_archive(
        &with_update,
        &destination,
        RepackPlan {
            remove_snapshots: vec![base_identity],
            additions: ArchiveInput::new(vec![]),
            metadata_sources: vec![],
        },
        &WriterOptions::default(),
        IntegrityPolicy::Fast,
    )
    .unwrap_err();
    assert_eq!(base_removal.kind(), WriterErrorKind::InvalidInput);
    assert!(base_removal
        .to_string()
        .contains("dangling snapshot dependency"));
    assert_eq!(fs::read(&destination).unwrap(), b"preserve me");

    let only_base = temp.path().join("only-base.nx");
    write_archive(
        base_input(vec![0x11; 128]),
        &only_base,
        &WriterOptions::default(),
    )
    .unwrap();
    let only_identity =
        inspect_archive(&only_base, &WriterOptions::default(), IntegrityPolicy::Fast)
            .unwrap()
            .applications[0]
            .snapshots[0]
            .identity;
    let last_removal = repack_archive(
        &only_base,
        &destination,
        RepackPlan {
            remove_snapshots: vec![only_identity],
            additions: ArchiveInput::new(vec![]),
            metadata_sources: vec![],
        },
        &WriterOptions::default(),
        IntegrityPolicy::Fast,
    )
    .unwrap_err();
    assert_eq!(last_removal.kind(), WriterErrorKind::InvalidInput);
    assert_eq!(fs::read(&destination).unwrap(), b"preserve me");
}

#[test]
fn dangling_cross_application_and_invalid_metadata_inputs_are_rejected() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let destination = temp.path().join("destination.nx");
    write_archive(
        base_input(vec![0x22; 64 * 1024]),
        &source,
        &WriterOptions::default(),
    )
    .unwrap();
    let base_identity = inspect_archive(&source, &WriterOptions::default(), IntegrityPolicy::Fast)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    fs::write(&destination, b"keep destination").unwrap();

    let mut dangling = addition_input(base_identity, vec![0x22; 64 * 1024]);
    dangling.applications[0].snapshots[0].dependencies[0].target_snapshot_identity = [0x98; 32];
    let error = repack_archive(
        &source,
        &destination,
        RepackPlan {
            remove_snapshots: vec![],
            additions: dangling,
            metadata_sources: vec![],
        },
        &WriterOptions::default(),
        IntegrityPolicy::Fast,
    )
    .unwrap_err();
    assert_eq!(error.kind(), WriterErrorKind::InvalidInput);
    assert!(error.to_string().contains("dangling snapshot dependency"));
    assert_eq!(fs::read(&destination).unwrap(), b"keep destination");

    let mut cross_app = addition_input(base_identity, vec![0x22; 64 * 1024]);
    cross_app.applications[0].application_id = APP + 1;
    let error = repack_archive(
        &source,
        &destination,
        RepackPlan {
            remove_snapshots: vec![],
            additions: cross_app,
            metadata_sources: vec![],
        },
        &WriterOptions::default(),
        IntegrityPolicy::Fast,
    )
    .unwrap_err();
    assert_eq!(error.kind(), WriterErrorKind::InvalidInput);
    assert!(error.to_string().contains("different application"));
    assert_eq!(fs::read(&destination).unwrap(), b"keep destination");

    let mut invalid_metadata = addition_input(base_identity, vec![0x22; 64 * 1024]);
    invalid_metadata.applications[0].snapshots[0].metadata = Some(SnapshotMetadata {
        title: None,
        publisher: None,
        icon: None,
        nacp: None,
        localized: vec![],
        opaque: vec![],
    });
    let error = repack_archive(
        &source,
        &destination,
        RepackPlan {
            remove_snapshots: vec![],
            additions: invalid_metadata,
            metadata_sources: vec![],
        },
        &WriterOptions::default(),
        IntegrityPolicy::Fast,
    )
    .unwrap_err();
    assert_eq!(error.kind(), WriterErrorKind::InvalidInput);
    assert_eq!(fs::read(&destination).unwrap(), b"keep destination");

    let missing_source = repack_archive(
        temp.path().join("missing.nx"),
        &destination,
        RepackPlan::new(),
        &WriterOptions::default(),
        IntegrityPolicy::Fast,
    )
    .unwrap_err();
    assert_eq!(missing_source.kind(), WriterErrorKind::Io);
    assert_eq!(fs::read(&destination).unwrap(), b"keep destination");
}

#[test]
fn cancellation_after_staging_output_begins_preserves_destination() {
    let temp = TempDir::new();
    let source = temp.path().join("large-source.nx");
    let destination = temp.path().join("destination.nx");
    let mut state = 0x1357_9bdfu32;
    let mut bytes = Vec::with_capacity(32 * 1024 * 1024);
    while bytes.len() < bytes.capacity() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    let source_options = WriterOptions {
        chunk_size_bytes: 1024 * 1024,
        compression_mode: nxformat_packer::CompressionMode::RawOnly,
        ..WriterOptions::default()
    };
    write_archive(base_input(bytes), &source, &source_options).unwrap();
    let identity = inspect_archive(&source, &source_options, IntegrityPolicy::Fast)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    fs::write(&destination, b"keep destination").unwrap();

    let options = Arc::new(WriterOptions::default());
    let token = options.cancellation.clone();
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher_finished = finished.clone();
    let watcher_dir = temp.path().to_path_buf();
    let watcher = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !watcher_finished.load(Ordering::Acquire) {
            let staged = fs::read_dir(&watcher_dir).is_ok_and(|items| {
                items.flatten().any(|item| {
                    item.file_name()
                        .to_string_lossy()
                        .starts_with(".nx-repack-output-")
                })
            });
            if staged {
                token.cancel();
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        false
    });
    let result = repack_archive(
        &source,
        &destination,
        RepackPlan {
            remove_snapshots: vec![],
            additions: single_update_input(identity, 1, vec![0x65; 16 * 1024 * 1024]),
            metadata_sources: vec![],
        },
        &options,
        IntegrityPolicy::Fast,
    );
    finished.store(true, Ordering::Release);
    assert!(
        watcher.join().unwrap(),
        "watcher did not observe staged output"
    );
    assert_eq!(result.unwrap_err().kind(), WriterErrorKind::Cancelled);
    assert_eq!(fs::read(&destination).unwrap(), b"keep destination");
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
}

#[test]
fn cancellation_temp_budget_and_publish_failure_preserve_destination_and_cleanup() {
    let temp = TempDir::new();
    let source = temp.path().join("source.nx");
    let destination = temp.path().join("destination.nx");
    write_archive(
        base_input(vec![0x22; 64 * 1024]),
        &source,
        &WriterOptions::default(),
    )
    .unwrap();
    let identity = inspect_archive(&source, &WriterOptions::default(), IntegrityPolicy::Fast)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    fs::write(&destination, b"keep destination").unwrap();

    let cancelled_options = WriterOptions::default();
    cancelled_options.cancellation.cancel();
    let cancelled = repack_archive(
        &source,
        &destination,
        RepackPlan {
            remove_snapshots: vec![[0x99; 32]],
            additions: ArchiveInput::new(vec![]),
            metadata_sources: vec![],
        },
        &cancelled_options,
        IntegrityPolicy::Fast,
    )
    .unwrap_err();
    assert_eq!(cancelled.kind(), WriterErrorKind::Cancelled);
    assert_eq!(fs::read(&destination).unwrap(), b"keep destination");

    let limited = WriterOptions {
        max_temporary_disk_bytes: 1024,
        ..WriterOptions::default()
    };
    let limited_error = repack_archive(
        &source,
        &destination,
        RepackPlan {
            remove_snapshots: vec![],
            additions: addition_input(identity, vec![0x22; 64 * 1024]),
            metadata_sources: vec![],
        },
        &limited,
        IntegrityPolicy::Fast,
    )
    .unwrap_err();
    assert_eq!(limited_error.kind(), WriterErrorKind::ResourceLimit);
    assert_eq!(fs::read(&destination).unwrap(), b"keep destination");

    let blocked_destination = temp.path().join("destination-directory");
    fs::create_dir(&blocked_destination).unwrap();
    fs::write(blocked_destination.join("marker"), b"keep directory").unwrap();
    let publish_error = repack_archive(
        &source,
        &blocked_destination,
        RepackPlan {
            remove_snapshots: vec![],
            additions: addition_input(identity, vec![0x22; 64 * 1024]),
            metadata_sources: vec![],
        },
        &WriterOptions::default(),
        IntegrityPolicy::Fast,
    )
    .unwrap_err();
    assert_eq!(publish_error.kind(), WriterErrorKind::Io);
    assert_eq!(
        fs::read(blocked_destination.join("marker")).unwrap(),
        b"keep directory"
    );
    assert_eq!(fs::read(&destination).unwrap(), b"keep destination");
    assert_eq!(
        fs::read_dir(temp.path()).unwrap().count(),
        3,
        "failed publication left a repack temporary behind"
    );
}

#[cfg(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    windows
))]
#[test]
fn deletion_reclaims_only_unreferenced_blocks_when_supported() {
    let dir = TempDir::new();
    let path = dir.path().join("reclaim.nx");
    let options = WriterOptions {
        compression_mode: nxformat_packer::CompressionMode::RawOnly,
        ..Default::default()
    };
    let mut base = vec![0x11; 256 * 1024];
    base[0] = 0x42;
    let mut input = base_input(base.clone());
    let mut dlc = base.clone(); // The first block remains shared with the base.
    dlc.resize(2 * 1024 * 1024, 0x77);
    for (i, chunk) in dlc.chunks_exact_mut(256 * 1024).enumerate().skip(1) {
        chunk[0] = i as u8; // Keep every DLC-only block distinct.
    }
    input.applications[0].snapshots.push(SnapshotInput {
        addon_title_id: None,
        native: Default::default(),
        program_id: None,
        snapshot_type: SnapshotType::Dlc,
        numeric_version: 7,
        content_id: Some([0x55; 16]),
        build_id: None,
        display_version: None,
        dependencies: vec![],
        metadata: None,
        root: tree(vec![EntryInput::file(
            "dlc.bin",
            FileInput::from_bytes(dlc),
        )]),
    });
    write_archive(input, &path, &options).unwrap();
    let original = inspect_archive(&path, &options, IntegrityPolicy::Strict).unwrap();
    let dlc = original.applications[0]
        .snapshots
        .iter()
        .find(|snapshot| snapshot.snapshot_type == SnapshotType::Dlc)
        .unwrap();
    let before = fs::metadata(&path).unwrap().len();
    // A plain POSIX read handle can exist outside the coordinated reader API.
    // Reclamation must preserve all data visible to that open handle.
    #[cfg(unix)]
    let mut old = File::open(&path).unwrap();
    #[cfg(unix)]
    let mut original_bytes = Vec::new();
    #[cfg(unix)]
    old.read_to_end(&mut original_bytes).unwrap();

    let mut plan = RepackPlan::new();
    plan.remove_snapshots.push(dlc.identity);
    let stats = repack_archive(&path, &path, plan, &options, IntegrityPolicy::Strict).unwrap();
    assert!(stats.in_place_append && !stats.auto_compacted);
    assert!(stats.output_bytes_written < 64 * 1024);
    assert!(fs::metadata(&path).unwrap().len() >= before);
    let after = inspect_archive(&path, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(after.applications[0].snapshots.len(), 1);
    #[cfg(unix)]
    {
        let mut still_readable = vec![0; original_bytes.len() - 8192];
        old.seek(SeekFrom::Start(8192)).unwrap();
        old.read_exact(&mut still_readable).unwrap();
        assert_eq!(still_readable, original_bytes[8192..]);
    }
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut volume: libc::statfs = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::statfs(name.as_ptr(), &mut volume) }, 0);
        let filesystem = unsafe { std::ffi::CStr::from_ptr(volume.f_fstypename.as_ptr()) };
        if filesystem.to_bytes() == b"apfs" {
            assert!(stats.reclaimed_disk_bytes >= 1024 * 1024, "{stats:?}");
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, RawHandle};
        #[link(name = "kernel32")]
        extern "system" {
            fn GetVolumeInformationByHandleW(
                handle: RawHandle,
                name: *mut u16,
                name_len: u32,
                serial: *mut u32,
                max_component: *mut u32,
                flags: *mut u32,
                filesystem: *mut u16,
                filesystem_len: u32,
            ) -> i32;
        }
        let file = File::open(&path).unwrap();
        let mut flags = 0;
        let mut filesystem = [0u16; 16];
        // SAFETY: the handle and output buffers remain live for the call.
        let result = unsafe {
            GetVolumeInformationByHandleW(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            )
        };
        assert_ne!(result, 0);
        if String::from_utf16_lossy(&filesystem).trim_end_matches('\0') == "NTFS"
            && flags & 0x40 != 0
        {
            assert!(stats.reclaimed_disk_bytes >= 1024 * 1024, "{stats:?}");
        }
    }
    #[cfg(unix)]
    {
        let fallback = dir.path().join("fallback.nx");
        let alias = dir.path().join("alias.nx");
        fs::write(&fallback, original_bytes).unwrap();
        fs::hard_link(&fallback, &alias).unwrap();
        let mut removal = RepackPlan::new();
        removal.remove_snapshots.push(dlc.identity);
        let fallback_stats = repack_archive(
            &fallback,
            &fallback,
            removal,
            &options,
            IntegrityPolicy::Strict,
        )
        .unwrap();
        assert_eq!(fallback_stats.reclaimed_disk_bytes, 0);
        assert!(fallback_stats.in_place_append);
        assert_eq!(
            inspect_archive(&alias, &options, IntegrityPolicy::Strict).unwrap(),
            after
        );
    }
    eprintln!("reclaimed {} bytes", stats.reclaimed_disk_bytes);
}

#[test]
fn edit_repairs_the_damaged_page_without_overwriting_the_survivor() {
    let dir = TempDir::new();
    let path = dir.path().join("repair.nx");
    let options = WriterOptions::default();
    write_archive(base_input(vec![0x11; 16384]), &path, &options).unwrap();
    let fresh = fs::read(&path).unwrap();
    let base_identity = inspect_archive(&path, &options, IntegrityPolicy::Strict)
        .unwrap()
        .applications[0]
        .snapshots[0]
        .identity;
    repack_archive(
        &path,
        &path,
        RepackPlan {
            additions: single_update_input(base_identity, 1, vec![0x22; 16384]),
            ..RepackPlan::new()
        },
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    let edited = fs::read(&path).unwrap();
    for (fixture, failed, expected_generation) in [(&fresh, 0, 0), (&edited, 0, 1), (&edited, 1, 0)]
    {
        let mut bytes = fixture.clone();
        let start = failed * 4096;
        bytes[start..start + 4096].fill(0xa5);
        fs::write(&path, &bytes).unwrap();
        let base = inspect_archive(&path, &options, IntegrityPolicy::Strict).unwrap();
        let base_identity = base.applications[0].snapshots[0].identity;
        let plan = RepackPlan {
            additions: single_update_input(base_identity, 77, vec![0x59; 16384]),
            ..RepackPlan::new()
        };
        let result = repack_archive(&path, &path, plan, &options, IntegrityPolicy::Strict).unwrap();
        assert!(result.in_place_append);
        let after = fs::read(&path).unwrap();
        let survivor = (1 - failed) * 4096;
        assert_eq!(
            &after[survivor..survivor + 4096],
            &bytes[survivor..survivor + 4096]
        );
        assert_eq!(
            read_u64_at(&path, (start + 216) as u64),
            expected_generation + 1
        );
        assert_eq!(&after[8192..bytes.len()], &bytes[8192..]);
        assert!(after[start + 256..start + 4096]
            .iter()
            .all(|&byte| byte == 0));
    }
}

#[test]
fn maximum_generation_is_readable_but_cannot_wrap_on_edit() {
    let dir = TempDir::new();
    let path = dir.path().join("max-generation.nx");
    let options = WriterOptions::default();
    let fixture = include_bytes!("../../tests/fixtures/max_generation.nx");
    fs::write(&path, fixture).unwrap();
    let summary = inspect_archive(&path, &options, IntegrityPolicy::Strict).unwrap();
    let mut plan = RepackPlan::new();
    plan.remove_snapshots
        .push(summary.applications[0].snapshots[1].identity);
    let error = repack_archive(&path, &path, plan, &options, IntegrityPolicy::Strict).unwrap_err();
    assert_eq!(error.kind(), WriterErrorKind::ResourceLimit);
    assert!(error.message().contains("generation overflow"));
    assert_eq!(fs::read(&path).unwrap(), fixture);
}

#[test]
fn raw_compaction_failures_preserve_original_archive() {
    let temp = TempDir::new();
    let archive = temp.path().join("compressed.nx");
    let payload: Vec<u8> = (0..16).flat_map(|i| vec![i; 256 * 1024]).collect();
    write_archive(base_input(payload), &archive, &WriterOptions::default()).unwrap();
    let original = fs::read(&archive).unwrap();
    for failure in 0..4 {
        let mut options = WriterOptions {
            compression_mode: nxformat_packer::CompressionMode::RawOnly,
            ..WriterOptions::default()
        };
        match failure {
            0 => options.max_temporary_disk_bytes = 1024 * 1024,
            1 => options.max_archive_size = 64 * 1024,
            2 => options.max_memory_bytes = 8 * 1024 * 1024,
            _ => options.cancellation.cancel(),
        }
        assert!(
            compact_archive(&archive, &options, IntegrityPolicy::Strict).is_err(),
            "failure {failure}"
        );
        assert_eq!(fs::read(&archive).unwrap(), original, "failure {failure}");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }
    // A damaged compressed payload must also fail in Fast mode before publish.
    let mut damaged = original;
    damaged[8192] ^= 1;
    fs::write(&archive, &damaged).unwrap();
    let raw = WriterOptions {
        compression_mode: nxformat_packer::CompressionMode::RawOnly,
        ..WriterOptions::default()
    };
    assert!(compact_archive(&archive, &raw, IntegrityPolicy::Fast).is_err());
    assert_eq!(fs::read(&archive).unwrap(), damaged);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
}
