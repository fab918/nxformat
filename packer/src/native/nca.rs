//! Decrypted NCA headers and the small native structures the inventory reads:
//! CNMT content meta, Control `control.nacp`, ExeFS `main` Build ID.
//!
//! Layouts follow the public SwitchBrew documentation (NCA, CNMT, NCM
//! services, NACP, NSO). Nothing here decrypts; any section whose header
//! still declares encryption is rejected.

use super::container::{parse_partition, u16_at, u32_at, u64_at, PartitionEntry, Source};
use super::{
    ContentMeta, ContentMetaType, ContentRecord, ContentRecordType, ControlMetadata, ControlTitle,
    InventoryOptions, NativeError, NativeGeneration, NcaContentType, NcaSection, NcaSummary,
    VerificationState,
};
use crate::hash::sha256;

pub(crate) const NCA_HEADER_SIZE: u64 = 0xC00;
const SECTOR: u64 = 0x200;
/// Marker NXConverter writes in the reserved FS header bytes of a section it
/// flattened from sparse storage (see `NcaSection::sparse_flattened`).
pub(crate) const SPARSE_MARKER_AT: usize = 0x1D0;
pub(crate) const SPARSE_MARKER: &[u8; 4] = b"DSPF";

/// Languages in `control.nacp` title-entry order, using the native names that
/// also appear in `icon_{Language}.dat` file names.
pub(crate) const NACP_LANGUAGES: [&str; 16] = [
    "AmericanEnglish",
    "BritishEnglish",
    "Japanese",
    "French",
    "German",
    "LatinAmericanSpanish",
    "Spanish",
    "Italian",
    "Dutch",
    "CanadianFrench",
    "Portuguese",
    "Russian",
    "Korean",
    "TraditionalChinese",
    "SimplifiedChinese",
    "BrazilianPortuguese",
];

pub(crate) struct ParsedNca {
    pub(crate) summary: NcaSummary,
    /// Absolute offset of the NCA in the source file.
    pub(crate) at: u64,
    pub(crate) hash_data: Vec<[u8; 0xF8]>,
    pub(crate) patch_data: Vec<[u8; 0x40]>,
    pub(crate) unsupported_storage: Vec<bool>,
    pub(crate) compression_data: Vec<[u8; 0x28]>,
    pub(crate) metadata_hash_data: Vec<Option<[u8; 0x30]>>,
}

pub(crate) enum NcaRejection {
    /// Header bytes do not carry the decrypted-content marker.
    NotDecrypted(String),
    /// A section header still declares encryption.
    EncryptedSection(String),
    Other(NativeError),
}

impl From<NativeError> for NcaRejection {
    fn from(e: NativeError) -> Self {
        NcaRejection::Other(e)
    }
}

pub(crate) fn header_marker(header: &[u8]) -> Option<&'static str> {
    match &header[0x200..0x204] {
        b"DNCA" => Some("DNCA"),
        b"NCA3" => Some("NCA3"),
        b"NCA2" => Some("NCA2"),
        b"NCA0" => Some("NCA0"),
        _ => None,
    }
}

