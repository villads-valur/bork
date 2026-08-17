# Current Work

> Last updated: 2026-08-17

## Active Task

**Task:** bork-105 — Auto-prune mechanism for stale worktrees
**Status:** Implementation complete, verified (check + clippy + fmt + 629 tests), uncommitted

## Design

- **Scheduled check:** main event loop checks every 60s per project; when the
  on-disk worktree count (excluding `main/`) reaches `prune_threshold`
  (default 10) and `auto_prune_check_interval` (default 24h) has passed since
  `last_prune_at`, a toast suggests pruning. A 5-min in-session cooldown
  (`last_auto_prune_prompt`) stops the toast re-flashing.
- **Manual trigger:** `p` in normal mode opens the prune dialog; `bork prune`
  CLI with `--dry-run`, `--yes`, `--include`, `--exclude`.
- **Dialog:** lists all worktrees with issue id, dirty/session/column state.
  Space toggles keep/remove, `a` all, `n` none, Enter confirms, Esc cancels.
  Defaults: remove clean Done/orphan worktrees; keep dirty, live-session, or
  non-Done ones. Submitting with a dirty worktree selected is refused.
- **Execution:** `git worktree remove` (never `--force`) per selection, run in
  a background thread; results flow back via `ActionResult.prune_outcome`.
- **Persistence:** `last_prune_at` lives in `.bork/state.json` (AppState), not
  config — it's machine-written state; keeping it out of config.toml avoids the
  global-layer merge footgun and write churn in a user-edited file. External
  writes merge via "later timestamp wins" in `merge_external_state`. Issues
  keep their card; `issue.worktree` cleared, `issue.pruned_at` set, card shows
  "pruned 3d ago". `pruned_at` clears when a new worktree is attached.

## Files

- `src/prune.rs` (new) — scan, classify, execute, apply-to-issues + tests
- `src/ui/prune_dialog.rs` (new) — dialog renderer
- `src/config.rs` — `prune_threshold` + `auto_prune_check_interval` config
  keys; `last_prune_at` on `AppState` (state.json)
- `src/types.rs` — `Issue.pruned_at`
- `src/app.rs` — `PruneDialogState`, `InputMode::PruneDialog`, open/close
- `src/handler.rs` — dialog action handling, submit, background removal
- `src/main.rs` — `bork prune` subcommand, scheduled toast, outcome apply
- `src/ui/card.rs` — "pruned Xd ago" indicator + `humanize_age`
- `src/input/{action,keybindings}.rs`, `src/ui/{help,mod}.rs`, `src/ops.rs`,
  `src/worktree.rs`, `src/external/opencode.rs`, `README.md` — wiring

## Progress

- [x] prune module with conservative defaults + tests
- [x] Interactive dialog (toggle/all/none, dirty refusal)
- [x] `bork prune` CLI (dry-run/yes/include/exclude)
- [x] Scheduled per-project prompt with threshold + interval + cooldown
- [x] `last_prune_at` persisted to state.json (moved out of config per review)
- [x] Card shows pruned date; clears on worktree reattach
- [x] Simplify pass applied (4-angle review): shared candidate builder for
  TUI+CLI, `partition_selection` as the single dirty-refusal policy, git as
  the sole remove-time authority (dropped `was_dirty`/`SkippedDirty`),
  `action` lives on the candidate (no parallel vectors), `ActionResult`
  Default, `Issue::attach_worktree`, shared `unix_now()`,
  `Project::prunable_worktree_names()`, check-throttled toast
- [x] cargo check + fmt clean, 627/627 tests pass
- [ ] Commit + PR

## Notes

- 6 clippy warnings exist on this branch, all on unchanged lines inherited
  from main — not introduced here.
