use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use futures::future::join_all;
use tokio::sync::Semaphore;

use crate::git::{
    BaseWorktree, Worktree, checkout_worktree_ref, cleanup_base_worktree, cleanup_worktree,
    create_review_worktree_from_base, diff_for_file, ensure_base_worktree, fetch_base_branch,
    fetch_pr_head_ref, mark_base_worktree_ready, seed_worktree_from_base,
};
use crate::github::fetch_pr_details;
use crate::progress::ProgressReporter;
use crate::provider::{Provider, invoke_typed};
use crate::runlog::RunLogger;
use crate::shell::{CommandProgress, capture_command_with_input_reported};
use crate::types::{
    BuildExecution, CheckExecution, CheckGenerationDraft, CheckPlanDraft, CheckSpec,
    FileReviewDraft, FileReviewJob, FinalReviewDraft, FinalReviewReport, InlineComment,
    PullRequestDetails, sort_findings, sort_inline_comments,
};

const TARGET_CHECK_COUNT: usize = 5;
const MAX_CHECK_PLAN_ROUNDS: usize = 8;
const MAX_EMPTY_CHECK_ROUNDS: usize = 2;

#[derive(Debug, Clone)]
pub struct ReviewOptions {
    pub pr_number: u64,
    pub repo_name: String,
    pub repo_path: PathBuf,
    pub provider_cwd: PathBuf,
    pub parallelism: usize,
    pub keep_worktree: bool,
}

pub async fn run_review(
    options: ReviewOptions,
    provider: Arc<dyn Provider>,
    run_logger: Arc<RunLogger>,
    progress: Arc<ProgressReporter>,
) -> Result<FinalReviewReport> {
    let pr = {
        let step = progress.begin_step(
            "phase",
            format!("loading PR #{} metadata", options.pr_number),
        );
        match fetch_pr_details(
            &options.repo_path,
            &options.repo_name,
            options.pr_number,
            progress.clone(),
        )
        .await
        {
            Ok(pr) => {
                step.done(format!("{} changed files", pr.files.len()));
                pr
            }
            Err(error) => {
                step.fail(error.to_string());
                return Err(error);
            }
        }
    };

    {
        let step = progress.begin_step(
            "phase",
            format!("fetching base branch {}", pr.base_ref_name),
        );
        match fetch_base_branch(&options.repo_path, &pr.base_ref_name, progress.clone()).await {
            Ok(()) => step.done("base branch ready"),
            Err(error) => {
                step.fail(error.to_string());
                return Err(error);
            }
        }
    }

    let review_ref = {
        let step = progress.begin_step("phase", format!("checking out PR #{}", options.pr_number));
        match fetch_pr_head_ref(&options.repo_path, options.pr_number, progress.clone()).await {
            Ok(review_ref) => {
                step.done(review_ref.clone());
                review_ref
            }
            Err(error) => {
                step.fail(error.to_string());
                return Err(error);
            }
        }
    };

    let base_worktree = {
        let step = progress.begin_step("phase", "checking base worktree cache".to_string());
        match ensure_base_worktree(&options.repo_path, progress.clone()).await {
            Ok(status) => {
                let detail = if status.reused {
                    format!(
                        "reusing {} @ {}",
                        status.worktree.path.display(),
                        short_commit(&status.worktree.commit_oid)
                    )
                } else {
                    format!(
                        "created {} @ {}",
                        status.worktree.path.display(),
                        short_commit(&status.worktree.commit_oid)
                    )
                };
                step.done(detail);
                status
            }
            Err(error) => {
                step.fail(error.to_string());
                return Err(error);
            }
        }
    };

    if !base_worktree.reused {
        progress.set_agent_total(1);
        if let Err(error) = execute_base_build_phase(
            &options,
            &pr,
            &base_worktree.worktree,
            provider.clone(),
            progress.clone(),
        )
        .await
        {
            let cleanup_step =
                progress.begin_step("phase", "discarding failed base worktree".to_string());
            match cleanup_base_worktree(
                &options.repo_path,
                &base_worktree.worktree,
                progress.clone(),
            )
            .await
            {
                Ok(()) => cleanup_step.done("failed base cache removed"),
                Err(cleanup_error) => cleanup_step.fail(cleanup_error.to_string()),
            }
            return Err(error);
        }

        if let Err(error) = mark_base_worktree_ready(&base_worktree.worktree) {
            let cleanup_step =
                progress.begin_step("phase", "discarding failed base worktree".to_string());
            match cleanup_base_worktree(
                &options.repo_path,
                &base_worktree.worktree,
                progress.clone(),
            )
            .await
            {
                Ok(()) => cleanup_step.done("unmarked base cache removed"),
                Err(cleanup_error) => cleanup_step.fail(cleanup_error.to_string()),
            }
            return Err(error);
        }
    }

    let worktree = {
        let step = progress.begin_step("phase", "creating worktree from base cache".to_string());
        match create_review_worktree_from_base(
            &options.repo_path,
            options.pr_number,
            &review_ref,
            &base_worktree.worktree,
            progress.clone(),
        )
        .await
        {
            Ok(worktree) => {
                step.done(worktree.path.display().to_string());
                worktree
            }
            Err(error) => {
                step.fail(error.to_string());
                return Err(error);
            }
        }
    };

    {
        let step = progress.begin_step(
            "phase",
            "seeding review worktree from base cache".to_string(),
        );
        match seed_worktree_from_base(&base_worktree.worktree, &worktree).await {
            Ok(artifact_roots) => {
                let detail = if artifact_roots == 0 {
                    "no reusable top-level artifacts found".to_string()
                } else {
                    format!(
                        "{artifact_roots} reusable artifact roots copied from {}",
                        base_worktree.worktree.path.display()
                    )
                };
                step.done(detail);
            }
            Err(error) => {
                step.fail(error.to_string());
                return Err(error);
            }
        }
    }

    {
        let step = progress.begin_step(
            "phase",
            format!("checking out PR #{} in seeded worktree", options.pr_number),
        );
        match checkout_worktree_ref(&worktree, &review_ref, options.pr_number, progress.clone())
            .await
        {
            Ok(()) => step.done(review_ref.clone()),
            Err(error) => {
                step.fail(error.to_string());
                return Err(error);
            }
        }
    }

    let mut run_result = async {
        progress.set_agent_total(1);
        let build = execute_build_phase(
            &options,
            &pr,
            &worktree,
            &base_worktree.worktree,
            provider.clone(),
            progress.clone(),
        )
        .await?;
        let jobs = prepare_file_jobs(&pr, &worktree, progress.clone()).await?;
        progress.set_agent_total(jobs.len());
        let file_reviews = review_files(
            &options,
            &pr,
            &worktree,
            jobs,
            provider.clone(),
            progress.clone(),
        )
        .await?;
        let check_plan = plan_checks(
            &options,
            &pr,
            &worktree,
            &build,
            &file_reviews,
            provider.clone(),
            progress.clone(),
        )
        .await?;
        let checks = run_checks(
            &options,
            &worktree,
            &check_plan,
            run_logger.clone(),
            progress.clone(),
        )
        .await?;
        let checks_summary = summarize_checks(&check_plan.summary, &checks);
        progress.set_agent_total(1);
        write_final_review(
            &options,
            &pr,
            &worktree,
            &build,
            file_reviews,
            checks,
            checks_summary,
            provider.clone(),
            progress.clone(),
        )
        .await
    }
    .await;

    if !options.keep_worktree {
        let step = progress.begin_step("phase", "cleaning up worktree".to_string());
        match cleanup_worktree(&options.repo_path, &worktree, progress.clone()).await {
            Ok(()) => {
                step.done("temporary worktree removed");
                if let Ok(report) = run_result.as_mut() {
                    report.worktree_path =
                        format!("{} (removed after run)", worktree.path.display());
                    report
                        .notes
                        .push("Temporary worktree was cleaned up after completion.".to_string());
                }
            }
            Err(error) => {
                step.fail(error.to_string());
                if let Ok(report) = run_result.as_mut() {
                    report.notes.push(format!(
                        "Failed to clean up temporary worktree {}: {error}",
                        worktree.path.display()
                    ));
                }
            }
        }
    }

    if let Ok(report) = run_result.as_mut() {
        report.run_artifact_dir = run_logger.root().display().to_string();
        report.notes.push(format!(
            "Review worktree was seeded from base cache {} at commit {}.",
            base_worktree.worktree.path.display(),
            short_commit(&base_worktree.worktree.commit_oid)
        ));
    }

    run_result
}

