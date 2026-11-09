//! Bounded, read-only snapshots of explicitly named workspace files.

#[cfg(test)]
use std::cell::Cell;
use std::{ffi::OsStr, fmt, fs::File, io, path::Path};

#[cfg(test)]
thread_local! {
    static TEST_SNAPSHOT_ALLOCATED_BYTES: Cell<usize> = const { Cell::new(0) };
    static TEST_STREAM_READS: Cell<usize> = const { Cell::new(0) };
    static TEST_STREAM_SCRATCH_BYTES: Cell<usize> = const { Cell::new(0) };
    static TEST_SAMPLE_READS: Cell<usize> = const { Cell::new(0) };
}

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use wheat_core::text_capture::{CaptureKind, StreamingCapture};
use wheat_core::{ContentId, FileKind, FileValue};

#[cfg(unix)]
const STREAM_SCRATCH_BYTES: usize = 64 * 1024;

/// Exact, bounded authentication of a raw workspace leaf: its kind, length and the
/// content ID digest of its raw bytes (a symlink's target).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPreimage {
    pub kind: FileKind,
    pub length: u64,
    pub digest: [u8; 32],
}

impl RawPreimage {
    /// The leaf that materializing `value`, of `length` bytes, leaves: materialization
    /// writes a value's stored bytes unchanged, so its content ID is the leaf's digest.
    /// Known from stored metadata, without reading the content.
    #[must_use]
    pub fn of_value(value: FileValue, length: u64) -> Self {
        Self {
            kind: value.kind,
            length,
            digest: *value.content.as_bytes(),
        }
    }
}

/// An owned snapshot suitable for borrowing as an `edit_capture::FileSnapshot`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceFileSnapshot {
    pub kind: FileKind,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalWorkspaceFile {
    pub kind: FileKind,
    pub content: ContentId,
    pub bytes: usize,
    pub read_bytes: usize,
    pub text: bool,
    pub retained_text: Option<Vec<u8>>,
    /// The leaf's stat, when it was the same before and after the read, so the
    /// content above is what a leaf with that stat holds.
    pub stamp: Option<FileStamp>,
    /// [`FileSamples`] of a binary regular file at least [`SAMPLED_FILE_BYTES`] long.
    pub samples: Option<FileSamples>,
}

/// What a leaf's stat says about it: a leaf whose stamp is unchanged has not been
/// written, renamed over, or had its mode changed since. `ctime` changes on every
/// write and cannot be set back, so it catches a write whose mtime was restored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStamp {
    /// The raw length: a symlink target's, or a regular file's on disk.
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub inode: u64,
    pub device: u64,
    /// `st_mode`: the file type and permission bits.
    pub mode: u32,
}

impl FileStamp {
    /// The kind of file the stamp describes.
    #[must_use]
    pub fn kind(&self) -> FileKind {
        const TYPE: u32 = 0o170_000;
        const SYMLINK: u32 = 0o120_000;
        if self.mode & TYPE == SYMLINK {
            FileKind::Symlink
        } else if self.mode & 0o111 != 0 {
            FileKind::Executable
        } else {
            FileKind::Regular
        }
    }
}

/// A digest of [`SAMPLE_COUNT`] evenly spaced [`SAMPLE_BYTES`]-byte pieces of a long
/// file and its length: two files with different samples differ, without reading the
/// rest of either.
pub type FileSamples = [u8; 32];

/// Files at least this long get [`FileSamples`]: the length past which content is
/// chunked.
pub const SAMPLED_FILE_BYTES: u64 = wheat_core::CHUNKED_CONTENT_THRESHOLD;
pub const SAMPLE_COUNT: u64 = 256;
pub const SAMPLE_BYTES: usize = 4 * 1024;

