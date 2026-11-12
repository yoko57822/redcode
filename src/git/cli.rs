//! Running the system `git` CLI. Every Git access in redcode goes through here.

use std::path::Path;

use anyhow::{Context as _, anyhow};
use smol::io::AsyncWriteExt;
use smol::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct GitOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Runs `git <args>` in `root`. A non-zero exit code is still `Ok`; only failing
/// to spawn the process (or to collect its output) is an `Err`.
pub async fn run(root: &Path, args: &[&str], stdin: Option<&[u8]>) -> anyhow::Result<GitOutput> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    // Feed stdin concurrently with draining stdout/stderr so a large input can
    // never deadlock against a full output pipe. Dropping the pipe closes it.
    let pipe = child.stdin.take();
    let feed = async move {
        if let (Some(mut pipe), Some(data)) = (pipe, stdin) {
            // A write error means git exited early; its exit code and stderr tell why.
            let _ = pipe.write_all(data).await;
            let _ = pipe.close().await;
        }
    };
    let ((), output) = smol::future::zip(feed, child.output()).await;
    let output =
        output.with_context(|| format!("failed to read output of git {}", args.join(" ")))?;

    Ok(GitOutput {
        code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Like [`run`], but a non-zero exit code becomes `Err` carrying the trimmed
/// stderr (stdout when stderr is empty).
pub async fn run_ok(root: &Path, args: &[&str], stdin: Option<&[u8]>) -> anyhow::Result<Vec<u8>> {
    let output = run(root, args, stdin).await?;
    if output.code == 0 {
        return Ok(output.stdout);
    }
    let stderr = output.stderr.trim();
    let message = if stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        stderr.to_string()
    };
    if message.is_empty() {
        Err(anyhow!(
            "git {} exited with code {}",
            args.join(" "),
            output.code
        ))
    } else {
        Err(anyhow!("{}", message))
    }
}
