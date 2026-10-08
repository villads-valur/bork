use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use crate::types::{
    ChecksStatus, GithubStack, GithubStackPullRequest, PrState, PrStatus, ReviewDecision,
};

#[derive(Clone)]
struct RepoIdentity {
    owner: String,
    name: String,
}

/// Single-flight state for one cache key: either a fetch is in progress, or a
/// value is cached. A missing entry means "cold, nobody is fetching yet".
enum FetchState<T> {
    InFlight,
    Ready(T),
}

/// Process-global repo-identity cache with per-path single-flight. Concurrent
/// cold-cache callers for the same path wait on `REPO_WAIT` for the one
/// in-flight `gh repo view` instead of each spawning their own.
static REPO_CACHE: Mutex<Option<HashMap<PathBuf, FetchState<RepoIdentity>>>> = Mutex::new(None);
static REPO_WAIT: Condvar = Condvar::new();

const PR_FIELDS: &str = r#"
    number url title state isDraft headRefName
    isCrossRepository
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

/// Process-global viewer-login cache with single-flight. Concurrent cold-cache
/// callers wait on `GITHUB_USER_WAIT` for the one in-flight `gh api user`.
static GITHUB_USER: Mutex<Option<FetchState<String>>> = Mutex::new(None);
static GITHUB_USER_WAIT: Condvar = Condvar::new();

fn parse_repo_identity(json_str: &str) -> Option<RepoIdentity> {
    let parsed: serde_json::Value = serde_json::from_str(json_str.trim()).ok()?;
    let owner = parsed.get("owner")?.get("login")?.as_str()?.to_string();
    let name = parsed.get("name")?.as_str()?.to_string();
    Some(RepoIdentity { owner, name })
}

fn get_repo_identity(main_worktree: &Path) -> Result<RepoIdentity, String> {
    let canonical =
        std::fs::canonicalize(main_worktree).unwrap_or_else(|_| main_worktree.to_path_buf());

    // Claim the fetch or wait for an in-flight one. We never hold the lock
    // across the `gh` call: a hung network request would otherwise block every
    // PR worker in every project.
    {
        let mut cache = REPO_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match cache.get_or_insert_with(HashMap::new).get(&canonical) {
                Some(FetchState::Ready(identity)) => return Ok(identity.clone()),
                Some(FetchState::InFlight) => {
                    // Another thread is fetching this path; wait for it, then
                    // re-check (it may have produced a value or given up).
                    cache = REPO_WAIT.wait(cache).unwrap_or_else(|e| e.into_inner());
                }
                None => {
                    // We are the fetcher.
                    cache
                        .get_or_insert_with(HashMap::new)
                        .insert(canonical.clone(), FetchState::InFlight);
                    break;
                }
            }
        }
    }

    // We hold the (logical) fetch claim. Run `gh` without the lock, and make
    // sure to clear the in-flight marker and wake waiters on every exit path.
    let identity = fetch_repo_identity(main_worktree);

    let mut cache = REPO_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = cache.get_or_insert_with(HashMap::new);
    match &identity {
        Ok(id) => {
            map.insert(canonical, FetchState::Ready(id.clone()));
        }
        Err(_) => {
            // Failed: drop the marker so a later poll retries (matches the
            // previous behaviour of not caching failures).
            map.remove(&canonical);
        }
    }
    REPO_WAIT.notify_all();
    identity
}

