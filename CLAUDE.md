# Bork

Terminal kanban board for orchestrating OpenCode/Claude Code/Codex/Pi/Cursor coding sessions across git worktrees and tmux.

## Architecture

- **Language**: Rust (no async runtime, pure `std::thread` + `mpsc`)
- **TUI**: ratatui + crossterm
- **External tools**: tmux, git, gh, linear (optional), tuicr (optional), and the coding agents: `opencode`, `claude`, `codex`, `pi`, `cursor-agent` (all via `std::process::Command`)

### Threading Model

```
Main Thread (50ms tick event loop)
├── Shared workers (one each, app-wide)
│   ├── Tmux Session Worker (polls every 2s - one global `tmux list-sessions`)
│   ├── Port Poll Worker (polls every 10s - listening TCP ports via lsof/ps)
│   ├── Linear Worker (polls every 45s - assigned Linear issues + open projects, conditional on `linear` CLI)
│   ├── tuicr Draft Worker (polls every 10s - unposted tuicr comments, conditional on `tuicr`)
│   ├── Activity Poller (polls every 5s - agent status dirs for all registered projects, feeds the sidebar's needs-you dots)
│   ├── Update Check Worker (every 6h - new-version banner, plus cache mtime poll every 2s)
│   └── Backup Worker (checks 15s after startup, then hourly; runs `backup::run` when no
│                      success in 24h or on `Shift+B`; feeds the header's backup warning)
├── Primary ProjectWorkers (for focused project)
│   ├── Agent Status Worker (polls every 2s - agent status files)
│   ├── Git Status Worker (adaptive: in-progress worktrees ~5s, others 15-30s, done
│   │                       skipped; per-worktree `git status` under a global
│   │                       concurrency bound + one batched `git worktree list
│   │                       --porcelain` for branches)
│   └── PR Status Worker (polls every 60s - GitHub PRs via gh api graphql)
├── Swimlane Workers (one ProjectWorkers set per visible swimlane, excluding focused)
├── Background PR Workers (per hidden project: PR poll every 180s + branch resolution
│                          every 60s into `live.background_branches`; feed the sidebar's
│                          needs-you dot only, no imports, assignment or cleanup)
└── Action Threads (fire-and-forget per user action)
```

Workers send results over per-worker `mpsc` channels drained on the main loop tick.
Results are diffed against the previous data before triggering a redraw. Wake
channels (`sleep_with_wake`) let user actions trigger an immediate poll; queued
wakes are coalesced. While a tmux popup or external editor owns the terminal,
all pollers idle via a shared `poll_suspended` flag.

### Data Flow

```
KeyEvent → map_key_to_action() → Action → handle_action() → App mutation
```

All rendering is pure: UI functions take `&App` and produce widgets, never mutate state.

All user-facing actions route through `active_project()` / `active_project_mut()` which returns the project in the currently focused swimlane (not necessarily the primary focused project).

### File Structure

