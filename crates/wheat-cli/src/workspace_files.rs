//! Bounded, read-only snapshots of explicitly named workspace files.

#[cfg(test)]
use std::cell::Cell;
#[cfg(unix)]
use std::{ffi::OsStr, fs::File};
use std::{fmt, io, path::Path};

#[cfg(test)]
thread_local! {
    static TEST_SNAPSHOT_ALLOCATED_BYTES: Cell<usize> = const { Cell::new(0) };
    static TEST_STREAM_READS: Cell<usize> = const { Cell::new(0) };
    static TEST_STREAM_SCRATCH_BYTES: Cell<usize> = const { Cell::new(0) };
}

use serde::{Deserialize, Serialize};
use wheat_core::{
    ContentId, FileKind,
    text_capture::{CaptureKind, StreamingCapture},
};

const STREAM_SCRATCH_BYTES: usize = 64 * 1024;
const RAW_PREIMAGE_DOMAIN: &[u8] = b"wheat-workspace-raw-preimage-v1\0";

/// Exact, bounded authentication of a raw workspace leaf.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPreimage {
    pub(crate) kind: FileKind,
    pub(crate) length: u64,
    pub(crate) digest: [u8; 32],
}

/// An owned snapshot suitable for borrowing as an `edit_capture::FileSnapshot`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceFileSnapshot {
    pub(crate) kind: FileKind,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CanonicalWorkspaceFile {
    pub(crate) kind: FileKind,
    pub(crate) content: ContentId,
    pub(crate) bytes: usize,
    pub(crate) read_bytes: usize,
    pub(crate) text: bool,
    pub(crate) retained_text: Option<Vec<u8>>,
}

#[derive(Debug)]
pub(crate) enum WorkspaceFileError {
    InvalidPath,
    IntermediateSymlink,
    UnsupportedFileType,
    TooLarge { limit: usize },
    Io(io::Error),
}

impl fmt::Display for WorkspaceFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath => f.write_str(
                "workspace path must be an explicit repository-relative path outside .wheat",
            ),
            Self::IntermediateSymlink => {
                f.write_str("workspace path has a symlink as an intermediate component")
            }
            Self::UnsupportedFileType => {
                f.write_str("workspace path is not a regular file or symlink")
            }
            Self::TooLarge { limit } => write!(f, "workspace file exceeds the {limit}-byte limit"),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for WorkspaceFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for WorkspaceFileError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Read one explicitly named workspace path without changing the workspace.
///
/// A missing path (including a missing ancestor) represents deletion. Reads are
/// capped even if a regular file grows after it is opened. Intermediate symlink
/// directories are rejected rather than followed, preventing traversal outside
/// `root`.
pub(crate) fn read_workspace_file(
    root: &Path,
    path: &Path,
    byte_limit: usize,
) -> Result<Option<WorkspaceFileSnapshot>, WorkspaceFileError> {
    let components = validate_path(path)?;
    platform::read(root, &components, byte_limit)
}

/// Analyze one descriptor-relative workspace leaf with fixed read scratch.
///
/// Regular files are never collected. At most `retain_limit` canonical text
/// bytes are retained for an inline diff. Symlink targets use their portable
/// 4 KiB bound and are always classified as binary.
pub(crate) fn analyze_workspace_file(
    root: &Path,
    path: &Path,
    byte_limit: usize,
    retain_limit: usize,
) -> Result<Option<CanonicalWorkspaceFile>, WorkspaceFileError> {
    let components = validate_path(path)?;
    platform::analyze(root, &components, byte_limit, retain_limit)
}

/// Authenticate a named workspace leaf without allocating its regular-file
/// contents. Symlink targets remain bounded by `byte_limit` and are hashed from
/// their small `readlinkat` buffer.
pub(crate) fn authenticate_workspace_file(
    root: &Path,
    path: &Path,
    expected: Option<(FileKind, &[u8])>,
    byte_limit: usize,
) -> Result<bool, WorkspaceFileError> {
    let components = validate_path(path)?;
    platform::authenticate(root, &components, expected, byte_limit)
}

/// Observe a raw leaf with a domain-separated streaming digest. Missing paths
/// are represented by `None`; regular contents are never collected.
pub(crate) fn observe_workspace_file(
    root: &Path,
    path: &Path,
    byte_limit: usize,
) -> Result<Option<RawPreimage>, WorkspaceFileError> {
    let components = validate_path(path)?;
    platform::observe(root, &components, byte_limit)
}