pub(crate) fn parse_nca(src: &mut Source, at: u64, size: u64) -> Result<ParsedNca, NcaRejection> {
    if size < NCA_HEADER_SIZE {
        return Err(NativeError::truncated("NCA is smaller than its 0xC00-byte header").into());
    }
    let mut h = vec![0u8; NCA_HEADER_SIZE as usize];
    src.read_exact_at(at, &mut h)?;
    match header_marker(&h) {
        Some("DNCA") => {}
        Some(other) => {
            return Err(NcaRejection::NotDecrypted(format!(
                "plaintext {other} header without the DNCA decrypted-content marker"
            )))
        }
        _ => {
            return Err(NcaRejection::NotDecrypted(
                "NCA header is encrypted or unrecognized".into(),
            ))
        }
    }
    let content_type = NcaContentType::from_u8(h[0x205])
        .ok_or_else(|| NativeError::invalid(format!("unknown NCA content type {}", h[0x205])))?;
    let content_size = u64_at(&h, 0x208);
    if content_size != size {
        return Err(NativeError::invalid(format!(
            "NCA header size {content_size} differs from its container entry size {size}"
        ))
        .into());
    }
    let mut rights_id = [0u8; 16];
    rights_id.copy_from_slice(&h[0x230..0x240]);
    let mut sections = Vec::new();
    let mut hash_data = Vec::new();
    let mut patch_data = Vec::new();
    let mut unsupported_storage = Vec::new();
    let mut compression_data = Vec::new();
    let mut metadata_hash_data = Vec::new();
    for i in 0..4usize {
        let start = u32_at(&h, 0x240 + i * 16) as u64 * SECTOR;
        let end = u32_at(&h, 0x244 + i * 16) as u64 * SECTOR;
        if start == 0 && end == 0 {
            continue;
        }
        if start < NCA_HEADER_SIZE || end <= start || end > content_size {
            return Err(NativeError::invalid(format!("NCA section {i} range is invalid")).into());
        }
        let fs = &h[0x400 + i * 0x200..0x600 + i * 0x200];
        if sha256(fs) != h[0x280 + i * 32..0x2A0 + i * 32] {
            return Err(NativeError::invalid(format!(
                "NCA section {i} header hash does not match"
            ))
            .into());
        }
        if u16_at(fs, 0) != 2 {
            return Err(
                NativeError::invalid(format!("NCA section {i} header version is not 2")).into(),
            );
        }
        let encryption_type = fs[4];
        if encryption_type != 1 {
            return Err(NcaRejection::EncryptedSection(format!(
                "NCA section {i} declares encryption type {encryption_type}"
            )));
        }
        let patch = &fs[0x100..0x140];
        let has_patch = &patch[0x10..0x14] == b"BKTR" || &patch[0x30..0x34] == b"BKTR";
        let mut data = [0u8; 0xF8];
        data.copy_from_slice(&fs[8..0x100]);
        sections.push(NcaSection {
            index: i as u8,
            start,
            end,
            fs_type: fs[2],
            hash_type: fs[3],
            encryption_type,
            has_patch_info: has_patch,
            sparse_flattened: &fs[SPARSE_MARKER_AT..SPARSE_MARKER_AT + 4] == SPARSE_MARKER,
        });
        hash_data.push(data);
        patch_data.push(patch.try_into().unwrap());
        unsupported_storage.push(
            fs[0x148..0x178].iter().any(|&b| b != 0)
                || !matches!(fs[5], 0 | 1)
                || (fs[5] == 0 && fs[0x1a0..0x1d0].iter().any(|&b| b != 0)),
        );
        compression_data.push(fs[0x178..0x1a0].try_into().unwrap());
        metadata_hash_data.push((fs[5] == 1).then(|| fs[0x1a0..0x1d0].try_into().unwrap()));
    }
    if sections.is_empty() {
        return Err(NativeError::invalid("NCA declares no sections").into());
    }
    // Four sections at most: reject overlapping storage without allocating a
    // second range table or sorting on the hot inventory path.
    for (i, section) in sections.iter().enumerate() {
        if sections[..i]
            .iter()
            .any(|previous| section.start < previous.end && previous.start < section.end)
        {
            return Err(NativeError::invalid("NCA sections overlap").into());
        }
    }
    Ok(ParsedNca {
        summary: NcaSummary {
            marker: "DNCA",
            content_type,
            distribution: h[0x204],
            program_id: u64_at(&h, 0x210),
            content_index: u32_at(&h, 0x218),
            sdk_addon_version: u32_at(&h, 0x21C),
            key_generation: h[0x206].max(h[0x220]),
            rights_id,
            sections,
        },
        at,
        hash_data,
        patch_data,
        unsupported_storage,
        compression_data,
        metadata_hash_data,
    })
}

/// A located filesystem region inside an NCA section.
struct Region {
    offset: u64,
    size: u64,
}

impl ParsedNca {
    fn section_slot(&self, index: usize) -> (&NcaSection, &[u8; 0xF8]) {
        (&self.summary.sections[index], &self.hash_data[index])
    }

