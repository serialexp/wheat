//! Opt-in timing of the phases a command goes through.
//!
//! Wheat code marks its phases with `tracing` spans. Without a subscriber a span costs one
//! disabled-callsite check, so the spans stay in place permanently. Two environment variables
//! switch recording on for one process:
//!
//! - `WHEAT_TRACE=<file>` writes every closed span as a Chrome trace event file. Perfetto
//!   (<https://ui.perfetto.dev>, which runs in the browser without a server) and
//!   `chrome://tracing` open it as a per-thread timeline, with each span's fields as arguments.
//! - `WHEAT_TRACE_SUMMARY=1` prints a tree of the spans on stderr when the command ends: for
//!   each path of nested span names, how often it ran, its total time, and its own time (the
//!   total minus its child spans).
//!
//! Only spans from `wheat*` targets are recorded.

use std::{
    cell::Cell,
    collections::BTreeMap,
    fmt::Write as _,
    fs::File,
    io::{self, BufWriter, Write as _},
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::Serialize;
use tracing::{
    Subscriber,
    field::{Field, Visit},
    span,
};
use tracing_subscriber::{
    Layer,
    filter::filter_fn,
    layer::{Context, SubscriberExt as _},
    registry::LookupSpan,
};

/// Records spans until dropped, then writes the requested reports.
pub(crate) struct SpanTrace {
    recorded: Arc<Mutex<Recorded>>,
    trace_file: Option<PathBuf>,
    summary: bool,
}

/// Starts recording when `WHEAT_TRACE` or `WHEAT_TRACE_SUMMARY` asks for it.
pub(crate) fn install_from_env() -> Option<SpanTrace> {
    let trace_file = std::env::var_os("WHEAT_TRACE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let summary = std::env::var_os("WHEAT_TRACE_SUMMARY")
        .is_some_and(|value| !value.is_empty() && value != "0");
    if trace_file.is_none() && !summary {
        return None;
    }
    let (recorder, recorded) = Recorder::new(trace_file.is_some());
    let subscriber = tracing_subscriber::registry().with(recorder.layer());
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        eprintln!("warning: span tracing is unavailable: a tracing subscriber is already set");
        return None;
    }
    Some(SpanTrace {
        recorded,
        trace_file,
        summary,
    })
}

impl Drop for SpanTrace {
    fn drop(&mut self) {
        let recorded = self.recorded.lock().unwrap_or_else(PoisonError::into_inner);
        if self.summary {
            eprint!("{}", recorded.summary());
        }
        if let Some(path) = &self.trace_file
            && let Err(error) = recorded.write_trace(File::create(path))
        {
            eprintln!(
                "warning: cannot write span trace {}: {error}",
                path.display()
            );
        }
    }
}

/// The `tracing` layer that times spans.
#[derive(Clone)]
pub(crate) struct Recorder {
    recorded: Arc<Mutex<Recorded>>,
    keep_spans: bool,
}

impl Recorder {
    /// A recorder and the results it fills in. `keep_spans` keeps every span for the timeline
    /// on top of the summary.
    pub(crate) fn new(keep_spans: bool) -> (Self, Arc<Mutex<Recorded>>) {
        let recorded = Arc::new(Mutex::new(Recorded {
            origin: Instant::now(),
            spans: keep_spans.then(Vec::new),
            root: Node::default(),
        }));
        (
            Self {
                recorded: Arc::clone(&recorded),
                keep_spans,
            },
            recorded,
        )
    }

    /// The recorder, limited to Wheat's own spans.
    pub(crate) fn layer<S>(self) -> impl Layer<S>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        self.with_filter(filter_fn(|metadata| {
            metadata.is_span() && metadata.target().starts_with("wheat")
        }))
    }
}

/// Timing kept on a span while it is open.
struct Open {
    start: Instant,
    children: Duration,
    thread: u64,
    fields: Vec<(&'static str, String)>,
}

impl<S> Layer<S> for Recorder
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attributes: &span::Attributes<'_>,
        id: &span::Id,
        context: Context<'_, S>,
    ) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut fields = Vec::new();
        if self.keep_spans {
            attributes.record(&mut FieldText(&mut fields));
        }
        span.extensions_mut().insert(Open {
            start: Instant::now(),
            children: Duration::ZERO,
            thread: thread_number(),
            fields,
        });
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, context: Context<'_, S>) {
        if !self.keep_spans {
            return;
        }
        if let Some(span) = context.span(id)
            && let Some(open) = span.extensions_mut().get_mut::<Open>()
        {
            values.record(&mut FieldText(&mut open.fields));
        }
    }

    fn on_close(&self, id: span::Id, context: Context<'_, S>) {
        let end = Instant::now();
        let Some(span) = context.span(&id) else {
            return;
        };
        let Some(open) = span.extensions_mut().remove::<Open>() else {
            return;
        };
        let total = end.saturating_duration_since(open.start);
        if let Some(parent) = span.parent()
            && let Some(parent) = parent.extensions_mut().get_mut::<Open>()
        {
            parent.children += total;
        }
        let path = span
            .scope()
            .from_root()
            .map(|span| span.name())
            .collect::<Vec<_>>();
        let mut recorded = self.recorded.lock().unwrap_or_else(PoisonError::into_inner);
        recorded.add(&path, total, total.saturating_sub(open.children));
        let origin = recorded.origin;
        if let Some(spans) = &mut recorded.spans {
            spans.push(ClosedSpan {
                name: span.name(),
                target: span.metadata().target(),
                start: open.start.saturating_duration_since(origin),
                total,
                thread: open.thread,
                fields: open.fields,
            });
        }
    }
}