fn checked_gh_output(output: std::io::Result<std::process::Output>) -> Result<String, String> {
    let output = output.map_err(|error| format!("Could not run gh: {error}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(message
            .lines()
            .next()
            .unwrap_or("GitHub request failed")
            .to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn fetch_repo_identity(main_worktree: &Path) -> Result<RepoIdentity, String> {
    let stdout = checked_gh_output(
        crate::external::gh_command()
            .args(["repo", "view", "--json", "owner,name"])
            .current_dir(main_worktree)
            .output(),
    )?;
    parse_repo_identity(&stdout).ok_or_else(|| "Invalid GitHub repository response".to_string())
}

pub fn fetch_prs(main_worktree: &Path) -> Result<Vec<PrStatus>, String> {
    let repo = get_repo_identity(main_worktree)?;

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

    let output = crate::external::gh_command()
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

    let stdout = checked_gh_output(output)?;
    Ok(parse_graphql_response(&stdout))
}

pub fn fetch_missing_prs(
    main_worktree: &Path,
    requested: &[u32],
    prs: &mut Vec<PrStatus>,
) -> Result<(), String> {
    let repo = get_repo_identity(main_worktree)?;
    let known: std::collections::HashSet<_> = prs.iter().map(|pr| pr.number).collect();
    let mut missing: Vec<_> = requested
        .iter()
        .copied()
        .filter(|number| !known.contains(number))
        .collect();
    missing.sort_unstable();
    missing.dedup();
    for chunk in missing.chunks(50) {
        let fields = chunk
            .iter()
            .map(|number| format!("pr{number}: pullRequest(number: {number}) {{ {PR_FIELDS} }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let query = format!("query($owner: String!, $repo: String!) {{ repository(owner: $owner, name: $repo) {{ {fields} }} }}");
        let output = crate::external::gh_command()
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
        let output = output.map_err(|error| format!("Could not run gh: {error}"))?;
        collect_status_response(output, prs)?;
    }
    Ok(())
}

fn collect_status_response(
    output: std::process::Output,
    prs: &mut Vec<PrStatus>,
) -> Result<(), String> {
    let value = match serde_json::from_slice::<serde_json::Value>(&output.stdout) {
        Ok(value) => value,
        Err(_) => {
            checked_gh_output(Ok(output))?;
            return Err("Invalid GitHub PR response".into());
        }
    };
    if let Some(repository) = value
        .pointer("/data/repository")
        .and_then(|v| v.as_object())
    {
        prs.extend(repository.values().filter_map(parse_pr_node));
        // A missing PR is local to its alias; retain partial successes and retry only that number.
        if let Some(errors) = value.get("errors").and_then(|v| v.as_array()) {
            if !errors.is_empty()
                && errors.iter().all(|error| {
                    error.get("type").and_then(|v| v.as_str()) == Some("NOT_FOUND")
                        && error
                            .get("path")
                            .and_then(|v| v.as_array())
                            .is_some_and(|path| {
                                path.len() == 2
                                    && path[0] == "repository"
                                    && path[1].as_str().is_some_and(|alias| {
                                        repository.get(alias).is_some_and(|v| v.is_null())
                                            && alias
                                                .strip_prefix("pr")
                                                .is_some_and(|n| n.parse::<u32>().is_ok())
                                    })
                            })
                })
            {
                return Ok(());
            }
        }
    }
    checked_gh_output(Ok(output))?;
    if let Some(errors) = value
        .get("errors")
        .and_then(|v| v.as_array())
        .filter(|e| !e.is_empty())
    {
        return Err(errors[0]
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("GitHub PR request failed")
            .into());
    }
    if value
        .pointer("/data/repository")
        .and_then(|v| v.as_object())
        .is_none()
    {
        return Err("Missing GitHub repository".into());
    }
    Ok(())
}

pub fn fetch_stacks(main_worktree: &Path) -> Result<Option<Vec<GithubStack>>, String> {
    let repo = get_repo_identity(main_worktree)?;
    let endpoint = format!("repos/{}/{}/stacks?per_page=100", repo.owner, repo.name);
    let output = crate::external::gh_command()
        .args(["api", "--paginate", "--slurp", &endpoint])
        .current_dir(main_worktree)
        .output();

    if output.as_ref().is_ok_and(|output| {
        !output.status.success()
            && stack_endpoint_unsupported(&String::from_utf8_lossy(&output.stderr))
    }) {
        return Ok(None);
    }
    let stdout = checked_gh_output(output)?;
    parse_stacks_response(&stdout)
        .map(Some)
        .ok_or_else(|| "Invalid GitHub stack response".to_string())
}

pub fn fetch_stack(main_worktree: &Path, number: u32) -> Result<GithubStack, String> {
    let repo = get_repo_identity(main_worktree)?;
    let endpoint = format!("repos/{}/{}/stacks/{number}", repo.owner, repo.name);
    let stdout = checked_gh_output(
        crate::external::gh_command()
            .args(["api", &endpoint])
            .current_dir(main_worktree)
            .output(),
    )?;
    let value = serde_json::from_str(&stdout).map_err(|_| "Invalid GitHub stack response")?;
    parse_stack(&value).ok_or_else(|| "Invalid GitHub stack response".to_string())
}

pub fn fetch_branch_prs(
    main_worktree: &Path,
    branches: &[String],
) -> Result<Vec<PrStatus>, String> {
    let repo = get_repo_identity(main_worktree)?;
    fetch_branch_pages(branches, |query| {
        checked_gh_output(
            crate::external::gh_command()
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
                .output(),
        )
    })
}

fn fetch_branch_pages(
    branches: &[String],
    mut fetch: impl FnMut(&str) -> Result<String, String>,
) -> Result<Vec<PrStatus>, String> {
    let mut pending: Vec<_> = branches
        .iter()
        .cloned()
        .map(|branch| (branch, None::<String>))
        .collect();
    let mut selected = HashMap::<String, PrStatus>::new();
    while !pending.is_empty() {
        let fields = pending.iter().enumerate().map(|(index, (branch, cursor))| {
            let branch = serde_json::to_string(branch).unwrap_or_default();
            let after = cursor.as_ref().map(|cursor| format!(",after:{}", serde_json::to_string(cursor).unwrap_or_default())).unwrap_or_default();
            format!("b{index}: pullRequests(headRefName:{branch},first:20{after},orderBy:{{field:UPDATED_AT,direction:DESC}}){{nodes{{{PR_FIELDS}}} pageInfo{{hasNextPage endCursor}}}}")
        }).collect::<Vec<_>>().join(" ");
        let query = format!("query($owner:String!,$repo:String!){{repository(owner:$owner,name:$repo){{{fields}}}}}");
        let stdout = fetch(&query)?;
        let value: serde_json::Value =
            serde_json::from_str(&stdout).map_err(|_| "Invalid GitHub branch response")?;
        let repository = value
            .pointer("/data/repository")
            .and_then(|v| v.as_object())
            .ok_or("Missing GitHub repository")?;
        let mut next = Vec::new();
        for (index, (branch, previous_cursor)) in pending.into_iter().enumerate() {
            let connection = repository
                .get(&format!("b{index}"))
                .ok_or("Missing GitHub branch response")?;
            let nodes = connection
                .get("nodes")
                .and_then(|v| v.as_array())
                .ok_or("Missing GitHub branch PRs")?;
            for pr in nodes
                .iter()
                .filter_map(parse_pr_node)
                .filter(|pr| !pr.is_cross_repository)
            {
                if selected
                    .get(&branch)
                    .is_none_or(|old| state_priority(&pr.state) > state_priority(&old.state))
                {
                    selected.insert(branch.clone(), pr);
                }
            }
            if selected
                .get(&branch)
                .is_some_and(|pr| pr.state == PrState::Open)
            {
                continue;
            }
            if connection
                .pointer("/pageInfo/hasNextPage")
                .and_then(|v| v.as_bool())
                == Some(true)
            {
                let cursor = connection
                    .pointer("/pageInfo/endCursor")
                    .and_then(|v| v.as_str())
                    .ok_or("Missing GitHub branch cursor")?;
                if previous_cursor.as_deref() == Some(cursor) {
                    return Err("GitHub branch cursor did not advance".into());
                }
                next.push((branch, Some(cursor.into())));
            }
        }
        pending = next;
    }
    Ok(selected.into_values().collect())
}

fn stack_endpoint_unsupported(stderr: &str) -> bool {
    ["HTTP 404", "HTTP 410", "HTTP 501"]
        .iter()
        .any(|status| stderr.contains(status))
}

fn parse_stacks_response(json_str: &str) -> Option<Vec<GithubStack>> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return None;
    };
    let stacks = parsed.as_array()?;
    let mut parsed_stacks = Vec::new();
    if stacks.iter().all(serde_json::Value::is_array) {
        for page in stacks {
            if let Some(page) = page.as_array() {
                parsed_stacks.extend(page.iter().filter_map(parse_stack));
            }
        }
    } else {
        parsed_stacks.extend(stacks.iter().filter_map(parse_stack));
    }
    Some(parsed_stacks)
}

fn parse_stack(value: &serde_json::Value) -> Option<GithubStack> {
    let number = value.get("number")?.as_u64()? as u32;
    let url = value
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let base_ref = value.pointer("/base/ref")?.as_str()?.to_string();
    let open = value.get("open").and_then(|v| v.as_bool()).unwrap_or(true);
    let mut pull_requests = value
        .get("pull_requests")
        .and_then(|v| v.as_array())?
        .iter()
        .map(parse_stack_pull_request)
        .collect::<Option<Vec<_>>>()?;

    let mut seen = std::collections::HashSet::new();
    pull_requests.retain(|pr| seen.insert(pr.number));
    Some(GithubStack {
        number,
        url,
        base_ref,
        open,
        pull_requests,
    })
}

fn parse_stack_pull_request(value: &serde_json::Value) -> Option<GithubStackPullRequest> {
    let number = value.get("number")?.as_u64()? as u32;
    let state = match value.get("merged_at").and_then(|v| v.as_str()) {
        Some(_) => PrState::Merged,
        None => match value.get("state")?.as_str()?.to_ascii_lowercase().as_str() {
            "open" => PrState::Open,
            "closed" => PrState::Closed,
            _ => return None,
        },
    };
    let is_draft = value
        .get("draft")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let head_branch = value.pointer("/head/ref")?.as_str()?.to_string();

    Some(GithubStackPullRequest {
        number,
        state,
        is_draft,
        head_branch,
    })
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

    // Kept but flagged so branch-keyed indexing can skip fork PRs, whose head
    // branch can collide with upstream names. Missing means same-repo.
    let is_cross_repository = node
        .get("isCrossRepository")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

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
        is_cross_repository,
    })
}

pub fn fetch_user_prs(main_worktree: &Path) -> Result<Vec<PrStatus>, String> {
    let repo = get_repo_identity(main_worktree)?;
    let user = fetch_current_user(main_worktree).ok_or("Could not fetch GitHub user")?;

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

    let output = crate::external::gh_command()
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

    let stdout = checked_gh_output(output)?;
    Ok(parse_search_response(&stdout))
}

#[derive(Default)]
pub struct ReviewDiscovery {
    pub prs: Vec<PrStatus>,
    pub stacks: HashMap<u32, Option<u32>>,
}

pub fn fetch_review_requested_prs(main_worktree: &Path) -> Result<ReviewDiscovery, String> {
    let repo = get_repo_identity(main_worktree)?;
    let user = fetch_current_user(main_worktree).ok_or("Could not fetch GitHub user")?;
    let search = format!(
        "repo:{}/{} is:pr is:open review-requested:{}",
        repo.owner, repo.name, user
    );
    fetch_review_pages(|query| {
        checked_gh_output(
            crate::external::gh_command()
                .args([
                    "api",
                    "graphql",
                    "-f",
                    &format!("query={query}"),
                    "-f",
                    &format!("search={search}"),
                ])
                .current_dir(main_worktree)
                .output(),
        )
    })
}

fn fetch_review_pages(
    mut fetch: impl FnMut(&str) -> Result<String, String>,
) -> Result<ReviewDiscovery, String> {
    let mut result = ReviewDiscovery::default();
    let mut prs = HashMap::new();
    let mut cursor = None::<String>;
    let mut with_stacks = true;
    loop {
        let after = cursor
            .as_ref()
            .map(|cursor| {
                format!(
                    ",after:{}",
                    serde_json::to_string(cursor).unwrap_or_default()
                )
            })
            .unwrap_or_default();
        let stack_field = if with_stacks { "stack { number }" } else { "" };
        let query = format!("query($search:String!){{ search(query:$search,type:ISSUE,first:50{after}){{ nodes{{ ... on PullRequest{{ {PR_FIELDS} {stack_field} }} }} issueCount pageInfo{{hasNextPage endCursor}} }} }}");
        let stdout = match fetch(&query) {
            Ok(stdout) => stdout,
            Err(error)
                if with_stacks
                    && error.contains("stack")
                    && (error.contains("doesn't exist")
                        || error.contains("Cannot query field")) =>
            {
                with_stacks = false;
                cursor = None;
                prs.clear();
                result.stacks.clear();
                continue;
            }
            Err(error) => return Err(error),
        };
        let value: serde_json::Value =
            serde_json::from_str(&stdout).map_err(|_| "Invalid GitHub review response")?;
        if value
            .get("errors")
            .and_then(|v| v.as_array())
            .is_some_and(|errors| !errors.is_empty())
        {
            return Err("GitHub review query returned incomplete data".into());
        }
        let search = value
            .pointer("/data/search")
            .ok_or("Missing GitHub review results")?;
        if search
            .get("issueCount")
            .and_then(|v| v.as_u64())
            .is_some_and(|count| count > 1000)
        {
            return Err(
                "GitHub review search exceeds its 1,000-result limit; keeping existing reviews"
                    .into(),
            );
        }
        let nodes = search
            .get("nodes")
            .and_then(|v| v.as_array())
            .ok_or("Missing GitHub review nodes")?;
        for node in nodes {
            let pr = parse_pr_node(node).ok_or("Invalid GitHub review PR")?;
            let stack = if with_stacks {
                match node.get("stack") {
                    Some(serde_json::Value::Null) => None,
                    Some(stack) => Some(
                        stack
                            .get("number")
                            .and_then(|v| v.as_u64())
                            .and_then(|n| u32::try_from(n).ok())
                            .ok_or("Invalid GitHub review stack")?,
                    ),
                    None => return Err("Missing GitHub review stack membership".into()),
                }
            } else {
                None
            };
            if result
                .stacks
                .get(&pr.number)
                .is_some_and(|old| old != &stack)
            {
                return Err("GitHub stack membership changed during discovery; retrying".into());
            }
            result.stacks.insert(pr.number, stack);
            prs.insert(pr.number, pr);
        }
        let has_next = search
            .pointer("/pageInfo/hasNextPage")
            .and_then(|v| v.as_bool())
            .ok_or("Missing GitHub review pagination")?;
        if !has_next {
            break;
        }
        let next = search
            .pointer("/pageInfo/endCursor")
            .and_then(|v| v.as_str())
            .ok_or("Missing GitHub review cursor")?;
        if cursor.as_deref() == Some(next) {
            return Err("GitHub review cursor did not advance".into());
        }
        cursor = Some(next.to_string());
    }
    result.prs = prs.into_values().collect();
    result
        .prs
        .sort_unstable_by_key(|pr| std::cmp::Reverse(pr.number));
    Ok(result)
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
    // Claim the fetch or wait for an in-flight one. Never hold the lock across
    // the `gh` call: a hung network request would block every PR worker in
    // every project.
    {
        let mut cached = GITHUB_USER.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match cached.as_ref() {
                Some(FetchState::Ready(user)) => return Some(user.clone()),
                Some(FetchState::InFlight) => {
                    cached = GITHUB_USER_WAIT
                        .wait(cached)
                        .unwrap_or_else(|e| e.into_inner());
                }
                None => {
                    *cached = Some(FetchState::InFlight);
                    break;
                }
            }
        }
    }

    let login = fetch_current_user_uncached(main_worktree);

    let mut cached = GITHUB_USER.lock().unwrap_or_else(|e| e.into_inner());
    match &login {
        Some(user) => *cached = Some(FetchState::Ready(user.clone())),
        // Failed: clear the marker so a later poll retries.
        None => *cached = None,
    }
    GITHUB_USER_WAIT.notify_all();
    login
}

