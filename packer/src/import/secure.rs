//! Read-only capability root. Walk each component without following links.
use super::*;
use std::{
    collections::{BTreeSet, HashMap},
    fs::{File, OpenOptions},
    io,
    path::{Component, Path},
    sync::{Arc, Mutex},
};
#[derive(Debug)]
pub(crate) struct Root {
    pub path: PathBuf,
    file: Arc<File>,
    #[cfg(windows)]
    _parents: Vec<File>,
    /// Directory handles opened once, each relative to its parent's handle.
    /// Holding them pins the tree (no rename/delete of an open directory) and
    /// makes every later open a single relative call instead of a root walk.
    dirs: Mutex<HashMap<String, Arc<File>>>,
    preserve_game_metadata: bool,
    game_metadata_paths: Option<BTreeSet<String>>,
}
/// One directory entry as reported by the listing itself (no per-entry open).
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Last-write time in host units; detects ordinary changes between passes.
    pub mtime: u64,
}
#[cfg(windows)]
mod nt {
    #![allow(non_snake_case, clippy::upper_case_acronyms)]
    use std::ffi::c_void;
    #[repr(C)]
    pub struct UNICODE_STRING {
        pub Length: u16,
        pub MaximumLength: u16,
        pub Buffer: *mut u16,
    }
    #[repr(C)]
    pub struct OBJECT_ATTRIBUTES {
        pub Length: u32,
        pub RootDirectory: *mut c_void,
        pub ObjectName: *mut UNICODE_STRING,
        pub Attributes: u32,
        pub SecurityDescriptor: *mut c_void,
        pub SecurityQualityOfService: *mut c_void,
    }
    #[repr(C)]
    pub struct IO_STATUS_BLOCK {
        pub Status: usize,
        pub Information: usize,
    }
    #[link(name = "ntdll")]
    extern "system" {
        pub fn NtCreateFile(
            handle: *mut *mut c_void,
            access: u32,
            attributes: *mut OBJECT_ATTRIBUTES,
            status: *mut IO_STATUS_BLOCK,
            allocation: *mut i64,
            file_attributes: u32,
            share: u32,
            disposition: u32,
            options: u32,
            ea: *mut c_void,
            ea_length: u32,
        ) -> i32;
        pub fn RtlNtStatusToDosError(status: i32) -> u32;
    }
}
/// Open `name` relative to an already pinned directory handle. Never follows
/// reparse points: the entry itself is opened and rejected if it is one.
#[cfg(windows)]
fn nt_open(parent: &File, name: &str, directory: bool, share: u32) -> io::Result<File> {
    use std::os::windows::{
        fs::MetadataExt,
        io::{AsRawHandle, FromRawHandle},
    };
    if name.is_empty() || name.contains(['\\', '/', ':']) || name == "." || name == ".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid relative name",
        ));
    }
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    let bytes = u16::try_from(wide.len() * 2)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name too long"))?;
    let mut unicode = nt::UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_mut_ptr(),
    };
    let mut attributes = nt::OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<nt::OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &mut unicode,
        Attributes: 0x40, // OBJ_CASE_INSENSITIVE, as Win32 path opens.
        SecurityDescriptor: std::ptr::null_mut(),
        SecurityQualityOfService: std::ptr::null_mut(),
    };
    let mut status = nt::IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let mut handle = std::ptr::null_mut();
    // FILE_GENERIC_READ; FILE_OPEN; FILE_SYNCHRONOUS_IO_NONALERT |
    // FILE_OPEN_REPARSE_POINT (| FILE_DIRECTORY_FILE).
    let options = 0x20 | 0x0020_0000 | u32::from(directory);
    // SAFETY: all pointers reference live locals; the handle is owned below.
    let code = unsafe {
        nt::NtCreateFile(
            &mut handle,
            0x0012_0089,
            &mut attributes,
            &mut status,
            std::ptr::null_mut(),
            0,
            share,
            1,
            options,
            std::ptr::null_mut(),
            0,
        )
    };
    if code < 0 {
        // SAFETY: plain status conversion.
        let dos = unsafe { nt::RtlNtStatusToDosError(code) };
        return Err(io::Error::from_raw_os_error(dos as i32));
    }
    // SAFETY: NtCreateFile succeeded and returned a handle we now own.
    let file = unsafe { File::from_raw_handle(handle) };
    let metadata = file.metadata()?;
    if metadata.file_attributes() & 0x400 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reparse point in import source",
        ));
    }
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "special import source",
        ));
    }
    Ok(file)
}
#[cfg(windows)]
fn win_open(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    let file = OpenOptions::new()
        .read(true)
        .share_mode(3)
        .custom_flags(0x02000000 | 0x00200000)
        .open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("source handle {}: {e}", path.display())))?;
    if file.metadata()?.file_attributes() & 0x400 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reparse point in import source",
        ));
    }
    if file.metadata()?.is_file() {
        // Directory handles allow child publication but deny directory renames.
        // Upgrade regular files to deny writes as well. The probe pins the
        // entry while reopening, and the second handle is checked again.
        let pinned = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .custom_flags(0x02000000 | 0x00200000)
            .open(path)?;
        if pinned.metadata()?.file_attributes() & 0x400 != 0 || !pinned.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source changed into reparse/special file",
            ));
        }
        return Ok(pinned);
    }
    Ok(file)
}
#[cfg(unix)]
fn at(parent: &File, name: &std::ffi::OsStr, directory: bool) -> io::Result<File> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    };
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL path"))?;
    let mut info: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut info,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let kind = info.st_mode & libc::S_IFMT;
    if kind != libc::S_IFDIR && kind != libc::S_IFREG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "link or special import source",
        ));
    }
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // All components are separately resolved relative to an owned directory fd.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "special import source",
        ));
    }
    Ok(file)
}
impl Root {
    #[cfg(windows)]
    fn check_within(&self, file: &File) -> Result<(), WriterError> {
        use std::os::windows::{ffi::OsStringExt, fs::MetadataExt, io::AsRawHandle};
        #[link(name = "kernel32")]
        extern "system" {
            fn GetFinalPathNameByHandleW(
                handle: *mut std::ffi::c_void,
                path: *mut u16,
                size: u32,
                flags: u32,
            ) -> u32;
        }
        if self.file.metadata()?.file_attributes() & 0x400 != 0 {
            return Err(invalid("source root became a reparse point"));
        }
        let needed =
            unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), std::ptr::null_mut(), 0, 0) };
        if needed == 0 || needed > 32768 {
            return Err(invalid("cannot establish bounded source handle location"));
        }
        let mut path = vec![0u16; needed as usize];
        let written = unsafe {
            GetFinalPathNameByHandleW(file.as_raw_handle(), path.as_mut_ptr(), needed, 0)
        };
        if written == 0 || written >= needed {
            return Err(invalid("source handle location changed"));
        }
        path.truncate(written as usize);
        let resolved = PathBuf::from(std::ffi::OsString::from_wide(&path));
        if !resolved.starts_with(&self.path) {
            return Err(invalid("source handle escaped pinned root"));
        }
        Ok(())
    }
    pub fn open(path: &Path) -> Result<Self, WriterError> {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        if absolute
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(invalid("parent traversal in source root"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // The root the caller chose is resolved once, so system links
            // above it (macOS /var -> /private/var) are accepted; from there
            // every component is opened without following links.
            let absolute = absolute.canonicalize()?;
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open("/")?;
            for c in absolute.components() {
                if let Component::Normal(name) = c {
                    file = at(&file, name, true)?;
                }
            }
            Ok(Self {
                path: absolute,
                file: Arc::new(file),
                dirs: Mutex::default(),
                preserve_game_metadata: false,
                game_metadata_paths: None,
            })
        }
        #[cfg(windows)]
        {
            let mut prefix = PathBuf::new();
            let mut parents = Vec::new();
            for c in absolute.components() {
                prefix.push(c.as_os_str());
                if matches!(c, Component::RootDir | Component::Normal(_)) {
                    let f = win_open(&prefix)?;
                    if !f.metadata()?.is_dir() {
                        return Err(invalid("source ancestor is not a directory"));
                    }
                    parents.push(f);
                }
            }
            let file = parents
                .pop()
                .ok_or_else(|| invalid("missing source root"))?;
            let root = Self {
                path: absolute.canonicalize()?,
                file: Arc::new(file),
                _parents: parents,
                dirs: Mutex::default(),
                preserve_game_metadata: false,
                game_metadata_paths: None,
            };
            // Location is established once; descendants are opened relative
            // to pinned handles and can no longer escape through reparse points.
            root.check_within(&root.file)?;
            Ok(root)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = absolute;
            Err(unsupported("host capability filesystem"))
        }
    }
    #[cfg(unix)]
    const MAX_CACHED_DIRS: usize = 64;
    /// Pinned handle of a directory below the root, opened once relative to
    /// its parent's pinned handle and then reused.
    fn dir(&self, path: &str) -> Result<Arc<File>, WriterError> {
        if path.is_empty() {
            return Ok(self.file.clone());
        }
        if let Some(found) = self
            .dirs
            .lock()
            .map_err(|_| invalid("directory cache poisoned"))?
            .get(path)
        {
            return Ok(found.clone());
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let parent = self.dir(parent)?;
        // Directories allow child publication but deny rename/delete.
        #[cfg(windows)]
        let file = nt_open(&parent, name, true, 3)?;
        #[cfg(unix)]
        let file = at(&parent, std::ffi::OsStr::new(name), true)?;
        #[cfg(not(any(unix, windows)))]
        let file: File = {
            let _ = (parent, name);
            return Err(unsupported("host capability filesystem"));
        };
        if !file.metadata()?.is_dir() {
            return Err(invalid("source ancestor is not a directory"));
        }
        let file = Arc::new(file);
        let mut dirs = self
            .dirs
            .lock()
            .map_err(|_| invalid("directory cache poisoned"))?;
        // Unix cannot pin a directory by holding it open (rename still works),
        // so the cache only saves lookups: bound it so a tree with thousands
        // of directories cannot exhaust the process's descriptors.
        #[cfg(unix)]
        if dirs.len() >= Self::MAX_CACHED_DIRS {
            return Ok(file);
        }
        dirs.insert(path.to_owned(), file.clone());
        Ok(file)
    }
    pub fn open_relative(&self, path: &str) -> Result<File, WriterError> {
        if path.is_empty() {
            return Ok(self.file.try_clone()?);
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let parent = self.dir(parent)?;
        #[cfg(unix)]
        {
            Ok(at(&parent, std::ffi::OsStr::new(name), false)?)
        }
        #[cfg(windows)]
        {
            // Regular files are held deny-write while open. A directory may
            // already be open with write sharing elsewhere: retry as one.
            match nt_open(&parent, name, false, 1) {
                Ok(f) => Ok(f),
                Err(e) if e.raw_os_error() == Some(32) => Ok(nt_open(&parent, name, true, 3)?),
                Err(e) => Err(e.into()),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (parent, name);
            Err(unsupported("host capability filesystem"))
        }
    }
    pub fn names(&self, path: &str, max: usize) -> Result<Vec<String>, WriterError> {
        Ok(self.list(path, max)?.into_iter().map(|e| e.name).collect())
    }
    /// Preserve files shipped in exact exports without changing normal imports.
    pub(crate) fn with_game_metadata(mut self) -> Self {
        self.preserve_game_metadata = true;
        self
    }
    pub(crate) fn with_game_metadata_paths(mut self, paths: BTreeSet<String>) -> Self {
        self.preserve_game_metadata = true;
        self.game_metadata_paths = Some(paths);
        self
    }

    /// Names, kinds, sizes and write times straight from the directory
    /// listing: no entry is opened. Links/reparse points/special files fail.
    pub fn list(&self, path: &str, max: usize) -> Result<Vec<Entry>, WriterError> {
        let dir = self.dir(path)?;
        let mut out = Vec::new();
        let check_name = |name: &str, len: usize| -> Result<(), WriterError> {
            if name.len() > 240 && !name.starts_with("._") {
                return Err(unsupported("long/nonportable source name"));
            }
            if len == max {
                return Err(limit("directory exceeds entry budget"));
            }
            Ok(())
        };
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            // The pinned handle keeps this directory from being replaced.
            let _pinned = dir;
            for entry in std::fs::read_dir(super::join(&self.path, path))? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| unsupported("non-UTF8 name"))?;
                check_name(&name, out.len())?;
                // Windows directory enumeration already carries this data.
                let m = entry.metadata()?;
                if m.file_attributes() & 0x400 != 0 {
                    return Err(invalid("reparse point in import source"));
                }
                if !m.is_file() && !m.is_dir() {
                    return Err(unsupported("special import source"));
                }
                out.push(Entry {
                    dir: m.is_dir(),
                    size: if m.is_dir() { 0 } else { m.len() },
                    mtime: m.last_write_time(),
                    name,
                });
            }
        }
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd, IntoRawFd};
            // fdopendir owns its descriptor; guard closes it on every error path.
            struct Dir(*mut libc::DIR);
            impl Drop for Dir {
                fn drop(&mut self) {
                    unsafe {
                        libc::closedir(self.0);
                    }
                }
            }
            let fd = dir.try_clone()?.into_raw_fd();
            let ptr = unsafe { libc::fdopendir(fd) };
            if ptr.is_null() {
                let error = io::Error::last_os_error();
                unsafe {
                    libc::close(fd);
                };
                return Err(error.into());
            }
            let guard = Dir(ptr);
            // The duplicate shares the cached descriptor's position.
            unsafe { libc::rewinddir(guard.0) };
            loop {
                #[cfg(target_os = "linux")]
                let errno = unsafe { libc::__errno_location() };
                #[cfg(target_os = "android")]
                let errno = unsafe { libc::__errno() };
                #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
                let errno = unsafe { libc::__error() };
                #[cfg(not(any(
                    target_os = "linux",
                    target_os = "android",
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd"
                )))]
                let errno: *mut libc::c_int = std::ptr::null_mut();
                if errno.is_null() {
                    return Err(unsupported("host directory errno API"));
                }
                unsafe {
                    *errno = 0;
                }
                let entry = unsafe { libc::readdir(guard.0) };
                if entry.is_null() {
                    if unsafe { *errno } != 0 {
                        return Err(io::Error::last_os_error().into());
                    }
                    break;
                }
                let raw = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
                let bytes = raw.to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                let name = std::str::from_utf8(bytes)
                    .map_err(|_| unsupported("non-UTF8 name"))?
                    .to_owned();
                check_name(&name, out.len())?;
                let mut info: libc::stat = unsafe { std::mem::zeroed() };
                if unsafe {
                    libc::fstatat(
                        dir.as_raw_fd(),
                        raw.as_ptr(),
                        &mut info,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                } < 0
                {
                    return Err(io::Error::last_os_error().into());
                }
                let kind = info.st_mode & libc::S_IFMT;
                if kind != libc::S_IFDIR && kind != libc::S_IFREG {
                    return Err(invalid("link or special import source"));
                }
                out.push(Entry {
                    dir: kind == libc::S_IFDIR,
                    size: if kind == libc::S_IFDIR {
                        0
                    } else {
                        info.st_size as u64
                    },
                    mtime: (info.st_mtime as u64)
                        .wrapping_mul(1_000_000_000)
                        .wrapping_add(info.st_mtime_nsec as u64),
                    name,
                });
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (dir, check_name);
            return Err(unsupported("host capability filesystem"));
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        // Files inside a RomFS/data namespace are game bytes, even when a
        // developer shipped a real DS_Store or AppleDouble file in the game.
        // Only filter identified host metadata outside those content roots.
        let game_content = self.preserve_game_metadata
            && path.split('/').any(|part| matches!(part, "romfs" | "data"));
        let mut ignored = vec![false; out.len()];
        for (index, entry) in out.iter().enumerate() {
            if entry.dir {
                continue;
            }
            let apple_double = entry.name.strip_prefix("._").is_some_and(|name| {
                !name.is_empty()
                    && out
                        .binary_search_by(|other| other.name.as_str().cmp(name))
                        .is_ok()
            });
            if !apple_double && entry.name != ".DS_Store" {
                continue;
            }
            let relative = if path.is_empty() {
                entry.name.clone()
            } else {
                format!("{path}/{}", entry.name)
            };
            if game_content
                && self
                    .game_metadata_paths
                    .as_ref()
                    .map_or(true, |paths| paths.contains(&relative))
            {
                continue;
            }
            let mut file = self.open_relative(&relative)?;
            ignored[index] = macos_metadata(&mut file, entry.size, apple_double)?;
        }
        let mut index = 0;
        out.retain(|_| {
            let keep = !ignored[index];
            index += 1;
            keep
        });
        if out.iter().any(|entry| entry.name.len() > 240) {
            return Err(unsupported("long/nonportable source name"));
        }
        Ok(out)
    }
}

