# Reversible extraction

`nxconvert extract GAME.nx NEW_FOLDER` writes the game files plus
`NEW_FOLDER/.nxrestore.zst`. Keep that file with the extracted folder.

To repack, drop the whole folder into NXConvert, or run either command:

```
nxconvert pack-tree NEW_FOLDER RESTORED.nx
nxconvert restore NEW_FOLDER RESTORED.nx
```

If every exported path and file is unchanged, restoration produces the **exact
original NX bytes**, including its NCA layout, BKTR patch data, metadata, original
compression, source resources and embedded DNSP/DXCI recipes. The final SHA-256
must match before publication. The original NX is not needed for restoration.
Existing NX archives require no migration or reconversion; their wire format,
serialization, C/C++ readers and FFI are unchanged.

If files were edited, the saved explicit declarations drive the existing tree
writer. Paths remain relative to the same application/Program/version snapshot;
only edited file contents change. Native NACP, default and localized icons,
localized titles, opaque metadata, required firmware, Program metadata and
block-backed source resources are retained. The original application metadata
source selection is reapplied to the newly written snapshot identities. File sizes may change. This produces complete
materialized views, not native NCA containers or a byte-identical original NX;
its size can differ. Original-package rebuild recipes are not attached to this
modified NX because their original byte hashes no longer describe its content.
No original DNSP reconstruction is promised after editing. The source extraction
and its recipe are kept, so reverting the edits allows exact restoration again.

Unchanged roundtrip validation on 30XX restores the exact 820,011,768-byte NX and
its three original DNSP packages. Editing one DLC RomFS file, including its size,
then repacking and reextracting changes exactly that path among 11,953 files.

## Compact recipe, independent of the NX format

The restoration file is a standalone **Zstandard frame compressed at level 13**,
with a maximum 8 MiB history and a frame checksum. Its uncompressed format magic
is `NXRSTR02`. The stream is also protected by a SHA-256 footer.

The recipe describes the original physical NX in order:

- Literal runs preserve headers, catalog, manifest, metadata and other bytes that
  cannot be read from the exported files. This includes embedded package recipes
  and any unused bytes that must be kept for byte-exact restoration.
- Reconstructed blocks gather file ranges by inventory ID and offset, plus small
  literal gaps. RAW blocks are copied; Zstd blocks use their recorded compression
  level/window. The encoder settings are selected only after reproducing the
  original block bytes during recipe creation. If reproduction is unavailable,
  that original stored block is retained as a literal instead.
- IVFC leaf SHA-256 tables whose source blocks lie within exported files are
  recomputed. Consecutive hashes are stored as a run instead of retaining their
  incompressible hash bytes or repeating an instruction for each hash.

Paths are stored once and referenced by integer IDs. One fingerprint of the
sorted path/type/size/content-hash inventory detects edits; per-file and per-block
hash copies are omitted because the inventory fingerprint, recipe footer and
final original-NX SHA-256 provide the required checks.

On the 30XX test archive this reduced an initial 8,692,673-byte recipe to
**1,804,114 bytes**, about 79% smaller. Exact size depends on the archive, including
unmapped content, stale generations, metadata and source resources. The recipe
is not assumed to contain metadata only.

## Integrity and resource limits

All source and output paths use the pinned source-tree reader. Traversal, path
lengths, inventories, derived hash blocks, decompression expansion and temporary
disk usage are bounded. Symlinks, invalid references, corrupt/truncated recipes,
cancellation and pre-existing outputs fail without publishing an output or
altering the source. Files outside declared snapshot roots are rejected during
an edited repack rather than silently dropped.

The whole recipe is verified in both unchanged and edited modes. Unchanged
restoration additionally checks the original NX SHA-256 and validates the archive
before atomically publishing it without overwriting an existing destination.
Original Zstd blocks depend on reproducible encoder output; an incompatible
future Zstd encoder fails the final SHA-256 check instead of publishing different
bytes. The bundled Zstd dependency is pinned.

Compact reconstruction is the only recipe creation mode. `--no-restore` and
the desktop checkbox can disable recipe creation. Existing recipes that retain
original blocks as literals remain readable, including recipes produced by the
former portable mode; the restoration stream format is unchanged.

