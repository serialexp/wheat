//! Publishing one change over a link with a real round-trip time.
//!
//! On loopback a round trip costs next to nothing, so a publish that talks back and
//! forth once per tree level looks as fast as one that sends everything in one go.
//! Here the writer reaches the in-process server through [`LatencyProxy`], which adds
//! [`RTT`] to every round trip and counts how many the command needed. Pushing the
//! difference with the request took the small publish from 19 round trips to 6
//! (2026-10-19).
//!
//! Each publish adds one file five directories deep, so the server's walk crosses
//! several tree levels: in the `small` step a file of a few bytes, and in the `large`
//! step one of [`LARGE_MIB`] MiB, above the size a client pushes without being asked.

use std::time::Duration;

use anyhow::Result;

use super::{
    CaseContext, CaseOutput, Recorder,
    latency_proxy::{LatencyProxy, Traffic},
    publish_content::write_noise,
    server::{self, Server},
    workspace::Workspace,
};
use crate::{measure, path_text};

pub(crate) const NAME: &str = "publish_latency";
/// A round trip between two hosts in one region.
const RTT: Duration = Duration::from_millis(30);
const LARGE_MIB: usize = 1;
const MIB: usize = 1024 * 1024;
const DIRECTORY: &str = "deep/a/b/c/d";

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let publishes = context.scale.pick(10, 2);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("rtt_ms", RTT.as_millis());
    recorder.parameter("publishes", publishes);
    recorder.parameter("large_mib", LARGE_MIB);

    let source = Workspace::new(context)?;
    let server = Server::start(source.root())?;
    let proxy = LatencyProxy::start(server.addr(), RTT)?;

    let writer = context.scratch.join("writer");
    source.ok_in(
        &context.scratch,
        &["clone", &server::url(proxy.addr()), path_text(&writer)?],
    )?;
    let change = |path: &str, value: &str| {
        source.ok_in(
            &writer,
            &[
                "change",
                "new",
                "--path",
                path,
                "--value",
                value,
                "--message",
                path,
            ],
        )
    };
    // The first publish makes the directories, so later ones change every level.
    change(&format!("{DIRECTORY}/seed.txt"), "seed\n")?;
    source.ok_in(&writer, &["publish"])?;
    proxy.take();

    let mut small = Step::default();
    for index in 0..publishes {
        change(
            &format!("{DIRECTORY}/small-{index}.txt"),
            &format!("small {index}\n"),
        )?;
        small.publish(&source, &writer, &proxy)?;
    }
    small.record(&mut recorder, "small");

    let mut large = Step::default();
    for index in 0..publishes {
        let path = format!("{DIRECTORY}/large-{index}.bin");
        source.ok_in(&writer, &["change", "new", "--message", &path])?;
        write_noise(&writer.join(&path), index, LARGE_MIB * MIB)?;
        source.ok_in(&writer, &["add", &path])?;
        large.publish(&source, &writer, &proxy)?;
    }
    large.record(&mut recorder, "large");
    // Opening the session takes three round trips (the WebSocket upgrade,
    // `ClientHello`, `RepositoryOpen`) and closing it one. The publish pushes its
    // difference with the request, so the walk needs no rounds: one trip carries the
    // push, one the commit. A blob over the push threshold adds its content round.
    recorder.at_most("small.round_trips", 6.0, "count");
    recorder.at_most("large.round_trips", 7.0, "count");

    proxy.stop()?;
    server.stop()?;
    Ok(recorder.finish())
}

/// The publishes of one step.
#[derive(Default)]
struct Step {
    walls: Vec<f64>,
    traffic: Vec<Traffic>,
}

impl Step {
    fn publish(
        &mut self,
        source: &Workspace,
        writer: &std::path::Path,
        proxy: &LatencyProxy,
    ) -> Result<()> {
        let published = source.ok_in(writer, &["publish"])?;
        self.walls.push(published.timing.wall_seconds);
        self.traffic.push(proxy.take());
        Ok(())
    }

    fn record(&self, recorder: &mut Recorder, step: &str) {
        let most =
            |field: fn(&Traffic) -> u64| self.traffic.iter().map(field).max().unwrap_or(0) as f64;
        recorder.value(
            &format!("{step}.publish.wall"),
            measure::median(self.walls.clone()),
            "s",
        );
        recorder.value(
            &format!("{step}.round_trips"),
            most(|traffic| traffic.turns),
            "count",
        );
        recorder.value(
            &format!("{step}.client_bytes"),
            most(|traffic| traffic.client_bytes),
            "bytes",
        );
        recorder.value(
            &format!("{step}.server_bytes"),
            most(|traffic| traffic.server_bytes),
            "bytes",
        );
    }
}