    /// Region holding the PFS0 of a HierarchicalSha256 PartitionFS section.
    fn pfs0_region(&self, index: usize) -> Result<(Region, Region, u32, [u8; 32]), NativeError> {
        let (section, data) = self.section_slot(index);
        if section.fs_type != 1 || section.hash_type != 2 {
            return Err(NativeError::unsupported(
                "section is not a HierarchicalSha256 PartitionFS",
            ));
        }
        let mut master = [0u8; 32];
        master.copy_from_slice(&data[..32]);
        let block_size = u32_at(data, 0x20);
        let layers = u32_at(data, 0x24);
        if layers != 2 || !block_size.is_power_of_two() || block_size < 0x200 {
            return Err(NativeError::invalid("unexpected HierarchicalSha256 layout"));
        }
        let table = Region {
            offset: u64_at(data, 0x28),
            size: u64_at(data, 0x30),
        };
        let pfs = Region {
            offset: u64_at(data, 0x38),
            size: u64_at(data, 0x40),
        };
        let len = section.end - section.start;
        for r in [&table, &pfs] {
            if r.offset.checked_add(r.size).map_or(true, |e| e > len) {
                return Err(NativeError::invalid("hash region exceeds its section"));
            }
        }
        let blocks = pfs.size.div_ceil(block_size as u64);
        if table.size < blocks * 32 {
            return Err(NativeError::invalid("hash table is too small for its data"));
        }
        Ok((table, pfs, block_size, master))
    }

    /// Region of the RomFS level of a HierarchicalIntegrity (IVFC) section.
    fn romfs_region(&self, index: usize) -> Result<Region, NativeError> {
        let (section, data) = self.section_slot(index);
        if section.fs_type != 0 || section.hash_type != 3 {
            return Err(NativeError::unsupported("section is not an IVFC RomFS"));
        }
        if section.has_patch_info {
            return Err(NativeError::unsupported(
                "patch RomFS requires its base content to be read",
            ));
        }
        if &data[..4] != b"IVFC" {
            return Err(NativeError::invalid("IVFC magic is missing"));
        }
        let max_layers = u32_at(data, 0x0C);
        if !(2..=7).contains(&max_layers) {
            return Err(NativeError::invalid("IVFC layer count is invalid"));
        }
        let level = 0x10 + (max_layers as usize - 2) * 0x18;
        let region = Region {
            offset: u64_at(data, level),
            size: u64_at(data, level + 8),
        };
        if region
            .offset
            .checked_add(region.size)
            .map_or(true, |e| e > section.end - section.start)
        {
            return Err(NativeError::invalid("RomFS level exceeds its section"));
        }
        Ok(region)
    }

    fn abs(&self, index: usize, rel: u64) -> u64 {
        self.at + self.summary.sections[index].start + rel
    }

    fn first_section(&self, fs_type: u8) -> Option<usize> {
        self.summary
            .sections
            .iter()
            .position(|s| s.fs_type == fs_type)
    }

    pub(crate) fn partition(
        &self,
        src: &mut Source,
        options: &InventoryOptions,
    ) -> Result<(usize, Vec<PartitionEntry>), NativeError> {
        let index = self
            .first_section(1)
            .ok_or_else(|| NativeError::invalid("NCA has no PartitionFS section"))?;
        let (_, pfs, _, _) = self.pfs0_region(index)?;
        let base = self.abs(index, pfs.offset);
        let entries = parse_partition(src, base, base + pfs.size, false, options)?;
        Ok((index, entries))
    }

    /// Verify the HierarchicalSha256 tree of a PartitionFS section.
    pub(crate) fn verify_partition(
        &self,
        src: &mut Source,
        index: usize,
        options: &InventoryOptions,
    ) -> Result<VerificationState, NativeError> {
        let (table, pfs, block_size, master) = self.pfs0_region(index)?;
        if pfs.size > options.max_strict_section_bytes {
            return Ok(VerificationState::SkippedSizeLimit);
        }
        let hashes = src.read_vec_at(
            self.abs(index, table.offset),
            table.size,
            options.max_strict_section_bytes,
            "hash table",
        )?;
        if sha256(&hashes) != master {
            return Ok(VerificationState::Failed(
                "HierarchicalSha256 master hash mismatch".into(),
            ));
        }
        let mut buf = vec![0u8; (block_size as u64).min(pfs.size) as usize];
        let mut done = 0u64;
        let mut i = 0usize;
        while done < pfs.size {
            let n = (pfs.size - done).min(block_size as u64) as usize;
            src.read_exact_at(self.abs(index, pfs.offset + done), &mut buf[..n])?;
            if sha256(&buf[..n]) != hashes[i * 32..i * 32 + 32] {
                return Ok(VerificationState::Failed(format!(
                    "HierarchicalSha256 block {i} mismatch"
                )));
            }
            done += n as u64;
            i += 1;
        }
        Ok(VerificationState::Verified)
    }