async fn execute_base_build_phase(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    base_worktree: &BaseWorktree,
    provider: Arc<dyn Provider>,
    progress: Arc<ProgressReporter>,
) -> Result<BuildExecution> {
    let step = progress.begin_step("phase", "building base worktree".to_string());
    let prompt = build_base_worktree_prompt(options, pr, base_worktree);
    let build_result = run_build_invocation(
        provider.as_ref(),
        &options.provider_cwd,
        &base_worktree.path,
        &format!(
            "build base worktree {}",
            short_commit(&base_worktree.commit_oid)
        ),
        &prompt,
    )
    .await;

    finish_build_step(step, build_result, "base worktree build phase failed")
}

async fn execute_build_phase(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    worktree: &Worktree,
    base_worktree: &BaseWorktree,
    provider: Arc<dyn Provider>,
    progress: Arc<ProgressReporter>,
) -> Result<BuildExecution> {
    let step = progress.begin_step("phase", "building repo".to_string());
    let prompt = build_repo_prompt(options, pr, worktree, base_worktree);
    let build_result = run_build_invocation(
        provider.as_ref(),
        &options.provider_cwd,
        &worktree.path,
        &format!("build repo {}", pr.number),
        &prompt,
    )
    .await;

    finish_build_step(step, build_result, "build phase failed")
}

async fn run_build_invocation(
    provider: &dyn Provider,
    provider_cwd: &PathBuf,
    worktree_path: &PathBuf,
    label: &str,
    prompt: &str,
) -> Result<BuildExecution> {
    let mut result: BuildExecution = invoke_typed(
        provider,
        provider_cwd,
        std::slice::from_ref(worktree_path),
        label,
        prompt,
    )
    .await?;
    normalize_build_execution(&mut result);
    validate_build_execution(&result)?;
    Ok(result)
}

fn finish_build_step(
    step: crate::progress::StepHandle,
    build_result: Result<BuildExecution>,
    failure_context: &str,
) -> Result<BuildExecution> {
    match build_result {
        Ok(result) => {
            let detail = match result.commands_run.is_empty() {
                true => format!("{}: {}", result.status, result.summary),
                false => format!(
                    "{}: {} ({} commands)",
                    result.status,
                    result.summary,
                    result.commands_run.len()
                ),
            };
            if result.status == "passed" {
                step.done(detail);
                Ok(result)
            } else {
                step.fail(&detail);
                Err(anyhow!("{failure_context}: {}", result.summary))
            }
        }
        Err(error) => {
            step.fail(error.to_string());
            Err(error)
        }
    }
}

async fn prepare_file_jobs(
    pr: &PullRequestDetails,
    worktree: &Worktree,
    progress: Arc<ProgressReporter>,
) -> Result<Vec<FileReviewJob>> {
    let total_files = pr.files.len();
    let step = progress.begin_step(
        "phase",
        format!(
            "preparing file review jobs for {} changed files",
            total_files
        ),
    );
    let mut jobs = Vec::with_capacity(total_files);

    for (index, file) in pr.files.iter().enumerate() {
        progress.info(
            "review",
            format!("[{}/{}] preparing {}", index + 1, total_files, file.path),
        );
        let diff_excerpt = diff_for_file(
            &worktree.path,
            &pr.base_ref_name,
            &file.path,
            progress.clone(),
        )
        .await
        .map(|value| excerpt(&value, 16_000))
        .unwrap_or_default();

        jobs.push(FileReviewJob {
            file: file.path.clone(),
            additions: file.additions,
            deletions: file.deletions,
            diff_excerpt,
        });
    }

    step.done(format!("{} file review jobs ready", jobs.len()));
    Ok(jobs)
}

async fn review_files(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    worktree: &Worktree,
    jobs: Vec<FileReviewJob>,
    provider: Arc<dyn Provider>,
    progress: Arc<ProgressReporter>,
) -> Result<Vec<FileReviewDraft>> {
    let queue_step = progress.begin_step(
        "phase",
        format!("spawning file reviewers for {} changed files", jobs.len()),
    );

    let semaphore = Arc::new(Semaphore::new(options.parallelism));
    let mut tasks = Vec::new();
    let total_files = jobs.len();

    for (index, job) in jobs.into_iter().enumerate() {
        progress.info(
            "agents",
            format!(
                "[{}/{}] queued review for {}",
                index + 1,
                total_files,
                job.file
            ),
        );
        let semaphore = semaphore.clone();
        let provider = provider.clone();
        let provider_cwd = options.provider_cwd.clone();
        let worktree_path = worktree.path.clone();
        let pr = pr.clone();
        tasks.push(tokio::spawn(async move {
            review_single_file(
                &semaphore,
                provider,
                &provider_cwd,
                &worktree_path,
                &pr,
                job,
            )
            .await
        }));
    }

    queue_step.done(format!(
        "{} file reviewers queued with parallelism={}",
        total_files, options.parallelism
    ));

    let results = join_all(tasks).await;
    let mut reviews = Vec::with_capacity(total_files);
    for result in results {
        reviews.push(result.context("file review task panicked")??);
    }
    reviews.sort_by(|left, right| left.file.cmp(&right.file));
    Ok(reviews)
}

async fn review_single_file(
    semaphore: &Arc<Semaphore>,
    provider: Arc<dyn Provider>,
    provider_cwd: &PathBuf,
    worktree_path: &PathBuf,
    pr: &PullRequestDetails,
    job: FileReviewJob,
) -> Result<FileReviewDraft> {
    let prompt = build_file_review_prompt(pr, &job, worktree_path, provider_cwd);
    let mut review: FileReviewDraft = invoke_with_semaphore(
        semaphore,
        provider.as_ref(),
        provider_cwd,
        &[worktree_path.clone()],
        &format!("review {}", job.file),
        &prompt,
    )
    .await?;
    normalize_file_review(&job, &mut review);
    validate_file_review(&job, &review)?;
    Ok(review)
}

