use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use crate::types::{ChecksStatus, PrState, PrStatus, ReviewDecision};

#[derive(Clone)]
struct RepoIdentity {
    owner: String,
    name: String,
}

static REPO_CACHE: Mutex<Option<HashMap<PathBuf, RepoIdentity>>> = Mutex::new(None);

const PR_FIELDS: &str = r#"
    number url title state isDraft headRefName
    author { login }
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

static GITHUB_USER: OnceLock<Option<String>> = OnceLock::new();

fn parse_repo_identity(json_str: &str) -> Option<RepoIdentity> {
    let parsed: serde_json::Value = serde_json::from_str(json_str.trim()).ok()?;
    let owner = parsed.get("owner")?.get("login")?.as_str()?.to_string();
    let name = parsed.get("name")?.as_str()?.to_string();
    Some(RepoIdentity { owner, name })
}

fn get_repo_identity(main_worktree: &Path) -> Option<RepoIdentity> {
    let canonical =
        std::fs::canonicalize(main_worktree).unwrap_or_else(|_| main_worktree.to_path_buf());

    let mut cache = REPO_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = cache.get_or_insert_with(HashMap::new);

    if let Some(identity) = map.get(&canonical) {
        return Some(identity.clone());
    }

    let output = Command::new("gh")
        .args(["repo", "view", "--json", "owner,name"])
        .current_dir(main_worktree)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let identity = parse_repo_identity(&stdout)?;
    map.insert(canonical, identity.clone());
    Some(identity)
}