fn fetch_current_user_uncached(main_worktree: &Path) -> Option<String> {
    let output = crate::external::gh_command()
        .args(["api", "user", "-q", ".login"])
        .current_dir(main_worktree)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let login = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if login.is_empty() {
        return None;
    }

    Some(login)
}

/// Build the canonical github.com URL for a PR, given the project's main worktree.
/// Returns None if we can't determine the repo identity (e.g. no `gh` available, or
/// the worktree isn't a GitHub remote). Uses the cached repo identity, so this is
/// effectively free after the first call.
pub fn pr_url(main_worktree: &Path, pr_number: u32) -> Option<String> {
    let repo = get_repo_identity(main_worktree).ok()?;
    Some(format_pr_url(&repo.owner, &repo.name, pr_number))
}

fn format_pr_url(owner: &str, name: &str, pr_number: u32) -> String {
    format!("https://github.com/{}/{}/pull/{}", owner, name, pr_number)
}

pub fn index_by_branch(prs: Vec<PrStatus>) -> HashMap<String, PrStatus> {
    // When multiple PRs share a branch (e.g. an old merged PR plus a current
    // open one on a reused branch name), prefer the higher-priority state.
    // Open beats Merged/Closed; within the same priority the first PR wins,
    // and `fetch_prs` returns PRs ordered by UPDATED_AT DESC, so "first" means
    // most recent. Fork PRs are skipped: their branch names live in another
    // repo's namespace and would pollute the index.
    let mut map: HashMap<String, PrStatus> = HashMap::new();
    for pr in prs {
        if pr.is_cross_repository {
            continue;
        }
        let new_priority = state_priority(&pr.state);
        match map.get(&pr.head_branch) {
            Some(existing) if state_priority(&existing.state) >= new_priority => {}
            _ => {
                map.insert(pr.head_branch.clone(), pr);
            }
        }
    }
    map
}

