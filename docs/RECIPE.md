# Rebuild recipes

Applies to **NXFormat / NXConvert 1.0.2** (reads existing recipes unchanged).

A recipe rebuilds an original package (supported decrypted DNSP or DXCI) byte for byte from the NX it was converted into. Recipes are
kept **inside the NX**, in the optional Catalog extension `0x00010006`
(see [SPEC](SPEC.md)): one NX file, no sidecar. Readers that do
not know the extension skip it, and an NX without recipes is complete for
emulation. The same bytes can also be written as a standalone `.nxrecipe`
file, which `nxconvert recipe` can put back inside an NX.

## Inside the NX

`nxconvert pack` and `nxconvert add` keep the recipe of each source by
default (`--no-recipe` skips it). The source is read for its recipe while the
conversion reads it, so the recipe adds no second pass, and a requested recipe that cannot be made or kept fails the operation. A new
`pack` is published only after its recipes have been stored successfully. On `add`,
content groups already committed remain if recipe creation fails.
`nxconvert contents` lists recipes (`@RECIPE` lines),
`nxconvert rebuild ARCHIVE.nx FOLDER [RECIPE_ID...]` writes the original files
back into an existing folder, and `nxconvert check-recipe ARCHIVE.nx` replays them all.

Recipes belong to the application whose snapshots hold their files. Every NX
edit (add, remove, compact) keeps a recipe only while every file it names is
still in the archive, so removing an update also removes the recipe of its
package, and adding it again brings it back. Adding the same
recipe twice keeps one copy. All recipes of one archive together are limited to 8 MiB. Firmware is stored
in block-backed Source Resources, outside the Catalog. The recipe contains a
file reference to the whole original system partition, so its size is independent
of the firmware payload. An oversized recipe fails explicitly.

## Why it is small

The NX already stores every NCA byte-exact. What a package adds around them is
small: the PFS0/HFS0 tables and padding, the XCI header and certificate area,
tickets, certificates and `.cnmt.xml`. The recipe keeps only that, compressed.

| Package | Size | Recipe | Rebuild |
|---|---|---|---|
| 20XX base + update gamecard (DXCI) | 1,428,312,064 B | 1,878 B | 4.9 s, SHA-256 identical |
| 20XX 1.1.1 update (DNSP, with ticket) | 141,346,066 B | 2,890 B | 0.5 s, SHA-256 identical |

A gamecard system-update partition is retained as an independent Source Resource
by default, even with `--no-recipe`. Rebuilding references its file digest and
places the entire partition at its original source offset. All container tables,
unknown system files and padding remain byte-exact. `--omit-firmware --no-recipe`
omits that resource and skips recipe creation to save space. To restore a
firmware partition missing from both an archive and its recipe, reconvert the
original DXCI.

## Segments

The recipe describes the original file as consecutive segments from byte 0 to
its end:

- **file** (kind 2): the whole content of one file stored in the NX, found by
  its size and SHA-256 in any snapshot or Source Resource;
- **fill** (kind 1): a run of one byte value of 64 KiB or more outside the
  package entries (for example 0xFF up to the card size);
- **literal** (kind 0): every other byte, read in order from one Zstd frame.

Package entries are the ones the native inventory lists (all PFS0 entries, and
the HFS0 entries outside the gamecard update partition, plus the complete
update partition itself). An entry whose bytes
are not in the NX is literal.

## Layout, version 1 (little-endian)

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic `NXRECIPE` |
| 8 | 2 | version, 1 |
| 10 | 2 | reserved, 0 |
| 12 | 4 | segment count, 1 to 2^20 |
| 16 | 8 | original size, the sum of the segment lengths |
| 24 | 32 | SHA-256 of the original |
| 56 | 8 | literal bytes, the sum of literal segment lengths |
| 64 | 4 | name length, at most 4096 |
| 68 | 4 | reserved, 0 |
| 72 | n | original file name, UTF-8 (a hint for user interfaces) |
| | 48 x count | segments |
| | rest | one Zstd frame holding the literal bytes |
| end - 32 | 32 | SHA-256 of every byte before it |

Segment row: kind `u8`, fill byte `u8` (zero unless kind 1), 6 zero bytes,
length `u64` (nonzero), then the file SHA-256 for kind 2 or 32 zero bytes.

## Rebuilding

1. Check the trailer, then every header and segment rule.
2. List the files of every snapshot in the NX; every file segment must be
   present, or the rebuild is refused before writing.
3. Write the segments to a same-directory temporary file, checking each file
   segment's SHA-256 and, at the end, the original SHA-256 and that the literal
   frame is fully used.
4. Publish atomically; an existing output is never overwritten.

Removing a snapshot from the NX can remove files a recipe needs (for example
the update of a combined package). A recipe kept inside the NX is then removed
by the same edit; a standalone `.nxrecipe` fails cleanly instead.
