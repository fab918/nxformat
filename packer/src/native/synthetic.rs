//! Synthetic, redistributable DNSP/DXCI builders for tests and benchmarks.
//!
//! Every structure is generated from the public layouts the inventory parses.
//! Bytes are not derived from any real title, contain no keys and are not
//! loadable by an emulator. Hash trees and FsHeader hashes are valid so both
//! Fast and Strict inventory paths can be exercised.

use crate::hash::sha256;

#[derive(Clone, Debug)]
pub struct ProgramSpec {
    pub program_id: u64,
    pub build_id: [u8; 32],
    /// Add a RomFS section whose PatchInfo carries a BKTR header.
    pub bktr: bool,
    pub romfs_bytes: usize,
    /// Optional artificial NPDM for native-loader association tests. This does
    /// not make the generated NSO executable or prove emulator compatibility.
    pub npdm: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct ControlSpec {
    pub title: String,
    pub publisher: String,
    pub display_version: String,
    pub icon: Vec<u8>,
    pub localized: Vec<(usize, String, String)>,
    pub extra_icons: Vec<(String, Vec<u8>)>,
    pub titles_format: u8,
}

#[derive(Clone, Debug)]
pub struct TitleSpec {
    /// 0x80 Application, 0x81 Patch, 0x82 AddOnContent.
    pub meta_type: u8,
    pub title_id: u64,
    pub version: u32,
    pub required_application_version: u32,
    /// Extended-header ApplicationId (Patch/AddOnContent) or PatchId (Application).
    pub related_id: u64,
    /// CNMT ContentMetaPlatform byte.
    pub platform: u8,
    pub program: Option<ProgramSpec>,
    pub extra_programs: Vec<ProgramSpec>,
    pub control: Option<ControlSpec>,
    pub data: Option<Vec<u8>>,
    /// Seed for synthetic content IDs.
    pub seed: u8,
    /// Declare AES-CTR on every section (with valid FsHeader hashes).
    pub encrypted_sections: bool,
}

impl TitleSpec {
    pub fn application(app: u64, version: u32, seed: u8) -> Self {
        Self {
            meta_type: 0x80,
            title_id: app,
            version,
            required_application_version: 0,
            related_id: app | 0x800,
            platform: 0,
            program: Some(ProgramSpec {
                program_id: app,
                build_id: [seed; 32],
                bktr: false,
                romfs_bytes: 4096,
                npdm: None,
            }),
            extra_programs: Vec::new(),
            control: Some(ControlSpec {
                title: "Synthetic Title".into(),
                publisher: "NX tests".into(),
                display_version: format!("1.{version}"),
                icon: vec![0xFF, 0xD8, 0xFF, 0xE0, seed],
                localized: Vec::new(),
                extra_icons: Vec::new(),
                titles_format: 0,
            }),
            data: None,
            seed,
            encrypted_sections: false,
        }
    }

    pub fn patch(app: u64, version: u32, seed: u8) -> Self {
        let mut s = Self::application(app, version, seed);
        s.meta_type = 0x81;
        s.title_id = app | 0x800;
        s.related_id = app;
        if let Some(p) = &mut s.program {
            p.bktr = true;
        }
        s
    }