pub fn fetch_prs(main_worktree: &Path) -> Vec<PrStatus> {
    let Some(repo) = get_repo_identity(main_worktree) else {
        return Vec::new();
    };

    let query = format!(
        r#"query($owner: String!, $repo: String!) {{
            repository(owner: $owner, name: $repo) {{
                pullRequests(states: [OPEN, MERGED, CLOSED], first: 100, orderBy: {{field: UPDATED_AT, direction: DESC}}) {{
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
    let title = node.get("title")?.as_str()?.to_string();
    let url = node.get("url")?.as_str()?.to_string();
    let author = node
        .pointer("/author/login")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
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
        title,
        url,
        author,
        state,
        is_draft,
        checks,
        review,
        additions,
        deletions,
        head_branch,
    })
}

pub fn fetch_user_prs(main_worktree: &Path) -> Vec<PrStatus> {
    let Some(repo) = get_repo_identity(main_worktree) else {
        return Vec::new();
    };
    let Some(user) = fetch_current_user(main_worktree) else {
        return Vec::new();
    };

    let search_query = format!(
        "repo:{}/{} is:pr is:open author:{}",
        repo.owner, repo.name, user
    );

    let query = format!(
        r#"query($search: String!) {{
            search(query: $search, type: ISSUE, first: 50) {{
                nodes {{
                    ... on PullRequest {{
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
            &format!("search={search_query}"),
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
    parse_search_response(&stdout)
}

fn parse_search_response(json_str: &str) -> Vec<PrStatus> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return Vec::new();
    };

    let Some(nodes) = parsed
        .pointer("/data/search/nodes")
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };

    nodes.iter().filter_map(parse_pr_node).collect()
}

pub fn fetch_current_user(main_worktree: &Path) -> Option<String> {
    GITHUB_USER
        .get_or_init(|| {
            let output = Command::new("gh")
                .args(["api", "user", "-q", ".login"])
                .current_dir(main_worktree)
                .output()
                .ok()?;

            if !output.status.success() {
                return None;
            }

            let login = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if login.is_empty() {
                None
            } else {
                Some(login)
            }
        })
        .clone()
}

pub fn open_pr_in_browser(pr_number: u32, main_worktree: &Path) {
    let _ = Command::new("gh")
        .args(["pr", "view", &pr_number.to_string(), "--web"])
        .current_dir(main_worktree)
        .output();
}

pub fn index_by_branch(prs: Vec<PrStatus>) -> HashMap<String, PrStatus> {
    prs.into_iter()
        .map(|pr| (pr.head_branch.clone(), pr))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pr_node(overrides: &str) -> serde_json::Value {
        let base = r#"{
            "number": 42,
            "title": "Fix the thing",
            "url": "https://github.com/test/repo/pull/42",
            "author": { "login": "testuser" },
            "state": "OPEN",
            "isDraft": false,
            "headRefName": "feature/my-branch",
            "reviewDecision": "APPROVED",
            "additions": 10,
            "deletions": 3,
            "commits": {
                "nodes": [{
                    "commit": {
                        "statusCheckRollup": { "state": "SUCCESS" }
                    }
                }]
            }
        }"#;
        let mut value: serde_json::Value = serde_json::from_str(base).unwrap();
        if !overrides.is_empty() {
            let overrides: serde_json::Value = serde_json::from_str(overrides).unwrap();
            if let (Some(base_obj), Some(over_obj)) = (value.as_object_mut(), overrides.as_object())
            {
                for (k, v) in over_obj {
                    base_obj.insert(k.clone(), v.clone());
                }
            }
        }
        value
    }

    fn wrap_in_response(nodes: Vec<serde_json::Value>) -> String {
        let response = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequests": {
                        "nodes": nodes
                    }
                }
            }
        });
        serde_json::to_string(&response).unwrap()
    }

    // --- parse_pr_node ---

    #[test]
    fn test_parse_full_pr_node() {
        let node = make_pr_node("");
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.title, "Fix the thing");
        assert_eq!(pr.url, "https://github.com/test/repo/pull/42");
        assert_eq!(pr.author, "testuser");
        assert_eq!(pr.state, PrState::Open);
        assert!(!pr.is_draft);
        assert_eq!(pr.head_branch, "feature/my-branch");
        assert_eq!(pr.checks, Some(ChecksStatus::Success));
        assert_eq!(pr.review, Some(ReviewDecision::Approved));
        assert_eq!(pr.additions, 10);
        assert_eq!(pr.deletions, 3);
    }

    #[test]
    fn test_parse_draft_pr() {
        let node = make_pr_node(r#"{"isDraft": true}"#);
        let pr = parse_pr_node(&node).unwrap();
        assert!(pr.is_draft);
    }

    #[test]
    fn test_parse_merged_pr() {
        let node = make_pr_node(r#"{"state": "MERGED"}"#);
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.state, PrState::Merged);
    }

    #[test]
    fn test_parse_closed_pr() {
        let node = make_pr_node(r#"{"state": "CLOSED"}"#);
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.state, PrState::Closed);
    }

    #[test]
    fn test_parse_unknown_state_returns_none() {
        let node = make_pr_node(r#"{"state": "BOGUS"}"#);
        assert!(parse_pr_node(&node).is_none());
    }

    #[test]
    fn test_parse_checks_failure() {
        let node = make_pr_node(
            r#"{"commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]}}"#,
        );
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.checks, Some(ChecksStatus::Failure));
    }

    #[test]
    fn test_parse_checks_pending() {
        let node = make_pr_node(
            r#"{"commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "PENDING"}}}]}}"#,
        );
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.checks, Some(ChecksStatus::Pending));
    }

    #[test]
    fn test_parse_checks_expected_maps_to_pending() {
        let node = make_pr_node(
            r#"{"commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "EXPECTED"}}}]}}"#,
        );
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.checks, Some(ChecksStatus::Pending));
    }

    #[test]
    fn test_parse_checks_error() {
        let node = make_pr_node(
            r#"{"commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "ERROR"}}}]}}"#,
        );
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.checks, Some(ChecksStatus::Error));
    }

    #[test]
    fn test_parse_no_checks_null_rollup() {
        let node =
            make_pr_node(r#"{"commits": {"nodes": [{"commit": {"statusCheckRollup": null}}]}}"#);
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.checks, None);
    }

    #[test]
    fn test_parse_no_checks_empty_commits() {
        let node = make_pr_node(r#"{"commits": {"nodes": []}}"#);
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.checks, None);
    }

    #[test]
    fn test_parse_no_checks_missing_commits() {
        let mut node = make_pr_node("");
        node.as_object_mut().unwrap().remove("commits");
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.checks, None);
    }

    #[test]
    fn test_parse_review_changes_requested() {
        let node = make_pr_node(r#"{"reviewDecision": "CHANGES_REQUESTED"}"#);
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.review, Some(ReviewDecision::ChangesRequested));
    }

    #[test]
    fn test_parse_review_required() {
        let node = make_pr_node(r#"{"reviewDecision": "REVIEW_REQUIRED"}"#);
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.review, Some(ReviewDecision::ReviewRequired));
    }

    #[test]
    fn test_parse_review_null() {
        let node = make_pr_node(r#"{"reviewDecision": null}"#);
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.review, None);
    }

    #[test]
    fn test_parse_missing_number_returns_none() {
        let mut node = make_pr_node("");
        node.as_object_mut().unwrap().remove("number");
        assert!(parse_pr_node(&node).is_none());
    }

    #[test]
    fn test_parse_missing_state_returns_none() {
        let mut node = make_pr_node("");
        node.as_object_mut().unwrap().remove("state");
        assert!(parse_pr_node(&node).is_none());
    }

    #[test]
    fn test_parse_missing_head_ref_returns_none() {
        let mut node = make_pr_node("");
        node.as_object_mut().unwrap().remove("headRefName");
        assert!(parse_pr_node(&node).is_none());
    }

    #[test]
    fn test_parse_zero_additions_deletions() {
        let node = make_pr_node(r#"{"additions": 0, "deletions": 0}"#);
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.additions, 0);
        assert_eq!(pr.deletions, 0);
    }

    #[test]
    fn test_parse_missing_additions_deletions_defaults_to_zero() {
        let mut node = make_pr_node("");
        let obj = node.as_object_mut().unwrap();
        obj.remove("additions");
        obj.remove("deletions");
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.additions, 0);
        assert_eq!(pr.deletions, 0);
    }

    #[test]
    fn test_parse_is_draft_missing_defaults_to_false() {
        let mut node = make_pr_node("");
        node.as_object_mut().unwrap().remove("isDraft");
        let pr = parse_pr_node(&node).unwrap();
        assert!(!pr.is_draft);
    }

    // --- parse_graphql_response ---

    #[test]
    fn test_parse_response_multiple_prs() {
        let nodes = vec![
            make_pr_node(r#"{"number": 1, "headRefName": "branch-a"}"#),
            make_pr_node(r#"{"number": 2, "headRefName": "branch-b"}"#),
        ];
        let response = wrap_in_response(nodes);
        let prs = parse_graphql_response(&response);
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 1);
        assert_eq!(prs[1].number, 2);
    }

    #[test]
    fn test_parse_response_empty_nodes() {
        let response = wrap_in_response(vec![]);
        let prs = parse_graphql_response(&response);
        assert!(prs.is_empty());
    }

    #[test]
    fn test_parse_response_invalid_json() {
        let prs = parse_graphql_response("not json at all {{{");
        assert!(prs.is_empty());
    }

    #[test]
    fn test_parse_response_missing_data_path() {
        let prs = parse_graphql_response(r#"{"data": {}}"#);
        assert!(prs.is_empty());
    }

    #[test]
    fn test_parse_response_null_nodes() {
        let response = r#"{"data": {"repository": {"pullRequests": {"nodes": null}}}}"#;
        let prs = parse_graphql_response(response);
        assert!(prs.is_empty());
    }

    #[test]
    fn test_parse_response_partial_failure_skips_bad_nodes() {
        let good = make_pr_node(r#"{"number": 1}"#);
        let mut bad = make_pr_node("");
        bad.as_object_mut().unwrap().remove("number"); // makes it unparseable
        let nodes = vec![good, bad, make_pr_node(r#"{"number": 3}"#)];
        let response = wrap_in_response(nodes);
        let prs = parse_graphql_response(&response);
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 1);
        assert_eq!(prs[1].number, 3);
    }

    #[test]
    fn test_parse_response_graphql_error() {
        let response = r#"{"errors": [{"message": "Something went wrong"}]}"#;
        let prs = parse_graphql_response(response);
        assert!(prs.is_empty());
    }

    // --- parse_repo_identity ---

    #[test]
    fn test_parse_repo_identity_valid() {
        let json = r#"{"owner": {"login": "octocat"}, "name": "hello-world"}"#;
        let id = parse_repo_identity(json).unwrap();
        assert_eq!(id.owner, "octocat");
        assert_eq!(id.name, "hello-world");
    }

    #[test]
    fn test_parse_repo_identity_missing_owner() {
        let json = r#"{"name": "hello-world"}"#;
        assert!(parse_repo_identity(json).is_none());
    }

    #[test]
    fn test_parse_repo_identity_missing_name() {
        let json = r#"{"owner": {"login": "octocat"}}"#;
        assert!(parse_repo_identity(json).is_none());
    }

    #[test]
    fn test_parse_repo_identity_invalid_json() {
        assert!(parse_repo_identity("not json").is_none());
    }

    #[test]
    fn test_parse_repo_identity_empty_string() {
        assert!(parse_repo_identity("").is_none());
    }

    #[test]
    fn test_parse_repo_identity_owner_not_object() {
        let json = r#"{"owner": "octocat", "name": "hello-world"}"#;
        assert!(parse_repo_identity(json).is_none());
    }

    // --- index_by_branch ---

    #[test]
    fn test_index_empty() {
        let map = index_by_branch(vec![]);
        assert!(map.is_empty());
    }

    fn test_pr_status(number: u32, branch: &str) -> PrStatus {
        PrStatus {
            number,
            title: format!("PR #{}", number),
            url: format!("https://github.com/test/repo/pull/{}", number),
            author: "testuser".into(),
            state: PrState::Open,
            is_draft: false,
            checks: None,
            review: None,
            additions: 0,
            deletions: 0,
            head_branch: branch.into(),
        }
    }

    #[test]
    fn test_index_single_pr() {
        let pr = test_pr_status(42, "feature/foo");
        let map = index_by_branch(vec![pr]);
        assert_eq!(map.len(), 1);
        assert_eq!(map["feature/foo"].number, 42);
    }

    #[test]
    fn test_index_multiple_prs() {
        let prs = vec![test_pr_status(1, "branch-a"), test_pr_status(2, "branch-b")];
        let map = index_by_branch(prs);
        assert_eq!(map.len(), 2);
        assert_eq!(map["branch-a"].number, 1);
        assert_eq!(map["branch-b"].number, 2);
    }

    #[test]
    fn test_index_duplicate_branch_last_wins() {
        let prs = vec![
            test_pr_status(1, "same-branch"),
            test_pr_status(2, "same-branch"),
        ];
        let map = index_by_branch(prs);
        assert_eq!(map.len(), 1);
        assert_eq!(map["same-branch"].number, 2);
    }
}