async fn plan_checks(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    worktree: &Worktree,
    build: &BuildExecution,
    file_reviews: &[FileReviewDraft],
    provider: Arc<dyn Provider>,
    progress: Arc<ProgressReporter>,
) -> Result<CheckPlanDraft> {
    let step = progress.begin_step("phase", "planning checks".to_string());
    let changed_test_files = changed_test_files(pr);
    let _changed_test_files = &changed_test_files;
    progress.set_agent_total(MAX_CHECK_PLAN_ROUNDS);
    let plan_result: Result<CheckPlanDraft> = async {
        let mut checks = Vec::new();
        let mut summaries = Vec::new();
        let mut empty_rounds = 0usize;
        let mut duplicate_rounds = 0usize;
        let mut stop_reason = None::<String>;

        // Regression proof is planned by the LLM using .reviewer.md guidance,
        // not hardcoded here — keeps the harness project-agnostic.

        for round in 0..MAX_CHECK_PLAN_ROUNDS {
            let prompt = build_next_check_prompt(
                options,
                pr,
                build,
                file_reviews,
                &checks,
                worktree,
                round + 1,
                TARGET_CHECK_COUNT,
                MAX_CHECK_PLAN_ROUNDS,
            );

            let mut draft: CheckGenerationDraft = invoke_typed(
                provider.as_ref(),
                &options.provider_cwd,
                &[worktree.path.clone()],
                &format!("plan check {} for PR {}", round + 1, pr.number),
                &prompt,
            )
            .await?;
            normalize_check_generation(&mut draft, checks.len());
            let planner_done = draft.done;

            if !draft.summary.trim().is_empty() {
                summaries.push(draft.summary.clone());
            }

            let Some(mut next_check) = take_next_check(draft) else {
                if planner_done {
                    stop_reason = Some("planner reported no more useful checks".to_string());
                    break;
                }
                empty_rounds += 1;
                if empty_rounds >= MAX_EMPTY_CHECK_ROUNDS {
                    stop_reason = Some("planner stopped producing runnable checks".to_string());
                    break;
                }
                continue;
            };

            empty_rounds = 0;
            normalize_check_spec(checks.len(), &mut next_check);
            if next_check.command.trim().is_empty() {
                if planner_done {
                    stop_reason =
                        Some("planner ended without producing a runnable command".to_string());
                    break;
                }
                duplicate_rounds += 1;
                if duplicate_rounds >= MAX_EMPTY_CHECK_ROUNDS {
                    stop_reason = Some("planner repeatedly omitted a runnable command".to_string());
                    break;
                }
                continue;
            }

            if is_duplicate_check(&next_check, &checks) {
                if planner_done {
                    stop_reason = Some("planner ended on a duplicate check".to_string());
                    break;
                }
                duplicate_rounds += 1;
                if duplicate_rounds >= MAX_EMPTY_CHECK_ROUNDS {
                    stop_reason = Some("planner repeated duplicate checks".to_string());
                    break;
                }
                continue;
            }

            duplicate_rounds = 0;
            checks.push(next_check);
            if planner_done {
                stop_reason = Some("planner marked the check list complete".to_string());
                break;
            }
            if checks.len() >= TARGET_CHECK_COUNT {
                stop_reason = Some(format!("reached target of {TARGET_CHECK_COUNT} checks"));
                break;
            }
        }

        let mut plan = CheckPlanDraft {
            summary: select_check_plan_summary(&summaries, checks.len(), stop_reason.as_deref()),
            checks,
        };
        normalize_check_plan(&mut plan);
        validate_check_plan(&plan)?;
        Ok(plan)
    }
    .await;

    match plan_result {
        Ok(plan) => {
            let detail = if plan.checks.is_empty() {
                "no runnable checks planned; continuing without post-review checks".to_string()
            } else {
                format!("{} checks planned", plan.checks.len())
            };
            step.done(detail);
            Ok(plan)
        }
        Err(error) => {
            step.fail(error.to_string());
            Err(error)
        }
    }
}

async fn run_checks(
    _options: &ReviewOptions,
    worktree: &Worktree,
    plan: &CheckPlanDraft,
    run_logger: Arc<RunLogger>,
    progress: Arc<ProgressReporter>,
) -> Result<Vec<CheckExecution>> {
    let total = plan.checks.len();
    let step = progress.begin_step("phase", format!("running {} checks sequentially", total));
    if total == 0 {
        step.done("no runnable checks were planned");
        return Ok(Vec::new());
    }

    let mut executions = Vec::with_capacity(total);

    for (index, check) in plan.checks.iter().enumerate() {
        let label = format!("[{}/{}] {}", index + 1, total, check.name);
        let check_step = progress.begin_step("check", label);
        let invocation = run_logger.begin(&format!("check {} {}", index + 1, check.name));
        let wrapped_command = wrap_check_command_for_execution(&check.command);
        let command_body = format!(
            "index: {}\nname: {}\ncommand: {}\nwrapped_command: {}\nrationale: {}\nexpected_signal: {}\nrelated_findings: {}\ncwd: {}\n",
            index + 1,
            check.name,
            check.command,
            wrapped_command,
            check.rationale,
            check.expected_signal,
            check.related_findings.join(" | "),
            worktree.path.display()
        );
        run_logger
            .write_text(&invocation, "check-command", &command_body)
            .await?;

        let started_at = Instant::now();
        let args = vec!["-lc".to_string(), wrapped_command.clone()];
        let output = capture_command_with_input_reported(
            "bash",
            &args,
            &worktree.path,
            None,
            Some(CommandProgress::new(
                progress.clone(),
                render_check_command_label(index + 1, total, &wrapped_command),
            )),
        )
        .await;
        let duration_secs = started_at.elapsed().as_secs_f32();

        let execution = match output {
            Ok(output) => {
                let status = if output.success { "passed" } else { "failed" };
                let result_body = format!(
                    "index: {}\nname: {}\ncommand: {}\nstatus: {}\nexit_code: {:?}\nduration_secs: {:.2}\n\nstdout:\n{}\n\nstderr:\n{}\n",
                    index + 1,
                    check.name,
                    check.command,
                    status,
                    output.status_code,
                    duration_secs,
                    excerpt(&output.stdout, 12000),
                    excerpt(&output.stderr, 12000)
                );
                run_logger
                    .write_text(&invocation, "check-result", &result_body)
                    .await?;

                if output.success {
                    check_step.done(format!("passed in {:.1}s", duration_secs));
                } else {
                    check_step.fail(format!(
                        "failed with exit code {:?} in {:.1}s",
                        output.status_code, duration_secs
                    ));
                }

                CheckExecution {
                    index: index + 1,
                    name: check.name.clone(),
                    command: check.command.clone(),
                    rationale: check.rationale.clone(),
                    expected_signal: check.expected_signal.clone(),
                    related_findings: check.related_findings.clone(),
                    status: status.to_string(),
                    exit_code: output.status_code,
                    duration_secs,
                    stdout_excerpt: excerpt(&output.stdout, 4000),
                    stderr_excerpt: excerpt(&output.stderr, 4000),
                }
            }
            Err(error) => {
                let result_body = format!(
                    "index: {}\nname: {}\ncommand: {}\nstatus: error\nduration_secs: {:.2}\n\nerror:\n{}\n",
                    index + 1,
                    check.name,
                    check.command,
                    duration_secs,
                    error
                );
                run_logger
                    .write_text(&invocation, "check-result", &result_body)
                    .await?;
                check_step.fail(format!("error after {:.1}s -> {}", duration_secs, error));

                CheckExecution {
                    index: index + 1,
                    name: check.name.clone(),
                    command: check.command.clone(),
                    rationale: check.rationale.clone(),
                    expected_signal: check.expected_signal.clone(),
                    related_findings: check.related_findings.clone(),
                    status: "error".to_string(),
                    exit_code: None,
                    duration_secs,
                    stdout_excerpt: String::new(),
                    stderr_excerpt: excerpt(&error.to_string(), 4000),
                }
            }
        };

        executions.push(execution);
    }

    let passed = executions
        .iter()
        .filter(|check| check.status == "passed")
        .count();
    let failed = executions
        .iter()
        .filter(|check| check.status == "failed")
        .count();
    let errored = executions
        .iter()
        .filter(|check| check.status == "error")
        .count();
    step.done(format!(
        "{} passed, {} failed, {} errored",
        passed, failed, errored
    ));
    Ok(executions)
}

