//! Many writers publishing unrelated changes to one server at once.
//!
//! Changes are independent, but a publish names the repository head it expects, and
//! the server takes only one publish per head. Every other writer that started from
//! the same head is turned away: either by the client's own check before it uploads
//! (`remote has advanced`) or by the server's compare-and-swap after the upload. It
//! then fetches, which keeps its own change beside the new remote work, and publishes
//! again. This case measures what that costs as writers are added.
//!
//! For each writer count a fresh server runs in this process, pinned to whole cores of
//! its own, and each writer is a loop of `wheat` processes on the remaining cores:
//! `change new`, then `publish` until it is accepted, with `fetch` after each refusal.
//! Every writer has its own clone and store home, as separate agents would.
//!
//! There is no budget yet. The planned server-side reconciliation (see
//! `docs/design/wheat-lessons-from-scion.md`) should bring attempts per publish close
//! to 1, which is when this case gets a contract rule.

use std::{path::PathBuf, sync::Barrier, time::Instant};

use anyhow::{Context, Result, bail};

use super::{
    CaseContext, CaseOutput, Recorder,
    cpus::{self, CpuSet},
    server::Server,
    workspace::Workspace,
};
use crate::{
    measure::{self, Captured},
    path_text,
};

pub(crate) const NAME: &str = "publish_contention";
/// A writer that needs more attempts than this for one change is starved; the case
/// fails rather than spin.
const MAX_ATTEMPTS: usize = 500;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let writer_counts: &[usize] = context.scale.pick(&[1, 2, 4, 8, 16], &[1, 2]);
    let publishes = context.scale.pick(20, 2);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("writers", writer_counts);
    recorder.parameter("publishes_per_writer", publishes);

    let split = cpus::split_for_server()?;
    match &split {
        Some((server, clients)) => {
            recorder.parameter("server_cpus", server.describe());
            recorder.parameter("client_cpus", clients.describe());
            clients.pin_current_thread()?;
        }
        None => recorder.parameter("server_cpus", "unpinned"),
    }

    for &writers in writer_counts {
        let level = Level::run(
            context,
            split.as_ref().map(|(server, _)| server),
            writers,
            publishes,
        )
        .with_context(|| format!("{writers} writers"))?;
        level.record(&mut recorder, writers);
    }
    Ok(recorder.finish())
}

/// One writer count's run.
struct Level {
    /// From releasing the writers to the last accepted publish.
    wall_seconds: f64,
    writers: Vec<WriterStats>,
    /// CPU time of this process while the writers ran, which is the server's.
    server_cpu_seconds: f64,
    server_cpus: usize,
}

#[derive(Default)]
struct WriterStats {
    /// Publish attempts for each accepted change.
    attempts: Vec<usize>,
    /// From the first publish attempt of a change to its acceptance, fetches included.
    latencies: Vec<f64>,
    /// Refused by the client's own check, before uploading anything.
    refused_before_upload: usize,
    /// Refused by the server's compare-and-swap, after the upload.
    refused_after_upload: usize,
    publish_walls: Vec<f64>,
    fetch_walls: Vec<f64>,
}

