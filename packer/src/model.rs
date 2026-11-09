use std::io::{Cursor, Read};

/// Platform as explicitly established by validated caller metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Platform {
    Unknown = 0,
    Switch1 = 1,
    Switch2 = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ProgramKind {
    Primary = 0,
    Auxiliary = 1,
    Unknown = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[repr(u8)]
pub enum SnapshotType {
    Base = 0,
    Update = 1,
    Dlc = 2,
    StandaloneUpdate = 3,
    Unknown = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DependencyKind {
    RequiredBase = 1,
    Requires = 2,
    Recommends = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum MediaType {
    PngIcon = 1,
    JpegIcon = 2,
    Npdm = 3,
    Nacp = 4,
    Opaque = 5,
}

/// A single caller-owned sequential source. `size` is checked against bytes
/// actually returned by the reader, including rejection of trailing bytes.
pub struct FileInput {
    pub size: u64,
    pub reader: Box<dyn Read>,
    /// Optional observed accesses, never inferred from names or content.
    pub read_profile: FileReadProfile,
    pub(crate) captured: Option<std::sync::Arc<crate::read_profile::CapturedFileProfile>>,
    pub(crate) adaptive_disabled: bool,
    /// Optional read-ahead: reads the source in the background and discards
    /// the bytes, so the real read finds them in the system cache.
    pub(crate) warm: Option<WarmUp>,
    /// Same bytes as a file written earlier in the same archive (see
    /// [`FileShare`]).
    pub(crate) share: Option<FileShare>,
}
/// See [`FileInput::warm`].
pub(crate) type WarmUp = std::sync::Arc<dyn Fn() + Send + Sync>;

/// One file stored once and named by several snapshots of one write, without
/// reading it again: the writer records the File ID of the `Publish` side in
/// the slot, and an `Alias` reuses that record. The alias must come later in
/// the write (a base before its update); the writer refuses it otherwise.
#[derive(Clone, Debug)]
pub(crate) enum FileShare {
    Publish(std::sync::Arc<std::sync::OnceLock<u32>>),
    Alias(std::sync::Arc<std::sync::OnceLock<u32>>),
}

/// Bounded summary of a representative caller-supplied trace. It contains no
/// native identity/provenance claim and is never serialized into NX metadata.
#[derive(Clone, Copy, Debug, Default)]
pub struct FileReadProfile {
    observations: u64,
    largest_read: u32,
    previous_end: Option<u64>,
    contiguous: u64,
    maximum_end: u64,
    invalid: bool,
}
impl FileReadProfile {
    pub(crate) fn observe_stream(&mut self, offset: u64, bytes: u32, previous: Option<u64>) {
        self.previous_end = previous;
        self.observe(offset, bytes);
    }
    /// Record completed successful bytes, not a request past EOF. Callers bind
    /// representative traces to the matching file/version before attaching them.
    pub fn observe(&mut self, offset: u64, bytes: u32) {
        if bytes != 0 {
            if self.previous_end == Some(offset) {
                self.contiguous = self.contiguous.saturating_add(1);
            }
            self.previous_end = offset.checked_add(bytes as u64);
            if let Some(end) = self.previous_end {
                self.maximum_end = self.maximum_end.max(end);
            } else {
                self.invalid = true;
            }
            self.observations = self.observations.saturating_add(1);
            self.largest_read = self.largest_read.max(bytes);
        }
    }
    pub fn observations(self) -> u64 {
        self.observations
    }
    pub fn largest_read(self) -> u32 {
        self.largest_read
    }
    pub fn contiguous_reads(self) -> u64 {
        self.contiguous
    }
    pub(crate) fn fits(self, size: u64) -> bool {
        !self.invalid && self.maximum_end <= size
    }
}

impl FileInput {
    pub fn new<R: Read + 'static>(size: u64, reader: R) -> Self {
        Self {
            size,
            reader: Box::new(reader),
            read_profile: FileReadProfile::default(),
            captured: None,
            adaptive_disabled: false,
            warm: None,
            share: None,
        }
    }
    /// Names the file of `slot`, written earlier in the same archive.
    pub(crate) fn alias(size: u64, slot: std::sync::Arc<std::sync::OnceLock<u32>>) -> Self {
        let mut file = Self::new(size, std::io::empty());
        file.share = Some(FileShare::Alias(slot));
        file
    }
    /// Records this file's File ID in `slot` once it is written.
    pub(crate) fn with_publish(mut self, slot: std::sync::Arc<std::sync::OnceLock<u32>>) -> Self {
        self.share = Some(FileShare::Publish(slot));
        self
    }
    /// Lets the writer read this file ahead of time in the background.
    pub(crate) fn with_warm_up(mut self, warm: WarmUp) -> Self {
        self.warm = Some(warm);
        self
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let size = bytes.len() as u64;
        Self::new(size, Cursor::new(bytes))
    }
    pub fn with_read_profile(mut self, profile: FileReadProfile) -> Self {
        self.read_profile = profile;
        self
    }
    pub fn with_captured_profile(
        mut self,
        profile: crate::read_profile::CapturedFileProfile,
    ) -> Self {
        self.read_profile = profile.profile;
        self.captured = Some(std::sync::Arc::new(profile));
        self
    }
}

/// A native media payload. PNG/JPEG bytes are preserved exactly as supplied.
pub struct MediaInput {
    pub media_type: MediaType,
    pub size: u64,
    pub reader: Box<dyn Read>,
}

impl MediaInput {
    pub fn new<R: Read + 'static>(media_type: MediaType, size: u64, reader: R) -> Self {
        Self {
            media_type,
            size,
            reader: Box::new(reader),
        }
    }

    pub fn from_bytes(media_type: MediaType, bytes: Vec<u8>) -> Self {
        let size = bytes.len() as u64;
        Self::new(media_type, size, Cursor::new(bytes))
    }
}

pub struct EntryInput {
    pub name: String,
    pub node: NodeInput,
}

pub enum NodeInput {
    Directory(DirectoryInput),
    File(FileInput),
}

pub struct DirectoryInput {
    pub entries: Vec<EntryInput>,
}

impl DirectoryInput {
    pub fn new(entries: Vec<EntryInput>) -> Self {
        Self { entries }
    }
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl EntryInput {
    pub fn directory(name: impl Into<String>, directory: DirectoryInput) -> Self {
        Self {
            name: name.into(),
            node: NodeInput::Directory(directory),
        }
    }
    pub fn file(name: impl Into<String>, file: FileInput) -> Self {
        Self {
            name: name.into(),
            node: NodeInput::File(file),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependencyInput {
    pub kind: DependencyKind,
    /// Exact source-provided identifier; `None` means unavailable.
    pub content_id: Option<[u8; 16]>,
    /// Exact identity of the referenced snapshot, never inferred from a name.
    pub target_snapshot_identity: [u8; 32],
}

pub struct ProgramInput {
    pub program_id: u64,
    pub kind: ProgramKind,
    pub content_id: Option<[u8; 16]>,
    /// Legacy convenience accepted only when this program has one snapshot.
    /// Prefer SnapshotInput::native.npdm for versioned content.
    pub npdm: Option<MediaInput>,
}

pub struct LocalizedMetadata {
    pub language: String,
    pub title: Option<String>,
    pub icon: Option<MediaInput>,
}

/// Display metadata that belongs to one exact snapshot. Empty metadata objects
/// are rejected rather than being serialized as a false provenance claim.
pub struct SnapshotMetadata {
    pub title: Option<String>,
    pub publisher: Option<String>,
    pub icon: Option<MediaInput>,
    pub nacp: Option<MediaInput>,
    pub localized: Vec<LocalizedMetadata>,
    pub opaque: Vec<MediaInput>,
}

/// Optional user-supplied display information. Native titles, icons and NACP
/// bytes take precedence. This never supplies execution identifiers or metadata.
#[derive(Clone, Default)]
pub struct DisplayFallback {
    pub title: Option<String>,
    pub icon: Option<(MediaType, std::sync::Arc<[u8]>)>,
}

impl SnapshotMetadata {
    pub fn has_any(&self) -> bool {
        self.title.is_some()
            || self.publisher.is_some()
            || self.icon.is_some()
            || self.nacp.is_some()
            || !self.localized.is_empty()
            || !self.opaque.is_empty()
    }
}

/// Native execution metadata belongs to this exact version, independently of
/// the selected application display metadata. It does not alter logical identity.
#[derive(Default)]
pub struct SnapshotNativeMetadata {
    pub npdm: Option<MediaInput>,
    pub required_firmware: Option<u32>,
}

pub struct SnapshotInput {
    pub native: SnapshotNativeMetadata,
    /// `None` is permitted only for DLC with no source-provided executable ID.
    pub program_id: Option<u64>,
    pub snapshot_type: SnapshotType,
    pub numeric_version: u32,
    pub content_id: Option<[u8; 16]>,
    /// Add-on (AOC) title ID of a DLC whose native Content ID is unknown,
    /// as for extracted trees and NUP packages. Stored in the Content ID
    /// field with snapshot flag bit 1; it excludes `content_id`.
    pub addon_title_id: Option<u64>,
    pub build_id: Option<[u8; 32]>,
    pub display_version: Option<String>,
    pub dependencies: Vec<DependencyInput>,
    pub metadata: Option<SnapshotMetadata>,
    pub root: DirectoryInput,
}

pub struct ApplicationInput {
    pub application_id: u64,
    pub platform: Platform,
    pub programs: Vec<ProgramInput>,
    pub snapshots: Vec<SnapshotInput>,
}

/// Block-backed source data, explicitly outside executable snapshots.
/// Kind 1 is a complete original system-update HFS0 in `partition.hfs0`.
pub struct SourceResourceInput {
    pub platform: Platform,
    pub application_id: u64,
    pub kind: u32,
    pub root: DirectoryInput,
}

pub struct ArchiveInput {
    pub applications: Vec<ApplicationInput>,
    pub source_resources: Vec<SourceResourceInput>,
}

impl ArchiveInput {
    pub fn new(applications: Vec<ApplicationInput>) -> Self {
        Self {
            applications,
            source_resources: Vec::new(),
        }
    }
}