    pub fn add_on(app: u64, dlc: u64, version: u32, seed: u8) -> Self {
        Self {
            meta_type: 0x82,
            title_id: dlc,
            version,
            required_application_version: 0,
            related_id: app,
            platform: 0,
            program: None,
            extra_programs: Vec::new(),
            control: None,
            data: Some(vec![seed; 8192]),
            seed,
            encrypted_sections: false,
        }
    }
}

fn align(v: usize, a: usize) -> usize {
    v.div_ceil(a) * a
}

fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// PFS0 (`hfs = false`) or HFS0 partition bytes.
pub fn partition(entries: &[(String, Vec<u8>)], hfs: bool) -> Vec<u8> {
    let esz = if hfs { 64 } else { 24 };
    let mut strings = Vec::new();
    let mut name_offsets = Vec::new();
    for (name, _) in entries {
        name_offsets.push(strings.len() as u32);
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
    }
    let header = 16 + entries.len() * esz + strings.len();
    strings.resize(align(header, 0x20) - 16 - entries.len() * esz, 0);
    let mut out = vec![0u8; 16 + entries.len() * esz];
    out[..4].copy_from_slice(if hfs { b"HFS0" } else { b"PFS0" });
    put_u32(&mut out, 4, entries.len() as u32);
    put_u32(&mut out, 8, strings.len() as u32);
    let mut rel = 0u64;
    for (i, (_, data)) in entries.iter().enumerate() {
        let e = 16 + i * esz;
        put_u64(&mut out, e, rel);
        put_u64(&mut out, e + 8, data.len() as u64);
        put_u32(&mut out, e + 16, name_offsets[i]);
        rel += data.len() as u64;
    }
    out.extend_from_slice(&strings);
    for (_, data) in entries {
        out.extend_from_slice(data);
    }
    out
}

struct Section {
    fs_type: u8,
    hash_type: u8,
    hash_data: [u8; 0xF8],
    bktr: bool,
    body: Vec<u8>,
}

/// HierarchicalSha256 PartitionFS section.
fn pfs_section(files: &[(String, Vec<u8>)]) -> Section {
    let pfs = partition(files, false);
    let block = 0x1000usize;
    let mut table = Vec::new();
    for chunk in pfs.chunks(block) {
        table.extend_from_slice(&sha256(chunk));
    }
    let data_off = align(table.len(), 0x200);
    let mut body = table.clone();
    body.resize(data_off, 0);
    body.extend_from_slice(&pfs);
    let mut h = [0u8; 0xF8];
    h[..32].copy_from_slice(&sha256(&table));
    put_u32(&mut h, 0x20, block as u32);
    put_u32(&mut h, 0x24, 2);
    put_u64(&mut h, 0x28, 0);
    put_u64(&mut h, 0x30, table.len() as u64);
    put_u64(&mut h, 0x38, data_off as u64);
    put_u64(&mut h, 0x40, pfs.len() as u64);
    Section {
        fs_type: 1,
        hash_type: 2,
        hash_data: h,
        bktr: false,
        body,
    }
}

/// RomFS image with root files only.
fn romfs(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut file_meta = Vec::new();
    let mut data = Vec::new();
    for (i, (name, bytes)) in files.iter().enumerate() {
        let entry_len = align(0x20 + name.len(), 4);
        let next = if i + 1 == files.len() {
            u32::MAX
        } else {
            (file_meta.len() + entry_len) as u32
        };
        let mut e = vec![0u8; entry_len];
        put_u32(&mut e, 0, 0);
        put_u32(&mut e, 4, next);
        put_u64(&mut e, 8, data.len() as u64);
        put_u64(&mut e, 16, bytes.len() as u64);
        put_u32(&mut e, 0x18, u32::MAX);
        put_u32(&mut e, 0x1C, name.len() as u32);
        e[0x20..0x20 + name.len()].copy_from_slice(name.as_bytes());
        file_meta.extend_from_slice(&e);
        data.extend_from_slice(bytes);
        data.resize(align(data.len(), 0x10), 0);
    }
    // Minimal root directory entry and one-slot hash tables.
    let mut dir_meta = vec![0u8; 0x18];
    put_u32(&mut dir_meta, 4, u32::MAX);
    put_u32(&mut dir_meta, 8, u32::MAX);
    put_u32(
        &mut dir_meta,
        12,
        if files.is_empty() { u32::MAX } else { 0 },
    );
    put_u32(&mut dir_meta, 16, u32::MAX);
    put_u32(&mut dir_meta, 20, 0);
    let hash_table = u32::MAX.to_le_bytes().to_vec();
    let mut out = vec![0u8; 0x50];
    let mut cursor = 0x50usize;
    let mut place = |out: &mut Vec<u8>, field: usize, bytes: &[u8]| {
        put_u64(out, field, cursor as u64);
        put_u64(out, field + 8, bytes.len() as u64);
        out.extend_from_slice(bytes);
        cursor += bytes.len();
    };
    put_u64(&mut out, 0, 0x50);
    place(&mut out, 0x08, &hash_table);
    place(&mut out, 0x18, &dir_meta);
    place(&mut out, 0x28, &hash_table);
    place(&mut out, 0x38, &file_meta);
    let data_off = align(out.len(), 0x10);
    out.resize(data_off, 0);
    put_u64(&mut out, 0x48, data_off as u64);
    out.extend_from_slice(&data);
    out
}

/// Three-layer IVFC section (master -> level 0 -> RomFS level).
fn ivfc_section(image: Vec<u8>, bktr: bool) -> Section {
    // Smallest block (at least 16 KiB) whose level-0 hash table fits in one block.
    let mut log2 = 14u32;
    while image.len().div_ceil(1 << log2) * 32 > 1 << log2 {
        log2 += 1;
    }
    let block = 1usize << log2;
    let mut level0 = Vec::new();
    for chunk in image.chunks(block) {
        let mut padded = chunk.to_vec();
        padded.resize(block, 0);
        level0.extend_from_slice(&sha256(&padded));
    }
    assert!(level0.len() <= block, "synthetic RomFS too large for IVFC");
    let mut padded0 = level0.clone();
    padded0.resize(block, 0);
    let master = sha256(&padded0);
    let data_off = align(level0.len(), block);
    let mut body = level0.clone();
    body.resize(data_off, 0);
    body.extend_from_slice(&image);
    let mut h = [0u8; 0xF8];
    h[..4].copy_from_slice(b"IVFC");
    put_u32(&mut h, 4, 0x20000);
    put_u32(&mut h, 8, 0x20);
    put_u32(&mut h, 0x0C, 3);
    let levels = [
        (0u64, level0.len() as u64),
        (data_off as u64, image.len() as u64),
    ];
    for (i, (off, size)) in levels.iter().enumerate() {
        put_u64(&mut h, 0x10 + i * 0x18, *off);
        put_u64(&mut h, 0x18 + i * 0x18, *size);
        put_u32(&mut h, 0x20 + i * 0x18, log2);
    }
    h[0xC0..0xE0].copy_from_slice(&master);
    Section {
        fs_type: 0,
        hash_type: 3,
        hash_data: h,
        bktr,
        body,
    }
}

fn nca(content_type: u8, program_id: u64, sections: Vec<Section>, encrypted: bool) -> Vec<u8> {
    let mut out = vec![0u8; 0xC00];
    for (i, s) in sections.iter().enumerate() {
        let start = align(out.len(), 0x200);
        out.resize(start, 0);
        out.extend_from_slice(&s.body);
        let virtual_size = align(s.body.len(), 0x200);
        let relocation_offset = align(virtual_size, 0x4000);
        if s.bktr {
            // Real, single-root BKTR fixture. The base supplies zero padding
            // between the IVFC hash table and RomFS; payload comes from patch.
            out.resize(start + relocation_offset + 0x8000, 0);
            let root = start + relocation_offset;
            put_u32(&mut out, root + 4, 1);
            put_u64(&mut out, root + 8, virtual_size as u64);
            let bucket = root + 0x4000;
            put_u32(&mut out, bucket + 4, 3);
            put_u64(&mut out, bucket + 8, virtual_size as u64);
            let data_offset = u64::from_le_bytes(s.hash_data[0x28..0x30].try_into().unwrap());
            let table_size = u64::from_le_bytes(s.hash_data[0x18..0x20].try_into().unwrap());
            for (i, (offset, patch)) in [(0, 1), (table_size, 0), (data_offset, 1)]
                .iter()
                .enumerate()
            {
                put_u64(&mut out, bucket + 16 + i * 20, *offset);
                put_u64(&mut out, bucket + 24 + i * 20, *offset);
                put_u32(&mut out, bucket + 32 + i * 20, *patch);
            }
        }
        let end = align(out.len(), 0x200);
        out.resize(end, 0);
        put_u32(&mut out, 0x240 + i * 16, (start / 0x200) as u32);
        put_u32(&mut out, 0x244 + i * 16, (end / 0x200) as u32);
        let fs = 0x400 + i * 0x200;
        put_u16(&mut out, fs, 2);
        out[fs + 2] = s.fs_type;
        out[fs + 3] = s.hash_type;
        out[fs + 4] = if encrypted { 3 } else { 1 };
        out[fs + 8..fs + 0x100].copy_from_slice(&s.hash_data);
        if s.bktr {
            put_u64(&mut out, fs + 0x100, relocation_offset as u64);
            put_u64(&mut out, fs + 0x108, 0x8000);
            out[fs + 0x110..fs + 0x114].copy_from_slice(b"BKTR");
            put_u32(&mut out, fs + 0x114, 1);
            put_u32(&mut out, fs + 0x118, 3);
            out[fs + 0x130..fs + 0x134].copy_from_slice(b"BKTR");
        }
        let digest = sha256(&out[fs..fs + 0x200]);
        out[0x280 + i * 32..0x2A0 + i * 32].copy_from_slice(&digest);
    }
    out[0x200..0x204].copy_from_slice(b"DNCA");
    out[0x205] = content_type;
    let size = out.len() as u64;
    put_u64(&mut out, 0x208, size);
    put_u64(&mut out, 0x210, program_id);
    // Native-loader fixtures need the u32 SDK add-on version as well as the
    // separate key-generation byte. Inventory fixtures previously omitted it.
    put_u32(&mut out, 0x21C, 0x000B_0000);
    out[0x220] = 0x0B;
    out
}

fn content_id(seed: u8, kind: u8, title: u64) -> [u8; 16] {
    let mut input = vec![seed, kind];
    input.extend_from_slice(&title.to_le_bytes());
    let d = sha256(&input);
    let mut id = [0u8; 16];
    id.copy_from_slice(&d[..16]);
    id
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}

/// Package entries (`<id>.nca`, `<id>.cnmt.nca`) for one content meta.
pub fn title_entries(spec: &TitleSpec) -> Vec<(String, Vec<u8>)> {
    title_entries_custom(spec, &[], &[])
}

/// Explicit synthetic Program offsets and reused content records; no real data.
pub fn title_entries_custom(
    spec: &TitleSpec,
    offsets: &[u8],
    shared: &[(usize, String, Vec<u8>)],
) -> Vec<(String, Vec<u8>)> {
    let enc = spec.encrypted_sections;
    // (record type, NCA bytes)
    let mut contents: Vec<(u8, Vec<u8>)> = Vec::new();
    for p in spec.program.iter().chain(&spec.extra_programs) {
        let mut nso = vec![0u8; 0x100];
        nso[..4].copy_from_slice(b"NSO0");
        if p.npdm.as_ref().is_some_and(|n| n.len() > 4) {
            nso = crate::import::synthetic::nso(p.build_id[0], false);
        }
        nso[0x40..0x60].copy_from_slice(&p.build_id);
        let mut sections = vec![pfs_section(&[
            ("main".into(), nso),
            (
                "main.npdm".into(),
                p.npdm.clone().unwrap_or_else(|| b"META".to_vec()),
            ),
        ])];
        let image = romfs(&[("data.bin".into(), vec![spec.seed; p.romfs_bytes])]);
        sections.push(ivfc_section(image, p.bktr));
        contents.push((1, nca(0, p.program_id, sections, enc)));
    }
    if let Some(c) = &spec.control {
        let mut nacp = vec![0u8; 0x4000];
        nacp[..c.title.len()].copy_from_slice(c.title.as_bytes());
        nacp[0x200..0x200 + c.publisher.len()].copy_from_slice(c.publisher.as_bytes());
        nacp[0x3060..0x3060 + c.display_version.len()]
            .copy_from_slice(c.display_version.as_bytes());
        for (index, title, publisher) in &c.localized {
            assert!(*index < 16 && title.len() <= 0x200 && publisher.len() <= 0x100);
            let at = index * 0x300;
            nacp[at..at + title.len()].copy_from_slice(title.as_bytes());
            nacp[at + 0x200..at + 0x200 + publisher.len()].copy_from_slice(publisher.as_bytes());
        }
        nacp[0x3215] = c.titles_format;
        let mut files = vec![
            ("control.nacp".into(), nacp),
            ("icon_AmericanEnglish.dat".into(), c.icon.clone()),
        ];
        files.extend(
            c.extra_icons
                .iter()
                .map(|(l, b)| (format!("icon_{l}.dat"), b.clone())),
        );
        let image = romfs(&files);
        let owner = spec.program.as_ref().map_or(
            if spec.meta_type == 0x81 {
                spec.related_id
            } else {
                spec.title_id
            },
            |p| p.program_id,
        );
        contents.push((3, nca(2, owner, vec![ivfc_section(image, false)], enc)));
    }
    if let Some(d) = &spec.data {
        let image = romfs(&[("dlc.bin".into(), d.clone())]);
        contents.push((
            2,
            nca(4, spec.title_id, vec![ivfc_section(image, false)], enc),
        ));
    }
    while let Some((_, _, data)) = shared.iter().find(|(index, _, _)| *index == contents.len()) {
        let kind = match data[0x205] {
            0 => 1,
            2 => 3,
            4 => 2,
            _ => panic!("unsupported synthetic extra content"),
        };
        contents.push((kind, data.clone()));
    }
    let ext_size = if spec.meta_type == 0x80 { 0x10 } else { 0x18 };
    let mut cnmt = vec![0u8; 0x20 + ext_size];
    put_u64(&mut cnmt, 0, spec.title_id);
    put_u32(&mut cnmt, 8, spec.version);
    cnmt[0x0C] = spec.meta_type;
    cnmt[0x0D] = spec.platform;
    put_u16(&mut cnmt, 0x0E, ext_size as u16);
    put_u16(&mut cnmt, 0x10, contents.len() as u16);
    put_u64(&mut cnmt, 0x20, spec.related_id);
    if spec.meta_type == 0x82 {
        put_u32(&mut cnmt, 0x28, spec.required_application_version);
    }
    let mut entries = Vec::new();
    for (i, (record_type, mut bytes)) in contents.into_iter().enumerate() {
        let mut id = content_id(spec.seed, i as u8, spec.title_id);
        if let Some((_, name, data)) = shared.iter().find(|(index, _, _)| *index == i) {
            for (j, b) in id.iter_mut().enumerate() {
                *b = u8::from_str_radix(&name[j * 2..j * 2 + 2], 16).unwrap();
            }
            bytes = data.clone();
        }
        let mut r = vec![0u8; 0x38];
        r[..32].copy_from_slice(&sha256(&bytes));
        r[32..48].copy_from_slice(&id);
        r[48..53].copy_from_slice(&(bytes.len() as u64).to_le_bytes()[..5]);
        r[0x36] = record_type;
        r[0x37] = offsets
            .get(i)
            .copied()
            .unwrap_or(if record_type == 1 { i as u8 } else { 0 });
        cnmt.extend_from_slice(&r);
        entries.push((format!("{}.nca", hex(&id)), bytes));
    }
    let cnmt_name = format!(
        "{}_{:016x}.cnmt",
        match spec.meta_type {
            0x80 => "Application",
            0x81 => "Patch",
            _ => "AddOnContent",
        },
        spec.title_id
    );
    let meta = nca(
        1,
        spec.title_id,
        vec![pfs_section(&[(cnmt_name, cnmt)])],
        enc,
    );
    let meta_id = content_id(spec.seed, 0xFF, spec.title_id);
    entries.push((format!("{}.cnmt.nca", hex(&meta_id)), meta));
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// Turn the program NCA of `entries` into a flattened sparse base, as a
/// decrypter writes one: the executable section is all holes (zeros, no
/// data; the update brings the executable). `marked` writes the `DSPF` marker
/// at FS header 0x1D0 that says so; without it the section is just damaged.
pub fn sparse_executable(entries: &mut [(String, Vec<u8>)], marked: bool) {
    let (_, program) = entries
        .iter_mut()
        .find(|(name, bytes)| !name.ends_with(".cnmt.nca") && bytes[0x205] == 0)
        .expect("no program NCA");
    let start = u32::from_le_bytes(program[0x240..0x244].try_into().unwrap()) as usize * 0x200;
    let end = u32::from_le_bytes(program[0x244..0x248].try_into().unwrap()) as usize * 0x200;
    program[start..end].fill(0);
    if marked {
        program[0x400 + 0x1D0..0x400 + 0x1D4].copy_from_slice(b"DSPF");
    }
    let digest = sha256(&program[0x400..0x600]);
    program[0x280..0x2A0].copy_from_slice(&digest);
}

pub fn dnsp(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    partition(entries, false)
}

/// DXCI image with the given gamecard partitions (e.g. `secure`).
/// Named package entries: `(entry name, bytes)`.
pub type Entries = Vec<(String, Vec<u8>)>;

pub fn dxci(partitions: &[(&str, Entries)]) -> Vec<u8> {
    let root_off = 0x1000usize;
    let parts: Vec<(String, Vec<u8>)> = partitions
        .iter()
        .map(|(name, entries)| (name.to_string(), partition(entries, true)))
        .collect();
    let root = partition(&parts, true);
    let mut out = vec![0u8; root_off];
    out[0x100..0x104].copy_from_slice(b"DXCI");
    put_u64(&mut out, 0x130, root_off as u64);
    let root_header = root.len() - parts.iter().map(|p| p.1.len()).sum::<usize>();
    put_u64(&mut out, 0x138, root_header as u64);
    out.extend_from_slice(&root);
    out
}
