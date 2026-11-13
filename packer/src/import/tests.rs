use super::structures::*;
use super::*;
use crate::{CancellationToken, IntegrityPolicy, RepackPlan, WriterOptions};
use std::{
    fs,
    io::Cursor,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

#[test]
fn native_observation_without_program_declaration_and_unknown_build_stay_distinct() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let mut main = synthetic::nso(0x41, true);
    main[0x40..0x60].fill(0);
    fs::write(source.join("base/exefs/main"), main).unwrap();
    let m = manifest(&base().replace("\t0000000000005678\t", "\t-\t"));
    let v = inspect_tree(
        &source,
        &m,
        "b",
        &ImportLimits::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert_eq!(v.scope.program_id.value, Some(0x5678));
    assert!(matches!(
        v.scope.program_id.origin,
        Origin::NativeStructure { .. }
    ));
    assert_eq!(v.scope.build_id.value, None);
    assert_eq!(v.scope.build_id.origin, Origin::Unknown);
    assert!(matches!(
        v.scope.platform.origin,
        Origin::UserDeclaration { .. }
    ));
    assert!(matches!(
        v.scope.application_id.origin,
        Origin::UserDeclaration { .. }
    ));
}

#[test]
fn tree_namespace_and_same_size_payload_changes_are_detected() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let token = CancellationToken::default();
    let v = inspect_tree(
        &source,
        &manifest(&base()),
        "b",
        &ImportLimits::default(),
        &token,
    )
    .unwrap();
    fs::write(source.join("base/romfs/new.bin"), b"new").unwrap();
    assert!(v.watch.check(&ImportLimits::default(), &token).is_err());
    fs::remove_file(source.join("base/romfs/new.bin")).unwrap();
    v.watch.check(&ImportLimits::default(), &token).unwrap();
    let path = source.join("base/romfs/data.bin");
    let mut data = fs::read(&path).unwrap();
    data[0] ^= 1;
    // Same size, new content: caught from the listing's write time, without
    // re-reading or re-hashing the file. Sleep past coarse timestamp ticks.
    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(&path, data).unwrap();
    assert!(v.watch.check(&ImportLimits::default(), &token).is_err());
    // ExeFS keeps inventory-time digests: its structure was validated then.
    let npdm = source.join("base/exefs/main.npdm");
    let mut bytes = fs::read(&npdm).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(&npdm, bytes).unwrap();
    let mut reader = v.files["exefs/main.npdm"].open_reader(&token).unwrap();
    assert!(std::io::copy(&mut reader, &mut std::io::sink()).is_err());
}

fn paranoid() -> ImportLimits {
    ImportLimits {
        compare_source_bytes: true,
        ..ImportLimits::default()
    }
}

#[test]
fn program_view_requires_matching_version_and_survives_latest_selection() {
    let declarations = concat!(
        "NXIMPORT\t1\n",
        "snapshot\tb\tbase\tswitch2\t0400000000001000\t0400000000001000\tprimary\tbase\t0\tcomplete\tmaterialized-v1\tmeta-acid3072-v1\n",
        "program-view\tb\tprogram1\n",
        "snapshot\tb1\tbase/program1\tswitch2\t0400000000001000\t0400000000001001\tauxiliary\tbase\t0\tcomplete\tmaterialized-v1\tmeta-acid3072-v1\n",
        "snapshot\tu\tupdates/2\tswitch2\t0400000000001000\t0400000000001000\tprimary\tupdate\t2\tcomplete\tmaterialized-v1\tmeta-acid3072-v1\n",
        "dependency\tu\trequired-base\tb\n",
        "program-view\tu\tprogram1\n",
        "snapshot\tu1\tupdates/2/program1\tswitch2\t0400000000001000\t0400000000001001\tauxiliary\tstandalone-update\t2\tcomplete\tmaterialized-v1\tmeta-acid3072-v1\n",
    );
    let limits = ImportLimits::default();
    let parsed = DeclarationManifest::read(declarations.as_bytes(), &limits).unwrap();
    let selected = parsed
        .select_latest_complete(&Default::default(), &limits)
        .unwrap();
    assert_eq!(selected.manifest.snapshots.len(), 4);
    assert!(selected.manifest.snapshots.iter().any(|s| s.key == "b1"));

    let invalid_version = declarations.replace(
        "auxiliary\tbase\t0\tcomplete",
        "auxiliary\tbase\t1\tcomplete",
    );
    assert!(DeclarationManifest::read(invalid_version.as_bytes(), &limits).is_err());
}

#[test]
fn cycles_scope_duplicates_build_conflicts_and_publication_failure_are_transactional() {
    let t = Temp::new();
    let source = t.0.join("source");
    let text = synthetic::package(&source, 0).unwrap();
    let out = t.0.join("out.nx");
    fs::write(&out, b"original").unwrap();
    for bad in [
        text.replace(
            "dependency\tu\trequired-base\tb",
            "dependency\tu\trequired-base\tv",
        )
        .replace(
            "dependency\tv\trequired-base\tb",
            "dependency\tv\trequired-base\tu",
        ),
        format!("{text}build\tu\t{}\n", "ff".repeat(32)),
        text.replace("\tupdate\t20\t", "\tupdate\t10\t"),
    ] {
        assert!(convert_extracted(
            &source,
            &manifest(&bad),
            &out,
            &ImportLimits::default(),
            &WriterOptions::default()
        )
        .is_err());
        assert_eq!(fs::read(&out).unwrap(), b"original");
    }
    let directory = t.0.join("occupied");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("keep"), b"safe").unwrap();
    assert!(convert_extracted(
        &source,
        &manifest(&text),
        &directory,
        &ImportLimits::default(),
        &WriterOptions::default()
    )
    .is_err());
    assert_eq!(fs::read(directory.join("keep")).unwrap(), b"safe");
    assert!(!fs::read_dir(&t.0).unwrap().any(|f| f
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".nx-")));
}

