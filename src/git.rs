use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::progress::ProgressReporter;
use crate::shell::{CommandProgress, run_command, run_command_reported};

const BASE_CACHE_DIR: &str = "reviewer-base-worktrees";
const BASE_READY_MARKER: &str = ".reviewer-base-ready";

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    review_ref: String,
}

#[derive(Debug, Clone)]
pub struct BaseWorktree {
    pub path: PathBuf,
    pub commit_oid: String,
    cache_root: PathBuf,
    ready_marker: PathBuf,
}

#[derive(Debug, Clone)]
pub struct BaseWorktreeStatus {
    pub worktree: BaseWorktree,
    pub reused: bool,
}

pub async fn fetch_pr_head_ref(
    repo_path: &Path,
    pr_number: u64,
    progress: Arc<ProgressReporter>,
) -> Result<String> {
    let review_ref = format!("refs/remotes/origin/reviewer-harness/pr-{pr_number}");
    let fetch_base = vec![
        "fetch".to_string(),
        "origin".to_string(),
        format!("refs/pull/{pr_number}/head:{review_ref}"),
    ];
    run_command_reported(
        "git",
        &prefix(repo_path, fetch_base),
        repo_path,
        CommandProgress::new(progress, format!("git fetch PR #{pr_number} head")),
    )
    .await
    .with_context(|| format!("failed fetching PR #{pr_number}"))?;

    Ok(review_ref)
}

pub async fn is_git_repo(repo_path: &Path, progress: Arc<ProgressReporter>) -> bool {
    let args = vec![
        "-C".to_string(),
        repo_path.display().to_string(),
        "rev-parse".to_string(),
        "--is-inside-work-tree".to_string(),
    ];
    run_command_reported(
        "git",
        &args,
        repo_path,
        CommandProgress::new(progress, "git rev-parse --is-inside-work-tree"),
    )
    .await
    .is_ok()
}

pub async fn ensure_base_worktree(
    repo_path: &Path,
    progress: Arc<ProgressReporter>,
) -> Result<BaseWorktreeStatus> {
    let commit_oid = current_head_oid(repo_path, progress.clone()).await?;
    let cache_root = base_cache_root(repo_path, &commit_oid)?;
    let path = cache_root.join("worktree");
    let ready_marker = cache_root.join(BASE_READY_MARKER);
    let worktree = BaseWorktree {
        path,
        commit_oid,
        cache_root,
        ready_marker,
    };

    if worktree.path.exists() && worktree.ready_marker.exists() {
        return Ok(BaseWorktreeStatus {
            worktree,
            reused: true,
        });
    }

    if worktree.path.exists() || worktree.cache_root.exists() {
        let _ = cleanup_base_worktree(repo_path, &worktree, progress.clone()).await;
    }

    fs::create_dir_all(&worktree.cache_root).with_context(|| {
        format!(
            "failed creating base cache directory {}",
            worktree.cache_root.display()
        )
    })?;

    add_detached_worktree(
        repo_path,
        &worktree.path,
        &worktree.commit_oid,
        progress,
        format!("git worktree add base cache at {}", worktree.commit_oid),
    )
    .await
    .context("failed creating base worktree")?;

    Ok(BaseWorktreeStatus {
        worktree,
        reused: false,
    })
}

pub fn mark_base_worktree_ready(worktree: &BaseWorktree) -> Result<()> {
    fs::write(&worktree.ready_marker, format!("{}\n", worktree.commit_oid)).with_context(|| {
        format!(
            "failed writing base cache marker {}",
            worktree.ready_marker.display()
        )
    })?;
    Ok(())
}

pub async fn cleanup_base_worktree(
    repo_path: &Path,
    worktree: &BaseWorktree,
    progress: Arc<ProgressReporter>,
) -> Result<()> {
    if worktree.path.exists() {
        let remove_args = vec![
            "worktree".to_string(),
            "remove".to_string(),
            "--force".to_string(),
            worktree.path.display().to_string(),
        ];
        run_command_reported(
            "git",
            &prefix(repo_path, remove_args),
            repo_path,
            CommandProgress::new(progress, "git worktree remove --force base cache"),
        )
        .await
        .context("failed removing base worktree")?;
    }

    if worktree.cache_root.exists() {
        fs::remove_dir_all(&worktree.cache_root).with_context(|| {
            format!(
                "failed removing base cache directory {}",
                worktree.cache_root.display()
            )
        })?;
    }

    Ok(())
}

pub async fn create_review_worktree_from_base(
    repo_path: &Path,
    pr_number: u64,
    review_ref: &str,
    base_worktree: &BaseWorktree,
    progress: Arc<ProgressReporter>,
) -> Result<Worktree> {
    let worktree_path = temp_review_worktree_path(pr_number);
    add_detached_worktree(
        repo_path,
        &worktree_path,
        &base_worktree.commit_oid,
        progress,
        format!(
            "git worktree add seeded review clone for PR #{pr_number} at {}",
            base_worktree.commit_oid
        ),
    )
    .await
    .context("failed creating seeded review worktree")?;

    Ok(Worktree {
        path: worktree_path,
        review_ref: review_ref.to_string(),
    })
}

