use schemars::JsonSchema;
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestDetails {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub body: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub files: Vec<ChangedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReviewJob {
    pub file: String,
    pub additions: u64,
    pub deletions: u64,
    pub diff_excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReviewFinding {
    #[serde(default)]
    pub file: String,
    #[serde(default, alias = "label", alias = "summary")]
    pub title: String,
    #[serde(default = "default_priority")]
    pub priority: u8,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    #[serde(default, alias = "body", alias = "reason", alias = "why")]
    pub rationale: String,
    #[serde(default, alias = "fix", alias = "suggestion")]
    pub suggested_fix: String,
    #[serde(
        default,
        alias = "references",
        deserialize_with = "deserialize_string_list"
    )]
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InlineComment {
    pub file: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub title: String,
    pub priority: u8,
    pub confidence: f32,
    pub body: String,
}

#[derive(Debug, Default, Deserialize)]
struct InlineCommentDraft {
    #[serde(default)]
    file: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    line: Option<usize>,
    #[serde(default)]
    line_number: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    label: String,
    #[serde(default = "default_priority")]
    priority: u8,
    #[serde(default = "default_confidence")]
    confidence: f32,
    #[serde(default)]
    body: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    rationale: String,
}

impl<'de> Deserialize<'de> for InlineComment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let draft = InlineCommentDraft::deserialize(deserializer)?;
        Ok(Self {
            file: first_nonempty_string([draft.file, draft.path]),
            start_line: draft.start_line.or(draft.line).or(draft.line_number),
            end_line: draft.end_line,
            title: first_nonempty_string([draft.title, draft.summary, draft.label]),
            priority: draft.priority,
            confidence: draft.confidence,
            body: first_nonempty_string([
                draft.body,
                draft.comment,
                draft.message,
                draft.rationale,
            ]),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileReviewDraft {
    #[serde(default)]
    pub file: String,
    #[serde(default, alias = "executive_summary")]
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    #[serde(default)]
    pub inline_comments: Vec<InlineComment>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FinalReviewDraft {
    #[serde(default, alias = "summary")]
    pub executive_summary: String,
    #[serde(default, alias = "findings")]
    pub summary_findings: Vec<ReviewFinding>,
    #[serde(default)]
    pub inline_comments: Vec<InlineComment>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BuildExecution {
    #[serde(default = "default_build_status")]
    pub status: String,
    #[serde(default, alias = "result")]
    pub summary: String,
    #[serde(
        default,
        alias = "commands",
        deserialize_with = "deserialize_string_list"
    )]
    pub commands_run: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckSpec {
    #[serde(default, alias = "title", alias = "label", alias = "summary")]
    pub name: String,
    #[serde(default, alias = "cmd", alias = "run", alias = "script")]
    pub command: String,
    #[serde(default, alias = "why", alias = "reason", alias = "description")]
    pub rationale: String,
    #[serde(
        default,
        alias = "expected",
        alias = "signal",
        alias = "success_criteria"
    )]
    pub expected_signal: String,
    #[serde(
        default,
        alias = "related",
        deserialize_with = "deserialize_string_list"
    )]
    pub related_findings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckPlanDraft {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub checks: Vec<CheckSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckGenerationDraft {
    #[serde(default)]
    pub summary: String,
    #[serde(default, alias = "complete", alias = "finished", alias = "stop")]
    pub done: bool,
    #[serde(default, alias = "next_check", alias = "candidate")]
    pub check: Option<CheckSpec>,
    #[serde(default)]
    pub checks: Vec<CheckSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckExecution {
    pub index: usize,
    pub name: String,
    pub command: String,
    pub rationale: String,
    pub expected_signal: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub related_findings: Vec<String>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub duration_secs: f32,
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FinalReviewReport {
    pub repo: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub provider: String,
    pub worktree_path: String,
    pub run_artifact_dir: String,
    pub executive_summary: String,
    pub build: Option<BuildExecution>,
    pub summary_findings: Vec<ReviewFinding>,
    pub inline_comments: Vec<InlineComment>,
    pub checks_summary: String,
    pub per_file: Vec<FileReviewDraft>,
    pub checks: Vec<CheckExecution>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub notes: Vec<String>,
}

pub fn sort_findings(findings: &mut [ReviewFinding]) {
    findings.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| {
                right
                    .confidence
                    .partial_cmp(&left.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.title.cmp(&right.title))
    });
}

pub fn sort_inline_comments(comments: &mut [InlineComment]) {
    comments.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then_with(|| left.start_line.cmp(&right.start_line))
            .then_with(|| left.end_line.cmp(&right.end_line))
            .then_with(|| left.priority.cmp(&right.priority))
            .then_with(|| {
                right
                    .confidence
                    .partial_cmp(&left.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.title.cmp(&right.title))
    });
}

fn default_priority() -> u8 {
    2
}

fn default_confidence() -> f32 {
    0.7
}

fn default_build_status() -> String {
    "failed".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StringList {
    One(String),
    Many(Vec<String>),
}

fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<StringList>::deserialize(deserializer)?;
    Ok(match value {
        Some(StringList::One(value)) => normalize_string_list(vec![value]),
        Some(StringList::Many(values)) => normalize_string_list(values),
        None => Vec::new(),
    })
}

fn normalize_string_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn first_nonempty_string<const N: usize>(values: [String; N]) -> String {
    values
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        BuildExecution, CheckPlanDraft, CheckSpec, FileReviewDraft, InlineComment, ReviewFinding,
    };

    #[test]
    fn deserializes_file_review_without_top_level_file() {
        let value = json!({
            "summary": "Looks fine overall.",
            "findings": [],
            "inline_comments": [],
            "notes": []
        });

        let parsed: FileReviewDraft = serde_json::from_value(value).expect("should deserialize");
        assert_eq!(parsed.file, "");
        assert_eq!(parsed.summary, "Looks fine overall.");
    }

    #[test]
    fn deserializes_inline_comment_line_alias() {
        let value = json!({
            "title": "Use a tuple append here",
            "line": 17,
            "comment": "This should stay line-anchored."
        });

        let parsed: InlineComment = serde_json::from_value(value).expect("should deserialize");
        assert_eq!(parsed.start_line, Some(17));
        assert_eq!(parsed.body, "This should stay line-anchored.");
    }

    #[test]
    fn deserializes_inline_comment_with_duplicate_alias_fields() {
        let value = json!({
            "file": "torch/_dynamo/utils.py",
            "path": "ignored.py",
            "start_line": 21,
            "line": 22,
            "line_number": 23,
            "title": "Keep canonical title",
            "summary": "ignored title",
            "body": "Keep canonical body",
            "comment": "ignored body"
        });

        let parsed: InlineComment = serde_json::from_value(value).expect("should deserialize");
        assert_eq!(parsed.file, "torch/_dynamo/utils.py");
        assert_eq!(parsed.start_line, Some(21));
        assert_eq!(parsed.title, "Keep canonical title");
        assert_eq!(parsed.body, "Keep canonical body");
    }

    #[test]
    fn deserializes_build_execution_with_defaults() {
        let value = json!({
            "summary": "Build could not run in this environment."
        });

        let parsed: BuildExecution = serde_json::from_value(value).expect("should deserialize");
        assert_eq!(parsed.status, "failed");
        assert_eq!(parsed.summary, "Build could not run in this environment.");
        assert!(parsed.commands_run.is_empty());
    }

    #[test]
    fn deserializes_build_execution_with_scalar_lists() {
        let value = json!({
            "status": "passed",
            "summary": "Completed build.",
            "commands_run": "python setup.py develop",
            "notes": "Imports succeeded afterward."
        });

        let parsed: BuildExecution = serde_json::from_value(value).expect("should deserialize");
        assert_eq!(parsed.commands_run, vec!["python setup.py develop"]);
        assert_eq!(parsed.notes, vec!["Imports succeeded afterward."]);
    }

    #[test]
    fn deserializes_file_review_notes_string() {
        let value = json!({
            "summary": "Looks fine overall.",
            "notes": "One follow-up note."
        });

        let parsed: FileReviewDraft = serde_json::from_value(value).expect("should deserialize");
        assert_eq!(parsed.notes, vec!["One follow-up note."]);
    }

    #[test]
    fn deserializes_other_scalar_string_lists() {
        let finding_value = json!({
            "title": "Missing assertion",
            "references": "test/test_file.py"
        });
        let finding: ReviewFinding =
            serde_json::from_value(finding_value).expect("finding should deserialize");
        assert_eq!(finding.source_refs, vec!["test/test_file.py"]);

        let check_value = json!({
            "name": "Run focused test",
            "command": "pytest test/test_file.py -k case",
            "related": "Missing assertion"
        });
        let check: CheckSpec =
            serde_json::from_value(check_value).expect("check should deserialize");
        assert_eq!(check.related_findings, vec!["Missing assertion"]);
    }

    #[test]
    fn deserializes_check_spec_common_aliases() {
        let value = json!({
            "title": "Run focused test",
            "cmd": "pytest test/test_file.py -k case",
            "description": "Exercise the changed path.",
            "signal": "Command passes."
        });

        let parsed: CheckSpec = serde_json::from_value(value).expect("should deserialize");
        assert_eq!(parsed.name, "Run focused test");
        assert_eq!(parsed.command, "pytest test/test_file.py -k case");
        assert_eq!(parsed.rationale, "Exercise the changed path.");
        assert_eq!(parsed.expected_signal, "Command passes.");
    }

    #[test]
    fn deserializes_check_plan_with_missing_names() {
        let value = json!({
            "summary": "Plan checks.",
            "checks": [
                {
                    "command": "pytest test/test_file.py -k case"
                }
            ]
        });

        let parsed: CheckPlanDraft = serde_json::from_value(value).expect("should deserialize");
        assert_eq!(parsed.checks.len(), 1);
        assert_eq!(parsed.checks[0].name, "");
    }
}