#[test]
fn replacement_composition_and_multiple_programs_use_existing_snapshot_model() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let opts = WriterOptions::default();
    let limits = ImportLimits::default();
    let first = convert_extracted(
        &source,
        &manifest(&base()),
        &t.0.join("base.nx"),
        &limits,
        &opts,
    )
    .unwrap();
    let id = first.receipt.snapshots["b"].identity;
    fs::create_dir_all(source.join("updates/partial/exefs")).unwrap();
    fs::write(
        source.join("updates/partial/exefs/main"),
        synthetic::nso(0x42, true),
    )
    .unwrap();
    synthetic::executable_tree(&source.join("auxiliary"), 0x5679, 0x50).unwrap();
    let text = format!(
        "{}{}compose\tp\treplace-delete-v1\tb\t{}\n{}",
        base(),
        synthetic::row("p", "updates/partial", "update", 10, false, 0x5678),
        hex(&id),
        synthetic::row("a", "auxiliary", "base", 0, true, 0x5679)
            .replace("\tprimary\t", "\tauxiliary\t")
    );
    let result = convert_extracted(
        &source,
        &manifest(&text),
        &t.0.join("out.nx"),
        &limits,
        &opts,
    )
    .unwrap();
    assert_eq!(result.receipt.snapshots["p"].build.value, Some([0x42; 32]));
    assert!(matches!(
        result.receipt.snapshots["p"].build.origin,
        Origin::NativeStructure { .. }
    ));
    let archive =
        crate::inspect_archive(t.0.join("out.nx"), &opts, IntegrityPolicy::Strict).unwrap();
    assert_eq!(archive.applications[0].programs.len(), 2);
    assert_eq!(result.receipt.snapshots["a"].program.value, Some(0x5679));
}

#[test]
fn declared_presentation_changes_do_not_change_nx_identity_or_native_display_selection() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let opts = WriterOptions::default();
    let a = convert_extracted(
        &source,
        &manifest(&base()),
        &t.0.join("a.nx"),
        &ImportLimits::default(),
        &opts,
    )
    .unwrap();
    let b=convert_extracted(&source,&manifest(&(base()+"title\tb\tUser title\npublisher\tb\tUser publisher\ndisplay-version\tb\tUser version\n")),&t.0.join("b.nx"),&ImportLimits::default(),&opts).unwrap();
    assert_eq!(
        a.receipt.snapshots["b"].identity,
        b.receipt.snapshots["b"].identity
    );
    assert_eq!(
        fs::read(t.0.join("a.nx")).unwrap(),
        fs::read(t.0.join("b.nx")).unwrap()
    );
    let mut corrupt = b.receipt.encode(&ImportLimits::default()).unwrap();
    corrupt[13] = b'2';
    assert!(ImportReceipt::read(corrupt.as_slice(), &ImportLimits::default()).is_err());
}
static NEXT: AtomicU64 = AtomicU64::new(0);

#[test]
fn deletion_only_partial_view_is_resolved_only_with_exact_base_and_tombstones() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    fs::create_dir_all(source.join("updates/delete-only")).unwrap();
    let opts = WriterOptions::default();
    let limits = ImportLimits::default();
    let b = convert_extracted(
        &source,
        &manifest(&base()),
        &t.0.join("b.nx"),
        &limits,
        &opts,
    )
    .unwrap();
    let text = format!(
        "{}{}compose\tp\treplace-delete-v1\tb\t{}\nremove\tp\tromfs/delete.me\n",
        base(),
        synthetic::row("p", "updates/delete-only", "update", 10, false, 0x5678),
        hex(&b.receipt.snapshots["b"].identity)
    );
    let result = convert_extracted(
        &source,
        &manifest(&text),
        &t.0.join("out.nx"),
        &limits,
        &opts,
    )
    .unwrap();
    let p = &result.receipt.snapshots["p"];
    assert_eq!(p.build.value, Some([0x41; 32]));
    let files = crate::inspect_snapshot_files(
        t.0.join("out.nx"),
        p.identity,
        &opts,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert!(!files.iter().any(|f| f.path == "romfs/delete.me"));
    assert!(files.iter().any(|f| f.path == "exefs/main"));
}

#[test]
fn additional_program_layouts_and_executable_exclusions_are_not_silently_accepted() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let limits = ImportLimits::default();
    let token = CancellationToken::default();
    assert!(
        DeclarationManifest::read((base() + "exclude\tb\texefs/sdk\n").as_bytes(), &limits)
            .is_err()
    );
    fs::write(
        source.join("base/exefs/other.npdm"),
        synthetic::npdm(0x9999, NpdmProfile::MetaAcid3072V1),
    )
    .unwrap();
    assert!(inspect_tree(&source, &manifest(&base()), "b", &limits, &token).is_err());
    fs::remove_file(source.join("base/exefs/other.npdm")).unwrap();
    fs::create_dir_all(source.join("base/exefs/other_program")).unwrap();
    assert!(inspect_tree(&source, &manifest(&base()), "b", &limits, &token).is_err());
}

