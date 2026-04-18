use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::progress::ProgressReporter;
use crate::shell::{CommandProgress, run_command_reported};
use crate::types::{ChangedFile, PullRequestDetails};

#[derive(Debug, Deserialize)]
struct GhRepoView {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Debug, Deserialize)]
struct GhPrView {
    number: u64,
    title: String,
    url: String,
    body: Option<String>,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    files: Vec<GhFile>,
}

#[derive(Debug, Deserialize)]
struct GhFile {
    path: String,
    additions: Option<u64>,
    deletions: Option<u64>,
}

pub async fn resolve_repo_name(
    repo_path: &Path,
    progress: Arc<ProgressReporter>,
) -> Result<String> {
    let args = vec![
        "repo".to_string(),
        "view".to_string(),
        "--json".to_string(),
        "nameWithOwner".to_string(),
    ];
    let output = run_command_reported(
        "gh",
        &args,
        repo_path,
        CommandProgress::new(progress, "gh repo view --json nameWithOwner"),
    )
    .await
    .context("failed to resolve GitHub repo via gh")?;
    let value: GhRepoView =
        serde_json::from_str(&output.stdout).context("failed to parse gh repo view output")?;
    Ok(value.name_with_owner)
}

pub async fn ensure_repo_checkout(
    repo: &str,
    target_dir: &Path,
    progress: Arc<ProgressReporter>,
) -> Result<PathBuf> {
    if target_dir.exists() {
        return Ok(target_dir.to_path_buf());
    }

    let parent = target_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid clone target {}", target_dir.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("failed creating {}", parent.display()))?;

    let args = vec![
        "repo".to_string(),
        "clone".to_string(),
        repo.to_string(),
        target_dir.display().to_string(),
        "--".to_string(),
        "--filter=blob:none".to_string(),
    ];
    run_command_reported(
        "gh",
        &args,
        parent,
        CommandProgress::new(progress, format!("gh repo clone {repo}")),
    )
    .await
    .with_context(|| format!("failed to clone {repo} into {}", target_dir.display()))?;

    Ok(target_dir.to_path_buf())
}

pub async fn fetch_pr_details(
    repo_path: &Path,
    repo: &str,
    pr_number: u64,
    progress: Arc<ProgressReporter>,
) -> Result<PullRequestDetails> {
    let args = vec![
        "pr".to_string(),
        "view".to_string(),
        pr_number.to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--json".to_string(),
        "number,title,url,body,baseRefName,headRefName,headRefOid,files".to_string(),
    ];

    let output = run_command_reported(
        "gh",
        &args,
        repo_path,
        CommandProgress::new(progress, format!("gh pr view #{pr_number}")),
    )
    .await
    .with_context(|| format!("failed to fetch PR #{pr_number}"))?;

    let pr: GhPrView =
        serde_json::from_str(&output.stdout).context("failed to parse gh pr view output")?;

    Ok(PullRequestDetails {
        number: pr.number,
        title: pr.title,
        url: pr.url,
        body: pr.body.unwrap_or_default(),
        base_ref_name: pr.base_ref_name,
        head_ref_name: pr.head_ref_name,
        head_ref_oid: pr.head_ref_oid,
        files: pr
            .files
            .into_iter()
            .map(|file| ChangedFile {
                path: file.path,
                additions: file.additions.unwrap_or(0),
                deletions: file.deletions.unwrap_or(0),
            })
            .collect(),
    })
}