/// Recognize bounded AppleDouble tables or the Finder DS_Store header. A name
/// alone never permits dropping bytes from an imported game.
fn macos_metadata(file: &mut File, size: u64, apple_double: bool) -> io::Result<bool> {
    use std::io::Read;
    if size < 8 {
        return Ok(false);
    }
    let mut header = [0u8; 26];
    file.read_exact(&mut header[..8])?;
    if !apple_double {
        return Ok(header[..8] == [0, 0, 0, 1, b'B', b'u', b'd', b'1']);
    }
    if size < 26 || header[..8] != [0, 5, 22, 7, 0, 2, 0, 0] {
        return Ok(false);
    }
    file.read_exact(&mut header[8..])?;
    let count = u16::from_be_bytes([header[24], header[25]]) as usize;
    if count == 0 || count > 16 || 26 + count as u64 * 12 > size {
        return Ok(false);
    }
    let mut table = [0u8; 16 * 12];
    file.read_exact(&mut table[..count * 12])?;
    for row in table[..count * 12].chunks_exact(12) {
        let offset = u32::from_be_bytes(row[4..8].try_into().unwrap()) as u64;
        let length = u32::from_be_bytes(row[8..12].try_into().unwrap()) as u64;
        if offset < 26 + count as u64 * 12 || offset + length > size {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    fn apple_double() -> Vec<u8> {
        let mut bytes = vec![0; 42];
        bytes[..8].copy_from_slice(&[0, 5, 22, 7, 0, 2, 0, 0]);
        bytes[24..26].copy_from_slice(&1u16.to_be_bytes());
        bytes[26..30].copy_from_slice(&9u32.to_be_bytes());
        bytes[30..34].copy_from_slice(&38u32.to_be_bytes());
        bytes[34..38].copy_from_slice(&4u32.to_be_bytes());
        bytes
    }
    #[test]
    fn list_ignores_only_identified_host_metadata() {
        let path = std::env::temp_dir().join(format!(
            "nx-host-metadata-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(path.join("romfs")).unwrap();
        std::fs::write(path.join("._romfs"), apple_double()).unwrap();
        std::fs::write(path.join(".DS_Store"), b"\0\0\0\x01Bud1synthetic").unwrap();
        std::fs::write(path.join("game"), b"payload").unwrap();
        std::fs::write(path.join("._game"), b"ordinary game file").unwrap();
        std::fs::write(path.join("._orphan"), apple_double()).unwrap();
        std::fs::create_dir(path.join("._directory")).unwrap();
        let root = Root::open(&path).unwrap();
        let names = root.names("", 32).unwrap();
        assert_eq!(
            names,
            ["._directory", "._game", "._orphan", "game", "romfs"]
        );
        let long_name = "a".repeat(240);
        std::fs::write(path.join(&long_name), b"payload").unwrap();
        std::fs::write(path.join(format!("._{long_name}")), apple_double()).unwrap();
        let names = root.names("", 32).unwrap();
        assert!(names.contains(&long_name));
        assert!(!names.contains(&format!("._{long_name}")));
        let mut invalid = apple_double();
        invalid[34..38].copy_from_slice(&100u32.to_be_bytes());
        std::fs::write(path.join("._romfs"), invalid).unwrap();
        std::fs::write(path.join(".DS_Store"), b"game content").unwrap();
        let names = root.names("", 32).unwrap();
        assert!(names.contains(&"._romfs".to_string()));
        assert!(names.contains(&".DS_Store".to_string()));
        std::fs::write(path.join("romfs/.DS_Store"), b"\0\0\0\x01Bud1game bytes").unwrap();
        std::fs::write(path.join("romfs/asset"), b"asset").unwrap();
        std::fs::write(path.join("romfs/._asset"), apple_double()).unwrap();
        assert_eq!(root.names("romfs", 32).unwrap(), ["asset"]);
        let exact = Root::open(&path).unwrap().with_game_metadata();
        assert_eq!(
            exact.names("romfs", 32).unwrap(),
            [".DS_Store", "._asset", "asset"]
        );
        drop(exact);
        drop(root);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn directory_handles_stay_bounded() {
        let path = std::env::temp_dir().join(format!(
            "nx-dir-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        for i in 0..(Root::MAX_CACHED_DIRS * 3) {
            std::fs::create_dir(path.join(format!("d{i}"))).unwrap();
            std::fs::write(path.join(format!("d{i}/f")), b"x").unwrap();
        }
        let root = Root::open(&path).unwrap();
        for i in 0..(Root::MAX_CACHED_DIRS * 3) {
            root.open_relative(&format!("d{i}/f")).unwrap();
        }
        assert!(root.dirs.lock().unwrap().len() <= Root::MAX_CACHED_DIRS);
        drop(root);
        std::fs::remove_dir_all(path).unwrap();
    }
}