impl Level {
    fn run(
        context: &CaseContext,
        server_cpus: Option<&CpuSet>,
        writers: usize,
        publishes: usize,
    ) -> Result<Self> {
        let scratch = context.scratch.join(format!("writers-{writers}"));
        std::fs::create_dir_all(&scratch)?;
        let level_context = CaseContext {
            scale: context.scale,
            wheat: context.wheat.clone(),
            scratch,
        };
        let source = Workspace::new(&level_context)?;
        let server = match server_cpus {
            Some(cpus) => Server::start_pinned(source.root(), cpus)?,
            None => Server::start(source.root())?,
        };
        let clients = (0..writers)
            .map(|index| Client::clone_new(&source, &level_context, server.url(), index))
            .collect::<Result<Vec<_>>>()?;

        let ready = Barrier::new(writers + 1);
        let (stats, wall_seconds, server_cpu_seconds) = std::thread::scope(|scope| {
            let handles = clients
                .iter()
                .map(|client| {
                    let ready = &ready;
                    let source = &source;
                    scope.spawn(move || {
                        ready.wait();
                        client.write(source, publishes)
                    })
                })
                .collect::<Vec<_>>();
            let cpu_before = process_cpu_seconds();
            ready.wait();
            let started = Instant::now();
            let stats = handles
                .into_iter()
                .map(|handle| handle.join().expect("writer thread panicked"))
                .collect::<Result<Vec<_>>>();
            let wall = started.elapsed().as_secs_f64();
            (stats, wall, process_cpu_seconds() - cpu_before)
        });
        let writers_stats = stats?;
        server.stop()?;
        Ok(Self {
            wall_seconds,
            writers: writers_stats,
            server_cpu_seconds,
            server_cpus: server_cpus.map_or_else(available_cpus, CpuSet::len),
        })
    }

    fn record(&self, recorder: &mut Recorder, writers: usize) {
        let step = format!("writers_{writers}");
        let attempts: Vec<f64> = self
            .writers
            .iter()
            .flat_map(|writer| writer.attempts.iter().map(|&count| count as f64))
            .collect();
        let accepted = attempts.len() as f64;
        let latencies: Vec<f64> = self
            .writers
            .iter()
            .flat_map(|writer| writer.latencies.iter().copied())
            .collect();
        let sum =
            |field: fn(&WriterStats) -> usize| self.writers.iter().map(field).sum::<usize>() as f64;
        let walls = |field: fn(&WriterStats) -> &Vec<f64>| {
            self.writers
                .iter()
                .flat_map(|writer| field(writer).iter().copied())
                .collect::<Vec<_>>()
        };

        recorder.wall(&step, self.wall_seconds);
        recorder.value(
            &format!("{step}.accepted_per_second"),
            accepted / self.wall_seconds,
            "count",
        );
        recorder.value(
            &format!("{step}.attempts_per_publish"),
            attempts.iter().sum::<f64>() / accepted,
            "count",
        );
        recorder.value(
            &format!("{step}.attempts_max"),
            attempts.iter().copied().fold(0.0, f64::max),
            "count",
        );
        recorder.value(
            &format!("{step}.refused_before_upload_per_publish"),
            sum(|writer| writer.refused_before_upload) / accepted,
            "count",
        );
        recorder.value(
            &format!("{step}.refused_after_upload_per_publish"),
            sum(|writer| writer.refused_after_upload) / accepted,
            "count",
        );
        recorder.value(
            &format!("{step}.latency_p50"),
            percentile(latencies.clone(), 50.0),
            "s",
        );
        recorder.value(
            &format!("{step}.latency_p95"),
            percentile(latencies, 95.0),
            "s",
        );
        recorder.value(
            &format!("{step}.publish_command.wall"),
            measure::median(walls(|writer| &writer.publish_walls)),
            "s",
        );
        let fetches = walls(|writer| &writer.fetch_walls);
        if !fetches.is_empty() {
            recorder.value(
                &format!("{step}.fetch_command.wall"),
                measure::median(fetches),
                "s",
            );
        }
        recorder.value(
            &format!("{step}.server_cpu_per_publish"),
            self.server_cpu_seconds / accepted,
            "s",
        );
        recorder.value(
            &format!("{step}.server_busy"),
            100.0 * self.server_cpu_seconds / (self.wall_seconds * self.server_cpus as f64),
            "percent",
        );
    }
}

/// A writer: a clone of the server with a store home of its own.
struct Client {
    index: usize,
    root: PathBuf,
    stores: PathBuf,
}

/// How one `wheat publish` ended.
enum Publish {
    Accepted,
    RefusedBeforeUpload,
    RefusedAfterUpload,
}

