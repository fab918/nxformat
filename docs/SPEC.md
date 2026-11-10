# NX Binary Specification

**Release:** 1.0.2. **Binary header versions:** `(0,2)` and `(0,3)`. **Extension:** `.nx`. **Magic:** ASCII `NXAR`. This document defines both wire versions. Integers are little-endian. Offsets and byte counts are unsigned unless stated otherwise. Indexes are zero-based. No on-disk C/C++ structure may be read by casting a pointer to it; implementations decode each field from bytes with explicit bounds and endian conversion.

Source container formats, including decrypted DNSP/DXCI/DNCA, are not NX wire types. Import-to-snapshot mapping is described in [CLI.md](CLI.md#source-import-contract); the byte layout and reader requirements here remain source-independent.

The fixed field tables in this document are the only authoritative wire definitions. All reserved bytes and bits are zero and MUST be rejected if nonzero.

## 1. File layout and offset rules

| Region | Location |
|---|---|
| Header slot A | offset 0, exactly 256 bytes |
| Header slot B | offset 4096, exactly 256 bytes |
| Data region | optional; offset 8192; individual blocks 16-byte aligned |
| Media region | optional; 16-byte aligned |
| Catalog | 16-byte aligned; at most 16 MiB |
| Manifest | 16-byte aligned |
| Hot block index | 16-byte aligned; `block_count × 24` bytes |
| Integrity table | 16-byte aligned; `check_count × 4` bytes |

Bulk data comes first and every metadata region follows it. A reader therefore reads the first 8192 bytes and then one contiguous *tail* (Media through integrity table) to mount, and a writer streams blocks straight into the final file without a staging copy. Each header occupies the first 256 bytes of its own 4096-byte page. The remainder of each page is zero. Validate the pages independently so damage or an I/O error in either page can leave the other usable. Separating the pages isolates faults confined to one 4 KiB storage unit.

**Header slots and generations.** Each slot holds a complete `NxHeader` with its own CRC and a `generation` number. A reader validates both slots and uses the valid slot with the highest generation; if both are valid with the same generation they MUST be byte-identical, otherwise the archive is rejected. A slot that fails validation is ignored when the other is valid. A freshly written archive has generation 0 in both slots. Every field below describes the selected generation only. If two distinct generations have valid header CRCs, a reader SHOULD check the newer generation's metadata ranges and Catalog, Manifest, and index digests before exposing its Catalog. If those checks fail due to damage, it SHOULD try the older generation and expose its Catalog only if the older metadata passes the same checks. Unsupported features and resource limits are reported as errors.

The selected generation's sections occur in the order above and do not overlap. Absent optional sections have both offset and size zero. Data begins at 8192 when blocks exist. Blocks are in strictly increasing offset order; the gap before a block is at least the minimum 16-byte alignment and MAY be larger (*free space* left by in-place edits, §8). Free-space bytes are unspecified and are never read. The tail is packed: the first present tail region (Media, otherwise Catalog) begins on a 16-byte boundary at or after `align_up(end, 16)`, where `end` is the end of the Data region or 8192 when there are no blocks (bytes before it are free space; in a compact archive it begins exactly there); each later tail region begins at `align_up(end_of_previous_region, 16)`. Tail alignment gaps are zero-filled. `archive_size` is the end of the integrity table, or of the Manifest when there are no blocks. The physical file MAY be longer than `archive_size` (bytes left by an interrupted edit); readers ignore those bytes. A *compact* archive, as produced by a full write or repack, has minimum-aligned zero padding between blocks and a file length equal to `archive_size`.

Top-level and block-data offsets are absolute from archive byte zero. `NxMediaRegionHeader` record/blob offsets, `NxCatalogHeader` table/string offsets, and `NxManifestHeader` table/path offsets are relative to the first byte of their containing region. Media record `data_offset` is relative to the start of the Media region. An `NxStringRef` offset is relative to the Catalog string pool. No offsets are relative to the end of a record.

A full rewrite (new archive, compaction) MUST build and validate a complete temporary archive, flush it, and atomically replace the original on the same filesystem. Synchronize the destination directory after replacement on platforms that support directory synchronization. An in-place edit follows the append protocol of §8: it never overwrites a byte referenced by any generation still present in a header slot, and it only rewrites the header slot that is not selected.

An implementation MUST use checked arithmetic for every range and count. The maximum archive size is `INT64_MAX`. Normative object count and payload limits are in [SECURITY.md](SECURITY.md).

## 2. Primitive encodings

- `u8`, `u16`, `u32`, `u64`: unsigned 8-, 16-, 32-, or 64-bit little-endian integer.
- `byte[N]`: exactly N uninterpreted bytes. A zero value has all bytes zero.
- `NxStringRef`: `offset:u32`, `length:u32`, 8 bytes. `offset` is relative to the Catalog string pool. Absent is `offset=0xffffffff,length=0`. Present references MUST have `length>=1`; empty optional strings are encoded as absent. Otherwise the byte range must lie inside the pool and contain valid UTF-8 with no NUL. It is not NUL terminated. Strings are not normalized.
- `media_index:u32`: index in the Media record array. Absent is `0xffffffff`.
- A 16-byte content identifier is preserved byte-for-byte from its source. All zero means unavailable; it is not inferred from another identifier.
- A 32-byte build ID is preserved byte-for-byte on the exact snapshot/version it describes. All zero means unavailable.
- SHA-256 fields store the 32 digest bytes in the standard output order, with no byte reversal.

## 3. `NxHeader` — 256 bytes at offsets 0 and 4096

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 4 | `byte[4]` | `magic` | ASCII `NXAR` |
| 4 | 2 | `u16` | `major_version` | 0 |
| 6 | 2 | `u16` | `minor_version` | 2 without Source Resources; 3 with Source Resources |
| 8 | 4 | `u32` | `header_size` | 256 |
| 12 | 2 | `u16` | `required_features` | 1 in this profile. Bit 0 identifies separate header pages. Unknown required bits reject |
| 14 | 2 | `u16` | `optional_features` | 0; no optional feature is currently defined |
| 16 | 8 | `u64` | `archive_size` | End of this generation's tail; `<=` physical file size and `<= INT64_MAX` |
| 24 | 8 | `u64` | `catalog_offset` | 16-byte aligned tail position (§1) |
| 32 | 8 | `u64` | `catalog_size` | At least 128, at most 16 MiB |
| 40 | 8 | `u64` | `media_offset` | Zero iff absent; otherwise 16-byte aligned |
| 48 | 8 | `u64` | `media_size` | Zero iff absent; otherwise at least 32 |
| 56 | 8 | `u64` | `data_offset` | Zero iff absent; otherwise 8192 |
| 64 | 8 | `u64` | `data_size` | Zero iff absent; otherwise ends at the last block byte |
| 72 | 8 | `u64` | `manifest_offset` | 16-byte aligned |
| 80 | 8 | `u64` | `manifest_size` | At least 96; at most 4 GiB |
| 88 | 8 | `u64` | `block_index_offset` | Zero iff `block_count=0`; otherwise 16-byte aligned |
| 96 | 8 | `u64` | `block_count` | At most `2^32` |
| 104 | 8 | `u64` | `integrity_offset` | Zero iff `block_count=0`; otherwise 16-byte aligned |
| 112 | 8 | `u64` | `integrity_size` | Exactly `check_count × 4` (§7); zero iff no blocks |
| 120 | 32 | `byte[32]` | `catalog_sha256` | SHA-256 of exactly `catalog_size` bytes |
| 152 | 32 | `byte[32]` | `manifest_sha256` | SHA-256 of exactly `manifest_size` bytes |
| 184 | 32 | `byte[32]` | `index_sha256` | SHA-256 of exactly `block_count × 24` hot-index bytes followed by `integrity_size` integrity-table bytes; SHA-256 of empty bytes when `block_count=0` |
| 216 | 8 | `u64` | `generation` | Edit generation; 0 for a fresh or compacted archive (§1, §8) |
| 224 | 4 | `u32` | `check_size` | 16384; logical bytes covered by one integrity check |
| 228 | 24 | `byte[24]` | `reserved` | All zero |
| 252 | 4 | `u32` | `header_crc32c` | CRC-32C of bytes 0 through 251 inclusive |

CRC-32C uses reflected Castagnoli polynomial `0x82f63b78`, initial state `0xffffffff`, and final XOR `0xffffffff`. The standard check value for ASCII `123456789` is `0xe3069283`.

### Format-version compatibility policy

The `major_version` and `minor_version` fields identify the **NX archive format**. These fields are independent of the NXFormat software release and the game version. NXFormat 1.0.0 wrote `(0,2)`. NXFormat 1.0.1 writes `(0,2)` for archives without Source Resources and `(0,3)` for archives with them. Current readers accept `(0,2)` and `(0,3)`; later readers may explicitly support several pairs with version-specific validation and decoding paths. Never silently reinterpret an old version as a new one.

Both versions use the same core layout: Data first, a metadata tail, two separate header pages, and slice CRCs. Header feature bit 0 is required. The profile includes the required Snapshot Native Metadata extension `0x00010005` where specified in §4.

Changes that make existing readers reject new archives require a version or profile change, with old-version readers and test fixtures retained. Optional extensions may remain in the same profile only when older readers can safely ignore them. Feature bits do not replace format-version negotiation.

For block count zero, block index and integrity offsets/sizes are all zero; no block arrays are present. `data_offset` and `data_size` are zero because there is no Data region. A nonempty archive still contains a Catalog and Manifest.

Before using any block descriptor or integrity digest, mount MUST verify `index_sha256`. Its input is precisely the hot-index bytes followed by the integrity-table bytes; alignment padding is excluded. This detects corruption only while the trusted header remains unchanged and does not authenticate the archive.

## 4. Fast Catalog

Catalog begins at `catalog_offset` in the tail. Its complete bytes are covered by `catalog_sha256`. The section is at most 16 MiB. Arrays are disjoint, correctly sized for their count, and 8-byte aligned. String bytes and extension payload bytes do not overlap arrays. All Catalog offsets are relative to its first byte.

### `NxCatalogHeader` — 128 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 4 | `byte[4]` | `magic` | ASCII `NXCT` |
| 4 | 2 | `u16` | `catalog_version` | 1 |
| 6 | 2 | `u16` | `header_size` | 128 |
| 8 | 8 | `u64` | `total_size` | Equals header `catalog_size` |
| 16 | 4 | `u32` | `catalog_flags` | 0 |
| 20 | 4 | `u32` | `application_count` | 1..4096 |
| 24 | 4 | `u32` | `program_count` | 0..16384; zero only for applications with no executable programs |
| 28 | 4 | `u32` | `snapshot_count` | 1..65536 |
| 32 | 4 | `u32` | `localized_entry_count` | 0..16384 |
| 36 | 4 | `u32` | `dependency_count` | 0..32768 |
| 40 | 4 | `u32` | `extension_count` | 0..4096 |
| 44 | 4 | `u32` | `reserved0` | 0 |
| 48 | 8 | `u64` | `application_table_offset` | Start of `application_count × 80` bytes |
| 56 | 8 | `u64` | `program_table_offset` | Start of `program_count × 64` bytes |
| 64 | 8 | `u64` | `snapshot_table_offset` | Start of `snapshot_count × 120` bytes |
| 72 | 8 | `u64` | `localized_entry_table_offset` | Start of `localized_entry_count × 24` bytes; zero iff count is zero |
| 80 | 8 | `u64` | `dependency_table_offset` | Start of `dependency_count × 56` bytes; zero iff count is zero |
| 88 | 8 | `u64` | `extension_table_offset` | Start of `extension_count × 32` bytes; zero iff count is zero |
| 96 | 8 | `u64` | `string_pool_offset` | Start of string bytes; 8-byte aligned |
| 104 | 8 | `u64` | `string_pool_size` | Byte length; no terminator is implied |
| 112 | 16 | `byte[16]` | `reserved1` | All zero |

Tables appear in the order application, program, snapshot, localized entry, dependency, extension, string pool, then extension payloads. A zero-count table has offset zero and occupies no bytes. Each present table starts at the minimum 8-byte aligned offset after the prior present table; the string pool starts at the minimum 8-byte aligned offset after the last table. Extension payloads are disjoint, after the string pool, and placed in extension-table order at the minimum 8-byte aligned offset after the previous payload. All alignment padding is zero.

### `NxApplication` — 80 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 8 | `u64` | `application_id` | Nonzero; unique with platform; ascending by `(platform, application_id)` |
| 8 | 1 | `u8` | `platform` | 0 unknown, 1 Switch 1, 2 Switch 2; derived from validated native content/container metadata |
| 9 | 1 | `u8` | `flags` | Bit 0 `HAS_BASE`, bit 1 `HAS_DLC`; other bits zero |
| 10 | 2 | `u16` | `reserved0` | 0 |
| 12 | 4 | `u32` | `first_program_index` | Valid program range |
| 16 | 4 | `u32` | `program_count` | Zero only when all snapshots are unassociated DLC |
| 20 | 4 | `u32` | `first_snapshot_index` | Valid contiguous snapshot range |
| 24 | 4 | `u32` | `snapshot_count` | At least 1 |
| 28 | 4 | `u32` | `default_title_offset` | `NxStringRef.offset` |
| 32 | 4 | `u32` | `default_title_length` | `NxStringRef.length` |
| 36 | 4 | `u32` | `publisher_offset` | `NxStringRef.offset` |
| 40 | 4 | `u32` | `publisher_length` | `NxStringRef.length` |
| 44 | 4 | `u32` | `default_icon_media_index` | Icon type or absent sentinel |
| 48 | 4 | `u32` | `first_localized_entry_index` | Valid localized-entry range |
| 52 | 4 | `u32` | `localized_entry_count` | Entries belong to this application |
| 56 | 4 | `u32` | `nacp_media_index` | NACP type or absent sentinel |
| 60 | 4 | `u32` | `first_opaque_media_index` | Opaque media range; absent range uses zero count |
| 64 | 4 | `u32` | `opaque_media_count` | Every member has opaque-source type |
| 68 | 4 | `u32` | `metadata_snapshot_index` | Exact snapshot carrying displayed metadata; `0xffffffff` when none is available |
| 72 | 8 | `byte[8]` | `reserved1` | All zero |

Application flags must agree with the application's snapshots. `HAS_BASE` means at least one snapshot has type base; `HAS_DLC` means at least one snapshot has type DLC. Application program and snapshot ranges partition their respective tables in application order. The snapshot range includes executable snapshots and DLC content. A zero-length program range uses `first_program_index=0xffffffff`; nonempty program ranges partition the Program table. `metadata_snapshot_index` is absent only when no reliable display metadata is present; otherwise it MUST identify a non-DLC snapshot in this application. The displayed strings, localized entries, NACP record and icons MUST all be associated with that exact snapshot. Native values originate from its metadata; explicit caller-supplied display fallbacks or edits may supply title, publisher or icons without modifying native NACP bytes. Display fields alone are not evidence of native origin or execution compatibility (see [CLI.md](CLI.md#display-title-and-artwork)). The packer selects the eligible snapshot with authoritative metadata and the highest numeric content version; when versions tie, it selects the greatest Catalog snapshot index. Localized-entry ranges partition the Localized Entry table in application order. Opaque-media ranges are disjoint, in bounds, and contain only opaque-source metadata records. Applications are strictly ascending by `(platform, application_id)`; the same numeric ID may occur on distinct platforms. Native display metadata for other snapshots with reliable values is preserved by the Snapshot Metadata History extension.

A non-absent `metadata_snapshot_index` requires at least one metadata reference (title, publisher, icon, NACP, localized entry, or opaque media). If any such reference exists, `metadata_snapshot_index` MUST be present. Opaque media alone is sufficient to require an exact source association.

### `NxProgram` — 64 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 8 | `u64` | `program_id` | Nonzero within application |
| 8 | 4 | `u32` | `application_index` | Valid application index |
| 12 | 1 | `u8` | `program_kind` | 0 primary, 1 auxiliary, 2 unknown |
| 13 | 1 | `u8` | `flags` | Bit 0 content ID present; other bits zero |
| 14 | 2 | `u16` | `reserved0` | 0 |
| 16 | 16 | `byte[16]` | `content_id` | All zero iff flag bit 0 clear |
| 32 | 4 | `u32` | `first_snapshot_index` | Valid contiguous range of snapshots associated with this executable |
| 36 | 4 | `u32` | `snapshot_count` | At least 1 |
| 40 | 4 | `u32` | `npdm_media_index` | NPDM type or absent sentinel |
| 44 | 4 | `u32` | `reserved1` | 0 |
| 48 | 16 | `byte[16]` | `reserved2` | All zero |

Program rows represent executable identities only. Rows are grouped by application index, then strictly ascending by `program_id`. Build IDs belong to snapshots because they identify a particular build/version, not a program across all versions. New writers MUST leave Program `npdm_media_index` absent and use Snapshot Native Metadata (`0x00010005`). A non-absent Program NPDM is accepted only when that Program has exactly one snapshot; it belongs to that snapshot. Multiple snapshots with one Program NPDM are ambiguous and reject. Program `content_id` is only an independently supplied program-scope source identifier; it is never a default for a snapshot Content ID. A version-specific Content ID belongs on its snapshot.

### `NxSnapshot` — 120 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 4 | `u32` | `program_index` | Valid program index, or `0xffffffff` only for DLC without an executable program |
| 4 | 1 | `u8` | `snapshot_type` | 0 base, 1 update, 2 DLC, 3 standalone update, 4 unknown |
| 5 | 1 | `u8` | `flags` | Bit 0 `BUILD_ID_PRESENT`; bit 1 `ADDON_TITLE_ID`; other bits zero |
| 6 | 2 | `u16` | `reserved0` | 0 |
| 8 | 4 | `u32` | `numeric_content_version` | Numeric source value; not inferred from display string |
| 12 | 4 | `u32` | `root_directory_index` | Valid Manifest directory index |
| 16 | 8 | `u64` | `logical_content_size` | Sum of file sizes in this snapshot tree; checked |
| 24 | 4 | `u32` | `display_version_offset` | `NxStringRef.offset` |
| 28 | 4 | `u32` | `display_version_length` | `NxStringRef.length` |
| 32 | 16 | `byte[16]` | `content_id` | All zero means unknown; with `ADDON_TITLE_ID`, a `u64` add-on title ID then 8 zero bytes |
| 48 | 4 | `u32` | `first_dependency_index` | Valid dependency range |
| 52 | 4 | `u32` | `dependency_count` | Dependencies belong to this snapshot |
| 56 | 32 | `byte[32]` | `snapshot_identity` | Canonical logical-content identity defined in §12 |
| 88 | 32 | `byte[32]` | `build_id` | Exact build ID for this snapshot/version; all zero means unavailable |

Snapshot rows are grouped by application. Snapshots with a program are grouped by program index and ordered by type, numeric version, content ID, then build ID. Unassociated DLC rows follow the executable snapshots for their application and are ordered by numeric version, content ID, then build ID. DLC may name a program only when the source explicitly supplies that executable association. A DLC record without an executable MUST use the absent program index; the packer MUST NOT invent a Program ID. Program snapshot ranges cover exactly snapshots associated with that program. Application snapshot ranges include both groups. Dependency ranges partition the Dependency table in snapshot order. Duplicate sort keys are invalid. Snapshot flag bit 0 is set exactly when `build_id` is nonzero. Flag bit 1 (`ADDON_TITLE_ID`) is allowed only on a DLC row with the absent program index: `content_id` then holds the add-on (AOC) title ID little-endian, nonzero, followed by eight zero bytes, and no native Content ID is recorded. It is used for DLC converted from NUP packages and extracted dumps, which carry the add-on title ID but no NCA; native DLC keeps its Content ID and its CNMT. All other flag bits are zero. Snapshot identity includes numeric version and snapshot type. Build ID and display-version text are preserved metadata but excluded from identity; identical trees with different type/version have distinct identities.

### `NxLocalizedEntry` — 24 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 4 | `u32` | `language_offset` | `NxStringRef.offset`; lowercase ASCII BCP 47 tag |
| 4 | 4 | `u32` | `language_length` | 1..63 bytes |
| 8 | 4 | `u32` | `title_offset` | `NxStringRef.offset` or absent |
| 12 | 4 | `u32` | `title_length` | `NxStringRef.length` |
| 16 | 4 | `u32` | `icon_media_index` | PNG/JPEG type or absent sentinel |
| 20 | 4 | `u32` | `flags` | 0; title and icon cannot both be absent |

Localized rows are grouped by application and strictly ascending by language-tag bytes. Duplicate language tags in one application are invalid.

Catalog display strings and icons MUST come from reliable native metadata. For Switch 1, the normal source is `control.nacp` plus the matching language-specific `icon_{Language}.dat` payloads. A Switch 2 native layout must be independently validated; it MUST NOT be assumed identical. A packer MUST preserve exact source strings, language distinctions, and JPEG/PNG bytes, and MUST NOT invent absent values, substitute online artwork, normalize strings, or recompress JPEG/PNG images. `platform` MUST be derived from a validated native content/container marker that distinguishes generations; title names and ID prefixes are not sufficient. If generation or a metadata value is unknown, encode unknown/absent and report that limitation.

When multiple updates are present, `metadata_snapshot_index` explicitly names the snapshot whose native metadata supplies the application's displayed title, publisher, localized entries, NACP reference, and icon references. The selected source must be a metadata-bearing, non-DLC snapshot owned by that application. A writer selects the eligible source with the highest authoritative numeric content version by default; when versions tie, it selects the one with the greatest Catalog snapshot index. An archive-editing caller may explicitly select another eligible source, for example to preserve a user's chosen display version. Every displayed value and reference in that application must come from the selected snapshot. If no snapshot supplies reliable metadata, the reference and all display fields are absent.

### `NxDependency` — 56 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 1 | `u8` | `kind` | 1 required base, 2 requires, 3 recommends |
| 1 | 1 | `u8` | `flags` | Bit 0 content ID present; other bits zero |
| 2 | 2 | `u16` | `reserved0` | 0 |
| 4 | 16 | `byte[16]` | `content_id` | Zero iff flag bit 0 clear |
| 20 | 32 | `byte[32]` | `target_snapshot_identity` | Exact target identity; nonzero |
| 52 | 4 | `u32` | `reserved1` | 0 |

For a partial source update, a required-base dependency must name the exact target snapshot identity, not only an application ID, numeric version, or content ID. Dependencies document relationships; the selected NX snapshot remains a complete tree and does not apply them at read time.

### `NxExtension` — 32 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 4 | `u32` | `type_id` | Extension identifier |
| 4 | 4 | `u32` | `flags` | Bit 0 required; other bits zero |
| 8 | 2 | `u16` | `owner_kind` | 0 global, 1 application, 2 program, 3 snapshot |
| 10 | 2 | `u16` | `reserved0` | 0 |
| 12 | 4 | `u32` | `owner_index` | Global uses `0xffffffff`; otherwise valid table index |
| 16 | 8 | `u64` | `data_offset` | Catalog-relative extension payload range |
| 24 | 8 | `u64` | `data_size` | Payload byte size |

Defined optional extension payloads: `0x00010001` required firmware is one little-endian `u32`, application owner; `0x00010002` supported languages is `u16 count` followed by `count` repetitions of `u8 length` and lowercase ASCII BCP 47 bytes, application owner, count 1..64; `0x00010003` regions is `u16 count` followed by `count` two-byte uppercase ISO 3166-1 alpha-2 codes, application owner, count 1..249. Entries in a list are bytewise sorted and unique. Values are emitted only when reliably known. `0x00010001` through `0x00010003` describe only the exact application `metadata_snapshot_index`, which MUST be present when these extensions exist; they are discovery facts, never defaults for other versions. An editor must remove/rederive them when that selected source changes. Native NACP/opaque metadata for other versions remains in their snapshot history or complete native tree. Per-snapshot launch firmware uses `0x00010005`. `0x00010004` is the required Snapshot Metadata History extension defined below; `0x00010005` is the required Snapshot Native Metadata extension. `0x00010006` is the optional Rebuild Recipes extension (flags 0, application owner): a 16-byte header (`NXRC`, `u16` version 1, `u16` header size 16, `u32` count 1..4096, `u32` zero), then for each recipe a `u64` length, the recipe bytes as defined in [RECIPE](RECIPE.md) and zero padding to 8 bytes. Recipes are strictly ascending by their trailing SHA-256 (the recipe checksum), each checksum must match, and the payload ends at the last padding byte. The reference writer limits the combined encoded recipe payloads to 8 MiB, within the 16 MiB Catalog limit. A reader may skip the extension; a writer keeps a recipe only while every file it names is present. Unknown optional extension payloads are skipped after range validation; unknown required types reject the archive. A `(type_id, owner_kind, owner_index)` tuple is unique.

#### Snapshot Metadata History (`0x00010004`)

This application-owned extension preserves native display metadata associated with snapshots other than the application's currently selected `metadata_snapshot_index`. It is present exactly when at least one other snapshot in the application has reliable native display metadata. The current application row remains the fast display source; a repacker uses this history table to select the best surviving source by default after a snapshot is removed, unless the caller explicitly selects another eligible source. The extension does not add runtime update overlays or require readers to select or apply an update.

The payload begins with a 24-byte header, followed contiguously by `record_count` 48-byte snapshot metadata rows, `localized_count` 24-byte rows using the `NxLocalizedEntry` layout, and `opaque_reference_count` little-endian `u32` Media indexes. The exact payload size is `24 + record_count×48 + localized_count×24 + opaque_reference_count×4`; counts and references are bounded by the containing Catalog and its 16 MiB limit.

| Header offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 4 | `byte[4]` | `magic` | ASCII `NXSM` |
| 4 | 2 | `u16` | `version` | 1 |
| 6 | 2 | `u16` | `header_size` | 24 |
| 8 | 4 | `u32` | `record_count` | 1..65,536 |
| 12 | 4 | `u32` | `localized_count` | 0..16,384 |
| 16 | 4 | `u32` | `opaque_reference_count` | Bounded by payload size |
| 20 | 4 | `u32` | `reserved` | 0 |

Each 48-byte row contains, in order: `snapshot_index`, `title_offset`, `title_length`, `publisher_offset`, `publisher_length`, `icon_media_index`, `nacp_media_index`, `first_localized_index`, `localized_count`, `first_opaque_reference`, `opaque_reference_count`, and `reserved`, all `u32`. String references use the Catalog string pool and the normal absent-sentinel/length rules. Media references use the existing type rules. Localized rows use the normal language and string rules; opaque indexes each reference one type-5 Media record.

Rows are strictly ascending by Catalog `snapshot_index`, belong to the extension's application, and MUST exclude its `metadata_snapshot_index`. Localized and opaque ranges partition their respective payload arrays in row order. A row must contain at least one title, publisher, icon, NACP reference, localized value, or opaque reference. No row is emitted for a snapshot with no reliable native metadata. Every non-icon media item has exactly one owning reference; identical PNG/JPEG icon payloads may share a Media index across metadata sources. Exact UTF-8 strings share string-pool ranges. A repacker remaps row indexes when it rewrites snapshot order and selects the highest-version surviving non-DLC source by default, unless its caller explicitly selects another eligible source; it must never attribute one snapshot's strings or images to another snapshot. Readers validate source ownership and metadata references but do not impose the writer's default source-selection policy.

#### Snapshot Native Metadata (`0x00010005`)

This required (`flags=1`), application-owned extension associates execution metadata with exact snapshot indexes. It MUST be emitted when an application contains any supplied per-snapshot NPDM or required-firmware value. It is independent of display metadata selection and may exist without any application title/icon/history. Older readers that do not implement it reject the unknown required extension.

The payload is a 16-byte header followed immediately by `record_count` 16-byte rows. Total size is exactly `16 + record_count * 16`.

| Header offset | Type | Value |
|---:|---|---|
| 0 | byte[4] | ASCII `NXNM` |
| 4 | u16 | Version 1 |
| 6 | u16 | Header size 16 |
| 8 | u32 | Record count, 1..owning application's snapshot count |
| 12 | u32 | Reserved, zero |

| Row offset | Type | Value |
|---:|---|---|
| 0 | u32 | Snapshot index, belonging to the owning application |
| 4 | u32 | NPDM Media index (type 3), or `0xffffffff` |
| 8 | u32 | Required firmware from this snapshot's source, or zero if absent |
| 12 | u32 | Bit 0: firmware present; all other bits zero |

Rows are strictly ascending by snapshot index and unique. A row must supply NPDM, firmware, or both. A present firmware may be zero; absence requires bit 0 clear and value zero. NPDM requires a real executable Program association and cannot coexist with a Program NPDM for that same snapshot. Each NPDM Media record is referenced exactly once. Identical NPDM bytes for two versions may be stored as distinct records; values must never be silently inherited from another snapshot. The reader exposes these fields on Snapshot. NPDM and firmware are metadata excluded from snapshot identity v3, like the Build ID.

Editing MUST remap row snapshot indexes and NPDM Media indexes, preserve surviving versions' exact values, drop removed versions' rows/media when unreferenced, and emit no empty table. The required firmware version is native metadata. It is independent of any bundled system-update partition.

### Source Resources (`0x00010008`, NX 0.3)

Source Resources preserve source-container data independently of every executable
snapshot. The Catalog holds only references to ordinary Manifest files and Data
blocks. Resources are outside the executable trees and their snapshot identities.

The extension is required (`flags=1`) and application-owned (`owner_kind=1`).
NX 0.3 MUST contain at least one Source Resource; NX 0.2 MUST contain none.
Readers and editors that only support NX 0.2 reject NX 0.3.

The payload is a 16-byte header followed by `count` 8-byte records:

| Offset | Type | Value |
|---:|---|---|
| 0 | byte[4] | ASCII `NXSR` |
| 4 | u16 | Version 1 |
| 6 | u16 | Header size 16 |
| 8 | u32 | Count, 1..4096; at most 4096 resources across the archive |
| 12 | u32 | Reserved, zero |
| 16 + 8*n | u32 | Kind: 0 opaque source file set, 1 original system-update HFS0 |
| 20 + 8*n | u32 | Manifest root Directory ID |

Records are strictly ordered by `(kind, root_directory)`. Other kinds are invalid
in version 1. Roots are independent,
unique across resources, not snapshot roots or child directories, and contain a
nonempty flat set of unique File IDs. All ordinary path, file, block, count and
integrity rules apply. Kind 1 contains exactly one file named `partition.hfs0`:
the complete original HFS0 partition, including tables, unknown files and padding.
Kind 1 identifies the native partition layout. A firmware installer must validate
its contents separately.

An editor MUST preserve every resource of a retained application, remap its root
and file/block references, and include those blocks in live-data accounting and
compaction. Removing a game version does not remove the application's resources.
Removing an entire application removes its resources. Rebuild recipes may reference
resource files by size and digest, as defined in [RECIPE.md](RECIPE.md).

## 5. Media region

The optional Media region begins with a 32-byte header, then a contiguous record array, then payload bytes. Catalog media indexes must be less than `record_count`. An index referenced as an icon must identify a PNG/JPEG record; a program NPDM index must identify NPDM; an application NACP index must identify NACP; and an opaque-media range must contain only opaque-source records. Icon records may be shared. Each non-icon media record is referenced by exactly one matching Catalog owner.

### `NxMediaRegionHeader` — 32 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 4 | `byte[4]` | `magic` | ASCII `NXMD` |
| 4 | 2 | `u16` | `media_version` | 1 |
| 6 | 2 | `u16` | `header_size` | 32 |
| 8 | 8 | `u64` | `total_size` | Equals header `media_size` |
| 16 | 4 | `u32` | `record_count` | 1..65535 |
| 20 | 2 | `u16` | `record_size` | 64 |
| 22 | 2 | `u16` | `reserved0` | 0 |
| 24 | 4 | `u32` | `record_table_offset` | 32 |
| 28 | 4 | `u32` | `payload_area_offset` | `align_up(32 + record_count × 64, 16)` |

### `NxMediaRecord` — 64 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 2 | `u16` | `media_type` | 1 PNG icon, 2 JPEG icon, 3 NPDM, 4 NACP, 5 opaque source metadata |
| 2 | 1 | `u8` | `codec` | 0 RAW |
| 3 | 1 | `u8` | `flags` | 0 |
| 4 | 8 | `u64` | `stored_size` | 1..64 MiB |
| 12 | 8 | `u64` | `logical_size` | Equals `stored_size` |
| 20 | 8 | `u64` | `data_offset` | Media-relative, 16-byte aligned payload range |
| 28 | 32 | `byte[32]` | `sha256` | SHA-256 of exact payload bytes |
| 60 | 4 | `u32` | `reserved` | 0 |

Media records and payloads are independently addressable. Payload ranges do not overlap records or one another and lie wholly within the Media region. Payloads appear in record-index order; each starts at the minimum 16-byte aligned offset after the previous payload, the first starts at `payload_area_offset`, and the Media region ends at the last payload byte. Every Media record must be referenced. Icon records may be referenced multiple times; every non-icon record is referenced exactly once by its owning Program/Application/Snapshot. Icons preserve original PNG/JPEG bytes; NX does not require image decoding for discovery. Native/opaque media bytes have no NX interpretation.

## 6. Manifest and file tree

The Manifest hash covers all bytes of the Manifest section, including its header, tables, path pool, and zero padding. All Manifest offsets are relative to the section start.

### `NxManifestHeader` — 96 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 4 | `byte[4]` | `magic` | ASCII `NXMF` |
| 4 | 2 | `u16` | `manifest_version` | 1 |
| 6 | 2 | `u16` | `header_size` | 96 |
| 8 | 8 | `u64` | `total_size` | Equals header `manifest_size` |
| 16 | 4 | `u32` | `directory_count` | 1..16,777,216 |
| 20 | 4 | `u32` | `file_count` | 0..33,554,432 |
| 24 | 4 | `u32` | `entry_count` | 0..50,331,648 |
| 28 | 4 | `u32` | `block_reference_count` | At most `0xffffffff` |
| 32 | 8 | `u64` | `directory_table_offset` | Start of `directory_count × 16` bytes; 8-byte aligned |
| 40 | 8 | `u64` | `file_table_offset` | Start of `file_count × 64` bytes; 8-byte aligned, zero iff empty |
| 48 | 8 | `u64` | `entry_table_offset` | Start of `entry_count × 24` bytes; 8-byte aligned |
| 56 | 8 | `u64` | `block_reference_table_offset` | Start of `block_reference_count × 4` bytes; 4-byte aligned, zero iff empty |
| 64 | 8 | `u64` | `path_pool_offset` | Start of path component bytes; 8-byte aligned |
| 72 | 8 | `u64` | `path_pool_size` | At most 2 GiB |
| 80 | 16 | `byte[16]` | `reserved` | All zero |

Tables occur in the order Directory, File, Entry, Block Reference, Path Pool. Their byte ranges are exact and disjoint. Zero-count tables use offset zero. Each present table starts at the minimum required alignment after the previous present table (8 bytes, except Block Reference at 4); Path Pool starts at the minimum 8-byte aligned offset after the last table. Padding is zero.

### `NxDirectory` — 16 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 4 | `u32` | `first_entry_index` | Entry range begins here |
| 4 | 4 | `u32` | `entry_count` | Number of immediate children |
| 8 | 4 | `u32` | `flags` | 0 |
| 12 | 4 | `u32` | `reserved` | 0 |

Directory index is its ID. Entry ranges in Directory ID order partition the Entry table exactly, with no overlap or gap. Empty directories have count zero and a valid insertion position. Snapshot root directories are ordinary Directory IDs; the root has no name.

### `NxDirectoryEntry` — 24 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 8 | `u64` | `name_offset` | Manifest-relative range within path pool |
| 8 | 4 | `u32` | `name_length` | 1..4096 bytes |
| 12 | 4 | `u32` | `target_index` | Directory ID or File ID as selected by `kind` |
| 16 | 1 | `u8` | `kind` | 1 directory, 2 file |
| 17 | 1 | `u8` | `flags` | 0 |
| 18 | 2 | `u16` | `reserved0` | 0 |
| 20 | 4 | `u32` | `reserved1` | 0 |

Names are exact UTF-8 bytes with no NUL, slash, backslash, `.` or `..` component. They are strictly sorted bytewise within each directory, with no duplicate name regardless of target kind. Unicode normalization and case folding are forbidden. Each tree root is walked with cycle detection; directory and file nodes may be reused across snapshots, but no node may be reached twice within one snapshot.

### `NxFile` — 64 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 8 | `u64` | `logical_size` | At most `INT64_MAX` |
| 8 | 4 | `u32` | `first_block_reference_index` | Valid reference range; empty files use current cursor |
| 12 | 4 | `u32` | `block_count` | Exact ceiling of size / chunk size; zero for empty file |
| 16 | 1 | `u8` | `chunk_size_log2` | 16, 17, 18, 19, or 20 |
| 17 | 1 | `u8` | `flags` | 0 |
| 18 | 2 | `u16` | `reserved0` | 0 |
| 20 | 32 | `byte[32]` | `content_sha256` | SHA-256 of the complete uncompressed file; empty file uses SHA-256 of empty bytes |
| 52 | 12 | `byte[12]` | `reserved1` | All zero |

File IDs are record indexes. File block-reference ranges partition the Block Reference table in File ID order. File records may be referenced in different snapshots, but only once in any single snapshot tree. Every block reference length is implied by its position and file size.

Within one application/program, a writer MUST reference the same `NxFile` record for an unchanged file at the same relative path across snapshots. “Unchanged” means equal logical size and byte-for-byte equal contents, verified after a digest match. Such a file therefore keeps the same block size and block-reference sequence. A file at a different path may have a separate `NxFile` record; its identical chunks still share block IDs. Readers validate that a file ID is not repeated in one snapshot tree; full verification checks the referenced bytes against the file digest.

### `NxBlockReference` — 4 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 4 | `u32` | `block_id` | Index in both block arrays, less than header `block_count` |

For a nonempty file, expected count is `ceil(logical_size / (1 << chunk_size_log2))`. Every referenced block's `logical_size` equals the full chunk size except the final block, which equals the remaining bytes. Identical blocks may be referenced by any files or snapshots.

## 7. Data block index and integrity table

There are exactly `block_count` packed `NxBlock` records at `block_index_offset`. The integrity table is a packed array of `u32` CRC-32C values. Block `i` owns `ceil(logical_size_i / check_size)` consecutive entries starting at its `first_check`; entry `first_check + k` is the CRC-32C of logical bytes `[k × check_size, min((k+1) × check_size, logical_size))` of the block. `check_count` is the sum over all blocks. There is no per-block index header.

A RAW block read therefore needs only the 16 KiB slices that cover the requested range, each checked with one hardware CRC-32C (SSE4.2 on x86-64, the ARMv8 CRC extension on ARM64, a table elsewhere). A Zstandard block is decoded completely and all its slices are checked. Strong per-file SHA-256 values in the Manifest remain the full-verification and identity hash.

### `NxBlock` — 24 bytes

| Offset | Size | Type | Field | Allowed value / validation |
|---:|---:|---|---|---|
| 0 | 8 | `u64` | `data_offset` | Absolute archive offset; 16-byte aligned and wholly in Data region |
| 8 | 4 | `u32` | `stored_size` | Nonzero; at most 1 MiB + 64 KiB |
| 12 | 4 | `u32` | `logical_size` | 1..1 MiB |
| 16 | 1 | `u8` | `codec` | 0 RAW, 1 ZSTD |
| 17 | 1 | `u8` | `flags` | 0 |
| 18 | 2 | `u16` | `reserved0` | 0 |
| 20 | 4 | `u32` | `first_check` | Sum of the check counts of all lower block IDs |

Blocks are indexed in strictly increasing physical `data_offset` order and do not overlap. The first block begins at or after `data_offset`; each later block starts at or after the minimum 16-byte aligned offset after the preceding block. In a compact archive every block starts at that minimum offset and the padding is zero; otherwise gaps are free space (§1). `data_size` ends at the last block byte. RAW requires `stored_size == logical_size`. ZSTD is exactly one standard frame, has no dictionary ID, declares content size equal to `logical_size`, has window size at most 1 MiB, consumes exactly `stored_size` bytes, and decodes to exactly `logical_size` bytes. Skippable frames, concatenated frames, trailing bytes, and cross-block dependencies are forbidden. Writers should store a block as RAW when ZSTD does not reduce its size; readers accept either codec when the stated constraints are met.

The Zstandard compression level is not encoded. A packer should avoid repeating compression work for byte-identical input chunks. It MUST compare bytes after a hash candidate match before sharing a block, because hash equality alone is not proof of equality. A writer may use any hash for candidate lookup, for example one derived from the slice CRCs, because the byte comparison decides.

## 8. Applications, updates, and complete views

Every `NxSnapshot` is a complete file-tree view for its application and, when present, executable program. An unassociated DLC snapshot still has a complete tree but is not an executable program view. A reader selects the snapshot and opens files under its root directory; it does not apply updates or traverse a base-to-update chain. Files absent from a later root are deleted from that view. There are no whiteouts or implicit directory overlays.

“Complete” means all NX logical files and source content units required to load the selected version are available in that snapshot's logical tree. A decrypted native NCA may reference another native base content unit in the same view. The NX reader MUST NOT interpret native patch structures; the consuming emulator's native content loader owns that behavior. A native importer MUST reject missing or ambiguous required content units. Native association rules must be verified independently for Switch 1 and Switch 2.

Packer input must explicitly identify the application, any source-provided executable Program ID association, snapshot type/version, and the complete resulting directory tree. Directory names such as `base/`, `updates/`, or `dlc/` do not establish semantics. A partial update input may be used only if the caller selects an exact base snapshot, the packer can resolve the complete required logical content set, and the declared required-base dependency names that exact snapshot identity. Otherwise packaging fails with an ambiguity error.

DLC is represented by a complete DLC snapshot and explicit dependency records when relationships are known. A DLC package is not itself an executable program. If source metadata supplies no real executable Program ID, `program_index` is absent and no `NxProgram` row is created for it. Its owning application association must also come from reliable source metadata. If that relationship is unknown, the packer must not guess it. Switch 1 and Switch 2 identifiers and update relationships are opaque source facts; NX does not assume common grouping rules.

### Adding and removing updates/DLC

The format MUST support adding and removing complete update and DLC snapshots while retaining any other selected versions, **without rewriting or recompressing the base**. An editor MUST serialize concurrent edits of the same archive from before parsing through verification and publication, including compaction. An editor uses one of two transactions:

- **In-place edit (default).** Let `g` be the selected generation. (1) Append the new blocks (deduplicated against existing blocks by byte comparison) starting at or after `align_up(archive_size_g, 16)`. Existing blocks keep their offsets and are never copied. If one header page was lost, append beyond the existing physical end to preserve data that the lost page may have referenced. (2) Write a complete new tail after them, describing the resulting snapshot set; removed blocks are left out of the new index and become free space. (3) Flush and synchronize the file. (4) Write the complete header page, including its zero padding, into the slot opposite the one selected for generation `g`, then flush and synchronize again. Until step 4 completes the archive still opens as generation `g`; an interruption at any point leaves `g` or `g + 1` fully valid. No byte referenced by generation `g` is modified, so an Archive that already mounted `g` stays consistent. Each edit grows the file by its new blocks and one tail.
- **Compaction (full repack).** Rewrites a compact generation-0 archive into a temporary file and atomically replaces the original. It reclaims free space and stale tails and is optional; an editor may suggest it when free space becomes large.

The reference packer appends edits by default. Compaction is explicit unless an API caller opts in with a nonzero `auto_compact_free_percent`. Appended edits can leave unused bytes in the archive; the reference packer reports an estimate of space reclaimable by compaction. An older generation can remain mounted even after its header slot is replaced. An editor MUST preserve bytes reachable by those readers unless it can establish that they have all closed.

**Optional space reclamation.** After validating a removal, an editor MAY deallocate unused Data ranges without changing the logical file size. It MUST keep every range referenced by the published generation, including shared blocks, all metadata, and the header area. Before deallocation, the editor MUST retire and synchronize any header that still references the ranges. It MUST either isolate the changes in a filesystem clone and atomically replace the original, or establish exclusive access that prevents readers from observing the retired ranges. A reclaimed file may have two identical header slots pointing to the surviving tail. If the fast path is unavailable, the logical removal still succeeds and the file may be compacted later.

An added update MUST have a complete required content set, including native base units it needs; an added DLC uses its reliable source associations. Removing a snapshot does not delete a file/block/media item still referenced by any surviving view. An update view may still contain all native base units required for its own loading, but the NX editor MUST reject removing a snapshot named by any surviving dependency record. It MUST NOT silently drop or rewrite those dependencies. Dependency identities describe provenance/selection and do not supply missing runtime bytes; a read-only archive may still contain older provenance pointing outside the archive. An editor MUST NOT claim an incomplete view is complete simply because it records a dependency.

A repack rebuilds application/program ranges, remaps indexes and retained extension references, reselects application display metadata (using the default version rule unless the caller explicitly selects another eligible source), removes unreferenced storage from the index (compaction also from the file), and preserves surviving snapshots' canonical identities. Programs with no surviving snapshots are removed; applications with no surviving snapshots are removed or the operation rejects if that would leave an empty archive. Source units and unrelated versions are never implicitly deleted. Existing stored frames can be copied verbatim without recompression; copying does not constitute payload verification. Repack edits reject dependency targets that are absent from the resulting snapshot set, belong to another application, or violate the dependency's applicable update/base rules, even though a read-only archive may preserve provenance records referring to omitted snapshots.

## 9. Compression, chunking, and deduplication

The required codecs are RAW and independent Zstandard frames. A file's block size is a power of two from 64 KiB through 1 MiB, recorded by `chunk_size_log2`. The current writer accepts 64, 128, 256, 512, and 1,024 KiB. Its default base chunk size is 256 KiB; files whose first chunk retains Zstandard use 64 KiB chunks by default, as described below. These are writer policies, not additional wire-format restrictions. Different files may use different allowed sizes.

A block represents at most one chunk and has no predecessor dependency. Deduplication hashes and compares uncompressed bytes and their lengths. A successful dedup result reuses the same block ID; data may be shared across files and snapshots. A writer MUST deduplicate before compression so an identical logical block is encoded at most once. Compression is performed independently. Compression level affects physical bytes, not logical identity or codec number.

A full rewrite MAY decode a Zstandard block and store the same logical bytes as
RAW. It MUST update the stored size, codec, physical ranges and index/header
digests consistently. The block's logical size and slice CRC-32C values remain
unchanged; file digests, chunk boundaries and canonical snapshot identities
remain unchanged. The reference `compact --no-compress` performs this rewrite
with Strict source/output verification and atomic replacement, preserving shared
block references and retained metadata/recipes. Recipe literal compression is a
separate encoding and is not changed by this operation.

Independent Zstandard frames allow decoding one containing block rather than scanning/decompressing a whole file or archive. Smaller frames bound extra decompression work for small reads, while larger frames can improve ratio and reduce per-block metadata. The size choice remains a benchmark decision; 256 KiB is not claimed optimal. The current packer uses Zstandard level 3 by default, allows levels 1 through 13, constrains the frame window to the configured block size and emits no dictionary. It keeps a frame only when it saves at least 25 % of the block (`min_compression_savings_bp`, default 2500): below that, the I/O saved on a SATA SSD no longer pays for the ~150-200 µs decode, and on NVMe or a warm cache any frame costs latency. Cheap probes skip the full compression attempt on blocks that cannot reach the threshold. A file whose first chunk keeps Zstandard can be written with smaller chunks (`compressed_chunk_bytes`, 64 KiB by default, 0 = off); other files keep their chunk size. On 30XX (Switch 1, about half the bytes compress), 64 KiB chunks cut the median 4 KiB read from 167 to 40 µs and p99 from 319 to 86 µs for a 1.3 % larger archive; on Donkey Kong Bananza (mostly RAW) they cost 3.9 % size for the same median read. Levels were measured on real data with 64 KiB blocks: decode time stays flat up to level 13 (about 24 µs per kept block) and rises about 15 % from level 15, where more blocks also cross the threshold; level 13 saves only about 0.03 %, so 3 (default), 9 and 13 are the useful presets. These are packer defaults, not wire requirements; deterministic bytes are scoped to identical inputs, options, and the same pinned codec build.

## 10. Catalog and path string rules

Catalog strings and Manifest path components are UTF-8 with no NUL. Catalog presentation strings have maximum length 4096 bytes; display-version strings have maximum length 256 bytes; language tags have the limits in their records. They are not normalized or case-folded. A path component additionally forbids `/`, `\\`, `.` and `..`; the archive tree uses directory edges, not slash-delimited stored paths.

Catalog table order is deterministic: applications by `(platform, application ID)`; programs by application index then program ID; snapshots grouped by application with program-associated rows first and unassociated DLC rows last, as specified in §4; localized entries by application then language tag; dependencies by snapshot order and their serialized order; extensions by type ID, owner kind, owner index. Identical UTF-8 string byte sequences in the string pool SHOULD share one exact range. Identical icon payloads of the same media type SHOULD share one Media record, referenced by each language/application that uses it. Values and images preserve source spelling, language distinction and bytes without normalization or recompression. Ties in a required sort key are invalid.

## 11. Versioning, features, and extensions

The supported header versions are (0,2) and (0,3), as defined in §3. Both use header size 256, Catalog version 1, Media version 1, and Manifest version 1. Unsupported versions reject. Draft producer changes must use explicit versioned extensions and required flags when needed; incompatible core-layout or identity changes require a version/algorithm change and regenerated fixtures. After release, a given core version never acquires a different interpretation. Unknown required extensions fail closed, including for older readers of the same core version. The separate-header-pages layout uses required feature bit 0. All other header bits must be zero. Compatible optional Catalog extensions may be added without changing the header version. A new minor version in the header requires explicit support in the reader, even if its core layout is unchanged. A major change may change core interpretation and requires a reader update.

Unknown optional Catalog extensions may be skipped after validating their record and data ranges. Unknown required extensions reject the archive. An implementation must never infer that an unknown extension is optional from its payload. No extension may redefine core fields or alter complete-snapshot semantics.

## 12. Integrity and identity

### Physical and logical integrity

- `header_crc32c` detects accidental damage to a header slot.
- `catalog_sha256` covers the exact Catalog bytes.
- `manifest_sha256` covers the exact Manifest bytes.
- `index_sha256` covers concatenated hot-index and integrity-table bytes, excluding alignment padding.
- Each media record SHA-256 covers its stored raw payload.
- Integrity-table entries are CRC-32C values of 16 KiB slices of uncompressed block bytes (§7).
- Each file record's SHA-256 covers concatenated logical file bytes.

These are integrity checks, not authentication. No archive-wide hash or signature is required. Merely listing a title does not hash every block.

### Canonical snapshot identity

`NxSnapshot.snapshot_identity` is SHA-256 of this exact byte stream:

1. ASCII bytes `NX-SNAPSHOT-ID`, followed by one zero byte.
2. `u16` little-endian identity algorithm version 3.
3. `u8` platform, `u64` little-endian application ID, `u64` little-endian program ID; use `0xffffffffffffffff` for a DLC snapshot without an executable program.
4. `u8` snapshot type, `u32` little-endian numeric content version, and the exact 16-byte `NxSnapshot.content_id` (all zeros if unavailable).
5. `u32` little-endian node count, including the root directory.
6. One node record for every directory and file in the selected snapshot. Sort by the complete relative path's UTF-8 bytes, with root first and having an empty path. Each record begins `u8 kind` (1 directory, 2 file), `u32` little-endian path byte length, and exact path bytes. Full paths join exact components with byte `0x2f`. For a file, append `u64` little-endian logical size and its 32-byte `NxFile.content_sha256`. A directory record has no trailing fields.

Display-version text, Build ID, dependency list, title, publisher, icon, other custom metadata, chunk size, block IDs, compression, offsets, and archive layout are excluded. Thus identity depends on executable scope, content ID, snapshot type/version and logical tree while remaining stable across recompression and display-name edits. Two snapshots MUST NOT have identical identities, even when their nonidentity metadata differs; an ambiguous duplicate is rejected. A snapshot's content hashes are trusted for this fast identity check; full verification recomputes file and block hashes from data.

### Verification levels

1. **Catalog validation:** header CRC, exact Catalog range/hash, Catalog limits, references, strings, media-index bounds, and required extensions. When Media is present it reads its 32-byte header for the media record count, but not media records or payloads. It does not read the Manifest, blocks, or block index.
2. **Mount structural validation:** Catalog plus Manifest hash and all Manifest tables, tree topology, Source Resource roots, snapshot identities, block references, hot block descriptors, media records, section ranges, and padding. Does not decompress Data blocks. Media/block payload hashes may be checked lazily.
3. **Per-read verification:** for RAW, read the slices covering the request and compare their CRC-32C entries; for Zstandard, decode the block and compare all its slice entries, before returning verified bytes.
4. **Full archive verification:** all structural validation, all media hashes, all block slice checks, decode every compressed block, and recompute every file SHA-256.

## 13. Portable reader requirements

The reference reader reads fields from byte spans with explicit little-endian helpers. It must not cast mapped bytes to a compiler structure, assume host alignment, or use host pointers as file offsets. The reference reader captures the hot block index and integrity table under a configurable memory budget and verifies their combined hash before use. It never re-trusts uncaptured descriptors/digests from a mutable source. Other implementations may use an equivalently protected paged design. Bytes referenced by the mounted generation MUST remain immutable for the Archive lifetime, especially when decoded blocks are cached; the in-place edit protocol guarantees this. A random file read maps file offset to a reference index arithmetically, resolves one `NxBlock`, reads the covering RAW slices or decompresses the block, and copies the requested in-block range.

The standalone distributable is `dist/nx_reader.hpp`, generated from the sole C++17 reader implementation `reader/nx_reader.hpp`. An optional `nxcore::reader` CMake target exposes that same implementation; no aggregate C++20 core or CXX bridge is part of the reader. Its API, ownership/thread contract, resource budgets, and validation boundary are documented in [READER.md](READER.md). Zstandard and optional accelerated SHA-256 are supplied through caller callbacks; the wire format does not require Rust, Cargo, NXEmu, a global runtime, or a linked crypto library.

## 14. Reference material

- [NUP specification](https://github.com/a55uka/nup/blob/meow/SPEC.md), [packer](https://github.com/a55uka/nup/blob/meow/tools/nup/nup/pack.py), [reader](https://github.com/a55uka/nup/blob/meow/tools/nup/nup/read.py), and [format structures](https://github.com/a55uka/nup/blob/meow/tools/nup/nup/format.py).
- [Zstandard frame format](https://github.com/meta/zstd/blob/dev/doc/zstd_compression_format.md), [seekable-format overview](https://github.com/meta/zstd/blob/dev/contrib/seekable_format/README.md), and [seekable-format specification](https://github.com/meta/zstd/blob/dev/contrib/seekable_format/zstd_seekable_compression_format.md).
- [EROFS design](https://erofs.docs.kernel.org/en/latest/design.html), [on-disk overview](https://erofs.docs.kernel.org/en/latest/ondisk/index.html), [chunk layout](https://erofs.docs.kernel.org/en/latest/ondisk/chunked_format.html), and [performance FAQ](https://erofs.docs.kernel.org/en/latest/faq.html).
