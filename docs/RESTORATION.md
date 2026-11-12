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

For encoder-independent long-term storage, use
`nxconvert extract GAME.nx NEW_FOLDER --portable-restore`, or enable **Portable
restoration** in the desktop application. This mode stores original block bytes
as literals and never emits encoder-dependent reconstruction operations. It
still uses a standard Zstd level 13 frame, so exact replay needs a decoder but
not the original encoder. The recipe can approach the original archive size.
Both modes warn about that possible additional disk cost before extraction;
`--no-restore` and the desktop checkbox can disable recipe creation.

During an edited repack, literal metadata regions are recovered into a private
sparse temporary capsule. Game data holes are not used as source content. Native
media is streamed and SHA-256 checked; resource blocks are retained literally,
even if originally deduplicated with a game file, then streamed and verified.
An older recipe missing a required resource fails explicitly instead of silently
dropping it. No new NX field or extension is introduced.

The capsule has the original NX's logical size. On filesystems without effective
sparse-file support it may occupy that full size physically, alongside the new NX
and writer staging files. Budget for all of these in addition to the extracted
folder and recipe. `--temp-mib` conservatively charges the capsule's full logical
size even when sparse holes save physical space; it also bounds the writer's
remaining temporary space. The CLI and desktop log this warning before restoration.
A disk-limit or I/O failure removes the temporary output and capsule, while
retaining the extracted folder and recipe.

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