    /// Verify the IVFC tree of a RomFS section level by level.
    pub(crate) fn verify_romfs(
        &self,
        src: &mut Source,
        index: usize,
        options: &InventoryOptions,
    ) -> Result<VerificationState, NativeError> {
        let (section, data) = self.section_slot(index);
        if section.has_patch_info {
            return Ok(VerificationState::NotApplicable);
        }
        let romfs = self.romfs_region(index)?;
        if romfs.size > options.max_strict_section_bytes {
            return Ok(VerificationState::SkippedSizeLimit);
        }
        let max_layers = u32_at(data, 0x0C) as usize;
        let master_size = u32_at(data, 0x08) as usize;
        let level = |i: usize| {
            let at = 0x10 + i * 0x18;
            (
                u64_at(data, at),
                u64_at(data, at + 8),
                u32_at(data, at + 16),
            )
        };
        let master = data[0xC0..0xC0 + master_size.min(0x20)].to_vec();
        // The master hash covers level 0; level n holds hashes of level n+1.
        let mut expected = master;
        for li in 0..max_layers - 1 {
            let (offset, size, log2) = level(li);
            if !(9..=24).contains(&log2) {
                return Err(NativeError::invalid("IVFC block size is invalid"));
            }
            let block = 1usize << log2;
            let bytes = src.read_vec_at(
                self.abs(index, offset),
                size,
                options.max_strict_section_bytes,
                "IVFC level",
            )?;
            let count = bytes.len().div_ceil(block);
            if expected.len() < count * 32 {
                return Ok(VerificationState::Failed(format!(
                    "IVFC level {li} hash table is too small"
                )));
            }
            let mut padded = vec![0u8; block];
            for b in 0..count {
                let chunk = &bytes[b * block..((b + 1) * block).min(bytes.len())];
                padded[..chunk.len()].copy_from_slice(chunk);
                padded[chunk.len()..].fill(0);
                if sha256(&padded) != expected[b * 32..b * 32 + 32] {
                    return Ok(VerificationState::Failed(format!(
                        "IVFC level {li} block {b} mismatch"
                    )));
                }
            }
            expected = bytes;
        }
        Ok(VerificationState::Verified)
    }
}

/// Parse the single `.cnmt` file of a Meta NCA.
pub(crate) fn read_content_meta(
    src: &mut Source,
    nca: &ParsedNca,
    options: &InventoryOptions,
) -> Result<ContentMeta, NativeError> {
    if nca.summary.content_type != NcaContentType::Meta {
        return Err(NativeError::invalid(
            "content meta must come from a Meta NCA",
        ));
    }
    let (_, entries) = nca.partition(src, options)?;
    let mut cnmts = entries.iter().filter(|e| e.name.ends_with(".cnmt"));
    let entry = cnmts
        .next()
        .ok_or_else(|| NativeError::invalid("Meta NCA contains no .cnmt"))?;
    if cnmts.next().is_some() {
        return Err(NativeError::invalid(
            "Meta NCA contains more than one .cnmt",
        ));
    }
    src.reserve_metadata(entry.size)?;
    let b = src.read_vec_at(entry.offset, entry.size, options.max_meta_bytes, "CNMT")?;
    parse_cnmt(&b)
}