pub(crate) fn authenticate_workspace_preimage(
    root: &Path,
    path: &Path,
    expected: &RawPreimage,
    byte_limit: usize,
) -> Result<bool, WorkspaceFileError> {
    Ok(observe_workspace_file(root, path, byte_limit)?.as_ref() == Some(expected))
}

/// Authenticate a leaf relative to an already-open directory descriptor.
/// Regular contents are streamed through fixed scratch; symlink targets are
/// bounded to the maximum portable target size used by materialization.
#[cfg(unix)]
pub(crate) fn authenticate_preimage_at(
    directory: &File,
    name: &OsStr,
    expected: &RawPreimage,
    byte_limit: usize,
) -> Result<bool, WorkspaceFileError> {
    Ok(platform::observe_at(directory, name, byte_limit)?.as_ref() == Some(expected))
}

pub(crate) fn raw_preimage(kind: FileKind, bytes: &[u8]) -> RawPreimage {
    let mut hasher = blake3::Hasher::new();
    hasher.update(RAW_PREIMAGE_DOMAIN);
    hasher.update(bytes);
    RawPreimage {
        kind,
        length: bytes.len() as u64,
        digest: *hasher.finalize().as_bytes(),
    }
}

fn validate_path(path: &Path) -> Result<Vec<&std::ffi::OsStr>, WorkspaceFileError> {
    use std::path::Component;

    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(WorkspaceFileError::InvalidPath);
    }
    // `Path::components` normalizes interior `.` components on Unix. Inspect the
    // original bytes so paths such as `a/./b` remain invalid rather than being
    // silently accepted as `a/b`.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        if path
            .as_os_str()
            .as_bytes()
            .split(|byte| *byte == b'/')
            .any(|part| part == b".")
        {
            return Err(WorkspaceFileError::InvalidPath);
        }
    }
    let mut result = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                if result.is_empty()
                    && matches!(
                        name.to_str(),
                        Some(
                            ".wheat"
                                | ".wheat-git"
                                | "repository.head"
                                | "repository.lock"
                                | "storage"
                        )
                    )
                {
                    return Err(WorkspaceFileError::InvalidPath);
                }
                result.push(name);
            }
            Component::Prefix(_)
            | Component::RootDir
            | Component::ParentDir
            | Component::CurDir => {
                return Err(WorkspaceFileError::InvalidPath);
            }
        }
    }
    if result.is_empty() {
        return Err(WorkspaceFileError::InvalidPath);
    }
    Ok(result)
}

#[cfg(unix)]
mod platform {
    use std::{
        ffi::{CString, OsStr},
        fs::File,
        io::{self, Read},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
        path::Path,
    };

    use super::{
        CanonicalWorkspaceFile, CaptureKind, ContentId, FileKind, RAW_PREIMAGE_DOMAIN, RawPreimage,
        STREAM_SCRATCH_BYTES, StreamingCapture, WorkspaceFileError, WorkspaceFileSnapshot,
    };

