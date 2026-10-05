//! The operating-system calls behind workspace files and transactions: opens that
//! never follow a symlink, calls relative to an open directory, and a few others.
//!
//! Wheat runs only on Linux. Elsewhere every call fails with `Unsupported`, so the
//! code above it still builds and cannot quietly fall back to path-based calls that
//! race or follow symlinks.

use std::io;

/// What a directory entry is, without following a symlink.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(target_os = "linux"),
    expect(dead_code, reason = "only the Linux calls report entries")
)]
pub(crate) enum EntryKind {
    Directory,
    Regular,
    Symlink,
    Other,
}

/// Whether an open may create the file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Creation {
    /// The file must exist.
    Existing,
    /// Create the file if it is missing.
    IfMissing,
    /// The file must not exist yet.
    New,
}

/// How to open a file. Opens never follow a symlink in the last component.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(target_os = "linux"),
    expect(dead_code, reason = "only the Linux calls open files")
)]
pub(crate) struct OpenFile {
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) append: bool,
    pub(crate) creation: Creation,
    /// Permission bits for a created file.
    pub(crate) mode: u32,
}

impl OpenFile {
    pub(crate) const READ: Self = Self {
        read: true,
        write: false,
        append: false,
        creation: Creation::Existing,
        mode: 0,
    };

    /// A private lock file, created if it is missing.
    pub(crate) const LOCK: Self = Self {
        read: true,
        write: true,
        append: false,
        creation: Creation::IfMissing,
        mode: 0o600,
    };
}

/// How a rename treats the destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Rename {
    /// Fail if the destination exists.
    NoReplace,
    /// Swap source and destination, both of which must exist.
    Exchange,
}

/// Whether `error` says a directory could not be removed because it has entries.
pub(crate) fn is_directory_not_empty(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::AlreadyExists
    )
}

