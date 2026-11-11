//! The everyday workflows, run through the nxconvert commands exactly as the
//! NXConvert window and NXEmu run them, on small synthetic games: DNSP, DXCI,
//! NUP and extracted exefs/romfs folders, with updates and DLC.
use nxformat_packer::cli;
use nxformat_packer::import::synthetic::executable_tree;
use nxformat_packer::import::synthetic_nup;
use nxformat_packer::native::synthetic::{
    dnsp, dxci, sparse_executable, title_entries, Entries, TitleSpec,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A Switch 1 application ID.
const APP: u64 = 0x0100_0000_0001_0000;
const DLC: u64 = APP + 0x1001;
const UPDATE: u32 = 65536;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("nx-workflow-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.0.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        p
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn s(path: &Path) -> String {
    path.display().to_string()
}

/// Runs one command; returns its output lines (not the diagnostics).
fn run(args: &[String]) -> Result<Vec<String>, String> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let store = lines.clone();
    let sink: cli::Sink = Arc::new(move |error, line: &str| {
        if !error {
            store.lock().unwrap().push(line.to_owned());
        }
    });
    cli::run(args.to_vec(), Some(sink), None).map_err(|e| e.to_string())?;
    let out = lines.lock().unwrap().clone();
    Ok(out)
}

macro_rules! cmd {
    ($($a:expr),* $(,)?) => { run(&[$(String::from($a)),*]) };
}

/// (kind, version, identity) of every snapshot, in archive order.
fn snapshots(nx: &Path) -> Vec<(String, u32, String)> {
    cmd!("contents", s(nx))
        .unwrap()
        .iter()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f[0] == "@SNAPSHOT").then(|| (f[2].to_owned(), f[5].parse().unwrap(), f[1].to_owned()))
        })
        .collect()
}

fn kinds(nx: &Path) -> Vec<(String, u32)> {
    let mut k: Vec<(String, u32)> = snapshots(nx).into_iter().map(|(k, v, _)| (k, v)).collect();
    k.sort();
    k
}

/// (recipe id, original file name) kept in the NX.
fn recipes(nx: &Path) -> Vec<(String, String)> {
    cmd!("contents", s(nx))
        .unwrap()
        .iter()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f[0] == "@RECIPE").then(|| (f[1].to_owned(), f[5].to_owned()))
        })
        .collect()
}

fn verify(nx: &Path) {
    cmd!("verify", s(nx)).unwrap();
}

fn pair(kind: &str, version: u32) -> (String, u32) {
    (kind.to_owned(), version)
}

/// A ticket and a certificate, which the NX does not store: only a recipe
/// brings them back.
fn with_ticket(mut entries: Entries, seed: u8) -> Entries {
    entries.push((
        format!("{:032x}.tik", APP as u128 + seed as u128),
        vec![seed; 0x2c0],
    ));
    entries.push((
        format!("{:032x}.cert", APP as u128 + seed as u128),
        vec![seed ^ 0x5a; 0x700],
    ));
    entries
}

fn base_nsp(seed: u8) -> Vec<u8> {
    dnsp(&with_ticket(
        title_entries(&TitleSpec::application(APP, 0, seed)),
        seed,
    ))
}
fn update_nsp(version: u32, seed: u8) -> Vec<u8> {
    dnsp(&with_ticket(
        title_entries(&TitleSpec::patch(APP, version, seed)),
        seed,
    ))
}
fn dlc_nsp(seed: u8) -> Vec<u8> {
    dnsp(&title_entries(&TitleSpec::add_on(APP, DLC, 0, seed)))
}

/// Rebuilds every original kept in `nx` into `folder` and checks each one
/// against the file it came from.
fn rebuild_all(nx: &Path, folder: &Path, originals: &[&Path]) {
    fs::create_dir_all(folder).unwrap();
    cmd!("check-recipe", s(nx)).unwrap();
    let lines = cmd!("rebuild", s(nx), s(folder), "--frontend").unwrap();
    let rebuilt = lines.iter().filter(|l| l.starts_with("@REBUILT\t")).count();
    assert_eq!(rebuilt, originals.len(), "{lines:?}");
    for original in originals {
        let copy = folder.join(original.file_name().unwrap());
        assert_eq!(
            fs::read(&copy).unwrap(),
            fs::read(original).unwrap(),
            "{}",
            copy.display()
        );
    }
}

#[test]
fn dnsp_to_nx_and_back() {
    let t = Temp::new();
    let source = t.write("in/Game.nsp", &base_nsp(1));
    let nx = t.path("Game.nx");
    let out = cmd!("pack", s(&source), s(&nx), "--recipe", "--frontend").unwrap();
    assert!(out.iter().any(|l| l.starts_with("@TOTAL\t")), "{out:?}");
    assert!(out.iter().any(|l| l.starts_with("PASS ")), "{out:?}");
    assert_eq!(kinds(&nx), [pair("base", 0)]);
    verify(&nx);
    assert_eq!(recipes(&nx).len(), 1);
    rebuild_all(&nx, &t.path("back"), &[&source]);
    // The output is never overwritten.
    assert!(cmd!("pack", s(&source), s(&nx)).is_err());
}