On the local Kirby Switch 2 roundtrip, a 15,969,850,688-byte NX extracted to
17,773,456,082 bytes of files and a **6,559,628-byte compact recipe**. A repeated
extraction with periodic file-size sampling peaked at 17,780,015,710 bytes
of file content, including its temporary recipe; no original-size NX copy was
observed during extraction. These are file lengths, excluding filesystem
allocation overhead and the already existing input NX. Restoring produced the
exact original SHA-256, and both extractions produced identical recipe bytes.
This is a measured case, not a fixed recipe-size guarantee for other archives.

During an edited repack, only the header and active metadata tail (tables and
native media) are retained in a compact private store. An in-memory range map
translates original NX offsets to packed offsets; no sparse or full-size original
NX is created. Game payloads, including literal blocks in portable recipes and
obsolete archive generations, are discarded while the recipe is checked.

If source resources exist, a second bounded streaming pass copies only their
original stored blocks, deduplicated by range. Both recipe passes check the same
SHA-256, and original native media and resources are streamed and independently
verified before publication. An older recipe missing a required resource fails
explicitly instead of silently dropping it. No new NX field or extension is
introduced, and existing restoration recipes remain readable.

Temporary space is the compact metadata/resource store plus the new NX and the
writer's staging needs. It no longer includes a capsule the size of the original
NX, and does not depend on sparse-file support. Large original source resources
still need their actual stored size. `--temp-mib` bounds the combined temporary
space; the CLI reports `metadata_staged_bytes` for the compact store. A failure
removes temporary files while retaining the extracted folder and recipe.

For a new NX around 30 GB with small metadata/resources, edited restoration
normally needs approximately that new NX size plus metadata and staging overhead,
not a fixed additional 30 GB copy of the original. Multi-pass imports and automatic
compaction can still increase peak space, so 30 GB is not an unconditional bound.
The writer streams compressed blocks into its eventual output. The final
selected-metadata update appends a tail with automatic compaction disabled,
avoiding a full payload copy.

Level 13 compression reserves additional encoder workspace. A memory budget too
small for the recipe fails explicitly. RAW NCA export (`extract-ncas`) remains
available without creating a restoration recipe.

## Extraction without a restoration recipe

Recipe creation remains enabled by default. Use
`nxconvert extract GAME.nx NEW_FOLDER --no-restore` or clear **Keep restoration
recipe after extraction** in the desktop application to skip it.

Game-file extraction and recipe creation are separate phases. A failed file
extraction still removes its incomplete output. Once the game files have been
successfully extracted, a memory/disk/I/O/cancellation error creating the recipe
keeps those files and emits a warning. A missing recipe means exact NX/package
restoration is unavailable; normal tree import remains possible.

Program NCAs without a RomFS section export their verified ExeFS and report that
RomFS is absent. Data-only updates resolve an inherited Program through exact
RequiredBase identities and the same application/Program association. Unsupported
BKTR layouts still fail explicitly.

## Bounded encoder discovery

Recipe creation caches up to four exact encoder configurations and allows at
most four exhaustive searches for the whole archive. A known configuration is
always compared byte for byte with the original frame. If discovery has exhausted
its budget or no candidate matches, the original stored bytes are retained as a
literal; a similar or approximately recompressed frame is never used.

`benchmarks/restore_unreproducible.py` builds a streamed, valid, multi-GiB archive
of unique checksum-enabled Zstd frames that the NX encoder cannot reproduce. The
benchmark reports physical/logical archive sizes, creation time, compression
attempts, exhaustive searches and literal fallback blocks. Run it with Python
3.14 and `google-crc32c` as described in the script. It uses bounded payload memory
and cleans up its temporary source, extracted data and recipe automatically.

## Unsupported snapshots and explicit recovery

Some snapshot types, including `Unknown`, have no extracted-tree representation.
Extraction warns when they are skipped. The recipe still preserves them for an
unchanged byte-exact roundtrip. Edited restoration checks that every original
snapshot is represented by the declarations and rejects reconstruction otherwise;
it never silently drops an unexported snapshot.

When the restoration recipe is invalid, `pack-tree` fails by default and leaves
it intact. Explicitly use `pack-tree FOLDER NEW.nx --ignore-restore` to use normal
tree discovery/import without the recipe. This can preserve only the exported
files, not original NX/package bytes or native metadata stored only in the recipe.
New filesystem exports include the independent `extracted.nximport` declarations,
which normal import uses automatically even for multiple Programs and applications.
Older noncanonical exports without this file still require an explicit manifest.
The desktop keeps discoverable files visible with an invalid-recipe warning and
requires **Rebuild normally without the restoration recipe** before selecting
that source. Valid recipe folders expose the same explicit normal-import choice.

