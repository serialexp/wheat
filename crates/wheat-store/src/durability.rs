use std::{fs::File, io, path::Path};

/// Persistence policy for filesystem-backed stores.
///
/// Deferred durability is intended only for an isolated directory that will be
/// committed with [`sync_filesystem`] before it becomes visible. It does not
/// provide crash-resumable progress while writes are in flight.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Durability {
    #[default]
    Durable,
    Deferred,
}

impl Durability {
    pub(crate) fn sync_file(self, file: &File) -> io::Result<()> {
        match self {
            Self::Durable => file.sync_all(),
            Self::Deferred => Ok(()),
        }
    }

    pub(crate) fn sync_directory(self, path: &Path) -> io::Result<()> {
        match self {
            Self::Durable => File::open(path)?.sync_all(),
            Self::Deferred => Ok(()),
        }
    }
}

/// Waits until every dirty write on the filesystem containing `path` is durable.
///
/// This is the terminal dependency barrier used by isolated atomic imports. Only
/// Linux has `syncfs`; elsewhere this fails rather than sync less than it promises.
#[cfg(target_os = "linux")]
pub fn sync_filesystem(path: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let file = File::open(path)?;
    let result = unsafe { libc::syncfs(file.as_raw_fd()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
pub fn sync_filesystem(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "syncing a whole filesystem needs Linux's syncfs",
    ))
}

pub fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
