use super::super::structures::NpdmProfile;
use super::super::synthetic::npdm;
use super::super::synthetic_nup::*;
use super::*;
use crate::{inspect_archive, IntegrityPolicy, SnapshotType};

const APP: u64 = 0x0400_0000_0000_1000;

fn work(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nx-nup-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}
fn nup_options() -> NupOptions {
    NupOptions {
        platform: Some(crate::Platform::Switch2),
        ..Default::default()
    }
}
fn limits() -> ImportLimits {
    ImportLimits::default()
}

#[test]
fn base_update_and_metadata_convert_to_nx() {
    for zstd in [false, true] {
        let dir = work(if zstd { "zstd" } else { "raw" });
        let nup = dir.join("game.nup");
        fs::write(
            &nup,
            build(&[base(APP), update(APP, 65536, 0), addon(APP)], zstd),
        )
        .unwrap();
        let out = dir.join("game.nx");
        let options = WriterOptions::default();
        // Add-ons become DLC snapshots stored by their add-on title ID.
        let with_dlc = dir.join("dlc.nx");
        let stats = convert_nup(&nup, &with_dlc, &nup_options(), &limits(), &options).unwrap();
        assert!(stats.skipped_addons.is_empty());
        let summary = inspect_archive(&with_dlc, &options, IntegrityPolicy::Strict).unwrap();
        let dlc: Vec<_> = summary.applications[0]
            .snapshots
            .iter()
            .filter(|s| s.snapshot_type == SnapshotType::Dlc)
            .collect();
        assert_eq!(dlc.len(), 1);
        assert_eq!(dlc[0].addon_title_id, Some(APP + 0x1001));
        assert_eq!((dlc[0].content_id, dlc[0].program_id), (None, None));
        let mut buf = [0u8; 3];
        crate::read_snapshot_file_range(
            &with_dlc,
            dlc[0].identity,
            "romfs/dlc.bin",
            0,
            &mut buf,
            &options,
        )
        .unwrap();
        assert_eq!(&buf, b"dlc");
        fs::remove_file(&with_dlc).unwrap();
        let stats = convert_nup(
            &nup,
            &out,
            &NupOptions {
                skip_addons: true,
                ..nup_options()
            },
            &limits(),
            &options,
        )
        .unwrap();
        assert_eq!(
            stats.skipped_addons,
            vec![SkippedAddon {
                title_id: APP + 0x1001,
                version: 0
            }]
        );
        // Only the NUP and the NX remain: the stage is gone.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        let summary = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
        assert_eq!(summary.applications.len(), 1);
        let app = &summary.applications[0];
        assert_eq!(
            (app.platform, app.application_id),
            (crate::Platform::Switch2, APP)
        );
        let types: Vec<_> = app
            .snapshots
            .iter()
            .map(|s| (s.snapshot_type, s.numeric_version))
            .collect();
        assert_eq!(
            types,
            vec![(SnapshotType::Base, 0), (SnapshotType::Update, 65536)]
        );
        let (b, u) = (&app.snapshots[0], &app.snapshots[1]);
        assert!(b.has_display_metadata && u.has_display_metadata);
        assert_eq!(b.build_id, Some([0x41; 32]));
        assert_eq!(u.build_id, Some([0x42; 32]));
        assert_eq!(u.dependencies.len(), 1);
        assert_eq!(u.dependencies[0].target_snapshot_identity, b.identity);
        let read = |identity, path: &str, size: usize| {
            let mut buf = vec![0; size];
            crate::read_snapshot_file_range(&out, identity, path, 0, &mut buf, &options)
                .map(|_| buf)
        };
        assert_eq!(
            read(u.identity, "romfs/shadowed.bin", 14).unwrap(),
            b"update version"
        );
        assert_eq!(
            read(u.identity, "romfs/common.bin", 16).unwrap(),
            b"base common data"
        );
        assert_eq!(
            read(u.identity, "romfs/sub/deep.bin", 5000).unwrap(),
            vec![7; 5000]
        );
        assert_eq!(
            read(b.identity, "romfs/shadowed.bin", 12).unwrap(),
            b"base version"
        );
        assert!(read(b.identity, "romfs/added.bin", 1).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn staged_metadata_projects_localized_titles_and_icons() {
    let dir = work("meta");
    let nup = dir.join("game.nup");
    fs::write(&nup, build(&[base(APP)], false)).unwrap();
    let stage = stage_nup(&nup, &dir, &nup_options(), &limits(), &Default::default()).unwrap();
    let root = super::super::secure::Root::open(&stage.package()).unwrap();
    let declaration = &stage.manifest.snapshots[0];
    assert_eq!(declaration.icons.len(), 2);
    let (meta, version, _) = super::super::convert::control_metadata(&root, declaration, &limits())
        .unwrap()
        .unwrap();
    assert_eq!(version.as_deref(), Some("1.2.0"));
    assert_eq!(meta.title.as_deref(), Some("Synthetic Game"));
    assert_eq!(meta.publisher.as_deref(), Some("Synthetic publisher"));
    assert!(meta.icon.is_some() && meta.nacp.is_some());
    let languages: Vec<_> = meta
        .localized
        .iter()
        .map(|l| (l.language.as_str(), l.title.as_deref(), l.icon.is_some()))
        .collect();
    assert_eq!(
        languages,
        vec![
            ("en-us", Some("Synthetic Game"), true),
            ("fr", Some("Jeu synthetique"), true)
        ]
    );
    drop(root);
    let package = stage.package();
    drop(stage);
    assert!(!package.exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn standalone_update_needs_the_flag_and_no_base() {
    let dir = work("standalone");
    let nup = dir.join("u.nup");
    let out = dir.join("u.nx");
    let options = WriterOptions::default();
    fs::write(&nup, build(&[update(APP, 131072, 0)], true)).unwrap();
    assert!(convert_nup(&nup, &out, &nup_options(), &limits(), &options).is_err());
    fs::write(&nup, build(&[base(APP), update(APP, 131072, 1)], true)).unwrap();
    assert!(convert_nup(&nup, &out, &nup_options(), &limits(), &options).is_err());
    fs::write(&nup, build(&[update(APP, 131072, 1)], true)).unwrap();
    convert_nup(&nup, &out, &nup_options(), &limits(), &options).unwrap();
    let summary = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
    let s = &summary.applications[0].snapshots;
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].snapshot_type, SnapshotType::StandaloneUpdate);
    assert!(s[0].dependencies.is_empty());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn damaged_nup_is_rejected_before_publication() {
    let dir = work("damaged");
    let nup = dir.join("game.nup");
    let out = dir.join("game.nx");
    let good = build(&[base(APP)], true);
    let options = WriterOptions::default();
    // A TOC byte, a payload byte (file SHA-256 mismatch), and the file size field.
    let blob = u64::from_le_bytes(good[56..64].try_into().unwrap()) as usize;
    for at in [SUPERBLOCK + 0x34, blob + 3, 0x40] {
        let mut bad = good.clone();
        bad[at] ^= 0x5a;
        fs::write(&nup, &bad).unwrap();
        assert!(
            convert_nup(&nup, &out, &nup_options(), &limits(), &options).is_err(),
            "corruption at {at} accepted"
        );
        assert!(!out.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "stage left behind");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn npdm_profiles_follow_the_acid_position() {
    assert_eq!(
        npdm_profile(&npdm(APP, NpdmProfile::MetaAcid3072V1)),
        "meta-acid3072-v1"
    );
    assert_eq!(
        npdm_profile(&npdm(0x0100_0000_0000_1000, NpdmProfile::MetaAcid2048V1)),
        "meta-acid2048-v1"
    );
    assert_eq!(npdm_profile(b"not an npdm"), "none");
}

#[test]
fn data_subtree_is_preserved_in_base_and_overlay_updates() {
    for zstd in [false, true] {
        let dir = work(if zstd { "data-zstd" } else { "data-raw" });
        let nup = dir.join("game.nup");
        let out = dir.join("game.nx");
        let mut b = base(APP);
        b.data = Some(vec![
            Node::File("common.bin", b"inherited data".to_vec()),
            Node::File("shadow.bin", b"old".to_vec()),
            Node::Dir("empty", vec![]),
        ]);
        let mut u = update(APP, 65536, 0);
        u.data = Some(vec![
            Node::File("shadow.bin", b"new".to_vec()),
            Node::Dir("sub", vec![Node::File("nested.bin", b"nested".to_vec())]),
        ]);
        fs::write(&nup, build(&[b, u], zstd)).unwrap();
        let options = WriterOptions::default();
        convert_nup(&nup, &out, &nup_options(), &limits(), &options).unwrap();
        let summary = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
        for snapshot in &summary.applications[0].snapshots {
            let mut common = [0; 14];
            crate::read_snapshot_file_range(
                &out,
                snapshot.identity,
                "data/common.bin",
                0,
                &mut common,
                &options,
            )
            .unwrap();
            assert_eq!(&common, b"inherited data");
            let mut shadow = [0; 3];
            crate::read_snapshot_file_range(
                &out,
                snapshot.identity,
                "data/shadow.bin",
                0,
                &mut shadow,
                &options,
            )
            .unwrap();
            assert_eq!(
                &shadow,
                if snapshot.snapshot_type == SnapshotType::Base {
                    b"old"
                } else {
                    b"new"
                }
            );
        }
        let mut nested = [0; 6];
        crate::read_snapshot_file_range(
            &out,
            summary.applications[0].snapshots[1].identity,
            "data/sub/nested.bin",
            0,
            &mut nested,
            &options,
        )
        .unwrap();
        assert_eq!(&nested, b"nested");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn nup_staging_and_writer_share_a_disk_budget_and_fail_without_publication() {
    let dir = work("budgets");
    let nup = dir.join("game.nup");
    let out = dir.join("game.nx");
    fs::write(&nup, build(&[base(APP)], true)).unwrap();
    fs::write(&out, b"keep destination").unwrap();
    let options = WriterOptions::default();
    let mut source = File::open(&nup).unwrap();
    let (toc, _) = read_toc(&mut source, &limits()).unwrap();
    let payload: u64 = toc.files.iter().map(|f| f.size).sum();
    let stage = stage_nup_with_options(&nup, &dir, &nup_options(), &limits(), &options).unwrap();
    let staged = stage.staged_bytes;
    assert!(staged > payload, "native metadata was not charged");
    assert_eq!(
        stage
            .remaining_writer_options(&options)
            .unwrap()
            .max_temporary_disk_bytes,
        options.max_temporary_disk_bytes - staged
    );
    drop(stage);
    for (import_limit, disk_limit) in [
        (1, u64::MAX),
        (u64::MAX, 1),
        (u64::MAX, payload),
        (u64::MAX, staged + 256),
    ] {
        let mut limits = limits();
        limits.max_total_bytes = limits.max_total_bytes.min(import_limit);
        let options = WriterOptions {
            max_temporary_disk_bytes: disk_limit,
            ..options.clone()
        };
        let error = convert_nup(&nup, &out, &nup_options(), &limits, &options)
            .err()
            .unwrap();
        assert_eq!(error.kind(), WriterErrorKind::ResourceLimit, "{error}");
        assert_eq!(fs::read(&out).unwrap(), b"keep destination");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2, "stage left behind");
    }
    let token = crate::CancellationToken::default();
    token.cancel();
    let error = stage_nup(&nup, &dir, &nup_options(), &limits(), &token)
        .err()
        .unwrap();
    assert_eq!(error.kind(), WriterErrorKind::Cancelled);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn inherited_copy_fallback_charges_before_writing_and_observes_cancellation() {
    let dir = work("copy-budget");
    let from = dir.join("base");
    let out = dir.join("copy");
    fs::write(&from, b"0123456789abcdef").unwrap();
    let mut buffer = [0; 4];
    let mut budget = StageBudget { used: 0, limit: 9 };
    let error = copy_inherited(
        &from,
        &out,
        16,
        &mut buffer,
        &Default::default(),
        &mut budget,
    )
    .unwrap_err();
    assert_eq!(error.kind(), WriterErrorKind::ResourceLimit);
    assert_eq!(fs::metadata(&out).unwrap().len(), 8);
    assert_eq!(budget.used, 8);
    fs::remove_file(&out).unwrap();
    budget.limit = 24;
    copy_inherited(
        &from,
        &out,
        16,
        &mut buffer,
        &Default::default(),
        &mut budget,
    )
    .unwrap();
    assert_eq!(budget.used, 24);
    assert_eq!(fs::read(&out).unwrap(), fs::read(&from).unwrap());
    fs::remove_file(&out).unwrap();
    let token = crate::CancellationToken::default();
    token.cancel();
    let error = copy_inherited(&from, &out, 16, &mut buffer, &token, &mut budget).unwrap_err();
    assert_eq!(error.kind(), WriterErrorKind::Cancelled);
    assert_eq!(fs::metadata(&out).unwrap().len(), 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn skipped_dlc_does_not_consume_the_nup_staging_budget() {
    let dir = work("skip-budget");
    let nup = dir.join("game.nup");
    let out = dir.join("game.nx");
    let mut dlc = addon(APP);
    dlc.romfs = Some(vec![Node::File("large.bin", vec![0; 2 * 1024 * 1024])]);
    fs::write(&nup, build(&[base(APP), dlc], true)).unwrap();
    let options = WriterOptions {
        max_temporary_disk_bytes: 1024 * 1024,
        ..Default::default()
    };
    let nup_options = NupOptions {
        skip_addons: true,
        ..nup_options()
    };
    let stats = convert_nup(&nup, &out, &nup_options, &limits(), &options).unwrap();
    assert_eq!(stats.skipped_addons.len(), 1);
    assert!(stats.staged_bytes < options.max_temporary_disk_bytes);
    assert_eq!(
        stats.peak_temporary_disk_bytes,
        stats.staged_bytes + stats.conversion.peak_temporary_disk_bytes
    );
    assert!(stats.peak_temporary_disk_bytes <= options.max_temporary_disk_bytes);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stage_name_collision_never_removes_an_existing_directory() {
    let dir = work("stage-collision");
    let sentinel = dir.join("keep");
    fs::write(&sentinel, b"another operation").unwrap();
    assert!(StageDir::create(dir.clone()).is_err());
    assert_eq!(fs::read(&sentinel).unwrap(), b"another operation");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn oversized_npdm_is_rejected_before_staging_or_reading_it_whole() {
    let dir = work("npdm-limit");
    let nup = dir.join("game.nup");
    let mut game = base(APP);
    game.exefs = Some(vec![Node::File("main.npdm", vec![0; 1024 * 1024 + 1])]);
    fs::write(&nup, build(&[game], true)).unwrap();
    let error = stage_nup(&nup, &dir, &nup_options(), &limits(), &Default::default())
        .err()
        .unwrap();
    assert_eq!(error.kind(), WriterErrorKind::ResourceLimit);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    fs::remove_dir_all(dir).unwrap();
}

/// Writes a synthetic NUP to $NX_NUP_DUMP for CLI checks; no-op otherwise.
#[test]
fn dump_synthetic_nup_for_cli_checks() {
    if let Ok(path) = std::env::var("NX_NUP_DUMP") {
        fs::write(
            path,
            build(&[base(APP), update(APP, 65536, 0), addon(APP)], true),
        )
        .unwrap();
    }
}

#[test]
fn streamed_conversion_matches_the_staged_one_and_reads_the_base_once() {
    for zstd in [false, true] {
        let dir = work(if zstd { "stream-zstd" } else { "stream-raw" });
        let nup = dir.join("game.nup");
        fs::write(
            &nup,
            build(&[base(APP), update(APP, 65536, 0), addon(APP)], zstd),
        )
        .unwrap();
        let options = WriterOptions::default();
        let staged = dir.join("staged.nx");
        let streamed = dir.join("streamed.nx");
        let a = convert_nup(&nup, &staged, &nup_options(), &limits(), &options).unwrap();
        let progress = crate::ProgressCounter::default();
        let streamed_options = NupOptions {
            stream: true,
            progress: progress.clone(),
            ..nup_options()
        };
        let streamed_writer = WriterOptions {
            progress: progress.clone(),
            ..WriterOptions::default()
        };
        let b = convert_nup(
            &nup,
            &streamed,
            &streamed_options,
            &limits(),
            &streamed_writer,
        )
        .unwrap();
        // One write: no repack generation, and the stage holds ExeFS only.
        assert!(b.conversion.additions.is_empty());
        assert!(b.staged_bytes < a.staged_bytes);
        // Each NUP file is read once: the update's base files are not read
        // again (the staged path decodes each file once too).
        // Reading the base files again for the update would pass this.
        assert!(b.conversion.initial.input_bytes <= a.staged_bytes);
        // The staged update reads its shared base files a second time.
        assert!(a.conversion.initial.input_bytes > b.conversion.initial.input_bytes);
        // The bar ends exactly at its total.
        assert!(progress.total() > 0);
        assert_eq!(progress.done(), progress.total());
        let identities = |path: &Path| {
            let summary = inspect_archive(path, &options, IntegrityPolicy::Strict).unwrap();
            summary.applications[0]
                .snapshots
                .iter()
                .map(|s| {
                    (
                        s.snapshot_type,
                        s.numeric_version,
                        s.identity,
                        s.dependencies
                            .iter()
                            .map(|d| d.target_snapshot_identity)
                            .collect::<Vec<_>>(),
                        s.has_display_metadata,
                    )
                })
                .collect::<Vec<_>>()
        };
        let (x, y) = (identities(&staged), identities(&streamed));
        assert_eq!(x.len(), 3);
        assert_eq!(x, y);
        // Every NUP file is in each archive, byte for byte.
        for archive in [&staged, &streamed] {
            let check = check_nup(&nup, archive, false, &limits(), &options).unwrap();
            assert_eq!((check.snapshots, check.nacp_checked), (3, 2));
            assert!(check.files_read > 0 && check.files_read < check.files);
        }
        // Leaving the add-on out of the check: the archive holds more.
        let error = check_nup(&nup, &streamed, true, &limits(), &options).unwrap_err();
        assert!(error.to_string().contains("not in the NUP"), "{error}");
        // A NUP with other bytes does not match.
        let other = dir.join("other.nup");
        let mut bytes = build(&[base(APP), update(APP, 65536, 0), addon(APP)], zstd);
        if !zstd {
            let at = bytes
                .windows(16)
                .position(|w| w == b"base common data")
                .unwrap();
            bytes[at] ^= 1;
            fs::write(&other, &bytes).unwrap();
            let error = check_nup(&other, &streamed, false, &limits(), &options).unwrap_err();
            assert!(error.to_string().contains("SHA-256"), "{error}");
            fs::remove_file(&other).unwrap();
        }
        // Only the NUP and the two archives remain.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3);
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn streamed_conversion_rejects_a_damaged_romfs_file() {
    let dir = work("stream-damaged");
    let nup = dir.join("game.nup");
    let mut bytes = build(&[base(APP)], false);
    // Flip a byte of a RomFS payload: base common data.
    let at = bytes
        .windows(16)
        .position(|w| w == b"base common data")
        .unwrap();
    bytes[at] ^= 1;
    fs::write(&nup, bytes).unwrap();
    let out = dir.join("game.nx");
    let options = NupOptions {
        stream: true,
        ..nup_options()
    };
    let error = convert_nup(&nup, &out, &options, &limits(), &WriterOptions::default())
        .err()
        .expect("a damaged file must be refused");
    assert!(error.to_string().contains("SHA-256"), "{error}");
    assert!(!out.exists());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    let _ = fs::remove_dir_all(&dir);
}

fn meta_block(records: &[(u16, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (kind, body) in records {
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
    }
    out
}

/// Blocks written by upstream nup after its PR #20 carry every type one
/// too low; they must read like spec blocks (nup PR #32 rule).
#[test]
fn shifted_meta_types_read_like_spec_types() {
    let nacp = vec![0u8; 0x4000];
    let build_id = [0xaa; 32];
    let spec = [
        (0x0001, &b"npdm"[..]),
        (0x0002, &nacp[..]),
        (0x0004, &b"1.0.0"[..]),
        (0x0005, &b"Game"[..]),
        (0x0006, &b"Publisher"[..]),
        (0x0007, &build_id[..]),
    ];
    let check = |records: &[(u16, &[u8])]| {
        let meta = parse_meta(&meta_block(records), Meta::default()).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Game"));
        assert_eq!(meta.publisher.as_deref(), Some("Publisher"));
        assert_eq!(meta.display_version.as_deref(), Some("1.0.0"));
        assert!(meta.nacp.is_some());
    };
    check(&spec);
    // Shifted with an NPDM: type 0 is present.
    let shifted: Vec<_> = spec.iter().map(|&(k, b)| (k - 1, b)).collect();
    check(&shifted);
    // Shifted without an NPDM (DLC): 0x0007 holds the dump name, not 32 bytes.
    let mut no_npdm: Vec<_> = shifted[1..].to_vec();
    no_npdm.push((0x0007, b"game.nsp"));
    check(&no_npdm);
    // A spec block with a 32-byte build ID and no NPDM stays as is.
    check(&spec[1..]);
}
