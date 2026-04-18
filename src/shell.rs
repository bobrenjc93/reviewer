use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::progress::ProgressReporter;
use crate::runlog::LiveStreamLogger;

const COMMAND_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub status_code: Option<i32>,
    pub success: bool,
}

#[derive(Debug, Clone)]
pub struct CommandProgress {
    reporter: Arc<ProgressReporter>,
    label: String,
}

impl CommandProgress {
    pub fn new(reporter: Arc<ProgressReporter>, label: impl Into<String>) -> Self {
        Self {
            reporter,
            label: label.into(),
        }
    }
}

pub async fn run_command(program: &str, args: &[String], cwd: &Path) -> Result<CmdOutput> {
    run_command_with_input_reported(program, args, cwd, None, None).await
}

pub async fn run_command_reported(
    program: &str,
    args: &[String],
    cwd: &Path,
    progress: CommandProgress,
) -> Result<CmdOutput> {
    run_command_with_input_reported(program, args, cwd, None, Some(progress)).await
}

pub async fn run_command_with_input_reported(
    program: &str,
    args: &[String],
    cwd: &Path,
    stdin_text: Option<&str>,
    progress: Option<CommandProgress>,
) -> Result<CmdOutput> {
    let output =
        capture_command_with_input_reported(program, args, cwd, stdin_text, progress).await?;

    if !output.success {
        bail!(
            "{program} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status_code,
            trim_for_error(&output.stdout),
            trim_for_error(&output.stderr)
        );
    }

    Ok(output)
}

pub async fn capture_command_with_input_reported(
    program: &str,
    args: &[String],
    cwd: &Path,
    stdin_text: Option<&str>,
    progress: Option<CommandProgress>,
) -> Result<CmdOutput> {
    capture_command_with_input_streamed(program, args, cwd, stdin_text, progress, None).await
}

pub async fn capture_command_with_input_streamed(
    program: &str,
    args: &[String],
    cwd: &Path,
    stdin_text: Option<&str>,
    progress: Option<CommandProgress>,
    live_stream: Option<LiveStreamLogger>,
) -> Result<CmdOutput> {
    let active = begin_command_progress(progress);

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command
        .spawn()
        .with_context(|| format!("failed to spawn {program}"));

    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            finish_command_error(active, &error.to_string());
            return Err(error);
        }
    };

    if let Some(input) = stdin_text {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("stdin unavailable for {program}"));
        let mut stdin = match stdin {
            Ok(stdin) => stdin,
            Err(error) => {
                finish_command_error(active, &error.to_string());
                return Err(error);
            }
        };
        let write_result = stdin
            .write_all(input.as_bytes())
            .await
            .with_context(|| format!("failed writing stdin to {program}"));
        if let Err(error) = write_result {
            finish_command_error(active, &error.to_string());
            return Err(error);
        }
        let shutdown_result = stdin
            .shutdown()
            .await
            .with_context(|| format!("failed closing stdin for {program}"));
        if let Err(error) = shutdown_result {
            finish_command_error(active, &error.to_string());
            return Err(error);
        }
    }

    let (stdout, stderr, status_code, success) = if let Some(live_stream) = live_stream {
        let stdout_reader = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("stdout unavailable for {program}"));
        let stdout_reader = match stdout_reader {
            Ok(stdout_reader) => stdout_reader,
            Err(error) => {
                finish_command_error(active, &error.to_string());
                return Err(error);
            }
        };
        let stderr_reader = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("stderr unavailable for {program}"));
        let stderr_reader = match stderr_reader {
            Ok(stderr_reader) => stderr_reader,
            Err(error) => {
                finish_command_error(active, &error.to_string());
                return Err(error);
            }
        };

        let stdout_stream = live_stream.clone();
        let stderr_stream = live_stream.clone();
        let stdout_task = tokio::spawn(async move {
            read_and_mirror_stream(stdout_reader, StreamKind::Stdout, stdout_stream).await
        });
        let stderr_task = tokio::spawn(async move {
            read_and_mirror_stream(stderr_reader, StreamKind::Stderr, stderr_stream).await
        });

        let status = child
            .wait()
            .await
            .with_context(|| format!("failed waiting for {program}"));
        let status = match status {
            Ok(status) => status,
            Err(error) => {
                finish_command_error(active, &error.to_string());
                return Err(error);
            }
        };

        let stdout = match stdout_task.await {
            Ok(result) => match result {
                Ok(bytes) => bytes,
                Err(error) => {
                    finish_command_error(active, &error.to_string());
                    return Err(error);
                }
            },
            Err(error) => {
                finish_command_error(active, &error.to_string());
                return Err(anyhow!("stdout reader task failed for {program}: {error}"));
            }
        };

        let stderr = match stderr_task.await {
            Ok(result) => match result {
                Ok(bytes) => bytes,
                Err(error) => {
                    finish_command_error(active, &error.to_string());
                    return Err(error);
                }
            },
            Err(error) => {
                finish_command_error(active, &error.to_string());
                return Err(anyhow!("stderr reader task failed for {program}: {error}"));
            }
        };

        (
            String::from_utf8_lossy(&stdout).to_string(),
            String::from_utf8_lossy(&stderr).to_string(),
            status.code(),
            status.success(),
        )
    } else {
        let output = child
            .wait_with_output()
            .await
            .with_context(|| format!("failed waiting for {program}"));
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                finish_command_error(active, &error.to_string());
                return Err(error);
            }
        };

        (
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
            output.status.code(),
            output.status.success(),
        )
    };

    let result = CmdOutput {
        stdout,
        stderr,
        status_code,
        success,
    };
    finish_command_result(active, &result);
    Ok(result)
}

fn trim_for_error(value: &str) -> String {
    let limit = 6_000;
    if value.chars().count() <= limit {
        return value.trim().to_string();
    }

    let head: String = value.chars().take(limit / 2).collect();
    let tail: String = value
        .chars()
        .rev()
        .take(limit / 2)
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    format!("{head}\n...\n{tail}")
}

struct ActiveCommand {
    handle: crate::progress::CommandHandle,
    ticker: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

async fn read_and_mirror_stream<R>(
    mut reader: R,
    kind: StreamKind,
    live_stream: LiveStreamLogger,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut all = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .context("failed reading subprocess stream")?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        all.extend_from_slice(chunk);
        let text = String::from_utf8_lossy(chunk);
        match kind {
            StreamKind::Stdout => live_stream.append_stdout_chunk(&text).await?,
            StreamKind::Stderr => live_stream.append_stderr_chunk(&text).await?,
        }
    }
    Ok(all)
}

fn begin_command_progress(progress: Option<CommandProgress>) -> Option<ActiveCommand> {
    progress.map(|progress| {
        let handle = progress.reporter.begin_command(&progress.label);
        let heartbeat_started_at = Instant::now();
        let heartbeat_handle = handle.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(COMMAND_HEARTBEAT_INTERVAL).await;
                heartbeat_handle.heartbeat(heartbeat_started_at.elapsed().as_secs_f32());
            }
        });

        ActiveCommand { handle, ticker }
    })
}

fn finish_command_result(active: Option<ActiveCommand>, output: &CmdOutput) {
    let Some(active) = active else {
        return;
    };
    active.ticker.abort();
    if output.success {
        active
            .handle
            .done(format!("exit {}", output.status_code.unwrap_or(0)));
    } else {
        active.handle.fail(format!("exit {:?}", output.status_code));
    }
}

fn finish_command_error(active: Option<ActiveCommand>, error: &str) {
    let Some(active) = active else {
        return;
    };
    active.ticker.abort();
    active.handle.fail(error);
}