#[test]
fn latest_complete_export_reports_history_and_keeps_exact_dependency_closure() {
    let t = Temp::new();
    let source = t.0.join("source");
    let m = manifest(&synthetic::package(&source, 0).unwrap());
    let selected = m
        .select_latest_complete(
            &std::collections::BTreeSet::from(["d".to_owned()]),
            &ImportLimits::default(),
        )
        .unwrap();
    assert_eq!(selected.omitted_snapshot_keys, vec!["u"]);
    assert_eq!(selected.retained_dependency_keys, vec!["b"]);
    let result = convert_extracted(
        &source,
        &selected.manifest,
        &t.0.join("latest.nx"),
        &ImportLimits::default(),
        &WriterOptions::default(),
    )
    .unwrap();
    assert_eq!(result.receipt.snapshots.len(), 3);
    assert!(result.receipt.snapshots.contains_key("v"));
    assert!(!result.receipt.snapshots.contains_key("u"));
}

#[test]
fn common_native_adapter_preserves_the_existing_writer_mapping_and_extent_probes() {
    use crate::native::synthetic::{dnsp, title_entries, TitleSpec};
    let t = Temp::new();
    let path = t.0.join("base.dnsp");
    fs::write(
        &path,
        dnsp(&title_entries(&TitleSpec::application(
            0x0100000000010000,
            0,
            0x30,
        ))),
    )
    .unwrap();
    let plan = crate::native::inventory(
        std::slice::from_ref(&path),
        &crate::native::InventoryOptions::default(),
    )
    .unwrap()
    .plan();
    let app = &plan.applications[0];
    let snapshot = &app.snapshots[0];
    let common = CompleteContent::NativeNca(snapshot);
    assert_eq!(common.layout(), ContentLayout::NcaUnits);
    assert_eq!(common.source_kind(), SourceKind::NativeNca);
    let input = || crate::ApplicationInput {
        platform: app.platform,
        application_id: app.application_id,
        programs: app
            .programs
            .iter()
            .map(|(program_id, kind)| crate::ProgramInput {
                program_id: *program_id,
                kind: *kind,
                content_id: None,
                npdm: None,
            })
            .collect(),
        snapshots: vec![common.to_snapshot_input(None).unwrap()],
    };
    crate::write_archive(
        crate::ArchiveInput::new(vec![input()]),
        t.0.join("common.nx"),
        &WriterOptions::default(),
    )
    .unwrap();
    let direct = app.to_application_input(&|_| true, &|_| None).unwrap();
    crate::write_archive(
        crate::ArchiveInput::new(vec![direct]),
        t.0.join("direct.nx"),
        &WriterOptions::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read(t.0.join("common.nx")).unwrap(),
        fs::read(t.0.join("direct.nx")).unwrap()
    );
    let files = common.files();
    let f = &files[0];
    let mut first = [0; 4];
    f.read_exact_at(0x200, &mut first).unwrap();
    assert_eq!(&first, b"DNCA");
    assert!(f.read_exact_at(f.size(), &mut first).is_err());
}

#[test]
fn deep_paths_use_bounded_scratch_and_unsupported_depth_is_rejected() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let mut deep = source.join("base/romfs");
    for _ in 0..60 {
        deep = deep.join("a");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("f"), b"deep").unwrap();
    let v = inspect_tree(
        &source,
        &manifest(&base()),
        "b",
        &ImportLimits::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(matches!(v.completeness(), Completeness::DeclaredComplete));
    CompleteContent::ExtractedTree(&v)
        .to_snapshot_input(None)
        .unwrap();
    for _ in 0..5 {
        deep = deep.join("a");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("f"), b"too deep").unwrap();
    assert!(inspect_tree(
        &source,
        &manifest(&base()),
        "b",
        &ImportLimits::default(),
        &CancellationToken::default()
    )
    .is_err());
}

#[test]
fn bounded_header_mutations_and_lz4_terminal_unused_bits_do_not_panic() {
    let c = CancellationToken::default();
    let limits = ImportLimits {
        max_segment_bytes: 1024 * 1024,
        max_executable_bytes: 1024 * 1024,
        ..Default::default()
    };
    let good = synthetic::nso(0x41, true);
    // The last token's low nibble is explicitly ignored by the LZ4 spec.
    let mut unused = good.clone();
    let ro = u32::from_le_bytes(unused[0x20..0x24].try_into().unwrap()) as usize;
    unused[ro - 9] |= 15;
    assert!(validate_nso(&mut Cursor::new(&unused), unused.len() as u64, &limits, &c).is_ok());
    for i in 0..512 {
        let mut b = good.clone();
        let at = (i * 37) % b.len();
        b[at] ^= 1 << (i % 8);
        assert!(std::panic::catch_unwind(|| {
            let _ = validate_nso(&mut Cursor::new(&b), b.len() as u64, &limits, &c);
        })
        .is_ok());
    }
    let good = synthetic::npdm(0x5678, NpdmProfile::MetaAcid3072V1);
    for i in 0..512 {
        let mut b = good.clone();
        let at = (i * 37) % b.len();
        b[at] ^= 1 << (i % 8);
        assert!(std::panic::catch_unwind(|| {
            let _ = validate_npdm(&b, NpdmProfile::MetaAcid3072V1);
        })
        .is_ok());
    }
}

