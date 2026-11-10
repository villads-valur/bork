//! The summary `bork import` prints: one block per project, then the
//! global config.

use std::fmt::Write as _;

use super::{GlobalConfigOutcome, RepoOutcome, Report};

impl Report {
    /// Whether at least one project was restored without an error.
    pub fn any_restored(&self) -> bool {
        self.projects.iter().any(|p| p.error.is_none())
    }

    /// Whether any project failed part-way or couldn't be cloned.
    pub fn failed(&self) -> bool {
        self.projects
            .iter()
            .any(|p| p.error.is_some() || p.repo.as_ref().is_some_and(RepoOutcome::is_failure))
            || matches!(self.global_config, GlobalConfigOutcome::Failed(_))
    }

    /// Human-readable summary, one block per project.
    pub fn render(&self) -> String {
        let would = |done: &str, planned: &str| {
            if self.dry_run {
                planned.to_string()
            } else {
                done.to_string()
            }
        };
        let mut out = String::new();
        if self.dry_run {
            out.push_str("Dry run, nothing was changed.\n");
        }
        if !self.host.is_empty() {
            let _ = writeln!(out, "Backup from {}.", self.host);
        }
        if self.dry_run || !self.host.is_empty() {
            out.push('\n');
        }
        for p in &self.projects {
            let label = p
                .label
                .as_deref()
                .map(|l| format!(" ({l})"))
                .unwrap_or_default();
            let _ = writeln!(out, "{}{label} -> {}", p.name, p.target.display());

            let repo = p.repo.as_ref().map(|repo| match repo {
                RepoOutcome::Cloned { url, branch } => {
                    let branch = branch
                        .as_deref()
                        .map(|b| format!(" on {b}"))
                        .unwrap_or_default();
                    format!("{} {url}{branch}", would("cloned", "would clone"))
                }
                RepoOutcome::KeptExisting => "kept the existing main/".to_string(),
                RepoOutcome::NoCloneUrl => {
                    "no remote in the backup; clone the repo into main/ yourself".to_string()
                }
                RepoOutcome::Failed { url, error } => {
                    format!("clone of {url} failed ({error}); clone it into main/ yourself")
                }
                RepoOutcome::WrongBranch { url, branch, error } => format!(
                    "cloned {url}, but couldn't check out {branch} ({error}); main/ is on the remote's default branch"
                ),
            });
            if let Some(repo) = repo {
                let _ = writeln!(out, "  repo:     {repo}");
            }

            if p.board_restored {
                let _ = writeln!(
                    out,
                    "  board:    {} {}",
                    would("restored", "would restore"),
                    count(p.issues, "issue")
                );
            }
            if !p.dropped_issues.is_empty() {
                let _ = writeln!(
                    out,
                    "  board:    {} not in the backup: {}",
                    would("dropped", "would drop"),
                    p.dropped_issues.join(", ")
                );
            }
            if let Some(previous) = &p.previous_board {
                let _ = writeln!(
                    out,
                    "  board:    old board {} {}",
                    would("saved to", "would be saved to"),
                    previous.display()
                );
            }
            if let Some(previous) = &p.previous_config {
                let _ = writeln!(
                    out,
                    "  config:   old .bork/config.toml {} {}",
                    would("saved to", "would be saved to"),
                    previous.display()
                );
            }
            if !p.cleared_worktrees.is_empty() {
                let list: Vec<String> = p
                    .cleared_worktrees
                    .iter()
                    .map(|(id, branch)| match branch {
                        Some(b) => format!("{id} ({b})"),
                        None => id.clone(),
                    })
                    .collect();
                let _ = writeln!(
                    out,
                    "  worktrees: not recreated, reopen from their branches: {}",
                    list.join(", ")
                );
            }
            if p.repo.is_some() {
                let _ = writeln!(
                    out,
                    "  files:    {} {}",
                    would("restored", "would restore"),
                    count(p.files.len(), "container file")
                );
            }
            if self.dry_run {
                for file in &p.files {
                    let _ = writeln!(out, "            {file}");
                }
            }
            if !p.plans.is_empty() {
                let _ = writeln!(
                    out,
                    "  plans:    {} plans/<id>/planning.md for {}",
                    would("wrote", "would write"),
                    p.plans.join(", ")
                );
            }
            if !p.kept_files.is_empty() {
                let _ = writeln!(out, "  kept:     existing {}", p.kept_files.join(", "));
            }
            if !p.kept_plans.is_empty() {
                let _ = writeln!(
                    out,
                    "  plans:    kept existing plans for {}",
                    p.kept_plans.join(", ")
                );
            }
            if p.error.is_none() {
                let _ = writeln!(
                    out,
                    "  {}",
                    would(
                        "registered in projects.json",
                        "would register in projects.json"
                    )
                );
            }
            for warning in &p.warnings {
                let _ = writeln!(out, "  warning:  {warning}");
            }
            if let Some(error) = &p.error {
                let _ = writeln!(out, "  error:    stopped here: {error}");
            }
            out.push('\n');
        }
        match &self.global_config {
            GlobalConfigOutcome::Skipped => {}
            GlobalConfigOutcome::Restored { path, previous } => {
                let _ = writeln!(
                    out,
                    "global config: {} {}",
                    would("restored", "would restore"),
                    path.display()
                );
                if let Some(previous) = previous {
                    let _ = writeln!(
                        out,
                        "global config: old one {} {}",
                        would("saved to", "would be saved to"),
                        previous.display()
                    );
                }
            }
            GlobalConfigOutcome::Failed(error) => {
                let _ = writeln!(out, "global config: not restored: {error}");
            }
            GlobalConfigOutcome::KeptExisting(path) => {
                let _ = writeln!(
                    out,
                    "global config: kept the existing {} (--force replaces it)",
                    path.display()
                );
            }
        }
        out.trim_end().to_string()
    }
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}