#[test]
fn dxci_with_its_update_to_nx_and_back() {
    let t = Temp::new();
    let mut secure = title_entries(&TitleSpec::application(APP, 0, 2));
    secure.extend(title_entries(&TitleSpec::patch(APP, UPDATE, 3)));
    let mut card = dxci(&[
        ("update", vec![("firmware.bin".to_owned(), vec![7u8; 3000])]),
        ("normal", Vec::new()),
        ("secure", secure),
    ]);
    card.resize(card.len() + 1024 * 1024, 0xff); // untrimmed card
    let source = t.write("in/Game.xci", &card);
    let nx = t.path("Game.nx");
    cmd!("pack", s(&source), s(&nx), "--recipe").unwrap();
    assert_eq!(kinds(&nx), [pair("base", 0), pair("update", UPDATE)]);
    verify(&nx);
    rebuild_all(&nx, &t.path("back"), &[&source]);
}

#[test]
fn game_update_and_dlc_files_make_one_nx() {
    let t = Temp::new();
    let base = t.write("in/Game.nsp", &base_nsp(1));
    let update = t.write("in/Game [v1].nsp", &update_nsp(UPDATE, 2));
    let dlc = t.write("in/Game [DLC].nsp", &dlc_nsp(3));
    let scan = cmd!("scan", s(&t.path("in"))).unwrap();
    let found: Vec<&str> = scan
        .iter()
        .filter(|l| l.starts_with("@CONTENT\t"))
        .map(|l| l.split('\t').nth(2).unwrap())
        .collect();
    assert_eq!(found.len(), 3, "{scan:?}");
    for kind in ["base", "update", "dlc"] {
        assert!(found.contains(&kind), "{scan:?}");
    }
    let nx = t.path("Game.nx");
    cmd!("pack", s(&base), s(&update), s(&dlc), s(&nx), "--recipe").unwrap();
    assert_eq!(
        kinds(&nx),
        [pair("base", 0), pair("dlc", 0), pair("update", UPDATE)]
    );
    verify(&nx);
    rebuild_all(&nx, &t.path("back"), &[&base, &update, &dlc]);
}

#[test]
fn add_update_and_dlc_then_remove_and_compact() {
    let t = Temp::new();
    let base = t.write("in/Game.nsp", &base_nsp(1));
    let update = t.write("in/Game [v1].nsp", &update_nsp(UPDATE, 2));
    let newer = t.write("in/Game [v2].nsp", &update_nsp(2 * UPDATE, 4));
    let dlc = t.write("in/Game [DLC].nsp", &dlc_nsp(3));
    let nx = t.path("Game.nx");
    cmd!("pack", s(&base), s(&nx), "--recipe").unwrap();

    let added = cmd!("add", s(&nx), s(&update), "--recipe", "--frontend").unwrap();
    assert!(
        added.iter().any(|l| l.starts_with("@ADDED\tupdate\t")),
        "{added:?}"
    );
    assert_eq!(kinds(&nx), [pair("base", 0), pair("update", UPDATE)]);

    // The same update again is skipped, not stored twice.
    let again = cmd!("add", s(&nx), s(&update)).unwrap();
    assert!(
        again.iter().any(|l| l.starts_with("@SKIPPED\t")),
        "{again:?}"
    );
    assert_eq!(snapshots(&nx).len(), 2);

    cmd!("add", s(&nx), s(&dlc), s(&newer), "--recipe").unwrap();
    assert_eq!(
        kinds(&nx),
        [
            pair("base", 0),
            pair("dlc", 0),
            pair("update", UPDATE),
            pair("update", 2 * UPDATE)
        ]
    );
    verify(&nx);
    // A base cannot be added: an NX holds one game.
    assert!(cmd!("add", s(&nx), s(&base)).is_err());

    // Remove the first update in place, then reclaim its space.
    let first = snapshots(&nx)
        .into_iter()
        .find(|(k, v, _)| k == "update" && *v == UPDATE)
        .unwrap()
        .2;
    let before = fs::metadata(&nx).unwrap().len();
    cmd!("remove", s(&nx), first, "--frontend").unwrap();
    assert!(fs::metadata(&nx).unwrap().len() > before);
    let contents = cmd!("contents", s(&nx)).unwrap();
    let reclaimable = contents
        .iter()
        .find_map(|line| line.strip_prefix("@RECLAIMABLE\t"))
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(reclaimable > 0);
    assert_eq!(
        kinds(&nx),
        [pair("base", 0), pair("dlc", 0), pair("update", 2 * UPDATE)]
    );
    verify(&nx);
    cmd!("compact", s(&nx), "--frontend").unwrap();
    assert!(fs::metadata(&nx).unwrap().len() < before);
    assert!(cmd!("contents", s(&nx))
        .unwrap()
        .iter()
        .any(|line| line == "@RECLAIMABLE\t0"));
    verify(&nx);

    // The originals still in the NX rebuild; the removed update's does not.
    let names: Vec<String> = recipes(&nx).into_iter().map(|(_, n)| n).collect();
    assert!(!names.iter().any(|n| n == "Game [v1].nsp"), "{names:?}");
    let back = t.path("back");
    fs::create_dir_all(&back).unwrap();
    for (id, name) in recipes(&nx) {
        cmd!("rebuild", s(&nx), s(&back), id).unwrap();
        let original = t.path("in").join(&name);
        assert_eq!(
            fs::read(back.join(&name)).unwrap(),
            fs::read(&original).unwrap(),
            "{name}"
        );
    }
    assert_eq!(fs::read_dir(&back).unwrap().count(), 3);
}

