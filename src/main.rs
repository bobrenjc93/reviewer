mod git;
mod github;
mod progress;
mod provider;
mod request;
mod review;
mod runlog;
mod shell;
mod types;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};

use git::is_git_repo;
use progress::ProgressReporter;
use provider::{PromptPreamble, Provider, build_provider};
use request::{RequestSpec, resolve_request};
use review::{ReviewOptions, render_markdown, run_review};
use runlog::RunLogger;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderKind {
    Codex,
    Claude,
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Codex => write!(f, "codex"),
            Self::Claude => write!(f, "claude"),
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "reviewer",
    about = "Worktree-based PR review harness that shells out to Codex or Claude.",
    after_help = "Example:\n  reviewer \\\n    --provider claude \\\n    --extra-args \"--dangerously-enable-internet-mode --dangerously-skip-permissions\" \\\n    --pr \"https://github.com/pytorch/pytorch/pull/180697\""
)]
struct Args {
    #[arg(long, value_enum)]
    provider: ProviderKind,

    #[arg(long)]
    pr: String,

    #[arg(long)]
    repo_path: Option<PathBuf>,

    #[arg(long)]
    repo: Option<String>,

    #[arg(long)]
    model: Option<String>,

    #[arg(long, allow_hyphen_values = true)]
    extra_args: Option<String>,

    #[arg(long, default_value_t = 10)]
    parallelism: usize,

    #[arg(long)]
    keep_worktree: bool,

    #[arg(long)]
    output_markdown: Option<PathBuf>,

    #[arg(long)]
    output_json: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let exit_code = match try_main().await {
        Ok(()) => 0,
        Err(_) => 1,
    };
    std::process::exit(exit_code);
}

async fn try_main() -> Result<()> {
    let args = Args::parse();
    let provider_cwd = std::env::current_dir().context("failed to determine current directory")?;
    let run_logger = Arc::new(RunLogger::create().await?);
    let progress = Arc::new(ProgressReporter::new(run_logger.session_log_path())?);

    let result: Result<()> = async {
        let prompt_preamble = load_prompt_preamble().await?;
        let extra_args = parse_extra_args(args.extra_args.as_deref())?;
        let request = resolve_request(&args.pr, args.repo.as_deref())?;
        let repo_path =
            resolve_repo_checkout(args.repo_path.clone(), &request, progress.clone()).await?;

        progress.info(
            "run",
            format!("artifacts -> {}", run_logger.root().display()),
        );
        progress.info(
            "run",
            format!("session log -> {}", run_logger.session_log_path().display()),
        );
        progress.info(
            "run",
            format!(
                "provider={} pr={} repo_path={}",
                args.provider,
                args.pr,
                repo_path.display()
            ),
        );

        progress.info(
            "config",
            format!(
                "loaded reviewer instructions from {}",
                prompt_preamble.path.display()
            ),
        );

        if extra_args.is_empty() {
            progress.info("config", "no provider extra args configured");
        } else {
            progress.info(
                "config",
                format!("provider extra args: {}", extra_args.join(" ")),
            );
        }

        let provider = build_provider(
            args.provider.into(),
            args.model.clone(),
            run_logger.clone(),
            progress.clone(),
            Some(prompt_preamble),
            extra_args,
        );

        let repo_name = match &request.repo_name {
            Some(repo) => repo.clone(),
            None => github::resolve_repo_name(&repo_path, progress.clone()).await?,
        };

        let options = ReviewOptions {
            pr_number: request.pr_number,
            repo_name,
            repo_path,
            provider_cwd,
            parallelism: args.parallelism.max(1),
            keep_worktree: args.keep_worktree,
        };

        let report = run_review(
            options,
            provider.clone(),
            run_logger.clone(),
            progress.clone(),
        )
        .await?;
        let markdown = render_markdown(&report);
        let json = serde_json::to_string_pretty(&report)?;

        let default_json_path = run_logger.final_json_path();
        tokio::fs::write(&default_json_path, &json)
            .await
            .with_context(|| format!("failed writing {}", default_json_path.display()))?;

        let default_markdown_path = run_logger.final_markdown_path();
        tokio::fs::write(&default_markdown_path, &markdown)
            .await
            .with_context(|| format!("failed writing {}", default_markdown_path.display()))?;

        if let Some(path) = args.output_json {
            tokio::fs::write(&path, &json)
                .await
                .with_context(|| format!("failed writing {}", path.display()))?;
        }

        if let Some(path) = args.output_markdown {
            tokio::fs::write(&path, &markdown)
                .await
                .with_context(|| format!("failed writing {}", path.display()))?;
        }

        progress.log_block("FINAL REVIEW MARKDOWN", &markdown);
        progress.summary(
            "done",
            format!(
                "review complete; report={} json={} artifacts={}",
                default_markdown_path.display(),
                default_json_path.display(),
                run_logger.root().display()
            ),
        );
        Ok(())
    }
    .await;

    if let Err(error) = &result {
        progress.log_block("FINAL ERROR", &format!("{error:#}"));
        progress.summary(
            "fail",
            format!(
                "review failed; session_log={} artifacts={}",
                run_logger.session_log_path().display(),
                run_logger.root().display()
            ),
        );
    }

    result
}