pub async fn seed_worktree_from_base(
    base_worktree: &BaseWorktree,
    review_worktree: &Worktree,
) -> Result<usize> {
    let roots = reusable_artifact_roots(&base_worktree.path).await?;
    if roots.is_empty() {
        return Ok(0);
    }

    let source = base_worktree.path.clone();
    let target = review_worktree.path.clone();
    let count = roots.len();
    tokio::task::spawn_blocking(move || copy_artifact_roots(&source, &target, &roots))
        .await
        .context("artifact copy task failed")??;
    Ok(count)
}

pub async fn checkout_worktree_ref(
    worktree: &Worktree,
    review_ref: &str,
    pr_number: u64,
    progress: Arc<ProgressReporter>,
) -> Result<()> {
    let args = vec![
        "-C".to_string(),
        worktree.path.display().to_string(),
        "checkout".to_string(),
        "--detach".to_string(),
        review_ref.to_string(),
    ];
    run_command_reported(
        "git",
        &args,
        &worktree.path,
        CommandProgress::new(
            progress,
            format!("git checkout PR #{pr_number} in seeded worktree"),
        ),
    )
    .await
    .context("failed checking out PR ref in seeded worktree")?;
    Ok(())
}

pub async fn cleanup_worktree(
    repo_path: &Path,
    worktree: &Worktree,
    progress: Arc<ProgressReporter>,
) -> Result<()> {
    let remove_args = vec![
        "worktree".to_string(),
        "remove".to_string(),
        "--force".to_string(),
        worktree.path.display().to_string(),
    ];
    run_command_reported(
        "git",
        &prefix(repo_path, remove_args),
        repo_path,
        CommandProgress::new(progress.clone(), "git worktree remove --force"),
    )
    .await
    .context("failed removing worktree")?;

    let update_ref_args = vec![
        "update-ref".to_string(),
        "-d".to_string(),
        worktree.review_ref.clone(),
    ];
    let _ = run_command_reported(
        "git",
        &prefix(repo_path, update_ref_args),
        repo_path,
        CommandProgress::new(progress, "git update-ref -d reviewer-harness ref"),
    )
    .await;
    Ok(())
}

pub async fn fetch_base_branch(
    repo_path: &Path,
    base_ref: &str,
    progress: Arc<ProgressReporter>,
) -> Result<()> {
    let args = vec![
        "fetch".to_string(),
        "origin".to_string(),
        base_ref.to_string(),
    ];
    run_command_reported(
        "git",
        &prefix(repo_path, args),
        repo_path,
        CommandProgress::new(progress, format!("git fetch origin {base_ref}")),
    )
    .await
    .with_context(|| format!("failed fetching base branch {base_ref}"))?;
    Ok(())
}

pub async fn diff_for_file(
    worktree_path: &Path,
    base_ref: &str,
    file: &str,
    progress: Arc<ProgressReporter>,
) -> Result<String> {
    let args = vec![
        "-C".to_string(),
        worktree_path.display().to_string(),
        "diff".to_string(),
        "--unified=40".to_string(),
        format!("origin/{base_ref}...HEAD"),
        "--".to_string(),
        file.to_string(),
    ];
    Ok(run_command_reported(
        "git",
        &args,
        worktree_path,
        CommandProgress::new(progress, format!("git diff for {file}")),
    )
    .await?
    .stdout)
}

async fn current_head_oid(repo_path: &Path, progress: Arc<ProgressReporter>) -> Result<String> {
    let args = vec![
        "-C".to_string(),
        repo_path.display().to_string(),
        "rev-parse".to_string(),
        "HEAD".to_string(),
    ];
    let output = run_command_reported(
        "git",
        &args,
        repo_path,
        CommandProgress::new(progress, "git rev-parse HEAD"),
    )
    .await
    .context("failed determining current repo HEAD")?;
    Ok(output.stdout.trim().to_string())
}

async fn add_detached_worktree(
    repo_path: &Path,
    worktree_path: &Path,
    revision: &str,
    progress: Arc<ProgressReporter>,
    label: String,
) -> Result<()> {
    let args = vec![
        "worktree".to_string(),
        "add".to_string(),
        "--detach".to_string(),
        worktree_path.display().to_string(),
        revision.to_string(),
    ];
    run_command_reported(
        "git",
        &prefix(repo_path, args),
        repo_path,
        CommandProgress::new(progress, label),
    )
    .await?;
    Ok(())
}

