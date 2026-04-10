# Current Work

> Last updated: 2026-04-10

## Active Task

**Task:** Add assigned GitHub review requests to the Code Review column
**Status:** Implementation complete, pending review

## Changes

### types.rs
- Added `PrImportSource` enum (`Authored`, `ReviewRequested`) with Display impl
- Added `pr_import_source: Option<PrImportSource>` field to `Issue`

### external/github.rs
- Added `fetch_review_requested_prs()` using GitHub search: `is:pr is:open review-requested:<user> -author:<user>`

### main.rs
- Extended `PrPollResult` with `review_requested_prs` field
- Added 4th parallel thread in PR poll worker for review-requested PRs
- Store `review_requested_prs` in live state, include in title sync

### app.rs
- Extended `LiveState` with `review_requested_prs: Vec<PrStatus>`
- Updated `has_github_prs()` to include review-requested PRs
- Updated `branch_for()` and `pr_for()` to search review-requested PRs
- Rewrote `sync_prs_as_issues()`:
  - Imports from union of authored + review-requested PRs
  - Authored imports: removed when PR disappears (existing behavior)
  - ReviewRequested imports: auto-moved to Done when no longer pending
  - Review imports get a review-focused default prompt
- Updated `filtered_github_prs()` to include review-requested PRs in picker
- Added `pr_import_source` to `merge_issue_fields`

### ui/card.rs
- Shows yellow "review" badge on status line for ReviewRequested issues

### handler.rs
- Added `pr_import_source` to all Issue construction sites
- Manual picker import sets `PrImportSource::Authored`
- Dialog attach/clear resets `pr_import_source` to None

### worktree.rs, external/opencode.rs
- Added `pr_import_source: None` to Issue construction in tests/CLI

## Progress

- [x] PrImportSource enum + Issue field
- [x] fetch_review_requested_prs() in github.rs
- [x] PR poll worker extended with 4th parallel fetch
- [x] LiveState + sync_prs_as_issues() rewritten
- [x] pr_for() / branch_for() / filtered_github_prs() updated
- [x] Review badge on cards
- [x] All Issue construction sites updated
- [x] Build passes (cargo check + clippy + fmt)
- [x] Tests pass (460/460)
