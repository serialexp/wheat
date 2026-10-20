//! Publishing, fetching and cloning over a link with a real round-trip time.
//!
//! On loopback a round trip costs next to nothing, so a command that talks back and
//! forth once per tree level looks as fast as one that sends everything in one go.
//! Here every command reaches the in-process server through [`LatencyProxy`], which
//! adds [`RTT`] to every round trip and counts how many the command needed. Pushing
//! the difference with the request took the small publish from 19 round trips to 6
//! (2026-10-19).
//!
//! A writer publishes changes that each add one file five directories deep, so a walk
//! crosses several tree levels: in the `small` step a file of a few bytes, and in the
//! `large` step one of [`LARGE_MIB`] MiB, above the size a client pushes without being
//! asked. After each publish a reader fetches it. At the end a new workspace clones
//! the repository.

use std::{path::PathBuf, time::Duration};

use anyhow::Result;

use super::{
    CaseContext, CaseOutput, Recorder,
    latency_proxy::{LatencyProxy, Traffic},
    publish_content::write_noise,
    server::{self, Server},
    workspace::Workspace,
};
use crate::{
    measure::{self, Captured},
    path_text,
};

pub(crate) const NAME: &str = "remote_latency";
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
    let url = server::url(proxy.addr());

    // Each client has a store home of its own: one shared with the server, or with
    // another client, would hold the objects before they cross the link.
    let (writer, _) = Client::clone(context, &source, &url, "writer")?;
    let (reader, _) = Client::clone(context, &source, &url, "reader")?;
    let change = |path: &str, value: &str| {
        writer.ok(
            &source,
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
    writer.ok(&source, &["publish"])?;
    reader.ok(&source, &["fetch"])?;
    proxy.take();

    let mut publish = Commands::default();
    let mut fetch = Commands::default();
    for index in 0..publishes {
        change(
            &format!("{DIRECTORY}/small-{index}.txt"),
            &format!("small {index}\n"),
        )?;
        publish.add(writer.ok(&source, &["publish"])?, &proxy);
        fetch.add(reader.ok(&source, &["fetch"])?, &proxy);
    }
    publish.record(&mut recorder, "small.publish");
    fetch.record(&mut recorder, "small.fetch");

    let mut publish = Commands::default();
    let mut fetch = Commands::default();
    for index in 0..publishes {
        let path = format!("{DIRECTORY}/large-{index}.bin");
        writer.ok(&source, &["change", "new", "--message", &path])?;
        write_noise(&writer.root.join(&path), index, LARGE_MIB * MIB)?;
        writer.ok(&source, &["add", &path])?;
        publish.add(writer.ok(&source, &["publish"])?, &proxy);
        fetch.add(reader.ok(&source, &["fetch"])?, &proxy);
    }
    publish.record(&mut recorder, "large.publish");
    fetch.record(&mut recorder, "large.fetch");

    let mut clone = Commands::default();
    let (_, cloned) = Client::clone(context, &source, &url, "cloned")?;
    clone.add(cloned, &proxy);
    clone.record(&mut recorder, "clone");

    // Opening the session takes three round trips (the WebSocket upgrade,
    // `ClientHello`, `RepositoryOpen`) and closing it one. The publish pushes its
    // difference with the request, so the walk needs no rounds: one trip carries the
    // push, one the commit. A blob over the push threshold adds its content round.
    recorder.at_most("small.publish.round_trips", 6.0, "count");
    recorder.at_most("large.publish.round_trips", 7.0, "count");
    // A fetch or a clone takes the server's push with one trip and its content with
    // another.
    for step in ["small.fetch", "large.fetch", "clone"] {
        recorder.at_most(&format!("{step}.round_trips"), 6.0, "count");
    }

    proxy.stop()?;
    server.stop()?;
    Ok(recorder.finish())
}

/// A workspace cloned through the proxy, with a store home of its own.
struct Client {
    root: PathBuf,
    stores: PathBuf,
}

impl Client {
    /// Clones `url` into the workspace `name`, and returns it and the measured clone.
    fn clone(
        context: &CaseContext,
        source: &Workspace,
        url: &str,
        name: &str,
    ) -> Result<(Self, Captured)> {
        let client = Self {
            root: context.scratch.join(name),
            stores: context.scratch.join(format!("{name}-stores")),
        };
        let cloned = source.ok_in_stores(
            &context.scratch,
            &client.stores,
            &["clone", url, path_text(&client.root)?],
        )?;
        Ok((client, cloned))
    }

    fn ok(&self, source: &Workspace, arguments: &[&str]) -> Result<Captured> {
        source.ok_in_stores(&self.root, &self.stores, arguments)
    }
}

/// Runs of one command, each measured on its own.
#[derive(Default)]
struct Commands {
    walls: Vec<f64>,
    traffic: Vec<Traffic>,
}

impl Commands {
    /// Adds a run that just finished, with the traffic it sent through `proxy`.
    fn add(&mut self, ran: Captured, proxy: &LatencyProxy) {
        self.walls.push(ran.timing.wall_seconds);
        self.traffic.push(proxy.take());
    }

    fn record(&self, recorder: &mut Recorder, step: &str) {
        let most =
            |field: fn(&Traffic) -> u64| self.traffic.iter().map(field).max().unwrap_or(0) as f64;
        recorder.value(
            &format!("{step}.wall"),
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