async fn reusable_artifact_roots(worktree_path: &Path) -> Result<Vec<String>> {
    let list_args = vec![
        "-C".to_string(),
        worktree_path.display().to_string(),
        "ls-files".to_string(),
        "-z".to_string(),
        "--others".to_string(),
        "--ignored".to_string(),
        "--exclude-standard".to_string(),
        "--directory".to_string(),
    ];
    let output = run_command("git", &list_args, worktree_path)
        .await
        .context("failed listing reusable base artifacts")?;

    let mut candidates = BTreeSet::new();
    for entry in output.stdout.split('\0') {
        let trimmed = entry.trim().trim_end_matches('/');
        if trimmed.is_empty() || trimmed == ".git" {
            continue;
        }
        if let Some(first) = trimmed.split('/').next() {
            if !first.is_empty() && first != ".git" {
                candidates.insert(first.to_string());
            }
        }
    }

    let mut roots = Vec::new();
    for candidate in candidates {
        if !top_level_path_has_tracked_files(worktree_path, &candidate).await? {
            roots.push(candidate);
        }
    }

    Ok(roots)
}

async fn top_level_path_has_tracked_files(worktree_path: &Path, candidate: &str) -> Result<bool> {
    let args = vec![
        "-C".to_string(),
        worktree_path.display().to_string(),
        "ls-files".to_string(),
        "-z".to_string(),
        "--".to_string(),
        candidate.to_string(),
    ];
    let output = run_command("git", &args, worktree_path)
        .await
        .with_context(|| format!("failed checking tracked files under {candidate}"))?;
    Ok(!output.stdout.is_empty())
}

fn base_cache_root(repo_path: &Path, commit_oid: &str) -> Result<PathBuf> {
    let canonical_repo = fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    let repo_key = sanitize_path_component(&canonical_repo.display().to_string());
    Ok(std::env::temp_dir()
        .join(BASE_CACHE_DIR)
        .join(repo_key)
        .join(commit_oid))
}

fn temp_review_worktree_path(pr_number: u64) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    std::env::temp_dir().join(format!("reviewer-pr-{pr_number}-{stamp}"))
}

fn sanitize_path_component(value: &str) -> String {
    let mut result = String::new();
    let mut last_was_dash = false;
    for ch in value.chars() {
        let normalized = if ch.is_ascii_alphanumeric() { ch } else { '-' };
        if normalized == '-' {
            if !last_was_dash {
                result.push(normalized);
            }
            last_was_dash = true;
        } else {
            result.push(normalized);
            last_was_dash = false;
        }
    }

    let trimmed = result.trim_matches('-');
    if trimmed.is_empty() {
        "repo".to_string()
    } else {
        trimmed.chars().take(96).collect()
    }
}

fn copy_artifact_roots(base_path: &Path, target_path: &Path, roots: &[String]) -> Result<()> {
    for root in roots {
        let relative = Path::new(root);
        let source = base_path.join(relative);
        if !source.exists() {
            continue;
        }
        let destination = target_path.join(relative);
        remove_existing_path(&destination)?;
        copy_path_recursive(&source, &destination)?;
    }
    Ok(())
}

fn copy_path_recursive(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed reading metadata for {}", source.display()))?;

    if metadata.file_type().is_symlink() {
        copy_symlink(source, destination)
    } else if metadata.is_dir() {
        fs::create_dir_all(destination)
            .with_context(|| format!("failed creating {}", destination.display()))?;
        fs::set_permissions(destination, metadata.permissions())
            .with_context(|| format!("failed setting permissions for {}", destination.display()))?;
        for entry in fs::read_dir(source)
            .with_context(|| format!("failed reading directory {}", source.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed reading entry in {}", source.display()))?;
            let child_source = entry.path();
            let child_destination = destination.join(entry.file_name());
            copy_path_recursive(&child_source, &child_destination)?;
        }
        Ok(())
    } else {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed creating {}", parent.display()))?;
        }
        fs::copy(source, destination).with_context(|| {
            format!(
                "failed copying {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        fs::set_permissions(destination, metadata.permissions())
            .with_context(|| format!("failed setting permissions for {}", destination.display()))?;
        Ok(())
    }
}

fn remove_existing_path(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };

    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed removing directory {}", path.display()))?;
    } else {
        fs::remove_file(path)
            .with_context(|| format!("failed removing file {}", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }
    let target = fs::read_link(source)
        .with_context(|| format!("failed reading symlink {}", source.display()))?;
    symlink(&target, destination).with_context(|| {
        format!(
            "failed creating symlink {} -> {}",
            destination.display(),
            target.display()
        )
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn copy_symlink(_source: &Path, _destination: &Path) -> Result<()> {
    anyhow::bail!("copying symlinks is not supported on this platform");
}

fn prefix(repo_path: &Path, args: Vec<String>) -> Vec<String> {
    let mut prefixed = vec!["-C".to_string(), repo_path.display().to_string()];
    prefixed.extend(args);
    prefixed
}
