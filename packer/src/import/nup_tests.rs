use super::super::structures::NpdmProfile;
use super::super::synthetic::npdm;
use super::super::synthetic_nup::*;
use super::*;
use crate::{inspect_archive, IntegrityPolicy, SnapshotType};

const APP: u64 = 0x0400_0000_0000_1000;
const LEGACY: u64 = 0x0100_4d30_0c5a_e000;
const OUNCE: u64 = 0x0400_4d30_0c5a_e001;
const XENOBLADE_LEGACY: u64 = 0x0100_ff50_0e34_a000;
const XENOBLADE_OUNCE: u64 = 0x0400_ff50_0e34_a000;

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
fn cross_application_errors_identify_package_and_owner() {
    let dir = work("cross-application-diagnostic");
    let nup = dir.join("unrelated.nup");
    let other = APP + 0x10000;
    for unrelated in [addon(other), update(other, 65536, 0)] {
        let actual = if unrelated.kind == 2 {
            unrelated.title_id
        } else {
            other | 0x800
        };
        let kind = if unrelated.kind == 2 {
            "addon"
        } else {
            "update"
        };
        fs::write(&nup, build(&[base(APP), unrelated], true)).unwrap();
        for message in [
            inspect_nup(&nup, &limits(), &Default::default())
                .err()
                .unwrap()
                .to_string(),
            stage_nup(&nup, &dir, &nup_options(), &limits(), &Default::default())
                .err()
                .unwrap()
                .to_string(),
        ] {
            assert!(message.contains("package #1"), "{message}");
            assert!(message.contains(kind), "{message}");
            assert!(message.contains(&format!("{actual:016x}")), "{message}");
            assert!(message.contains(&format!("{APP:016x}")), "{message}");
            assert!(message.contains(&format!("{other:016x}")), "{message}");
        }
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn nested_program_views_roundtrip_without_changing_nx_format() {
    let dir = work("multi-program");
    let nup = dir.join("game.nup");
    let out = dir.join("game.nx");
    let mut original = base(APP);
    original.extras.push(Node::Dir(
        "program1",
        vec![
            Node::Dir("exefs", exefs(0x51, APP | 1)),
            Node::Dir(
                "romfs",
                vec![Node::File("extra.bin", b"base extra".to_vec())],
            ),
        ],
    ));
    let mut patch = update(APP, 65536, 0);
    patch.extras.push(Node::Dir(
        "program1",
        vec![
            Node::Dir("exefs", exefs(0x52, APP | 1)),
            Node::Dir(
                "romfs",
                vec![Node::File("extra.bin", b"new extra".to_vec())],
            ),
        ],
    ));
    fs::write(&nup, build(&[original, patch], true)).unwrap();
    let options = WriterOptions::default();
    for stream in [true, false] {
        let mode = NupOptions {
            stream,
            ..nup_options()
        };
        convert_nup(&nup, &out, &mode, &limits(), &options).unwrap();
        let summary = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
        assert_eq!(summary.applications.len(), 1);
        assert_eq!(summary.applications[0].programs.len(), 2);
        assert_eq!(summary.applications[0].snapshots.len(), 4);
        let main_base = summary.applications[0]
            .snapshots
            .iter()
            .find(|snapshot| {
                snapshot.program_id == Some(APP) && snapshot.snapshot_type == SnapshotType::Base
            })
            .unwrap();
        let main_update = summary.applications[0]
            .snapshots
            .iter()
            .find(|snapshot| {
                snapshot.program_id == Some(APP) && snapshot.snapshot_type == SnapshotType::Update
            })
            .unwrap();
        let extra_base = summary.applications[0]
            .snapshots
            .iter()
            .find(|snapshot| {
                snapshot.program_id == Some(APP | 1) && snapshot.snapshot_type == SnapshotType::Base
            })
            .unwrap();
        let extra = summary.applications[0]
            .snapshots
            .iter()
            .find(|snapshot| {
                snapshot.program_id == Some(APP | 1)
                    && snapshot.snapshot_type == SnapshotType::Update
            })
            .unwrap();
        assert_eq!(
            main_update.dependencies[0].target_snapshot_identity,
            main_base.identity
        );
        assert_eq!(
            extra.dependencies[0].target_snapshot_identity,
            extra_base.identity
        );
        assert_ne!(
            extra.dependencies[0].target_snapshot_identity,
            main_base.identity
        );
        let mut bytes = [0u8; 9];
        crate::read_snapshot_file_range(
            &out,
            extra.identity,
            "romfs/extra.bin",
            0,
            &mut bytes,
            &options,
        )
        .unwrap();
        assert_eq!(&bytes, b"new extra");
        let verified = check_nup(&nup, &out, false, &limits(), &options).unwrap();
        assert_eq!(verified.snapshots, 4);
        fs::remove_file(&out).unwrap();
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn switch2_edition_keeps_legacy_application_and_both_programs() {
    let dir = work("switch2-edition");
    let nup = dir.join("edition.nup");
    let out = dir.join("edition.nx");
    let mut patch = update(OUNCE & !0xfff, 65536, 0);
    patch.exefs = Some(exefs(0x73, OUNCE));
    fs::write(&nup, build(&[base(LEGACY), patch, addon(LEGACY)], true)).unwrap();
    let options = WriterOptions::default();
    let info = inspect_nup(&nup, &limits(), &options.cancellation).unwrap();
    assert_eq!(info.application_id, LEGACY);
    assert_eq!(info.platform, crate::Platform::Switch2);
    convert_nup(&nup, &out, &nup_options(), &limits(), &options).unwrap();
    let archive = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(archive.applications.len(), 1);
    let app = &archive.applications[0];
    assert_eq!(app.application_id, LEGACY);
    assert!(app
        .programs
        .iter()
        .any(|p| p.program_id == LEGACY && p.kind == crate::ProgramKind::Auxiliary));
    assert!(app
        .programs
        .iter()
        .any(|p| p.program_id == OUNCE && p.kind == crate::ProgramKind::Primary));
    let base = app
        .snapshots
        .iter()
        .find(|s| s.snapshot_type == SnapshotType::Base)
        .unwrap();
    let upgrade = app
        .snapshots
        .iter()
        .find(|s| s.program_id == Some(OUNCE))
        .unwrap();
    assert_eq!(upgrade.snapshot_type, SnapshotType::StandaloneUpdate);
    assert!(upgrade
        .dependencies
        .iter()
        .any(|d| d.kind == crate::DependencyKind::Requires
            && d.target_snapshot_identity == base.identity));
    assert_eq!(
        check_nup(&nup, &out, false, &limits(), &options)
            .unwrap()
            .snapshots,
        3
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn switch2_edition_multiple_base_packages_match_real_nup_ids() {
    let dir = work("multi-base-edition");
    let nup = dir.join("edition.nup");
    let out = dir.join("edition.nx");
    // Observed in the Xenoblade Definitive Edition NUP: Program 1 is listed
    // before Program 0 and its 0100...B001 upgrade DLC is the third package.
    let mut p1 = base(XENOBLADE_OUNCE | 1);
    p1.romfs = Some(vec![Node::File("one.bin", b"program 1".to_vec())]);
    let mut p0 = base(XENOBLADE_OUNCE);
    p0.romfs = Some(vec![Node::File("zero.bin", b"program 0".to_vec())]);
    fs::write(&nup, build(&[p1, p0, addon(XENOBLADE_LEGACY)], true)).unwrap();
    let options = WriterOptions::default();
    let wrong_platform = NupOptions {
        platform: Some(crate::Platform::Switch1),
        ..nup_options()
    };
    assert!(convert_nup(&nup, &out, &wrong_platform, &limits(), &options).is_err());
    let info = inspect_nup(&nup, &limits(), &options.cancellation).unwrap();
    assert_eq!(info.application_id, XENOBLADE_LEGACY);
    assert_eq!(info.platform, crate::Platform::Switch2);
    convert_nup(&nup, &out, &nup_options(), &limits(), &options).unwrap();
    let archive = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(archive.applications.len(), 1);
    let app = &archive.applications[0];
    assert_eq!(app.application_id, XENOBLADE_LEGACY);
    assert_eq!(app.snapshots.len(), 3);
    assert!(app
        .programs
        .iter()
        .any(|p| p.program_id == (XENOBLADE_OUNCE | 1) && p.kind == crate::ProgramKind::Primary));
    assert!(app
        .programs
        .iter()
        .any(|p| p.program_id == XENOBLADE_OUNCE && p.kind == crate::ProgramKind::Auxiliary));
    for (id, present, absent) in [
        (XENOBLADE_OUNCE, "romfs/zero.bin", "romfs/one.bin"),
        (XENOBLADE_OUNCE | 1, "romfs/one.bin", "romfs/zero.bin"),
    ] {
        let snapshot = app
            .snapshots
            .iter()
            .find(|s| s.program_id == Some(id))
            .unwrap();
        let mut bytes = [0u8; 9];
        crate::read_snapshot_file_range(&out, snapshot.identity, present, 0, &mut bytes, &options)
            .unwrap();
        assert!(crate::read_snapshot_file_range(
            &out,
            snapshot.identity,
            absent,
            0,
            &mut bytes,
            &options
        )
        .is_err());
    }
    assert_eq!(
        check_nup(&nup, &out, false, &limits(), &options)
            .unwrap()
            .snapshots,
        3
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn switch2_multi_base_update_index_links_only_to_its_program() {
    let dir = work("multi-base-indexed-update");
    let nup = dir.join("edition.nup");
    let out = dir.join("edition.nx");
    let mut patch = update(XENOBLADE_OUNCE, 65536, 0);
    patch.title_id = XENOBLADE_OUNCE | 0x801;
    patch.exefs = Some(exefs(0x5a, XENOBLADE_OUNCE | 1));
    fs::write(
        &nup,
        build(
            &[
                base(XENOBLADE_OUNCE | 1),
                base(XENOBLADE_OUNCE),
                patch,
                addon(XENOBLADE_LEGACY),
            ],
            false,
        ),
    )
    .unwrap();
    let options = WriterOptions::default();
    convert_nup(&nup, &out, &nup_options(), &limits(), &options).unwrap();
    let archive = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
    let snapshots = &archive.applications[0].snapshots;
    let program1_base = snapshots
        .iter()
        .find(|s| {
            s.program_id == Some(XENOBLADE_OUNCE | 1) && s.snapshot_type == SnapshotType::Base
        })
        .unwrap();
    let program0_base = snapshots
        .iter()
        .find(|s| s.program_id == Some(XENOBLADE_OUNCE) && s.snapshot_type == SnapshotType::Base)
        .unwrap();
    let update = snapshots
        .iter()
        .find(|s| s.snapshot_type == SnapshotType::Update)
        .unwrap();
    assert_eq!(update.program_id, Some(XENOBLADE_OUNCE | 1));
    assert!(update.dependencies.iter().any(|d| {
        d.kind == crate::DependencyKind::RequiredBase
            && d.target_snapshot_identity == program1_base.identity
    }));
    assert!(!update
        .dependencies
        .iter()
        .any(|d| d.target_snapshot_identity == program0_base.identity));
    assert_eq!(
        check_nup(&nup, &out, false, &limits(), &options)
            .unwrap()
            .snapshots,
        4
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn indexed_romfs_only_update_inherits_the_correct_program_npdm() {
    let dir = work("indexed-romfs-only");
    let nup = dir.join("edition.nup");
    let out = dir.join("edition.nx");
    let mut update1 = update(XENOBLADE_OUNCE, 65536, 0);
    update1.title_id = XENOBLADE_OUNCE | 0x801;
    update1.exefs = None;
    fs::write(
        &nup,
        build(
            &[base(XENOBLADE_OUNCE), base(XENOBLADE_OUNCE | 1), update1],
            true,
        ),
    )
    .unwrap();
    let options = WriterOptions::default();
    convert_nup(&nup, &out, &nup_options(), &limits(), &options).unwrap();
    let summary = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
    let snapshots = &summary.applications[0].snapshots;
    let original = snapshots
        .iter()
        .find(|s| {
            s.snapshot_type == SnapshotType::Base && s.program_id == Some(XENOBLADE_OUNCE | 1)
        })
        .unwrap();
    let patched = snapshots
        .iter()
        .find(|s| s.snapshot_type == SnapshotType::Update)
        .unwrap();
    assert_eq!(patched.program_id, Some(XENOBLADE_OUNCE | 1));
    assert!(patched.dependencies.iter().any(|d| {
        d.kind == crate::DependencyKind::RequiredBase
            && d.target_snapshot_identity == original.identity
    }));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn switch2_multi_base_rejects_unrelated_and_unverified_packages() {
    let dir = work("multi-base-invalid");
    let nup = dir.join("bad.nup");
    let out = dir.join("bad.nx");
    let options = WriterOptions::default();
    let valid = || vec![base(XENOBLADE_OUNCE | 1), base(XENOBLADE_OUNCE)];
    let mut wrong_program = base(XENOBLADE_OUNCE | 1);
    wrong_program.exefs = Some(exefs(0x5b, XENOBLADE_OUNCE | 2));
    let mut wrong_update = update(XENOBLADE_OUNCE, 65536, 0);
    wrong_update.title_id = XENOBLADE_OUNCE | 0x801;
    wrong_update.exefs = Some(exefs(0x5c, XENOBLADE_OUNCE));
    let mut wrong_suffix = update(XENOBLADE_OUNCE, 65536, 0);
    wrong_suffix.title_id = XENOBLADE_OUNCE | 0x810;
    let mut cases = Vec::new();
    cases.push(vec![wrong_program, base(XENOBLADE_OUNCE)]);
    cases.push(vec![
        base(XENOBLADE_OUNCE | 1),
        base(XENOBLADE_OUNCE | 0x10),
    ]);
    let mut mismatched_update = valid();
    mismatched_update.push(wrong_update);
    cases.push(mismatched_update);
    let mut invalid_update = valid();
    invalid_update.push(wrong_suffix);
    cases.push(invalid_update);
    let mut missing_exefs = update(XENOBLADE_OUNCE, 65536, 0);
    missing_exefs.title_id = XENOBLADE_OUNCE | 0x801;
    missing_exefs.exefs = Some(vec![]);
    let mut missing_program = valid();
    missing_program.push(missing_exefs);
    cases.push(missing_program);
    let mut missing_upgrade = valid();
    missing_upgrade.push(addon(XENOBLADE_LEGACY + 2));
    cases.push(missing_upgrade);
    let mut unrelated_addon = valid();
    unrelated_addon.push(addon(0x0100_aaaa_0e34_a000));
    cases.push(unrelated_addon);
    for packages in cases {
        fs::write(&nup, build(&packages, false)).unwrap();
        assert!(convert_nup(&nup, &out, &nup_options(), &limits(), &options).is_err());
        assert!(!out.exists());
    }
    let mut wrong_single_base = update(XENOBLADE_OUNCE, 65536, 0);
    wrong_single_base.title_id = XENOBLADE_OUNCE | 0x801;
    fs::write(
        &nup,
        build(&[base(XENOBLADE_OUNCE), wrong_single_base], false),
    )
    .unwrap();
    let error = inspect_nup(&nup, &limits(), &options.cancellation)
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("no verified matching base Program"),
        "{error}"
    );
    assert!(convert_nup(&nup, &out, &nup_options(), &limits(), &options).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn real_producer_edition_layouts_preserve_legacy_and_ounce_programs() {
    let dir = work("producer-edition-layouts");
    let nup = dir.join("edition.nup");
    let out = dir.join("edition.nx");
    let options = WriterOptions::default();
    for mixed in [false, true] {
        let mut packages = vec![base(OUNCE)];
        if mixed {
            let mut legacy = base(OUNCE & !0xfff);
            legacy.exefs = Some(vec![
                Node::File("main", super::super::synthetic::nso(0x42, false)),
                Node::File("main.npdm", npdm(LEGACY, NpdmProfile::MetaAcid2048V1)),
            ]);
            packages.push(legacy);
        }
        for name in ["control", "logo", "legal", "meta"] {
            packages[0].extras.push(Node::Dir(
                name,
                vec![Node::File("preserved.bin", vec![42; 17])],
            ));
        }
        packages.push(addon(LEGACY));
        let mut bytes = build(&packages, true);
        if !mixed {
            // The producer left the RomFS child slots but zeroed its count.
            let romfs = u32_at(&bytes, SUPERBLOCK + 0x30 + 0x10) as usize;
            let dirs_at = SUPERBLOCK + 0x30 + packages.len() * PACKAGE;
            bytes[dirs_at + romfs * DIRECTORY + 12..dirs_at + romfs * DIRECTORY + 16].fill(0);
            let size = u64_at(&bytes, 0x10) as usize;
            let digest = crate::hash::sha256(&bytes[SUPERBLOCK..SUPERBLOCK + size]);
            bytes[0x18..0x38].copy_from_slice(&digest);
        }
        fs::write(&nup, bytes).unwrap();
        assert_eq!(
            inspect_nup(&nup, &limits(), &options.cancellation)
                .unwrap()
                .application_id,
            LEGACY
        );
        for stream in [false, true] {
            convert_nup(
                &nup,
                &out,
                &NupOptions {
                    stream,
                    ..nup_options()
                },
                &limits(),
                &options,
            )
            .unwrap();
            assert_eq!(
                check_nup(&nup, &out, false, &limits(), &options)
                    .unwrap()
                    .snapshots,
                packages.len()
            );
            let archive = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
            assert!(archive.applications[0]
                .programs
                .iter()
                .any(|p| p.program_id == OUNCE && p.kind == crate::ProgramKind::Primary));
            if mixed {
                assert!(archive.applications[0]
                    .programs
                    .iter()
                    .any(|p| p.program_id == LEGACY));
            }
            fs::remove_file(&out).unwrap();
        }
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn indexed_updates_require_the_exact_declared_program() {
    let dir = work("indexed-update-program-proof");
    let nup = dir.join("indexed.nup");
    let out = dir.join("indexed.nx");
    let options = WriterOptions::default();
    for standalone in [false, true] {
        for with_addon in [false, true] {
            for observed in [None, Some(OUNCE), Some(OUNCE + 1)] {
                let mut patch = update(OUNCE & !0xfff, 65536, u8::from(standalone));
                patch.title_id = (OUNCE & !0xfff) | 0x801;
                patch.exefs = observed.map(|id| exefs(0x78, id));
                let mut packages = vec![patch];
                if with_addon {
                    packages.push(addon(LEGACY));
                }
                if !standalone {
                    packages.push(base(LEGACY));
                }
                fs::write(&nup, build(&packages, true)).unwrap();
                if observed == Some(OUNCE) {
                    assert!(inspect_nup(&nup, &limits(), &options.cancellation).is_ok());
                    convert_nup(&nup, &out, &nup_options(), &limits(), &options).unwrap();
                    check_nup(&nup, &out, false, &limits(), &options).unwrap();
                    fs::remove_file(&out).unwrap();
                } else {
                    assert!(
                        inspect_nup(&nup, &limits(), &options.cancellation).is_err(),
                        "indexed update accepted: standalone={standalone}, NPDM={observed:?}"
                    );
                    assert!(convert_nup(&nup, &out, &nup_options(), &limits(), &options).is_err());
                    assert!(!out.exists());
                }
            }
        }
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unknown_nup_package_root_reports_exact_entry() {
    let dir = work("unknown-root-diagnostic");
    let nup = dir.join("bad.nup");
    let out = dir.join("bad.nx");
    let mut pkg = base(APP);
    pkg.extras
        .push(Node::Dir("unexpected_switch2_tree", vec![]));
    fs::write(&nup, build(&[pkg], false)).unwrap();
    let error = convert_nup(
        &nup,
        &out,
        &nup_options(),
        &limits(),
        &WriterOptions::default(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("unexpected_switch2_tree"), "{error}");
    assert!(error.contains("package #0"), "{error}");
    assert!(!out.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn switch2_edition_standalone_update_with_matching_upgrade_dlc() {
    let dir = work("switch2-standalone-edition");
    let nup = dir.join("edition.nup");
    let out = dir.join("edition.nx");
    let mut patch = update(OUNCE & !0xfff, 65536, 1);
    patch.exefs = Some(exefs(0x74, OUNCE));
    fs::write(&nup, build(&[patch, addon(LEGACY)], true)).unwrap();
    let options = WriterOptions::default();
    convert_nup(&nup, &out, &nup_options(), &limits(), &options).unwrap();
    let archive = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(archive.applications.len(), 1);
    assert_eq!(archive.applications[0].application_id, LEGACY);
    assert_eq!(archive.applications[0].programs[0].program_id, OUNCE);
    assert_eq!(
        check_nup(&nup, &out, false, &limits(), &options)
            .unwrap()
            .snapshots,
        2
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn switch2_edition_inherits_latest_legacy_update_content() {
    let dir = work("edition-with-legacy-patch");
    let nup = dir.join("edition.nup");
    let out = dir.join("edition.nx");
    let mut patch = update(OUNCE & !0xfff, 65536, 0);
    patch.exefs = Some(exefs(0x76, OUNCE));
    patch.romfs = Some(vec![Node::File("switch2.bin", b"new content".to_vec())]);
    fs::write(
        &nup,
        build(
            &[base(LEGACY), update(LEGACY, 65536, 0), patch, addon(LEGACY)],
            true,
        ),
    )
    .unwrap();
    let options = WriterOptions::default();
    convert_nup(&nup, &out, &nup_options(), &limits(), &options).unwrap();
    let archive = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
    let edition = archive.applications[0]
        .snapshots
        .iter()
        .find(|s| s.program_id == Some(OUNCE))
        .unwrap();
    let legacy_patch = archive.applications[0]
        .snapshots
        .iter()
        .find(|s| s.program_id == Some(LEGACY) && s.snapshot_type == SnapshotType::Update)
        .unwrap();
    assert!(edition
        .dependencies
        .iter()
        .any(|d| d.kind == crate::DependencyKind::Requires
            && d.target_snapshot_identity == legacy_patch.identity));
    let mut bytes = [0u8; 14];
    crate::read_snapshot_file_range(
        &out,
        edition.identity,
        "romfs/shadowed.bin",
        0,
        &mut bytes,
        &options,
    )
    .unwrap();
    assert_eq!(&bytes, b"update version");
    assert_eq!(
        check_nup(&nup, &out, false, &limits(), &options)
            .unwrap()
            .snapshots,
        4
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn legacy_and_ounce_updates_may_share_nup_title_and_version() {
    let dir = work("edition-same-title-version");
    let nup = dir.join("edition.nup");
    let out = dir.join("edition.nx");
    let mut patch = update(LEGACY, 65536, 0);
    patch.exefs = Some(exefs(0x77, OUNCE));
    patch.romfs = Some(vec![Node::File("edition.bin", b"edition".to_vec())]);
    fs::write(
        &nup,
        build(
            &[base(LEGACY), update(LEGACY, 65536, 0), patch, addon(LEGACY)],
            false,
        ),
    )
    .unwrap();
    let options = WriterOptions::default();
    convert_nup(&nup, &out, &nup_options(), &limits(), &options).unwrap();
    let summary = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(summary.applications[0].programs.len(), 2);
    assert_eq!(
        check_nup(&nup, &out, false, &limits(), &options)
            .unwrap()
            .snapshots,
        4
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn switch2_upgrade_rejects_unrelated_native_program() {
    let dir = work("unrelated-edition");
    let nup = dir.join("unrelated.nup");
    let out = dir.join("unrelated.nx");
    let mut patch = update(OUNCE & !0xfff, 65536, 0);
    patch.exefs = Some(exefs(0x75, 0x0400_0000_1234_5001));
    fs::write(&nup, build(&[base(LEGACY), patch], false)).unwrap();
    assert!(convert_nup(
        &nup,
        &out,
        &nup_options(),
        &limits(),
        &WriterOptions::default()
    )
    .is_err());
    assert!(!out.exists());
    fs::remove_dir_all(dir).unwrap();
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

#[test]
fn nested_program_exefs_replaces_only_its_own_modules() {
    let dir = work("nested-exefs-replacement");
    let nup = dir.join("game.nup");
    let out = dir.join("game.nx");
    let mut original = base(APP);
    let mut modules = exefs(0x51, APP | 1);
    modules.push(Node::File(
        "subsdk0",
        super::super::synthetic::nso(0x61, false),
    ));
    original.extras.push(Node::Dir(
        "program1",
        vec![
            Node::Dir("exefs", modules),
            Node::Dir("data", vec![Node::File("same.bin", b"aux data".to_vec())]),
        ],
    ));
    original.data = Some(vec![Node::File("same.bin", b"main data".to_vec())]);
    let mut patch = update(APP, 65536, 0);
    patch.extras.push(Node::Dir(
        "program1",
        vec![Node::Dir("exefs", exefs(0x52, APP | 1))],
    ));
    fs::write(&nup, build(&[original, patch], true)).unwrap();
    for stream in [false, true] {
        let options = WriterOptions::default();
        convert_nup(
            &nup,
            &out,
            &NupOptions {
                stream,
                ..nup_options()
            },
            &limits(),
            &options,
        )
        .unwrap();
        let summary = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
        for snapshot in &summary.applications[0].snapshots {
            let files = crate::inspect_snapshot_files(
                &out,
                snapshot.identity,
                &options,
                IntegrityPolicy::Strict,
            )
            .unwrap();
            let removed = snapshot.program_id == Some(APP | 1)
                && snapshot.snapshot_type == SnapshotType::Base;
            assert_eq!(files.iter().any(|f| f.path == "exefs/subsdk0"), removed);
            let expected: &[u8] = if snapshot.program_id == Some(APP) {
                b"main data"
            } else {
                b"aux data"
            };
            let mut data = vec![0; expected.len()];
            crate::read_snapshot_file_range(
                &out,
                snapshot.identity,
                "data/same.bin",
                0,
                &mut data,
                &options,
            )
            .unwrap();
            assert_eq!(data, expected);
        }
        check_nup(&nup, &out, false, &limits(), &options).unwrap();
        fs::remove_file(&out).unwrap();
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unrelated_npdm_and_invalid_nested_layouts_never_publish() {
    let dir = work("invalid-program-layouts");
    let nup = dir.join("game.nup");
    let out = dir.join("game.nx");
    let mut cases = Vec::new();
    let mut unrelated = base(APP);
    unrelated.exefs = Some(exefs(0x41, APP + 0x2000));
    cases.push(vec![unrelated]);
    let mut unrelated_update = update(APP, 65536, 0);
    unrelated_update.exefs = Some(exefs(0x42, APP + 0x2000));
    cases.push(vec![base(APP), unrelated_update]);
    cases.push(vec![update(APP, 65536, 1), update(APP + 0x2000, 131072, 1)]);
    cases.push(vec![update(APP, 65536, 1), addon(APP + 0x2000)]);
    for name in [
        "program0",
        "program16",
        "program01",
        "program255",
        "program999999999999999999999",
    ] {
        let mut game = base(APP);
        game.extras.push(Node::Dir(
            name,
            vec![Node::Dir("exefs", exefs(0x51, APP | 1))],
        ));
        cases.push(vec![game]);
    }
    for id in [APP, APP | 2, APP + 0x2001] {
        let mut game = base(APP);
        game.extras.push(Node::Dir(
            "program1",
            vec![Node::Dir("exefs", exefs(0x51, id))],
        ));
        cases.push(vec![game]);
    }
    let mut missing = base(APP);
    missing
        .extras
        .push(Node::Dir("program1", vec![Node::Dir("romfs", vec![])]));
    cases.push(vec![missing]);
    for packages in cases {
        fs::write(&nup, build(&packages, true)).unwrap();
        for stream in [false, true] {
            assert!(convert_nup(
                &nup,
                &out,
                &NupOptions {
                    stream,
                    ..nup_options()
                },
                &limits(),
                &WriterOptions::default()
            )
            .is_err());
            assert!(!out.exists());
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        }
    }
    fs::remove_dir_all(dir).unwrap();
}
