use anyhow::{Result, bail};
use regex::Regex;

#[derive(Debug, Clone)]
pub struct RequestSpec {
    pub pr_number: u64,
    pub repo_name: Option<String>,
}

pub fn resolve_request(pr_arg: &str, explicit_repo: Option<&str>) -> Result<RequestSpec> {
    let value = pr_arg.trim();
    let parsed = if let Ok(pr_number) = value.parse::<u64>() {
        RequestSpec {
            pr_number,
            repo_name: explicit_repo.map(ToOwned::to_owned),
        }
    } else if let Some(reference) = parse_pr_reference(value) {
        RequestSpec {
            pr_number: reference.pr_number,
            repo_name: explicit_repo
                .map(ToOwned::to_owned)
                .or(Some(reference.repo_name)),
        }
    } else {
        bail!(
            "could not parse --pr value `{value}`. Pass a PR number like `180747` or a full URL like `https://github.com/pytorch/pytorch/pull/180747`."
        );
    };

    Ok(parsed)
}

#[derive(Debug)]
struct ParsedReference {
    repo_name: String,
    pr_number: u64,
}

fn parse_pr_reference(prompt: &str) -> Option<ParsedReference> {
    parse_github_pull_url(prompt).or_else(|| parse_repo_issue_reference(prompt))
}

fn parse_github_pull_url(prompt: &str) -> Option<ParsedReference> {
    let regex = Regex::new(
        r"https?://github\.com/(?P<owner>[A-Za-z0-9_.-]+)/(?P<repo>[A-Za-z0-9_.-]+)/pull/(?P<pr>\d+)",
    )
    .ok()?;
    let captures = regex.captures(prompt)?;
    let repo_name = format!(
        "{}/{}",
        captures.name("owner")?.as_str(),
        captures.name("repo")?.as_str()
    );
    let pr_number = captures.name("pr")?.as_str().parse().ok()?;
    Some(ParsedReference {
        repo_name,
        pr_number,
    })
}

fn parse_repo_issue_reference(prompt: &str) -> Option<ParsedReference> {
    let regex =
        Regex::new(r"(?P<owner>[A-Za-z0-9_.-]+)/(?P<repo>[A-Za-z0-9_.-]+)#(?P<pr>\d+)").ok()?;
    let captures = regex.captures(prompt)?;
    let repo_name = format!(
        "{}/{}",
        captures.name("owner")?.as_str(),
        captures.name("repo")?.as_str()
    );
    let pr_number = captures.name("pr")?.as_str().parse().ok()?;
    Some(ParsedReference {
        repo_name,
        pr_number,
    })
}

#[cfg(test)]
mod tests {
    use super::resolve_request;

    #[test]
    fn parses_prompt_with_pr_url() {
        let request = resolve_request("https://github.com/pytorch/pytorch/pull/180747", None)
            .expect("request should parse");

        assert_eq!(request.pr_number, 180747);
        assert_eq!(request.repo_name.as_deref(), Some("pytorch/pytorch"));
    }

    #[test]
    fn explicit_repo_overrides_url_repo_when_present() {
        let request = resolve_request(
            "https://github.com/pytorch/pytorch/pull/180747",
            Some("openai/codex"),
        )
        .expect("request should parse");

        assert_eq!(request.pr_number, 180747);
        assert_eq!(request.repo_name.as_deref(), Some("openai/codex"));
    }
}
