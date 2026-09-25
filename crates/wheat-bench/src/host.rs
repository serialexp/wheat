//! Facts about the run that decide whether two results are comparable: the source
//! revision, the build, the CPU, and the filesystem the benchmarks write to.

use std::{
    collections::BTreeMap,
    ffi::CString,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

/// The source checkout being measured.
#[derive(Clone, Debug)]
pub struct Checkout {
    pub root: PathBuf,
    pub rev: String,
    pub dirty: bool,
}

impl Checkout {
    /// The Git checkout containing `directory`.
    pub fn find(directory: &Path) -> Result<Self> {
        let root = PathBuf::from(git(directory, &["rev-parse", "--show-toplevel"])?);
        Ok(Self {
            rev: git(&root, &["rev-parse", "--short=12", "HEAD"])?,
            dirty: !git(&root, &["status", "--porcelain"])?.is_empty(),
            root,
        })
    }
}

fn git(directory: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .context("run git")?;
    ensure!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// The CPU model name from `/proc/cpuinfo`.
pub fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find_map(|line| line.strip_prefix("model name"))
                .and_then(|rest| rest.split_once(':'))
                .map(|(_, model)| model.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".into())
}

pub fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

/// The kind of filesystem at `path`: durable-write benchmarks on `tmpfs` measure
/// memory, not a disk.
pub fn filesystem(path: &Path) -> Result<String> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: an all-zero `statfs` is a valid value; `statfs` overwrites it.
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stats` is a valid out-pointer.
    let result = unsafe { libc::statfs(path.as_ptr(), &mut stats) };
    ensure!(result == 0, "statfs: {}", std::io::Error::last_os_error());
    #[allow(clippy::unnecessary_cast)] // `f_type`'s integer type differs by target.
    Ok(match stats.f_type as i64 {
        0x0102_1994 => "tmpfs",
        0xEF53 => "ext4",
        0x5846_5342 => "xfs",
        0x9123_683E => "btrfs",
        0x794C_7630 => "overlayfs",
        0x2FC1_2FC1 => "zfs",
        0x6969 => "nfs",
        other => return Ok(format!("0x{other:x}")),
    }
    .to_owned())
}

/// Workload parameters every profile records.
pub fn parameters(checkout: &Checkout, scratch: &Path) -> Result<BTreeMap<String, Value>> {
    Ok(BTreeMap::from([
        ("rev".into(), json!(checkout.rev)),
        ("dirty".into(), json!(checkout.dirty)),
        ("release".into(), json!(cfg!(not(debug_assertions)))),
        ("cpu".into(), json!(cpu_model())),
        ("cores".into(), json!(cores())),
        ("scratch_fs".into(), json!(filesystem(scratch)?)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checkout_and_host_are_described() {
        let checkout = Checkout::find(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        assert!(checkout.root.join("Cargo.toml").is_file());
        assert_eq!(checkout.rev.len(), 12);
        assert!(cores() >= 1);
        assert!(!cpu_model().is_empty());
        let filesystem = filesystem(Path::new("/proc")).unwrap();
        assert_eq!(filesystem, "0x9fa0", "procfs has its own magic number");
    }
}