#[test]
fn an_update_alone_is_refused() {
    let t = Temp::new();
    let update = t.write("in/Game [v1].nsp", &update_nsp(UPDATE, 2));
    let nx = t.path("Game.nx");
    assert!(cmd!("pack", s(&update), s(&nx)).is_err());
    assert!(!nx.exists());
}

#[test]
fn nup_to_nx() {
    use synthetic_nup::{addon, base, build, update};
    let t = Temp::new();
    let nup = t.write(
        "in/Game.nup",
        &build(&[base(APP), update(APP, UPDATE, 0), addon(APP)], true),
    );
    let nx = t.path("Game.nx");
    assert!(cmd!("pack-nup", s(&nup), s(&nx)).is_err());
    assert!(!nx.exists());
    cmd!("pack-nup", s(&nup), s(&nx), "--platform", "switch2").unwrap();
    assert_eq!(
        kinds(&nx),
        [pair("base", 0), pair("dlc", 0), pair("update", UPDATE)]
    );
    verify(&nx);
    // What the import presumed is kept beside the archive.
    assert!(t.path("Game.nx.nximport").exists());
    let assumptions = fs::read_to_string(t.path("Game.nx.assumptions.txt")).unwrap();
    assert!(assumptions.contains("DECLARED: platform Switch2"));

    let without = t.path("Game without DLC.nx");
    cmd!(
        "pack-nup",
        s(&nup),
        s(&without),
        "--platform",
        "switch2",
        "--skip-dlc"
    )
    .unwrap();
    assert_eq!(kinds(&without), [pair("base", 0), pair("update", UPDATE)]);
}

#[test]
fn extracted_exefs_romfs_folder_to_nx() {
    let t = Temp::new();
    let game = t.path("Game");
    executable_tree(&game.join("base"), APP, 0x41).unwrap();
    fs::write(game.join("base/romfs/level.bin"), vec![3u8; 70_000]).unwrap();
    executable_tree(&game.join(format!("updates/{UPDATE}")), APP, 0x42).unwrap();
    fs::write(
        game.join(format!("updates/{UPDATE}/romfs/level.bin")),
        vec![4u8; 70_000],
    )
    .unwrap();
    let dlc = game.join(format!("dlc/{DLC:016x}/romfs"));
    fs::create_dir_all(&dlc).unwrap();
    fs::write(dlc.join("extra.bin"), b"synthetic DLC data").unwrap();
    let nx = t.path("Game.nx");
    cmd!("pack-tree", s(&game), s(&nx), "--platform", "switch1").unwrap();
    assert_eq!(
        kinds(&nx),
        [pair("base", 0), pair("dlc", 0), pair("update", UPDATE)]
    );
    verify(&nx);
    // A 01xx Program ID with this NPDM is not proof of Switch 1: the caller
    // says so. An exefs/romfs folder alone is a base game too.
    let single = t.path("Single.nx");
    cmd!(
        "pack-tree",
        s(&game.join("base")),
        s(&single),
        "--platform",
        "switch1"
    )
    .unwrap();
    assert_eq!(kinds(&single), [pair("base", 0)]);
}

// Lichii runs each conversion with --frontend (and --no-trail for NUPs and
// folders) and draws a progress bar from @TOTAL and @PROGRESS.

/// Runs one command as Lichii does; returns (sum of @TOTAL, every @PROGRESS).
fn frontend(args: &[String]) -> (u64, Vec<u64>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let store = lines.clone();
    let sink: cli::Sink = Arc::new(move |error, line: &str| {
        if !error {
            store.lock().unwrap().push(line.to_owned());
        }
    });
    // An in-process token: --frontend would otherwise cancel when stdin closes.
    let token = nxformat_packer::CancellationToken::default();
    cli::run(args.to_vec(), Some(sink), Some(token)).unwrap();
    let lines = lines.lock().unwrap();
    let total = lines
        .iter()
        .filter_map(|l| l.strip_prefix("@TOTAL\t"))
        .map(|v| v.parse::<u64>().unwrap())
        .sum();
    let progress = lines
        .iter()
        .filter_map(|l| l.strip_prefix("@PROGRESS\t"))
        .map(|v| v.parse::<u64>().unwrap())
        .collect();
    (total, progress)
}

macro_rules! frontend {
    ($($a:expr),* $(,)?) => { frontend(&[$(String::from($a)),*]) };
}

