# Current Work

> Last updated: 2026-08-26

## Active Task

**Task:** bork-148 — Error handling for integrations missing
**Status:** Implementation complete, reviewed, verified (787 tests + clippy), uncommitted

## Problem

Pressing `o` (open PR) / `O` (open Linear) gave no feedback when the underlying
command failed (gh missing, unauthenticated, PR gone, no URL handler). The old
code discarded the `Command` output entirely.

## Design

- New `src/external/browser.rs` with `open_url(url) -> Result<(), String>` —
  `open` on macOS, `xdg-open` elsewhere; the error is the first non-empty
  stderr line so it fits the one-row status bar.
- `Action::OpenPR` / `Action::OpenLinear` in `src/handler.rs` now use the
  established async-report pattern (`begin_busy()` + `set_message` + spawned
  thread sends `ActionResult` over `action_tx`), same shape as
  `OpenTerminal`/`OpenReview`.
- PRs open via `github::pr_url` (cached repo identity) instead of
  `gh pr view --web`: no gh network round trip per PR, no `$GH_BROWSER`
  blocking risk. `github::open_pr_in_browser` deleted.
- Shared pure `summarize_open_links(noun, total, first_label, failures)` builds
  the status message: "Opened PR #42" / "Opened 3 PRs" /
  "Opened 2 of 3 PRs; #2: <err> (+1 more failed)" /
  "Failed to open PR #42: <err>". Partial failures name what failed so the
  user knows not to retry the links that worked.

## Files

- `src/external/browser.rs` (new) — `open_url`
- `src/external/mod.rs` — register module
- `src/external/github.rs` — remove `open_pr_in_browser`
- `src/handler.rs` — rewritten OpenPR/OpenLinear arms, `summarize_open_links`
  + 5 tests on the summarizer

## Progress

- [x] Implementation
- [x] /code-review: 6 findings, all addressed (partial-failure reporting,
      stderr first-line only, no real subprocesses in tests, pr_url instead of
      gh pr view, dedup via shared summarizer)
- [x] /simplify (4-angle): browser.rs module split, xdg-open fallback, pure
      summarizer instead of fn-pointer test seam, single ActionResult build
- [x] cargo build + 787 tests + clippy clean (verified before worktree move)
- [ ] Commit + PR

## Out of scope (candidate follow-up issues)

- Background polls swallow errors: gh graphql polls return an empty Vec on
  failure (indistinguishable from "no PRs") and `main.rs` discards Linear poll
  errors with `unwrap_or_default()`. Needs debouncing to avoid nagging every
  poll cycle.
- Unifying stderr-first-line extraction with the inline copy in `prune.rs`.