```
src/
├── main.rs           # Entry point, CLI (clap), event loop, terminal setup, worker management
├── app.rs            # App/Project/LiveState/SidebarState structs, navigation, worktree detection
├── agent_config.rs   # Agent preferences from layered config + PATH detection
├── toml_lite.rs      # Shared minimal TOML reader for config files
├── handler.rs        # Action dispatch, state mutations, dialog submit/confirm
├── undo.rs           # Single-slot undo (UndoEntry + per-field Change variants, 10s window)
├── config.rs         # Config/state persistence (atomic writes)
├── global_config.rs  # Global project registry (~/.config/bork/projects.json)
├── types.rs          # Domain types (Issue, Column, AgentKind, IssueKind, PrStatus, etc.)
├── error.rs          # Error types
├── init.rs           # `bork init` subcommand (clone repo, scaffold .bork/ directory)
├── lock.rs           # Single-instance PID file lock + signal handlers (SIGTERM, SIGHUP)
├── worktree.rs       # `bork worktree` subcommand (create git worktree, register with state)
├── issue_plan.rs     # Save an issue's plan before its worktree is removed; drop `plans/<id>/` on delete
├── backup/           # Backups (library; `bork backup` CLI in main.rs is a thin layer)
│   ├── mod.rs        # Selection, plan/run (one tar.gz, skip when unchanged), is_due, save_issue_plan
│   ├── config.rs     # `[backup]`: enabled (project file), path (global file)
│   ├── import/       # `bork import`: mod.rs extract, checks, clone, restore board/files/plans, register; report.rs the summary
│   ├── schedule.rs   # TUI worker decisions: BackupIndicator, failure de-dup, messages (pure)
│   ├── manifest.rs   # Public manifest.json types (archive contract for `bork import`) + repo facts
│   ├── archive.rs    # Staging copy + system `tar -czf`, atomic rename, content fingerprint
│   ├── select.rs     # Container file selection, worktree detection, ephemeral excludes, issue plans
│   └── status.rs     # Last result per backup file (~/.config/bork/backup.json)
├── input/
│   ├── mod.rs
│   ├── action.rs     # Action enum (~70 variants)
│   └── keybindings.rs # KeyEvent → Action mapping (vim-style, per input mode)
├── external/
│   ├── mod.rs
│   ├── tmux.rs       # Tmux session management
│   ├── agent/        # Agent session launcher + provider registry
│   │   ├── mod.rs    # Generic launcher, AgentProvider trait, AgentKind→provider dispatch, shared helpers
│   │   ├── opencode.rs # OpenCode provider (mode flags, launch/resume, session detection, hooks)
│   │   ├── claude.rs   # Claude provider
│   │   ├── codex.rs    # Codex provider
│   │   ├── pi.rs       # Pi provider
│   │   ├── cursor.rs   # Cursor provider (`cursor-agent`; chat id via `create-chat`; no status hooks)
│   │   └── handoff.rs  # Files passed between `launch_session` and the `bork issue launch` command it types into the pane
│   ├── git.rs        # Git worktree status polling
│   ├── github.rs     # GitHub PR polling via gh api graphql (per-repo identity cache)
│   ├── linear.rs     # Linear CLI integration (assigned issues and projects via graphql)
│   └── hooks.rs      # Shared hook install/uninstall helpers; install()/uninstall() iterate the registry
└── ui/
    ├── mod.rs         # Root render, layout composition, swimlane splitting
    ├── board.rs       # 4-column kanban board with adaptive card sizes
    ├── card.rs        # Issue card widget (Full/Medium/Compact sizes)
    ├── sidebar.rs     # Project sidebar with needs-you markers
    ├── dialog.rs      # New/edit issue dialog overlay
    ├── help.rs        # Help overlay (keybinding reference popup)
    ├── linear_picker.rs # Import picker for Linear issues, Linear projects, and GitHub PRs
    ├── status_bar.rs  # Header + footer (swimlane indicator)
    └── styles.rs      # Colors, styles, shared UI utilities (ANSI 16 only)
```

## Data Model

```
App
├── projects: Vec<Project>          # All registered projects
├── focused_project: usize          # Primary project (has main workers)
├── focused_swimlane: usize         # Which swimlane receives keyboard input
├── sidebar: Option<SidebarState>   # None if single project
│   └── swimlane_indices: Vec<usize>  # Source of truth for visible swimlanes
└── (global UI state: input_mode, dialog, picker, message, etc.)

Project
├── issues: Vec<Issue>              # Persistent (saved to state.json)
├── config: AppConfig               # From .bork/config.toml
├── label: Option<String>           # Sidebar rename from projects.json; display_name() falls back to config.project_name
├── available_agents: Vec<AgentKind> # Resolved at startup from layered config + PATH
├── selected_column/row             # Board cursor (per-project)
├── live: LiveState                 # Ephemeral worker data (sessions, git, PRs, etc.)
└── state_dirty: bool               # Triggers flush to disk
```

