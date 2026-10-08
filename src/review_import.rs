use std::collections::{BTreeMap, HashSet};

use crate::app::{unix_now, Project};
use crate::config::DEFAULT_STACK_REVIEW_PROMPT;
use crate::types::{Column, GithubStack, Issue, PrImportSource, PrState, StackReviewImport};

impl Project {
    pub fn review_requested_for(&self, number: u32) -> bool {
        self.live
            .review_requested_prs
            .iter()
            .any(|pr| pr.number == number)
    }

    fn stack_review_prompt(&self, stack: &GithubStack, requested: &[u32]) -> String {
        let mut prompt = self
            .config
            .stack_review_prompt
            .as_deref()
            .unwrap_or(DEFAULT_STACK_REVIEW_PROMPT)
            .to_string();
        prompt.push_str(&format!(
            "\n\nStack #{}: {}\nBase: {}\nPRs in dependency order:\n",
            stack.number, stack.url, stack.base_ref
        ));
        let repo_url = self
            .live
            .review_requested_prs
            .iter()
            .find_map(|pr| pr.url.rsplit_once("/pull/").map(|(repo, _)| repo));
        for (index, member) in stack.pull_requests.iter().enumerate() {
            let url = repo_url
                .map(|repo| format!("{repo}/pull/{}", member.number))
                .unwrap_or_default();
            let scope = if requested.contains(&member.number) {
                "your review requested"
            } else {
                "context only"
            };
            prompt.push_str(&format!(
                "{}. #{} {} ({}, {})\n",
                index + 1,
                member.number,
                url,
                member.state,
                scope
            ));
        }
        prompt
    }

