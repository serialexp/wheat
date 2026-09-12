use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    process::{Child, Command, Stdio},
};

use anyhow::{Context, Result, ensure};

const MAX_GIT_OUTPUT_BYTES: usize = 8 * 1024 * 1024 + 64 * 1024;
const MAX_GIT_LINE_BYTES: usize = 64 * 1024;

pub(crate) struct SelectedPaths {
    pub(crate) readme: String,
    pub(crate) regular: String,
    pub(crate) executable: String,
}

pub(crate) fn source_inventory(source: &Path) -> Result<(String, bool, u64, u64, u64, u64)> {
    ensure!(
        git(source, &["rev-parse", "--is-shallow-repository"])? == "false",
        "source checkout is shallow; fetch full ancestry first"
    );
    let corpus_sha = git(source, &["rev-parse", "HEAD"])?;
    let tested_worktree_dirty = !git(source, &["status", "--porcelain"])?.is_empty();
    let reachable_commits = git(source, &["rev-list", "--count", "HEAD"])?.parse()?;
    let tracked_files = count_git_lines(source, &["ls-tree", "-r", "--name-only", "HEAD"])?;
    let (git_objects, git_bytes) = reachable_git_size(source)?;
    Ok((
        corpus_sha,
        tested_worktree_dirty,
        reachable_commits,
        tracked_files,
        git_objects,
        git_bytes,
    ))
}

pub(crate) fn select_paths(source: &Path) -> Result<SelectedPaths> {
    ensure!(
        tree_mode(source, "README.md")?.as_deref() == Some("100644"),
        "README.md is not a regular file in source HEAD"
    );
    ensure!(
        tree_mode(source, "Cargo.toml")?.as_deref() == Some("100644"),
        "Cargo.toml is not a regular file in source HEAD"
    );
    let mut child = git_stream(source, &["ls-tree", "-r", "HEAD"])?;
    let stdout = child
        .child_mut()?
        .stdout
        .take()
        .context("git ls-tree stdout unavailable")?;
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::with_capacity(256);
    let mut executable = None;
    while read_bounded_line(&mut reader, &mut line, MAX_GIT_LINE_BYTES)? {
        let line_text = std::str::from_utf8(trim_line(&line))?;
        let (metadata, path) = line_text
            .split_once('\t')
            .context("unexpected ls-tree output")?;
        if executable.is_none() && metadata.split_whitespace().next() == Some("100755") {
            executable = Some(path.to_owned());
        }
    }
    child.ensure_ok("git ls-tree")?;
    let executable = executable.context("source HEAD has no executable file")?;
    ensure!(
        tree_mode(source, &executable)?.as_deref() == Some("100755"),
        "selected executable mode changed during inventory"
    );
    Ok(SelectedPaths {
        readme: "README.md".into(),
        regular: "Cargo.toml".into(),
        executable,
    })
}

fn tree_mode(source: &Path, path: &str) -> Result<Option<String>> {
    let output = git(source, &["ls-tree", "HEAD", "--", path])?;
    Ok(output
        .split_once('\t')
        .and_then(|(metadata, actual)| (actual == path).then_some(metadata))
        .and_then(|metadata| metadata.split_whitespace().next())
        .map(str::to_owned))
}

