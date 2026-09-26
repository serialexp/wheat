//! Fetching one change, early and late in a long history.
//!
//! Fetch once sent the whole repository every time. Now the client names the
//! envelope it last fetched, and the server walks from there to its head
//! ([`StructuralDelta`]) and pushes only what that walk finds. A writer publishes
//! `changes` one-file changes to a server in this process. A reader that cloned after
//! `early` changes fetches one more, and so does a reader that cloned after all of
//! them: neither the push nor the walk's store reads for that one change may grow
//! with the history. The early reader then catches up on everything in between, and
//! its store must validate.

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use wheat_core::RepositoryEnvelopeId;
use wheat_store::{
    DeltaStep, PackedObjectStore, StructuralDelta, StructuralDeltaLimits, StructuralDeltaStats,
};

use super::{CaseContext, CaseOutput, Recorder, server::Server, workspace::Workspace};
use crate::{measure::Timing, path_text};

pub(crate) const NAME: &str = "fetch_incremental";
/// Budget for one change late in the history, as a percentage of the same for one
/// change early: the objects pushed and the walk's store reads. Measured on
/// 2026-09-26 (release) for changes 11 and 201: 20 → 26 objects and 33 → 43 reads,
/// 130%, as the history's collections grow deeper. A walk that grew with the history
/// itself would be near 2,000%. The bytes pushed grow faster (6 → 44 KiB); they are
/// recorded, without a budget until the wire sends deltas.
const ONE_CHANGE_GROWTH_BUDGET_PERCENT: f64 = 150.0;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let changes = context.scale.pick(200, 6);
    let early = context.scale.pick(10, 2);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("changes", changes);
    recorder.parameter("early", early);

    let source = Workspace::new(context)?;
    let server = Server::start(source.root())?;
    let url = server.url().to_owned();
    let clients = Clients {
        source: &source,
        context,
        url: &url,
    };
    let mut history = History {
        writer: clients.clone_new("writer")?.0,
        envelopes: Vec::new(),
        publishes: Vec::new(),
    };

    history.publish_until(&clients, early)?;
    let (early_reader, _) = clients.clone_new("early-reader")?;
    history.publish_until(&clients, early + 1)?;
    let fetch_early = clients.fetch(&early_reader)?;

    history.publish_until(&clients, changes)?;
    let (late_reader, clone) = clients.clone_new("late-reader")?;
    recorder.timing("clone", clone.timing);
    recorder.value("clone.objects", number(&clone.result, "objects")?, "count");
    history.publish_until(&clients, changes + 1)?;
    let fetch_late = clients.fetch(&late_reader)?;

    let catch_up = clients.fetch(&early_reader)?;
    clients.run(&early_reader, &["validate"])?;
    recorder.runs("publish", &history.publishes);

    // The walks the server ran for those fetches, now counted on its store.
    let envelopes = history.envelopes;
    let store = server.stop()?;
    let objects = PackedObjectStore::open(store.join("storage"))?;
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    let envelope = |change: usize| envelopes[change - 1];
    let walk_early = runtime.block_on(walk(&objects, envelope(early), envelope(early + 1)))?;
    let walk_late = runtime.block_on(walk(&objects, envelope(changes), envelope(changes + 1)))?;
    let walk_catch_up =
        runtime.block_on(walk(&objects, envelope(early + 1), envelope(changes + 1)))?;
    for (name, fetched, walked) in [
        ("early", &fetch_early, &walk_early),
        ("late", &fetch_late, &walk_late),
        ("catch-up", &catch_up, &walk_catch_up),
    ] {
        ensure!(
            number(&fetched.result, "pushed")? == walked.emitted as f64,
            "the {name} fetch pushed {} objects but the walk finds {}",
            fetched.result["pushed"],
            walked.emitted
        );
    }

    let caught_up = (changes - early) as f64;
    for (step, fetched, walked, per) in [
        ("fetch_early", &fetch_early, &walk_early, 1.0),
        ("fetch_late", &fetch_late, &walk_late, 1.0),
        ("catch_up", &catch_up, &walk_catch_up, caught_up),
    ] {
        recorder.timing(step, fetched.timing);
        recorder.value(
            &format!("{step}.pushed_per_change"),
            walked.emitted as f64 / per,
            "count",
        );
        recorder.value(
            &format!("{step}.pushed_bytes_per_change"),
            walked.emitted_bytes as f64 / per,
            "bytes",
        );
        recorder.value(
            &format!("{step}.reads_per_change"),
            reads(walked) / per,
            "count",
        );
    }
    recorder.value(
        "one_change.pushed_growth",
        percent(walk_late.emitted as f64, walk_early.emitted as f64),
        "percent",
    );
    recorder.value(
        "one_change.read_growth",
        percent(reads(&walk_late), reads(&walk_early)),
        "percent",
    );
    recorder.at_most(
        "one_change.pushed_growth",
        ONE_CHANGE_GROWTH_BUDGET_PERCENT,
        "percent",
    );
    recorder.at_most(
        "one_change.read_growth",
        ONE_CHANGE_GROWTH_BUDGET_PERCENT,
        "percent",
    );
    Ok(recorder.finish())
}