    /// Resolve groups before the ordinary per-PR importer sees the discovery result.
    pub fn sync_stack_reviews(&mut self, protected: &HashSet<String>) -> bool {
        if !self.config.auto_import_reviews || self.live.review_prs_ready != Some(true) {
            return false;
        }
        let Some(membership) = &self.live.review_stacks else {
            return false;
        };
        let mut groups = BTreeMap::<u32, Vec<u32>>::new();
        for pr in &self.live.review_requested_prs {
            if pr.state != PrState::Open
                || pr.is_draft
                || self
                    .live
                    .github_user
                    .as_deref()
                    .is_some_and(|user| user.eq_ignore_ascii_case(&pr.author))
            {
                continue;
            }
            if let Some(Some(number)) = membership.get(&pr.number) {
                groups.entry(*number).or_default().push(pr.number);
            }
        }
        for numbers in groups.values_mut() {
            numbers.sort_unstable();
            numbers.dedup();
        }
        let mut changed = false;
        // Only cards created by this importer follow the review-request lifecycle.
        for issue in &mut self.issues {
            let Some(import) = &mut issue.stack_review_import else {
                continue;
            };
            if issue.github_stack != Some(import.stack_number) {
                continue;
            }
            let requested = groups
                .get(&import.stack_number)
                .cloned()
                .unwrap_or_default();
            if requested.is_empty()
                && !import.requested_prs.is_empty()
                && issue.column == Column::CodeReview
            {
                issue.column = Column::Done;
                issue.done_at = Some(unix_now());
                changed = true;
            } else if !requested.is_empty()
                && requested
                    .iter()
                    .any(|number| !import.requested_prs.contains(number))
                && issue.column == Column::Done
            {
                issue.column = Column::CodeReview;
                issue.done_at = None;
                changed = true;
            }
            if import.requested_prs != requested {
                import.requested_prs = requested;
                changed = true;
            }
        }
        for (number, requested) in groups {
            let Some(stack) = self
                .live
                .github_stacks
                .iter()
                .find(|stack| stack.number == number)
                .cloned()
            else {
                continue;
            };
            // Discovery and membership can race. Wait for a coherent member list, never
            // fall back to importing the same requests as individual cards.
            if requested
                .iter()
                .any(|number| !stack.pull_requests.iter().any(|pr| pr.number == *number))
            {
                continue;
            }
            let prompt = self.stack_review_prompt(&stack, &requested);
            if let Some(issue) = self
                .issues
                .iter_mut()
                .find(|issue| issue.github_stack == Some(number))
            {
                if let Some(import) = &mut issue.stack_review_import {
                    if protected.contains(&issue.id) {
                        continue;
                    }
                    if issue.prompt.as_deref() == Some(&import.generated_prompt)
                        && issue.prompt.as_deref() != Some(&prompt)
                    {
                        issue.prompt = Some(prompt.clone());
                        changed = true;
                    }
                    if import.generated_prompt != prompt {
                        import.generated_prompt = prompt;
                        changed = true;
                    }
                }
                continue;
            }
            let members: HashSet<_> = stack.pull_requests.iter().map(|pr| pr.number).collect();
            let existing: Vec<_> = self
                .issues
                .iter()
                .filter(|issue| {
                    issue
                        .pr_numbers()
                        .iter()
                        .any(|number| members.contains(number))
                })
                .collect();
            // A manually attached or edited card owns its PRs. Do not create another
            // card over it or silently broaden an explicit PR-only attachment.
            if existing.iter().any(|issue| {
                !issue.is_untouched_pr_import()
                    || protected.contains(&issue.id)
                    || issue.primary_pr_import_source() != Some(PrImportSource::ReviewRequested)
                    || self.is_session_alive(&issue.session_name(&self.config.project_name))
                    || self.detect_worktree(issue).is_some()
                    || self
                        .live
                        .agent_statuses
                        .contains_key(&issue.session_name(&self.config.project_name))
                    || self.marked_issues.contains(&issue.id.to_lowercase())
                    || self.issues.iter().any(|other| {
                        other
                            .linked_issues
                            .iter()
                            .any(|id| id.eq_ignore_ascii_case(&issue.id))
                    })
            }) {
                continue;
            }
            let reusable_id = existing.first().map(|issue| issue.id.clone());
            let old_ids: HashSet<_> = existing.iter().map(|issue| issue.id.clone()).collect();
            let id = reusable_id.unwrap_or_else(|| self.next_issue_id());
            let title = self
                .live
                .review_requested_prs
                .iter()
                .find(|pr| pr.number == requested[0])
                .map(|pr| pr.title.clone())
                .unwrap_or_else(|| format!("Review stack #{number}"));
            self.issues.retain(|issue| !old_ids.contains(&issue.id));
            self.issues.push(Issue {
                github_stack: Some(number),
                prompt: Some(prompt.clone()),
                stack_review_import: Some(StackReviewImport {
                    stack_number: number,
                    requested_prs: requested,
                    generated_prompt: prompt,
                }),
                ..Issue::new(id, title, Column::CodeReview, self.config.agent_kind)
            });
            changed = true;
        }
        if changed {
            self.clamp_all_rows("");
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, AppState};
    use crate::types::{AgentKind, GithubStackPullRequest, LinkedGithubPr, PrStatus};

    fn pr(number: u32) -> PrStatus {
        PrStatus {
            number,
            title: format!("PR {number}"),
            url: format!("https://github.com/owner/repo/pull/{number}"),
            author: "colleague".into(),
            state: PrState::Open,
            is_draft: false,
            checks: None,
            review: None,
            additions: 0,
            deletions: 0,
            head_branch: format!("branch-{number}"),
            is_cross_repository: false,
        }
    }

    fn project() -> Project {
        let mut project = Project::new(
            AppConfig {
                project_name: "test".into(),
                project_root: "/tmp/bork-stack-review-test".into(),
                ..Default::default()
            },
            AppState::default(),
        );
        project.live.pr_poll_done = true;
        project.live.review_prs_ready = Some(true);
        project.live.authored_prs_ready = Some(false);
        project.live.review_stacks = Some(
            [
                (1, Some(10)),
                (2, Some(10)),
                (3, Some(10)),
                (4, Some(10)),
                (5, None),
            ]
            .into(),
        );
        project.live.review_requested_prs = vec![pr(4), pr(2), pr(2), pr(1), pr(3), pr(5)];
        project.live.github_stacks = vec![GithubStack {
            number: 10,
            url: "https://api.github.com/repos/owner/repo/stacks/10".into(),
            base_ref: "main".into(),
            open: true,
            pull_requests: (1..=4)
                .map(|number| GithubStackPullRequest {
                    number,
                    state: PrState::Open,
                    is_draft: false,
                    head_branch: format!("branch-{number}"),
                })
                .collect(),
        }];
        project
    }

    fn sync(project: &mut Project) {
        project.sync_stack_reviews(&HashSet::new());
        project.sync_prs_as_issues();
    }

    #[test]
    fn four_requests_and_duplicates_create_one_stack_plus_one_standalone() {
        let mut project = project();
        for _ in 0..4 {
            sync(&mut project);
        }
        assert_eq!(project.issues.len(), 2);
        let issue = project
            .issues
            .iter()
            .find(|issue| issue.github_stack == Some(10))
            .unwrap();
        assert_eq!(
            issue.stack_review_import.as_ref().unwrap().requested_prs,
            vec![1, 2, 3, 4]
        );
        assert!(project.issues.iter().any(|issue| issue.has_pr_number(5)));
        assert!(project
            .issues
            .iter()
            .all(|issue| issue.github_stack.is_some() || issue.pr_numbers() == vec![5]));
    }

    #[test]
    fn waiting_for_membership_never_creates_individual_stack_cards() {
        let mut project = project();
        let stacks = std::mem::take(&mut project.live.github_stacks);
        sync(&mut project);
        assert_eq!(project.issues.len(), 1);
        assert!(project.issues[0].has_pr_number(5));
        project.live.github_stacks = stacks;
        sync(&mut project);
        assert_eq!(project.issues.len(), 2);
    }

    #[test]
    fn any_member_triggers_import_and_lifecycle_tracks_only_requests() {
        let mut project = project();
        project.live.review_requested_prs = vec![pr(4)];
        sync(&mut project);
        assert_eq!(project.issues.len(), 1);
        let id = project.issues[0].id.clone();
        let prompt = project.issues[0].prompt.as_ref().unwrap();
        assert!(prompt.contains("#1 https://github.com/owner/repo/pull/1 (open, context only)"));
        assert!(prompt
            .contains("#4 https://github.com/owner/repo/pull/4 (open, your review requested)"));
        project.live.review_requested_prs.clear();
        project.live.review_prs_ready = Some(false);
        sync(&mut project);
        assert_eq!(project.issues[0].column, Column::CodeReview);
        project.live.review_prs_ready = Some(true);
        sync(&mut project);
        assert_eq!(project.issues[0].column, Column::Done);
        assert!(project.issues[0].done_at.is_some());
        project.live.review_requested_prs = vec![pr(2)];
        sync(&mut project);
        assert_eq!(project.issues[0].column, Column::CodeReview);
        assert_eq!(project.issues[0].id, id);
        assert!(project.issues[0].done_at.is_none());
    }

    #[test]
    fn existing_explicit_pr_only_card_prevents_automatic_duplicate_group() {
        let mut project = project();
        let mut issue = Issue::new("manual", "My review", Column::CodeReview, AgentKind::Codex);
        issue.github_pr_links.push(LinkedGithubPr {
            number: 1,
            imported: false,
            import_source: None,
        });
        project.issues.push(issue.clone());
        sync(&mut project);
        assert_eq!(project.issues.len(), 2); // explicit PR card and standalone #5
        assert_eq!(project.issues[0], issue);
        assert!(!project
            .issues
            .iter()
            .any(|issue| issue.github_stack == Some(10)));
    }

    #[test]
    fn standalone_auto_import_is_promoted_when_pr_joins_stack() {
        let mut project = project();
        project.live.review_requested_prs = vec![pr(1)];
        project.live.review_stacks = Some([(1, None)].into());
        sync(&mut project);
        let id = project.issues[0].id.clone();
        project.live.review_requested_prs = vec![pr(1), pr(2)];
        project.live.review_stacks = Some([(1, Some(10)), (2, Some(10))].into());
        sync(&mut project);
        assert_eq!(project.issues.len(), 1);
        assert_eq!(project.issues[0].id, id);
        assert_eq!(project.issues[0].github_stack, Some(10));
    }

    #[test]
    fn prompt_overrides_are_separate_and_user_edits_survive_refresh() {
        let mut project = project();
        project.config.review_prompt = Some("Single only".into());
        project.config.stack_review_prompt = Some("Check interactions carefully".into());
        sync(&mut project);
        let stack = project
            .issues
            .iter_mut()
            .find(|issue| issue.github_stack.is_some())
            .unwrap();
        assert!(stack
            .prompt
            .as_ref()
            .unwrap()
            .starts_with("Check interactions carefully"));
        stack.prompt = Some("My custom instructions".into());
        project.config.stack_review_prompt = Some("Changed default".into());
        sync(&mut project);
        let stack = project
            .issues
            .iter()
            .find(|issue| issue.github_stack.is_some())
            .unwrap();
        assert_eq!(stack.prompt.as_deref(), Some("My custom instructions"));
        assert!(project
            .issues
            .iter()
            .find(|issue| issue.has_pr_number(5))
            .unwrap()
            .prompt
            .as_ref()
            .unwrap()
            .contains("Single only"));
        let saved = serde_json::to_string(&project.to_state()).unwrap();
        let state: AppState = serde_json::from_str(&saved).unwrap();
        assert_eq!(state.issues, project.issues);
    }
}
