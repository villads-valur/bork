use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::app::Project;
use crate::external::github;
use crate::types::{Column, GithubStack, PrState, PrStatus};
use crate::PrPollResult;

const REVIEW_INTERVAL: u64 = 60;
const DISCOVERY_INTERVAL: u64 = 300;
const DONE_BATCH_LIMIT: usize = 50;
const MEMBERSHIP_FAILURE_LIMIT: u8 = 3;

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
    picker_numbers: HashSet<u32>,
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
        cache.result.refreshed_stacks.clear();
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

    pub fn snapshot(&self, targets: &Targets) -> PrPollResult {
        let mut result = self.result.clone();
        // Missing active membership briefly delays imports to avoid duplicate cards.
        // Historical stacks and refresh failures must not disable background imports.
        let membership_ready = result.stacks_unsupported
            || targets
                .stacks
                .iter()
                .filter(|(_, priority)| *priority < 3)
                .all(|(number, _)| {
                    result
                        .stacks
                        .as_ref()
                        .is_some_and(|stacks| stacks.iter().any(|s| s.number == *number))
                        || self
                            .stacks
                            .get(number)
                            .is_some_and(|stamp| stamp.failures >= MEMBERSHIP_FAILURE_LIMIT)
                });
        if result.review_stacks.is_none() {
            result.reviews_ready &= membership_ready;
        }
        result.authored_ready &= membership_ready;
        if targets.auto_reviews && result.error.is_none() {
            result.error = result
                .review_stacks
                .iter()
                .flat_map(|map| map.values().flatten())
                .find_map(|number| {
                    result
                        .stack_errors
                        .get(number)
                        .map(|error| format!("Review stack #{number}: {error}"))
                });
        }
        result
    }

    fn prune(&mut self, targets: &Targets, now: u64) {
        let mut stack_numbers: HashSet<_> = targets.stacks.iter().map(|(n, _)| *n).collect();
        stack_numbers.extend(
            self.result
                .review_stacks
                .iter()
                .flat_map(|map| map.values().filter_map(|number| *number)),
        );
        let branches: HashSet<_> = targets.branches.iter().map(|(b, _)| b.as_str()).collect();
        if now.saturating_sub(self.picker_prs.success) >= DISCOVERY_INTERVAL {
            self.picker_numbers.clear();
        }
        if let Some(stacks) = &mut self.result.stacks {
            stacks.retain(|s| {
                stack_numbers.contains(&s.number)
                    || now.saturating_sub(self.picker_stacks.success) < DISCOVERY_INTERVAL
            });
        }
        let mut numbers: HashSet<_> = targets.prs.iter().map(|(n, _)| *n).collect();
        for stack in self
            .result
            .stacks
            .iter()
            .flatten()
            .filter(|s| stack_numbers.contains(&s.number))
        {
            numbers.extend(stack.pull_requests.iter().map(|pr| pr.number));
        }
        numbers.extend(&self.picker_numbers);
        numbers.extend(
            self.result
                .user_prs
                .iter()
                .chain(&self.result.review_requested_prs)
                .map(|p| p.number),
        );
        numbers.extend(
            self.result
                .prs
                .values()
                .filter(|p| branches.contains(p.head_branch.as_str()))
                .map(|p| p.number),
        );
        self.result.prs_by_number.retain(|n, _| numbers.contains(n));
        self.statuses.retain(|n, _| numbers.contains(n));
        self.branches.retain(|b, _| branches.contains(b.as_str()));
        self.stacks.retain(|n, _| {
            stack_numbers.contains(n) || self.result.stacks.iter().flatten().any(|s| s.number == *n)
        });
        self.result
            .stack_errors
            .retain(|n, _| stack_numbers.contains(n));
        self.result.prs =
            github::index_by_branch(self.result.prs_by_number.values().cloned().collect());
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
    pub cancelled: bool,
    prs: Vec<PrStatus>,
    stacks: Option<Vec<GithubStack>>,
    unsupported: bool,
    github_user: Option<String>,
    review_stacks: Option<HashMap<u32, Option<u32>>>,
    error: Option<String>,
}

