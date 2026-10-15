//! Splitting the CPUs this process may use between a server and its clients, so the
//! two contend for the server's state rather than for the same cores.
//!
//! Whole physical cores go to the server, with their hyperthread siblings, and the
//! rest to the clients. A thread pinned with [`CpuSet::pin_current_thread`] passes its
//! mask on to every process it starts.

use std::{collections::BTreeMap, fs};

use anyhow::{Context, Result, ensure};

/// A set of logical CPUs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuSet(Vec<usize>);

/// Physical cores given to the server when there are enough to split.
const SERVER_CORES: usize = 2;
/// Fewer physical cores than this are not split: the clients would get too few.
const MIN_CORES_TO_SPLIT: usize = 4;

/// The CPUs for the server and for its clients, or `None` when there are too few
/// cores to split.
pub(crate) fn split_for_server() -> Result<Option<(CpuSet, CpuSet)>> {
    let allowed = CpuSet::allowed()?;
    let cores = physical_cores(&allowed.0)?;
    if cores.len() < MIN_CORES_TO_SPLIT {
        return Ok(None);
    }
    Ok(Some(split(cores, SERVER_CORES)))
}

/// The first `server_cores` cores for the server, the rest for the clients.
fn split(cores: Vec<Vec<usize>>, server_cores: usize) -> (CpuSet, CpuSet) {
    let mut server = Vec::new();
    let mut clients = Vec::new();
    for (index, core) in cores.into_iter().enumerate() {
        if index < server_cores {
            server.extend(core);
        } else {
            clients.extend(core);
        }
    }
    server.sort_unstable();
    clients.sort_unstable();
    (CpuSet(server), CpuSet(clients))
}

/// The logical CPUs in `allowed`, grouped by physical core, cores in order of their
/// lowest CPU.
fn physical_cores(allowed: &[usize]) -> Result<Vec<Vec<usize>>> {
    let mut cores: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for &cpu in allowed {
        let topology = format!("/sys/devices/system/cpu/cpu{cpu}/topology");
        let read = |name: &str| {
            fs::read_to_string(format!("{topology}/{name}"))
                .map(|text| text.trim().to_owned())
                .with_context(|| format!("read {topology}/{name}"))
        };
        cores
            .entry((read("physical_package_id")?, read("core_id")?))
            .or_default()
            .push(cpu);
    }
    let mut cores: Vec<Vec<usize>> = cores.into_values().collect();
    cores.sort_unstable_by_key(|core| core[0]);
    Ok(cores)
}

impl CpuSet {
    /// The CPUs this thread may run on.
    fn allowed() -> Result<Self> {
        // SAFETY: an all-zero `cpu_set_t` is a valid empty set.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: `set` is a valid, exclusive `cpu_set_t` of the size passed.
        let result = unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) };
        ensure!(
            result == 0,
            "sched_getaffinity: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(
            (0..libc::CPU_SETSIZE as usize)
                // SAFETY: `cpu` is below `CPU_SETSIZE`, inside `set`.
                .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
                .collect(),
        ))
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Restricts the calling thread, and every process it starts from now on, to
    /// these CPUs.
    pub(crate) fn pin_current_thread(&self) -> Result<()> {
        // SAFETY: an all-zero `cpu_set_t` is a valid empty set.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        for &cpu in &self.0 {
            // SAFETY: every CPU in a `CpuSet` came from `allowed`, below `CPU_SETSIZE`.
            unsafe { libc::CPU_SET(cpu, &mut set) };
        }
        // SAFETY: `set` is a valid `cpu_set_t` of the size passed; 0 is this thread.
        let result = unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) };
        ensure!(
            result == 0,
            "sched_setaffinity: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    /// The CPUs as a list such as `0-1,12-13`.
    pub(crate) fn describe(&self) -> String {
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &cpu in &self.0 {
            match ranges.last_mut() {
                Some((_, end)) if *end + 1 == cpu => *end = cpu,
                _ => ranges.push((cpu, cpu)),
            }
        }
        ranges
            .iter()
            .map(|&(start, end)| {
                if start == end {
                    start.to_string()
                } else {
                    format!("{start}-{end}")
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_gets_whole_cores_with_their_siblings() {
        let cores = vec![vec![0, 4], vec![1, 5], vec![2, 6], vec![3, 7]];
        let (server, clients) = split(cores, 2);
        assert_eq!(server, CpuSet(vec![0, 1, 4, 5]));
        assert_eq!(clients, CpuSet(vec![2, 3, 6, 7]));
        assert_eq!(server.describe(), "0-1,4-5");
        assert_eq!(clients.describe(), "2-3,6-7");
    }

    #[test]
    fn this_thread_can_be_pinned_to_what_it_may_use() {
        let allowed = CpuSet::allowed().unwrap();
        assert!(allowed.len() > 0);
        allowed.pin_current_thread().unwrap();
        assert_eq!(CpuSet::allowed().unwrap(), allowed);
    }
}