impl From<ProviderKind> for provider::ProviderKind {
    fn from(value: ProviderKind) -> Self {
        match value {
            ProviderKind::Codex => provider::ProviderKind::Codex,
            ProviderKind::Claude => provider::ProviderKind::Claude,
        }
    }
}

#[allow(dead_code)]
fn _provider_name(provider: &Arc<dyn Provider>) -> &str {
    provider.kind().as_str()
}

async fn load_prompt_preamble() -> Result<PromptPreamble> {
    let Some(home) = std::env::var_os("HOME") else {
        bail!(
            "required reviewer instructions file ~/.reviewer.md could not be resolved because HOME is not set. Create ~/.reviewer.md and rerun reviewer."
        );
    };

    let path = PathBuf::from(home).join(".reviewer.md");
    if !path.exists() {
        bail!(
            "required reviewer instructions file {} was not found. Please create it with repo-specific build/test/review guidance and rerun reviewer.",
            path.display()
        );
    }

    let content = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed reading {}", path.display()))?;

    Ok(PromptPreamble { path, content })
}

fn parse_extra_args(value: Option<&str>) -> Result<Vec<String>> {
    match value {
        Some(raw) => {
            shlex::split(raw).with_context(|| format!("failed to parse --extra-args value: {raw}"))
        }
        None => Ok(Vec::new()),
    }
}

async fn resolve_repo_checkout(
    requested_repo_path: Option<PathBuf>,
    request: &RequestSpec,
    progress: Arc<ProgressReporter>,
) -> Result<PathBuf> {
    let explicit_path = match requested_repo_path {
        Some(path) => Some(
            path.canonicalize()
                .with_context(|| format!("failed to resolve repo path {}", path.display()))?,
        ),
        None => None,
    };

    if let Some(path) = &explicit_path {
        if !is_git_repo(path, progress.clone()).await {
            bail!(
                "repo path {} is not a git checkout. Point --repo-path at a clone of the target repo or omit it and let reviewer clone the repo automatically.",
                path.display()
            );
        }

        if let Some(expected_repo) = &request.repo_name {
            let actual_repo = github::resolve_repo_name(path, progress.clone())
                .await
                .with_context(|| format!("failed to resolve GitHub repo for {}", path.display()))?;
            if actual_repo != *expected_repo {
                bail!(
                    "repo path {} points at {}, but the request targets {}. Use the matching checkout or omit --repo-path.",
                    path.display(),
                    actual_repo,
                    expected_repo
                );
            }
        }

        progress.info(
            "repo",
            format!("using explicit checkout {}", path.display()),
        );
        return Ok(path.clone());
    }

    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    if is_git_repo(&cwd, progress.clone()).await {
        if let Some(expected_repo) = &request.repo_name {
            if let Ok(actual_repo) = github::resolve_repo_name(&cwd, progress.clone()).await {
                if actual_repo == *expected_repo {
                    let cwd = cwd
                        .canonicalize()
                        .with_context(|| format!("failed to resolve {}", cwd.display()))?;
                    progress.info("repo", format!("using current checkout {}", cwd.display()));
                    return Ok(cwd);
                }
            }
        } else if let Ok(cwd) = cwd.canonicalize() {
            progress.info("repo", format!("using current checkout {}", cwd.display()));
            return Ok(cwd);
        }
    }

    let repo_name = request.repo_name.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "could not determine which repository to review. Pass --repo owner/name, use a full GitHub PR URL with --pr, or run reviewer inside the target repo checkout."
        )
    })?;
    let clone_dir = clone_dir_for_repo(repo_name);
    let step = progress.begin_step(
        "phase",
        format!("materializing repo checkout for {}", repo_name),
    );
    let repo_path = github::ensure_repo_checkout(repo_name, &clone_dir, progress.clone()).await?;
    step.done(repo_path.display().to_string());
    Ok(repo_path)
}

fn clone_dir_for_repo(repo_name: &str) -> PathBuf {
    let sanitized = repo_name.replace('/', "__");
    std::env::temp_dir().join("reviewer-repos").join(sanitized)
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;

    #[test]
    fn parses_hyphenated_extra_args_value() {
        let args = Args::try_parse_from([
            "reviewer",
            "--provider",
            "claude",
            "--extra-args",
            "--dangerously-skip-permissions",
            "--pr",
            "https://github.com/pytorch/pytorch/pull/180747",
        ])
        .expect("args should parse");

        assert_eq!(
            args.extra_args.as_deref(),
            Some("--dangerously-skip-permissions")
        );
    }

    #[test]
    fn parses_multi_flag_extra_args_string() {
        let args = Args::try_parse_from([
            "reviewer",
            "--provider",
            "claude",
            "--extra-args",
            "--dangerously-enable-internet-mode --dangerously-skip-permissions",
            "--pr",
            "https://github.com/pytorch/pytorch/pull/180747",
        ])
        .expect("args should parse");

        assert_eq!(
            args.extra_args.as_deref(),
            Some("--dangerously-enable-internet-mode --dangerously-skip-permissions")
        );
    }
}