#[cfg(target_os = "linux")]
pub(crate) use linux::*;
#[cfg(not(target_os = "linux"))]
pub(crate) use unsupported::*;

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        ffi::CString,
        fs::{self, File, Metadata, OpenOptions},
        io,
        os::{
            fd::{AsFd, AsRawFd, FromRawFd},
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
        path::Path,
    };

    use super::{Creation, EntryKind, OpenFile, Rename};

    fn c_string(value: &[u8]) -> io::Result<CString> {
        CString::new(value)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL byte"))
    }

    fn check(result: libc::c_int) -> io::Result<()> {
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn owned(fd: libc::c_int) -> io::Result<File> {
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    pub(crate) fn open_directory(path: &Path) -> io::Result<File> {
        let path = c_string(path.as_os_str().as_encoded_bytes())?;
        owned(unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })
    }

    pub(crate) fn open_directory_at(directory: &File, name: &str) -> io::Result<File> {
        let name = c_string(name.as_bytes())?;
        owned(unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })
    }

    /// Opens `name` in `directory`. A FIFO does not block the open.
    pub(crate) fn open_file_at(directory: &File, name: &str, how: OpenFile) -> io::Result<File> {
        let name = c_string(name.as_bytes())?;
        let access = match (how.read, how.write) {
            (_, false) => libc::O_RDONLY,
            (false, true) => libc::O_WRONLY,
            (true, true) => libc::O_RDWR,
        };
        let creation = match how.creation {
            Creation::Existing => 0,
            Creation::IfMissing => libc::O_CREAT,
            Creation::New => libc::O_CREAT | libc::O_EXCL,
        };
        let append = if how.append { libc::O_APPEND } else { 0 };
        owned(unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                access | creation | append | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                how.mode as libc::c_uint,
            )
        })
    }

    /// Opens the file at `path`, following symlinks in its parents but not in its
    /// last component.
    pub(crate) fn open_file(path: &Path, how: OpenFile) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options
            .read(how.read)
            .write(how.write)
            .append(how.append)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        match how.creation {
            Creation::Existing => {}
            Creation::IfMissing => {
                options.create(true).truncate(false).mode(how.mode);
            }
            Creation::New => {
                options.create_new(true).mode(how.mode);
            }
        }
        options.open(path)
    }

    pub(crate) fn make_directory_at(directory: &File, name: &str, mode: u32) -> io::Result<()> {
        let name = c_string(name.as_bytes())?;
        check(unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) })
    }

    pub(crate) fn symlink_at(target: &[u8], directory: &File, name: &str) -> io::Result<()> {
        let target = c_string(target)?;
        let name = c_string(name.as_bytes())?;
        check(unsafe { libc::symlinkat(target.as_ptr(), directory.as_raw_fd(), name.as_ptr()) })
    }

    pub(crate) fn remove_file_at(directory: &File, name: &str) -> io::Result<()> {
        let name = c_string(name.as_bytes())?;
        check(unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) })
    }

    pub(crate) fn remove_directory_at(directory: &File, name: &str) -> io::Result<()> {
        let name = c_string(name.as_bytes())?;
        check(unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) })
    }

    /// What `name` in `directory` is, or `None` if it does not exist.
    pub(crate) fn entry_kind_at(directory: &File, name: &str) -> io::Result<Option<EntryKind>> {
        let name = c_string(name.as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if let Err(error) = check(result) {
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error)
            };
        }
        Ok(Some(
            match unsafe { stat.assume_init() }.st_mode & libc::S_IFMT {
                libc::S_IFDIR => EntryKind::Directory,
                libc::S_IFREG => EntryKind::Regular,
                libc::S_IFLNK => EntryKind::Symlink,
                _ => EntryKind::Other,
            },
        ))
    }

    /// Reads the target of symlink `name` into `buffer` and returns its length. A
    /// target that fills `buffer` may have been cut short.
    pub(crate) fn read_link_at(
        directory: &File,
        name: &str,
        buffer: &mut [u8],
    ) -> io::Result<usize> {
        let name = c_string(name.as_bytes())?;
        let read = unsafe {
            libc::readlinkat(
                directory.as_raw_fd(),
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(usize::try_from(read).expect("readlinkat returned a nonnegative length"))
    }

    pub(crate) fn rename_at(
        source: &File,
        source_name: &str,
        destination: &File,
        destination_name: &str,
        how: Rename,
    ) -> io::Result<()> {
        let source_name = c_string(source_name.as_bytes())?;
        let destination_name = c_string(destination_name.as_bytes())?;
        let flags = match how {
            Rename::NoReplace => libc::RENAME_NOREPLACE,
            Rename::Exchange => libc::RENAME_EXCHANGE,
        };
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                source.as_raw_fd(),
                source_name.as_ptr(),
                destination.as_raw_fd(),
                destination_name.as_ptr(),
                flags,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Lists a directory this process holds open, through `/proc/self/fd`. The
    /// duplicate descriptor is owned (and close-on-exec), so it is closed on every
    /// path, errors included.
    pub(crate) fn read_open_directory(directory: &File) -> io::Result<fs::ReadDir> {
        let duplicate = directory.as_fd().try_clone_to_owned()?;
        fs::read_dir(format!("/proc/self/fd/{}", duplicate.as_raw_fd()))
    }

    pub(crate) fn is_executable(metadata: &Metadata) -> bool {
        metadata.mode() & 0o111 != 0
    }

    /// Whether two opened files are the same file.
    pub(crate) fn same_file(a: &Metadata, b: &Metadata) -> bool {
        a.dev() == b.dev() && a.ino() == b.ino()
    }

    /// Bytes an unprivileged user may still write on the filesystem holding `path`.
    /// The product of two `u64` counts always fits a `u128`.
    pub(crate) fn available_bytes(path: &Path) -> io::Result<u128> {
        let path = c_string(path.as_os_str().as_encoded_bytes())?;
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `path` is a valid NUL-terminated C string and `stats` points to
        // writable storage.
        check(unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) })?;
        // SAFETY: a successful `statvfs` call initialized the output structure.
        let stats = unsafe { stats.assume_init() };
        Ok(u128::from(stats.f_bavail) * u128::from(stats.f_frsize))
    }

    /// Ends the process at once, without unwinding or running destructors.
    pub(crate) fn exit_immediately(code: i32) -> ! {
        unsafe { libc::_exit(code) }
    }

    /// Kills the process with SIGKILL, as a crash would.
    pub(crate) fn kill_self() -> ! {
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
            libc::_exit(87)
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod unsupported {
    use std::{
        fs::{self, File, Metadata},
        io,
        path::Path,
    };

    use super::{EntryKind, OpenFile, Rename};

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Wheat's workspace file operations need Linux",
        ))
    }

    pub(crate) fn open_directory(_path: &Path) -> io::Result<File> {
        unsupported()
    }

    pub(crate) fn open_directory_at(_directory: &File, _name: &str) -> io::Result<File> {
        unsupported()
    }

    pub(crate) fn open_file_at(_directory: &File, _name: &str, _how: OpenFile) -> io::Result<File> {
        unsupported()
    }

    pub(crate) fn open_file(_path: &Path, _how: OpenFile) -> io::Result<File> {
        unsupported()
    }

    pub(crate) fn make_directory_at(_directory: &File, _name: &str, _mode: u32) -> io::Result<()> {
        unsupported()
    }

    pub(crate) fn symlink_at(_target: &[u8], _directory: &File, _name: &str) -> io::Result<()> {
        unsupported()
    }

    pub(crate) fn remove_file_at(_directory: &File, _name: &str) -> io::Result<()> {
        unsupported()
    }

    pub(crate) fn remove_directory_at(_directory: &File, _name: &str) -> io::Result<()> {
        unsupported()
    }

    pub(crate) fn entry_kind_at(_directory: &File, _name: &str) -> io::Result<Option<EntryKind>> {
        unsupported()
    }

    pub(crate) fn read_link_at(
        _directory: &File,
        _name: &str,
        _buffer: &mut [u8],
    ) -> io::Result<usize> {
        unsupported()
    }

    pub(crate) fn rename_at(
        _source: &File,
        _source_name: &str,
        _destination: &File,
        _destination_name: &str,
        _how: Rename,
    ) -> io::Result<()> {
        unsupported()
    }

    pub(crate) fn read_open_directory(_directory: &File) -> io::Result<fs::ReadDir> {
        unsupported()
    }

    pub(crate) fn is_executable(_metadata: &Metadata) -> bool {
        false
    }

    /// Without a file identity to compare, no two files count as the same.
    pub(crate) fn same_file(_a: &Metadata, _b: &Metadata) -> bool {
        false
    }

    pub(crate) fn available_bytes(_path: &Path) -> io::Result<u128> {
        unsupported()
    }

    pub(crate) fn exit_immediately(code: i32) -> ! {
        std::process::exit(code)
    }

    pub(crate) fn kill_self() -> ! {
        std::process::abort()
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};

    use super::*;

    #[cfg(target_os = "linux")]
    fn new_file(mode: u32) -> OpenFile {
        OpenFile {
            read: false,
            write: true,
            append: false,
            creation: Creation::New,
            mode,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn entries_are_described_without_following_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("directory")).unwrap();
        fs::write(root.path().join("file"), b"bytes").unwrap();
        symlink("directory", root.path().join("link")).unwrap();
        let directory = open_directory(root.path()).unwrap();

        assert_eq!(
            entry_kind_at(&directory, "directory").unwrap(),
            Some(EntryKind::Directory)
        );
        assert_eq!(
            entry_kind_at(&directory, "file").unwrap(),
            Some(EntryKind::Regular)
        );
        assert_eq!(
            entry_kind_at(&directory, "link").unwrap(),
            Some(EntryKind::Symlink)
        );
        assert_eq!(entry_kind_at(&directory, "missing").unwrap(), None);

        let mut target = [0_u8; 64];
        let length = read_link_at(&directory, "link", &mut target).unwrap();
        assert_eq!(&target[..length], b"directory");
        let mut names = read_open_directory(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["directory", "file", "link"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn opens_refuse_a_symlink_in_the_last_component() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("directory")).unwrap();
        fs::write(root.path().join("file"), b"bytes").unwrap();
        symlink("file", root.path().join("file-link")).unwrap();
        symlink("directory", root.path().join("directory-link")).unwrap();
        let directory = open_directory(root.path()).unwrap();

        open_file_at(&directory, "file", OpenFile::READ).unwrap();
        open_file(&root.path().join("file"), OpenFile::READ).unwrap();
        open_directory_at(&directory, "directory").unwrap();
        assert!(open_file_at(&directory, "file-link", OpenFile::READ).is_err());
        assert!(open_file(&root.path().join("file-link"), OpenFile::READ).is_err());
        assert!(open_file(&root.path().join("file-link"), OpenFile::LOCK).is_err());
        assert!(open_directory_at(&directory, "directory-link").is_err());
        assert!(open_directory(&root.path().join("directory-link")).is_err());
        // A lock file is created once and then reopened, never truncated.
        open_file(&root.path().join("lock"), OpenFile::LOCK).unwrap();
        fs::write(root.path().join("lock"), b"kept").unwrap();
        open_file(&root.path().join("lock"), OpenFile::LOCK).unwrap();
        assert_eq!(fs::read(root.path().join("lock")).unwrap(), b"kept");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn created_files_and_directories_take_the_requested_mode() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let directory = open_directory(root.path()).unwrap();
        let file = open_file_at(&directory, "private", new_file(0o600)).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!is_executable(&file.metadata().unwrap()));
        let script = open_file_at(&directory, "script", new_file(0o700)).unwrap();
        assert!(is_executable(&script.metadata().unwrap()));
        let error = open_file_at(&directory, "private", new_file(0o600)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        make_directory_at(&directory, "made", 0o700).unwrap();
        let made = fs::metadata(root.path().join("made")).unwrap();
        assert_eq!(made.permissions().mode() & 0o777, 0o700);
        symlink_at(b"made", &directory, "made-link").unwrap();
        assert_eq!(
            fs::read_link(root.path().join("made-link")).unwrap(),
            std::path::Path::new("made")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn renames_keep_or_swap_the_destination() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), b"a").unwrap();
        fs::write(root.path().join("b"), b"b").unwrap();
        let directory = open_directory(root.path()).unwrap();

        let error = rename_at(&directory, "a", &directory, "b", Rename::NoReplace).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        rename_at(&directory, "a", &directory, "b", Rename::Exchange).unwrap();
        assert_eq!(fs::read(root.path().join("a")).unwrap(), b"b");
        assert_eq!(fs::read(root.path().join("b")).unwrap(), b"a");
        rename_at(&directory, "a", &directory, "c", Rename::NoReplace).unwrap();
        assert!(!root.path().join("a").exists());
        assert_eq!(fs::read(root.path().join("c")).unwrap(), b"b");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn removal_tells_a_nonempty_directory_apart() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("full/inner")).unwrap();
        fs::create_dir(root.path().join("empty")).unwrap();
        fs::write(root.path().join("file"), b"bytes").unwrap();
        let directory = open_directory(root.path()).unwrap();

        let error = remove_directory_at(&directory, "full").unwrap_err();
        assert!(is_directory_not_empty(&error), "{error:?}");
        remove_directory_at(&directory, "empty").unwrap();
        remove_file_at(&directory, "file").unwrap();
        let error = remove_file_at(&directory, "file").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!is_directory_not_empty(&error));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_and_free_space_come_from_the_filesystem() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), b"a").unwrap();
        fs::write(root.path().join("b"), b"b").unwrap();
        let a = File::open(root.path().join("a"))
            .unwrap()
            .metadata()
            .unwrap();
        let again = fs::metadata(root.path().join("a")).unwrap();
        let b = fs::metadata(root.path().join("b")).unwrap();
        assert!(same_file(&a, &again));
        assert!(!same_file(&a, &b));
        assert!(available_bytes(root.path()).unwrap() > 0);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn every_call_is_refused_off_linux() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), b"bytes").unwrap();
        let file = File::open(root.path().join("file")).unwrap();
        let refused = |error: io::Error| error.kind() == io::ErrorKind::Unsupported;

        assert!(refused(open_directory(root.path()).unwrap_err()));
        assert!(refused(
            open_file(&root.path().join("file"), OpenFile::READ).unwrap_err()
        ));
        assert!(refused(
            open_file_at(&file, "file", OpenFile::READ).unwrap_err()
        ));
        assert!(refused(entry_kind_at(&file, "file").unwrap_err()));
        assert!(refused(remove_file_at(&file, "file").unwrap_err()));
        assert!(refused(
            rename_at(&file, "a", &file, "b", Rename::NoReplace).unwrap_err()
        ));
        assert!(refused(read_open_directory(&file).unwrap_err()));
        assert!(refused(available_bytes(root.path()).unwrap_err()));
        let metadata = file.metadata().unwrap();
        assert!(!same_file(&metadata, &metadata));
    }
}