async fn write_final_review(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    worktree: &Worktree,
    build: &BuildExecution,
    mut file_reviews: Vec<FileReviewDraft>,
    checks: Vec<CheckExecution>,
    checks_summary: String,
    provider: Arc<dyn Provider>,
    progress: Arc<ProgressReporter>,
) -> Result<FinalReviewReport> {
    let step = progress.begin_step(
        "phase",
        "writing final review summary and inline comments".to_string(),
    );

    for review in &mut file_reviews {
        sort_findings(&mut review.findings);
        sort_inline_comments(&mut review.inline_comments);
    }
    file_reviews.sort_by(|left, right| left.file.cmp(&right.file));

    let prompt = build_final_review_prompt(options, pr, build, &file_reviews, &checks, worktree);
    let draft_result: Result<FinalReviewDraft> = async {
        let mut draft: FinalReviewDraft = invoke_typed(
            provider.as_ref(),
            &options.provider_cwd,
            &[worktree.path.clone()],
            &format!("write final review {}", pr.number),
            &prompt,
        )
        .await?;
        normalize_final_review(&mut draft);
        validate_final_review(&draft)?;
        Ok(draft)
    }
    .await;

    match draft_result {
        Ok(draft) => {
            let mut summary_findings = draft.summary_findings;
            let mut inline_comments = draft.inline_comments;
            sort_findings(&mut summary_findings);
            sort_inline_comments(&mut inline_comments);
            let summary_count = summary_findings.len();
            let comment_count = inline_comments.len();
            let report = FinalReviewReport {
                repo: options.repo_name.clone(),
                pr_number: pr.number,
                pr_title: pr.title.clone(),
                provider: provider.kind().as_str().to_string(),
                worktree_path: worktree.path.display().to_string(),
                run_artifact_dir: String::new(),
                executive_summary: draft.executive_summary,
                build: Some(build.clone()),
                summary_findings,
                inline_comments,
                checks_summary,
                per_file: file_reviews,
                checks,
                notes: draft.notes,
            };
            step.done(format!(
                "{} summary findings, {} inline comments",
                summary_count, comment_count
            ));
            Ok(report)
        }
        Err(error) => {
            step.fail(error.to_string());
            Err(error)
        }
    }
}

fn validate_file_review(job: &FileReviewJob, review: &FileReviewDraft) -> Result<()> {
    anyhow::ensure!(
        !review.summary.trim().is_empty(),
        "file review for {} returned an empty summary",
        job.file
    );
    anyhow::ensure!(
        review.file.trim() == job.file,
        "file review returned file `{}` but expected `{}`",
        review.file,
        job.file
    );

    for (index, finding) in review.findings.iter().enumerate() {
        anyhow::ensure!(
            !finding.title.trim().is_empty(),
            "file review finding {} for {} is missing a title",
            index + 1,
            job.file
        );
    }

    for (index, comment) in review.inline_comments.iter().enumerate() {
        anyhow::ensure!(
            !comment.title.trim().is_empty(),
            "inline comment {} for {} is missing a title",
            index + 1,
            job.file
        );
        anyhow::ensure!(
            !comment.body.trim().is_empty(),
            "inline comment {} for {} is missing a body",
            index + 1,
            job.file
        );
    }

    Ok(())
}

fn validate_build_execution(result: &BuildExecution) -> Result<()> {
    anyhow::ensure!(
        matches!(result.status.as_str(), "passed" | "failed"),
        "build phase returned unsupported status `{}`",
        result.status
    );
    anyhow::ensure!(
        !result.summary.trim().is_empty(),
        "build phase returned an empty summary"
    );
    anyhow::ensure!(
        !result.commands_run.is_empty(),
        "build phase must report at least one command when status is {}",
        result.status
    );
    Ok(())
}

fn validate_check_plan(plan: &CheckPlanDraft) -> Result<()> {
    for (index, check) in plan.checks.iter().enumerate() {
        anyhow::ensure!(
            !check.name.trim().is_empty(),
            "check {} is missing a name",
            index + 1
        );
        anyhow::ensure!(
            !check.command.trim().is_empty(),
            "check {} is missing a command",
            index + 1
        );
    }

    Ok(())
}

fn validate_final_review(draft: &FinalReviewDraft) -> Result<()> {
    anyhow::ensure!(
        !draft.executive_summary.trim().is_empty(),
        "final review returned an empty executive summary"
    );

    for (index, finding) in draft.summary_findings.iter().enumerate() {
        anyhow::ensure!(
            !finding.title.trim().is_empty(),
            "summary finding {} is missing a title",
            index + 1
        );
    }

    for (index, comment) in draft.inline_comments.iter().enumerate() {
        anyhow::ensure!(
            !comment.title.trim().is_empty(),
            "final inline comment {} is missing a title",
            index + 1
        );
        anyhow::ensure!(
            !comment.body.trim().is_empty(),
            "final inline comment {} is missing a body",
            index + 1
        );
    }

    Ok(())
}

fn normalize_build_execution(result: &mut BuildExecution) {
    if !matches!(result.status.as_str(), "passed" | "failed") {
        result.status = "failed".to_string();
    }

    if result.summary.trim().is_empty() {
        result.summary = fallback_build_summary(&result.status, result.commands_run.len());
    }

    if result.commands_run.is_empty() {
        result.commands_run.push(
            "Build agent did not report exact commands; inspect the transcript artifact."
                .to_string(),
        );
    }
}

fn normalize_check_plan(plan: &mut CheckPlanDraft) {
    let mut unique_checks = Vec::with_capacity(plan.checks.len());
    for mut check in std::mem::take(&mut plan.checks) {
        normalize_check_spec(unique_checks.len(), &mut check);
        if check.command.trim().is_empty() || is_duplicate_check(&check, &unique_checks) {
            continue;
        }
        unique_checks.push(check);
    }
    plan.checks = unique_checks;

    if plan.summary.trim().is_empty() {
        plan.summary = fallback_check_plan_summary(plan.checks.len());
    }
}

fn normalize_check_generation(draft: &mut CheckGenerationDraft, existing_checks: usize) {
    if draft.summary.trim().is_empty() {
        draft.summary = if draft.done {
            fallback_check_plan_summary(existing_checks)
        } else {
            "Continuing to assemble targeted post-review checks.".to_string()
        };
    }

    if draft.check.is_none() && !draft.checks.is_empty() {
        draft.check = draft.checks.drain(..).next();
    }
}

fn take_next_check(mut draft: CheckGenerationDraft) -> Option<CheckSpec> {
    if draft.done && draft.check.is_none() && draft.checks.is_empty() {
        return None;
    }
    draft
        .check
        .take()
        .or_else(|| draft.checks.into_iter().next())
}

fn normalize_check_spec(index: usize, check: &mut CheckSpec) {
    if check.name.trim().is_empty() {
        check.name = first_nonempty_line(&check.command)
            .map(|line| format!("Check {}: {}", index + 1, truncate_line(&line, 72)))
            .or_else(|| {
                first_nonempty_line(&check.rationale)
                    .map(|line| format!("Check {}: {}", index + 1, truncate_line(&line, 72)))
            })
            .unwrap_or_else(|| format!("Check {}", index + 1));
    }

    if check.rationale.trim().is_empty() {
        check.rationale =
            "Validate the changed behavior and guard against regressions.".to_string();
    }

    if check.expected_signal.trim().is_empty() {
        check.expected_signal =
            "The command should complete successfully and confirm the expected behavior."
                .to_string();
    }
}