#[test]
fn file_larger_than_combined_memory_budget_is_streamed_and_fully_compared() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let large = fs::File::create(source.join("base/romfs/large.bin")).unwrap();
    large.set_len(128 * 1024 * 1024).unwrap();
    drop(large);
    let options = WriterOptions {
        max_memory_bytes: 96 * 1024 * 1024,
        max_temporary_disk_bytes: 16 * 1024 * 1024,
        ..Default::default()
    };
    let result = convert_extracted(
        &source,
        &manifest(&base()),
        &t.0.join("out.nx"),
        &paranoid(),
        &options,
    )
    .unwrap();
    assert!(result.source_bytes_compared > options.max_memory_bytes);
    assert!(result.initial.accounted_memory_bytes < options.max_memory_bytes);
    assert!(result.peak_temporary_disk_bytes <= options.max_temporary_disk_bytes);
    assert!(result.initial.deduplicated_blocks > 100);
}

#[test]
fn in_flight_inventory_cancellation_cleans_temporaries_and_keeps_destination() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    fs::File::create(source.join("base/romfs/large.bin"))
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    let out = t.0.join("out.nx");
    fs::write(&out, b"keep").unwrap();
    let options = WriterOptions::default();
    let token = options.cancellation.clone();
    let parent = t.0.clone();
    let cancel = std::thread::spawn(move || {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if fs::read_dir(&parent).unwrap().any(|f| {
                f.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".nx-tree-")
            }) {
                token.cancel();
                return;
            }
            assert!(
                std::time::Instant::now() < end,
                "conversion stage never appeared"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });
    let result = convert_extracted(
        &source,
        &manifest(&base()),
        &out,
        &ImportLimits::default(),
        &options,
    );
    cancel.join().unwrap();
    assert_eq!(result.err().unwrap().kind(), WriterErrorKind::Cancelled);
    assert_eq!(fs::read(&out).unwrap(), b"keep");
    assert!(!fs::read_dir(&t.0).unwrap().any(|f| f
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".nx-")));
}
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "nx-extracted-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn manifest(s: &str) -> DeclarationManifest {
    DeclarationManifest::read(s.as_bytes(), &ImportLimits::default()).unwrap()
}
fn base() -> String {
    format!(
        "NXIMPORT\t1\n{}",
        synthetic::row("b", "base", "base", 0, true, 0x5678)
    )
}
fn put(b: &mut [u8], at: usize, n: u32) {
    b[at..at + 4].copy_from_slice(&n.to_le_bytes());
}

