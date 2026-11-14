use super::*;
use crate::{inspect_archive, IntegrityPolicy, ProgramKind, WriterOptions};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

const APP: u64 = 0x0100_4530_19aa_8000;
const OUNCE: u64 = 0x0400_4530_19aa_8001;
static SERIAL: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nx-program-dump-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn package(&self) -> PathBuf {
        self.0.join("dump")
    }
    fn put(&self, path: &str, bytes: &[u8]) {
        let path = self.package().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn plan(&self, selected: &[String]) -> Result<Option<ProgramDumpPlan>, WriterError> {
        plan_program_dump(
            &self.package(),
            selected,
            &ImportLimits::default(),
            &CancellationToken::default(),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn cnmt(kind: u8, title: u64, version: u32, required: u32) -> Vec<u8> {
    let indices: &[u8] = if kind == 0x82 { &[0] } else { &[0, 1] };
    let ext = if kind == 0x80 { 0x10 } else { 0x18 };
    let mut b = vec![0u8; 0x20 + ext + indices.len() * 0x38];
    b[..8].copy_from_slice(&title.to_le_bytes());
    b[8..12].copy_from_slice(&version.to_le_bytes());
    b[12] = kind;
    b[14..16].copy_from_slice(&(ext as u16).to_le_bytes());
    b[16..18].copy_from_slice(&(indices.len() as u16).to_le_bytes());
    b[0x20..0x28].copy_from_slice(&(if kind == 0x80 { APP | 0x800 } else { APP }).to_le_bytes());
    if kind == 0x82 {
        b[0x28..0x2c].copy_from_slice(&required.to_le_bytes());
    }
    for (i, index) in indices.iter().enumerate() {
        let at = 0x20 + ext + i * 0x38;
        b[at + 0x36] = if kind == 0x82 { 2 } else { 1 };
        b[at + 0x37] = *index;
    }
    b
}

fn fixture(with_base: bool) -> Fixture {
    let f = Fixture::new();
    let mut groups = vec![("updates/196608", 0x81, APP | 0x800, 196608)];
    if with_base {
        groups.push(("base", 0x80, APP, 0));
    }
    for (group, kind, title, version) in groups {
        for (index, id, profile) in [
            (0, APP, NpdmProfile::MetaAcid2048V1),
            (1, OUNCE, NpdmProfile::MetaAcid3072V1),
        ] {
            let root = format!("{group}/program/{index}");
            f.put(
                &format!("{root}/exefs/main.npdm"),
                &super::super::synthetic::npdm(id, profile),
            );
            f.put(
                &format!("{root}/exefs/main"),
                &super::super::synthetic::nso(index + 0x50, false),
            );
            f.put(&format!("{root}/romfs/common.bin"), &[index; 31]);
            f.put(
                &format!("{root}/control/control.nacp"),
                &super::super::synthetic_nup::nacp(&[(
                    0,
                    if index == 1 {
                        "Ounce Edition"
                    } else {
                        "Legacy"
                    },
                )]),
            );
            f.put(&format!("{root}/logo/common.bin"), &[index; 17]);
        }
        f.put(
            &format!("{group}/program/0/meta/native.cnmt"),
            &cnmt(kind, title, version, 0),
        );
    }
    for index in [1, 2] {
        let root = format!("dlc/{:016x}/0/base/program/0", APP + 0x1000 + index);
        f.put(&format!("{root}/romfs/common.bin"), &[index as u8; 13]);
        f.put(
            &format!("{root}/meta/native.cnmt"),
            &cnmt(0x82, APP + 0x1000 + index, 0, 196608),
        );
    }
    f
}

#[test]
fn shared_cnmt_scopes_programs_controls_dlc_and_exact_bases() {
    for with_base in [false, true] {
        let f = fixture(with_base);
        let plan = f.plan(&[]).unwrap().unwrap();
        let out = f.0.join("game.nx");
        let limits = ImportLimits {
            compare_source_bytes: true,
            ..Default::default()
        };
        let options = WriterOptions::default();
        let stats =
            convert_extracted(&f.package(), &plan.manifest, &out, &limits, &options).unwrap();
        assert!(
            verify_extracted_sources(&out, &f.package(), &stats.receipt, &limits, &options)
                .unwrap()
                > 0
        );
        let archive = inspect_archive(&out, &options, IntegrityPolicy::Strict).unwrap();
        let app = &archive.applications[0];
        assert_eq!(app.application_id, APP);
        assert_eq!(app.platform, Platform::Switch2);
        assert!(app
            .programs
            .iter()
            .any(|p| p.program_id == OUNCE && p.kind == ProgramKind::Primary));
        assert_eq!(app.snapshots.len(), if with_base { 6 } else { 4 });
        for s in &app.snapshots {
            if s.snapshot_type == SnapshotType::Dlc {
                assert_eq!(s.dependencies.len(), 1);
                continue;
            }
            let index = u8::from(s.program_id == Some(OUNCE));
            for (path, size) in [("romfs/common.bin", 31), ("data/logo/common.bin", 17)] {
                let mut bytes = vec![0; size];
                crate::read_snapshot_file_range(&out, s.identity, path, 0, &mut bytes, &options)
                    .unwrap();
                assert_eq!(bytes, vec![index; size]);
            }
            assert!(crate::read_snapshot_nacp(&out, s.identity, &options)
                .unwrap()
                .is_some());
            if s.snapshot_type == SnapshotType::Update {
                assert!(s.dependencies.iter().any(|d| app
                    .snapshots
                    .iter()
                    .any(|b| b.identity == d.target_snapshot_identity
                        && b.program_id == s.program_id
                        && b.snapshot_type == SnapshotType::Base)));
            }
        }
        assert!(plan.notes.contains("PRESUMED"));
    }
}

#[test]
fn selected_programs_use_shared_metadata_without_importing_siblings() {
    let f = fixture(false);
    let chosen = vec!["updates/196608/program/1".to_owned()];
    let plan = f.plan(&chosen).unwrap().unwrap();
    assert_eq!(plan.manifest.snapshots.len(), 1);
    assert_eq!(plan.manifest.snapshots[0].program_id, Some(OUNCE));
    assert!(f.plan(&["updates/196608/program/7".into()]).is_err());
    assert!(f
        .plan(&[format!("dlc/{:016x}/0/base/program/0", APP + 0x1001)])
        .is_err());
    f.put(
        &format!(
            "dlc/{:016x}/0/base/program/0/meta/native.cnmt",
            APP + 0x1001
        ),
        &[0; 7],
    );
    assert!(
        f.plan(&chosen).is_ok(),
        "unselected package metadata must not block an explicit Program selection"
    );
}

#[test]
fn top_level_program_container_uses_native_base_version() {
    let f = fixture(true);
    let standalone = Fixture::new();
    fs::create_dir_all(standalone.package()).unwrap();
    fs::rename(
        f.package().join("base/program"),
        standalone.package().join("program"),
    )
    .unwrap();
    standalone.put("program/0/meta/native.cnmt", &cnmt(0x80, APP, 65536, 0));
    let plan = standalone.plan(&[]).unwrap().unwrap();
    assert_eq!(plan.manifest.snapshots.len(), 2);
    assert!(plan
        .manifest
        .snapshots
        .iter()
        .all(|s| s.snapshot_type == SnapshotType::Base));
    assert!(plan
        .manifest
        .snapshots
        .iter()
        .all(|s| s.numeric_version == 65536));
}

#[test]
fn metadata_conflicts_and_ambiguous_views_are_rejected() {
    for case in 0..8 {
        let f = fixture(false);
        let metadata = "updates/196608/program/0/meta/native.cnmt";
        match case {
            0 => f.put(metadata, &[0; 7]),
            1 => f.put(metadata, &cnmt(0x81, APP | 0x800, 65536, 0)),
            2 => f.put(
                "updates/196608/program/1/exefs/main.npdm",
                &super::super::synthetic::npdm(OUNCE + 1, NpdmProfile::MetaAcid3072V1),
            ),
            3 => {
                fs::remove_file(f.package().join(metadata)).unwrap();
            }
            4 => f.put(
                "updates/196608/program/1/meta/other.cnmt",
                &cnmt(0x81, APP | 0x800, 0, 0),
            ),
            5 => f.put(
                &format!(
                    "dlc/{:016x}/0/base/program/0/meta/native.cnmt",
                    APP + 0x1001
                ),
                &cnmt(0x82, APP + 0x1001, 0, 262144),
            ),
            6 => {
                let mut b = cnmt(0x81, APP | 0x800, 196608, 0);
                b[0x20..0x28].copy_from_slice(&(APP + 0x10000).to_le_bytes());
                f.put(metadata, &b);
            }
            _ => {
                let mut b = cnmt(0x81, APP | 0x800, 196608, 0);
                b[13] = 2;
                f.put(metadata, &b);
            }
        }
        assert!(f.plan(&[]).is_err(), "case {case} accepted");
    }
    let f = fixture(false);
    f.put(
        "updates/196608/program/1/data/logo/common.bin",
        b"collision",
    );
    let plan = f.plan(&[]).unwrap().unwrap();
    assert!(convert_extracted(
        &f.package(),
        &plan.manifest,
        &f.0.join("bad.nx"),
        &ImportLimits::default(),
        &WriterOptions::default()
    )
    .is_err());
    assert!(!f.0.join("bad.nx").exists());
}