pub fn fetch(path: &Path, request: &Request) -> Response {
    let mut response = Response::default();
    let result = match request {
        Request::Reviews => github::fetch_review_requested_prs(path).map(|discovery| {
            response.prs = discovery.prs;
            response.review_stacks = Some(discovery.stacks);
        }),
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
    cache.prune(targets, now);
    let mut throttled = false;
    // Each call publishes completion, so empty queues and errors cannot leave a spinner running.
    let mut run = |cache: &mut Cache, request: Request| {
        if throttled {
            return;
        }
        cache.result.refreshed_stacks.clear();
        publish(cache, true);
        let response = fetch(&request);
        if response.cancelled {
            throttled = true;
            return;
        }
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
                    cache.result.review_stacks = response.review_stacks;
                }
            }
            Request::Authored => {
                cache.authored.finish(now, success);
                if success {
                    cache.result.authored_ready = true;
                    cache.result.user_prs = response.prs.clone();
                }
            }
            Request::PickerPrs => {
                cache.picker_prs.finish(now, success);
                if success {
                    cache.picker_numbers = response.prs.iter().map(|p| p.number).collect();
                }
            }
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
                let stored = cache.result.stacks.get_or_insert_with(Vec::new);
                stored.retain(|old| {
                    targets.stacks.iter().any(|(n, _)| *n == old.number)
                        && !stacks.iter().any(|new| new.number == old.number)
                });
                stored.extend(stacks.clone());
            } else {
                let stored = cache.result.stacks.get_or_insert_with(Vec::new);
                for stack in &stacks {
                    stored.retain(|old| old.number != stack.number);
                    stored.push(stack.clone());
                }
            }
            for stack in stacks {
                cache.result.refreshed_stacks.push(stack.number);
                cache.result.stack_errors.remove(&stack.number);
                cache
                    .stacks
                    .entry(stack.number)
                    .or_default()
                    .finish(now, true);
            }
        }
        cache.prune(targets, now);
        publish(cache, false);
    };

    let reviews_due = targets.auto_reviews && cache.reviews.due(now, REVIEW_INTERVAL, force);
    let authored_due = targets.auto_authored && cache.authored.due(now, DISCOVERY_INTERVAL, force);
    // Discovery can find new members before the slower column-based refresh is due.
    // Refresh active membership first, retaining per-stack failure/repeat cooldowns.
    let mut active_stacks = targets.stacks.clone();
    active_stacks.sort_unstable_by_key(|&(n, p)| (p, n));
    for (number, priority) in active_stacks {
        if priority < 3
            && cache.stacks.get(&number).unwrap_or(&Stamp::default()).due(
                now,
                interval(priority, None),
                force || reviews_due || authored_due,
            )
        {
            run(cache, Request::Stack(number));
        }
    }

    if reviews_due {
        run(cache, Request::Reviews);
    }
    let mut review_stacks: Vec<_> = if targets.auto_reviews {
        cache
            .result
            .review_stacks
            .iter()
            .flat_map(|map| map.values().filter_map(|number| *number))
            .collect()
    } else {
        Vec::new()
    };
    review_stacks.sort_unstable();
    review_stacks.dedup();
    for number in review_stacks {
        if cache
            .stacks
            .get(&number)
            .unwrap_or(&Stamp::default())
            .due(now, REVIEW_INTERVAL, force)
        {
            run(cache, Request::Stack(number));
        }
    }
    if authored_due {
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
    fn review_discovery_fetches_each_stack_once_and_keeps_it_before_board_attachment() {
        let targets = Targets {
            auto_reviews: true,
            ..Default::default()
        };
        let mut cache = Cache::default();
        let mut requests = Vec::new();
        poll(
            &mut cache,
            &targets,
            1000,
            false,
            |request| {
                requests.push(request.clone());
                match request {
                    Request::Reviews => Response {
                        prs: vec![pr(1), pr(2), pr(2), pr(3)],
                        review_stacks: Some([(1, Some(42)), (2, Some(42)), (3, None)].into()),
                        ..Default::default()
                    },
                    Request::Stack(42) => Response {
                        stacks: Some(vec![stack(42, 1)]),
                        ..Default::default()
                    },
                    _ => Response::default(),
                }
            },
            |_, _| {},
        );
        assert_eq!(requests, vec![Request::Reviews, Request::Stack(42)]);
        assert_eq!(cache.result.stacks.as_ref().unwrap()[0].number, 42);
        assert!(cycle(&mut cache, &targets, 1010, false).is_empty());
    }

    #[test]
    fn failed_review_discovery_preserves_membership_and_requests() {
        let targets = Targets {
            auto_reviews: true,
            ..Default::default()
        };
        let mut cache = Cache::default();
        cache.result.review_requested_prs = vec![pr(1)];
        cache.result.review_stacks = Some([(1, Some(42))].into());
        poll(
            &mut cache,
            &targets,
            1000,
            false,
            |_| Response {
                error: Some("offline".into()),
                ..Default::default()
            },
            |_, _| {},
        );
        assert_eq!(cache.result.review_requested_prs.len(), 1);
        assert_eq!(cache.result.review_stacks.as_ref().unwrap()[&1], Some(42));
        assert!(!cache.result.reviews_ready);
    }

    #[test]
    fn successful_stack_refresh_signal_is_not_replayed_from_cache_or_other_requests() {
        let targets = Targets {
            stacks: vec![(42, 0)],
            auto_reviews: true,
            ..Default::default()
        };
        let mut cache = Cache::default();
        let mut signals = Vec::new();
        poll(
            &mut cache,
            &targets,
            1000,
            false,
            |request| match request {
                Request::Stack(_) => Response {
                    stacks: Some(vec![stack(42, 43)]),
                    ..Default::default()
                },
                _ => Response::default(),
            },
            |cache, started| {
                if !started {
                    signals.push(cache.result.refreshed_stacks.clone());
                }
            },
        );
        assert_eq!(
            signals.iter().filter(|numbers| !numbers.is_empty()).count(),
            1
        );
        assert_eq!(signals[0], vec![42]);
        cache.result.refreshed_stacks = vec![42];
        let bytes = serde_json::to_vec(&cache).unwrap();
        let loaded: Cache = serde_json::from_slice(&bytes).unwrap();
        assert!(loaded.result.refreshed_stacks.is_empty());
    }

    #[test]
    fn discovery_waits_for_attached_membership_even_when_refresh_fails() {
        let targets = Targets {
            auto_reviews: true,
            stacks: vec![(42, 0)],
            ..Default::default()
        };
        let mut cache = Cache::default();
        let mut calls = Vec::new();
        poll(
            &mut cache,
            &targets,
            1000,
            false,
            |request| {
                calls.push(request.clone());
                match request {
                    Request::Stack(_) => Response {
                        error: Some("offline".into()),
                        ..Default::default()
                    },
                    Request::Reviews => Response {
                        prs: vec![pr(43)],
                        ..Default::default()
                    },
                    _ => Response::default(),
                }
            },
            |cache, started| {
                if !started {
                    assert!(!cache.snapshot(&targets).reviews_ready);
                }
            },
        );
        assert_eq!(calls, vec![Request::Stack(42), Request::Reviews]);
        assert!(cache.result.reviews_ready);
        cycle(&mut cache, &targets, 1060, false);
        let snapshot = cache.snapshot(&targets);
        assert!(snapshot.reviews_ready);
        assert_eq!(snapshot.stacks.unwrap()[0].pull_requests[0].number, 43);
    }

    #[test]
    fn done_stacks_never_gate_discovery_even_when_missing_or_broken() {
        for count in [1, 8, 13, 30] {
            let targets = Targets {
                auto_reviews: true,
                auto_authored: true,
                stacks: (1..=count).map(|number| (number, 3)).collect(),
                ..Default::default()
            };
            let mut cache = Cache::default();
            for now in (1000..15400).step_by(60) {
                poll(
                    &mut cache,
                    &targets,
                    now,
                    false,
                    |request| match request {
                        Request::Stack(1) => Response {
                            error: Some("HTTP 404".into()),
                            ..Default::default()
                        },
                        Request::Stack(number) => Response {
                            stacks: Some(vec![stack(*number, *number + 100)]),
                            ..Default::default()
                        },
                        _ => Response::default(),
                    },
                    |cache, started| {
                        if !started {
                            let snapshot = cache.snapshot(&targets);
                            assert_eq!(snapshot.reviews_ready, cache.result.reviews_ready);
                            assert_eq!(snapshot.authored_ready, cache.result.authored_ready);
                        }
                    },
                );
                assert!(cache.snapshot(&targets).reviews_ready);
                assert!(cache.snapshot(&targets).authored_ready);
            }
        }
    }

    #[test]
    fn cached_membership_allows_imports_after_refresh_failure() {
        let targets = Targets {
            auto_reviews: true,
            auto_authored: true,
            stacks: vec![(42, 0)],
            ..Default::default()
        };
        let mut cache = Cache::default();
        cache.result.stacks = Some(vec![stack(42, 43)]);
        poll(
            &mut cache,
            &targets,
            1000,
            false,
            |request| match request {
                Request::Stack(_) => Response {
                    error: Some("offline".into()),
                    ..Default::default()
                },
                _ => Response::default(),
            },
            |_, _| {},
        );
        assert!(cache.snapshot(&targets).reviews_ready);
        assert!(cache.snapshot(&targets).authored_ready);
        assert!(cache.result.stack_errors.contains_key(&42));
    }

    #[test]
    fn unknown_active_membership_stops_gating_after_three_failures_and_restart() {
        let targets = Targets {
            auto_reviews: true,
            auto_authored: true,
            stacks: vec![(42, 0)],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-cache.json");
        let mut cache = Cache::load(&path, "repo");
        for (now, ready) in [(1000, false), (1060, false), (1180, true), (1480, true)] {
            poll(
                &mut cache,
                &targets,
                now,
                false,
                |request| match request {
                    Request::Stack(_) => Response {
                        error: Some("HTTP 404".into()),
                        ..Default::default()
                    },
                    _ => Response::default(),
                },
                |_, _| {},
            );
            assert_eq!(cache.snapshot(&targets).reviews_ready, ready);
            assert_eq!(cache.snapshot(&targets).authored_ready, ready);
            if now == 1180 {
                cache.save(&path);
                cache = Cache::load(&path, "repo");
                assert_eq!(cache.stacks[&42].failures, 3);
            }
        }
    }

    #[test]
    fn unsupported_stacks_do_not_gate_successful_discovery() {
        let targets = Targets {
            stacks: vec![(42, 0)],
            ..Default::default()
        };
        let mut cache = Cache::default();
        cache.result.stacks_unsupported = true;
        assert!(!cache.snapshot(&targets).reviews_ready);
        cache.result.reviews_ready = true;
        cache.result.authored_ready = true;
        assert!(cache.snapshot(&targets).reviews_ready);
        assert!(cache.snapshot(&targets).authored_ready);
    }

    #[test]
    fn discovery_refreshes_slower_active_stacks_before_publishing_new_members() {
        for priority in [1, 2] {
            for authored in [false, true] {
                let targets = Targets {
                    stacks: vec![(42, priority), (42, priority)],
                    auto_reviews: !authored,
                    auto_authored: authored,
                    ..Default::default()
                };
                let mut cache = Cache::default();
                cycle(&mut cache, &targets, 1000, false);
                let now = if authored {
                    cycle(&mut cache, &targets, 1240, false);
                    1300
                } else {
                    1060
                };
                let mut calls = Vec::new();
                let discovery = if authored {
                    Request::Authored
                } else {
                    Request::Reviews
                };
                poll(
                    &mut cache,
                    &targets,
                    now,
                    false,
                    |request| {
                        calls.push(request.clone());
                        match request {
                            Request::Stack(_) => Response {
                                stacks: Some(vec![stack(42, 44)]),
                                ..Default::default()
                            },
                            request if *request == discovery => Response {
                                prs: vec![pr(44)],
                                ..Default::default()
                            },
                            _ => Response::default(),
                        }
                    },
                    |cache, started| {
                        let snapshot = cache.snapshot(&targets);
                        let imported = if authored {
                            &snapshot.user_prs
                        } else {
                            &snapshot.review_requested_prs
                        };
                        if !started && imported.iter().any(|p| p.number == 44) {
                            assert!(snapshot
                                .stacks
                                .unwrap()
                                .iter()
                                .any(|s| s.pull_requests.iter().any(|p| p.number == 44)));
                        }
                    },
                );
                assert_eq!(&calls[..2], &[Request::Stack(42), discovery]);
                assert_eq!(
                    calls
                        .iter()
                        .filter(|r| matches!(r, Request::Stack(_)))
                        .count(),
                    1
                );
            }
        }
    }

    #[test]
    fn discovery_does_not_bypass_stack_failure_backoff() {
        let targets = Targets {
            stacks: vec![(42, 2)],
            auto_reviews: true,
            ..Default::default()
        };
        let mut cache = Cache::default();
        for now in [1000, 1060, 1120] {
            let mut calls = Vec::new();
            poll(
                &mut cache,
                &targets,
                now,
                false,
                |request| {
                    calls.push(request.clone());
                    match request {
                        Request::Stack(_) => Response {
                            error: Some("offline".into()),
                            ..Default::default()
                        },
                        _ => Response::default(),
                    }
                },
                |_, _| {},
            );
            assert!(calls.contains(&Request::Reviews));
            assert_eq!(calls.contains(&Request::Stack(42)), now != 1120);
        }
    }

    #[test]
    fn active_stacks_keep_column_cadence_without_due_discovery() {
        let targets = Targets {
            stacks: vec![(42, 1)],
            ..Default::default()
        };
        let mut cache = Cache::default();
        cycle(&mut cache, &targets, 1000, false);
        assert!(cycle(&mut cache, &targets, 1060, false).is_empty());
        assert!(cycle(&mut cache, &targets, 1120, false).contains(&Request::Stack(42)));
    }

    #[test]
    fn picker_preserves_attached_stacks_omitted_from_listing() {
        let mut cache = Cache::default();
        cache.result.stacks = Some(vec![stack(7, 8)]);
        cache.stacks.entry(7).or_default().finish(1000, true);
        let targets = Targets {
            picker: true,
            stacks: vec![(7, 0)],
            ..Default::default()
        };
        cycle(&mut cache, &targets, 1010, false);
        assert!(cache
            .result
            .stacks
            .as_ref()
            .unwrap()
            .iter()
            .any(|s| s.number == 7));
        assert_eq!(cache.requested_prs(&targets), vec![(8, 0)]);
    }

    #[test]
    fn cache_prunes_expired_picker_data_but_preserves_board_and_discovery() {
        let mut cache = Cache::default();
        let targets = Targets {
            picker: true,
            prs: vec![(1, 0)],
            ..Default::default()
        };
        poll(
            &mut cache,
            &targets,
            1000,
            false,
            |request| match request {
                Request::PickerPrs => Response {
                    prs: (1..101).map(pr).collect(),
                    ..Default::default()
                },
                _ => Response::default(),
            },
            |_, _| {},
        );
        cache.result.review_requested_prs = vec![pr(2)];
        cache
            .branches
            .entry("deleted".into())
            .or_default()
            .finish(1000, true);
        cache.prune(
            &Targets {
                prs: vec![(1, 0)],
                ..Default::default()
            },
            1400,
        );
        assert_eq!(cache.result.prs_by_number.len(), 2);
        assert!(cache.result.prs_by_number.contains_key(&1));
        assert!(cache.result.prs_by_number.contains_key(&2));
        assert_eq!(cache.statuses.len(), 2);
        assert!(cache.branches.is_empty());
    }

    #[test]
    fn cancelled_request_stops_cycle_without_publishing_or_mutating_cache() {
        let targets = Targets {
            auto_reviews: true,
            auto_authored: true,
            ..Default::default()
        };
        let mut calls = 0;
        let mut completed = 0;
        let mut cache = Cache::default();
        poll(
            &mut cache,
            &targets,
            1000,
            false,
            |_| {
                calls += 1;
                Response {
                    cancelled: true,
                    ..Default::default()
                }
            },
            |_, started| {
                if !started {
                    completed += 1;
                }
            },
        );
        assert_eq!(calls, 1);
        assert_eq!(completed, 0);
        assert_eq!(cache.reviews.attempt, 0);
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