#[derive(Debug)]
pub enum WorkspaceFileError {
    InvalidPath,
    IntermediateSymlink,
    /// An ancestor of the path is a file or another non-directory, so the path
    /// itself cannot exist. Capture reads it as deleted (see
    /// [`absent_where_shape_changed`]); materialization, which must not replace
    /// that ancestor unasked, treats it like an unsupported leaf.
    AncestorNotDirectory,
    /// The path is a directory. Only the reads capture uses tell this apart from
    /// [`Self::UnsupportedFileType`]; capture reads it as the file deleted.
    LeafIsDirectory,
    UnsupportedFileType,
    TooLarge {
        limit: usize,
    },
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
            Self::AncestorNotDirectory => {
                f.write_str("workspace path has a non-directory as an intermediate component")
            }
            Self::LeafIsDirectory => f.write_str("workspace path is a directory"),
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
pub fn read_workspace_file(
    root: &Path,
    path: &Path,
    byte_limit: usize,
) -> Result<Option<WorkspaceFileSnapshot>, WorkspaceFileError> {
    let components = validate_path(path)?;
    platform::read(root, &components, byte_limit)
}

/// Read a tracked path whose shape changed as absent, for capture and status: a
/// tracked directory replaced by a file deletes every path that was below it, and
/// a tracked file replaced by a directory deletes the file.
pub fn absent_where_shape_changed<T>(
    read: Result<Option<T>, WorkspaceFileError>,
) -> Result<Option<T>, WorkspaceFileError> {
    match read {
        Err(WorkspaceFileError::AncestorNotDirectory | WorkspaceFileError::LeafIsDirectory) => {
            Ok(None)
        }
        other => other,
    }
}

/// Analyze one descriptor-relative workspace leaf with fixed read scratch.
///
/// Regular files are never collected. At most `retain_limit` canonical text
/// bytes are retained for an inline diff. Symlink targets use their portable
/// 4 KiB bound and are always classified as binary.
pub fn analyze_workspace_file(
    root: &Path,
    path: &Path,
    byte_limit: usize,
    retain_limit: usize,
) -> Result<Option<CanonicalWorkspaceFile>, WorkspaceFileError> {
    let components = validate_path(path)?;
    platform::analyze(root, &components, byte_limit, retain_limit)
}

/// Stats a workspace's leaves one after another without reading them. The
/// directories above a leaf stay open for the next one, so leaves in the same
/// directory cost one stat each: give them in tree order. A directory renamed while
/// it is open is still the one stat'd, as if its leaves had been stat'd before the
/// rename.
pub struct WorkspaceStats {
    directories: platform::Directories,
}

impl WorkspaceStats {
    pub fn new(root: &Path) -> Result<Self, WorkspaceFileError> {
        Ok(Self {
            directories: platform::Directories::new(root)?,
        })
    }

    /// The stamp of the leaf at `path`; `None` when it is missing. A directory or
    /// special file there is refused as [`analyze_workspace_file`] refuses it.
    pub fn stat(&mut self, path: &Path) -> Result<Option<FileStamp>, WorkspaceFileError> {
        let components = validate_path(path)?;
        self.directories.stat(&components)
    }
}

/// The [`FileSamples`] of the regular file at `path` if it still has `stamp`, read
/// without reading the rest of it; `None` when it no longer has that stamp.
pub fn sample_workspace_file(
    root: &Path,
    path: &Path,
    stamp: &FileStamp,
) -> Result<Option<FileSamples>, WorkspaceFileError> {
    let components = validate_path(path)?;
    platform::sample(root, &components, stamp)
}

/// Streams the regular file at `path` to `out`, a piece at a time: its canonical text
/// when `text` (see [`analyze_workspace_file`]), else its bytes. Returns its kind, or
/// `None` when it is missing.
pub fn stream_workspace_file(
    root: &Path,
    path: &Path,
    text: bool,
    byte_limit: usize,
    out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
) -> Result<Option<FileKind>, WorkspaceFileError> {
    let components = validate_path(path)?;
    platform::stream(root, &components, text, byte_limit, out)
}

/// Observe a raw leaf with a streaming digest. Missing paths are represented by
/// `None`; regular contents are never collected.
pub fn observe_workspace_file(
    root: &Path,
    path: &Path,
    byte_limit: usize,
) -> Result<Option<RawPreimage>, WorkspaceFileError> {
    let components = validate_path(path)?;
    platform::observe(root, &components, byte_limit, None)
}

/// Whether nothing is at `path`, without reading a leaf that is there. A directory
/// or special file at `path` is refused rather than reported as present.
pub fn workspace_file_is_absent(root: &Path, path: &Path) -> Result<bool, WorkspaceFileError> {
    let components = validate_path(path)?;
    platform::is_absent(root, &components)
}

/// Whether the leaf at `path` is exactly `expected`, streaming a regular file
/// through fixed scratch. A file of another kind or length is not read.
pub fn authenticate_workspace_preimage(
    root: &Path,
    path: &Path,
    expected: &RawPreimage,
    byte_limit: usize,
) -> Result<bool, WorkspaceFileError> {
    let components = validate_path(path)?;
    Ok(
        platform::observe(root, &components, byte_limit, Some(expected))?.as_ref()
            == Some(expected),
    )
}

/// Authenticate a leaf relative to an already-open directory descriptor.
/// Regular contents are streamed through fixed scratch, and a file of another kind
/// or length is not read; symlink targets are bounded to the maximum portable
/// target size used by materialization.
pub fn authenticate_preimage_at(
    directory: &File,
    name: &OsStr,
    expected: &RawPreimage,
    byte_limit: usize,
) -> Result<bool, WorkspaceFileError> {
    Ok(
        platform::observe_at(directory, name, byte_limit, Some(expected))?.as_ref()
            == Some(expected),
    )
}

pub fn raw_preimage(kind: FileKind, bytes: &[u8]) -> RawPreimage {
    let mut hasher = ContentId::hasher();
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
                    && name
                        .to_str()
                        .is_some_and(|name| wheat_ignore::WORKSPACE_CONTROL_NAMES.contains(&name))
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
        ffi::{CString, OsStr, OsString},
        fs::File,
        io::{self, Read},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
        path::Path,
    };

