//! `add` over thousands of files in one directory (formerly the ignored
//! `add_scale::add_scale_benchmark` test; the 5,000-file test stays in the suite).
//!
//! One revision once could not hold more than 4,096 edits, and every per-path tree
//! lookup decoded the shared directory again, so `add`, and `validate` after it, grew
//! with the square of the file count. At 20,000 files each step must stay within
//! 30 s, which a quadratic step cannot meet.

use anyhow::{Result, ensure};
use serde_json::Value;

use super::{CaseContext, CaseOutput, Recorder, workspace::Workspace};

pub(crate) const NAME: &str = "add_scale";
/// Budget for one step. The quadratic steps this replaced took 65 s (`add` of new
/// files) and 200 s (`add` of modified files) at 5,000 files; `validate` took 127 s at
/// 20,000.
const STEP_BUDGET_SECONDS: f64 = 30.0;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let files = context.scale.pick(20_000, 300);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("files", files);

    let workspace = Workspace::new(context)?;
    let paths = (0..files)
        .map(|index| format!("src/f{index:06}.txt"))
        .collect::<Vec<_>>();
    let write_files = |version: &str| -> Result<()> {
        for (index, path) in paths.iter().enumerate() {
            workspace.write(path, format!("file {index}\nversion {version}\n"))?;
        }
        Ok(())
    };
    let step = |recorder: &mut Recorder, name: &str, arguments: &[&str]| -> Result<Value> {
        let captured = workspace.ok(arguments)?;
        recorder.timing(name, captured.timing);
        recorder.at_most(&format!("{name}.wall"), STEP_BUDGET_SECONDS, "s");
        Ok(if arguments.first() == Some(&"--format=json") {
            serde_json::from_slice(&captured.stdout)?
        } else {
            Value::Null
        })
    };
    let add = |message: &'static str| {
        let mut arguments = vec!["add", "--message", message];
        arguments.extend(paths.iter().map(String::as_str));
        arguments
    };

    workspace.ok(&["change", "new", "--message", "create"])?;
    write_files("one")?;
    step(&mut recorder, "add_new", &add("add new files"))?;
    settled(&step(
        &mut recorder,
        "status_new",
        &["--format=json", "status"],
    )?)?;

    workspace.ok(&["change", "new", "--message", "modify"])?;
    write_files("two")?;
    step(&mut recorder, "add_modified", &add("add modified files"))?;
    settled(&step(
        &mut recorder,
        "status_modified",
        &["--format=json", "status"],
    )?)?;

    step(&mut recorder, "validate", &["validate"])?;
    Ok(recorder.finish())
}

/// The workspace holds exactly the working view and nothing is left to add.
fn settled(status: &Value) -> Result<()> {
    let result = &status["result"];
    ensure!(
        result["changes"] == Value::Array(Vec::new()),
        "workspace is not clean: {status}"
    );
    ensure!(
        result["materialized_view"] == result["effective_view"],
        "add left materialization pending: {status}"
    );
    Ok(())
}
