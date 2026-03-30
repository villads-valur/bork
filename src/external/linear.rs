use std::process::Command;

use serde::Deserialize;

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct LinearIssue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub branch_name: String,
    pub priority: u8,
    pub state_name: String,
    pub team_key: String,
}

#[derive(Debug)]
pub struct LinearPollResult {
    pub issues: Vec<LinearIssue>,
}

pub fn check_available() -> bool {
    Command::new("linear")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
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
    let output = Command::new("linear")
        .arg("api")
        .arg(QUERY)
        .output()
        .map_err(|e| AppError::Linear(format!("failed to run linear cli: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::Linear(format!(
            "linear api failed: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let response: GraphqlResponse =
        serde_json::from_str(&stdout).map_err(|e| AppError::Linear(format!("parse error: {e}")))?;

    let issues = response
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
        .collect();

    Ok(issues)
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