pub(crate) fn parse_cnmt(b: &[u8]) -> Result<ContentMeta, NativeError> {
    if b.len() < 0x20 {
        return Err(NativeError::truncated("CNMT header is truncated"));
    }
    let title_id = u64_at(b, 0);
    let version = u32_at(b, 8);
    let meta_type = ContentMetaType::from_u8(b[0x0C]);
    let platform_byte = b[0x0D];
    let ext_size = u16_at(b, 0x0E) as usize;
    let content_count = u16_at(b, 0x10) as usize;
    let meta_count = u16_at(b, 0x12) as usize;
    let records_at = 0x20 + ext_size;
    let records_end = records_at + content_count * 0x38;
    if records_end + meta_count * 0x10 > b.len() {
        return Err(NativeError::truncated("CNMT records are truncated"));
    }
    let ext = &b[0x20..records_at];
    let need = match meta_type {
        // AddOnContent grew from 0x10 to 0x18 bytes in 15.0.0 (DataPatchId).
        ContentMetaType::Application | ContentMetaType::AddOnContent => 0x10,
        ContentMetaType::Patch => 0x18,
        _ => 0,
    };
    if ext.len() < need {
        return Err(NativeError::invalid("CNMT extended header is too small"));
    }
    let (application_id, patch_id, required_application_version, required_system_version) =
        match meta_type {
            ContentMetaType::Application => (
                title_id,
                Some(u64_at(ext, 0)),
                Some(u32_at(ext, 0x0C)),
                Some(u32_at(ext, 0x08)),
            ),
            ContentMetaType::Patch => (u64_at(ext, 0), None, None, Some(u32_at(ext, 0x08))),
            ContentMetaType::AddOnContent => (u64_at(ext, 0), None, Some(u32_at(ext, 0x08)), None),
            _ => (title_id, None, None, None),
        };
    if title_id == 0 || application_id == 0 {
        return Err(NativeError::invalid(
            "CNMT has a zero title or application ID",
        ));
    }
    let generation = match platform_byte {
        0 => NativeGeneration::Switch1,
        1 => NativeGeneration::Switch2,
        other => NativeGeneration::UnknownMarker(other),
    };
    let mut contents = Vec::with_capacity(content_count);
    for i in 0..content_count {
        let r = &b[records_at + i * 0x38..records_at + (i + 1) * 0x38];
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&r[..32]);
        let mut content_id = [0u8; 16];
        content_id.copy_from_slice(&r[32..48]);
        let mut size_bytes = [0u8; 8];
        size_bytes[..5].copy_from_slice(&r[48..53]);
        contents.push(ContentRecord {
            hash,
            content_id,
            size: u64::from_le_bytes(size_bytes),
            record_type: ContentRecordType::from_u8(r[0x36]),
            id_offset: r[0x37],
        });
    }
    Ok(ContentMeta {
        title_id,
        version,
        meta_type,
        platform_byte,
        generation,
        application_id,
        patch_id,
        required_application_version,
        required_system_version,
        contents,
        meta_content_id: None,
        control: None,
        controls: Vec::new(),
        build_id: None,
        program_ids: Vec::new(),
        programs: Vec::new(),
        requires_base_romfs: false,
        verification: VerificationState::NotChecked,
    })
}

/// Read `control.nacp` titles, publisher and display version plus the names
/// and sizes of `icon_{Language}.dat` payloads from a Control NCA.
pub(crate) fn read_control(
    src: &mut Source,
    nca: &ParsedNca,
    options: &InventoryOptions,
) -> Result<ControlMetadata, NativeError> {
    let index = nca
        .first_section(0)
        .ok_or_else(|| NativeError::invalid("Control NCA has no RomFS section"))?;
    let region = nca.romfs_region(index)?;
    let base = nca.abs(index, region.offset);
    if region.size < 0x50 {
        return Err(NativeError::truncated("RomFS header is truncated"));
    }
    let mut h = [0u8; 0x50];
    src.read_exact_at(base, &mut h)?;
    if u64_at(&h, 0) != 0x50 {
        return Err(NativeError::invalid("RomFS header size is not 0x50"));
    }
    let file_meta_off = u64_at(&h, 0x38);
    let file_meta_size = u64_at(&h, 0x40);
    let data_off = u64_at(&h, 0x48);
    for (off, size) in [(file_meta_off, file_meta_size), (data_off, 0)] {
        if off.checked_add(size).map_or(true, |e| e > region.size) {
            return Err(NativeError::invalid("RomFS table exceeds its level"));
        }
    }
    src.reserve_metadata(file_meta_size)?;
    let meta = src.read_vec_at(
        base + file_meta_off,
        file_meta_size,
        options.max_meta_bytes,
        "RomFS file table",
    )?;
    let mut nacp = None;
    let mut icons = Vec::new();
    let mut icon_ranges = Vec::new();
    let mut at = 0usize;
    while at + 0x20 <= meta.len() {
        let parent = u32_at(&meta, at);
        let offset = u64_at(&meta, at + 8);
        let size = u64_at(&meta, at + 16);
        let name_len = u32_at(&meta, at + 0x1C) as usize;
        let name_end = at + 0x20 + name_len;
        if name_end > meta.len() {
            return Err(NativeError::invalid("RomFS file name exceeds its table"));
        }
        let name = std::str::from_utf8(&meta[at + 0x20..name_end])
            .map_err(|_| NativeError::invalid("RomFS file name is not UTF-8"))?;
        if data_off
            .checked_add(offset)
            .and_then(|v| v.checked_add(size))
            .map_or(true, |e| e > region.size)
        {
            return Err(NativeError::invalid("RomFS file data exceeds its level"));
        }
        if parent == 0 {
            if name == "control.nacp" {
                if nacp.replace((base + data_off + offset, size)).is_some() {
                    return Err(NativeError::invalid("duplicate control.nacp"));
                }
            } else if let Some(lang) = name
                .strip_prefix("icon_")
                .and_then(|n| n.strip_suffix(".dat"))
            {
                icons.push((lang.to_owned(), size));
                if super::NACP_LANGUAGE_MAP
                    .iter()
                    .any(|(native, _)| *native == lang)
                {
                    if icon_ranges.iter().any(|(l, _, _, _)| l == lang) {
                        return Err(NativeError::invalid("duplicate native icon language"));
                    }
                    if size > options.max_meta_bytes {
                        return Err(NativeError::limit("native icon exceeds metadata limit"));
                    }
                    let mut magic = [0; 8];
                    src.read_exact_at(
                        base + data_off + offset,
                        &mut magic[..size.min(8) as usize],
                    )?;
                    let kind = if magic == *b"\x89PNG\r\n\x1a\n" {
                        Some(crate::MediaType::PngIcon)
                    } else if size >= 3 && magic[..3] == [0xff, 0xd8, 0xff] {
                        Some(crate::MediaType::JpegIcon)
                    } else {
                        None
                    };
                    if let Some(kind) = kind {
                        icon_ranges.push((lang.to_owned(), base + data_off + offset, size, kind));
                    }
                }
            }
        }
        at = (name_end + 3) & !3;
    }
    let (nacp_at, nacp_size) =
        nacp.ok_or_else(|| NativeError::invalid("Control RomFS has no control.nacp"))?;
    if nacp_size != 0x4000 {
        return Err(NativeError::invalid(format!(
            "control.nacp is {nacp_size} bytes, expected 0x4000"
        )));
    }
    src.reserve_metadata(nacp_size)?;
    let n = src.read_vec_at(nacp_at, nacp_size, nacp_size, "control.nacp")?;
    let (titles, display_version, invalid_strings) = parse_nacp_text(&n);
    icons.sort();
    Ok(ControlMetadata {
        display_version,
        titles,
        icons,
        invalid_strings,
        nacp_digest: sha256(&n),
        nacp_bytes: n,
        icon_ranges,
    })
}