    pub(super) fn analyze(
        root: &Path,
        components: &[&OsStr],
        limit: usize,
        retain_limit: usize,
    ) -> Result<Option<CanonicalWorkspaceFile>, WorkspaceFileError> {
        let mut directory = File::open(root)?;
        for component in &components[..components.len() - 1] {
            let name = c_name(component)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::ENOENT) => Ok(None),
                    Some(libc::ELOOP) | Some(libc::ENOTDIR)
                        if is_symlink(directory.as_raw_fd(), &name) =>
                    {
                        Err(WorkspaceFileError::IntermediateSymlink)
                    }
                    Some(libc::ENOTDIR) => Err(WorkspaceFileError::UnsupportedFileType),
                    _ => Err(error.into()),
                };
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }
        let name = c_name(components.last().expect("validated nonempty path"))?;
        analyze_leaf(directory.as_raw_fd(), &name, limit, retain_limit)
    }

    fn analyze_leaf(
        directory: libc::c_int,
        name: &CString,
        limit: usize,
        retain_limit: usize,
    ) -> Result<Option<CanonicalWorkspaceFile>, WorkspaceFileError> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                directory,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error.into())
            };
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
            // Linux symlink targets are bounded independently of the regular-file
            // scan cap; do not size an allocation from the 256 MiB file limit.
            let bytes = read_link(directory, name, limit.min(4_095))?;
            return Ok(Some(CanonicalWorkspaceFile {
                kind: FileKind::Symlink,
                content: ContentId::of(&bytes),
                bytes: bytes.len(),
                read_bytes: bytes.len(),
                text: false,
                retained_text: None,
            }));
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(WorkspaceFileError::UnsupportedFileType);
        }
        let fd = unsafe {
            libc::openat(
                directory,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let opened = file.metadata()?;
        if !opened.is_file() {
            return Err(WorkspaceFileError::UnsupportedFileType);
        }
        if opened.len() > u64::try_from(limit).unwrap_or(u64::MAX) {
            return Err(WorkspaceFileError::TooLarge { limit });
        }
        use std::os::unix::fs::MetadataExt as _;
        let kind = if opened.mode() & 0o111 != 0 {
            FileKind::Executable
        } else {
            FileKind::Regular
        };
        let mut capture = StreamingCapture::new(retain_limit);
        let mut scratch = [0_u8; STREAM_SCRATCH_BYTES];
        #[cfg(test)]
        super::TEST_STREAM_SCRATCH_BYTES.with(|size| size.set(scratch.len()));
        let mut raw_length = 0usize;
        loop {
            let read = file.read(&mut scratch)?;
            if read == 0 {
                break;
            }
            #[cfg(test)]
            super::TEST_STREAM_READS.with(|reads| reads.set(reads.get() + 1));
            raw_length = raw_length
                .checked_add(read)
                .ok_or(WorkspaceFileError::TooLarge { limit })?;
            if raw_length > limit {
                return Err(WorkspaceFileError::TooLarge { limit });
            }
            capture.update(&scratch[..read]);
        }
        let result = capture.finish();
        Ok(Some(CanonicalWorkspaceFile {
            kind,
            content: ContentId::from_digest(result.digest),
            bytes: result.bytes,
            read_bytes: raw_length,
            text: result.kind == CaptureKind::Text,
            retained_text: result.retained,
        }))
    }

    pub(super) fn observe(
        root: &Path,
        components: &[&OsStr],
        limit: usize,
    ) -> Result<Option<RawPreimage>, WorkspaceFileError> {
        let mut directory = File::open(root)?;
        for component in &components[..components.len() - 1] {
            let name = c_name(component)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::ENOENT) => Ok(None),
                    Some(libc::ELOOP) | Some(libc::ENOTDIR)
                        if is_symlink(directory.as_raw_fd(), &name) =>
                    {
                        Err(WorkspaceFileError::IntermediateSymlink)
                    }
                    Some(libc::ENOTDIR) => Err(WorkspaceFileError::UnsupportedFileType),
                    _ => Err(error.into()),
                };
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }
        let name = c_name(components.last().expect("validated nonempty path"))?;
        observe_leaf(directory.as_raw_fd(), &name, limit)
    }

    pub(super) fn observe_at(
        directory: &File,
        name: &OsStr,
        limit: usize,
    ) -> Result<Option<RawPreimage>, WorkspaceFileError> {
        observe_leaf(directory.as_raw_fd(), &c_name(name)?, limit)
    }

    fn observe_leaf(
        directory: libc::c_int,
        name: &CString,
        limit: usize,
    ) -> Result<Option<RawPreimage>, WorkspaceFileError> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                directory,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error.into())
            };
        }
        let stat = unsafe { stat.assume_init() };
        let (kind, length, digest) = match stat.st_mode & libc::S_IFMT {
            libc::S_IFLNK => {
                const MAX_SYMLINK_BYTES: usize = 4_095;
                let effective_limit = limit.min(MAX_SYMLINK_BYTES);
                let mut scratch = [0_u8; MAX_SYMLINK_BYTES];
                let length = unsafe {
                    libc::readlinkat(
                        directory,
                        name.as_ptr(),
                        scratch.as_mut_ptr().cast(),
                        scratch.len(),
                    )
                };
                if length < 0 {
                    return Err(io::Error::last_os_error().into());
                }
                let length =
                    usize::try_from(length).expect("readlinkat returned a nonnegative ssize_t");
                if length > effective_limit
                    || (length == scratch.len() && stat.st_size > length as libc::off_t)
                {
                    return Err(WorkspaceFileError::TooLarge {
                        limit: effective_limit,
                    });
                }
                (
                    FileKind::Symlink,
                    length as u64,
                    digest_bytes(&scratch[..length]),
                )
            }
            libc::S_IFREG => {
                let fd = unsafe {
                    libc::openat(
                        directory,
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error().into());
                }
                let mut file = unsafe { File::from_raw_fd(fd) };
                let opened = file.metadata()?;
                if !opened.is_file() {
                    return Err(WorkspaceFileError::UnsupportedFileType);
                }
                if opened.len() > u64::try_from(limit).unwrap_or(u64::MAX) {
                    return Err(WorkspaceFileError::TooLarge { limit });
                }
                use std::os::unix::fs::MetadataExt as _;
                let kind = if opened.mode() & 0o111 != 0 {
                    FileKind::Executable
                } else {
                    FileKind::Regular
                };
                let mut hasher = blake3::Hasher::new();
                hasher.update(RAW_PREIMAGE_DOMAIN);
                let mut scratch = [0_u8; 64 * 1024];
                let mut length = 0_u64;
                loop {
                    let read = file.read(&mut scratch)?;
                    if read == 0 {
                        break;
                    }
                    length = length
                        .checked_add(read as u64)
                        .ok_or(WorkspaceFileError::TooLarge { limit })?;
                    if length > limit as u64 {
                        return Err(WorkspaceFileError::TooLarge { limit });
                    }
                    hasher.update(&scratch[..read]);
                }
                (kind, length, *hasher.finalize().as_bytes())
            }
            _ => return Err(WorkspaceFileError::UnsupportedFileType),
        };
        Ok(Some(RawPreimage {
            kind,
            length,
            digest,
        }))
    }

    fn digest_bytes(bytes: &[u8]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(RAW_PREIMAGE_DOMAIN);
        hasher.update(bytes);
        *hasher.finalize().as_bytes()
    }

    pub(super) fn authenticate(
        root: &Path,
        components: &[&OsStr],
        expected: Option<(FileKind, &[u8])>,
        limit: usize,
    ) -> Result<bool, WorkspaceFileError> {
        let mut directory = File::open(root)?;
        for component in &components[..components.len() - 1] {
            let name = c_name(component)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::ENOENT) => Ok(expected.is_none()),
                    Some(libc::ELOOP) | Some(libc::ENOTDIR)
                        if is_symlink(directory.as_raw_fd(), &name) =>
                    {
                        Err(WorkspaceFileError::IntermediateSymlink)
                    }
                    Some(libc::ENOTDIR) => Err(WorkspaceFileError::UnsupportedFileType),
                    _ => Err(error.into()),
                };
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }
        let name = c_name(components.last().expect("validated nonempty path"))?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(expected.is_none())
            } else {
                Err(error.into())
            };
        }
        let stat = unsafe { stat.assume_init() };
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFLNK => {
                let bytes = read_link(directory.as_raw_fd(), &name, limit)?;
                Ok(expected
                    .is_some_and(|(kind, expected)| kind == FileKind::Symlink && expected == bytes))
            }
            libc::S_IFREG => {
                let fd = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error().into());
                }
                let mut file = unsafe { File::from_raw_fd(fd) };
                let opened = file.metadata()?;
                if !opened.is_file() {
                    return Err(WorkspaceFileError::UnsupportedFileType);
                }
                use std::os::unix::fs::MetadataExt as _;
                let kind = if opened.mode() & 0o111 != 0 {
                    FileKind::Executable
                } else {
                    FileKind::Regular
                };
                if opened.len() > u64::try_from(limit).unwrap_or(u64::MAX) {
                    return Err(WorkspaceFileError::TooLarge { limit });
                }
                let Some((expected_kind, expected)) = expected else {
                    return Ok(false);
                };
                if expected_kind != kind
                    || expected.len() > limit
                    || opened.len() != expected.len() as u64
                {
                    return Ok(false);
                }
                let mut scratch = [0_u8; 64 * 1024];
                let mut offset = 0_usize;
                loop {
                    let read = file.read(&mut scratch)?;
                    if read == 0 {
                        break;
                    }
                    let end = offset
                        .checked_add(read)
                        .ok_or(WorkspaceFileError::TooLarge { limit })?;
                    if end > expected.len() || scratch[..read] != expected[offset..end] {
                        return Ok(false);
                    }
                    offset = end;
                }
                Ok(offset == expected.len())
            }
            _ => Err(WorkspaceFileError::UnsupportedFileType),
        }
    }

    pub(super) fn read(
        root: &Path,
        components: &[&OsStr],
        limit: usize,
    ) -> Result<Option<WorkspaceFileSnapshot>, WorkspaceFileError> {
        let mut directory = File::open(root)?;
        for component in &components[..components.len() - 1] {
            let name = c_name(component)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::ENOENT) => Ok(None),
                    Some(libc::ELOOP) | Some(libc::ENOTDIR)
                        if is_symlink(directory.as_raw_fd(), &name) =>
                    {
                        Err(WorkspaceFileError::IntermediateSymlink)
                    }
                    Some(libc::ENOTDIR) => Err(WorkspaceFileError::UnsupportedFileType),
                    _ => Err(error.into()),
                };
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }

        let name = c_name(components.last().expect("validated nonempty path"))?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error.into())
            };
        }
        let stat = unsafe { stat.assume_init() };
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFLNK => {
                let bytes = read_link(directory.as_raw_fd(), &name, limit)?;
                Ok(Some(WorkspaceFileSnapshot {
                    kind: FileKind::Symlink,
                    bytes,
                }))
            }
            libc::S_IFREG => open_regular(directory.as_raw_fd(), &name, limit).map(Some),
            _ => Err(WorkspaceFileError::UnsupportedFileType),
        }
    }

    pub(super) fn open_regular(
        directory: libc::c_int,
        name: &CString,
        limit: usize,
    ) -> Result<WorkspaceFileSnapshot, WorkspaceFileError> {
        let fd = unsafe {
            libc::openat(
                directory,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        // Authenticate the opened node rather than the earlier directory entry:
        // it may have been replaced between `fstatat` and `openat`. O_NONBLOCK
        // ensures opening a replacement FIFO cannot wait indefinitely.
        let opened = file.metadata()?;
        if !opened.is_file() {
            return Err(WorkspaceFileError::UnsupportedFileType);
        }
        use std::os::unix::fs::MetadataExt as _;
        let kind = if opened.mode() & 0o111 != 0 {
            FileKind::Executable
        } else {
            FileKind::Regular
        };
        let bytes = bounded_read(file, limit)?;
        Ok(WorkspaceFileSnapshot { kind, bytes })
    }

    fn c_name(name: &OsStr) -> Result<CString, WorkspaceFileError> {
        CString::new(name.as_bytes()).map_err(|_| WorkspaceFileError::InvalidPath)
    }

    fn is_symlink(directory: libc::c_int, name: &CString) -> bool {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        unsafe {
            libc::fstatat(
                directory,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            ) == 0
                && stat.assume_init().st_mode & libc::S_IFMT == libc::S_IFLNK
        }
    }

    fn bounded_read(file: File, limit: usize) -> Result<Vec<u8>, WorkspaceFileError> {
        let probe = limit.checked_add(1).unwrap_or(limit);
        let mut bytes = Vec::new();
        file.take(u64::try_from(probe).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(WorkspaceFileError::TooLarge { limit });
        }
        #[cfg(test)]
        super::TEST_SNAPSHOT_ALLOCATED_BYTES
            .with(|allocated| allocated.set(allocated.get().saturating_add(bytes.capacity())));
        Ok(bytes)
    }

    fn read_link(
        directory: libc::c_int,
        name: &CString,
        limit: usize,
    ) -> Result<Vec<u8>, WorkspaceFileError> {
        let capacity = limit
            .checked_add(1)
            .ok_or(WorkspaceFileError::TooLarge { limit })?;
        let mut bytes = vec![0_u8; capacity];
        let length = unsafe {
            libc::readlinkat(
                directory,
                name.as_ptr(),
                bytes.as_mut_ptr().cast(),
                capacity,
            )
        };
        if length < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let length = usize::try_from(length).expect("readlinkat returned a nonnegative ssize_t");
        if length > limit {
            return Err(WorkspaceFileError::TooLarge { limit });
        }
        bytes.truncate(length);
        Ok(bytes)
    }
}

#[cfg(not(unix))]
mod platform {
    use std::{ffi::OsStr, fs, io::Read, path::Path};

    use super::{
        CanonicalWorkspaceFile, CaptureKind, ContentId, FileKind, RawPreimage,
        STREAM_SCRATCH_BYTES, StreamingCapture, WorkspaceFileError, WorkspaceFileSnapshot,
        raw_preimage,
    };

    pub(super) fn analyze(
        root: &Path,
        components: &[&OsStr],
        limit: usize,
        retain_limit: usize,
    ) -> Result<Option<CanonicalWorkspaceFile>, WorkspaceFileError> {
        let mut current = root.to_path_buf();
        for component in &components[..components.len() - 1] {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(WorkspaceFileError::IntermediateSymlink);
                }
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => return Err(WorkspaceFileError::UnsupportedFileType),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
        current.push(components.last().expect("validated nonempty path"));
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            return Err(WorkspaceFileError::UnsupportedFileType);
        }
        if metadata.len() > u64::try_from(limit).unwrap_or(u64::MAX) {
            return Err(WorkspaceFileError::TooLarge { limit });
        }
        let mut file = fs::File::open(current)?;
        let mut capture = StreamingCapture::new(retain_limit);
        let mut scratch = [0_u8; STREAM_SCRATCH_BYTES];
        let mut raw_bytes = 0usize;
        loop {
            let read = file.read(&mut scratch)?;
            if read == 0 {
                break;
            }
            raw_bytes = raw_bytes
                .checked_add(read)
                .ok_or(WorkspaceFileError::TooLarge { limit })?;
            if raw_bytes > limit {
                return Err(WorkspaceFileError::TooLarge { limit });
            }
            capture.update(&scratch[..read]);
        }
        let result = capture.finish();
        Ok(Some(CanonicalWorkspaceFile {
            kind: FileKind::Regular,
            content: ContentId::from_digest(result.digest),
            bytes: result.bytes,
            read_bytes: raw_bytes,
            text: result.kind == CaptureKind::Text,
            retained_text: result.retained,
        }))
    }

    pub(super) fn observe(
        root: &Path,
        components: &[&OsStr],
        limit: usize,
    ) -> Result<Option<RawPreimage>, WorkspaceFileError> {
        Ok(read(root, components, limit)?
            .map(|snapshot| raw_preimage(snapshot.kind, &snapshot.bytes)))
    }

    pub(super) fn authenticate(
        root: &Path,
        components: &[&OsStr],
        expected: Option<(FileKind, &[u8])>,
        limit: usize,
    ) -> Result<bool, WorkspaceFileError> {
        Ok(match (read(root, components, limit)?, expected) {
            (None, None) => true,
            (Some(actual), Some((kind, bytes))) => actual.kind == kind && actual.bytes == bytes,
            _ => false,
        })
    }

    pub(super) fn read(
        root: &Path,
        components: &[&OsStr],
        limit: usize,
    ) -> Result<Option<WorkspaceFileSnapshot>, WorkspaceFileError> {
        let mut current = root.to_path_buf();
        for component in &components[..components.len() - 1] {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(WorkspaceFileError::IntermediateSymlink);
                }
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => return Err(WorkspaceFileError::UnsupportedFileType),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
        current.push(components.last().expect("validated nonempty path"));
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            return Err(WorkspaceFileError::UnsupportedFileType);
        }
        let mut bytes = Vec::new();
        fs::File::open(current)?
            .take(u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(WorkspaceFileError::TooLarge { limit });
        }
        Ok(Some(WorkspaceFileSnapshot {
            kind: FileKind::Regular,
            bytes,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    #[cfg(unix)]
    #[test]
    fn canonical_analyzer_streams_oversized_text_without_snapshot_allocation() {
        let root = tempfile::tempdir().unwrap();
        // Eight MiB is representative of a source/generated workspace leaf and
        // is eight times the default diff inline cap. The analyzer must still
        // retain no body and perform bounded reads.
        let mut bytes = vec![b'x'; 8 * 1024 * 1024 + 7];
        bytes[STREAM_SCRATCH_BYTES - 1] = b'\r';
        bytes[STREAM_SCRATCH_BYTES] = b'\n';
        fs::write(root.path().join("file"), &bytes).unwrap();
        TEST_SNAPSHOT_ALLOCATED_BYTES.with(|allocated| allocated.set(0));
        TEST_STREAM_READS.with(|reads| reads.set(0));
        TEST_STREAM_SCRATCH_BYTES.with(|size| size.set(0));
        let analyzed = analyze_workspace_file(root.path(), Path::new("file"), bytes.len(), 32)
            .unwrap()
            .unwrap();
        let expected = wheat_core::text_capture::capture(&bytes);
        let wheat_core::text_capture::CapturedBytes::Text(expected) = expected else {
            panic!()
        };
        assert_eq!(analyzed.content, ContentId::of(expected.as_bytes()));
        assert_eq!(analyzed.bytes, expected.len());
        assert_eq!(analyzed.read_bytes, bytes.len());
        assert!(analyzed.text);
        assert_eq!(analyzed.retained_text, None);
        TEST_SNAPSHOT_ALLOCATED_BYTES.with(|allocated| assert_eq!(allocated.get(), 0));
        TEST_STREAM_SCRATCH_BYTES.with(|size| assert_eq!(size.get(), 64 * 1024));
        TEST_STREAM_READS.with(|reads| {
            assert_eq!(
                reads.get(),
                bytes.len().div_ceil(STREAM_SCRATCH_BYTES),
                "regular files must be consumed exactly once in 64 KiB reads"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn canonical_analyzer_late_binary_marker_hashes_raw_bytes() {
        for marker in [0, 0xff] {
            let root = tempfile::tempdir().unwrap();
            let mut bytes = vec![b'x'; 8 * 1024 * 1024 + 1];
            bytes[10] = b'\r';
            let last = bytes.len() - 1;
            bytes[last] = marker;
            fs::write(root.path().join("file"), &bytes).unwrap();
            let analyzed =
                analyze_workspace_file(root.path(), Path::new("file"), bytes.len(), bytes.len())
                    .unwrap()
                    .unwrap();
            assert!(!analyzed.text);
            assert_eq!(analyzed.content, ContentId::of(&bytes));
            assert_eq!(analyzed.bytes, bytes.len());
            assert_eq!(analyzed.retained_text, None);
        }
    }

    #[test]
    fn streaming_authentication_does_not_allocate_a_snapshot_buffer() {
        let root = tempfile::tempdir().unwrap();
        let bytes = vec![b'x'; 3 * 64 * 1024 + 7];
        fs::write(root.path().join("file"), &bytes).unwrap();
        TEST_SNAPSHOT_ALLOCATED_BYTES.with(|allocated| allocated.set(0));
        assert!(
            read_workspace_file(root.path(), Path::new("file"), bytes.len())
                .unwrap()
                .is_some()
        );
        TEST_SNAPSHOT_ALLOCATED_BYTES.with(|allocated| assert!(allocated.get() >= bytes.len()));
        TEST_SNAPSHOT_ALLOCATED_BYTES.with(|allocated| allocated.set(0));
        assert!(
            authenticate_workspace_file(
                root.path(),
                Path::new("file"),
                Some((FileKind::Regular, &bytes)),
                bytes.len(),
            )
            .unwrap()
        );
        TEST_SNAPSHOT_ALLOCATED_BYTES.with(|allocated| assert_eq!(allocated.get(), 0));
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_preimage_authentication_streams_without_snapshot_allocation() {
        use std::os::unix::ffi::OsStrExt as _;

        let root = tempfile::tempdir().unwrap();
        let bytes = vec![b'x'; 3 * 64 * 1024 + 7];
        fs::write(root.path().join("file"), &bytes).unwrap();
        let directory = File::open(root.path()).unwrap();
        TEST_SNAPSHOT_ALLOCATED_BYTES.with(|allocated| allocated.set(0));
        assert!(
            authenticate_preimage_at(
                &directory,
                OsStr::from_bytes(b"file"),
                &raw_preimage(FileKind::Regular, &bytes),
                bytes.len(),
            )
            .unwrap()
        );
        TEST_SNAPSHOT_ALLOCATED_BYTES.with(|allocated| assert_eq!(allocated.get(), 0));

        std::os::unix::fs::symlink("target", root.path().join("link")).unwrap();
        assert!(
            authenticate_preimage_at(
                &directory,
                OsStr::from_bytes(b"link"),
                &raw_preimage(FileKind::Symlink, b"target"),
                usize::MAX,
            )
            .unwrap()
        );
    }

    #[test]
    fn raw_preimage_streams_kind_length_and_domain_separated_digest() {
        let root = tempfile::tempdir().unwrap();
        let bytes = vec![b'x'; 2 * 64 * 1024 + 7];
        std::fs::write(root.path().join("large"), &bytes).unwrap();
        let observed =
            observe_workspace_file(root.path(), Path::new("large"), bytes.len()).unwrap();
        assert_eq!(observed, Some(raw_preimage(FileKind::Regular, &bytes)));
        assert_eq!(observed.unwrap().length, bytes.len() as u64);
        assert!(
            authenticate_workspace_preimage(
                root.path(),
                Path::new("large"),
                &raw_preimage(FileKind::Regular, &bytes),
                bytes.len(),
            )
            .unwrap()
        );
        std::fs::write(root.path().join("large"), vec![b'y'; bytes.len()]).unwrap();
        assert!(
            !authenticate_workspace_preimage(
                root.path(),
                Path::new("large"),
                &raw_preimage(FileKind::Regular, &bytes),
                bytes.len(),
            )
            .unwrap()
        );
    }

    #[test]
    fn reads_regular_file_without_writing_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        fs::write(&path, b"original").unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();

        let snapshot = read_workspace_file(root.path(), Path::new("file"), 100)
            .unwrap()
            .unwrap();

        assert_eq!(snapshot.kind, FileKind::Regular);
        assert_eq!(snapshot.bytes, b"original");
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn recognizes_executable_and_preserves_symlink_target_bytes() {
        use std::os::unix::{
            ffi::OsStringExt,
            fs::{PermissionsExt, symlink},
        };

        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("run");
        fs::write(&executable, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o751)).unwrap();
        assert_eq!(
            read_workspace_file(root.path(), Path::new("run"), 100)
                .unwrap()
                .unwrap()
                .kind,
            FileKind::Executable
        );

        let target = std::ffi::OsString::from_vec(vec![b'.', b'.', b'/', 0xff, b'x']);
        symlink(&target, root.path().join("link")).unwrap();
        let snapshot = read_workspace_file(root.path(), Path::new("link"), 100)
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.kind, FileKind::Symlink);
        assert_eq!(snapshot.bytes, target.into_vec());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_ancestor_symlink_escape() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"secret").unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(matches!(
            read_workspace_file(root.path(), Path::new("escape/secret"), 100),
            Err(WorkspaceFileError::IntermediateSymlink)
        ));
    }

    #[test]
    fn rejects_unsafe_paths() {
        let root = tempfile::tempdir().unwrap();
        for path in [
            "",
            ".",
            "../x",
            "a/../x",
            "a/./b",
            ".wheat",
            ".wheat/state",
            ".wheat-git",
            ".wheat-git/projection.git",
            "repository.head",
            "repository.lock",
            "storage",
            "storage/packed",
        ] {
            assert!(
                matches!(
                    read_workspace_file(root.path(), Path::new(path), 10),
                    Err(WorkspaceFileError::InvalidPath)
                ),
                "{path:?}"
            );
        }
        let absolute = root.path().join("x");
        assert!(matches!(
            read_workspace_file(root.path(), &absolute, 10),
            Err(WorkspaceFileError::InvalidPath)
        ));
    }

    #[test]
    fn missing_path_is_deletion_and_directories_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            read_workspace_file(root.path(), Path::new("missing/child"), 10).unwrap(),
            None
        );
        fs::create_dir(root.path().join("directory")).unwrap();
        assert!(matches!(
            read_workspace_file(root.path(), Path::new("directory"), 10),
            Err(WorkspaceFileError::UnsupportedFileType)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn replacement_fifo_and_directory_are_rejected_by_nonblocking_final_open() {
        use std::{ffi::CString, fs::File, os::fd::AsRawFd};

        let root = tempfile::tempdir().unwrap();
        let directory = File::open(root.path()).unwrap();
        let fifo = CString::new("fifo").unwrap();
        assert_eq!(
            unsafe { libc::mkfifoat(directory.as_raw_fd(), fifo.as_ptr(), 0o600) },
            0
        );

        // This exercises the final-open seam directly, as though the regular
        // file authenticated by fstatat had just been replaced with a FIFO.
        // Without O_NONBLOCK, opening this FIFO for reading would hang.
        assert!(matches!(
            platform::open_regular(directory.as_raw_fd(), &fifo, 10),
            Err(WorkspaceFileError::UnsupportedFileType)
        ));

        fs::create_dir(root.path().join("directory")).unwrap();
        let name = CString::new("directory").unwrap();
        assert!(matches!(
            platform::open_regular(directory.as_raw_fd(), &name, 10),
            Err(WorkspaceFileError::UnsupportedFileType)
        ));
    }

    #[test]
    fn enforces_byte_limit() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), b"12345").unwrap();
        assert!(matches!(
            read_workspace_file(root.path(), Path::new("file"), 4),
            Err(WorkspaceFileError::TooLarge { limit: 4 })
        ));
        assert_eq!(
            read_workspace_file(root.path(), Path::new("file"), 5)
                .unwrap()
                .unwrap()
                .bytes,
            b"12345"
        );
    }
}
