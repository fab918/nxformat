//! Artificial NUP v1 packages, for tests and tools. File contents are
//! placeholders; NSO bytes contain no code.
use super::nup::{DIRECTORY, FILE, NONE, PACKAGE, SUPERBLOCK};
use super::structures::NpdmProfile;
use super::synthetic::{npdm, nso};

/// A tiny PNG signature followed by placeholder bytes.
pub const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3];

/// Node of a NUP tree under construction.
pub enum Node {
    Dir(&'static str, Vec<Node>),
    File(&'static str, Vec<u8>),
}
pub struct Pkg {
    pub title_id: u64,
    pub version: u32,
    pub kind: u8,
    pub flags: u8,
    pub exefs: Option<Vec<Node>>,
    pub romfs: Option<Vec<Node>>,
    pub data: Option<Vec<Node>>,
    /// Additional top-level package trees, including program1/exefs.
    pub extras: Vec<Node>,
    pub meta: Vec<(u16, Vec<u8>)>,
}

pub fn nacp(titles: &[(usize, &str)]) -> Vec<u8> {
    let mut n = vec![0u8; 0x4000];
    for (index, title) in titles {
        n[index * 0x300..index * 0x300 + title.len()].copy_from_slice(title.as_bytes());
        let publisher = b"Synthetic publisher";
        n[index * 0x300 + 0x200..index * 0x300 + 0x200 + publisher.len()]
            .copy_from_slice(publisher);
    }
    n[0x3060..0x3065].copy_from_slice(b"1.2.0");
    n
}
pub fn localized_icons(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (tag, data) in entries {
        out.push(tag.len() as u8);
        out.extend_from_slice(tag.as_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
    }
    out
}
pub fn exefs(build: u8, program: u64) -> Vec<Node> {
    vec![
        Node::File("main", nso(build, true)),
        Node::File("main.npdm", npdm(program, NpdmProfile::MetaAcid3072V1)),
        Node::File("sdk", nso(0x60, false)),
    ]
}

struct Flat {
    dirs: Vec<(String, Vec<u32>)>,
    files: Vec<(String, Vec<u8>)>,
}
const FILE_FLAG: u32 = 0x8000_0000;
// Directory and file IDs are assigned first; file children are resolved
// once the directory count is known.
fn add(flat: &mut Flat, name: &str, nodes: &[Node]) -> u32 {
    let id = flat.dirs.len();
    flat.dirs.push((name.into(), Vec::new()));
    let mut sorted: Vec<&Node> = nodes.iter().collect();
    sorted.sort_by_key(|n| match n {
        Node::Dir(n, _) | Node::File(n, _) => n.to_string(),
    });
    let mut kids = Vec::new();
    for n in sorted {
        match n {
            Node::Dir(name, sub) => kids.push(add(flat, name, sub)),
            Node::File(name, data) => {
                flat.files.push((name.to_string(), data.clone()));
                kids.push(FILE_FLAG | (flat.files.len() as u32 - 1));
            }
        }
    }
    flat.dirs[id].1 = kids;
    id as u32
}

/// Minimal NUP v1 writer; `zstd` picks the codec for every non-empty file.
pub fn build(packages: &[Pkg], zstd: bool) -> Vec<u8> {
    let mut flat = Flat {
        dirs: Vec::new(),
        files: Vec::new(),
    };
    let mut records = Vec::new();
    for p in packages {
        let root = add(&mut flat, "", &[]);
        let mut kids = Vec::new();
        let (mut ex, mut ro) = (NONE, NONE);
        if let Some(data) = &p.data {
            kids.push(add(&mut flat, "data", data));
        }
        if let Some(e) = &p.exefs {
            ex = add(&mut flat, "exefs", e);
            kids.push(ex);
        }
        if let Some(r) = &p.romfs {
            ro = add(&mut flat, "romfs", r);
            kids.push(ro);
        }
        for node in &p.extras {
            if let Node::Dir(name, nested) = node {
                kids.push(add(&mut flat, name, nested));
            } else {
                panic!("synthetic NUP package root extras must be directories");
            }
        }
        flat.dirs[root as usize].1 = kids;
        records.push((root, ex, ro));
    }
    let dir_count = flat.dirs.len() as u32;
    let mut pool = Vec::new();
    let mut intern = |s: &str| {
        let at = pool.len() as u32;
        pool.extend_from_slice(s.as_bytes());
        (at, s.len() as u32)
    };
    let mut children = Vec::new();
    let mut dir_rows = Vec::new();
    for (name, kids) in &flat.dirs {
        let (o, l) = intern(name);
        let mut row = vec![0u8; DIRECTORY];
        row[0..4].copy_from_slice(&o.to_le_bytes());
        row[4..8].copy_from_slice(&l.to_le_bytes());
        row[8..12].copy_from_slice(&(children.len() as u32).to_le_bytes());
        row[12..16].copy_from_slice(&(kids.len() as u32).to_le_bytes());
        for k in kids {
            children.push(if k & FILE_FLAG != 0 {
                dir_count + (k & !FILE_FLAG)
            } else {
                *k
            });
        }
        dir_rows.push(row);
    }
    let file_names: Vec<_> = flat.files.iter().map(|(n, _)| intern(n)).collect();
    let package_count = packages.len();
    let pool_at = 0x30
        + package_count * PACKAGE
        + flat.dirs.len() * DIRECTORY
        + flat.files.len() * FILE
        + children.len() * 4;
    let toc_size = pool_at + pool.len();
    let blob = (SUPERBLOCK + toc_size + 15) & !15;
    let mut out = vec![0u8; blob];
    let mut file_rows = Vec::new();
    for ((_, data), (o, l)) in flat.files.iter().zip(&file_names) {
        let compressed = zstd && !data.is_empty();
        let stored = if compressed {
            zstd::bulk::compress(data, 3).unwrap()
        } else {
            data.clone()
        };
        let at = out.len();
        out.extend_from_slice(&stored);
        out.resize((out.len() + 15) & !15, 0);
        let mut row = vec![0u8; FILE];
        row[0..4].copy_from_slice(&o.to_le_bytes());
        row[4..8].copy_from_slice(&l.to_le_bytes());
        row[8..16].copy_from_slice(&(data.len() as u64).to_le_bytes());
        let offset = if data.is_empty() { 0 } else { at as u64 };
        row[16..24].copy_from_slice(&offset.to_le_bytes());
        row[24..32].copy_from_slice(&(stored.len() as u64).to_le_bytes());
        row[32..64].copy_from_slice(&crate::hash::sha256(data));
        row[64] = u8::from(compressed);
        file_rows.push(row);
    }
    let mut package_rows = Vec::new();
    for (p, (root, ex, ro)) in packages.iter().zip(&records) {
        let mut meta = Vec::new();
        for (kind, body) in &p.meta {
            meta.extend_from_slice(&kind.to_le_bytes());
            meta.extend_from_slice(&[0, 0]);
            meta.extend_from_slice(&(body.len() as u32).to_le_bytes());
            meta.extend_from_slice(body);
        }
        let meta_at = if meta.is_empty() { 0 } else { out.len() as u64 };
        out.extend_from_slice(&meta);
        let mut row = vec![0u8; PACKAGE];
        row[0..8].copy_from_slice(&p.title_id.to_le_bytes());
        row[8..12].copy_from_slice(&root.to_le_bytes());
        row[12..16].copy_from_slice(&ex.to_le_bytes());
        row[16..20].copy_from_slice(&ro.to_le_bytes());
        row[20..24].copy_from_slice(&p.version.to_le_bytes());
        row[24] = p.kind;
        row[25] = p.flags;
        row[32..40].copy_from_slice(&meta_at.to_le_bytes());
        row[40..44].copy_from_slice(&(meta.len() as u32).to_le_bytes());
        package_rows.push(row);
    }
    let mut toc = Vec::new();
    toc.extend_from_slice(b"TOC\x01");
    toc.extend_from_slice(&(package_count as u32).to_le_bytes());
    toc.extend_from_slice(&(flat.dirs.len() as u64).to_le_bytes());
    toc.extend_from_slice(&(flat.files.len() as u64).to_le_bytes());
    toc.extend_from_slice(&(pool_at as u64).to_le_bytes());
    toc.extend_from_slice(&(pool.len() as u64).to_le_bytes());
    toc.extend_from_slice(&0u64.to_le_bytes());
    for r in package_rows.iter().chain(&dir_rows).chain(&file_rows) {
        toc.extend_from_slice(r);
    }
    for c in &children {
        toc.extend_from_slice(&c.to_le_bytes());
    }
    toc.extend_from_slice(&pool);
    assert_eq!(toc.len(), toc_size);
    out[SUPERBLOCK..SUPERBLOCK + toc_size].copy_from_slice(&toc);
    out[..4].copy_from_slice(b"NUP\x01");
    out[4..6].copy_from_slice(&(SUPERBLOCK as u16).to_le_bytes());
    out[6..8].copy_from_slice(&1u16.to_le_bytes());
    out[8..16].copy_from_slice(&(SUPERBLOCK as u64).to_le_bytes());
    out[16..24].copy_from_slice(&(toc_size as u64).to_le_bytes());
    out[24..56].copy_from_slice(&crate::hash::sha256(&toc));
    out[56..64].copy_from_slice(&(blob as u64).to_le_bytes());
    let size = out.len() as u64;
    out[64..72].copy_from_slice(&size.to_le_bytes());
    out[72..76].copy_from_slice(&(package_count as u32).to_le_bytes());
    out
}

/// A base game: ExeFS, RomFS and metadata.
pub fn base(app: u64) -> Pkg {
    Pkg {
        title_id: app,
        version: 0,
        kind: 0,
        flags: 0,
        data: None,
        extras: Vec::new(),
        exefs: Some(exefs(0x41, app)),
        romfs: Some(vec![
            Node::File("common.bin", b"base common data".to_vec()),
            Node::File("shadowed.bin", b"base version".to_vec()),
            Node::Dir("sub", vec![Node::File("deep.bin", vec![7; 5000])]),
        ]),
        meta: vec![
            (
                0x0002,
                nacp(&[(0, "Synthetic Game"), (3, "Jeu synthetique")]),
            ),
            (0x000a, localized_icons(&[("en-us", PNG), ("fr", PNG)])),
        ],
    }
}
/// An update replacing one RomFS file and adding another.
pub fn update(app: u64, version: u32, flags: u8) -> Pkg {
    Pkg {
        title_id: app | 0x800,
        version,
        kind: 1,
        flags,
        data: None,
        extras: Vec::new(),
        exefs: Some(exefs(0x42, app)),
        romfs: Some(vec![
            Node::File("shadowed.bin", b"update version".to_vec()),
            Node::File("added.bin", b"new in the update".to_vec()),
        ]),
        meta: vec![(0x0002, nacp(&[(0, "Synthetic Game")]))],
    }
}
/// One add-on content (DLC).
pub fn addon(app: u64) -> Pkg {
    Pkg {
        title_id: app + 0x1001,
        version: 0,
        kind: 2,
        flags: 0,
        data: None,
        extras: Vec::new(),
        exefs: None,
        romfs: Some(vec![Node::File("dlc.bin", b"dlc".to_vec())]),
        meta: Vec::new(),
    }
}