/// The bar starts, never goes past its total and ends exactly at 100 %.
fn assert_full_bar((total, progress): (u64, Vec<u64>), what: &str) {
    assert!(total > 0, "{what}: no @TOTAL");
    assert!(
        progress.iter().all(|&p| p <= total),
        "{what}: {progress:?} > {total}"
    );
    assert_eq!(
        progress.last(),
        Some(&total),
        "{what}: {progress:?} / {total}"
    );
}

/// Files in `folder`, sorted.
fn listing(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(folder)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn lichii_package_conversion_bar_ends_at_100_percent() {
    let t = Temp::new();
    let base = t.write("in/Game.nsp", &base_nsp(1));
    let update = t.write("in/Game [v1].nsp", &update_nsp(UPDATE, 2));
    let dlc = t.write("in/Game [DLC].nsp", &dlc_nsp(3));
    let nx = t.path("out/Game.nx");
    fs::create_dir_all(t.path("out")).unwrap();
    assert_full_bar(frontend!("pack", s(&base), s(&nx), "--frontend"), "pack");
    // Update and DLC in one command: one bar, not one total per file.
    assert_full_bar(
        frontend!("add", s(&nx), s(&update), s(&dlc), "--frontend"),
        "add",
    );
    assert_eq!(
        kinds(&nx),
        [pair("base", 0), pair("dlc", 0), pair("update", UPDATE)]
    );
    verify(&nx);
    assert_eq!(listing(&t.path("out")), ["Game.nx"]);
}

#[test]
fn lichii_nup_conversion_writes_only_the_archive() {
    use synthetic_nup::{addon, base, build, update};
    let t = Temp::new();
    let nup = t.write(
        "in/Game.nup",
        &build(&[base(APP), update(APP, UPDATE, 0), addon(APP)], true),
    );
    fs::create_dir_all(t.path("out")).unwrap();
    let nx = t.path("out/Game.nx");
    assert_full_bar(
        frontend!(
            "pack-nup",
            s(&nup),
            s(&nx),
            "--platform",
            "switch2",
            "--frontend",
            "--no-trail"
        ),
        "pack-nup",
    );
    assert_eq!(
        kinds(&nx),
        [pair("base", 0), pair("dlc", 0), pair("update", UPDATE)]
    );
    verify(&nx);
    // No .nximport, .assumptions.txt or stage left beside it.
    assert_eq!(listing(&t.path("out")), ["Game.nx"]);
}

#[test]
fn lichii_folder_conversion_writes_only_the_archive() {
    let t = Temp::new();
    let game = t.path("in/Game");
    executable_tree(&game.join("base"), APP, 0x41).unwrap();
    executable_tree(&game.join(format!("updates/{UPDATE}")), APP, 0x42).unwrap();
    fs::create_dir_all(t.path("out")).unwrap();
    let nx = t.path("out/Game.nx");
    assert_full_bar(
        frontend!(
            "pack-tree",
            s(&game.join("base")),
            s(&nx),
            "--platform",
            "switch1",
            "--frontend",
            "--no-trail"
        ),
        "pack-tree",
    );
    assert_full_bar(
        frontend!(
            "add-tree",
            s(&nx),
            s(&game.join(format!("updates/{UPDATE}"))),
            "--platform",
            "switch1",
            "--update-version",
            UPDATE.to_string(),
            "--frontend",
            "--no-trail"
        ),
        "add-tree",
    );
    assert_eq!(kinds(&nx), [pair("base", 0), pair("update", UPDATE)]);
    verify(&nx);
    assert_eq!(listing(&t.path("out")), ["Game.nx"]);
    // Without --no-trail the review files are still written.
    let kept = t.path("kept/Game.nx");
    fs::create_dir_all(t.path("kept")).unwrap();
    cmd!("pack-tree", s(&game), s(&kept), "--platform", "switch1").unwrap();
    assert!(listing(&t.path("kept")).len() > 1);
}

#[test]
fn switch2_folder_with_zbic_executable_converts() {
    use nxformat_packer::import::synthetic::nso_zbic;
    let t = Temp::new();
    let game = t.path("in/Game");
    executable_tree(&game, APP, 0x41).unwrap();
    // Switch 2 games such as Zelda Ocarina of Time ship a ZBIC main.
    let main = nso_zbic(0x41);
    fs::write(game.join("exefs/main"), &main).unwrap();
    let nx = t.path("Game.nx");
    cmd!(
        "pack-tree",
        s(&game),
        s(&nx),
        "--platform",
        "switch2",
        "--no-trail"
    )
    .unwrap();
    verify(&nx);
    assert_eq!(kinds(&nx), [pair("base", 0)]);
}

/// The archive generation: each committed edit publishes a new one. Both
/// header copies are read, the newer one counts.
fn generation(nx: &Path) -> u64 {
    let bytes = fs::read(nx).unwrap();
    let at =
        |offset: usize| u64::from_le_bytes(bytes[offset + 216..offset + 224].try_into().unwrap());
    at(0).max(at(4096))
}
fn dlc_needing_update_nsp(seed: u8) -> Vec<u8> {
    let mut spec = TitleSpec::add_on(APP, DLC + 1, 0, seed);
    spec.required_application_version = UPDATE;
    dnsp(&title_entries(&spec))
}

#[test]
fn add_writes_an_update_and_its_dlcs_in_as_few_generations_as_possible() {
    let t = Temp::new();
    let base = t.write("in/Game.nsp", &base_nsp(1));
    let update = t.write("in/Game [v1].nsp", &update_nsp(UPDATE, 2));
    let dlc = t.write("in/Game [DLC].nsp", &dlc_nsp(3));
    let late = t.write("in/Game [DLC 2].nsp", &dlc_needing_update_nsp(4));
    let nx = t.path("Game.nx");
    cmd!("pack", s(&base), s(&nx)).unwrap();
    let packed = generation(&nx);

    // The update and the DLC that works with the base go in one write; the
    // DLC that needs the update goes in a second one. The DLC is listed twice.
    // The same update in two files (Mario Kart 8 had two v3.0.3 dumps).
    let copy = t.write("in/Game [v1] copy.nsp", &update_nsp(UPDATE, 2));
    // Recipes are kept in one more write of their own: left out here.
    let added = cmd!(
        "add",
        s(&nx),
        s(&late),
        s(&dlc),
        s(&update),
        s(&copy),
        "--no-recipe"
    )
    .unwrap();
    assert_eq!(generation(&nx), packed + 2, "{added:?}");
    assert_eq!(
        added.iter().filter(|l| l.starts_with("@ADDED\t")).count(),
        3
    );
    assert!(
        added.iter().any(|l| l.ends_with("\tlisted twice")),
        "{added:?}"
    );
    assert_eq!(
        kinds(&nx),
        [
            pair("base", 0),
            pair("dlc", 0),
            pair("dlc", 0),
            pair("update", UPDATE)
        ]
    );
    verify(&nx);

    // Without a DLC that waits for the update, everything is one write.
    let nx = t.path("Game2.nx");
    cmd!("pack", s(&base), s(&nx)).unwrap();
    let packed = generation(&nx);
    cmd!("add", s(&nx), s(&update), s(&dlc), "--no-recipe").unwrap();
    assert_eq!(generation(&nx), packed + 1);
    verify(&nx);
}

#[test]
fn add_with_one_bad_member_leaves_the_archive_unchanged() {
    let t = Temp::new();
    let base = t.write("in/Game.nsp", &base_nsp(1));
    let update = t.write("in/Game [v1].nsp", &update_nsp(UPDATE, 2));
    // A valid file, but a DLC of a game that is not in this archive.
    let other = t.write(
        "in/Other [DLC].nsp",
        &dnsp(&title_entries(&TitleSpec::add_on(
            APP + 0x2000,
            APP + 0x3001,
            0,
            5,
        ))),
    );
    let nx = t.path("Game.nx");
    cmd!("pack", s(&base), s(&nx)).unwrap();
    let before = fs::read(&nx).unwrap();
    let error = cmd!("add", s(&nx), s(&update), s(&other)).unwrap_err();
    assert!(
        error.contains("Other [DLC].nsp") && error.contains("not in this archive"),
        "{error}"
    );
    assert_eq!(
        fs::read(&nx).unwrap(),
        before,
        "the update must not be added alone"
    );
    // The good file alone still goes in.
    cmd!("add", s(&nx), s(&update)).unwrap();
    assert_eq!(kinds(&nx), [pair("base", 0), pair("update", UPDATE)]);
    verify(&nx);
}

#[test]
fn pack_writes_the_base_then_its_additions_in_one_generation() {
    let t = Temp::new();
    let base = t.write("in/Game.nsp", &base_nsp(1));
    let update = t.write("in/Game [v1].nsp", &update_nsp(UPDATE, 2));
    let dlc = t.write("in/Game [DLC].nsp", &dlc_nsp(3));
    let dlc2 = t.write(
        "in/Game [DLC 2].nsp",
        &dnsp(&title_entries(&TitleSpec::add_on(APP, DLC + 2, 0, 6))),
    );
    let late = t.write("in/Game [DLC 3].nsp", &dlc_needing_update_nsp(4));

    let alone = t.path("Base.nx");
    cmd!("pack", s(&base), s(&alone)).unwrap();
    let nx = t.path("Game.nx");
    cmd!("pack", s(&base), s(&update), s(&dlc), s(&dlc2), s(&nx)).unwrap();
    assert_eq!(generation(&nx), generation(&alone) + 1);
    verify(&nx);

    let nx = t.path("Game3.nx");
    cmd!("pack", s(&base), s(&update), s(&dlc), s(&late), s(&nx)).unwrap();
    assert_eq!(generation(&nx), generation(&alone) + 2);
    assert_eq!(kinds(&nx).len(), 4);
    verify(&nx);
}

#[test]
fn dlc_that_needs_a_missing_update_is_refused_with_its_name() {
    let t = Temp::new();
    let base = t.write("in/Game.nsp", &base_nsp(1));
    let dlc = t.write("in/Game [DLC].nsp", &dlc_nsp(3));
    let late = t.write("in/Game [DLC 2].nsp", &dlc_needing_update_nsp(4));
    let nx = t.path("Game.nx");
    cmd!("pack", s(&base), s(&nx)).unwrap();
    let before = fs::read(&nx).unwrap();
    let error = cmd!("add", s(&nx), s(&dlc), s(&late)).unwrap_err();
    assert!(
        error.contains("Game [DLC 2].nsp") && error.contains(&format!("needs version {UPDATE}")),
        "{error}"
    );
    assert_eq!(fs::read(&nx).unwrap(), before);
}

/// Early game cards also copy the control NCA and the CNMT into the `normal`
/// partition (New Super Mario Bros. U Deluxe). Identical copies are one
/// content; a copy that differs is still refused.
#[test]
fn card_with_copies_in_the_normal_partition_converts() {
    let t = Temp::new();
    let secure = title_entries(&TitleSpec::application(APP, 0, 2));
    let normal: Entries = secure
        .iter()
        .filter(|(name, bytes)| name.ends_with(".cnmt.nca") || bytes[0x205] == 2)
        .cloned()
        .collect();
    assert_eq!(normal.len(), 2);
    let card = |normal: Entries| {
        dxci(&[
            ("update", Vec::new()),
            ("normal", normal),
            ("secure", secure.clone()),
        ])
    };
    let source = t.write("in/Game.xci", &card(normal.clone()));
    let nx = t.path("Game.nx");
    cmd!("pack", s(&source), s(&nx), "--recipe").unwrap();
    assert_eq!(kinds(&nx), [pair("base", 0)]);
    verify(&nx);
    rebuild_all(&nx, &t.path("back"), &[&source]);

    let mut changed = normal;
    let last = changed[0].1.len() - 1;
    changed[0].1[last] ^= 1;
    let source = t.write("in/Changed.xci", &card(changed));
    assert!(cmd!("pack", s(&source), s(&t.path("Changed.nx"))).is_err());
}

/// A folder downloaded from the console over FTP has a `.listing` file in
/// every folder, also in the top folder and in updates/ (Kirby Air Riders,
/// Mario Kart World). None of them is game data: the archive is the same
/// as without them.
#[test]
fn ftp_listing_files_are_left_out_of_a_folder_conversion() {
    let t = Temp::new();
    let make = |game: &Path, listings: bool| {
        executable_tree(&game.join("base"), APP, 0x41).unwrap();
        fs::write(game.join("base/romfs/level.bin"), vec![3u8; 70_000]).unwrap();
        executable_tree(&game.join(format!("updates/{UPDATE}")), APP, 0x42).unwrap();
        if listings {
            let listing = b"dr-xr-xr-x 1 ftp ftp 0 Jan 01  1970 base\r\n".to_vec();
            for at in [
                "",
                "base",
                "base/romfs",
                "updates",
                &format!("updates/{UPDATE}"),
            ] {
                fs::write(game.join(at).join(".listing"), &listing).unwrap();
            }
        }
    };
    let plain = t.path("Plain");
    make(&plain, false);
    let ftp = t.path("Ftp");
    make(&ftp, true);
    let (a, b) = (t.path("Plain.nx"), t.path("Ftp.nx"));
    cmd!(
        "pack-tree",
        s(&plain),
        s(&a),
        "--platform",
        "switch2",
        "--no-trail"
    )
    .unwrap();
    cmd!(
        "pack-tree",
        s(&ftp),
        s(&b),
        "--platform",
        "switch2",
        "--no-trail"
    )
    .unwrap();
    verify(&b);
    assert_eq!(snapshots(&b), snapshots(&a));
    assert_eq!(kinds(&b), [pair("base", 0), pair("update", UPDATE)]);
}

/// A base shipped sparse, as NXConverter writes Castlevania Belmont's Curse
/// or SHINOBI; `marked` adds the decrypter's sparse marker.
fn sparse_base_nsp(seed: u8, marked: bool) -> Vec<u8> {
    let mut entries = title_entries(&TitleSpec::application(APP, 0, seed));
    sparse_executable(&mut entries, marked);
    dnsp(&with_ticket(entries, seed))
}

#[test]
fn sparse_base_converts_and_runs_with_its_update() {
    let t = Temp::new();
    let base = t.write("in/Game.nsp", &sparse_base_nsp(1, true));
    let update = t.write("in/Game [v1].nsp", &update_nsp(UPDATE, 2));
    // Alone, as Lichii packs it before adding the rest.
    let nx = t.path("Game.nx");
    cmd!("pack", s(&base), s(&nx)).unwrap();
    verify(&nx);
    cmd!("add", s(&nx), s(&update)).unwrap();
    verify(&nx);
    assert_eq!(kinds(&nx), vec![pair("base", 0), pair("update", UPDATE)]);
    // In one command, and back to the original files.
    let both = t.path("Both.nx");
    cmd!("pack", s(&base), s(&update), s(&both), "--recipe").unwrap();
    verify(&both);
    rebuild_all(&both, &t.path("out"), &[&base, &update]);
}

#[test]
fn damaged_executable_without_the_sparse_marker_is_refused() {
    let t = Temp::new();
    let base = t.write("in/Game.nsp", &sparse_base_nsp(1, false));
    let nx = t.path("Game.nx");
    let error = cmd!("pack", s(&base), s(&nx)).unwrap_err();
    assert!(
        error.contains("Build ID") || error.contains("ExeFS"),
        "{error}"
    );
    assert!(!nx.exists());
}

#[test]
fn base_with_its_update_version_number_still_takes_the_update_and_dlc() {
    // Castlevania Belmont's Curse: base and update both say version 65536.
    let t = Temp::new();
    let base = t.write(
        "in/Game.nsp",
        &dnsp(&with_ticket(
            title_entries(&TitleSpec::application(APP, UPDATE, 1)),
            1,
        )),
    );
    let update = t.write("in/Game [v1].nsp", &update_nsp(UPDATE, 2));
    let dlc = t.write("in/Game [DLC].nsp", &dlc_nsp(3));
    let nx = t.path("Game.nx");
    cmd!("pack", s(&base), s(&nx)).unwrap();
    let out = cmd!("add", s(&nx), s(&update), s(&dlc)).unwrap();
    assert!(!out.iter().any(|l| l.starts_with("@SKIPPED")), "{out:?}");
    assert_eq!(
        kinds(&nx),
        vec![pair("base", UPDATE), pair("dlc", 0), pair("update", UPDATE)]
    );
    verify(&nx);
    // The DLC added later fits the base and the update alike: it takes the base.
    let nx = t.path("Later.nx");
    cmd!("pack", s(&base), s(&nx)).unwrap();
    cmd!("add", s(&nx), s(&update)).unwrap();
    cmd!("add", s(&nx), s(&dlc)).unwrap();
    assert_eq!(kinds(&nx).len(), 3);
    verify(&nx);
}

#[test]
fn pack_keeps_a_recipe_unless_told_not_to() {
    let t = Temp::new();
    let base = t.write("in/Game.nsp", &base_nsp(1));
    let recipes = |nx: &Path| {
        cmd!("contents", s(nx))
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("@RECIPE\t"))
            .count()
    };
    let nx = t.path("Game.nx");
    cmd!("pack", s(&base), s(&nx)).unwrap();
    assert_eq!(recipes(&nx), 1);
    cmd!("check-recipe", s(&nx)).unwrap();
    let bare = t.path("Bare.nx");
    cmd!("pack", s(&base), s(&bare), "--no-recipe").unwrap();
    assert_eq!(recipes(&bare), 0);
    // Only pack and add take a recipe flag.
    assert!(cmd!("compact", s(&bare), "--recipe").is_err());
}

