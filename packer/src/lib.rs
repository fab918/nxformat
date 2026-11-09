//! Source-independent NX 0.2 writer.
//!
//! The crate accepts explicit complete logical snapshots and byte streams. It
//! does not decrypt content or infer identifiers. The read-only [`native`]
//! module inventories decrypted DNSP/DXCI sources from their native records.

mod hash;
pub mod import;
mod model;
pub mod native;
pub mod read_profile;
pub mod recipe;
pub mod source_file;
mod writer;

pub use model::{
    ApplicationInput, ArchiveInput, DependencyInput, DependencyKind, DirectoryInput,
    DisplayFallback, EntryInput, FileInput, FileReadProfile, LocalizedMetadata, MediaInput,
    MediaType, NodeInput, Platform, ProgramInput, ProgramKind, SnapshotInput, SnapshotMetadata,
    SnapshotNativeMetadata, SnapshotType, SourceResourceInput,
};
pub use writer::{
    add_native_content, add_native_contents, compact_archive, edit_archive_metadata,
    edit_archive_recipes, inspect_archive, inspect_snapshot_files, inspect_source_resources,
    read_snapshot_display, read_snapshot_file_range, read_snapshot_nacp, repack_archive,
    write_archive, ApplicationSummary, ArchiveSummary, BlockSizing, CancellationToken,
    CompressionMode, IntegrityPolicy, MetadataEdit, MetadataSourceSelection, NativeAddRequest,
    NativeAddStats, ProgramSummary, ProgressCounter, RepackPlan, RepackStats,
    SnapshotBlockReference, SnapshotDisplay, SnapshotFileReference, SnapshotSummary,
    SourceFileSummary, VerificationStats, WriterError, WriterErrorKind, WriterOptions, WriterStats,
    MAX_EMBEDDED_RECIPE_BYTES,
};

pub mod cli;
pub mod conversion;