/// A workspace cloned from the server, with a store home of its own: clones of one
/// remote into one store home share a store, and these must not.
struct Client {
    root: PathBuf,
    stores: PathBuf,
}

/// Runs `wheat` for the case's clients of one server.
struct Clients<'a> {
    source: &'a Workspace,
    context: &'a CaseContext,
    url: &'a str,
}

impl Clients<'_> {
    /// Clones the server into a new client named `name`.
    fn clone_new(&self, name: &str) -> Result<(Client, Reported)> {
        let client = Client {
            root: self.context.scratch.join(name),
            stores: self.context.scratch.join(format!("{name}-stores")),
        };
        let cloned = self.json(
            &self.context.scratch,
            &client,
            &["clone", self.url, path_text(&client.root)?],
        )?;
        Ok((client, cloned))
    }

    /// Runs `wheat --format json <arguments>` in `client` and returns its `result`.
    fn run(&self, client: &Client, arguments: &[&str]) -> Result<Reported> {
        self.json(&client.root, client, arguments)
    }

    /// Fetches into `client`, which must get everything in the push.
    fn fetch(&self, client: &Client) -> Result<Reported> {
        let fetched = self.run(client, &["fetch"])?;
        ensure!(
            fetched.result["incremental"] == true && fetched.result["pulled"] == 0,
            "expected an incremental fetch with nothing pulled, got {}",
            fetched.result
        );
        Ok(fetched)
    }

    fn json(
        &self,
        directory: &std::path::Path,
        client: &Client,
        arguments: &[&str],
    ) -> Result<Reported> {
        let arguments: Vec<&str> = ["--format", "json"]
            .iter()
            .chain(arguments)
            .copied()
            .collect();
        let captured = self
            .source
            .ok_in_stores(directory, &client.stores, &arguments)?;
        let document: Value = serde_json::from_slice(&captured.stdout)?;
        Ok(Reported {
            timing: captured.timing,
            result: document["result"].clone(),
        })
    }
}

/// The writer's published history: `envelopes[n - 1]` is the envelope after change
/// `n`.
struct History {
    writer: Client,
    envelopes: Vec<RepositoryEnvelopeId>,
    publishes: Vec<Timing>,
}

impl History {
    /// Adds and publishes one-file changes until there are `changes` of them.
    fn publish_until(&mut self, clients: &Clients<'_>, changes: usize) -> Result<()> {
        while self.envelopes.len() < changes {
            let change = self.envelopes.len() + 1;
            let path = format!("dir{}/file{change}.txt", change % 16);
            let value = format!("change {change}\n");
            clients.run(
                &self.writer,
                &[
                    "change",
                    "new",
                    "--path",
                    &path,
                    "--value",
                    &value,
                    "--message",
                    &format!("change {change}"),
                ],
            )?;
            let published = clients.run(&self.writer, &["publish"])?;
            self.publishes.push(published.timing);
            let envelope = published.result["envelope"]
                .as_str()
                .context("publish reported no envelope")?;
            self.envelopes.push(envelope.parse()?);
        }
        Ok(())
    }
}

/// A command's timing and its JSON `result`.
struct Reported {
    timing: Timing,
    result: Value,
}

fn number(result: &Value, field: &str) -> Result<f64> {
    result[field]
        .as_f64()
        .with_context(|| format!("no number {field:?} in {result}"))
}

/// Runs the walk from `old` to `new` over a store that holds both.
async fn walk(
    objects: &PackedObjectStore,
    old: RepositoryEnvelopeId,
    new: RepositoryEnvelopeId,
) -> Result<StructuralDeltaStats> {
    let mut delta = StructuralDelta::new(objects, Some(old), new, StructuralDeltaLimits::default());
    while let Some(step) = delta.next().await? {
        if let DeltaStep::Missing(ids) = step {
            bail!("the server's store lacks {} objects of {new}", ids.len());
        }
    }
    Ok(delta.stats().clone())
}

fn reads(stats: &StructuralDeltaStats) -> f64 {
    (stats.new_reads + stats.old_reads) as f64
}

fn percent(late: f64, early: f64) -> f64 {
    100.0 * late / early.max(1.0)
}