    use wheat_core::text_capture::LineEndings;

    use super::{
        CanonicalWorkspaceFile, CaptureKind, ContentId, FileKind, FileSamples, FileStamp,
        RawPreimage, SAMPLE_BYTES, SAMPLE_COUNT, SAMPLED_FILE_BYTES, STREAM_SCRATCH_BYTES,
        StreamingCapture, WorkspaceFileError, WorkspaceFileSnapshot,
    };

    pub(super) fn analyze(
        root: &Path,
        components: &[&OsStr],
        limit: usize,
        retain_limit: usize,
    ) -> Result<Option<CanonicalWorkspaceFile>, WorkspaceFileError> {
        let Some(directory) = open_parent(root, components)? else {
            return Ok(None);
        };
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
        let before = stamp_of_stat(&stat);
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
                // A symlink's target cannot change in place.
                stamp: Some(before),
                samples: None,
            }));
        }
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => {}
            libc::S_IFDIR => return Err(WorkspaceFileError::LeafIsDirectory),
            _ => return Err(WorkspaceFileError::UnsupportedFileType),
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
        let text = result.kind == CaptureKind::Text;
        // Samples are of the raw bytes, so they only stand for binary content, whose
        // canonical bytes are its raw bytes.
        let mut samples = None;
        if !text && raw_length as u64 >= SAMPLED_FILE_BYTES {
            match sample_digest(&file, raw_length as u64) {
                Ok(digest) => samples = Some(digest),
                // The file shrank under the read; the stamp below differs too.
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {}
                Err(error) => return Err(error.into()),
            }
        }
        let after = stamp_of_metadata(&file.metadata()?);
        let stamp = (after == before && after.size == raw_length as u64).then_some(before);
        Ok(Some(CanonicalWorkspaceFile {
            kind,
            content: ContentId::from_digest(result.digest),
            bytes: result.bytes,
            read_bytes: raw_length,
            text,
            retained_text: result.retained,
            stamp,
            samples: samples.filter(|_| stamp.is_some()),
        }))
    }

    /// The workspace's root and the directories below it last stat'd in, outermost
    /// first.
    pub(super) struct Directories {
        root: File,
        open: Vec<(OsString, File)>,
    }

    impl Directories {
        pub(super) fn new(root: &Path) -> Result<Self, WorkspaceFileError> {
            Ok(Self {
                root: File::open(root)?,
                open: Vec::new(),
            })
        }

        pub(super) fn stat(
            &mut self,
            components: &[&OsStr],
        ) -> Result<Option<FileStamp>, WorkspaceFileError> {
            let (name, parents) = components.split_last().expect("validated nonempty path");
            let kept = self
                .open
                .iter()
                .zip(parents)
                .take_while(|((open, _), parent)| open.as_os_str() == **parent)
                .count();
            self.open.truncate(kept);
            for parent in &parents[kept..] {
                let above = self
                    .open
                    .last()
                    .map_or(&self.root, |(_, directory)| directory);
                let Some(directory) = open_directory(above, parent)? else {
                    return Ok(None);
                };
                self.open.push(((*parent).to_owned(), directory));
            }
            let directory = self
                .open
                .last()
                .map_or(&self.root, |(_, directory)| directory);
            stat_at(directory, name)
        }
    }

    fn stat_at(directory: &File, name: &OsStr) -> Result<Option<FileStamp>, WorkspaceFileError> {
        let name = c_name(name)?;
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
            libc::S_IFLNK | libc::S_IFREG => Ok(Some(stamp_of_stat(&stat))),
            libc::S_IFDIR => Err(WorkspaceFileError::LeafIsDirectory),
            _ => Err(WorkspaceFileError::UnsupportedFileType),
        }
    }

    pub(super) fn sample(
        root: &Path,
        components: &[&OsStr],
        stamp: &FileStamp,
    ) -> Result<Option<FileSamples>, WorkspaceFileError> {
        let Some(directory) = open_parent(root, components)? else {
            return Ok(None);
        };
        let name = c_name(components.last().expect("validated nonempty path"))?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ENOENT | libc::ELOOP) => Ok(None),
                _ => Err(error.into()),
            };
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if stamp_of_metadata(&file.metadata()?) != *stamp {
            return Ok(None);
        }
        let samples = match sample_digest(&file, stamp.size) {
            Ok(samples) => samples,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok((stamp_of_metadata(&file.metadata()?) == *stamp).then_some(samples))
    }

    /// The [`FileSamples`] of `file`, `length` bytes long: the length and the
    /// samples, evenly spaced from the first byte to the last.
    fn sample_digest(file: &File, length: u64) -> io::Result<FileSamples> {
        use std::os::unix::fs::FileExt as _;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"wheat-file-samples\0");
        hasher.update(&length.to_le_bytes());
        let span = length.saturating_sub(SAMPLE_BYTES as u64);
        let mut piece = [0_u8; SAMPLE_BYTES];
        for index in 0..SAMPLE_COUNT {
            let offset = span * index / (SAMPLE_COUNT - 1);
            file.read_exact_at(&mut piece, offset)?;
            #[cfg(test)]
            super::TEST_SAMPLE_READS.with(|reads| reads.set(reads.get() + 1));
            hasher.update(&piece);
        }
        Ok(*hasher.finalize().as_bytes())
    }

    // `st_dev` and `st_mode` are u64 and u32 on Linux but i32 and u16 on
    // macOS. They are cast the way std's `MetadataExt::dev` and `mode` cast
    // them, so this stamp equals `stamp_of_metadata`'s for the same file on
    // every platform; on Linux the casts are no-ops.
    #[allow(clippy::unnecessary_cast)]
    fn stamp_of_stat(stat: &libc::stat) -> FileStamp {
        FileStamp {
            size: stat.st_size as u64,
            mtime_ns: stat.st_mtime * 1_000_000_000 + stat.st_mtime_nsec,
            ctime_ns: stat.st_ctime * 1_000_000_000 + stat.st_ctime_nsec,
            inode: stat.st_ino,
            device: stat.st_dev as u64,
            mode: stat.st_mode as u32,
        }
    }

    fn stamp_of_metadata(metadata: &std::fs::Metadata) -> FileStamp {
        use std::os::unix::fs::MetadataExt as _;
        FileStamp {
            size: metadata.size(),
            mtime_ns: metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
            ctime_ns: metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec(),
            inode: metadata.ino(),
            device: metadata.dev(),
            mode: metadata.mode(),
        }
    }

    pub(super) fn observe(
        root: &Path,
        components: &[&OsStr],
        limit: usize,
        expected: Option<&RawPreimage>,
    ) -> Result<Option<RawPreimage>, WorkspaceFileError> {
        let Some(directory) = open_parent(root, components)? else {
            return Ok(None);
        };
        let name = c_name(components.last().expect("validated nonempty path"))?;
        observe_leaf(directory.as_raw_fd(), &name, limit, expected)
    }

    pub(super) fn observe_at(
        directory: &File,
        name: &OsStr,
        limit: usize,
        expected: Option<&RawPreimage>,
    ) -> Result<Option<RawPreimage>, WorkspaceFileError> {
        observe_leaf(directory.as_raw_fd(), &c_name(name)?, limit, expected)
    }

    pub(super) fn is_absent(
        root: &Path,
        components: &[&OsStr],
    ) -> Result<bool, WorkspaceFileError> {
        let Some(directory) = open_parent(root, components)? else {
            return Ok(true);
        };
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
                Ok(true)
            } else {
                Err(error.into())
            };
        }
        let stat = unsafe { stat.assume_init() };
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFLNK | libc::S_IFREG => Ok(false),
            _ => Err(WorkspaceFileError::UnsupportedFileType),
        }
    }

    /// Observes the leaf. When `expected` is given and a regular file's kind or length
    /// already differs from it, the file is not read: the result then has a zero
    /// digest, and is only good for comparing with `expected`, which it never equals.
    fn observe_leaf(
        directory: libc::c_int,
        name: &CString,
        limit: usize,
        expected: Option<&RawPreimage>,
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
                if expected.is_some_and(|expected| {
                    expected.kind != kind || expected.length != opened.len()
                }) {
                    return Ok(Some(RawPreimage {
                        kind,
                        length: opened.len(),
                        digest: [0; 32],
                    }));
                }
                let mut hasher = ContentId::hasher();
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
        let mut hasher = ContentId::hasher();
        hasher.update(bytes);
        *hasher.finalize().as_bytes()
    }

    pub(super) fn read(
        root: &Path,
        components: &[&OsStr],
        limit: usize,
    ) -> Result<Option<WorkspaceFileSnapshot>, WorkspaceFileError> {
        let Some(directory) = open_parent(root, components)? else {
            return Ok(None);
        };

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
                // A target is at most 4,095 bytes; `read_link` allocates its limit.
                let bytes = read_link(directory.as_raw_fd(), &name, limit.min(4_095))?;
                Ok(Some(WorkspaceFileSnapshot {
                    kind: FileKind::Symlink,
                    bytes,
                }))
            }
            libc::S_IFREG => open_regular(directory.as_raw_fd(), &name, limit).map(Some),
            libc::S_IFDIR => Err(WorkspaceFileError::LeafIsDirectory),
            _ => Err(WorkspaceFileError::UnsupportedFileType),
        }
    }

    /// Opens the directory holding the leaf of `components`, without following a
    /// symlink on the way; `None` when a directory on the way is missing.
    fn open_parent(root: &Path, components: &[&OsStr]) -> Result<Option<File>, WorkspaceFileError> {
        let mut directory = File::open(root)?;
        for component in &components[..components.len() - 1] {
            match open_directory(&directory, component)? {
                Some(next) => directory = next,
                None => return Ok(None),
            }
        }
        Ok(Some(directory))
    }

    /// The directory `name` in `directory`, not following a symlink; `None` when it
    /// is missing.
    fn open_directory(directory: &File, name: &OsStr) -> Result<Option<File>, WorkspaceFileError> {
        let name = c_name(name)?;
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
                Some(libc::ENOTDIR) => Err(WorkspaceFileError::AncestorNotDirectory),
                _ => Err(error.into()),
            };
        }
        Ok(Some(unsafe { File::from_raw_fd(fd) }))
    }

    /// Streams the regular file at `components` to `out`: its canonical text when
    /// `text`, else its bytes. Returns its kind, or `None` when it is missing. A leaf
    /// that is no longer a regular file, or has grown past `limit`, is refused.
    pub(super) fn stream(
        root: &Path,
        components: &[&OsStr],
        text: bool,
        limit: usize,
        out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Result<Option<FileKind>, WorkspaceFileError> {
        let Some(directory) = open_parent(root, components)? else {
            return Ok(None);
        };
        let name = c_name(components.last().expect("validated nonempty path"))?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ENOENT) => Ok(None),
                // A symlink now stands where the file was.
                Some(libc::ELOOP) => Err(WorkspaceFileError::UnsupportedFileType),
                _ => Err(error.into()),
            };
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
        let mut line_endings = LineEndings::default();
        let mut scratch = vec![0_u8; STREAM_SCRATCH_BYTES];
        let mut length = 0usize;
        let mut failed = None;
        loop {
            let read = file.read(&mut scratch)?;
            if read == 0 {
                break;
            }
            length = length.saturating_add(read);
            if length > limit {
                return Err(WorkspaceFileError::TooLarge { limit });
            }
            let piece = &scratch[..read];
            if text {
                line_endings.update(piece, |bytes| {
                    if failed.is_none()
                        && let Err(error) = out(bytes)
                    {
                        failed = Some(error);
                    }
                });
            } else {
                out(piece)?;
            }
            if let Some(error) = failed.take() {
                return Err(error.into());
            }
        }
        if text {
            line_endings.finish(|bytes| {
                if let Err(error) = out(bytes) {
                    failed = Some(error);
                }
            });
            if let Some(error) = failed {
                return Err(error.into());
            }
        }
        Ok(Some(kind))
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

