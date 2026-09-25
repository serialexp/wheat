use tracing_subscriber::layer::SubscriberExt as _;

use super::*;

/// Runs `work` with a recorder installed on this thread and returns what it recorded.
fn record(keep_spans: bool, work: impl FnOnce()) -> Arc<Mutex<Recorded>> {
    let (recorder, recorded) = Recorder::new(keep_spans);
    let subscriber = tracing_subscriber::registry().with(recorder.layer());
    tracing::subscriber::with_default(subscriber, work);
    recorded
}

fn nested_work() {
    let _outer = tracing::info_span!(target: "wheat_test", "outer", path = "big").entered();
    for n in 0..2 {
        let _inner = tracing::debug_span!(target: "wheat_test", "inner", n).entered();
        // A span from another crate is not recorded; Wheat's spans inside it still nest
        // under the Wheat span around it.
        let _foreign = tracing::info_span!(target: "other_crate", "foreign").entered();
        let _leaf = tracing::info_span!(target: "wheat_test", "leaf").entered();
    }
}

#[test]
fn the_summary_nests_spans_and_splits_own_time_from_children() {
    let recorded = record(false, nested_work);
    let recorded = recorded.lock().unwrap();
    let [outer] = recorded.root.children.as_slice() else {
        panic!("one top-level span")
    };
    assert_eq!((outer.name, outer.count), ("outer", 1));
    let [inner] = outer.children.as_slice() else {
        panic!("one child path")
    };
    assert_eq!((inner.name, inner.count), ("inner", 2));
    let [leaf] = inner.children.as_slice() else {
        panic!("the foreign span is skipped")
    };
    assert_eq!((leaf.name, leaf.count), ("leaf", 2));

    assert!(outer.total >= inner.total && inner.total >= leaf.total);
    assert_eq!(outer.own, outer.total - inner.total);
    assert!(inner.own <= inner.total - leaf.total);
    assert!(recorded.spans.is_none(), "the summary alone keeps no spans");

    let summary = recorded.summary();
    let names = summary
        .lines()
        .skip(2)
        .map(|line| {
            line.split("  ")
                .find(|part| !part.is_empty())
                .unwrap()
                .trim()
        })
        .collect::<Vec<_>>();
    assert_eq!(names, ["outer", "inner", "leaf"], "{summary}");
    assert!(
        summary.lines().nth(3).unwrap().starts_with("    inner"),
        "{summary}"
    );
    assert!(
        summary.lines().nth(3).unwrap().trim_end().ends_with(" 2"),
        "{summary}"
    );
}

#[test]
fn the_trace_file_holds_every_span_with_its_fields() {
    let recorded = record(true, nested_work);
    let file = tempfile::NamedTempFile::new().unwrap();
    recorded.lock().unwrap().write_trace(file.reopen()).unwrap();
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
    let events = document["traceEvents"].as_array().unwrap();
    let named = |name: &str| {
        events
            .iter()
            .filter(|event| event["name"] == name)
            .collect::<Vec<_>>()
    };
    assert_eq!(events.len(), 5, "{document}");
    assert!(named("foreign").is_empty());

    let [outer] = named("outer")[..] else {
        panic!("one outer span")
    };
    assert_eq!(outer["ph"], "X");
    assert_eq!(outer["cat"], "wheat_test");
    assert_eq!(outer["args"]["path"], "big");
    let (start, end) = (
        outer["ts"].as_f64().unwrap(),
        outer["ts"].as_f64().unwrap() + outer["dur"].as_f64().unwrap(),
    );
    let inner = named("inner");
    assert_eq!(inner.len(), 2);
    for (n, event) in inner.iter().enumerate() {
        assert_eq!(event["args"]["n"], n.to_string());
        assert_eq!(event["tid"], outer["tid"]);
        let at = event["ts"].as_f64().unwrap();
        assert!(start <= at && at + event["dur"].as_f64().unwrap() <= end);
    }
}
