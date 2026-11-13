//! nxconvert commands run in process, as a frontend runs them.
use nxformat_packer::{cli, CancellationToken};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/fixtures")
        .join(name)
}

type Lines = Arc<Mutex<Vec<(bool, String)>>>;

fn collect() -> (cli::Sink, Lines) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let store = lines.clone();
    let sink: cli::Sink =
        Arc::new(move |error, line: &str| store.lock().unwrap().push((error, line.to_owned())));
    (sink, lines)
}

#[test]
fn contents_lines_go_to_the_sink() {
    let (sink, lines) = collect();
    let archive = fixture("dlc_only_application.nx");
    cli::run(
        vec!["contents".into(), archive.display().to_string()],
        Some(sink),
        None,
    )
    .unwrap();
    let lines = lines.lock().unwrap();
    let size = std::fs::metadata(&archive).unwrap().len();
    assert!(lines.iter().any(|(e, l)| !e && l.starts_with("@APP\t")));
    assert!(lines.contains(&(false, format!("@SIZE\t{size}"))));
}

#[test]
fn a_cancelled_edit_leaves_the_archive_as_it_was() {
    let dir = std::env::temp_dir().join(format!("nxformat-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let archive = dir.join("shared_with_orphan_block.nx");
    std::fs::copy(fixture("shared_with_orphan_block.nx"), &archive).unwrap();
    let before = std::fs::read(&archive).unwrap();
    let token = CancellationToken::default();
    token.cancel();
    let (sink, _) = collect();
    let result = cli::run(
        vec![
            "compact".into(),
            archive.display().to_string(),
            "--frontend".into(),
        ],
        Some(sink),
        Some(token),
    );
    assert!(result.is_err());
    assert_eq!(std::fs::read(&archive).unwrap(), before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_sink_is_released_after_a_run() {
    let (sink, lines) = collect();
    let archive = fixture("dlc_only_application.nx").display().to_string();
    cli::run(vec!["contents".into(), archive.clone()], Some(sink), None).unwrap();
    let count = lines.lock().unwrap().len();
    // A second run without a sink prints to stdout, not to the old sink.
    cli::run(vec!["contents".into(), archive], None, None).unwrap();
    assert_eq!(lines.lock().unwrap().len(), count);
}

#[test]
fn nup_cli_checks_disk_limits_before_decoding() {
    use nxformat_packer::import::synthetic_nup::{base, build, Node};
    let dir = std::env::temp_dir().join(format!("nxformat-cli-nup-budget-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let nup = dir.join("game.nup");
    let out = dir.join("game.nx");
    let mut game = base(0x0400_0000_0000_1000);
    // RomFS is streamed from the NUP; only ExeFS is staged on disk.
    game.exefs
        .get_or_insert_with(Vec::new)
        .push(Node::File("large.bin", vec![0; 2 * 1024 * 1024]));
    std::fs::write(&nup, build(&[game], true)).unwrap();
    let (sink, _) = collect();
    let result = cli::run(
        vec![
            "pack-nup".into(),
            nup.display().to_string(),
            out.display().to_string(),
            "--platform".into(),
            "switch2".into(),
            "--temp-mib".into(),
            "1".into(),
        ],
        Some(sink),
        None,
    );
    assert!(result.unwrap_err().to_string().contains("staging budget"));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn nup_cli_honors_the_callers_cancellation_before_staging() {
    use nxformat_packer::import::synthetic_nup::{base, build};
    let dir = std::env::temp_dir().join(format!("nxformat-cli-nup-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let nup = dir.join("game.nup");
    std::fs::write(&nup, build(&[base(0x0400_0000_0000_1000)], true)).unwrap();
    let token = CancellationToken::default();
    token.cancel();
    let (sink, _) = collect();
    let result = cli::run(
        vec![
            "pack-nup".into(),
            nup.display().to_string(),
            dir.join("game.nx").display().to_string(),
            "--platform".into(),
            "switch2".into(),
        ],
        Some(sink),
        Some(token),
    );
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn extracted_multi_program_creates_independent_program_snapshots() {
    use nxformat_packer::{
        import::{self, structures::NpdmProfile},
        inspect_archive, IntegrityPolicy, WriterOptions,
    };
    let dir = std::env::temp_dir().join(format!("nxformat-cli-program-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let source = dir.join("game");
    let output = dir.join("game.nx");
    let app = 0x0400_0000_0000_1000;
    for (root, program, build) in [
        ("base", app, 0x41),
        ("base/program1", app | 1, 0x42),
        ("updates/65536", app, 0x43),
        ("updates/65536/program1", app | 1, 0x44),
    ] {
        let exefs = source.join(root).join("exefs");
        std::fs::create_dir_all(&exefs).unwrap();
        std::fs::write(
            exefs.join("main.npdm"),
            import::synthetic::npdm(program, NpdmProfile::MetaAcid3072V1),
        )
        .unwrap();
        std::fs::write(exefs.join("main"), import::synthetic::nso(build, true)).unwrap();
    }
    let ordinary = source.join("base/data/program_data");
    std::fs::create_dir_all(&ordinary).unwrap();
    std::fs::write(ordinary.join("readme.bin"), b"ordinary content").unwrap();
    let (sink, _) = collect();
    cli::run(
        vec![
            "pack-tree".into(),
            source.display().to_string(),
            output.display().to_string(),
            "--platform".into(),
            "switch2".into(),
        ],
        Some(sink),
        None,
    )
    .unwrap();
    let summary =
        inspect_archive(&output, &WriterOptions::default(), IntegrityPolicy::Strict).unwrap();
    assert_eq!(summary.applications.len(), 1);
    assert_eq!(summary.applications[0].programs.len(), 2);
    assert_eq!(summary.applications[0].snapshots.len(), 4);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn switch2_edition_extracted_tree_keeps_upgrade_and_dlc_together() {
    use nxformat_packer::{
        import::{self, structures::NpdmProfile},
        inspect_archive, IntegrityPolicy, ProgramKind, SnapshotType, WriterOptions,
    };
    let dir = std::env::temp_dir().join(format!(
        "nxformat-cli-switch2-edition-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let source = dir.join("game");
    let output = dir.join("game.nx");
    let old = 0x0100_4d30_0c5a_e000;
    let upgraded = 0x0400_4d30_0c5a_e001;
    for (root, id, build) in [("base", old, 0x41), ("updates/65536", upgraded, 0x42)] {
        let exefs = source.join(root).join("exefs");
        std::fs::create_dir_all(&exefs).unwrap();
        std::fs::write(
            exefs.join("main.npdm"),
            import::synthetic::npdm(id, NpdmProfile::MetaAcid3072V1),
        )
        .unwrap();
        std::fs::write(exefs.join("main"), import::synthetic::nso(build, true)).unwrap();
    }
    let dlc = source.join(format!("dlc/{:016x}/0/romfs", old + 0x1001));
    std::fs::create_dir_all(&dlc).unwrap();
    std::fs::write(dlc.join("extra.bin"), b"upgrade data").unwrap();
    let (sink, _) = collect();
    cli::run(
        vec![
            "pack-tree".into(),
            source.display().to_string(),
            output.display().to_string(),
            "--platform".into(),
            "switch2".into(),
        ],
        Some(sink),
        None,
    )
    .unwrap();
    let archive =
        inspect_archive(&output, &WriterOptions::default(), IntegrityPolicy::Strict).unwrap();
    assert_eq!(archive.applications.len(), 1);
    let app = &archive.applications[0];
    assert_eq!(app.application_id, old);
    assert!(app
        .programs
        .iter()
        .any(|p| p.program_id == old && p.kind == ProgramKind::Auxiliary));
    assert!(app
        .programs
        .iter()
        .any(|p| p.program_id == upgraded && p.kind == ProgramKind::Primary));
    assert!(app
        .snapshots
        .iter()
        .any(|s| s.snapshot_type == SnapshotType::Dlc));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn standalone_switch2_edition_tree_uses_verified_dlc_family() {
    use nxformat_packer::{
        import::{self, structures::NpdmProfile},
        inspect_archive, IntegrityPolicy, WriterOptions,
    };
    let dir = std::env::temp_dir().join(format!(
        "nxformat-cli-standalone-edition-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let source = dir.join("game");
    let output = dir.join("game.nx");
    let old = 0x0100_4d30_0c5a_e000;
    let upgraded = 0x0400_4d30_0c5a_e001;
    let exefs = source.join("updates/65536/exefs");
    std::fs::create_dir_all(&exefs).unwrap();
    std::fs::write(
        exefs.join("main.npdm"),
        import::synthetic::npdm(upgraded, NpdmProfile::MetaAcid3072V1),
    )
    .unwrap();
    std::fs::write(exefs.join("main"), import::synthetic::nso(0x46, true)).unwrap();
    let dlc = source.join(format!("dlc/{:016x}/0/romfs", old + 0x1001));
    std::fs::create_dir_all(&dlc).unwrap();
    std::fs::write(dlc.join("extra.bin"), b"upgrade only").unwrap();
    let (sink, _) = collect();
    cli::run(
        vec![
            "pack-tree".into(),
            source.display().to_string(),
            output.display().to_string(),
            "--platform".into(),
            "switch2".into(),
        ],
        Some(sink),
        None,
    )
    .unwrap();
    let archive =
        inspect_archive(&output, &WriterOptions::default(), IntegrityPolicy::Strict).unwrap();
    assert_eq!(archive.applications.len(), 1);
    assert_eq!(archive.applications[0].application_id, old);
    assert_eq!(archive.applications[0].programs[0].program_id, upgraded);
    let _ = std::fs::remove_dir_all(dir);
}