fn is_duplicate_check(candidate: &CheckSpec, existing: &[CheckSpec]) -> bool {
    let candidate_command = normalize_check_identity(&candidate.command);
    let candidate_name = normalize_check_identity(&candidate.name);
    existing.iter().any(|existing_check| {
        let existing_command = normalize_check_identity(&existing_check.command);
        let existing_name = normalize_check_identity(&existing_check.name);
        (!candidate_command.is_empty() && candidate_command == existing_command)
            || (!candidate_name.is_empty()
                && !existing_name.is_empty()
                && candidate_name == existing_name
                && candidate_command == existing_command)
    })
}

fn normalize_check_identity(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn select_check_plan_summary(
    summaries: &[String],
    check_count: usize,
    stop_reason: Option<&str>,
) -> String {
    let base = summaries
        .iter()
        .rev()
        .map(|summary| summary.trim())
        .find(|summary| !summary.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| fallback_check_plan_summary(check_count));

    match stop_reason {
        Some(reason) if !reason.trim().is_empty() => format!("{base} Stop reason: {reason}."),
        _ => base,
    }
}

fn normalize_file_review(job: &FileReviewJob, review: &mut FileReviewDraft) {
    if review.file.trim().is_empty() {
        review.file = job.file.clone();
    }

    for finding in &mut review.findings {
        normalize_review_finding(finding, Some(&job.file));
    }
    for comment in &mut review.inline_comments {
        normalize_inline_comment(comment, Some(&job.file));
    }

    if review.summary.trim().is_empty() {
        review.summary =
            fallback_review_summary(review.findings.len(), review.inline_comments.len());
    }
}

fn normalize_final_review(draft: &mut FinalReviewDraft) {
    for finding in &mut draft.summary_findings {
        normalize_review_finding(finding, None);
    }
    for comment in &mut draft.inline_comments {
        normalize_inline_comment(comment, None);
    }

    if draft.executive_summary.trim().is_empty() {
        draft.executive_summary =
            fallback_final_summary(draft.summary_findings.len(), draft.inline_comments.len());
    }
}

fn normalize_review_finding(
    finding: &mut crate::types::ReviewFinding,
    fallback_file: Option<&str>,
) {
    if finding.file.trim().is_empty() {
        finding.file = fallback_file.unwrap_or("unknown").to_string();
    }
    if finding.title.trim().is_empty() {
        finding.title = first_nonempty_line(&finding.rationale)
            .or_else(|| first_nonempty_line(&finding.suggested_fix))
            .unwrap_or_else(|| "Review finding".to_string());
    }
    if finding.rationale.trim().is_empty() {
        finding.rationale = format!("Potential issue identified in {}.", finding.file);
    }
    if finding.suggested_fix.trim().is_empty() {
        finding.suggested_fix =
            "Investigate the issue and update the implementation or tests.".to_string();
    }
    if finding.priority > 3 {
        finding.priority = 3;
    }
    if !finding.confidence.is_finite() || finding.confidence <= 0.0 {
        finding.confidence = 0.7;
    } else if finding.confidence > 1.0 {
        finding.confidence = 1.0;
    }
    if finding.source_refs.is_empty() {
        finding.source_refs.push(finding.file.clone());
    }
}

fn normalize_inline_comment(comment: &mut InlineComment, fallback_file: Option<&str>) {
    if comment.file.trim().is_empty() {
        comment.file = fallback_file.unwrap_or("unknown").to_string();
    }
    if comment.end_line.is_none() {
        comment.end_line = comment.start_line;
    }
    if comment.title.trim().is_empty() {
        comment.title =
            first_nonempty_line(&comment.body).unwrap_or_else(|| "Review comment".to_string());
    }
    if comment.body.trim().is_empty() {
        comment.body = comment.title.clone();
    }
    if comment.priority > 3 {
        comment.priority = 3;
    }
    if !comment.confidence.is_finite() || comment.confidence <= 0.0 {
        comment.confidence = 0.7;
    } else if comment.confidence > 1.0 {
        comment.confidence = 1.0;
    }
}

fn fallback_review_summary(findings: usize, comments: usize) -> String {
    if findings == 0 && comments == 0 {
        "No substantive issues found in this starting-file review.".to_string()
    } else {
        format!(
            "Review captured {} findings and {} inline comments.",
            findings, comments
        )
    }
}

fn fallback_final_summary(findings: usize, comments: usize) -> String {
    if findings == 0 && comments == 0 {
        "Review completed without high-confidence issues.".to_string()
    } else {
        format!(
            "Review completed with {} summary findings and {} inline comments.",
            findings, comments
        )
    }
}

fn fallback_build_summary(status: &str, command_count: usize) -> String {
    match status {
        "passed" => format!("Build/setup completed successfully after {command_count} command(s)."),
        _ => format!(
            "Build/setup failed after {command_count} command(s); inspect the transcript for details."
        ),
    }
}

fn fallback_check_plan_summary(check_count: usize) -> String {
    if check_count == 0 {
        "No additional non-interactive post-review checks were planned.".to_string()
    } else {
        format!("Planned {check_count} targeted post-review checks.")
    }
}

fn changed_test_files(pr: &PullRequestDetails) -> Vec<String> {
    pr.files
        .iter()
        .filter(|file| looks_like_test_file(&file.path))
        .map(|file| file.path.clone())
        .collect()
}

fn shell_join_paths(paths: &[String]) -> String {
    paths
        .iter()
        .map(|path| shell_single_quote(path))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn wrap_check_command_for_execution(command: &str) -> String {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        "set -o pipefail".to_string()
    } else {
        format!("set -o pipefail\n{trimmed}")
    }
}

fn short_commit(value: &str) -> String {
    value.chars().take(12).collect()
}

fn first_nonempty_line(value: &str) -> Option<String> {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToString::to_string)
}

fn truncate_line(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    let chars = trimmed.chars().count();
    if chars <= max_chars {
        return trimmed.to_string();
    }

    let shortened: String = trimmed.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{shortened}...")
}

fn render_check_command_label(index: usize, total: usize, command: &str) -> String {
    format!(
        "check {index}/{total}: {}",
        truncate_line(command.trim(), 120)
    )
}

async fn invoke_with_semaphore<T>(
    semaphore: &Arc<Semaphore>,
    provider: &dyn Provider,
    cwd: &PathBuf,
    extra_dirs: &[PathBuf],
    label: &str,
    prompt: &str,
) -> Result<T>
where
    T: serde::de::DeserializeOwned + schemars::JsonSchema,
{
    let _permit = semaphore.acquire().await?;
    invoke_typed(provider, cwd, extra_dirs, label, prompt).await
}