#[test]
fn explicit_npdm_profiles_ids_ranges_and_unknown_fields() {
    for p in [NpdmProfile::MetaAcid2048V1, NpdmProfile::MetaAcid3072V1] {
        let good = synthetic::npdm(0x5678, p);
        assert_eq!(validate_npdm(&good, p).unwrap().program_id, 0x5678);
        let mut b = good.clone();
        put(&mut b, 0x70, u32::MAX);
        assert!(validate_npdm(&b, p).is_err());
        let mut b = good.clone();
        b[0xd] = 1;
        assert!(validate_npdm(&b, p).is_err());
        let mut b = good.clone();
        let aci = u32::from_le_bytes(b[0x70..0x74].try_into().unwrap()) as usize;
        b[aci + 0x10] ^= 1;
        assert!(validate_npdm(&b, p).is_err());
        let mut b = good.clone();
        put(&mut b, 0x10, 192 * 1024 * 1024);
        assert_eq!(
            validate_npdm(&b, p).unwrap().remote_memory_size,
            192 * 1024 * 1024
        );
        assert!(validate_npdm(
            &good,
            if p == NpdmProfile::MetaAcid2048V1 {
                NpdmProfile::MetaAcid3072V1
            } else {
                NpdmProfile::MetaAcid2048V1
            }
        )
        .is_err());
    }
}
#[test]
fn streamed_nso_raw_lz4_hashes_overlap_unknown_codec_and_missing_build() {
    let l = ImportLimits::default();
    let c = CancellationToken::default();
    for compressed in [false, true] {
        let good = synthetic::nso(0x41, compressed);
        let f = validate_nso(&mut Cursor::new(&good), good.len() as u64, &l, &c).unwrap();
        assert_eq!(f.build_id, Some([0x41; 32]));
        assert_eq!(f.segment_hashes_checked, 3);
        let mut bad = good.clone();
        bad[0xa0] ^= 1;
        assert!(validate_nso(&mut Cursor::new(&bad), bad.len() as u64, &l, &c).is_err());
        let mut bad = good.clone();
        put(&mut bad, 0x20, 0x100);
        assert!(validate_nso(&mut Cursor::new(&bad), bad.len() as u64, &l, &c).is_err());
        let mut bad = good.clone();
        bad[0xc] |= 0x80;
        assert!(validate_nso(&mut Cursor::new(&bad), bad.len() as u64, &l, &c).is_err());
        let mut no_id = good.clone();
        no_id[0x40..0x60].fill(0);
        assert_eq!(
            validate_nso(&mut Cursor::new(&no_id), no_id.len() as u64, &l, &c)
                .unwrap()
                .build_id,
            None
        );
        for size in [0, 12, 255, good.len() - 1] {
            assert!(validate_nso(&mut Cursor::new(&good[..size]), size as u64, &l, &c).is_err());
        }
    }
}
#[test]
fn zbic_nso_is_kept_without_decoding() {
    let l = ImportLimits::default();
    let c = CancellationToken::default();
    let good = synthetic::nso_zbic(0x42);
    let f = validate_nso(&mut Cursor::new(&good), good.len() as u64, &l, &c).unwrap();
    assert_eq!(f.build_id, Some([0x42; 32]));
    assert_eq!(f.zbic_segments, 3);
    assert_eq!(f.segment_hashes_checked, 0);
    assert_eq!(f.decoded_bytes, 131072 + 64 + 32);
    // A ZBIC segment must start with the ZBIC magic.
    let at = u32::from_le_bytes(good[0x20..0x24].try_into().unwrap()) as usize;
    let mut bad = good.clone();
    bad[at] = b'X';
    assert!(validate_nso(&mut Cursor::new(&bad), bad.len() as u64, &l, &c).is_err());
    // The ZBIC flag needs at least one compressed segment.
    let mut bad = good.clone();
    put(&mut bad, 0xc, 0xf8);
    assert!(validate_nso(&mut Cursor::new(&bad), bad.len() as u64, &l, &c).is_err());
    // Flags above bit 7 stay unknown.
    let mut bad = good.clone();
    put(&mut bad, 0xc, 0x1ff);
    assert!(validate_nso(&mut Cursor::new(&bad), bad.len() as u64, &l, &c).is_err());
    // Truncated segment ranges are still refused.
    assert!(validate_nso(
        &mut Cursor::new(&good[..good.len() - 1]),
        good.len() as u64 - 1,
        &l,
        &c
    )
    .is_err());
}
#[test]
fn manifest_missing_identity_duplicates_dangerous_paths_and_partial_contract() {
    let good = base();
    assert_eq!(manifest(&good).snapshots[0].numeric_version, 0);
    for bad in [
        good.replace("NXIMPORT\t1", "NXIMPORT\t2"),
        good.replace("switch2", "unknown"),
        good.replace("\tbase\tswitch2", "\t../base\tswitch2"),
        good.replace("\tbase\tswitch2", "\tC:/base\tswitch2"),
        good.replace("\tcomplete\t", "\tpartial\t"),
        format!(
            "{good}{}",
            synthetic::row("b", "base", "base", 0, true, 0x5678)
        ),
    ] {
        assert!(DeclarationManifest::read(bad.as_bytes(), &ImportLimits::default()).is_err());
    }
    for path in [
        "../x", "a/../x", "/x", "a\\b", "a:b", "AUX.txt", "a.", "a ", "a//b", "a/\u{e9}",
    ] {
        assert!(portable_path(path, &ImportLimits::default()).is_err());
    }
}
#[test]
fn discovery_has_no_identity_and_complete_update_only_needs_no_base() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("updates/917504"), 0x5678, 0x42).unwrap();
    let found = discover(&source, &ImportLimits::default()).unwrap();
    assert_eq!(found[0].relative_root, "updates/917504");
    let m = manifest(&format!(
        "NXIMPORT\t1\n{}",
        synthetic::row("u", "updates/917504", "update", 77, true, 0x5678)
    ));
    let r = convert_extracted(
        &source,
        &m,
        &t.0.join("out.nx"),
        &ImportLimits::default(),
        &WriterOptions::default(),
    )
    .unwrap();
    let a = crate::inspect_archive(
        t.0.join("out.nx"),
        &WriterOptions::default(),
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert_eq!(a.applications[0].snapshots[0].numeric_version, 77);
    assert_eq!(a.applications[0].platform, Platform::Switch2);
    assert_eq!(r.receipt.snapshots["u"].build.value, Some([0x42; 32]));
}
#[test]
fn complete_versions_dlc_and_detached_provenance_survive_source_removal_and_repack() {
    let t = Temp::new();
    let source = t.0.join("source");
    let m = manifest(&synthetic::package(&source, 0).unwrap());
    let opts = WriterOptions::default();
    let result = convert_extracted(&source, &m, &t.0.join("out.nx"), &paranoid(), &opts).unwrap();
    assert!(result.source_bytes_compared > 0);
    assert_eq!(result.receipt.snapshots["b"].build.value, Some([0x41; 32]));
    assert_eq!(result.receipt.snapshots["u"].build.value, Some([0x42; 32]));
    assert_eq!(
        result.receipt.snapshots["v"].module_build_ids["exefs/sdk"],
        Some([0x61; 32])
    );
    let bytes = result.receipt.encode(&ImportLimits::default()).unwrap();
    let receipt = ImportReceipt::read(bytes.as_slice(), &ImportLimits::default()).unwrap();
    assert_eq!(receipt.encode(&ImportLimits::default()).unwrap(), bytes);
    fs::remove_dir_all(&source).unwrap();
    crate::repack_archive(
        t.0.join("out.nx"),
        t.0.join("repack.nx"),
        RepackPlan::new(),
        &opts,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    let archive =
        crate::inspect_archive(t.0.join("repack.nx"), &opts, IntegrityPolicy::Strict).unwrap();
    receipt.match_archive(&archive).unwrap();
    assert!(archive.applications[0].metadata_snapshot_identity.is_none());
    assert!(archive.applications[0]
        .snapshots
        .iter()
        .all(|s| !s.has_display_metadata));
    assert!(String::from_utf8(receipt.manifest.bytes().to_vec())
        .unwrap()
        .contains("Declared title"));
}
#[test]
fn partial_composition_is_exact_preserves_data_only_build_and_explicit_deletions() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let opts = WriterOptions::default();
    let limits = ImportLimits::default();
    let r = convert_extracted(
        &source,
        &manifest(&base()),
        &t.0.join("base.nx"),
        &limits,
        &opts,
    )
    .unwrap();
    let id = r.receipt.snapshots["b"].identity;
    fs::create_dir_all(source.join("updates/partial/romfs")).unwrap();
    fs::write(
        source.join("updates/partial/romfs/data.bin"),
        b"changed data only",
    )
    .unwrap();
    let text = format!(
        "{}{}compose\tp\treplace-delete-v1\tb\t{}\nremove\tp\tromfs/delete.me\n",
        base(),
        synthetic::row("p", "updates/partial", "update", 10, false, 0x5678),
        hex(&id)
    );
    let result = convert_extracted(
        &source,
        &manifest(&text),
        &t.0.join("out.nx"),
        &limits,
        &opts,
    )
    .unwrap();
    let p = &result.receipt.snapshots["p"];
    assert_eq!(p.build.value, Some([0x41; 32]));
    assert!(matches!(p.build.origin, Origin::Derived { .. }));
    let files = crate::inspect_snapshot_files(
        t.0.join("out.nx"),
        p.identity,
        &opts,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    assert!(!files.iter().any(|f| f.path == "romfs/delete.me"));
    assert!(files.iter().any(|f| f.path == "exefs/main"));
    let bad = text.replace(&hex(&id), &"ab".repeat(32));
    assert!(convert_extracted(
        &source,
        &manifest(&bad),
        &t.0.join("bad.nx"),
        &limits,
        &opts
    )
    .is_err());
    assert!(!t.0.join("bad.nx").exists());
}
#[test]
fn native_conflicts_corruption_cancellation_budgets_and_publication_preserve_destination() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let out = t.0.join("out.nx");
    fs::write(&out, b"keep destination").unwrap();
    let limits = ImportLimits::default();
    let conflict = manifest(&base().replace("0000000000005678", "0000000000009999"));
    assert!(
        convert_extracted(&source, &conflict, &out, &limits, &WriterOptions::default()).is_err()
    );
    let m = manifest(&base());
    let opts = WriterOptions {
        max_temporary_disk_bytes: 1,
        ..Default::default()
    };
    assert!(convert_extracted(&source, &m, &out, &limits, &opts).is_err());
    let opts = WriterOptions::default();
    opts.cancellation.cancel();
    assert!(matches!(
        convert_extracted(&source, &m, &out, &limits, &opts)
            .err()
            .unwrap()
            .kind(),
        WriterErrorKind::Cancelled
    ));
    let tiny = ImportLimits {
        max_file_bytes: 4,
        ..Default::default()
    };
    assert!(convert_extracted(&source, &m, &out, &tiny, &WriterOptions::default()).is_err());
    assert_eq!(fs::read(&out).unwrap(), b"keep destination");
    assert!(convert_extracted(
        &source,
        &m,
        &source.join("base/romfs/out.nx"),
        &limits,
        &WriterOptions::default()
    )
    .is_err());
    let mut bad = synthetic::nso(0x41, true);
    bad[0xa0] ^= 1;
    fs::write(source.join("base/exefs/main"), bad).unwrap();
    assert!(convert_extracted(&source, &m, &out, &limits, &WriterOptions::default()).is_err());
    assert_eq!(fs::read(&out).unwrap(), b"keep destination");
}
#[test]
fn case_collisions_unknown_artifacts_and_explicit_exclusions() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    fs::write(source.join("base/.listing"), b"not a game file").unwrap();
    assert!(inspect_tree(
        &source,
        &manifest(&base()),
        "b",
        &ImportLimits::default(),
        &CancellationToken::default()
    )
    .is_err());
    let m = manifest(&(base() + "exclude\tb\t.listing\n"));
    let v = inspect_tree(
        &source,
        &m,
        "b",
        &ImportLimits::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(!v.files.contains_key(".listing"));
    fs::create_dir_all(source.join("base/romfs/EXEFS")).unwrap();
    fs::create_dir_all(source.join("base/romfs/exefs")).unwrap();
    // Only a case-sensitive file system (not macOS or Windows by default)
    // can hold both names.
    #[cfg(unix)]
    let both = fs::read_dir(source.join("base/romfs")).unwrap().count() == 2;
    #[cfg(unix)]
    assert!(
        !both
            || inspect_tree(
                &source,
                &m,
                "b",
                &ImportLimits::default(),
                &CancellationToken::default()
            )
            .is_err()
    );
}
#[test]
fn symlink_sources_are_rejected_including_excluded_entries() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let outside = t.0.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret"), b"synthetic").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, source.join("base/romfs/link")).unwrap();
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(source.join("base").join("romfs").join("link"))
            .arg(&outside)
            .output()
            .unwrap();
        assert!(status.status.success(), "junction fixture creation failed");
    }
    assert!(inspect_tree(
        &source,
        &manifest(&base()),
        "b",
        &ImportLimits::default(),
        &CancellationToken::default()
    )
    .is_err());
}