Key accessors:
- `app.project()` → primary focused project (has workers)
- `app.active_project()` → project in the focused swimlane (receives user actions)

## Project Layout

Bork uses a container directory pattern. The project root is NOT a git repo. It holds:

```
bork/                           # container (the agent's cwd)
├── .bork/                      # bork state (config.toml, state.json)
├── AGENTS.md                   # agent instructions
├── opencode.jsonc              # opencode config
├── main/                       # main branch worktree (this repo, owns .git/)
└── {issue-id}/                 # issue worktrees (created by agent)
```

State lives in `.bork/` at the container root. Config is detected by walking up from cwd looking for a `.bork/` directory. Failures that would otherwise only flash in the status bar (currently worktree prune) are appended to `.bork/errors.log`.

## Global State

- `~/.config/bork/config.toml` — global config layer (agents allowlist, default_agent, default_mode, default_prompt, review_prompt, stack_review_prompt, orchestrator_prompt, setup_script, teardown_script, auto_import_reviews, auto_import_authored_prs, etc.). Same flat schema as `<project>/.bork/config.toml`; project values override global. `default_mode` (alias `agent_mode`) sets the default agent mode (plan/build/yolo) for new issues created via the TUI dialog or `bork issue create`/`bork issue start` when no `--mode` is given. Scalar keys can be read/written with `bork config get|set|list`.
- `~/.config/bork/projects.json` — registry of all bork projects (auto-registered, auto-pruned). Its order is the sidebar order (Shift+J/K in the sidebar). Each entry's `name` follows the config `project_name`; an optional `label` holds a rename from the sidebar (Shift+R) and is shown in the sidebar, header, lane titles and terminal title. Issue IDs and tmux session names always use `project_name`. `--project` matches `name` or `label`. Removing from the sidebar (Shift+D, with a y/n prompt) only unregisters; files stay on disk.
- `~/.config/bork/bork.pid` — flock-based single instance lock

Legacy: `~/.config/bork/agents.toml` is no longer read (bork-119). A one-line stderr warning is printed if it still exists.

## Backups