#[test]
fn no_compress_writes_raw_blocks_for_every_import_path() {
    use nxformat_packer::{
        inspect_archive, inspect_snapshot_files, IntegrityPolicy, WriterOptions,
    };
    let t = Temp::new();
    let native = t.write("native/game.dnsp", &base_nsp(0x41));
    let nup = t.write(
        "nup/game.nup",
        &synthetic_nup::build(&[synthetic_nup::base(APP)], true),
    );
    let tree = t.path("tree/Game/base");
    executable_tree(&tree, APP, 0x41).unwrap();
    fs::write(tree.join("romfs/zeros.bin"), vec![0; 256 * 1024]).unwrap();
    let options = WriterOptions::default();
    for (command, source, extra) in [
        ("pack", native, vec!["--no-recipe"]),
        ("pack-nup", nup, vec!["--platform", "switch2", "--no-trail"]),
        (
            "pack-tree",
            t.path("tree/Game"),
            vec!["--platform", "switch1", "--no-trail"],
        ),
    ] {
        let mut identities = Vec::new();
        for raw in [false, true] {
            let nx = t.path(&format!("{command}-{raw}.nx"));
            let mut args = vec![command.to_string(), s(&source), s(&nx)];
            args.extend(extra.iter().map(|a| (*a).to_string()));
            if raw {
                // RAW wins even when compression settings occur after the flag.
                args.extend(["--no-compress", "--level", "13", "--workers", "4"].map(String::from));
            }
            run(&args).unwrap();
            let summary = inspect_archive(&nx, &options, IntegrityPolicy::Strict).unwrap();
            let snapshots: Vec<_> = summary
                .applications
                .iter()
                .flat_map(|a| &a.snapshots)
                .collect();
            identities.push(snapshots.iter().map(|s| s.identity).collect::<Vec<_>>());
            let mut blocks = Vec::new();
            for snapshot in snapshots {
                let files =
                    inspect_snapshot_files(&nx, snapshot.identity, &options, IntegrityPolicy::Fast)
                        .unwrap();
                blocks.extend(files.into_iter().flat_map(|f| f.blocks));
            }
            assert!(!blocks.is_empty(), "{command}");
            if raw {
                assert!(
                    blocks
                        .iter()
                        .all(|b| b.codec == 0 && b.stored_size == b.logical_size),
                    "{command}"
                );
            } else {
                assert!(
                    blocks.iter().any(|b| b.codec == 1),
                    "test input must exercise compression: {command}"
                );
            }
        }
        assert_eq!(
            identities[0], identities[1],
            "{command}: compression must not change content identity"
        );
    }
}