pub(crate) fn create_exact_fixture(source: &Path, fixture: &Path, head: &str) -> Result<()> {
    let mut command = Command::new("git");
    command
        .args(["clone", "--bare", "--no-local"])
        .arg(source)
        .arg(fixture)
        .env("GIT_TERMINAL_PROMPT", "0");
    run_command_ok("git clone fixture", &mut command)?;
    let refs = git(fixture, &["for-each-ref", "--format=%(refname)"])?;
    for reference in refs
        .lines()
        .filter(|reference| *reference != "refs/heads/main")
    {
        git(fixture, &["update-ref", "-d", reference])?;
    }
    git(fixture, &["update-ref", "refs/heads/main", head])?;
    git(fixture, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    ensure!(
        git(fixture, &["rev-parse", "refs/heads/main"])? == head,
        "fixture tip mismatch"
    );
    ensure!(
        git(fixture, &["rev-list", "--count", "refs/heads/main"])?
            == git(source, &["rev-list", "--count", "HEAD"])?,
        "fixture ancestry mismatch"
    );
    Ok(())
}

pub(crate) fn git(directory: &Path, arguments: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = run_command("git", &mut command)?;
    Ok(String::from_utf8(output)?.trim().to_owned())
}

pub(crate) fn git_init_bare(path: &Path) -> Result<()> {
    let mut command = Command::new("git");
    command.args(["init", "--bare"]).arg(path);
    run_command_ok("git init --bare", &mut command)
}

pub(crate) fn git_clone(remote: &Path, destination: &Path) -> Result<()> {
    let mut command = Command::new("git");
    command
        .args(["clone", "--branch", "main"])
        .arg(remote)
        .arg(destination)
        .env("GIT_TERMINAL_PROMPT", "0");
    run_command_ok("git clone validation", &mut command)
}

fn run_command_ok(name: &str, command: &mut Command) -> Result<()> {
    run_command(name, command).map(|_| ())
}

fn run_command(name: &str, command: &mut Command) -> Result<Vec<u8>> {
    // Files let the child finish without pipe backpressure. Only bounded prefixes
    // are ever brought back into profile memory.
    let stdout_file = tempfile::tempfile()?;
    let stderr_file = tempfile::tempfile()?;
    let status = command
        .stdout(Stdio::from(stdout_file.try_clone()?))
        .stderr(Stdio::from(stderr_file.try_clone()?))
        .status()?;
    let stdout = read_bounded(stdout_file, MAX_GIT_OUTPUT_BYTES)
        .with_context(|| format!("read {name} stdout"))?;
    ensure_bounded_lines(&stdout, MAX_GIT_LINE_BYTES)
        .with_context(|| format!("validate {name} stdout"))?;
    let stderr = read_bounded(stderr_file, MAX_GIT_OUTPUT_BYTES)
        .with_context(|| format!("read {name} stderr"))?;
    ensure_bounded_lines(&stderr, MAX_GIT_LINE_BYTES)
        .with_context(|| format!("validate {name} stderr"))?;
    ensure!(
        status.success(),
        "{name} failed with {status}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    Ok(stdout)
}

struct GitStream {
    child: Option<Child>,
    stderr: File,
}

impl GitStream {
    fn child_mut(&mut self) -> Result<&mut Child> {
        self.child.as_mut().context("Git child already waited")
    }

    fn kill_and_wait(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn ensure_ok(mut self, name: &str) -> Result<()> {
        let status = self.child_mut()?.wait()?;
        self.child.take();
        let stderr = read_bounded(self.stderr.try_clone()?, MAX_GIT_OUTPUT_BYTES)?;
        ensure_bounded_lines(&stderr, MAX_GIT_LINE_BYTES)?;
        ensure!(
            status.success(),
            "{name} failed with {status}\nstderr:\n{}",
            String::from_utf8_lossy(&stderr)
        );
        Ok(())
    }
}

impl Drop for GitStream {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

fn command_stream(command: &mut Command) -> Result<GitStream> {
    let stderr = tempfile::tempfile()?;
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr.try_clone()?))
        .spawn()?;
    Ok(GitStream {
        child: Some(child),
        stderr,
    })
}

fn git_stream(directory: &Path, arguments: &[&str]) -> Result<GitStream> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0");
    command_stream(&mut command)
}

fn count_git_lines(directory: &Path, arguments: &[&str]) -> Result<u64> {
    let mut child = git_stream(directory, arguments)?;
    let stdout = child
        .child_mut()?
        .stdout
        .take()
        .context("Git stdout unavailable")?;
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::with_capacity(256);
    let mut count = 0_u64;
    while read_bounded_line(&mut reader, &mut line, MAX_GIT_LINE_BYTES)? {
        count = count.checked_add(1).context("Git line count overflow")?;
    }
    child.ensure_ok("git inventory")?;
    Ok(count)
}

fn reachable_git_size(source: &Path) -> Result<(u64, u64)> {
    let mut rev_list = git_stream(source, &["rev-list", "--objects", "HEAD"])?;
    let rev_stdout = rev_list
        .child_mut()?
        .stdout
        .take()
        .context("rev-list stdout unavailable")?;

    let mut cat_command = Command::new("git");
    cat_command
        .arg("-C")
        .arg(source)
        .args(["cat-file", "--batch-check=%(objectsize)"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped());
    let mut cat_file = command_stream(&mut cat_command)?;
    let mut cat_input = cat_file
        .child_mut()?
        .stdin
        .take()
        .context("cat-file stdin unavailable")?;
    let cat_stdout = cat_file
        .child_mut()?
        .stdout
        .take()
        .context("cat-file stdout unavailable")?;

    // Feed stdin concurrently with parsing stdout: cat-file can otherwise fill
    // its stdout pipe while this thread is blocked writing its stdin pipe. If
    // parsing fails, kill cat-file before joining the feeder so a blocked write
    // is released; dropping rev_list then terminates its producer as well.
    let (feed_result, size_result) = std::thread::scope(|scope| {
        let feeder = scope.spawn(move || -> Result<u64> {
            let mut reader = BufReader::new(rev_stdout);
            let mut line = Vec::with_capacity(256);
            let mut objects = 0_u64;
            while read_bounded_line(&mut reader, &mut line, MAX_GIT_LINE_BYTES)? {
                let oid_end = line
                    .iter()
                    .position(u8::is_ascii_whitespace)
                    .unwrap_or(line.len());
                ensure!(oid_end != 0, "missing object id");
                cat_input.write_all(&line[..oid_end])?;
                cat_input.write_all(b"\n")?;
                objects = objects
                    .checked_add(1)
                    .context("Git object count overflow")?;
            }
            Ok(objects)
        });

        let mut reader = BufReader::new(cat_stdout);
        let mut line = Vec::with_capacity(32);
        let mut bytes = 0_u64;
        let mut sized_objects = 0_u64;
        let size_result = (|| -> Result<(u64, u64)> {
            while read_bounded_line(&mut reader, &mut line, MAX_GIT_LINE_BYTES)? {
                bytes = bytes
                    .checked_add(std::str::from_utf8(trim_line(&line))?.parse()?)
                    .context("Git byte count overflow")?;
                sized_objects = sized_objects
                    .checked_add(1)
                    .context("Git object count overflow")?;
            }
            Ok((sized_objects, bytes))
        })();
        if size_result.is_err() {
            cat_file.kill_and_wait();
        }
        let feed_result = feeder
            .join()
            .map_err(|_| anyhow::anyhow!("cat-file input feeder panicked"))
            .and_then(|result| result);
        (feed_result, size_result)
    });

    let objects = feed_result?;
    let (sized_objects, bytes) = size_result?;
    rev_list.ensure_ok("git rev-list")?;
    cat_file.ensure_ok("git cat-file")?;
    ensure!(
        objects == sized_objects,
        "cat-file returned {sized_objects} sizes for {objects} objects"
    );
    Ok((objects, bytes))
}

fn read_bounded_line<R: BufRead>(reader: &mut R, line: &mut Vec<u8>, limit: usize) -> Result<bool> {
    line.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!line.is_empty());
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len() + take > limit {
            // Return immediately. Streaming callers own a child guard whose
            // Drop implementation kills and waits instead of draining output.
            anyhow::bail!("Git output line exceeded {limit} byte limit");
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            return Ok(true);
        }
    }
}

