//! Hostile NUP inputs, after NUP issues #24 (names that escape the output
//! folder) and #27 (cycles, repeated nodes, amplification). Every NUP entry
//! point (`convert_nup`/`stage_nup`, streamed or not, and `check_nup`) must
//! refuse them before anything is written outside its private stage, and
//! must leave no stage behind.
use super::super::synthetic_nup::*;
use super::*;

const APP: u64 = 0x0400_0000_0000_1000;

fn work(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nx-nup-hostile-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("out")).unwrap();
    dir
}
fn options(stream: bool) -> NupOptions {
    NupOptions {
        platform: Some(crate::Platform::Switch2),
        stream,
        ..Default::default()
    }
}
fn base_with(romfs: Vec<Node>) -> Pkg {
    Pkg {
        romfs: Some(romfs),
        ..base(APP)
    }
}
/// Any archive will do: `check_nup` must fail on the NUP before matching.
fn some_archive() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/raw_only.nx")
}

/// Every entry point rejects `nup`; nothing is left in `dir/out` (the stage
/// parent) and nothing named `pwned` appears beside `dir`.
fn assert_rejected(dir: &Path, nup: &[u8], expect: &[&str]) {
    let path = dir.join("input.nup");
    fs::write(&path, nup).unwrap();
    let out = dir.join("out");
    let mut errors = Vec::new();
    for stream in [false, true] {
        let e = stage_nup(
            &path,
            &out,
            &options(stream),
            &ImportLimits::default(),
            &Default::default(),
        )
        .err()
        .expect("hostile NUP staged");
        errors.push(e.to_string());
        let e = convert_nup(
            &path,
            &out.join("game.nx"),
            &options(stream),
            &ImportLimits::default(),
            &WriterOptions::default(),
        )
        .err()
        .expect("hostile NUP converted");
        errors.push(e.to_string());
        assert_eq!(fs::read_dir(&out).unwrap().count(), 0, "stage left behind");
    }
    let e = check_nup(
        &path,
        &some_archive(),
        false,
        &ImportLimits::default(),
        &WriterOptions::default(),
    )
    .expect_err("hostile NUP checked");
    errors.push(e.to_string());
    for e in &errors {
        assert!(
            expect.iter().any(|x| e.contains(x)),
            "rejected for another reason: {e}"
        );
    }
    for escaped in [dir.join("pwned"), dir.parent().unwrap().join("pwned")] {
        assert!(!escaped.exists(), "{} written", escaped.display());
    }
}