#[test]
fn compact_no_compress_removes_existing_compression_without_original_sources() {
    use nxformat_packer::{
        inspect_archive, inspect_snapshot_files, IntegrityPolicy, WriterOptions,
    };
    let t = Temp::new();
    let sources = [
        t.write("in/base.dnsp", &base_nsp(0x41)),
        t.write("in/update.dnsp", &update_nsp(UPDATE, 0x42)),
        t.write("in/dlc.dnsp", &dlc_nsp(0x43)),
    ];
    let originals: Vec<_> = sources.iter().map(|p| fs::read(p).unwrap()).collect();
    let nx = t.path("game.nx");
    cmd!(
        "pack",
        s(&sources[0]),
        s(&sources[1]),
        s(&sources[2]),
        s(&nx)
    )
    .unwrap();
    let options = WriterOptions::default();
    let before = inspect_archive(&nx, &options, IntegrityPolicy::Strict).unwrap();
    let block_codecs = |archive: &Path| {
        let mut blocks = std::collections::BTreeMap::new();
        for app in &before.applications {
            for snapshot in &app.snapshots {
                for file in inspect_snapshot_files(
                    archive,
                    snapshot.identity,
                    &options,
                    IntegrityPolicy::Fast,
                )
                .unwrap()
                {
                    for block in file.blocks {
                        blocks.insert(block.index, block.codec);
                    }
                }
            }
        }
        blocks
    };
    let compressed = block_codecs(&nx);
    assert!(compressed.values().any(|&codec| codec == 1));
    for source in &sources {
        fs::remove_file(source).unwrap();
    }
    cmd!("compact", s(&nx), "--no-compress").unwrap();
    let after = inspect_archive(&nx, &options, IntegrityPolicy::Strict).unwrap();
    assert_eq!(
        before, after,
        "snapshots, metadata, dependencies and recipes must survive"
    );
    let raw = block_codecs(&nx);
    assert_eq!(
        compressed.len(),
        raw.len(),
        "shared blocks must remain shared"
    );
    assert!(raw.values().all(|&codec| codec == 0));
    cmd!("check-recipe", s(&nx)).unwrap();
    let rebuilt = t.path("rebuilt");
    fs::create_dir_all(&rebuilt).unwrap();
    cmd!("rebuild", s(&nx), s(&rebuilt)).unwrap();
    for (source, original) in sources.iter().zip(originals) {
        assert_eq!(
            fs::read(rebuilt.join(source.file_name().unwrap())).unwrap(),
            original
        );
    }
    // Repeating the operation on an already RAW archive preserves logical state.
    cmd!("compact", s(&nx), "--no-compress").unwrap();
    assert_eq!(
        after,
        inspect_archive(&nx, &options, IntegrityPolicy::Strict).unwrap()
    );
}

