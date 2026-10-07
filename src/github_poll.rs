use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::app::Project;
use crate::external::github;
use crate::types::{Column, GithubStack, PrState, PrStatus};
use crate::PrPollResult;

const REVIEW_INTERVAL: u64 = 60;
const DISCOVERY_INTERVAL: u64 = 300;
const DONE_BATCH_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy)]
pub enum Wake {
    Refresh,
    TargetsChanged,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Targets {
    pub prs: Vec<(u32, u8)>,
    pub stacks: Vec<(u32, u8)>,
    pub branches: Vec<(String, u8)>,
    pub picker: bool,
    pub auto_reviews: bool,
    pub auto_authored: bool,
}

impl Targets {
    pub fn for_project(project: &Project) -> Self {
        let mut targets = Self {
            auto_reviews: project.config.auto_import_reviews,
            auto_authored: project.config.auto_import_authored_prs,
            ..Self::default()
        };
        for issue in &project.issues {
            let priority = match issue.column {
                Column::CodeReview => 0,
                Column::InProgress => 1,
                Column::Todo => 2,
                Column::Done => 3,
            };
            targets
                .prs
                .extend(issue.pr_numbers().into_iter().map(|n| (n, priority)));
            if let Some(number) = issue.github_stack {
                targets.stacks.push((number, priority));
            } else if !issue.has_pr() {
                if let Some(branch) = project.branch_for(issue) {
                    targets.branches.push((branch.to_string(), priority));
                }
            }
        }
        targets.prs.sort_unstable();
        targets.stacks.sort_unstable();
        targets.branches.sort_unstable();
        targets
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Stamp {
    success: u64,
    attempt: u64,
    failures: u8,
}

impl Stamp {
    fn due(&self, now: u64, interval: u64, force: bool) -> bool {
        let cooldown = if self.failures > 0 {
            (60u64 * 2u64.pow(u32::from(self.failures.saturating_sub(1).min(4)))).min(900)
        } else {
            5
        };
        if self.attempt != 0 && now >= self.attempt && now - self.attempt < cooldown {
            return false;
        }
        force || self.success == 0 || now < self.success || now - self.success >= interval
    }

    fn finish(&mut self, now: u64, success: bool) {
        self.attempt = now;
        if success {
            self.success = now;
            self.failures = 0;
        } else {
            self.failures = self.failures.saturating_add(1);
        }
    }
}

fn interval(priority: u8, state: Option<PrState>) -> u64 {
    match priority {
        0 => 60,
        1 => 120,
        2 => 300,
        _ if matches!(state, Some(PrState::Merged | PrState::Closed)) => 86_400,
        _ => 3_600,
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Cache {
    version: u8,
    remote: String,
    pub result: PrPollResult,
    statuses: HashMap<u32, Stamp>,
    branches: HashMap<String, Stamp>,
    stacks: HashMap<u32, Stamp>,
    reviews: Stamp,
    authored: Stamp,
    picker_prs: Stamp,
    picker_stacks: Stamp,
    done_batch: Stamp,
    retry_after: u64,
}

impl Cache {
    pub fn load(path: &Path, remote: &str) -> Self {
        let cache = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok());
        let mut cache = cache
            .filter(|cache| cache.version == 1 && cache.remote == remote)
            .unwrap_or_else(|| Self {
                version: 1,
                remote: remote.to_string(),
                ..Self::default()
            });
        cache.result.started = false;
        cache.result.loading_more = false;
        cache.result.authored_ready = false;
        cache.result.reviews_ready = false;
        cache
    }

    pub fn save(&self, path: &Path) {
        let Some(parent) = path.parent() else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec(self) else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let temp = path.with_extension(format!("tmp.{}", std::process::id()));
        if std::fs::write(&temp, bytes).is_ok() {
            let _ = std::fs::rename(&temp, path);
        }
    }

    fn incorporate(&mut self, prs: &[PrStatus], now: u64) {
        for pr in prs {
            self.statuses
                .entry(pr.number)
                .or_default()
                .finish(now, true);
            self.result.prs_by_number.insert(pr.number, pr.clone());
        }
        self.result.prs =
            github::index_by_branch(self.result.prs_by_number.values().cloned().collect());
    }

    fn requested_prs(&self, targets: &Targets) -> Vec<(u32, u8)> {
        let mut priorities = HashMap::<u32, u8>::new();
        let mut add = |number, priority| {
            priorities
                .entry(number)
                .and_modify(|p| *p = (*p).min(priority))
                .or_insert(priority);
        };
        for &(number, priority) in &targets.prs {
            add(number, priority);
        }
        for &(number, priority) in &targets.stacks {
            if let Some(stack) = self
                .result
                .stacks
                .as_ref()
                .and_then(|stacks| stacks.iter().find(|s| s.number == number))
            {
                for pr in &stack.pull_requests {
                    if pr.state == PrState::Open {
                        add(pr.number, priority);
                    }
                }
            }
        }
        let mut requested: Vec<_> = priorities.into_iter().collect();
        requested.sort_unstable_by_key(|&(number, priority)| (priority, std::cmp::Reverse(number)));
        requested
    }

    fn due_prs(&self, targets: &Targets, now: u64, force: bool, target_priority: u8) -> Vec<u32> {
        self.requested_prs(targets)
            .into_iter()
            .filter(|&(number, priority)| {
                priority == target_priority
                    && self.statuses.get(&number).unwrap_or(&Stamp::default()).due(
                        now,
                        interval(
                            priority,
                            self.result.prs_by_number.get(&number).map(|pr| pr.state),
                        ),
                        force,
                    )
            })
            .map(|(number, _)| number)
            .take(if target_priority == 3 {
                DONE_BATCH_LIMIT
            } else {
                usize::MAX
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Reviews,
    Authored,
    PickerPrs,
    PickerStacks,
    Stack(u32),
    Statuses(Vec<u32>),
    Branches(Vec<String>),
}

#[derive(Default)]
pub struct Response {
    prs: Vec<PrStatus>,
    stacks: Option<Vec<GithubStack>>,
    unsupported: bool,
    github_user: Option<String>,
    error: Option<String>,
}

pub fn fetch(path: &Path, request: &Request) -> Response {
    let mut response = Response::default();
    let result = match request {
        Request::Reviews => github::fetch_review_requested_prs(path).map(|prs| response.prs = prs),
        Request::Authored => github::fetch_user_prs(path).map(|prs| response.prs = prs),
        Request::PickerPrs => github::fetch_prs(path).map(|prs| response.prs = prs),
        Request::PickerStacks => github::fetch_stacks(path).map(|stacks| {
            response.unsupported = stacks.is_none();
            response.stacks = stacks;
        }),
        Request::Stack(number) => {
            github::fetch_stack(path, *number).map(|stack| response.stacks = Some(vec![stack]))
        }
        Request::Statuses(numbers) => github::fetch_missing_prs(path, numbers, &mut response.prs),
        Request::Branches(branches) => {
            github::fetch_branch_prs(path, branches).map(|prs| response.prs = prs)
        }
    };
    response.error = result.err();
    if matches!(request, Request::Reviews | Request::Authored) && response.error.is_none() {
        response.github_user = github::fetch_current_user(path);
    }
    response
}

pub fn poll(
    cache: &mut Cache,
    targets: &Targets,
    now: u64,
    force: bool,
    mut fetch: impl FnMut(&Request) -> Response,
    mut publish: impl FnMut(&Cache, bool),
) {
    if !force && now < cache.retry_after {
        return;
    }
    let mut throttled = false;
    // Each call publishes completion, so empty queues and errors cannot leave a spinner running.
    let mut run = |cache: &mut Cache, request: Request| {
        if throttled {
            return;
        }
        publish(cache, true);
        let response = fetch(&request);
        let success = response.error.is_none();
        cache.result.error = response.error.clone();
        if response.error.as_ref().is_some_and(|error| {
            let error = error.to_ascii_lowercase();
            error.contains("rate limit") || error.contains("http 429")
        }) {
            cache.retry_after = now + 900;
            throttled = true;
        }
        if response.github_user.is_some() {
            cache.result.github_user = response.github_user;
        }
        match &request {
            Request::Reviews => {
                cache.reviews.finish(now, success);
                if success {
                    cache.result.reviews_ready = true;
                    cache.result.review_requested_prs = response.prs.clone();
                }
            }
            Request::Authored => {
                cache.authored.finish(now, success);
                if success {
                    cache.result.authored_ready = true;
                    cache.result.user_prs = response.prs.clone();
                }
            }
            Request::PickerPrs => cache.picker_prs.finish(now, success),
            Request::PickerStacks => {
                cache.picker_stacks.finish(now, success);
                if success {
                    cache.result.stacks_unsupported = response.unsupported;
                }
            }
            Request::Stack(number) => {
                cache
                    .stacks
                    .entry(*number)
                    .or_default()
                    .finish(now, success);
                if let Some(error) = &response.error {
                    cache.result.stack_errors.insert(*number, error.clone());
                } else {
                    cache.result.stack_errors.remove(number);
                }
            }
            Request::Statuses(numbers) => {
                for number in numbers {
                    cache
                        .statuses
                        .entry(*number)
                        .or_default()
                        .finish(now, response.prs.iter().any(|pr| pr.number == *number));
                }
            }
            Request::Branches(branches) => {
                for branch in branches {
                    cache
                        .branches
                        .entry(branch.clone())
                        .or_default()
                        .finish(now, success);
                }
            }
        }
        cache.incorporate(&response.prs, now);
        if let Some(stacks) = response.stacks {
            if matches!(request, Request::PickerStacks) {
                cache.result.stacks = Some(stacks.clone());
            } else {
                let stored = cache.result.stacks.get_or_insert_with(Vec::new);
                for stack in &stacks {
                    stored.retain(|old| old.number != stack.number);
                    stored.push(stack.clone());
                }
            }
            for stack in stacks {
                cache.result.stack_errors.remove(&stack.number);
                cache
                    .stacks
                    .entry(stack.number)
                    .or_default()
                    .finish(now, true);
            }
        }
        publish(cache, false);
    };

    if targets.auto_reviews && cache.reviews.due(now, REVIEW_INTERVAL, force) {
        run(cache, Request::Reviews);
    }
    if targets.auto_authored && cache.authored.due(now, DISCOVERY_INTERVAL, force) {
        run(cache, Request::Authored);
    }

    let mut stacks = targets.stacks.clone();
    stacks.sort_unstable_by_key(|&(number, priority)| (priority, number));
    let mut branches = targets.branches.clone();
    branches.sort_unstable_by_key(|(branch, priority)| (*priority, branch.clone()));
    branches.dedup_by(|a, b| a.0 == b.0);
    for priority in 0..3 {
        for &(number, stack_priority) in &stacks {
            if stack_priority == priority
                && cache.stacks.get(&number).unwrap_or(&Stamp::default()).due(
                    now,
                    interval(priority, None),
                    force,
                )
            {
                run(cache, Request::Stack(number));
            }
        }
        for batch in cache.due_prs(targets, now, force, priority).chunks(50) {
            run(cache, Request::Statuses(batch.to_vec()));
        }
        let due_branches: Vec<_> = branches
            .iter()
            .filter(|(branch, branch_priority)| {
                *branch_priority == priority
                    && cache.branches.get(branch).unwrap_or(&Stamp::default()).due(
                        now,
                        interval(priority, None),
                        force,
                    )
            })
            .map(|(branch, _)| branch.clone())
            .collect();
        for batch in due_branches.chunks(20) {
            run(cache, Request::Branches(batch.to_vec()));
        }
    }

    if targets.picker {
        if cache.picker_prs.due(now, DISCOVERY_INTERVAL, force) {
            run(cache, Request::PickerPrs);
        }
        if cache.picker_stacks.due(now, DISCOVERY_INTERVAL, force) {
            run(cache, Request::PickerStacks);
        }
    }

    // Persist the backlog cadence too: restarting must not immediately fetch the next 50 Done PRs.
    if !cache.done_batch.due(now, DISCOVERY_INTERVAL, force) {
        return;
    }
    cache.done_batch.finish(now, true);
    let done_branches: Vec<_> = branches
        .iter()
        .filter(|(branch, priority)| {
            *priority == 3
                && cache.branches.get(branch).unwrap_or(&Stamp::default()).due(
                    now,
                    interval(*priority, None),
                    force,
                )
        })
        .map(|(branch, _)| branch.clone())
        .take(20)
        .collect();
    if !done_branches.is_empty() {
        run(cache, Request::Branches(done_branches));
    }
    let done_stack = targets.stacks.iter().find(|(number, priority)| {
        *priority == 3
            && cache.stacks.get(number).unwrap_or(&Stamp::default()).due(
                now,
                interval(*priority, None),
                force,
            )
    });
    if let Some(&(number, _)) = done_stack {
        run(cache, Request::Stack(number));
    }
    for batch in cache.due_prs(targets, now, force, 3).chunks(50) {
        run(cache, Request::Statuses(batch.to_vec()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u32) -> PrStatus {
        PrStatus {
            number,
            title: format!("PR {number}"),
            url: String::new(),
            author: "author".into(),
            state: PrState::Open,
            is_draft: false,
            checks: Some(crate::types::ChecksStatus::Success),
            review: None,
            additions: 1,
            deletions: 0,
            head_branch: format!("branch-{number}"),
            is_cross_repository: false,
        }
    }

    fn stack(number: u32, member: u32) -> GithubStack {
        GithubStack {
            number,
            url: String::new(),
            base_ref: "main".into(),
            open: true,
            pull_requests: vec![crate::types::GithubStackPullRequest {
                number: member,
                state: PrState::Open,
                is_draft: false,
                head_branch: format!("branch-{member}"),
            }],
        }
    }

    fn cycle(cache: &mut Cache, targets: &Targets, now: u64, force: bool) -> Vec<Request> {
        let mut requests = Vec::new();
        poll(
            cache,
            targets,
            now,
            force,
            |request| {
                let response = match request {
                    Request::Statuses(numbers) => Response {
                        prs: numbers.iter().map(|n| pr(*n)).collect(),
                        ..Default::default()
                    },
                    Request::Stack(number) => Response {
                        stacks: Some(vec![stack(*number, *number + 1)]),
                        ..Default::default()
                    },
                    Request::PickerStacks => Response {
                        stacks: Some(vec![stack(42, 43), stack(99, 100)]),
                        ..Default::default()
                    },
                    _ => Response::default(),
                };
                requests.push(request.clone());
                response
            },
            |_, _| {},
        );
        requests
    }

    #[test]
    fn idle_board_does_not_discover_repository_prs_or_stacks() {
        assert!(cycle(&mut Cache::default(), &Targets::default(), 1000, false).is_empty());
        let targets = Targets {
            auto_reviews: true,
            auto_authored: true,
            ..Default::default()
        };
        assert_eq!(
            cycle(&mut Cache::default(), &targets, 1000, false),
            vec![Request::Reviews, Request::Authored]
        );
    }

    #[test]
    fn priority_order_deduplicates_and_bounds_done_work() {
        let mut targets = Targets {
            prs: vec![(3, 2), (2, 1), (1, 0), (1, 3)],
            ..Default::default()
        };
        targets.prs.extend((10..110).map(|n| (n, 3)));
        let mut cache = Cache::default();
        let calls = cycle(&mut cache, &targets, 1000, false);
        assert_eq!(calls[0], Request::Statuses(vec![1]));
        assert_eq!(calls[1], Request::Statuses(vec![2]));
        assert_eq!(calls[2], Request::Statuses(vec![3]));
        assert_eq!(calls[3], Request::Statuses((60..110).rev().collect()));
        let next = cycle(&mut cache, &targets, 1060, false);
        assert_eq!(next[0], Request::Statuses(vec![1]));
        assert_eq!(next.len(), 1);
        let later = cycle(&mut cache, &targets, 1300, false);
        assert_eq!(
            later.last(),
            Some(&Request::Statuses((10..60).rev().collect()))
        );
    }

    #[test]
    fn only_attached_stack_members_get_status_lookups() {
        let mut cache = Cache::default();
        cache.result.stacks = Some(vec![stack(42, 43), stack(99, 100)]);
        cache.stacks.entry(42).or_default().finish(1000, true);
        let targets = Targets {
            stacks: vec![(42, 0)],
            ..Default::default()
        };
        assert_eq!(
            cycle(&mut cache, &targets, 1010, false),
            vec![Request::Statuses(vec![43])]
        );
    }

    #[test]
    fn restarting_reuses_cache_and_remote_changes_invalidate_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-cache.json");
        let targets = Targets {
            prs: vec![(1, 0)],
            auto_reviews: true,
            auto_authored: true,
            ..Default::default()
        };
        let mut cache = Cache::load(&path, "repo-a");
        cycle(&mut cache, &targets, 1000, false);
        cache.save(&path);
        let mut restarted = Cache::load(&path, "repo-a");
        assert!(cycle(&mut restarted, &targets, 1010, false).is_empty());
        assert_eq!(restarted.result.prs_by_number[&1], pr(1));
        let mut different_repo = Cache::load(&path, "repo-b");
        assert!(!cycle(&mut different_repo, &targets, 1010, false).is_empty());
    }

    #[test]
    fn restarting_does_not_start_another_done_backlog_batch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-cache.json");
        let targets = Targets {
            prs: (1..101).map(|n| (n, 3)).collect(),
            ..Default::default()
        };
        let mut cache = Cache::load(&path, "repo");
        assert_eq!(cycle(&mut cache, &targets, 1000, false).len(), 1);
        cache.save(&path);
        let mut restarted = Cache::load(&path, "repo");
        assert!(cycle(&mut restarted, &targets, 1010, false).is_empty());
        assert_eq!(
            cycle(&mut restarted, &targets, 1300, false),
            vec![Request::Statuses((1..51).rev().collect())]
        );
    }

    #[test]
    fn review_branches_are_checked_before_lower_priority_links() {
        let targets = Targets {
            prs: vec![(1, 1)],
            branches: vec![("review-branch".into(), 0)],
            ..Default::default()
        };
        assert_eq!(
            cycle(&mut Cache::default(), &targets, 1000, false),
            vec![
                Request::Branches(vec!["review-branch".into()]),
                Request::Statuses(vec![1])
            ]
        );
    }

    #[test]
    fn picker_discovery_is_lazy_and_cached() {
        let mut cache = Cache::default();
        let targets = Targets {
            picker: true,
            ..Default::default()
        };
        assert_eq!(
            cycle(&mut cache, &targets, 1000, false),
            vec![Request::PickerPrs, Request::PickerStacks]
        );
        assert!(cycle(&mut cache, &targets, 1010, false).is_empty());
        assert!(cycle(&mut cache, &Targets::default(), 1400, false).is_empty());
        assert_eq!(
            cycle(&mut cache, &targets, 1400, false),
            vec![Request::PickerPrs, Request::PickerStacks]
        );
    }

    #[test]
    fn review_discovery_runs_every_minute_without_the_picker() {
        let targets = Targets {
            auto_reviews: true,
            auto_authored: true,
            ..Default::default()
        };
        let mut cache = Cache::default();
        cycle(&mut cache, &targets, 1000, false);
        assert!(cycle(&mut cache, &targets, 1059, false).is_empty());
        assert_eq!(
            cycle(&mut cache, &targets, 1060, false),
            vec![Request::Reviews]
        );
        assert!(cache.result.reviews_ready);
        assert!(cache.result.authored_ready);
    }

    #[test]
    fn done_terminal_prs_refresh_daily_but_moving_to_review_makes_them_due() {
        let mut cache = Cache::default();
        let mut merged = pr(1);
        merged.state = PrState::Merged;
        cache.incorporate(&[merged], 1000);
        let done = Targets {
            prs: vec![(1, 3)],
            ..Default::default()
        };
        assert!(cycle(&mut cache, &done, 4600, false).is_empty());
        assert_eq!(
            cycle(
                &mut cache,
                &Targets {
                    prs: vec![(1, 0)],
                    ..Default::default()
                },
                4600,
                false
            ),
            vec![Request::Statuses(vec![1])]
        );
    }

    #[test]
    fn failures_back_off_and_publish_completion_without_clearing_cached_data() {
        let targets = Targets {
            prs: vec![(1, 0)],
            ..Default::default()
        };
        let mut cache = Cache::default();
        cache.incorporate(&[pr(1)], 900);
        let mut events = Vec::new();
        poll(
            &mut cache,
            &targets,
            1000,
            false,
            |_| Response {
                error: Some("connection failed".into()),
                ..Default::default()
            },
            |_, started| events.push(started),
        );
        assert_eq!(events, vec![true, false]);
        assert_eq!(cache.result.prs_by_number[&1], pr(1));
        assert!(cycle(&mut cache, &targets, 1010, true).is_empty());
        assert_eq!(
            cycle(&mut cache, &targets, 1060, false),
            vec![Request::Statuses(vec![1])]
        );
    }

    #[test]
    fn rate_limit_stops_the_queue_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-cache.json");
        let targets = Targets {
            auto_reviews: true,
            auto_authored: true,
            prs: vec![(1, 0), (2, 1), (3, 3)],
            ..Default::default()
        };
        let mut cache = Cache::load(&path, "repo");
        let mut requests = 0;
        poll(
            &mut cache,
            &targets,
            1000,
            false,
            |_| {
                requests += 1;
                Response {
                    error: Some("API rate limit exceeded".into()),
                    ..Default::default()
                }
            },
            |_, _| {},
        );
        assert_eq!(requests, 1);
        cache.save(&path);
        let mut restarted = Cache::load(&path, "repo");
        assert!(cycle(&mut restarted, &targets, 1060, false).is_empty());
        assert!(restarted
            .result
            .error
            .as_ref()
            .unwrap()
            .contains("rate limit"));
        assert!(!cycle(&mut restarted, &targets, 1900, false).is_empty());
    }

    #[test]
    fn manual_refresh_bypasses_freshness_but_coalesces_immediate_repeats() {
        let targets = Targets {
            prs: vec![(1, 0)],
            ..Default::default()
        };
        let mut cache = Cache::default();
        cycle(&mut cache, &targets, 1000, false);
        assert!(cycle(&mut cache, &targets, 1001, true).is_empty());
        assert_eq!(
            cycle(&mut cache, &targets, 1005, true),
            vec![Request::Statuses(vec![1])]
        );
    }
}