#[test]
fn latest_partial_chain_retains_exact_closure_and_rejects_unresolvable_versions() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let limits = paranoid();
    let opts = WriterOptions::default();
    let first = convert_extracted(
        &source,
        &manifest(&base()),
        &t.0.join("base.nx"),
        &limits,
        &opts,
    )
    .unwrap();
    fs::create_dir_all(source.join("updates/p/romfs")).unwrap();
    fs::write(source.join("updates/p/romfs/data.bin"), b"partial one").unwrap();
    let text = format!(
        "{}{}compose\tp\treplace-delete-v1\tb\t{}\n",
        base(),
        synthetic::row("p", "updates/p", "update", 10, false, 0x5678),
        hex(&first.receipt.snapshots["b"].identity)
    );
    let selected = manifest(&text)
        .select_latest_complete(&Default::default(), &limits)
        .unwrap();
    assert_eq!(selected.retained_dependency_keys, vec!["b"]);
    assert!(selected.omitted_snapshot_keys.is_empty());
    let second = convert_extracted(
        &source,
        &selected.manifest,
        &t.0.join("p.nx"),
        &limits,
        &opts,
    )
    .unwrap();
    assert_eq!(
        verify_extracted_sources(&t.0.join("p.nx"), &source, &second.receipt, &limits, &opts)
            .unwrap(),
        second.source_bytes_compared
    );
    fs::create_dir_all(source.join("updates/q/romfs")).unwrap();
    fs::write(source.join("updates/q/romfs/data.bin"), b"partial two").unwrap();
    let chained = format!(
        "{}{}compose\tq\treplace-delete-v1\tp\t{}\n",
        text,
        synthetic::row("q", "updates/q", "update", 20, false, 0x5678),
        hex(&second.receipt.snapshots["p"].identity)
    );
    let latest = manifest(&chained)
        .select_latest_complete(&Default::default(), &limits)
        .unwrap();
    assert_eq!(latest.retained_dependency_keys, vec!["b", "p"]);
    let final_result =
        convert_extracted(&source, &latest.manifest, &t.0.join("q.nx"), &limits, &opts).unwrap();
    assert_eq!(final_result.receipt.snapshots.len(), 3);
    fs::create_dir_all(source.join("dlc/extra/0/romfs")).unwrap();
    fs::write(source.join("dlc/extra/0/romfs/asset"), b"DLC").unwrap();
    let with_dlc = format!("{}snapshot\td\tdlc/extra/0\tswitch2\t0000000000001234\t-\tnone\tdlc\t1\tcomplete\tmaterialized-v1\tnone\ncontent\td\t11111111111111111111111111111111\ndependency\td\trequires\tq\n",chained);
    let dlc_selection = manifest(&with_dlc)
        .select_latest_complete(&std::collections::BTreeSet::from(["d".into()]), &limits)
        .unwrap();
    assert_eq!(dlc_selection.retained_dependency_keys, vec!["b", "p"]);
    let dlc_result = convert_extracted(
        &source,
        &dlc_selection.manifest,
        &t.0.join("dlc.nx"),
        &limits,
        &opts,
    )
    .unwrap();
    assert_eq!(dlc_result.receipt.snapshots.len(), 4);

    verify_extracted_sources(
        &t.0.join("q.nx"),
        &source,
        &final_result.receipt,
        &limits,
        &opts,
    )
    .unwrap();
    let wrong = chained.replace(
        &hex(&second.receipt.snapshots["p"].identity),
        &"ab".repeat(32),
    );
    let selected_wrong = manifest(&wrong)
        .select_latest_complete(&Default::default(), &limits)
        .unwrap();
    fs::write(t.0.join("preserved.nx"), b"original destination").unwrap();
    assert!(convert_extracted(
        &source,
        &selected_wrong.manifest,
        &t.0.join("preserved.nx"),
        &limits,
        &opts
    )
    .is_err());
    assert_eq!(
        fs::read(t.0.join("preserved.nx")).unwrap(),
        b"original destination"
    );
    assert!(DeclarationManifest::read(
        chained
            .replace("replace-delete-v1\tp\t", "replace-delete-v1\tmissing\t")
            .as_bytes(),
        &limits
    )
    .is_err());
    let reverse = chained.replace(
        "q\tupdates/q\tswitch2\t0000000000001234\t0000000000005678\tprimary\tupdate\t20",
        "q\tupdates/q\tswitch2\t0000000000001234\t0000000000005678\tprimary\tupdate\t5",
    );
    assert!(manifest(&reverse)
        .select_latest_complete(&Default::default(), &limits)
        .is_err());
    let ambiguous = format!(
        "{}{}",
        chained,
        synthetic::row("r", "updates/q", "update", 20, true, 0x5678)
    );
    assert!(manifest(&ambiguous)
        .select_latest_complete(&Default::default(), &limits)
        .is_err());
    // A receipt never permits a changed source, even at the same length.
    fs::write(source.join("updates/q/romfs/data.bin"), b"partial bad").unwrap();
    assert!(verify_extracted_sources(
        &t.0.join("q.nx"),
        &source,
        &final_result.receipt,
        &limits,
        &opts
    )
    .is_err());
}