#[test]
fn macos_metadata_does_not_change_extracted_archive_or_source_verification() {
    let t = Temp::new();
    let game = t.path("Game");
    executable_tree(&game.join("base"), APP, 0x41).unwrap();
    fs::write(game.join("base/romfs/game"), b"real game file").unwrap();
    fs::write(game.join("base/romfs/._game"), b"similarly named game file").unwrap();
    let clean = t.path("clean.nx");
    let clean_bar = frontend!(
        "pack-tree",
        s(&game),
        s(&clean),
        "--platform",
        "switch2",
        "--frontend"
    );
    assert_full_bar(clean_bar.clone(), "clean extracted tree");
    let mut metadata = vec![0; 42];
    metadata[..8].copy_from_slice(&[0, 5, 22, 7, 0, 2, 0, 0]);
    metadata[24..26].copy_from_slice(&1u16.to_be_bytes());
    metadata[26..30].copy_from_slice(&9u32.to_be_bytes());
    metadata[30..34].copy_from_slice(&38u32.to_be_bytes());
    metadata[34..38].copy_from_slice(&4u32.to_be_bytes());
    fs::write(game.join("base/._romfs"), &metadata).unwrap();
    fs::write(game.join("base/romfs/._data.bin"), &metadata).unwrap();
    fs::write(game.join("base/.DS_Store"), b"\0\0\0\x01Bud1synthetic").unwrap();
    fs::write(
        game.join("base/romfs/.DS_Store"),
        b"\0\0\0\x01Bud1synthetic",
    )
    .unwrap();
    let archive = t.path("with-host-metadata.nx");
    let metadata_bar = frontend!(
        "pack-tree",
        s(&game),
        s(&archive),
        "--platform",
        "switch2",
        "--frontend"
    );
    assert_eq!(clean_bar.0, metadata_bar.0);
    assert_full_bar(metadata_bar, "tree with macOS metadata");
    assert_eq!(fs::read(&clean).unwrap(), fs::read(&archive).unwrap());
    verify(&archive);
    cmd!(
        "verify",
        s(&archive),
        s(&game),
        format!("{}.nxreceipt", archive.display())
    )
    .unwrap();
    assert!(game.join("base/._romfs").is_file());
    assert!(game.join("base/.DS_Store").is_file());
}