- One file per Mac: `bork backup` writes `<path>/bork-backup-<host>.tar.gz` with every registered, opted-in project plus `_global/` (`projects.json`, `config.toml`); `--project` writes `bork-backup-<host>--<name>.tar.gz` with just that project. `<host>` is `backup::host_name()` (macOS LocalHostName via `scutil`, else gethostname's first label; lowercased, `[a-z0-9-]`, never `--`, cached per process) and is recorded in the manifest. Never spell the file name: use `backup::backup_file_name()` / `backup::full_backup_file()` (worker, import). Built with the system `tar` from a private staging dir, then renamed into place.
- Config: `[backup] enabled = true` is the per-project opt-in (project file only). `path` (absolute or `~/`) is global-only. No schedule setting.
- Schedule: once a day automatically plus explicit `bork backup`. `backup::is_due(now)` is true when `~/.config/bork/backup.json` has no successful full backup (`backup::full_backup_file()`) or the last success is 24h+ old (`BACKUP_MAX_AGE_SECS`); the TUI Backup Worker checks it.
- Layout and manifest: see the module docs in `backup/manifest.rs`. Those serde types are the contract for `bork import`; add fields with `#[serde(default)]` and only bump `MANIFEST_VERSION` for breaking changes.
- Per project: `.bork/state.json` + `.bork/config.toml` (symlinks followed), every container entry except `main/`, worktrees (union of `git worktree list`, `issue.worktree` and dirs with a `.git` entry), nested checkouts at any depth (dirs with a `.git` entry) and ephemeral files, plus each live worktree's `.claude/planning.md` as `_issues/<issue-id>/planning.md`. Skipped checkouts (nested at any depth, or standalone top-level clones with a `.git` dir) that have no remote are reported as `local_only_repos` in the plan, outcome and status record, and printed as a warning by the CLI.
- Unchanged content (fingerprint over manifest without `created_at` + every file) skips the write. Read errors other than NotFound fail the run and keep the previous file. The path may not be inside a registered project.
- `backup::save_issue_plan(root, issue_id, worktree_dir)` copies a worktree's plan into the container's `plans/<issue-id>/planning.md` (no-op when backups are off).
- Removal paths save plans through `issue_plan::save_before_removal`, called inside both removal primitives: `worktree::remove_worktree_in` (archive, takes the issue id) and `prune::execute_removals` (`PruneTarget` carries the issue id; orphans have none and save to `plans/_orphans/<worktree>/` via `backup::save_orphan_plan`). Don't add a removal path that bypasses them. The plan is saved before the teardown, and a failed save stops before the teardown runs, keeping the worktree (`ForceRequired::PlanNotSaved`, `RemoveOutcome::PlanNotSaved`) until forced. The TUI's force retry runs the still-due teardown for those (`ConfirmAction::ArchiveIssue.teardown_due`); `Removal::ForceSkipTeardown` skips both the save and the teardown, so a TUI retry never overwrites the saved plan with what the teardown left (a CLI `--force` run is a fresh `Removal::Force`, which saves the current plan again). Forcing happens via the TUI y/n prompt or `--force` on `bork issue archive` / `bork prune`, and a forced loss is logged to `.bork/errors.log`. Deleting an issue removes `plans/<issue-id>/` (`issue_plan::remove_saved_plans`, backups-on projects only), which for an orchestrator is its live plan. Plan paths come from `backup::issue_plan_dir` / `PLANS_DIR`.
- Runs are serialized with a required flock on `~/.config/bork/backup.lock`.
- `bork import [file]` (`backup/import/`, see the module docs): any backup file path; without one, `backup::full_backup_file()`.
  - Before writing: `manifest.version` first; every target and name checked (duplicates, names registered elsewhere, an unreadable `projects.json`); per target, refuse a symlink, a repo, an existing dir inside a repo (real path), and a (dev, inode) clash with an earlier target.
  - Never touched: `main/`, worktrees and any checkout at the target (`select::worktree_dirs` plus any ancestor with a `.git`), plans that existed before the import (`plans/` matched case-insensitively), symlinks at the target (a symlinked `.bork/state.json` skips the board). Nothing is read or written through a symlinked directory.
  - Board: through `try_load_state`/`save_state` with `rewrite_state` (missing worktrees cleared like a prune; a session kept only when the replaced board has the same id, since OpenCode's `has_session` is always true). A replaced board, project `.bork/config.toml` or global config is kept as `<file>.pre-import-<time>` unless nothing changed (`save_pre_import` for the configs, also when the archive holds a symlink).
  - Clone via `init::clone_repo` (shared with `bork init`), default branch and recorded remote names; git args from the manifest are guarded against option injection. Only the global agent hooks are installed (`hooks::install_agent_hooks`).
  - Failures (a project, a clone or wrong branch, the global config) are reported and the rest continues; the CLI exits non-zero after the summary.
- TUI: the shared Backup Worker (`spawn_backup_worker` in main.rs, decisions in `backup/schedule.rs`, report handling in `App::apply_backup_report`).
  - Schedule: checks `backup::is_due` 15s after startup and then hourly, and runs `backup::run(&Selection::All)` when due or when the last run failed (`schedule::retry_or_due`, so a failed CLI run is retried hourly), after checking a path is set and some project is opted in (`try_list_projects`, so a corrupt registry or config is a failure, not "off"; turning backups off clears an old failure). The worker also retries after its own failed attempt, since some failures (an unusable `backup.lock`) aren't recorded in `backup.json`. `Shift+B` forces a run.
  - Header: renders only warnings, red when the last run failed, yellow when the last success is over 48h old (from the clock, not a worker report: the 2s state poll redraws when the warning text changes, so it shows even if the worker is stuck on the backup lock). Status is read with `backup::full_backup_record()` (keyed by the resolved path `run` records).
  - Failure reporting: a new failure (or changed error) from a real check flashes once and is appended to `.bork/errors.log` of each loaded project that isn't opted out (an unreadable config may be the failure itself); a failure already in `backup.json` at startup only sets the baseline (`startup_report`). `backup::run` never prints: a `backup.json` it couldn't write is `Outcome::status_warning` (the CLI prints it, the TUI shows it as a warning and logs it) or part of the error. If the worker thread dies, `backup_worker_stopped` releases a pending `Shift+B` and shows an error.
  - CLI pickup: the 2s state poll re-reads `backup.json` when its mtime changes, so a CLI `bork backup` updates the header without waiting for the hourly check. Skipped git repos with no remote (`local_only_repos` in the status record) get a status bar warning once per change and in every `Shift+B` result.
  - Quit: `finish_backup_before_quit` refuses new runs (`backup::stop_new_runs`) and waits up to `schedule::QUIT_WAIT` (10s) with "Finishing backup…" for a run in flight (`backup::in_flight`: from taking the lock until the result is recorded), then `backup::abort_in_flight` kills the `tar` group and removes its temp file and staging dir. A run that started before quit still writes; only an aborted write isn't recorded as a failure (`archive::aborted`), so a real failure during the wait still is.
- Stale temp files: each `backup::run` first removes, under the lock, this host's `.bork-backup-<host>….tmp.<pid>` files in the backup folder (never another Mac's: the folder may be synced) and `bork-backup-<pid>-<n>` staging dirs in `$TMPDIR` owned by this user whose pid is gone (`archive::remove_stale`). Symlinks, other names and live pids are never touched.

## Build & Run

```bash
cd main && cargo build --release
```

The binary is symlinked to `/opt/homebrew/bin/bork`.

### Testing a change with a debug build

Build in your own worktree with `cargo build` and run the result as `./target/debug/bork`. Don't build `--release` in `main/`, because that replaces the installed binary.

- bork finds its state by walking up from the cwd to the nearest `.bork/`. From inside an issue worktree that's the real project board, so `./target/debug/bork issue list`/`show` see live data and any write changes it.
- The TUI holds a global single-instance lock in `~/.config/bork/`. A debug TUI won't start while the installed one is running, so quit that one first to try your build against the real board.
- To test without touching real state, point the global config at a temp dir and run from a scratch project:

```bash
mkdir -p /tmp/bork-sandbox/proj/.bork
cd /tmp/bork-sandbox/proj
XDG_CONFIG_HOME=/tmp/bork-sandbox/xdg <worktree>/target/debug/bork issue create "test"
```

## Conventions

- Vim-style navigation: h/j/k/l for column jumping and vertical movement
- State: {project_root}/.bork/state.json (atomic writes via .tmp.{pid} + rename)
- Config: {project_root}/.bork/config.toml
- Issue IDs: {project_name}-{number} (e.g. bork-1, bork-2)
- Tmux agent sessions named: {project_name}-{issue-id}
- Wrapper tmux session: always named "bork" (single global session)
- Opencode launched at project root with --prompt for issue context

## Issue Kinds

- `Agentic` (default) — launches a coding agent in a tmux session, usually in its own worktree
- `NonAgentic` ("Todo") — plain checklist card, never launches an agent; Enter opens the edit dialog
- `Orchestrator` — launches a coordinating agent at the project root with no worktree and no GitHub PR field. Its prompt comes from `orchestrator_prompt` (project overrides global, falls back to `DEFAULT_ORCHESTRATOR_PROMPT` in config.rs) instead of `default_prompt`, and bork appends the planning file path `plans/{issue-id}/planning.md`. The agent breaks the goal into issues, spawns them via `bork issue start`, and monitors/nudges their agents via `bork issue list --json` and tmux. Cards render with a magenta border and an `◆ orch` badge. `IssueKind::is_agentic()` is true for both `Agentic` and `Orchestrator`. Kind changes go through `Issue::set_kind()`, which clears all agent sessions when crossing the orchestrator boundary and additionally drops the worktree and PR links when becoming an orchestrator (`bork integration attach-pr` also rejects orchestrators). Only orchestrators carry Linear projects (`Issue.linear_projects`): leaving the kind drops them, `bork integration attach-linear-project` rejects other kinds, the dialog's Projects field only appears for orchestrators, and the picker's Projects tab is offered when attaching only if the dialog is an orchestrator (importing a project from it always creates an orchestrator, so the tab shows in import mode for any card). Linked projects are appended to the orchestrator prompt as context (read the project; link tickets via `--linear` when a spawned issue covers one), not as a backlog to work through.

## Agent Sessions

Each `Issue` stores agent session IDs in `sessions: BTreeMap<AgentKind, String>`, keyed by the agent that created them; launch/resume only reads the current agent's entry, so switching agents starts fresh while other agents' sessions stay resumable on switch-back. Agent changes go through `Issue::set_agent_kind()`, which (like `set_kind()`) tells the caller to kill the live tmux session but never clears the map. `launch_session()` types only `'<current_exe>' issue launch <id>` into the pane; that command (`agent::exec_launch()`) reads the issue staged by the caller in `.bork/agent-status/launch-<id>/` (falling back to state.json when run by hand), runs setup, checks `AgentProvider::has_session()` before resuming and launches fresh when the stored session is gone, builds the agent command and execs it. It reports the session id, setup result and `replaced_lost_session` back through the same dir; the caller still snapshots and detects OpenCode/Codex/Pi ids, so sequential bulk launches can't swap ids. A lost session with no new id is dropped (`forget_lost_session` in the TUI, `ops::record_launch()` when run by hand) so it isn't retried. The worktree setup script runs once per worktree: `Issue::setup_ran` is set only after the launch command reports a successful setup exit (independent of session-id capture, which can miss), cleared by `attach_worktree()` so a fresh checkout re-runs setup, and seeded from any recorded session id when migrating legacy states. Failed or interrupted setup returns a launch error and remains eligible for retry. The edit dialog marks resumable agents with `↺`, derived live from the issue's sessions map, minus sessions `AgentProvider::has_session()` reports gone and ones the user dropped with Backspace (applied on save via `Issue::forget_session()`; `Issue::forgetting_kills_session()` says when the live session must die first, and `bork issue reset-session` uses the same pair). Claude looks for its transcript under `<claude config dir>/projects/` (`hooks::claude_config_dir()`, which honours `CLAUDE_CONFIG_DIR` and is also where Claude hooks are installed); Codex and Pi search their session files by filename (`session_in`); Cursor looks for `~/.cursor/chats/*/<chat id>/`. OpenCode keeps the default (`true`): its sessions live in SQLite and are never cleaned up. Unreadable storage, including any unreadable dir or entry along the search, counts as present unless the session was found. The dialog records lost sessions by id, so a new session stored while it's open gets its `↺` back. Session teardown (kill + status-file removal) goes through `agent::terminate_session()`, which reports whether a live session actually died.

## Integration Data Model

Each `Issue` can link to multiple Linear issues and GitHub PRs via Vec fields:

- `linear_links: Vec<LinkedLinear>` — each has `id`, `identifier`, `url`, `imported`
- `linear_projects: Vec<LinkedLinearProject>` — orchestrators only; each has `id`, `slug` (Linear `slugId`), `name`, `url`, `imported`
- `github_pr_links: Vec<LinkedGithubPr>` — each has `number`, `imported`, `import_source`

Legacy singular fields (`linear_id`, `pr_number`, etc.) are kept for deserialization backward compat but marked `#[serde(skip_serializing)]`. Migration happens automatically in `load_state()`.

The dialog picker uses multi-select in Attach mode (Enter toggles, Backspace removes last). Import mode stays single-select (creates a new bork issue per selection).

## Issue Links

Issues can be tied to other issues in the same project via a symmetric `linked_issues: Vec<String>` field (each side stores the other's id, case-insensitive). Links power a board filter that narrows the view to one connected component (BFS over `linked_issues`, see `Project::linked_component` and `ops::linked_component`).

- **Auto-link on spawn**: agent sessions export `BORK_ISSUE_ID`; `bork issue start` links the new issue back to the spawning issue (or to `--link <id>`) when the target resolves in the same project. This ties an orchestrator to the sub-issues it spawns.
- **Manual links**: `bork integration link <a> <b>` / `unlink <a> <b>`, or the TUI link picker (`c`, model after `linear_picker`).
- **Filter**: `f` toggles the board to the selected issue's connected component; `Esc`/`f` clears. `bork issue list --linked <id>` does the same from the CLI.
- Cards show a cyan `∞N` badge (link count). Deleting an issue strips its id from every other issue's `linked_issues` (`ops::remove_link_references`).
- Any new `Issue` field (including `linked_issues`) must be added to `merge_issue_fields` in `app.rs` or concurrent TUI edits drop CLI-written values.

## Loading indicators

- Use the existing animated spinner for routine loading; do not add textual loaders such as `Loading...`, `Fetching...`, or `Refreshing...` to cards, rows, or modal summaries.
- Put modal spinners at the bottom-right inside the modal and the global spinner at the bottom-right of the app. Reserve their space so shortcuts, counts, and update notices cannot overlap them.
- Keep cached content visible during refresh. Spinners must track actual requests and clear on success or failure; missing tools and unsupported features must not leave a spinner running.
- Keep errors and required user actions readable as text. A spinner does not replace an error message.

## GitHub request scope

- Keep review discovery automatic in the background. It must not depend on opening the picker or wait behind the Done backlog.
- Fetch the repository-wide recent-PR list and stack discovery only when the GitHub picker is open. Board status polling covers linked PRs, attached stack members, and issue worktree branches.
- Prioritize Code Review, In Progress, To Do, then Done. Reuse `.bork/github-cache.json` across restarts, batch and deduplicate requests, and preserve retry cooldowns. Do not reintroduce a sweep of every repository stack's PR statuses.

## Merge queue status

- `PrStatus.merge_queue` comes from GitHub's `mergeQueueEntry`/`isInMergeQueue` first, then Trunk's `Trunk Merge Queue (<branch>)` check run on the head commit. `PR_FIELDS_WITH_QUEUE` asks only for the check suite of Trunk's GitHub App (id 120106), so the query stays small in repos with many checks. Review searches use plain `PR_FIELDS`, since queue state only matters on your own PRs; anything that can feed `pr_for` for your own PR must use the queue version.
- Trunk's check is part of `statusCheckRollup`, so `pr_attention` checks the queue state before CI and review. `Attention::Queue(state)` carries the state; queued/testing stay out of the needs-you count, failed/dequeued count.

## Automatic stack review imports

- Review discovery uses `user-review-requested` (direct personal requests), not `review-requested` (which includes team queues) or `involves`. Return PRs and their stack numbers atomically, after all pages succeed.
- Resolve distinct stacks before importing cards. A known stack member must never fall back to an individual automatic card while metadata is pending. Preserve explicit PR-only attachments and edited cards; when they block a stack card, import the remaining requests individually so every direct request reaches the board. Done cards neither block nor get absorbed.
- Track automatic stack review lifecycle separately from manual stack attachments. Complete only from a successful discovery with no outstanding requests; reopen on a new member request.
- Submitting a review clears your GitHub review request, so discovery also searches open PRs you `reviewed-by`. A PR where your latest opinionated review is `CHANGES_REQUESTED` counts as still pending, for both single-PR and stack cards, until you approve or it closes.
- `stack_review_prompt` has the same global/project precedence as `review_prompt`. Keep them independent and preserve per-card prompt edits.