#[test]
fn latest_partial_keeps_explicitly_selected_dlc_and_rejects_cycles() {
    let t = Temp::new();
    let source = t.0.join("source");
    let original = synthetic::package(&source, 0).unwrap();
    let cyclic = original
        .replace(
            "dependency\tu\trequired-base\tb",
            "dependency\tu\trequires\tv",
        )
        .replace(
            "dependency\tv\trequired-base\tb",
            "dependency\tv\trequires\tu",
        );
    assert!(manifest(&cyclic)
        .select_latest_complete(&Default::default(), &ImportLimits::default())
        .is_err());
    let m = manifest(&original);
    let no_dlc = m
        .select_latest_complete(&Default::default(), &ImportLimits::default())
        .unwrap();
    assert!(no_dlc.omitted_snapshot_keys.contains(&"d".to_owned()));
    assert!(m
        .select_latest_complete(
            &std::collections::BTreeSet::from(["missing".into()]),
            &ImportLimits::default()
        )
        .is_err());
}

#[test]
fn addon_declarations_must_name_the_application_add_on_range() {
    let manifest = |addon: &str, extra: &str| {
        format!(
            "NXIMPORT\t1\nsnapshot\tdlc\tdlc\tswitch2\t0400c76024710000\t-\tnone\tdlc\t0\tcomplete\tmaterialized-v1\tnone\naddon\tdlc\t{addon}\n{extra}"
        )
    };
    let read = |text: String| DeclarationManifest::read(text.as_bytes(), &ImportLimits::default());
    let ok = read(manifest("0400c76024711001", "")).unwrap();
    assert_eq!(ok.snapshots[0].addon_title_id, Some(0x0400c76024711001));
    // Another application's range, index 0, and a Content ID beside it.
    assert!(read(manifest("0400c76024721001", "")).is_err());
    assert!(read(manifest("0400c76024711000", "")).is_err());
    assert!(read(manifest(
        "0400c76024711001",
        &format!("content\tdlc\t{}\n", "11".repeat(16))
    ))
    .is_err());
    assert!(addon_belongs_to(0x010015100b515001, 0x010015100b514000));
}

