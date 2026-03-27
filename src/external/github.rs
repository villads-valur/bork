use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use crate::types::{ChecksStatus, PrState, PrStatus, ReviewDecision};

struct RepoIdentity {
    owner: String,
    name: String,
}

static REPO_IDENTITY: OnceLock<Option<RepoIdentity>> = OnceLock::new();

const PR_FIELDS: &str = r#"
    number url title state isDraft headRefName
    reviewDecision
    additions deletions
    commits(last: 1) {
        nodes {
            commit {
                statusCheckRollup { state }
            }
        }
    }
"#;

fn get_repo_identity(main_worktree: &Path) -> Option<&'static RepoIdentity> {
    REPO_IDENTITY
        .get_or_init(|| {
            let output = Command::new("gh")
                .args(["repo", "view", "--json", "owner,name"])
                .current_dir(main_worktree)
                .output()
                .ok()?;

            if !output.status.success() {
                return None;
            }

            let stdout = String::from_utf8_lossy(&output.stdout);
            let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
            let owner = parsed.get("owner")?.get("login")?.as_str()?.to_string();
            let name = parsed.get("name")?.as_str()?.to_string();

            Some(RepoIdentity { owner, name })
        })
        .as_ref()
}

pub fn fetch_open_prs(main_worktree: &Path) -> Vec<PrStatus> {
    let Some(repo) = get_repo_identity(main_worktree) else {
        return Vec::new();
    };

    let query = format!(
        r#"query($owner: String!, $repo: String!) {{
            repository(owner: $owner, name: $repo) {{
                pullRequests(states: OPEN, first: 100, orderBy: {{field: UPDATED_AT, direction: DESC}}) {{
                    nodes {{
                        {PR_FIELDS}
                    }}
                }}
            }}
        }}"#
    );

    let output = Command::new("gh")
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-f",
            &format!("owner={}", repo.owner),
            "-f",
            &format!("repo={}", repo.name),
        ])
        .current_dir(main_worktree)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_graphql_response(&stdout)
}

fn parse_graphql_response(json_str: &str) -> Vec<PrStatus> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return Vec::new();
    };

    let Some(nodes) = parsed
        .pointer("/data/repository/pullRequests/nodes")
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };

    nodes.iter().filter_map(parse_pr_node).collect()
}

fn parse_pr_node(node: &serde_json::Value) -> Option<PrStatus> {
    let number = node.get("number")?.as_u64()? as u32;
    let state_str = node.get("state")?.as_str()?;
    let is_draft = node
        .get("isDraft")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let head_branch = node.get("headRefName")?.as_str()?.to_string();

    let state = match state_str {
        "OPEN" => PrState::Open,
        "CLOSED" => PrState::Closed,
        "MERGED" => PrState::Merged,
        _ => return None,
    };

    let checks = node
        .pointer("/commits/nodes/0/commit/statusCheckRollup/state")
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "SUCCESS" => Some(ChecksStatus::Success),
            "FAILURE" => Some(ChecksStatus::Failure),
            "PENDING" | "EXPECTED" => Some(ChecksStatus::Pending),
            "ERROR" => Some(ChecksStatus::Error),
            _ => None,
        });

    let review = node
        .get("reviewDecision")
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "APPROVED" => Some(ReviewDecision::Approved),
            "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
            "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
            _ => None,
        });

    let additions = node.get("additions").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let deletions = node.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

    Some(PrStatus {
        number,
        state,
        is_draft,
        checks,
        review,
        additions,
        deletions,
        head_branch,
    })
}

pub fn open_pr_in_browser(pr_number: u32, main_worktree: &Path) {
    let _ = Command::new("gh")
        .args(["pr", "view", &pr_number.to_string(), "--web"])
        .current_dir(main_worktree)
        .output();
}

/// Convert the raw Vec<PrStatus> into a HashMap keyed by branch name for O(1) lookup.
pub fn index_by_branch(prs: Vec<PrStatus>) -> HashMap<String, PrStatus> {
    let mut map = HashMap::with_capacity(prs.len());
    for pr in prs {
        map.insert(pr.head_branch.clone(), pr);
    }
    map
}