fn state_priority(state: &PrState) -> u8 {
    match state {
        PrState::Open => 2,
        PrState::Merged | PrState::Closed => 1,
    }
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
    fn review_pages_dedupe_prs_and_resolve_stack_before_returning() {
        let mut pages = 0;
        let result = fetch_review_pages(|query| {
            assert!(query.contains("stack { number }"));
            pages += 1;
            let nodes = if pages == 1 {
                vec![make_pr_node(r#"{"number":1,"stack":{"number":10}}"#), make_pr_node(r#"{"number":2,"stack":{"number":10}}"#)]
            } else {
                assert!(query.contains("after:\"next\""));
                vec![make_pr_node(r#"{"number":2,"stack":{"number":10}}"#), make_pr_node(r#"{"number":3,"stack":null}"#)]
            };
            Ok(serde_json::json!({"data":{"search":{"nodes":nodes,"pageInfo":{"hasNextPage":pages == 1,"endCursor":"next"}}}}).to_string())
        }).unwrap();
        assert_eq!(pages, 2);
        assert_eq!(
            result.prs.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        assert_eq!(
            result.stacks,
            [(1, Some(10)), (2, Some(10)), (3, None)].into()
        );
    }

    #[test]
    fn incomplete_review_discovery_is_an_error_not_an_empty_success() {
        for response in [
            r#"{}"#,
            r#"{"data":{"search":{"nodes":[]}},"errors":[{"message":"denied"}]}"#,
            r#"{"data":{"search":{"nodes":[null],"pageInfo":{"hasNextPage":false}}}}"#,
        ] {
            assert!(fetch_review_pages(|_| Ok(response.into())).is_err());
        }
        let mut calls = 0;
        assert!(fetch_review_pages(|_| {
            calls += 1;
            if calls == 2 { return Err("offline".into()); }
            Ok(serde_json::json!({"data":{"search":{"nodes":[make_pr_node(r#"{"stack":null}"#)],"pageInfo":{"hasNextPage":true,"endCursor":"next"}}}}).to_string())
        }).is_err());
    }

    #[test]
    fn review_discovery_falls_back_only_for_missing_stack_schema() {
        let mut calls = 0;
        let result = fetch_review_pages(|query| {
            calls += 1;
            if calls == 1 { return Err("Field 'stack' doesn't exist on type 'PullRequest'".into()); }
            assert!(!query.contains("stack { number }"));
            Ok(serde_json::json!({"data":{"search":{"nodes":[make_pr_node("")],"pageInfo":{"hasNextPage":false}}}}).to_string())
        }).unwrap();
        assert_eq!(result.stacks[&42], None);
        let mut calls = 0;
        assert!(fetch_review_pages(|_| {
            calls += 1;
            Err("HTTP 403".into())
        })
        .is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn branch_lookup_skips_forks_and_finds_open_pr_on_later_page() {
        let mut calls = 0;
        let prs = fetch_branch_pages(&["feature".into()], |query| {
            calls += 1;
            let nodes = if calls == 1 {
                assert!(query.contains("first:20"));
                vec![
                    make_pr_node(r#"{"number":9,"isCrossRepository":true}"#),
                    make_pr_node(r#"{"number":8,"state":"MERGED"}"#),
                ]
            } else {
                assert!(query.contains("after:\"cursor\""));
                vec![make_pr_node(r#"{"number":7,"state":"OPEN"}"#)]
            };
            Ok(serde_json::json!({"data":{"repository":{"b0":{
                "nodes":nodes,"pageInfo":{"hasNextPage":calls == 1,"endCursor":"cursor"}
            }}}})
            .to_string())
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 7);
        assert!(!prs[0].is_cross_repository);
    }

    #[test]
    fn partial_not_found_is_local_but_auth_errors_keep_their_message() {
        use std::os::unix::process::ExitStatusExt;
        let mut prs = Vec::new();
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: serde_json::json!({"data":{"repository":{"pr42":make_pr_node(""),"pr999":null}},
                "errors":[{"type":"NOT_FOUND","path":["repository","pr999"],"message":"Could not resolve to a PullRequest"}]}).to_string().into_bytes(),
            stderr: b"gh: Could not resolve to a PullRequest".to_vec(),
        };
        assert!(collect_status_response(output, &mut prs).is_ok());
        assert_eq!(prs.len(), 1);
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: b"gh: not logged in".to_vec(),
        };
        assert_eq!(
            collect_status_response(output, &mut prs).unwrap_err(),
            "gh: not logged in"
        );
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: br#"{"data":{"repository":null},"errors":[{"type":"NOT_FOUND","path":["repository"]}]}"#.to_vec(),
            stderr: b"repository inaccessible".to_vec(),
        };
        assert_eq!(
            collect_status_response(output, &mut prs).unwrap_err(),
            "repository inaccessible"
        );
    }

    #[test]
    fn unsupported_stack_endpoint_does_not_swallow_auth_or_network_errors() {
        assert!(stack_endpoint_unsupported("gh: Not Found (HTTP 404)"));
        assert!(stack_endpoint_unsupported("gh: Not Implemented (HTTP 501)"));
        for error in [
            "gh: Forbidden (HTTP 403)",
            "gh: Unauthorized (HTTP 401)",
            "connection refused",
            "gh: Internal Server Error (HTTP 500)",
        ] {
            assert!(!stack_endpoint_unsupported(error));
        }
    }

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
    fn test_parse_stacks_response_preserves_order_and_membership() {
        let response = serde_json::json!([{
            "number": 7,
            "url": "https://api.github.com/repos/test/repo/stacks/7",
            "base": { "ref": "main" },
            "open": true,
            "pull_requests": [
                {
                    "number": 10,
                    "state": "open",
                    "draft": false,
                    "merged_at": null,
                    "head": { "ref": "feature/base" }
                },
                {
                    "number": 11,
                    "state": "closed",
                    "draft": true,
                    "merged_at": "2026-08-28T10:00:00Z",
                    "head": { "ref": "feature/top" }
                }
            ]
        }]);

        let stacks = parse_stacks_response(&response.to_string());
        let stacks = stacks.unwrap();
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].number, 7);
        assert_eq!(stacks[0].base_ref, "main");
        assert_eq!(stacks[0].pull_requests[0].number, 10);
        assert_eq!(stacks[0].pull_requests[0].state, PrState::Open);
        assert_eq!(stacks[0].pull_requests[1].number, 11);
        assert_eq!(stacks[0].pull_requests[1].state, PrState::Merged);
        assert!(stacks[0].pull_requests[1].is_draft);
    }

    #[test]
    fn test_parse_stacks_response_skips_invalid_entries() {
        let response = serde_json::json!([{
            "number": 7,
            "pull_requests": [{ "number": 10 }]
        }, {
            "number": 8,
            "base": { "ref": "main" },
            "pull_requests": []
        }]);

        let stacks = parse_stacks_response(&response.to_string());
        let stacks = stacks.unwrap();
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].number, 8);
    }

    #[test]
    fn stack_with_incomplete_membership_is_not_actionable() {
        let response = serde_json::json!([{
            "number": 42,
            "base": { "ref": "main" },
            "pull_requests": [
                { "number": 1, "state": "open", "head": { "ref": "first" } },
                { "number": 2 }
            ]
        }]);
        assert!(parse_stacks_response(&response.to_string())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_parse_paginated_stacks_response() {
        let response = serde_json::json!([
            [{
                "number": 7,
                "base": { "ref": "main" },
                "pull_requests": []
            }],
            [{
                "number": 8,
                "base": { "ref": "main" },
                "pull_requests": []
            }]
        ]);

        let stacks = parse_stacks_response(&response.to_string()).unwrap();
        assert_eq!(
            stacks.iter().map(|stack| stack.number).collect::<Vec<_>>(),
            [7, 8]
        );
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

    // --- format_pr_url ---

    #[test]
    fn test_format_pr_url_basic() {
        assert_eq!(
            format_pr_url("octocat", "hello-world", 42),
            "https://github.com/octocat/hello-world/pull/42"
        );
    }

    #[test]
    fn test_format_pr_url_with_dashes_and_dots() {
        assert_eq!(
            format_pr_url("my-org", "some.repo", 1),
            "https://github.com/my-org/some.repo/pull/1"
        );
    }

    #[test]
    fn test_format_pr_url_large_number() {
        assert_eq!(
            format_pr_url("o", "r", 123456),
            "https://github.com/o/r/pull/123456"
        );
    }

    // --- index_by_branch ---

    #[test]
    fn test_index_empty() {
        let map = index_by_branch(vec![]);
        assert!(map.is_empty());
    }

    fn test_pr_status(number: u32, branch: &str) -> PrStatus {
        test_pr_status_with_state(number, branch, PrState::Open)
    }

    fn test_pr_status_with_state(number: u32, branch: &str, state: PrState) -> PrStatus {
        PrStatus {
            number,
            title: format!("PR #{}", number),
            url: format!("https://github.com/test/repo/pull/{}", number),
            author: "testuser".into(),
            state,
            is_draft: false,
            checks: None,
            review: None,
            additions: 0,
            deletions: 0,
            head_branch: branch.into(),
            is_cross_repository: false,
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
    fn test_index_duplicate_branch_first_open_wins() {
        // Input is ordered newest-first (UPDATED_AT DESC), so the first PR
        // in the vec is the most recent. With equal priority, first wins.
        let prs = vec![
            test_pr_status(1, "same-branch"),
            test_pr_status(2, "same-branch"),
        ];
        let map = index_by_branch(prs);
        assert_eq!(map.len(), 1);
        assert_eq!(map["same-branch"].number, 1);
    }

    #[test]
    fn test_index_open_pr_beats_older_merged_pr() {
        // Real-world bug: a current Open PR should not be hidden by a stale
        // Merged PR on a reused branch name, regardless of input order.
        let prs = vec![
            test_pr_status_with_state(99, "reused-branch", PrState::Merged),
            test_pr_status_with_state(100, "reused-branch", PrState::Open),
        ];
        let map = index_by_branch(prs);
        assert_eq!(map.len(), 1);
        assert_eq!(map["reused-branch"].number, 100);
        assert_eq!(map["reused-branch"].state, PrState::Open);
    }

    #[test]
    fn test_index_open_pr_beats_newer_merged_pr() {
        // Even when the Merged PR is "newer" in the input order, Open wins.
        let prs = vec![
            test_pr_status_with_state(50, "reused-branch", PrState::Open),
            test_pr_status_with_state(51, "reused-branch", PrState::Merged),
        ];
        let map = index_by_branch(prs);
        assert_eq!(map.len(), 1);
        assert_eq!(map["reused-branch"].number, 50);
        assert_eq!(map["reused-branch"].state, PrState::Open);
    }

    #[test]
    fn test_index_two_merged_prefers_first() {
        // No Open PR present: keep the most recent (first in vec) closed/merged.
        let prs = vec![
            test_pr_status_with_state(7, "abandoned", PrState::Merged),
            test_pr_status_with_state(6, "abandoned", PrState::Closed),
        ];
        let map = index_by_branch(prs);
        assert_eq!(map.len(), 1);
        assert_eq!(map["abandoned"].number, 7);
        assert_eq!(map["abandoned"].state, PrState::Merged);
    }

    #[test]
    fn test_parse_cross_repository_pr_sets_flag() {
        let node = make_pr_node(r#"{"isCrossRepository": true}"#);
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.number, 42);
        assert!(pr.is_cross_repository);
    }

    #[test]
    fn test_parse_same_repository_pr_parses_normally() {
        let node = make_pr_node(r#"{"isCrossRepository": false}"#);
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.number, 42);
        assert!(!pr.is_cross_repository);
    }

    #[test]
    fn test_parse_missing_cross_repository_treated_as_same_repo() {
        let mut node = make_pr_node("");
        node.as_object_mut().unwrap().remove("isCrossRepository");
        let pr = parse_pr_node(&node).unwrap();
        assert_eq!(pr.number, 42);
        assert!(!pr.is_cross_repository);
    }

    #[test]
    fn test_index_skips_cross_repository_prs() {
        let mut fork_pr = test_pr_status(1, "feature");
        fork_pr.is_cross_repository = true;
        let same_repo_pr = test_pr_status(2, "feature");
        let map = index_by_branch(vec![fork_pr, same_repo_pr]);
        assert_eq!(map.len(), 1);
        assert_eq!(map["feature"].number, 2);
    }

    #[test]
    fn test_index_only_cross_repository_pr_yields_empty() {
        let mut fork_pr = test_pr_status(1, "feature");
        fork_pr.is_cross_repository = true;
        let map = index_by_branch(vec![fork_pr]);
        assert!(map.is_empty());
    }
}