#[test]
fn names_that_escape_or_alias_are_rejected() {
    let dir = work("names");
    for name in [
        "../pwned",
        "../../pwned",
        "..\\..\\pwned",
        "..\\pwned/../x",
        "/tmp/pwned",
        "C:\\pwned",
        "C:pwned",
        "a:pwned",
        "pwned ",
        "pwned.",
        "pwned. .",
        "..",
        ".",
        " ",
        "",
        "CON",
        "nul.txt",
        "COM1",
        "pw\0ned",
        "pwned?",
    ] {
        let nup = build(
            &[base_with(vec![Node::File(name, b"pwned".to_vec())])],
            false,
        );
        assert_rejected(
            &dir,
            &nup,
            &["invalid entry name", "unsafe", "reserved", "empty"],
        );
    }
    // The same names as directories.
    for name in ["..", "../pwned", "pwned ", "CON", "a\\b"] {
        let nup = build(
            &[base_with(vec![Node::Dir(
                name,
                vec![Node::File("x.bin", b"x".to_vec())],
            )])],
            false,
        );
        assert_rejected(
            &dir,
            &nup,
            &[
                "invalid entry name",
                "unsafe",
                "reserved",
                "empty",
                "directory child",
            ],
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// TOC layout of a NUP built by `synthetic_nup::build`.
struct Layout {
    dirs_at: usize,
    files_at: usize,
    children_at: usize,
    dir_count: u32,
}
fn layout(nup: &[u8]) -> Layout {
    let toc = &nup[SUPERBLOCK..];
    let packages = u32_at(toc, 4) as usize;
    let dir_count = u64_at(toc, 8) as usize;
    let file_count = u64_at(toc, 0x10) as usize;
    let dirs_at = 0x30 + packages * PACKAGE;
    let files_at = dirs_at + dir_count * DIRECTORY;
    Layout {
        dirs_at,
        files_at,
        children_at: files_at + file_count * FILE,
        dir_count: dir_count as u32,
    }
}
fn dir_id(nup: &[u8], name: &str) -> u32 {
    let l = layout(nup);
    let toc = &nup[SUPERBLOCK..];
    let pool = u64_at(toc, 0x18) as usize;
    (0..l.dir_count)
        .find(|&i| {
            let d = &toc[l.dirs_at + i as usize * DIRECTORY..];
            let (o, n) = (u32_at(d, 0) as usize, u32_at(d, 4) as usize);
            &toc[pool + o..pool + o + n] == name.as_bytes()
        })
        .unwrap()
}
fn file_node(nup: &[u8], name: &str) -> u32 {
    let l = layout(nup);
    let toc = &nup[SUPERBLOCK..];
    let pool = u64_at(toc, 0x18) as usize;
    let count = (l.children_at - l.files_at) / FILE;
    let i = (0..count)
        .find(|&i| {
            let f = &toc[l.files_at + i * FILE..];
            let (o, n) = (u32_at(f, 0) as usize, u32_at(f, 4) as usize);
            &toc[pool + o..pool + o + n] == name.as_bytes()
        })
        .unwrap();
    l.dir_count + i as u32
}
/// Point the first child slot of directory `dir` at `node`, then re-hash the
/// TOC so only the graph is wrong.
fn retarget(nup: &mut [u8], dir: u32, node: u32) {
    let l = layout(nup);
    let row = SUPERBLOCK + l.dirs_at + dir as usize * DIRECTORY;
    let first = u32_at(nup, row + 8) as usize;
    let slot = SUPERBLOCK + l.children_at + first * 4;
    nup[slot..slot + 4].copy_from_slice(&node.to_le_bytes());
    let toc_size = u64_at(nup, 0x10) as usize;
    let digest = crate::hash::sha256(&nup[SUPERBLOCK..SUPERBLOCK + toc_size]);
    nup[0x18..0x38].copy_from_slice(&digest);
}

/// Simulate the extra 14 child slots reported by a Lichii-produced Kirby NUP.
/// This alters only the TOC directory count and keeps its SHA-256 valid.
fn unused_tail_slots(mut nup: Vec<u8>, zero_fill: bool) -> Vec<u8> {
    let l = layout(&nup);
    let dir = dir_id(&nup, "tail");
    let row = SUPERBLOCK + l.dirs_at + dir as usize * DIRECTORY;
    let first = u32_at(&nup, row + 8) as usize;
    let pool = u64_at(&nup[SUPERBLOCK..], 0x18) as usize;
    let total = (pool - l.children_at) / 4;
    assert_eq!(u32_at(&nup, row + 12), 14);
    assert_eq!(first + 14, total);
    nup[row + 12..row + 16].copy_from_slice(&0u32.to_le_bytes());
    if zero_fill {
        nup[SUPERBLOCK + l.children_at + first * 4..SUPERBLOCK + pool].fill(0);
    }
    let toc_size = u64_at(&nup, 0x10) as usize;
    let digest = crate::hash::sha256(&nup[SUPERBLOCK..SUPERBLOCK + toc_size]);
    nup[0x18..0x38].copy_from_slice(&digest);
    nup
}

#[test]
fn unaccounted_nup_children_are_diagnosed_or_proven_zero_padding() {
    let dir = work("extra-slots");
    let files = [
        "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9", "aa", "ab", "ac", "ad",
    ]
    .into_iter()
    .map(|name| Node::File(name, vec![42]))
    .collect();
    let valid = build(&[base_with(vec![Node::Dir("tail", files)])], false);

    // Unknown entries remain rejected, even if a less strict NUP consumer
    // manages to start the game. The failure lists package IDs for support.
    let unsafe_extra = unused_tail_slots(valid.clone(), false);
    assert_rejected(
        &dir,
        &unsafe_extra,
        &["14 extra", "unused child slots are not zero padding"],
    );
    let error = inspect_nup(
        &dir.join("input.nup"),
        &ImportLimits::default(),
        &Default::default(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        error.contains("14 extra") && error.contains("#0 base"),
        "{error}"
    );

    // Explicitly validated trailing zero padding is harmless. Do not reject
    // a game merely because its producer left such bytes before the pool.
    let padded = unused_tail_slots(valid, true);
    let path = dir.join("input.nup");
    fs::write(&path, padded).unwrap();
    assert!(inspect_nup(&path, &ImportLimits::default(), &Default::default()).is_ok());
    for stream in [false, true] {
        let stage = stage_nup(
            &path,
            &dir.join("out"),
            &options(stream),
            &ImportLimits::default(),
            &Default::default(),
        )
        .unwrap();
        drop(stage);
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn child_slot_overlap_and_gap_are_rejected() {
    let directory = |first_child, child_count| Directory {
        name: String::new(),
        first_child,
        child_count,
    };
    for (ranges, reason) in [
        (vec![directory(0, 3), directory(2, 2)], "overlap"),
        (vec![directory(0, 1), directory(3, 1)], "gap"),
        (vec![directory(0, 5)], "outside the table"),
    ] {
        let error = validate_child_table(&ranges, &[0, 0, 0, 0], &[])
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains(reason), "{error}");
    }
}

#[test]
fn cycles_and_repeated_nodes_are_rejected() {
    let dir = work("graphs");
    let tree = || {
        vec![
            Node::File("a.bin", b"a".to_vec()),
            Node::Dir("d", vec![Node::File("x.bin", b"x".to_vec())]),
            Node::Dir("e", vec![Node::File("y.bin", b"y".to_vec())]),
        ]
    };
    let good = build(&[base_with(tree())], true);
    // romfs/d/x.bin -> romfs: a cycle.
    let mut cycle = good.clone();
    retarget(&mut cycle, dir_id(&good, "d"), dir_id(&good, "romfs"));
    // romfs/e/y.bin -> d: directory d reached twice.
    let mut shared_dir = good.clone();
    retarget(&mut shared_dir, dir_id(&good, "e"), dir_id(&good, "d"));
    // romfs/d/x.bin -> a.bin's node: one file at two paths.
    let mut shared_file = good.clone();
    retarget(
        &mut shared_file,
        dir_id(&good, "d"),
        file_node(&good, "a.bin"),
    );
    // d points at itself.
    let mut self_loop = good.clone();
    retarget(&mut self_loop, dir_id(&good, "d"), dir_id(&good, "d"));
    for nup in [cycle, shared_dir, shared_file, self_loop] {
        assert_rejected(
            &dir,
            &nup,
            &["cyclic", "listed twice", "duplicate or case-colliding"],
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn shared_trees_cannot_amplify_across_packages() {
    // A base of 300 RomFS files and 40 updates: each update resolves to the
    // whole base, so the TOC (~40 small packages) names ~12,000 entries.
    let dir = work("amplify");
    let files: Vec<Node> = (0..300)
        .map(|i| {
            Node::File(
                Box::leak(format!("f{i:03}.bin").into_boxed_str()),
                vec![i as u8],
            )
        })
        .collect();
    let mut packages = vec![base_with(files)];
    packages.extend((1..=40).map(|v| update(APP, v << 16, 0)));
    let path = dir.join("input.nup");
    fs::write(&path, build(&packages, true)).unwrap();
    let out = dir.join("out");
    for stream in [false, true] {
        let e = stage_nup_bounded(
            &path,
            &out,
            &options(stream),
            &ImportLimits::default(),
            &Default::default(),
            u64::MAX,
            5_000,
        )
        .err()
        .expect("amplified NUP staged");
        assert_eq!(e.kind(), WriterErrorKind::ResourceLimit, "{e}");
        assert_eq!(fs::read_dir(&out).unwrap().count(), 0, "stage left behind");
        // The same NUP within the ceiling stages.
        let stage = stage_nup_bounded(
            &path,
            &out,
            &options(stream),
            &ImportLimits::default(),
            &Default::default(),
            u64::MAX,
            20_000,
        )
        .unwrap();
        drop(stage);
        assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
    }
    // More packages than snapshots allowed: refused before staging.
    let limits = ImportLimits {
        max_snapshots: 8,
        ..Default::default()
    };
    let e = stage_nup(&path, &out, &options(true), &limits, &Default::default())
        .err()
        .unwrap();
    assert_eq!(e.kind(), WriterErrorKind::ResourceLimit, "{e}");
    let e = check_nup(
        &path,
        &some_archive(),
        false,
        &limits,
        &WriterOptions::default(),
    )
    .err()
    .unwrap();
    assert_eq!(e.kind(), WriterErrorKind::ResourceLimit, "{e}");
    assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn staged_bytes_are_bounded_before_decoding() {
    // Streamed, only ExeFS is decoded to disk, within the executable budget.
    let dir = work("staged");
    let path = dir.join("input.nup");
    fs::write(&path, build(&[base(APP)], true)).unwrap();
    let out = dir.join("out");
    let limits = ImportLimits {
        max_executable_bytes: 16,
        ..Default::default()
    };
    let e = stage_nup(&path, &out, &options(true), &limits, &Default::default())
        .err()
        .unwrap();
    assert_eq!(e.kind(), WriterErrorKind::ResourceLimit, "{e}");
    assert!(e.to_string().contains("staging"), "{e}");
    assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
    // Not streamed, the whole tree is staged, within the total budget.
    let limits = ImportLimits {
        max_total_bytes: 64,
        ..Default::default()
    };
    let e = stage_nup(&path, &out, &options(false), &limits, &Default::default())
        .err()
        .unwrap();
    assert_eq!(e.kind(), WriterErrorKind::ResourceLimit, "{e}");
    assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
    // Default budgets stage it.
    drop(
        stage_nup(
            &path,
            &out,
            &options(true),
            &ImportLimits::default(),
            &Default::default(),
        )
        .unwrap(),
    );
    fs::remove_dir_all(&dir).unwrap();
}
