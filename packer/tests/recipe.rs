//! Byte-exact rebuild recipes on synthetic DNSP/DXCI packages.

use nxformat_packer::conversion::convert_inventory;
use nxformat_packer::native::synthetic::{dnsp, dxci, title_entries, TitleSpec};
use nxformat_packer::native::{inventory, InventoryMode, InventoryOptions};
use nxformat_packer::recipe::{
    check_recipe, create_recipe, rebuild_from_recipe, recipe_source_name,
};
use nxformat_packer::{
    inspect_archive, repack_archive, ArchiveInput, IntegrityPolicy, RepackPlan, SnapshotType,
    WriterOptions,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const APP: u64 = 0x0100_0000_0001_0000;

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("nx-recipe-test-{}-{id}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.0.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        p
    }
    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn strict() -> InventoryOptions {
    InventoryOptions {
        mode: InventoryMode::Strict,
        ..InventoryOptions::default()
    }
}

fn convert(sources: &[PathBuf], out: &Path) {
    let inv = inventory(sources, &strict()).unwrap();
    convert_inventory(
        &inv,
        out,
        &strict(),
        IntegrityPolicy::Strict,
        &WriterOptions::default(),
    )
    .unwrap();
}

/// A ticket and a certificate are not stored in the NX; the recipe keeps them.
fn with_ticket(mut entries: Vec<(String, Vec<u8>)>, seed: u8) -> Vec<(String, Vec<u8>)> {
    entries.push((
        format!("{:032x}.tik", APP as u128 + seed as u128),
        (0..0x2c0u32)
            .map(|i| (i as u8).wrapping_mul(seed))
            .collect(),
    ));
    entries.push((
        format!("{:032x}.cert", APP as u128 + seed as u128),
        vec![seed; 0x700],
    ));
    entries
}

#[test]
fn dnsp_rebuilds_byte_for_byte() {
    let temp = TempDir::new();
    let source = temp.write(
        "src/Game [base].nsp",
        &dnsp(&with_ticket(
            title_entries(&TitleSpec::application(APP, 0, 1)),
            3,
        )),
    );
    let nx = temp.join("game.nx");
    convert(std::slice::from_ref(&source), &nx);
    let recipe = temp.join("game.nxrecipe");
    let options = WriterOptions::default();
    let stats = create_recipe(&source, &nx, &recipe, &options).unwrap();
    assert!(stats.referenced_bytes > 0);
    assert_eq!(
        stats.unmatched_entries.len(),
        2,
        "{:?}",
        stats.unmatched_entries
    );
    assert!(
        stats.recipe_bytes < 4096,
        "recipe is {} bytes",
        stats.recipe_bytes
    );
    assert_eq!(recipe_source_name(&recipe).unwrap(), "Game [base].nsp");
    let check = check_recipe(&nx, &recipe, &options).unwrap();
    assert_eq!(check.bytes, fs::metadata(&source).unwrap().len());
    let rebuilt = temp.join("out/rebuilt.nsp");
    fs::create_dir(temp.join("out")).unwrap();
    rebuild_from_recipe(&nx, &recipe, &rebuilt, &options).unwrap();
    assert_eq!(fs::read(&rebuilt).unwrap(), fs::read(&source).unwrap());
    // Never overwrites.
    assert!(rebuild_from_recipe(&nx, &recipe, &rebuilt, &options).is_err());
}

#[test]
fn dxci_padding_becomes_fill_and_rebuilds() {
    let temp = TempDir::new();
    let mut image = dxci(&[
        ("update", vec![("firmware.bin".to_owned(), vec![7u8; 5000])]),
        ("normal", Vec::new()),
        ("secure", title_entries(&TitleSpec::application(APP, 0, 4))),
    ]);
    // Untrimmed card: 0xFF up to the card size.
    image.resize(image.len() + 3 * 1024 * 1024 + 123, 0xff);
    let source = temp.write("src/game.xci", &image);
    let nx = temp.join("game.nx");
    convert(std::slice::from_ref(&source), &nx);
    let recipe = temp.join("game.nxrecipe");
    let options = WriterOptions::default();
    let stats = create_recipe(&source, &nx, &recipe, &options).unwrap();
    assert!(stats.fill_bytes >= 3 * 1024 * 1024, "{stats:?}");
    assert!(
        stats.recipe_bytes < 8192,
        "recipe is {} bytes",
        stats.recipe_bytes
    );
    // The system partition is a block-backed source resource, not a literal.
    assert!(stats.literal_bytes < 5000, "{stats:?}");
    assert!(stats.unmatched_entries.is_empty());
    let rebuilt = temp.join("rebuilt.xci");
    rebuild_from_recipe(&nx, &recipe, &rebuilt, &options).unwrap();
    assert_eq!(fs::read(&rebuilt).unwrap(), image);
}

#[test]
fn several_titles_in_one_package_and_edits() {
    let temp = TempDir::new();
    let mut entries = title_entries(&TitleSpec::application(APP, 0, 1));
    entries.extend(title_entries(&TitleSpec::patch(APP, 65536, 2)));
    let source = temp.write("src/all.nsp", &dnsp(&entries));
    let nx = temp.join("game.nx");
    convert(std::slice::from_ref(&source), &nx);
    let recipe = temp.join("all.nxrecipe");
    let options = WriterOptions::default();
    create_recipe(&source, &nx, &recipe, &options).unwrap();
    check_recipe(&nx, &recipe, &options).unwrap();
    // Removing the update removes files the recipe needs: rebuilding fails
    // cleanly and leaves nothing behind.
    let summary = inspect_archive(&nx, &options, IntegrityPolicy::Fast).unwrap();
    let update = summary.applications[0]
        .snapshots
        .iter()
        .find(|s| s.snapshot_type == SnapshotType::Update)
        .unwrap()
        .identity;
    repack_archive(
        &nx,
        &nx,
        RepackPlan {
            remove_snapshots: vec![update],
            additions: ArchiveInput::new(Vec::new()),
            metadata_sources: Vec::new(),
        },
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    let err = check_recipe(&nx, &recipe, &options).unwrap_err();
    assert!(
        err.to_string().contains("no longer in the archive"),
        "{err}"
    );
    let rebuilt = temp.join("rebuilt.nsp");
    assert!(rebuild_from_recipe(&nx, &recipe, &rebuilt, &options).is_err());
    assert!(!rebuilt.exists());
}

#[test]
fn damaged_recipe_is_refused() {
    let temp = TempDir::new();
    let source = temp.write(
        "src/game.nsp",
        &dnsp(&title_entries(&TitleSpec::application(APP, 0, 5))),
    );
    let nx = temp.join("game.nx");
    convert(std::slice::from_ref(&source), &nx);
    let recipe = temp.join("game.nxrecipe");
    let options = WriterOptions::default();
    create_recipe(&source, &nx, &recipe, &options).unwrap();
    let good = fs::read(&recipe).unwrap();
    for at in [0, 20, good.len() / 2, good.len() - 1] {
        let mut bad = good.clone();
        bad[at] ^= 1;
        fs::write(&recipe, &bad).unwrap();
        assert!(check_recipe(&nx, &recipe, &options).is_err(), "byte {at}");
    }
    fs::write(&recipe, &good[..good.len() - 10]).unwrap();
    assert!(check_recipe(&nx, &recipe, &options).is_err());
    // A source with nothing in the archive gets no recipe.
    let other = temp.write(
        "src/other.nsp",
        &dnsp(&title_entries(&TitleSpec::application(APP + 0x2000, 0, 9))),
    );
    assert!(create_recipe(&other, &nx, &temp.join("x.nxrecipe"), &options).is_err());
}

#[test]
fn recipes_kept_inside_the_nx_follow_edits() {
    use nxformat_packer::recipe::{
        build_recipe, check_embedded, embed_recipes, embedded_recipes, rebuild_embedded,
        remove_recipes,
    };
    let temp = TempDir::new();
    let base = temp.write(
        "src/Game [base].nsp",
        &dnsp(&with_ticket(
            title_entries(&TitleSpec::application(APP, 0, 1)),
            3,
        )),
    );
    let update = temp.write(
        "src/Game [update].nsp",
        &dnsp(&with_ticket(title_entries(&TitleSpec::patch(APP, 1, 9)), 4)),
    );
    let nx = temp.join("game.nx");
    convert(&[base.clone(), update.clone()], &nx);
    let options = WriterOptions::default();
    let recipes: Vec<Vec<u8>> = [&base, &update]
        .iter()
        .map(|s| build_recipe(s, &nx, &options).unwrap().0)
        .collect();
    embed_recipes(&nx, recipes, &options).unwrap();
    let kept = embedded_recipes(&nx, &options).unwrap();
    assert_eq!(kept.len(), 2);
    assert!(kept.iter().all(|r| r.application_id == APP));
    // The C-compatible readers skip the optional extension; strict checks pass.
    inspect_archive(&nx, &options, IntegrityPolicy::Strict).unwrap();
    fs::create_dir(temp.join("out")).unwrap();
    for r in &kept {
        let out = temp.join("out").join(&r.info.name);
        rebuild_embedded(&nx, &r.bytes, &out, &options).unwrap();
        let original = if r.info.name.contains("update") {
            &update
        } else {
            &base
        };
        assert_eq!(fs::read(&out).unwrap(), fs::read(original).unwrap());
        assert!(rebuild_embedded(&nx, &r.bytes, &out, &options).is_err());
    }
    // Embedding the same recipe again changes nothing.
    embed_recipes(&nx, vec![kept[0].bytes.clone()], &options).unwrap();
    assert_eq!(embedded_recipes(&nx, &options).unwrap().len(), 2);
    // Removing the update drops the recipe that needs its files.
    let summary = inspect_archive(&nx, &options, IntegrityPolicy::Fast).unwrap();
    let update_id = summary.applications[0]
        .snapshots
        .iter()
        .find(|s| s.snapshot_type == SnapshotType::Update)
        .unwrap()
        .identity;
    let mut plan = RepackPlan::new();
    plan.remove_snapshots.push(update_id);
    repack_archive(&nx, &nx, plan, &options, IntegrityPolicy::Strict).unwrap();
    let left = embedded_recipes(&nx, &options).unwrap();
    assert_eq!(left.len(), 1);
    assert!(left[0].info.name.contains("base"));
    check_embedded(&nx, &left[0].bytes, &options).unwrap();
    // A recipe can also be dropped on its own.
    remove_recipes(&nx, vec![left[0].info.recipe_sha256], &options).unwrap();
    assert!(embedded_recipes(&nx, &options).unwrap().is_empty());
    // A recipe naming a file that is not in the archive is refused.
    let foreign = temp.write(
        "src/Other.nsp",
        &dnsp(&title_entries(&TitleSpec::application(APP + 0x2000, 0, 7))),
    );
    let other_nx = temp.join("other.nx");
    convert(std::slice::from_ref(&foreign), &other_nx);
    let other = build_recipe(&foreign, &other_nx, &options).unwrap().0;
    assert!(embed_recipes(&nx, vec![other], &options).is_err());
}

/// A firmware larger than the entire Catalog must stay out of metadata,
/// survive native edits/compaction, and rebuild in its original byte position.
#[test]
fn large_source_partition_survives_native_edits_and_rebuilds_exactly() {
    use nxformat_packer::recipe::{
        build_recipe, check_embedded, embed_recipes, embedded_recipes, rebuild_embedded,
    };
    use nxformat_packer::{
        add_native_content, compact_archive, inspect_snapshot_files, inspect_source_resources,
        CompressionMode, NativeAddRequest, Platform,
    };
    let temp = TempDir::new();
    let mut state = 0x12345678u32;
    let payload: Vec<u8> = (0..(17 * 1024 * 1024))
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let image = dxci(&[
        ("update", vec![("unknown-system.bin".into(), payload)]),
        ("secure", title_entries(&TitleSpec::application(APP, 0, 4))),
    ]);
    let source = temp.write("src/base.dxci", &image);
    let nx = temp.join("game.nx");
    let options = WriterOptions {
        compression_mode: CompressionMode::RawOnly,
        compression_workers: 1,
        max_memory_bytes: 160 * 1024 * 1024,
        ..WriterOptions::default()
    };
    let inv = inventory(std::slice::from_ref(&source), &strict()).unwrap();
    assert!(inv.sources[0].system_update_bytes.unwrap() > 17 * 1024 * 1024);
    convert_inventory(&inv, &nx, &strict(), IntegrityPolicy::Strict, &options).unwrap();
    assert_eq!(
        u16::from_le_bytes(fs::read(&nx).unwrap()[6..8].try_into().unwrap()),
        3
    );
    let resources = inspect_source_resources(&nx, Platform::Switch1, APP, &options).unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].0, 1);
    assert!(resources[0].1[0].size > 17 * 1024 * 1024);
    let summary = inspect_archive(&nx, &options, IntegrityPolicy::Strict).unwrap();
    let base = summary.applications[0].snapshots[0].identity;
    assert!(
        inspect_snapshot_files(&nx, base, &options, IntegrityPolicy::Fast)
            .unwrap()
            .iter()
            .all(|f| f.path.ends_with(".nca"))
    );
    let (recipe, stats) = build_recipe(&source, &nx, &options).unwrap();
    assert!(stats.recipe_bytes < 8192, "{stats:?}");
    assert!(stats.unmatched_entries.is_empty());
    embed_recipes(&nx, vec![recipe], &options).unwrap();
    let update = temp.write(
        "src/update.dxci",
        &dxci(&[
            (
                "update",
                vec![("another-system.bin".into(), vec![0x55; 3000])],
            ),
            ("secure", title_entries(&TitleSpec::patch(APP, 1, 9))),
        ]),
    );
    add_native_content(
        &nx,
        &nx,
        &NativeAddRequest::new(&update, APP | 0x800, 1),
        &strict(),
        &options,
        IntegrityPolicy::Strict,
    )
    .unwrap();
    let summary = inspect_archive(&nx, &options, IntegrityPolicy::Strict).unwrap();
    assert!(summary.applications[0]
        .snapshots
        .iter()
        .any(|s| s.identity == base));
    assert_eq!(
        inspect_source_resources(&nx, Platform::Switch1, APP, &options)
            .unwrap()
            .len(),
        2,
        "native batch must retain newly imported resources"
    );
    let mut remove = RepackPlan::new();
    remove.remove_snapshots.push(
        summary.applications[0]
            .snapshots
            .iter()
            .find(|s| s.snapshot_type == SnapshotType::Update)
            .unwrap()
            .identity,
    );
    repack_archive(&nx, &nx, remove, &options, IntegrityPolicy::Strict).unwrap();
    compact_archive(&nx, &options, IntegrityPolicy::Strict).unwrap();
    let recipes = embedded_recipes(&nx, &options).unwrap();
    assert_eq!(recipes.len(), 1);
    check_embedded(&nx, &recipes[0].bytes, &options).unwrap();
    let rebuilt = temp.join("rebuilt.dxci");
    rebuild_embedded(&nx, &recipes[0].bytes, &rebuilt, &options).unwrap();
    assert_eq!(fs::read(rebuilt).unwrap(), image);
    // Exclusion is explicit and leaves game identities unchanged.
    let stripped = temp.join("no-firmware.nx");
    let omitted = WriterOptions {
        preserve_firmware: false,
        ..options
    };
    convert_inventory(
        &inv,
        &stripped,
        &strict(),
        IntegrityPolicy::Strict,
        &omitted,
    )
    .unwrap();
    assert!(
        inspect_source_resources(&stripped, Platform::Switch1, APP, &omitted)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        inspect_archive(&stripped, &omitted, IntegrityPolicy::Fast)
            .unwrap()
            .applications[0]
            .snapshots[0]
            .identity,
        base
    );
    assert!(
        fs::metadata(&nx).unwrap().len() > fs::metadata(stripped).unwrap().len() + 16 * 1024 * 1024
    );
}

#[test]
fn requested_recipe_failure_never_publishes_a_pack() {
    use nxformat_packer::cli;
    let temp = TempDir::new();
    let mut state = 0x12345678u32;
    let payload: Vec<_> = (0..6 * 1024 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let mut entries = title_entries(&TitleSpec::application(APP, 0, 4));
    entries.push(("unmatched.ticket".to_owned(), payload));
    let source = temp.write("source.dnsp", &dnsp(&entries));
    let out = temp.join("must-not-exist.nx");
    let sink: cli::Sink = std::sync::Arc::new(|_, _| {});
    let result = cli::run(
        vec![
            "pack".into(),
            source.display().to_string(),
            out.display().to_string(),
        ],
        Some(sink),
        None,
    );
    assert!(result.is_err());
    assert!(!out.exists());
    assert_eq!(
        fs::read_dir(&temp.0).unwrap().count(),
        1,
        "temporary outputs must be removed"
    );
}
