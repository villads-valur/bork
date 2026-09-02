use std::io::Write;
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::error::AppError;

const API_ENDPOINT: &str = "https://api.linear.app/graphql";
const API_KEY_ENV: &str = "LINEAR_API_KEY";

#[derive(Debug, Clone)]
pub struct LinearIssue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    #[allow(dead_code)] // Intended for prefilling worktree branch names on import
    pub branch_name: String,
    pub priority: u8,
    pub state_name: String,
    pub team_key: String,
}

#[derive(Debug)]
pub struct LinearPollResult {
    pub issues: Vec<LinearIssue>,
    /// Why the poll returned nothing, when it failed. An empty list is
    /// otherwise indistinguishable from having no assigned issues, which sends
    /// the user looking at Linear instead of at their key.
    pub error: Option<String>,
}

/// How a query reaches Linear.
///
/// Linear publishes no CLI, so a personal API key is the path most users have.
/// A `linear` binary is still preferred when one is on PATH, so anyone who
/// wrote or installed one keeps their existing setup and its own auth.
#[derive(Debug, PartialEq, Eq)]
enum Transport {
    Cli,
    Api(String),
}

fn cli_available() -> bool {
    Command::new("linear")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn api_key() -> Option<String> {
    let key = std::env::var(API_KEY_ENV).ok()?;
    let key = key.trim();
    (!key.is_empty()).then(|| key.to_string())
}

fn transport() -> Option<Transport> {
    select_transport(cli_available(), api_key())
}

/// A `linear` command wins over the API, so an existing wrapper keeps its own
/// auth and behaviour. Split from [`transport`] so the precedence is testable
/// without spawning a process.
fn select_transport(cli_present: bool, key: Option<String>) -> Option<Transport> {
    if cli_present {
        return Some(Transport::Cli);
    }
    key.map(Transport::Api)
}

pub fn check_available() -> bool {
    transport().is_some()
}

const QUERY: &str = concat!(
    "{ viewer { assignedIssues(",
    "filter: { state: { type: { nin: [\"completed\", \"canceled\"] } } }, ",
    "first: 50, ",
    "orderBy: updatedAt",
    ") { nodes { id identifier title url branchName priority ",
    "state { name } team { key } } } } }",
);

pub fn fetch_assigned_issues() -> Result<Vec<LinearIssue>, AppError> {
    parse_issues(&run_query(QUERY)?)
}

/// Run a GraphQL query, returning the raw response body.
fn run_query(query: &str) -> Result<String, AppError> {
    match transport() {
        Some(Transport::Cli) => run_via_cli(query),
        Some(Transport::Api(key)) => run_via_api(&key, query),
        None => Err(AppError::Linear(format!(
            "no `linear` command on PATH and {API_KEY_ENV} is not set"
        ))),
    }
}

fn run_via_cli(query: &str) -> Result<String, AppError> {
    let output = Command::new("linear")
        .arg("api")
        .arg(query)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| AppError::Linear(format!("failed to run linear cli: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::Linear(format!(
            "linear api failed: {}",
            stderr.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// POST the query with `curl`, matching how bork reaches every other external
/// service rather than pulling in an HTTP stack for one endpoint.
///
/// The request is fed to curl as a config file on stdin: an `-H` argument would
/// put the API key in the process table for anything running `ps` to read.
///
/// Deliberately no `--fail`: Linear returns auth and scope failures as a 4xx
/// whose body still carries the GraphQL `errors` array, and `--fail` would
/// discard that body, turning a precise message into an exit code.
///
/// The timeouts are load-bearing. One poll worker serves Linear for the whole
/// session, so an unbounded request that never returns stops every later poll
/// with the thread still alive and nothing on screen to say so.
fn run_via_api(key: &str, query: &str) -> Result<String, AppError> {
    let body = serde_json::json!({ "query": query }).to_string();

    let mut child = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--config",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AppError::Linear(format!("failed to run curl: {e}")))?;

    let config = curl_config(key, &body);
    child
        .stdin
        .take()
        .ok_or_else(|| AppError::Linear("curl stdin unavailable".to_string()))?
        .write_all(config.as_bytes())
        .map_err(|e| AppError::Linear(format!("failed to send request to curl: {e}")))?;

    let output = child
        .wait_with_output()
        .map_err(|e| AppError::Linear(format!("curl failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::Linear(format!(
            "linear api request failed: {}",
            stderr.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A curl config file for one POST. Values are double-quoted, so backslashes
/// and quotes inside the JSON body have to be escaped or curl truncates it.
///
/// The key goes in `Authorization` raw, with no `Bearer` prefix: that is the
/// format Linear personal API keys use. OAuth tokens need `Bearer`, so this
/// looks like an omission and is not.
fn curl_config(key: &str, body: &str) -> String {
    format!(
        concat!(
            "url = \"{}\"\n",
            "request = \"POST\"\n",
            "header = \"Authorization: {}\"\n",
            "header = \"Content-Type: application/json\"\n",
            "data = \"{}\"\n",
        ),
        API_ENDPOINT,
        escape_for_config(key),
        escape_for_config(body),
    )
}

/// Escapes for curl's double-quoted config values. Assumes single-line input:
/// a literal newline would end the value early. Safe here because the body is
/// built by `serde_json`, which escapes newlines as `\n`.
fn escape_for_config(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Read issues out of a response body.
///
/// GraphQL reports query and permission failures in the body — Linear returns
/// some of them with a non-2xx status too — so a bare deserialize would show an
/// expired token as a parse error.
fn parse_issues(body: &str) -> Result<Vec<LinearIssue>, AppError> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        AppError::Linear(format!(
            "could not parse the Linear response: {e} (body: {})",
            truncate(body, 200)
        ))
    })?;

    if let Some(errors) = value.get("errors").and_then(|e| e.as_array()) {
        if !errors.is_empty() {
            let messages: Vec<&str> = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect();
            // Errors without an extractable message still mean failure. Falling
            // through would deserialize a null `data` and report a shape
            // mismatch, which is the confusion this function exists to prevent.
            if messages.is_empty() {
                return Err(AppError::Linear(
                    "Linear returned errors with no message".to_string(),
                ));
            }
            return Err(AppError::Linear(messages.join("; ")));
        }
    }

    let response: GraphqlResponse = serde_json::from_value(value)
        .map_err(|e| AppError::Linear(format!("unexpected Linear response shape: {e}")))?;

    Ok(response
        .data
        .viewer
        .assigned_issues
        .nodes
        .into_iter()
        .map(|node| LinearIssue {
            id: node.id,
            identifier: node.identifier,
            title: node.title,
            url: node.url,
            branch_name: node.branch_name.unwrap_or_default(),
            priority: node.priority,
            state_name: node.state.name,
            team_key: node.team.key,
        })
        .collect())
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

// Serde types matching the GraphQL response shape

#[derive(Deserialize)]
struct GraphqlResponse {
    data: GraphqlData,
}

#[derive(Deserialize)]
struct GraphqlData {
    viewer: Viewer,
}

#[derive(Deserialize)]
struct Viewer {
    #[serde(rename = "assignedIssues")]
    assigned_issues: IssueConnection,
}

#[derive(Deserialize)]
struct IssueConnection {
    nodes: Vec<IssueNode>,
}

#[derive(Deserialize)]
struct IssueNode {
    id: String,
    identifier: String,
    title: String,
    url: String,
    #[serde(rename = "branchName")]
    branch_name: Option<String>,
    priority: u8,
    state: IssueState,
    team: IssueTeam,
}

#[derive(Deserialize)]
struct IssueState {
    name: String,
}

#[derive(Deserialize)]
struct IssueTeam {
    key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cli_wins_over_a_configured_key() {
        // The precedence is the contract the fallback hinges on: someone with a
        // wrapper keeps their own auth even after setting a key.
        assert_eq!(
            select_transport(true, Some("lin_api_key".into())),
            Some(Transport::Cli)
        );
    }

    #[test]
    fn the_key_is_used_when_no_cli_exists() {
        assert_eq!(
            select_transport(false, Some("lin_api_key".into())),
            Some(Transport::Api("lin_api_key".into()))
        );
    }

    #[test]
    fn neither_configured_is_unavailable() {
        assert_eq!(select_transport(false, None), None);
    }

    #[test]
    fn parse_reports_errors_that_carry_no_message() {
        // A non-empty errors array is a failure even when nothing readable can
        // be pulled out of it; falling through would blame the response shape.
        let err = parse_issues(r#"{"errors":[{"extensions":{"code":"X"}}],"data":null}"#)
            .expect_err("a non-empty errors array is a failure");

        assert!(err.to_string().contains("no message"), "got: {err}");
    }

    #[test]
    fn parse_ignores_an_empty_errors_array() {
        let body = r#"{"errors":[],"data":{"viewer":{"assignedIssues":{"nodes":[]}}}}"#;

        assert!(parse_issues(body)
            .expect("empty errors is not a failure")
            .is_empty());
    }

    #[test]
    fn config_escapes_the_json_body() {
        // curl reads double-quoted values, so an unescaped quote from the JSON
        // body would truncate the request mid-query.
        let config = curl_config("lin_api_key", r#"{"query":"{ viewer { id } }"}"#);

        assert!(config.contains(r#"data = "{\"query\":\"{ viewer { id } }\"}""#));
        assert!(config.contains("header = \"Authorization: lin_api_key\""));
        assert!(config.contains("request = \"POST\""));
    }

    #[test]
    fn config_escapes_backslashes_before_quotes() {
        assert_eq!(escape_for_config(r#"a\b"c"#), r#"a\\b\"c"#);
    }

    #[test]
    fn parse_reports_a_graphql_error_rather_than_a_shape_mismatch() {
        // A wrongly scoped or expired key comes back as a normal body; showing
        // "unexpected shape" would send the user looking in the wrong place.
        let err = parse_issues(r#"{"errors":[{"message":"Invalid scope: `read` required"}]}"#)
            .expect_err("an errors array is a failure");

        assert!(err.to_string().contains("Invalid scope"), "got: {err}");
    }

    #[test]
    fn parse_reads_issues() {
        let body = r#"{"data":{"viewer":{"assignedIssues":{"nodes":[
            {"id":"abc","identifier":"ENG-1","title":"Fix it","url":"https://linear.app/x",
             "branchName":"eng-1-fix-it","priority":2,"state":{"name":"In Progress"},
             "team":{"key":"ENG"}}]}}}}"#;

        let issues = parse_issues(body).expect("should parse");

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "ENG-1");
        assert_eq!(issues[0].branch_name, "eng-1-fix-it");
        assert_eq!(issues[0].state_name, "In Progress");
        assert_eq!(issues[0].team_key, "ENG");
    }

    #[test]
    fn parse_defaults_a_missing_branch_name() {
        let body = r#"{"data":{"viewer":{"assignedIssues":{"nodes":[
            {"id":"abc","identifier":"ENG-1","title":"t","url":"u","branchName":null,
             "priority":0,"state":{"name":"Todo"},"team":{"key":"ENG"}}]}}}}"#;

        assert_eq!(parse_issues(body).unwrap()[0].branch_name, "");
    }

    #[test]
    fn parse_shows_the_body_when_it_is_not_json() {
        let err = parse_issues("<html>gateway timeout</html>").expect_err("not json");

        assert!(err.to_string().contains("gateway timeout"), "got: {err}");
    }
}

/// Live smoke test against the real API. Ignored by default; run with
/// `LINEAR_API_KEY=... cargo test linear_api_smoke -- --ignored --nocapture`.
#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    #[ignore]
    fn linear_api_smoke() {
        let key = api_key().expect("LINEAR_API_KEY must be set for this test");
        let body = run_via_api(&key, QUERY).expect("request should succeed");
        let issues = parse_issues(&body).expect("response should parse");
        println!("fetched {} assigned issues", issues.len());
    }
}
