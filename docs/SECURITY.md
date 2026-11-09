# Parser and extraction security

Applies to **NXFormat / NXConvert 1.0.1**.

NX archives are untrusted input. A reader must validate fields before allocation, mapping, indexing, decompression, or extraction. Failure returns a bounded error; it must not continue with partial or guessed structures.

## Required checks

- Validate the complete fixed header, magic, supported version, header size, CRC-32C, feature flags, declared archive size, and every region range before reading a region. At mount, verify `index_sha256` across the exact hot-index bytes followed by integrity-table bytes before trusting either table.
- Use checked addition and multiplication for `offset + length`, `count * record_size`, `ceil(file_size / chunk_size)`, and all conversions to host-sized types. Reject overflow and values above `INT64_MAX` before calling file APIs.
- Require regions to be ordered and non-overlapping; verify alignment, section sizes, table offsets, table counts, string references, media indices, directory/file indices, block-reference ranges, and per-block data ranges.
- Require reserved fields and padding bytes to be zero. Reject unknown required header feature bits and unknown required Catalog extensions. Skip only well-formed unknown optional extensions.
- Validate all directory entry names as nonempty UTF-8 components without NUL, `/`, `\\`, `.` or `..`. Compare names as exact UTF-8 byte sequences; do not case-fold or normalize Unicode. Reject duplicate sibling names and entries that are not bytewise sorted.
- Bound traversal depth and detect directory cycles. A directory node may be shared between snapshots, but within one snapshot it must not be reachable more than once. A file record must not appear at multiple paths within one snapshot.
- Validate that every file's block count equals the ceiling of size divided by its declared block size; empty files have zero references and SHA-256 of the empty byte sequence. Every reference must name an existing block whose uncompressed length equals the expected chunk length.
- Accept only RAW or one-frame Zstandard blocks. RAW stored and logical sizes must match. Zstandard blocks must have no dictionary, declare the expected decompressed size, use at most a 1 MiB window, contain exactly one complete frame and no trailing bytes, and produce no more than the declared logical size.
- Validate media payload sizes and hashes before use. Media records are uncompressed in NX 0.2 and 0.3; unknown media types may be retained/skipped only if their record is structurally valid.
- During extraction, resolve the destination path beneath the chosen extraction root using filesystem-safe APIs. Do not concatenate unchecked archive strings into paths. Reject symlink traversal and destination-specific reserved names, path length limits, case collisions, and names the target filesystem cannot represent. Report these separately from archive path validity.

## Normative resource limits for NX 0.2 and 0.3

| Resource | Maximum |
|---|---:|
| Archive size | `INT64_MAX` bytes |
| Fast Catalog | 16 MiB |
| Applications / programs / snapshots | 4,096 / 16,384 / 65,536 |
| Localized entries / dependencies / extensions | 16,384 / 32,768 / 4,096 |
| Media records / one media payload / Media region | 65,535 / 64 MiB / 4 GiB |
| Manifest section | 4 GiB |
| Directories / files / entries | 16,777,216 / 33,554,432 / 50,331,648 |
| One path component / aggregate path pool | 4,096 bytes / 2 GiB |
| Block count | `2^32` |
| One file's block-reference count | `2^32-1` |
| File logical size | `INT64_MAX` bytes, also constrained by its block count and chunk size |
| Directory depth | 256 |
| Zstandard window / decoded block | 1 MiB |

These are format rejection limits, not suggestions. Implementations may impose lower per-mount budgets and return a resource-limit error. They must not raise the limits above these values in NX 0.2 and 0.3. The reader must budget index storage before allocation. The reference reader captures and authenticates both index tables under max_index_bytes; implementations using paging must provide equivalent stable integrity. Backing sources must remain immutable for the Archive lifetime. A default mount budget should cap concurrently decoded block buffers (recommended initial cap: 8 MiB total); this is an implementation setting, not a cache design mandate.

## Error behavior

The reader distinguishes unsupported version/feature, malformed structure, integrity mismatch, decompression error, resource-limit rejection, and I/O failure. It never treats a failed hash as advisory. A random-access read verifies the relevant 16 KiB logical slices before returning bytes when block verification is enabled; otherwise callers can choose a faster read policy and request verification explicitly. A Zstandard failure or wrong output length is always an error.