/// Workspace files are read only through no-follow, descriptor-relative opens, which
/// exist here only for Unix. Elsewhere every read fails, so a path-based read that
/// could race or follow a symlink cannot be switched on by accident.
#[cfg(not(unix))]
mod platform {
    use std::{ffi::OsStr, fs::File, io, path::Path};

    use super::{
        CanonicalWorkspaceFile, FileKind, FileSamples, FileStamp, RawPreimage, WorkspaceFileError,
        WorkspaceFileSnapshot,
    };

    pub(super) struct Directories;

    impl Directories {
        pub(super) fn new(_root: &Path) -> Result<Self, WorkspaceFileError> {
            Ok(Self)
        }

        pub(super) fn stat(
            &mut self,
            _components: &[&OsStr],
        ) -> Result<Option<FileStamp>, WorkspaceFileError> {
            unsupported()
        }
    }

    pub(super) fn sample(
        _root: &Path,
        _components: &[&OsStr],
        _stamp: &FileStamp,
    ) -> Result<Option<FileSamples>, WorkspaceFileError> {
        unsupported()
    }

    fn unsupported<T>() -> Result<T, WorkspaceFileError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "workspace files can only be read safely on Unix",
        )
        .into())
    }

    pub(super) fn analyze(
        _root: &Path,
        _components: &[&OsStr],
        _limit: usize,
        _retain_limit: usize,
    ) -> Result<Option<CanonicalWorkspaceFile>, WorkspaceFileError> {
        unsupported()
    }

    pub(super) fn observe(
        _root: &Path,
        _components: &[&OsStr],
        _limit: usize,
        _expected: Option<&RawPreimage>,
    ) -> Result<Option<RawPreimage>, WorkspaceFileError> {
        unsupported()
    }

    pub(super) fn read(
        _root: &Path,
        _components: &[&OsStr],
        _limit: usize,
    ) -> Result<Option<WorkspaceFileSnapshot>, WorkspaceFileError> {
        unsupported()
    }

    pub(super) fn observe_at(
        _directory: &File,
        _name: &OsStr,
        _limit: usize,
        _expected: Option<&RawPreimage>,
    ) -> Result<Option<RawPreimage>, WorkspaceFileError> {
        unsupported()
    }

    pub(super) fn is_absent(
        _root: &Path,
        _components: &[&OsStr],
    ) -> Result<bool, WorkspaceFileError> {
        unsupported()
    }

    pub(super) fn stream(
        _root: &Path,
        _components: &[&OsStr],
        _text: bool,
        _limit: usize,
        _out: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Result<Option<FileKind>, WorkspaceFileError> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    #[cfg(not(unix))]
    #[test]
    fn every_read_is_refused_off_unix() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("file"), b"bytes").unwrap();
        let path = Path::new("file");
        let unsupported = |error: WorkspaceFileError| match error {
            WorkspaceFileError::Io(error) => error.kind() == io::ErrorKind::Unsupported,
            _ => false,
        };
        assert!(unsupported(
            read_workspace_file(root.path(), path, 64).unwrap_err()
        ));
        assert!(unsupported(
            analyze_workspace_file(root.path(), path, 64, 64).unwrap_err()
        ));
        assert!(unsupported(
            observe_workspace_file(root.path(), path, 64).unwrap_err()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn streaming_a_file_passes_on_the_bytes_analysis_hashed() {
        let root = tempfile::tempdir().unwrap();
        let stream = |path: &str, text: bool| {
            let mut out = Vec::new();
            let kind =
                stream_workspace_file(root.path(), Path::new(path), text, 1 << 20, &mut |b| {
                    out.extend_from_slice(b);
                    Ok(())
                })
                .unwrap();
            (kind, out)
        };
        // A CR at the scratch boundary, with its LF in the next read.
        let mut text = vec![b'x'; STREAM_SCRATCH_BYTES + 10];
        text[STREAM_SCRATCH_BYTES - 1] = b'\r';
        text[STREAM_SCRATCH_BYTES] = b'\n';
        text.extend_from_slice(b"end\r");
        fs::write(root.path().join("text"), &text).unwrap();
        let analyzed = analyze_workspace_file(root.path(), Path::new("text"), 1 << 20, 0)
            .unwrap()
            .unwrap();
        assert!(analyzed.text);
        let (kind, out) = stream("text", true);
        assert_eq!(kind, Some(FileKind::Regular));
        assert_eq!(ContentId::of(&out), analyzed.content);
        assert_eq!(out.len(), analyzed.bytes);

        // Binary bytes pass unchanged.
        fs::write(root.path().join("binary"), b"\0a\r\n").unwrap();
        assert_eq!(stream("binary", false).1, b"\0a\r\n");
        assert_eq!(stream("missing", false), (None, Vec::new()));
        assert!(matches!(
            stream_workspace_file(root.path(), Path::new("text"), true, 16, &mut |_| Ok(())),
            Err(WorkspaceFileError::TooLarge { limit: 16 })
        ));
    }

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
            authenticate_workspace_preimage(
                root.path(),
                Path::new("file"),
                &raw_preimage(FileKind::Regular, &bytes),
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
    fn raw_preimage_streams_kind_length_and_content_digest() {
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
    fn stats_keep_directories_open_from_one_leaf_to_the_next() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("a/b")).unwrap();
        fs::create_dir(root.path().join("c")).unwrap();
        for (path, bytes) in [
            ("a/b/x", 1),
            ("a/b/y", 2),
            ("a/z", 3),
            ("c/w", 4),
            ("top", 5),
        ] {
            fs::write(root.path().join(path), vec![b'x'; bytes]).unwrap();
        }
        fs::write(outside.path().join("secret"), b"secret").unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();

        let mut stats = WorkspaceStats::new(root.path()).unwrap();
        let mut size = |path: &str| {
            stats
                .stat(Path::new(path))
                .map(|stamp| stamp.map(|stamp| stamp.size))
        };
        assert_eq!(size("a/b/x").unwrap(), Some(1));
        assert_eq!(size("a/b/y").unwrap(), Some(2));
        assert_eq!(size("a/z").unwrap(), Some(3));
        assert_eq!(size("a/missing/v").unwrap(), None);
        assert_eq!(size("c/w").unwrap(), Some(4));
        assert_eq!(size("top").unwrap(), Some(5));
        assert!(matches!(
            size("escape/secret"),
            Err(WorkspaceFileError::IntermediateSymlink)
        ));
        assert!(matches!(
            size("a/b"),
            Err(WorkspaceFileError::LeafIsDirectory)
        ));
        assert!(matches!(
            size("top/v"),
            Err(WorkspaceFileError::AncestorNotDirectory)
        ));
        assert!(matches!(size("../x"), Err(WorkspaceFileError::InvalidPath)));
        assert_eq!(size("a/b/x").unwrap(), Some(1));
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
            Err(WorkspaceFileError::LeafIsDirectory)
        ));
        fs::write(root.path().join("file"), b"x").unwrap();
        assert!(matches!(
            read_workspace_file(root.path(), Path::new("file/child"), 10),
            Err(WorkspaceFileError::AncestorNotDirectory)
        ));
        // Capture reads both as the file deleted.
        for path in ["directory", "file/child"] {
            assert_eq!(
                absent_where_shape_changed(read_workspace_file(root.path(), Path::new(path), 10))
                    .unwrap(),
                None,
                "{path}"
            );
        }
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
