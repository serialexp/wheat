//! Bounded, read-only snapshots of explicitly named workspace files.

use std::{fmt, io, path::Path};

use wheat_core::FileKind;

/// An owned snapshot suitable for borrowing as an `edit_capture::FileSnapshot`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceFileSnapshot {
    pub(crate) kind: FileKind,
    pub(crate) bytes: Vec<u8>,
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

    use super::{FileKind, WorkspaceFileError, WorkspaceFileSnapshot};

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

    use super::{FileKind, WorkspaceFileError, WorkspaceFileSnapshot};

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