fn build_file_review_prompt(
    pr: &PullRequestDetails,
    job: &FileReviewJob,
    worktree_path: &PathBuf,
    provider_cwd: &PathBuf,
) -> String {
    format!(
        "You are reviewing PR #{pr_number} ({pr_title}) in repo {repo_url}.\n\
         Starting from this file `{file}`, can you review this file, but feel free to look at other files to ensure the changes in this file are good.\n\n\
         Agent launch cwd: {provider_cwd}\n\
         Worktree path: {worktree}\n\
         Base branch: {base}\n\
         File change stats: +{additions} / -{deletions}\n\n\
         Current diff excerpt for the starting file:\n```diff\n{diff}\n```\n\n\
         Requirements:\n\
         - The harness launched you from `{provider_cwd}`, but the PR snapshot lives at `{worktree}`. If you run commands or inspect files, operate on the worktree path, not the launch cwd.\n\
         - Inspect any nearby or dependent files you need, but keep the review centered on the starting file.\n\
         - Report only substantive correctness, regression, reliability, or maintainability issues.\n\
         - Ignore style-only nits and duplicate observations.\n\
         - Return a compact JSON object with keys: `summary`, `findings`, `inline_comments`, `notes`.\n\
         - Do not include a top-level `file` field; the harness already knows the starting file.\n\
         - In `findings`, the `file` field is optional and defaults to the starting file.\n\
         - In `inline_comments`, `file` is optional and defaults to the starting file. Use either `line` or `start_line`; `end_line` is optional.\n\
         - If a field is not important, omit it instead of inventing filler values.\n\
         - Return at most 5 findings and at most 5 inline comments.\n\
         - Inline comments should be line-anchored when possible. If exact lines are unclear, use a file-level comment with null line numbers.\n\
         - Inline comments can mention another file only when it is directly necessary to explain why the starting file's change is wrong.\n\
         - Use priority 0 for release-blocking issues, 1 for major bugs, 2 for moderate issues, 3 for minor-but-actionable issues.\n\
         - Keep source_refs specific to the files, symbols, or commands you inspected.\n\
         - If there is no meaningful issue, return empty findings and inline_comments arrays.",
        pr_number = pr.number,
        pr_title = pr.title,
        repo_url = pr.url,
        file = job.file,
        provider_cwd = provider_cwd.display(),
        worktree = worktree_path.display(),
        base = pr.base_ref_name,
        additions = job.additions,
        deletions = job.deletions,
        diff = job.diff_excerpt
    )
}