fn trim_line(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn ensure_bounded_lines(bytes: &[u8], limit: usize) -> Result<()> {
    let mut line_start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            ensure!(
                index + 1 - line_start <= limit,
                "Git output line exceeded {limit} byte limit"
            );
            line_start = index + 1;
        }
    }
    ensure!(
        bytes.len() - line_start <= limit,
        "Git output line exceeded {limit} byte limit"
    );
    Ok(())
}

fn read_bounded(mut file: File, limit: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(limit.min(8192));
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "command output exceeded {limit} byte limit"
    );
    Ok(bytes)
}

pub(crate) fn directory_size(root: &Path) -> Result<u64> {
    let mut total = 0_u64;
    let mut stack = vec![root.to_owned()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total = total
                    .checked_add(metadata.len())
                    .context("store size overflow")?;
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_line_reader_accepts_limit_and_reuses_scratch() {
        let mut reader = BufReader::new(&b"123\nxy\n"[..]);
        let mut line = Vec::new();
        assert!(read_bounded_line(&mut reader, &mut line, 4).unwrap());
        assert_eq!(line, b"123\n");
        assert!(read_bounded_line(&mut reader, &mut line, 4).unwrap());
        assert_eq!(line, b"xy\n");
        assert!(!read_bounded_line(&mut reader, &mut line, 4).unwrap());
        assert!(read_bounded_line(&mut BufReader::new(&b"12345\n"[..]), &mut line, 4).is_err());
    }

    #[test]
    fn oversized_stream_line_kills_and_waits_without_draining() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 12345; exec sleep 30"]);
        let mut stream = command_stream(&mut command).unwrap();
        let stdout = stream.child_mut().unwrap().stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();

        assert!(read_bounded_line(&mut reader, &mut line, 4).is_err());
        drop(reader);
        let started = std::time::Instant::now();
        drop(stream);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "stream guard drained or failed to terminate the child"
        );
    }

    #[test]
    fn bounded_command_capture_rejects_aggregate_excess() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 12345"]);
        assert!(run_command("test", &mut command).is_ok());
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&vec![b'x'; MAX_GIT_OUTPUT_BYTES + 1])
            .unwrap();
        assert!(read_bounded(file, MAX_GIT_OUTPUT_BYTES).is_err());
    }

    #[test]
    fn inventory_pins_head_corpus_and_reports_worktree_overlay_separately() {
        let repository = tempfile::tempdir().unwrap();
        git_init(repository.path(), &["init"]).unwrap();
        fs::write(repository.path().join("tracked"), b"corpus\n").unwrap();
        git_init(repository.path(), &["add", "tracked"]).unwrap();
        git_init(
            repository.path(),
            &[
                "-c",
                "user.name=Wheat Profile Test",
                "-c",
                "user.email=wheat-profile-test@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        )
        .unwrap();

        let clean = source_inventory(repository.path()).unwrap();
        assert_eq!(
            clean.0,
            git(repository.path(), &["rev-parse", "HEAD"]).unwrap()
        );
        assert!(!clean.1);

        fs::write(repository.path().join("untracked"), b"overlay\n").unwrap();
        let dirty = source_inventory(repository.path()).unwrap();
        assert_eq!(dirty.0, clean.0);
        assert!(dirty.1);
    }

    fn git_init(directory: &Path, arguments: &[&str]) -> Result<()> {
        let status = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .status()?;
        ensure!(status.success(), "test Git command failed with {status}");
        Ok(())
    }
}