Fast Catalog validation checks the header CRC, Catalog range/hash, catalog limits, record/string references, media-index bounds using the Media region header count, application/program/snapshot/metadata associations, and required extensions. Mount-time structural validation also checks the Manifest hash, block-index/integrity hash, all ranges and references, file/directory topology, block descriptors, media descriptor types, and all snapshot identities, without decompressing every game block. Reads can validate the CRC-32C of each relevant 16 KiB logical slice. Full verification visits every block and recomputes every file content hash.

## Integrity limits

SHA-256 and CRC detect accidental or malicious modification only when the relevant trusted digest remains unchanged. Since an attacker can replace the archive header and its CRC, NX provides no signature, key management, or authenticity. Do not treat the format as proof of publisher origin.

## Decrypted-source import boundary

The Rust importer MUST reject encrypted or partially decrypted content, malformed source-container structures, missing metadata needed to establish an unambiguous snapshot, and unresolved partial updates. It MUST validate actual source headers and internal fields rather than trusting a filename extension or the `DNCA` marker alone. The converter does not load keys or decrypt content. Importers parse untrusted inputs and must use checked offsets/counts, bounded allocations, and safe path validation before writing; see [CLI.md](CLI.md#source-import-contract).

The reference reader also bounds cumulative snapshot visits/path bytes, supports cancellation and has an optional synchronized verified-block cache with separate payload-byte and entry-count limits. Full physical verification bypasses this cache. The Rust writer caps combined block/media staging and output temporary disk, constrains Zstandard to one single-threaded 1 MiB-or-smaller-window context, checks cancellation between bounded blocks, and verifies compressed output by exact-frame decoding plus logical slice CRC-32C before publication. See [READER.md](READER.md) and [CLI.md](CLI.md) for current defaults.

## Existing-archive repack integrity

`IntegrityPolicy::Fast` validates the header, Catalog, Manifest, hot block index, integrity-table hash, media descriptors, references, and snapshot/file relationships before editing. It copies retained stored frames verbatim and does not decode or recompute payload CRC-32C for those retained blocks; a slice CRC covered by the unkeyed index hash is not proof that the payload itself matches. New unique data is still deduplicated only after digest-and-byte comparison and the default writer read-verifies newly encoded output before publication.

`IntegrityPolicy::Strict` additionally decodes every source physical block (including unreferenced blocks), checks each logical slice CRC-32C, verifies Media payload hashes, and verifies every logical File hash before editing. Within one verification pass, a bounded cache may reuse a successful file check only when size, digest, chunk size and the exact block-reference sequence all match a previously verified File record; every physical block is still checked, including unreferenced blocks. It also verifies the staged output before publish. Both modes reject structural errors. In-place edits lock the archive before parsing and keep the lock through verification and publication. Append edits synchronize the new tail before committing its header slot. Compaction stages and verifies a same-directory temporary file before replacing the destination. A plan that would leave no snapshots is rejected because a valid NX archive requires at least one snapshot.

`compact --no-compress` uses Strict verification before rewriting retained
Zstandard blocks as RAW, then verifies the complete staged result before atomic
replacement. Decoder buffers are budgeted and output expansion is checked against
archive and temporary-disk limits. Failure leaves the original archive intact.

## NUP staging limits

NUP imports preflight declared decoded bytes before creating staging files.
Every decoded write, metadata file and inherited-file copy is charged before
writing. Hard links do not duplicate file content. Staged files reduce the
writer temporary-disk budget until the transaction ends; cleanup runs on failure,
cancellation and success. The byte budget excludes filesystem metadata and
allocation granularity, and does not reserve free space from other processes.

## Current NUP import limits

NUP names use the portable ASCII extracted-tree profile; non-ASCII names are
rejected. The streaming NUP decoder uses its default Zstandard window limit
(currently 128 MiB), independently of the NX block limit of 1 MiB.

The importer currently interprets the documented NUP v1 metadata TLV IDs only.
Historical producers that shifted these IDs by one can yield incorrect display
titles, publishers or versions; `check-nup` checks paths, NACP and file bytes,
not every display TLV. Review display metadata when migrating such archives.
These display values do not establish execution identity.

## Independent source resources (NX 0.3)

Source Resource descriptors are authenticated by the Catalog digest and contain
only bounded Manifest directory references. Total resources are limited to 4096.
Roots are independent flat file sets; invalid kinds, duplicate roots, repeated
file IDs, references into game directory trees and out-of-range targets reject
before exposing files. Kind 1 has exactly one `partition.hfs0` file. All files use
the ordinary block decoder limits, slice checks and full-file SHA-256 validation.
A resource kind is not evidence that native firmware is valid or installable.
Source preservation never interprets unknown source bytes as game content.