#[test]
fn dump_metadata_beside_exefs_is_metadata_not_tree_content() {
    let t = Temp::new();
    let source = t.0.join("source");
    synthetic::executable_tree(&source.join("base"), 0x5678, 0x41).unwrap();
    let mut nacp = vec![0u8; 0x4000];
    nacp[..10].copy_from_slice(b"Dump Title");
    fs::write(source.join("base/control.nacp"), &nacp).unwrap();
    fs::write(
        source.join("base/icon_AmericanEnglish.dat"),
        [0xff, 0xd8, 0xff, 0xe0, 0, 0x10, b'J', b'F', b'I', b'F'],
    )
    .unwrap();
    let opts = WriterOptions::default();
    let limits = ImportLimits::default();
    // Undeclared, the files are unknown root entries.
    assert!(convert_extracted(
        &source,
        &manifest(&base()),
        &t.0.join("a.nx"),
        &limits,
        &opts
    )
    .is_err());
    let declared = base()
        + "nacp\tb\tbase/control.nacp\nicon\tb\tAmericanEnglish\tbase/icon_AmericanEnglish.dat\n";
    let out = t.0.join("b.nx");
    let result = convert_extracted(&source, &manifest(&declared), &out, &limits, &opts).unwrap();
    let identity = result.receipt.snapshots["b"].identity;
    let files =
        crate::inspect_snapshot_files(&out, identity, &opts, IntegrityPolicy::Strict).unwrap();
    assert!(files
        .iter()
        .all(|f| f.path.starts_with("exefs/") || f.path.starts_with("romfs/")));
    let summary = crate::inspect_archive(&out, &opts, IntegrityPolicy::Strict).unwrap();
    let snapshot = &summary.applications[0].snapshots[0];
    assert!(snapshot.has_display_metadata && snapshot.nacp_digest.is_some());
}

#[test]
fn update_tree_is_added_in_place_and_duplicates_leave_the_archive_unchanged() {
    let t = Temp::new();
    let game = t.0.join("game");
    synthetic::executable_tree(&game.join("base"), 0x5678, 0x41).unwrap();
    let out = t.0.join("out.nx");
    convert_extracted(
        &game,
        &manifest(&base()),
        &out,
        &ImportLimits::default(),
        &WriterOptions::default(),
    )
    .unwrap();
    let update = t.0.join("update");
    synthetic::executable_tree(&update, 0x5678, 0x42).unwrap();
    fs::write(update.join("romfs/patched.bin"), b"update data").unwrap();
    let text = format!(
        "NXIMPORT\t1\n{}",
        synthetic::row("u", ".", "standalone-update", 65536, true, 0x5678)
    );
    let stats = add_extracted(
        &out,
        &update,
        &manifest(&text),
        &ImportLimits::default(),
        &WriterOptions::default(),
    )
    .unwrap();
    assert_eq!(stats.additions.len(), 1);
    let summary =
        crate::inspect_archive(&out, &WriterOptions::default(), IntegrityPolicy::Strict).unwrap();
    let versions: Vec<_> = summary.applications[0]
        .snapshots
        .iter()
        .map(|s| s.numeric_version)
        .collect();
    assert_eq!(versions.len(), 2);
    assert!(versions.contains(&0) && versions.contains(&65536));
    // The same update again, and a second base, are refused in place.
    let before = fs::read(&out).unwrap();
    for (package, text) in [(&update, text.clone()), (&game, base())] {
        assert!(add_extracted(
            &out,
            package,
            &manifest(&text),
            &ImportLimits::default(),
            &WriterOptions::default(),
        )
        .is_err());
        assert_eq!(fs::read(&out).unwrap(), before);
    }
    // Only an existing archive is extended.
    assert!(add_extracted(
        &t.0.join("missing.nx"),
        &update,
        &manifest(&text),
        &ImportLimits::default(),
        &WriterOptions::default(),
    )
    .is_err());
}
