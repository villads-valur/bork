use crate::app::Project;
use crate::types::{ChecksStatus, GithubStack, Issue, PrState, PrStatus};

impl Project {
    pub fn linked_pr_numbers(&self) -> Vec<u32> {
        let mut numbers: Vec<_> = self
            .issues
            .iter()
            .flat_map(|issue| issue.pr_numbers())
            .collect();
        numbers.sort_unstable();
        numbers.dedup();
        numbers
    }

    pub fn attached_stack(&self, issue: &Issue) -> Option<&GithubStack> {
        let number = issue.github_stack?;
        self.live
            .github_stacks
            .iter()
            .find(|stack| stack.number == number)
    }

    pub fn pr_by_number(&self, number: u32) -> Option<&PrStatus> {
        self.live.pr_statuses_by_number.get(&number).or_else(|| {
            self.live
                .pr_statuses
                .values()
                .chain(self.live.user_prs.iter())
                .chain(self.live.review_requested_prs.iter())
                .find(|pr| pr.number == number)
        })
    }

    pub fn issue_pr_numbers(&self, issue: &Issue) -> Vec<u32> {
        let mut numbers = issue.pr_numbers();
        if let Some(stack) = self.attached_stack(issue) {
            for pr in &stack.pull_requests {
                if !numbers.contains(&pr.number) {
                    numbers.push(pr.number);
                }
            }
        }
        numbers
    }

    pub fn stack_open_numbers(&self, issue: &Issue) -> Result<Vec<u32>, &'static str> {
        if self.live.gh_missing || self.live.stacks_unsupported {
            return Err("Stack support is not enabled");
        }
        if issue
            .github_stack
            .is_some_and(|number| self.live.stack_errors.contains_key(&number))
        {
            return Err("Stack refresh failed; press P to retry");
        }
        if !self.live.stacks_available {
            if self.live.github_loading() {
                return Err("Stack data is loading; try again shortly");
            }
            return Err("Stack refresh failed; press P to retry");
        }
        let stack = self
            .attached_stack(issue)
            .ok_or("Attached stack not found; refresh or change its attachment")?;
        let numbers: Vec<_> = stack
            .pull_requests
            .iter()
            .filter(|pr| pr.state == PrState::Open)
            .map(|pr| pr.number)
            .collect();
        if numbers.is_empty() {
            return Err("No open PRs in this stack");
        }
        Ok(numbers)
    }

    pub fn stack_checks(&self, stack: &GithubStack) -> StackChecks {
        let mut summary = StackChecks::default();
        for member in &stack.pull_requests {
            if member.state != PrState::Open {
                continue;
            }
            let checks = self.pr_by_number(member.number).and_then(|pr| pr.checks);
            match checks {
                Some(ChecksStatus::Success) => summary.passed += 1,
                Some(ChecksStatus::Failure | ChecksStatus::Error) => summary.failed += 1,
                Some(ChecksStatus::Pending) => summary.pending += 1,
                None => summary.unknown += 1,
            }
        }
        summary
    }
}

#[derive(Default)]
pub struct StackChecks {
    pub passed: usize,
    pub failed: usize,
    pub pending: usize,
    pub unknown: usize,
}

impl StackChecks {
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        for (count, label) in [
            (self.failed, "failed"),
            (self.pending, "pending"),
            (self.unknown, "unknown"),
            (self.passed, "passed"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {label}"));
            }
        }
        if parts.is_empty() {
            return "no open PRs".to_string();
        }
        parts.join(" · ")
    }
}
