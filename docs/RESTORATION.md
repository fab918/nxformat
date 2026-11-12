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
only edited file contents change. File sizes may change. This produces complete
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

Level 13 compression reserves additional encoder workspace. A memory budget too
small for the recipe fails explicitly. RAW NCA export (`extract-ncas`) remains
available without creating a restoration recipe.
