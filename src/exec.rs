//! Command execution with separate stdout/stderr capture, CRLF
//! normalisation, and a per-stream truncation cap.
//!
//! The cap is "200 lines or 8KB, whichever comes first" per stream. We
//! normalise CRLF to LF before counting lines so the output is consistent
//! across platforms. The single `truncated` flag in [`ExecOutcome`] is true
//! if either stream was clipped.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

/// Per-stream output cap (bytes). Counted on the post-CRLF-normalised stream.
pub const MAX_BYTES_PER_STREAM: usize = 8 * 1024;
/// Per-stream output cap (lines). Counted after CRLF normalisation.
pub const MAX_LINES_PER_STREAM: usize = 200;
/// Hard wall-clock cap for any single command. Exceeding this returns a
/// truncated result with whatever output has been gathered so far.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Knobs for a single execution.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub cwd: PathBuf,
    pub timeout: Duration,
}

impl ExecOptions {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Result of running a single command.
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    pub stdout: String,
    pub stderr: String,
    /// `None` when the process was killed (signal or timeout).
    pub exit_code: Option<i32>,
    /// True if any stream was clipped or the process was killed by timeout.
    pub truncated: bool,
    /// True if the command was terminated due to wall-clock timeout.
    pub timed_out: bool,
}

/// Execute the given tokens. The first token is the program; the rest are
/// passed as discrete arguments — no shell is invoked, which is why the
/// metacharacter rejection in [`crate::safety`] is a hard prerequisite.
pub async fn execute(tokens: &[String], opts: &ExecOptions) -> Result<ExecOutcome, ExecError> {
    let (program, args) = tokens.split_first().ok_or(ExecError::EmptyCommand)?;

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(&opts.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output_future = cmd.output();
    match timeout(opts.timeout, output_future).await {
        Ok(Ok(output)) => {
            let (stdout, stdout_truncated) = clip(&output.stdout);
            let (stderr, stderr_truncated) = clip(&output.stderr);
            Ok(ExecOutcome {
                stdout,
                stderr,
                exit_code: output.status.code(),
                truncated: stdout_truncated || stderr_truncated,
                timed_out: false,
            })
        }
        Ok(Err(e)) => Err(ExecError::Spawn {
            program: program.clone(),
            source: e,
        }),
        Err(_) => Ok(ExecOutcome {
            stdout: String::new(),
            stderr: format!(
                "command timed out after {} seconds and was killed",
                opts.timeout.as_secs()
            ),
            exit_code: None,
            truncated: true,
            timed_out: true,
        }),
    }
}

/// Normalise CRLF to LF and clip to the per-stream caps. Returns
/// `(text, truncated)`.
fn clip(raw: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(raw).replace("\r\n", "\n");
    let mut byte_truncated = false;
    let mut line_truncated = false;

    let bounded_bytes = if text.len() > MAX_BYTES_PER_STREAM {
        byte_truncated = true;
        // Find the largest valid UTF-8 boundary at or before MAX_BYTES_PER_STREAM
        // so we never split a multi-byte codepoint.
        let mut cut = MAX_BYTES_PER_STREAM;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        &text[..cut]
    } else {
        text.as_str()
    };

    let mut out = String::with_capacity(bounded_bytes.len());
    for (i, line) in bounded_bytes.split_inclusive('\n').enumerate() {
        if i >= MAX_LINES_PER_STREAM {
            line_truncated = true;
            break;
        }
        out.push_str(line);
    }
    (out, byte_truncated || line_truncated)
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("cannot execute empty command")]
    EmptyCommand,

    #[error("could not spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// Helper used by integration tests and `shell_describe` to surface the
/// resolved command directory as a string.
pub fn cwd_label(cwd: &Path) -> String {
    cwd.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_normalises_crlf() {
        let raw = b"a\r\nb\r\nc\n";
        let (text, truncated) = clip(raw);
        assert_eq!(text, "a\nb\nc\n");
        assert!(!truncated);
    }

    #[test]
    fn clip_enforces_line_cap() {
        let raw: Vec<u8> = (0..300)
            .map(|i| format!("line-{i}\n"))
            .collect::<String>()
            .into_bytes();
        let (text, truncated) = clip(&raw);
        assert!(truncated);
        let line_count = text.lines().count();
        assert_eq!(line_count, MAX_LINES_PER_STREAM);
    }

    #[test]
    fn clip_enforces_byte_cap() {
        let raw = vec![b'x'; MAX_BYTES_PER_STREAM + 1024];
        let (text, truncated) = clip(&raw);
        assert!(truncated);
        assert!(text.len() <= MAX_BYTES_PER_STREAM);
    }

    #[tokio::test]
    async fn echo_runs_and_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = ExecOptions::new(tmp.path());
        let program = if cfg!(windows) { "cmd" } else { "echo" };
        let tokens: Vec<String> = if cfg!(windows) {
            ["cmd", "/C", "echo", "hi"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            ["echo", "hi"].iter().map(|s| s.to_string()).collect()
        };
        let _ = program; // silence unused warning on non-windows
        let out = execute(&tokens, &opts).await.unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("hi"));
        assert!(!out.truncated);
    }
}