/// Legacy NACP title table and display version. Text stops at the first NUL;
/// invalid UTF-8 is reported, never repaired. `n` must be 0x4000 bytes.
pub(crate) fn parse_nacp_text(n: &[u8]) -> (Vec<ControlTitle>, Option<String>, Vec<String>) {
    let mut invalid_strings = Vec::new();
    if n[0x3215] != 0 {
        invalid_strings.push("unsupported TitlesDataFormat".into());
    }
    let mut text = |bytes: &[u8], field: String| -> Option<String> {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        if end == 0 {
            return None;
        }
        match std::str::from_utf8(&bytes[..end]) {
            Ok(s) => Some(s.to_owned()),
            Err(_) => {
                invalid_strings.push(field);
                None
            }
        }
    };
    let mut titles = Vec::new();
    // Compressed/newer title tables must never be parsed as legacy strings.
    let languages: &[&str] = if n[0x3215] == 0 { &NACP_LANGUAGES } else { &[] };
    for (i, language) in languages.iter().enumerate() {
        let entry = &n[i * 0x300..(i + 1) * 0x300];
        let name = text(&entry[..0x200], format!("{language} name"));
        let publisher = text(&entry[0x200..], format!("{language} publisher"));
        if name.is_some() || publisher.is_some() {
            titles.push(ControlTitle {
                language: (*language).to_owned(),
                name,
                publisher,
            });
        }
    }
    let display_version = text(&n[0x3060..0x3070], "display version".into());
    (titles, display_version, invalid_strings)
}

/// Read the 32-byte module (Build) ID of ExeFS `main`.
pub(crate) fn read_build_id(
    src: &mut Source,
    nca: &ParsedNca,
    options: &InventoryOptions,
) -> Result<Option<[u8; 32]>, NativeError> {
    let (_, entries) = nca.partition(src, options)?;
    let Some(main) = entries.iter().find(|e| e.name == "main") else {
        return Ok(None);
    };
    if main.size < 0x100 {
        return Err(NativeError::truncated(
            "ExeFS main is smaller than an NSO header",
        ));
    }
    let mut h = [0u8; 0x60];
    src.read_exact_at(main.offset, &mut h)?;
    if &h[..4] != b"NSO0" {
        return Err(NativeError::invalid("ExeFS main is not an NSO0 module"));
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&h[0x40..0x60]);
    Ok(Some(id))
}
