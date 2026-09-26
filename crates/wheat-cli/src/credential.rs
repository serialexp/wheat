//! Bearer tokens for native remotes, from the `credential.helper` command.
//!
//! The helper is a shell command. Wheat runs it as
//! `sh -c '<helper> "$@"' wheat-credential get` and writes the request to its
//! stdin as `key=value` lines, the way git talks to credential helpers:
//!
//! ```text
//! protocol=wheat
//! host=example.com:7420
//! path=owner/repo
//! url=wheat://example.com:7420/owner/repo
//!
//! ```
//!
//! The helper prints `token=<token>` on stdout. Printing no `token=` line means
//! "I have no token for this remote", and Wheat connects without one. Other
//! lines are ignored. Stderr and the terminal stay connected, so a helper may
//! prompt (for example to unlock a password store).

use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::authoring_metadata;
use crate::remote::WheatUrl;

/// Largest token Wheat will send; the server looks at no more than this.
const MAX_TOKEN_BYTES: usize = 8 * 1024;
/// Largest helper output Wheat will read.
const MAX_HELPER_OUTPUT_BYTES: u64 = 64 * 1024;

/// The token for `url`: from the configured helper, or empty when no helper is
/// configured or it has none. `store_root` is `None` before a store exists.
pub(crate) fn token_for(store_root: Option<&Path>, url: &WheatUrl) -> Result<Vec<u8>> {
    match authoring_metadata::credential_helper(store_root)? {
        Some(helper) => run_helper(&helper, url),
        None => Ok(Vec::new()),
    }
}

/// Run `helper` for `url` and return the token it prints (empty for none).
pub(crate) fn run_helper(helper: &str, url: &WheatUrl) -> Result<Vec<u8>> {
    let request = helper_request(url)?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("{helper} \"$@\""))
        .args(["wheat-credential", "get"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("could not start credential.helper")?;

    // The request is far smaller than a pipe buffer, so writing it before
    // reading stdout cannot deadlock. A helper that ignores stdin may already
    // have exited; that is fine.
    let mut stdin = child
        .stdin
        .take()
        .context("credential.helper has no stdin")?;
    match stdin.write_all(request.as_bytes()) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        Err(error) => return Err(error).context("could not write to credential.helper"),
    }
    drop(stdin);

    let mut output = Vec::new();
    let read = child
        .stdout
        .take()
        .context("credential.helper has no stdout")?
        .take(MAX_HELPER_OUTPUT_BYTES + 1)
        .read_to_end(&mut output);
    let status = child
        .wait()
        .context("could not wait for credential.helper")?;
    read.context("could not read credential.helper output")?;
    ensure!(
        status.success(),
        "credential.helper failed ({status}) for {}",
        url.authority
    );
    ensure!(
        output.len() as u64 <= MAX_HELPER_OUTPUT_BYTES,
        "credential.helper printed more than {MAX_HELPER_OUTPUT_BYTES} bytes"
    );
    parse_token(&output)
}

/// The helper's stdin for `url`.
fn helper_request(url: &WheatUrl) -> Result<String> {
    let fields = [
        ("protocol", url.scheme),
        ("host", url.authority.as_str()),
        ("path", url.repo_name.as_str()),
        ("url", url.canonical_identity()),
    ];
    let mut request = String::new();
    for (key, value) in fields {
        // A newline in a value would let a remote URL forge request lines.
        if value.chars().any(char::is_control) {
            bail!("the remote {key} contains a control character; not asking credential.helper");
        }
        request.push_str(key);
        request.push('=');
        request.push_str(value);
        request.push('\n');
    }
    request.push('\n');
    Ok(request)
}

/// Pick the token out of the helper's stdout.
fn parse_token(output: &[u8]) -> Result<Vec<u8>> {
    let mut token: Option<&[u8]> = None;
    for line in output.split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(value) = line.strip_prefix(b"token=") {
            ensure!(
                token.is_none(),
                "credential.helper printed more than one token= line"
            );
            token = Some(value);
        }
    }
    let token = token.unwrap_or_default();
    ensure!(
        token.len() <= MAX_TOKEN_BYTES,
        "the token from credential.helper is longer than {MAX_TOKEN_BYTES} bytes"
    );
    Ok(token.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::parse_wheat_url;

    fn url() -> WheatUrl {
        parse_wheat_url("wheat://Example.com:7420/team/repo").unwrap()
    }

    #[test]
    fn the_printed_token_is_returned() {
        let token = run_helper("printf 'user=x\\ntoken=s3cret\\n' #", &url()).unwrap();
        assert_eq!(token, b"s3cret");
    }

    #[test]
    fn the_helper_gets_the_request_on_stdin_and_get_as_its_argument() {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen");
        let helper = format!(
            "f() {{ echo \"arg=$1\" > '{0}'; cat >> '{0}'; }}; f",
            seen.display()
        );
        let token = run_helper(&helper, &url()).unwrap();
        assert!(token.is_empty());
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap(),
            "arg=get\nprotocol=wheat\nhost=example.com:7420\npath=team/repo\n\
             url=wheat://example.com:7420/team/repo\n\n"
        );
    }

    #[test]
    fn no_token_line_means_no_token() {
        assert!(run_helper("true", &url()).unwrap().is_empty());
        assert!(run_helper("echo hello", &url()).unwrap().is_empty());
    }

    #[test]
    fn a_failing_helper_is_an_error() {
        let error = run_helper("exit 3", &url()).unwrap_err().to_string();
        assert!(error.contains("credential.helper failed"), "{error}");
    }

    #[test]
    fn two_tokens_are_an_error() {
        let error = run_helper("printf 'token=a\\ntoken=b\\n' #", &url())
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than one"), "{error}");
    }

    #[test]
    fn oversized_output_and_tokens_are_errors() {
        let error = run_helper("head -c 70000 /dev/zero #", &url())
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than"), "{error}");

        let long = "a".repeat(MAX_TOKEN_BYTES + 1);
        let error = parse_token(format!("token={long}\n").as_bytes())
            .unwrap_err()
            .to_string();
        assert!(error.contains("longer than"), "{error}");
    }

    #[test]
    fn carriage_returns_are_trimmed() {
        assert_eq!(parse_token(b"token=abc\r\n").unwrap(), b"abc");
    }

    #[test]
    fn control_characters_in_the_url_are_refused() {
        let url = parse_wheat_url("wheat://example.com/team/re%0Apo").unwrap();
        let error = helper_request(&url).unwrap_err().to_string();
        assert!(error.contains("control character"), "{error}");
    }
}