/// Collects a span's fields as text.
struct FieldText<'a>(&'a mut Vec<(&'static str, String)>);

impl Visit for FieldText<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name(), value.to_owned()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name(), format!("{value:?}")));
    }
}

/// A small, stable number for the current thread, for the timeline's rows.
fn thread_number() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    thread_local! {
        static NUMBER: Cell<u64> = const { Cell::new(0) };
    }
    NUMBER.with(|number| {
        if number.get() == 0 {
            number.set(NEXT.fetch_add(1, Ordering::Relaxed));
        }
        number.get()
    })
}

/// Everything recorded so far.
pub(crate) struct Recorded {
    origin: Instant,
    spans: Option<Vec<ClosedSpan>>,
    root: Node,
}

struct ClosedSpan {
    name: &'static str,
    target: &'static str,
    start: Duration,
    total: Duration,
    thread: u64,
    fields: Vec<(&'static str, String)>,
}

/// One path of nested span names in the summary.
#[derive(Default)]
struct Node {
    name: &'static str,
    count: u64,
    total: Duration,
    own: Duration,
    children: Vec<Node>,
}

impl Node {
    fn child(&mut self, name: &'static str) -> &mut Node {
        let index = match self.children.iter().position(|child| child.name == name) {
            Some(index) => index,
            None => {
                self.children.push(Node {
                    name,
                    ..Node::default()
                });
                self.children.len() - 1
            }
        };
        &mut self.children[index]
    }
}

/// One span in the Chrome trace event format, as a complete (`X`) event.
#[derive(Serialize)]
struct TraceEvent<'a> {
    name: &'a str,
    cat: &'a str,
    ph: &'static str,
    /// Microseconds since recording started.
    ts: f64,
    dur: f64,
    pid: u32,
    tid: u64,
    args: BTreeMap<&'a str, &'a str>,
}

#[derive(Serialize)]
struct TraceFile<'a> {
    #[serde(rename = "traceEvents")]
    trace_events: Vec<TraceEvent<'a>>,
    #[serde(rename = "displayTimeUnit")]
    display_time_unit: &'static str,
}

impl Recorded {
    fn add(&mut self, path: &[&'static str], total: Duration, own: Duration) {
        let mut node = &mut self.root;
        for name in path {
            node = node.child(name);
        }
        node.count += 1;
        node.total += total;
        node.own += own;
    }

    /// The span tree: count, total and own time per path of span names, in the order each
    /// path first finished.
    pub(crate) fn summary(&self) -> String {
        fn width(node: &Node, depth: usize) -> usize {
            node.children
                .iter()
                .map(|child| (2 * depth + child.name.len()).max(width(child, depth + 1)))
                .max()
                .unwrap_or(0)
        }
        fn lines(node: &Node, depth: usize, width: usize, out: &mut String) {
            for child in &node.children {
                let label = format!("{:indent$}{}", "", child.name, indent = 2 * depth);
                let _ = writeln!(
                    out,
                    "  {label:<width$}  {:>10.1}  {:>10.1}  {:>8}",
                    milliseconds(child.total),
                    milliseconds(child.own),
                    child.count,
                );
                lines(child, depth + 1, width, out);
            }
        }
        let width = width(&self.root, 0).max("span".len());
        let mut out = format!(
            "wheat span summary\n  {:<width$}  {:>10}  {:>10}  {:>8}\n",
            "span", "total ms", "own ms", "count"
        );
        lines(&self.root, 0, width, &mut out);
        out
    }

    /// Writes the Chrome trace event file.
    pub(crate) fn write_trace(&self, file: io::Result<File>) -> io::Result<()> {
        let mut out = BufWriter::new(file?);
        let trace_events = self
            .spans
            .iter()
            .flatten()
            .map(|span| TraceEvent {
                name: span.name,
                cat: span.target,
                ph: "X",
                ts: microseconds(span.start),
                dur: microseconds(span.total),
                pid: std::process::id(),
                tid: span.thread,
                args: span
                    .fields
                    .iter()
                    .map(|(name, value)| (*name, value.as_str()))
                    .collect(),
            })
            .collect();
        serde_json::to_writer(
            &mut out,
            &TraceFile {
                trace_events,
                display_time_unit: "ms",
            },
        )?;
        out.flush()
    }
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn microseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

#[cfg(test)]
mod tests;