impl Client {
    fn clone_new(
        source: &Workspace,
        context: &CaseContext,
        url: &str,
        index: usize,
    ) -> Result<Self> {
        let client = Self {
            index,
            root: context.scratch.join(format!("writer-{index}")),
            stores: context.scratch.join(format!("writer-{index}-stores")),
        };
        source.ok_in_stores(
            &context.scratch,
            &client.stores,
            &["clone", url, path_text(&client.root)?],
        )?;
        Ok(client)
    }

    /// Makes and publishes `publishes` one-file changes, fetching after each refusal.
    fn write(&self, source: &Workspace, publishes: usize) -> Result<WriterStats> {
        let mut stats = WriterStats::default();
        for change in 0..publishes {
            let path = format!("writer-{}/change-{change}.txt", self.index);
            let value = format!("writer {} change {change}\n", self.index);
            let message = format!("writer {} change {change}", self.index);
            source.ok_in_stores(
                &self.root,
                &self.stores,
                &[
                    "change",
                    "new",
                    "--path",
                    &path,
                    "--value",
                    &value,
                    "--message",
                    &message,
                ],
            )?;
            let started = Instant::now();
            let mut attempts = 0;
            loop {
                attempts += 1;
                if attempts > MAX_ATTEMPTS {
                    bail!(
                        "writer {} needed more than {MAX_ATTEMPTS} attempts for change {change}",
                        self.index
                    );
                }
                let (outcome, wall) = self.publish(source)?;
                stats.publish_walls.push(wall);
                match outcome {
                    Publish::Accepted => break,
                    Publish::RefusedBeforeUpload => stats.refused_before_upload += 1,
                    Publish::RefusedAfterUpload => stats.refused_after_upload += 1,
                }
                let fetched = source.ok_in_stores(&self.root, &self.stores, &["fetch"])?;
                stats.fetch_walls.push(fetched.timing.wall_seconds);
            }
            stats.attempts.push(attempts);
            stats.latencies.push(started.elapsed().as_secs_f64());
        }
        Ok(stats)
    }

    fn publish(&self, source: &Workspace) -> Result<(Publish, f64)> {
        let captured = source.run_in_stores(&self.root, &self.stores, &["publish"])?;
        let wall = captured.timing.wall_seconds;
        Ok((classify(&captured)?, wall))
    }
}

/// Sorts a refused publish by where it was refused; anything else is an error.
fn classify(captured: &Captured) -> Result<Publish> {
    if captured.status.success() {
        return Ok(Publish::Accepted);
    }
    let stderr = String::from_utf8_lossy(&captured.stderr);
    if stderr.contains("remote has advanced") {
        Ok(Publish::RefusedBeforeUpload)
    } else if stderr.contains("publish rejected: remote is at generation") {
        Ok(Publish::RefusedAfterUpload)
    } else {
        captured.ensure_success("wheat publish")?;
        unreachable!("ensure_success fails for an unsuccessful command")
    }
}

/// User and system CPU time of this whole process so far.
fn process_cpu_seconds() -> f64 {
    // SAFETY: an all-zero `rusage` is a valid value; `getrusage` overwrites it.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a valid, exclusive out-pointer for this call.
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(result, 0, "getrusage(RUSAGE_SELF) cannot fail");
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

fn available_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

/// The `percent`th percentile of `values`, nearest rank.
fn percentile(mut values: Vec<f64>, percent: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let rank = ((percent / 100.0) * values.len() as f64).ceil() as usize;
    values[rank.clamp(1, values.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let values: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(percentile(values.clone(), 50.0), 10.0);
        assert_eq!(percentile(values.clone(), 95.0), 19.0);
        assert_eq!(percentile(values, 100.0), 20.0);
        assert_eq!(percentile(vec![3.0], 95.0), 3.0);
        assert_eq!(percentile(Vec::new(), 50.0), 0.0);
    }
}
