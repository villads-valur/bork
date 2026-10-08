# bork-194: stacked PR UI

## Agreed direction

- GitHub picker marks stacked PRs. Enter keeps the current single-PR behavior.
- Explicit stack attachment follows changing membership rather than copying PR links.
- Compact stack summary with checks; expand to a bounded, scrollable PR list.
- `o` opens all open members; `R` reviews the stack through tuicr.
- Remove card diff counts. Keep ordinary PR workflows unchanged.

## Implementation

- [x] Inspect picker, persistence, polling, card rendering, and tuicr CLI.
- [x] Create worktree and fast-forward to known origin/main.
- [x] Add attachment persistence and shared membership/status resolution.
- [x] Add picker attachment and stack summary/detail UI.
- [x] Add browser and tuicr actions.
- [x] Verify formatting, tests, clippy, and rendering.

## Decisions

- User approved Ctrl+S for stack attachment, preserving type-to-search and Enter.
- User chose separate tuicr reviews in stack order, with a continue/stop prompt.
- User approved regression tests.
- `s` on the board expands an attached stack; j/k navigates; Esc closes.
- One stack attachment per issue; existing individual links remain stored.
- Stack-wide actions target open members. Merged and closed members remain visible.

## Verification

- Full suite before final error-handling fixes: 896 passed, 1 ignored.
- All 12 stack regression tests passed after the final fixes.
- Clippy with all targets and warnings denied passed.
- Debug executable built at target/debug/bork; installed Bork is unchanged.
- Formatting and diff whitespace checks passed.
- Renderer tests cover 120 PRs and narrow cards. Shell tests use a fake tuicr to
  verify ordering, continuing, stopping, and failure handling.
- No live browser launch or GitHub-backed tuicr review was performed.

## Implementation notes

- Persist only the stack number; resolve its current membership from GitHub polling.
- Fetch missing member statuses in batches beyond the existing latest-100-PR query.
- A failed stack refresh disables stack-wide actions and displays unknown status.
- Reject a stack with malformed members rather than opening an incomplete list.
- Existing ordinary PR review command remains unchanged.
- Changes are uncommitted on bork-194/stacked-pr-ui.

## Debug-build feedback fixes

- Restored compact, individually colored checks/review icons and kept draft/state labels.
- Editor stores saved PR links directly, independent of live GitHub status.
- Picker includes linked PRs whose status is unavailable, so users can still detach them.
- Saving with partial live data preserves all links and their import metadata.
- PR worker explicitly fetches missing linked PRs beyond the latest-100 query;
  linked-number changes wake the worker.
- Full suite: 900 passed, 1 ignored, including offline and partially loaded editor cases.

## Large-repository startup fix

- Verified GitHub authentication and both reported PRs against the live legora repository.
- Found 721 stacks with 2,416 unique PRs; the worker held back all results until
  every historical member's status had loaded.
- Now publish the initial PR/stack result immediately, then publish each batch.
- Fetch only open stack members plus explicitly linked PRs, with linked PRs first.
- Keep previously loaded statuses visible during supplementary refresh batches.
- Show loading state separately from unavailable data and display GitHub command errors.
- Full suite passed (900 passed, 2 ignored); one ignored test is the new opt-in live poll check.
- Live poll check returned initial data in 7.1 seconds and both requested PRs in
  the first supplementary batch at 11.0 seconds.

## Picker footer and ordering

- Footer has one shortcut row (Enter, Ctrl+s for stacks, Ctrl+r when space allows,
  Esc) and a right-aligned count. Loading/error text stays on its own row.
- Attached PRs and members of an attached stack sort first in attach mode;
  each group is newest-first by PR number, regardless of open/closed state.
- Toggling an attachment keeps keyboard focus on the same PR after it moves.
- All 19 picker tests passed, including sorting/focus and narrow-footer cases;
  clippy passed with warnings denied.

## Card layout refinement

- Small stacks show one row per PR with connected lines and the existing colored
  checks/review icons, draft labels, and merged/closed states.
- A subtle `s expand` hint appears when there is space; it never displaces status.
- Larger stacks use a PR count and individually colored check counts, with no
  prominent stack ID or `CI: ... passed` sentence.

## Card footer, type labels, and optional GitHub support

- Stack connectors start at the first PR row (`┌`), without extending above it.
- Linked-issue count moved before Linear IDs in the common footer; `s expand`
  stays at bottom-right. Medium cards now have four content rows to keep the footer.
- Removed the no-worktree `ø` indicator. Orchestrator/todo types follow the issue
  ID in the same border title (`bork-180 · orch`).
- Individual PRs show additions/deletions unless draft; stacks omit diff counts.
- Worker announces refresh start before network calls. Missing data shows loading
  during all fetch phases; failed refreshes retain cached status and expose errors.
- Missing gh hides live GitHub UI. Stack API 404/410/501 hides optional stack controls;
  authentication/network failures remain errors. No gh-stack extension is required.
- Added regression coverage for footer placement, type labels, draft diffs, loading
  vs errors, optional capabilities, and unsupported endpoint classification.

## Stack summary follow-up

- Larger stacks now use one row, e.g. `5 PRs · ◌ 5 pending`; mixed states use
  compact counts, and narrow cards omit whole status groups with an ellipsis.
- Removed the card's `s expand` hint. The app shortcut bar shows `s view stack`
  only for the selected stack issue when stack support is enabled.
- Merged individual PRs omit additions/deletions, just like draft PRs.

## Loading spinners

- Reused the five-dot animated spinner in the picker/stack panel bottom-right.
- Global bottom-right spinner tracks GitHub requests for visible projects.
- Reserved spinner space separately from the update notice and footer shortcuts.
- Removed routine loading text from issue/PR rows, retaining cached statuses and
  identifiers. Errors remain readable in the picker and stack details.

- Spinner visibility now requires an explicit in-flight GitHub request, not merely
  an uninitialized project. Same-screen regression covers completion and failures.
- Live Legora smoke test loaded 453 PR statuses from 734 stacks in 47.2 seconds,
  with the final result correctly clearing loading.
- Update notice shortened to `↑ Update: bork update`.

## Demand-driven GitHub polling

- Added `github_poll.rs`: targeted board polling, ordered by Review / In Progress / Todo / Done.
- Review discovery stays automatic every minute; authored discovery every five minutes. Existing config toggles still apply.
- Latest-100 PR listing and repository stack discovery run only for an open GitHub picker. Attached stacks use the individual stack endpoint, verified against Legora.
- Persist statuses, metadata, freshness, and retry state in `.bork/github-cache.json`, invalidated on remote change. Save after each request/batch.
- Bound Done lookups to 50 per five minutes, including across restarts. Open Done statuses refresh hourly, terminal ones daily.
- Added retry backoff and a persisted rate-limit pause. Manual refresh bypasses freshness with a brief repeat cooldown.
- Guarded auto-import reconciliation until its corresponding discovery succeeds; cached/partial failures must not delete imported issues or falsely complete reviews. Own PRs are not imported as reviews while authored discovery is pending.
- Added regression coverage for priority, scope, cache restarts, lazy picker discovery, background reviews, cold Done backlogs, retries, and import readiness.

## Final UI polish

- Spinner has a two-cell gap before it in both modal and global footers.
- Update notice reads `↑ Update Available`.
- Final pre-PR feature suite: 930 passed, 2 ignored; Clippy clean.