## Machine status and required recipes

After successfully extracting game files, the CLI emits one tab-separated line:
`@RESTORE_STATUS` followed by `created`, `failed`, `disabled` (`--no-restore`), or
`not-applicable` (raw NCA export). `created` confirms the recipe was published;
`@RESTORATION` also reports its path, size and referenced bytes. These lines are
also delivered to an in-process CLI output callback.

The default exit status still permits a successful extraction with recipe failure,
keeping the extracted files and warning about the missing recipe. Automation that
requires reversible extraction should use `extract GAME.nx FOLDER --require-restore`:
a recipe failure then returns an error/nonzero exit status, still emits `failed`,
and retains extracted files. It cannot be combined with `--no-restore`.
An extraction failure itself returns an error before this
recipe status is emitted.

## Independent declarations and platform-qualified paths

Every full filesystem export attempts to publish `extracted.nximport` independently
of recipe generation, including `--no-restore` and recipe-failure cases. It uses
the existing NXIMPORT text grammar. Platform/application-prefixed and multi-Program
roots, versions, DLC ownership and symbolic dependencies are explicit rather than
inferred. Multi-application prefixes are `switch1/<application id>/` and
`switch2/<application id>/`, avoiding cross-platform ID collisions.

`pack-tree FOLDER NEW.nx` uses this sidecar when no recipe is present;
`--ignore-restore` chooses it explicitly when a recipe exists. The desktop uses
the same declarations for discovery. A malformed sidecar is rejected rather than
replaced with guessed associations. Normal import does not restore recipe-only
native assets/resources or original-package recipes. Whole-export import retains
all declared snapshots; partial root selection requires an explicit manifest.
The sidecar is export metadata outside the snapshot game-file roots. New recipes
include it in their inventory, so retain it unchanged for byte-exact replay.

Recipe creation reuses the decoder's original Zstd frame or RAW logical buffer.
It no longer allocates a second source-block buffer or rereads the block just to
preserve original bytes. `benchmarks/restore_read_reuse.py` compares a baseline
binary against the current implementation with alternating runs, RAW and foreign
Zstd inputs, identical recipe SHA-256 and exact replay checks. Filesystem cache
is not flushed, so logical read reduction is not a physical-device speed claim.

The recorded comparison (`benchmarks/restoration-read-reuse-m4pro.json`) uses
1 GiB of unique data per codec, three runs per binary in alternating order.
Median recipe time was 7.699 s before/after for RAW and 13.822 s before versus
13.662 s after for foreign Zstd. About 1 GiB of logical block rereads per case is
removed, but the measured warm-cache wall-time gain is small (none for RAW,
about 1.2% for Zstd). There is no claimed physical I/O or slow-device speedup.
Recipe bytes match between binaries and exact replay is independently verified.

## Internally compressed native RomFS

The native extractor reads compression tables and RAW, zero and LZ4 entries in
an NCA's integrity-protected data region, after BKTR resolution when applicable.
The original IVFC storage is verified before decompression. Reserved whole hash
blocks which are unreachable from the declared data extent are not interpreted
as live hashes; every reachable hash block and all live data remain checked.

The compression index and a single 64 KiB decoded block are bounded by the
configured memory limit; the full decompressed section is never staged. The
same file paths feed normal edited repacking. Unknown compression types, sparse
and metadata-hashed storage and unsupported tree shapes still fail explicitly.

A compact restoration recipe retains original LZ4 storage bytes literally rather
than guessing an encoder configuration. This preserves exact original NX/package
replay, but the recipe may approach source size for these snapshots. The NX format
and restoration opcodes do not change. An unchanged roundtrip remains byte exact.

Opt-in read-only tests can inspect local native inputs without publishing game
files: set `NXFORMAT_COMPRESSED_PROBES` to a TSV containing label, package path,
NCA offset, NCA size, base-package path (or `-`), base-NCA offset and size; then run
`cargo test --release --manifest-path packer/Cargo.toml real_compressed_romfs_read_only_probe -- --ignored --nocapture`.
The probe verifies IVFC and streams all decompressed files into SHA-256 digests.