fn build_repo_prompt(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    worktree: &Worktree,
    base_worktree: &BaseWorktree,
) -> String {
    let changed_files = pr
        .files
        .iter()
        .map(|file| {
            format!(
                "- {} (+{} / -{})",
                file.path, file.additions, file.deletions
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are the repo build phase agent for PR #{pr_number} ({pr_title}) in repo {repo}.\n\
         Agent launch cwd: {provider_cwd}\n\
         Worktree path: {worktree}\n\
         Seeded base cache: {base_worktree} @ {base_commit}\n\
         Base branch: {base}\n\
         PR URL: {pr_url}\n\n\
         Changed files:\n{changed_files}\n\n\
         Task:\n\
         - You were launched from `{provider_cwd}`, but the PR snapshot lives at `{worktree}`. For any repo command, first `cd` into the worktree or otherwise target that path explicitly. Do not build from the launch cwd.\n\
         - This PR worktree was seeded from `{base_worktree}`, which already contains reusable build artifacts from commit `{base_commit}`. Prefer incremental commands that reuse those artifacts when safe. Do not clean the build tree unless you have to.\n\
         - Read the global reviewer instructions loaded above and use them as the primary source of truth for how this repo should be built, prepared, or bootstrapped.\n\
         - Actually execute the build or setup flow from the worktree. Do not just recommend commands.\n\
         - Trust the reviewer instructions more than lightweight heuristics. If they specify a full build command, run it.\n\
         - Keep the run non-interactive.\n\
         - If the build fails, stop after enough investigation to explain the blocker clearly and return status `failed`.\n\
         - If the reviewer instructions are missing, unusable, or do not contain executable build guidance, treat that as a hard failure and return status `failed` with a concrete explanation.\n\n\
         Return requirements:\n\
         - Return a compact JSON object with keys: `status`, `summary`, `commands_run`, `notes`.\n\
         - `status` must be one of: passed, failed.\n\
         - `commands_run` must contain the exact shell commands you actually executed, in order.\n\
         - `summary` should be a short plain-English result.\n\
         - `notes` must be a JSON array of strings. Use `[]` or `[\"...\"]`; never return a bare string for `notes`.\n\
         - Omit fields you do not need instead of filling them with noise.",
        pr_number = pr.number,
        pr_title = pr.title,
        repo = options.repo_name,
        provider_cwd = options.provider_cwd.display(),
        worktree = worktree.path.display(),
        base_worktree = base_worktree.path.display(),
        base_commit = short_commit(&base_worktree.commit_oid),
        base = pr.base_ref_name,
        pr_url = pr.url,
        changed_files = changed_files
    )
}

fn build_base_worktree_prompt(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    base_worktree: &BaseWorktree,
) -> String {
    format!(
        "You are preparing a reusable base build cache for repo {repo}.\n\
         Agent launch cwd: {provider_cwd}\n\
         Base worktree path: {worktree}\n\
         Base worktree commit: {commit}\n\
         Upcoming PR under review: #{pr_number} ({pr_title})\n\
         PR URL: {pr_url}\n\n\
         Task:\n\
         - You were launched from `{provider_cwd}`, but the reusable base snapshot lives at `{worktree}`. For any repo command, first `cd` into the base worktree or otherwise target that path explicitly.\n\
         - Read the global reviewer instructions loaded above and use them as the primary source of truth for how this repo should be built, prepared, or bootstrapped.\n\
         - Your goal is to create a reusable baseline that future PR worktrees can clone from for incremental builds.\n\
         - Actually execute the build or setup flow from the base worktree. Do not just recommend commands.\n\
         - Because this is a reusable base cache, do not take a PR-specific lightweight validation-only fast path just because the upcoming PR changes are small or Python-only. Prefer the heaviest reusable local build/setup path the reviewer instructions support.\n\
         - Keep the run non-interactive.\n\
         - If the build fails, stop after enough investigation to explain the blocker clearly and return status `failed`.\n\
         - If the reviewer instructions are missing, unusable, or do not contain executable build guidance, treat that as a hard failure and return status `failed` with a concrete explanation.\n\n\
         Return requirements:\n\
         - Return a compact JSON object with keys: `status`, `summary`, `commands_run`, `notes`.\n\
         - `status` must be one of: passed, failed.\n\
         - `commands_run` must contain the exact shell commands you actually executed, in order.\n\
         - `summary` should be a short plain-English result.\n\
         - `notes` must be a JSON array of strings. Use `[]` or `[\"...\"]`; never return a bare string for `notes`.\n\
         - Omit fields you do not need instead of filling them with noise.",
        repo = options.repo_name,
        provider_cwd = options.provider_cwd.display(),
        worktree = base_worktree.path.display(),
        commit = short_commit(&base_worktree.commit_oid),
        pr_number = pr.number,
        pr_title = pr.title,
        pr_url = pr.url
    )
}

fn build_next_check_prompt(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    build: &BuildExecution,
    file_reviews: &[FileReviewDraft],
    planned_checks: &[CheckSpec],
    worktree: &Worktree,
    round: usize,
    target_count: usize,
    max_rounds: usize,
) -> String {
    let changed_files = pr
        .files
        .iter()
        .map(|file| {
            format!(
                "- {} (+{} / -{})",
                file.path, file.additions, file.deletions
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let changed_test_files = pr
        .files
        .iter()
        .filter(|file| looks_like_test_file(&file.path))
        .map(|file| format!("- {}", file.path))
        .collect::<Vec<_>>();
    let changed_test_files = if changed_test_files.is_empty() {
        "- none".to_string()
    } else {
        changed_test_files.join("\n")
    };
    let existing_checks = if planned_checks.is_empty() {
        "[]".to_string()
    } else {
        serde_json::to_string_pretty(planned_checks).unwrap_or_else(|_| "[]".to_string())
    };

    format!(
        "You are planning the checks phase for PR #{pr_number} ({pr_title}) in repo {repo}.\n\
         This planner runs incrementally: return at most one new check for this round.\n\
         Agent launch cwd: {provider_cwd}\n\
         Worktree path: {worktree}\n\
         Base branch: {base}\n\
         Planning round: {round}/{max_rounds}\n\
         Existing planned checks: {existing_count}/{target_count}\n\n\
         Changed files:\n{changed_files}\n\n\
         Changed test-like files:\n{changed_test_files}\n\n\
         Build phase result:\n{build}\n\n\
         Per-file review results:\n{file_reviews}\n\n\
         Checks already planned:\n{existing_checks}\n\n\
         Requirements:\n\
         - If you inspect repo state, use the worktree path rather than the launch cwd.\n\
         - Return a compact JSON object with keys: `summary`, `done`, and `check`.\n\
         - `check` must be either a single check object or omitted/null. Do not return more than one new check in this round.\n\
         - Aim to reach {target_count} total checks when there is enough meaningful coverage to justify them, but do not pad with low-value or duplicate commands.\n\
         - If there are no more worthwhile non-interactive checks, set `done` to true and omit `check`.\n\
         - Every returned check must include a non-empty `name` and a non-empty `command`.\n\
         - Use the exact keys `name`, `command`, `rationale`, `expected_signal`, and `related_findings` inside `check`. Do not substitute alternate key names.\n\
         - If there are no existing planned checks AND the PR changes both test and non-test files, the first check MUST be a regression proof: revert the non-test files to the base branch (`git checkout origin/{base} -- <non-test files>`), run the changed tests, then restore (`git checkout HEAD -- <non-test files>`). The test should fail after reverting, proving it encodes the fix. Use the environment setup from the global reviewer instructions.\n\
         - If there are no existing planned checks AND the PR only changes test files (no non-test files), the first check should just run the changed tests directly.\n\
         - Commands must be non-interactive shell commands that can be run from the worktree with `bash -lc`.\n\
         - Do not use shell pipelines that can hide failures. If you need a pipeline, preserve failures with `set -o pipefail`.\n\
         - Use the build phase result plus repo-specific guidance from the global reviewer instructions when choosing commands.\n\
         - Prefer targeted checks first, then broader validation where useful.\n\
         - Avoid duplicates of any already-planned command.\n\
         - Every check should include a clear rationale and expected signal, but those fields can be brief.\n\
         - Related findings should cite finding titles, inline comment titles, or file paths, not vague references.\n\
         - Be conservative: it is acceptable to stop before {target_count} checks if additional commands would be redundant, speculative, too broad, or not runnable from the current environment.",
        pr_number = pr.number,
        pr_title = pr.title,
        repo = options.repo_name,
        provider_cwd = options.provider_cwd.display(),
        worktree = worktree.path.display(),
        base = pr.base_ref_name,
        round = round,
        max_rounds = max_rounds,
        existing_count = planned_checks.len(),
        target_count = target_count,
        changed_files = changed_files,
        changed_test_files = changed_test_files,
        build = serde_json::to_string_pretty(build).unwrap_or_default(),
        file_reviews = serde_json::to_string_pretty(file_reviews).unwrap_or_default(),
        existing_checks = existing_checks
    )
}

fn build_final_review_prompt(
    options: &ReviewOptions,
    pr: &PullRequestDetails,
    build: &BuildExecution,
    file_reviews: &[FileReviewDraft],
    checks: &[CheckExecution],
    worktree: &Worktree,
) -> String {
    format!(
        "You are writing the final PR review.\n\
         Repo: {repo}\n\
         PR: #{pr_number} {pr_title}\n\
         URL: {pr_url}\n\
         Agent launch cwd: {provider_cwd}\n\
         Worktree: {worktree}\n\
         Provider: {provider}\n\n\
         Build phase result:\n{build}\n\n\
         Per-file reviews:\n{file_reviews}\n\n\
         Executed checks:\n{checks}\n\n\
         Requirements:\n\
         - If you inspect repo state, use the worktree path rather than the launch cwd.\n\
         - Return a compact JSON object with keys: `executive_summary`, `summary_findings`, `inline_comments`, `notes`.\n\
         - In `summary_findings`, `file`, `priority`, `confidence`, `suggested_fix`, and `source_refs` are optional when they are obvious from context.\n\
         - In `inline_comments`, `file` is optional if obvious from context. Use either `line` or `start_line`; `end_line` is optional.\n\
         - Omit fields you are unsure about rather than inventing values.\n\
         - Write a concise executive summary of the real review outcome after considering the checks.\n\
         - Return at most 10 summary findings.\n\
         - Return all inline comments that should actually be left on the PR, deduplicated and filtered against the check results.\n\
         - Prefer line-anchored inline comments when possible. If an issue is real but cannot be tied to an exact line, use a file-level comment with null line numbers.\n\
         - Do not keep speculative comments that were weakened or disproved by the checks.\n\
         - Notes should capture coverage gaps, unresolved uncertainty, or why potentially interesting issues were dropped.",
        repo = options.repo_name,
        pr_number = pr.number,
        pr_title = pr.title,
        pr_url = pr.url,
        provider_cwd = options.provider_cwd.display(),
        worktree = worktree.path.display(),
        provider = "delegated-subprocess",
        build = serde_json::to_string_pretty(build).unwrap_or_default(),
        file_reviews = serde_json::to_string_pretty(file_reviews).unwrap_or_default(),
        checks = serde_json::to_string_pretty(checks).unwrap_or_default()
    )
}

pub fn render_markdown(report: &FinalReviewReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# PR Review: #{} {}\n\n",
        report.pr_number, report.pr_title
    ));
    out.push_str(&format!(
        "- Repo: `{}`\n- Provider: `{}`\n- Worktree: `{}`\n- Run artifacts: `{}`\n\n",
        report.repo, report.provider, report.worktree_path, report.run_artifact_dir
    ));
    out.push_str("## Executive Summary\n\n");
    out.push_str(report.executive_summary.trim());
    out.push_str("\n\n");

    out.push_str("## Build\n\n");
    match &report.build {
        Some(build) => {
            out.push_str(&format!(
                "Status: **{}**\n\n{}\n\n",
                build.status.to_ascii_uppercase(),
                build.summary.trim()
            ));
            if !build.commands_run.is_empty() {
                out.push_str("Commands run:\n");
                for (index, command) in build.commands_run.iter().enumerate() {
                    out.push_str(&format!("{}. `{}`\n", index + 1, command));
                }
                out.push('\n');
            }
            if !build.notes.is_empty() {
                out.push_str("Build notes:\n");
                for note in &build.notes {
                    out.push_str(&format!("- {}\n", note.trim()));
                }
                out.push('\n');
            }
        }
        None => out.push_str("Build phase did not run.\n\n"),
    }

    out.push_str("## Summary Findings\n\n");
    if report.summary_findings.is_empty() {
        out.push_str("No high-confidence summary findings.\n\n");
    } else {
        for (index, finding) in report.summary_findings.iter().enumerate() {
            out.push_str(&format!(
                "{}. [P{}] `{}`: {}\n",
                index + 1,
                finding.priority,
                finding.file,
                finding.title
            ));
            out.push_str(&format!("   Confidence: {:.2}\n", finding.confidence));
            out.push_str(&format!(
                "   Why it matters: {}\n",
                finding.rationale.trim()
            ));
            out.push_str(&format!(
                "   Suggested fix: {}\n",
                finding.suggested_fix.trim()
            ));
            if !finding.source_refs.is_empty() {
                out.push_str(&format!(
                    "   References: {}\n",
                    finding.source_refs.join(", ")
                ));
            }
            out.push('\n');
        }
    }

    out.push_str("## Inline Comments\n\n");
    if report.inline_comments.is_empty() {
        out.push_str("No inline comments.\n\n");
    } else {
        let mut current_file = None::<&str>;
        for comment in &report.inline_comments {
            if current_file != Some(comment.file.as_str()) {
                current_file = Some(comment.file.as_str());
                out.push_str(&format!("### `{}`\n\n", comment.file));
            }
            out.push_str(&format!(
                "- {} [P{}] {} (confidence {:.2})\n",
                line_range_label(comment),
                comment.priority,
                comment.title,
                comment.confidence
            ));
            out.push_str(&format!("  {}\n", comment.body.trim()));
        }
        out.push('\n');
    }

    out.push_str("## Checks\n\n");
    out.push_str(report.checks_summary.trim());
    out.push_str("\n\n");
    if report.checks.is_empty() {
        out.push_str("No checks were executed.\n\n");
    } else {
        for check in &report.checks {
            out.push_str(&format!(
                "{}. [{}] {}\n",
                check.index,
                check.status.to_ascii_uppercase(),
                check.name
            ));
            out.push_str(&format!("   Command: `{}`\n", check.command));
            out.push_str(&format!("   Rationale: {}\n", check.rationale.trim()));
            out.push_str(&format!(
                "   Expected signal: {}\n",
                check.expected_signal.trim()
            ));
            out.push_str(&format!(
                "   Exit code: {:?}, duration: {:.1}s\n",
                check.exit_code, check.duration_secs
            ));
            if !check.related_findings.is_empty() {
                out.push_str(&format!(
                    "   Related findings: {}\n",
                    check.related_findings.join(", ")
                ));
            }
            if !check.stdout_excerpt.trim().is_empty() {
                out.push_str("   Stdout excerpt:\n");
                out.push_str("```text\n");
                out.push_str(check.stdout_excerpt.trim());
                out.push_str("\n```\n");
            }
            if !check.stderr_excerpt.trim().is_empty() {
                out.push_str("   Stderr excerpt:\n");
                out.push_str("```text\n");
                out.push_str(check.stderr_excerpt.trim());
                out.push_str("\n```\n");
            }
            out.push('\n');
        }
    }

    out.push_str("## Per-File Reviews\n\n");
    for review in &report.per_file {
        out.push_str(&format!("### `{}`\n\n", review.file));
        out.push_str(review.summary.trim());
        out.push_str("\n\n");
        if review.findings.is_empty() {
            out.push_str("No retained findings.\n\n");
        } else {
            out.push_str("Findings:\n");
            for finding in &review.findings {
                out.push_str(&format!(
                    "- [P{}] {} (confidence {:.2})\n",
                    finding.priority, finding.title, finding.confidence
                ));
            }
            out.push('\n');
        }
        if review.inline_comments.is_empty() {
            out.push_str("No inline comments proposed for this starting file.\n\n");
        } else {
            out.push_str("Inline comments proposed from this starting file:\n");
            for comment in &review.inline_comments {
                out.push_str(&format!(
                    "- `{}` {} [P{}]\n",
                    comment.file,
                    line_range_label(comment),
                    comment.priority
                ));
                out.push_str(&format!("  {}\n", comment.title.trim()));
            }
            out.push('\n');
        }
    }

    if !report.notes.is_empty() {
        out.push_str("## Notes\n\n");
        for note in &report.notes {
            out.push_str(&format!("- {}\n", note.trim()));
        }
        out.push('\n');
    }

    out
}

fn excerpt(value: &str, max_chars: usize) -> String {
    let chars = value.chars().count();
    if chars <= max_chars {
        return value.trim().to_string();
    }

    let head: String = value.chars().take(max_chars * 2 / 3).collect();
    let tail: String = value
        .chars()
        .rev()
        .take(max_chars / 3)
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    format!("{head}\n...\n{tail}")
}

fn summarize_checks(plan_summary: &str, checks: &[CheckExecution]) -> String {
    let summary = plan_summary.trim();
    if checks.is_empty() {
        if summary.is_empty() {
            return "No post-review checks were executed.".to_string();
        }
        return format!("{summary}\n\nNo post-review checks were executed.");
    }

    let passed = checks
        .iter()
        .filter(|check| check.status == "passed")
        .count();
    let failed = checks
        .iter()
        .filter(|check| check.status == "failed")
        .count();
    let errored = checks
        .iter()
        .filter(|check| check.status == "error")
        .count();

    format!(
        "{}\n\nExecuted {} checks: {} passed, {} failed, {} errored.",
        summary,
        checks.len(),
        passed,
        failed,
        errored
    )
}

fn looks_like_test_file(path: &str) -> bool {
    let lowered = path.to_ascii_lowercase();
    lowered.contains("/test")
        || lowered.contains("/tests/")
        || lowered.contains("__tests__")
        || lowered.contains("spec")
        || lowered.ends_with("_test.py")
        || lowered.ends_with("_test.rs")
        || lowered.ends_with(".spec.ts")
        || lowered.ends_with(".spec.js")
        || lowered.ends_with(".test.ts")
        || lowered.ends_with(".test.js")
}

fn line_range_label(comment: &InlineComment) -> String {
    match (comment.start_line, comment.end_line) {
        (Some(start), Some(end)) if end > start => format!("L{start}-L{end}"),
        (Some(start), _) => format!("L{start}"),
        _ => "file-level".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CheckPlanDraft, CheckSpec, fallback_check_plan_summary,
        is_duplicate_check, summarize_checks, validate_check_plan,
        wrap_check_command_for_execution,
    };
    use crate::types::{ChangedFile, PullRequestDetails};
    use std::path::Path;

    #[test]
    fn validate_check_plan_allows_zero_checks() {
        let plan = CheckPlanDraft {
            summary: fallback_check_plan_summary(0),
            checks: Vec::new(),
        };

        validate_check_plan(&plan).expect("empty check plan should be allowed");
    }

    #[test]
    fn duplicate_checks_are_detected_by_command() {
        let existing = vec![CheckSpec {
            name: "Run focused test".to_string(),
            command: "python -m pytest test/dynamo/test_exc.py -k source_location".to_string(),
            rationale: String::new(),
            expected_signal: String::new(),
            related_findings: Vec::new(),
        }];
        let candidate = CheckSpec {
            name: "Same command, different spacing".to_string(),
            command: "python   -m   pytest test/dynamo/test_exc.py -k source_location".to_string(),
            rationale: String::new(),
            expected_signal: String::new(),
            related_findings: Vec::new(),
        };

        assert!(is_duplicate_check(&candidate, &existing));
    }

    #[test]
    fn summarize_checks_handles_zero_checks() {
        let summary = summarize_checks("Planner found no safe follow-up checks.", &[]);
        assert!(summary.contains("No post-review checks were executed."));
    }

    #[test]
    fn wraps_check_commands_with_pipefail() {
        let wrapped = wrap_check_command_for_execution("python -m pytest test_file.py | tail -20");
        assert!(wrapped.starts_with("set -o pipefail"));
        assert!(wrapped.contains("python -m pytest test_file.py | tail -20"));
    }
}
