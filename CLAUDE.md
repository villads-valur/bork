# Bork

Terminal kanban board for orchestrating OpenCode/Claude coding sessions across git worktrees and tmux.

## Architecture

- **Language**: Rust (no async runtime, pure `std::thread` + `mpsc`)
- **TUI**: ratatui + crossterm
- **External tools**: tmux, git, gh, linear (optional), and the coding agents (all via `std::process::Command`)

### Threading Model

```
Main Thread (50ms tick event loop)
├── Shared workers (one each, app-wide)
│   ├── Tmux Session Worker (polls every 2s - one global `tmux list-sessions`)
│   ├── Port Poll Worker (polls every 10s - listening TCP ports via lsof/ps)
│   ├── Linear Worker (polls every 45s - assigned Linear issues, conditional on `linear` CLI)
│   ├── Activity Poller (polls every 5s - agent status dirs for all registered projects)
│   └── Update Check Worker (every 6h - new-version banner, plus cache mtime poll every 2s)
├── Primary ProjectWorkers (for focused project)
│   ├── Agent Status Worker (polls every 2s - agent status files)
│   ├── Git Status Worker (adaptive: in-progress worktrees ~5s, others 15-30s, done
│   │                       skipped; per-worktree `git status` under a global
│   │                       concurrency bound + one batched `git worktree list
│   │                       --porcelain` for branches)
│   └── PR Status Worker (polls every 60s - GitHub PRs via gh api graphql)
├── Swimlane Workers (one ProjectWorkers set per visible swimlane, excluding focused)
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
├── config.rs         # Config/state persistence (atomic writes)
├── global_config.rs  # Global project registry (~/.config/bork/projects.json)
├── types.rs          # Domain types (Issue, Column, AgentKind, IssueKind, PrStatus, etc.)
├── error.rs          # Error types
├── init.rs           # `bork init` subcommand (clone repo, scaffold .bork/ directory)
├── lock.rs           # Single-instance PID file lock + signal handlers (SIGTERM, SIGHUP)
├── worktree.rs       # `bork worktree` subcommand (create git worktree, register with state)
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
│   │   └── pi.rs       # Pi provider
│   ├── git.rs        # Git worktree status polling
│   ├── github.rs     # GitHub PR polling via gh api graphql (per-repo identity cache)
│   ├── linear.rs     # Linear CLI integration (assigned issues via graphql)
│   └── hooks.rs      # Shared hook install/uninstall helpers; install()/uninstall() iterate the registry
└── ui/
    ├── mod.rs         # Root render, layout composition, swimlane splitting
    ├── board.rs       # 4-column kanban board with adaptive card sizes
    ├── card.rs        # Issue card widget (Full/Medium/Compact sizes)
    ├── sidebar.rs     # Project sidebar with activity markers
    ├── dialog.rs      # New/edit issue dialog overlay
    ├── help.rs        # Help overlay (keybinding reference popup)
    ├── linear_picker.rs # Import picker for Linear issues and GitHub PRs
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

State lives in `.bork/` at the container root. Config is detected by walking up from cwd looking for a `.bork/` directory.

## Global State

- `~/.config/bork/config.toml` — global config layer (agents allowlist, default_agent, default_mode, default_prompt, review_prompt, stack_review_prompt, orchestrator_prompt, setup_script, teardown_script, auto_import_reviews, auto_import_authored_prs, etc.). Same flat schema as `<project>/.bork/config.toml`; project values override global. `default_mode` (alias `agent_mode`) sets the default agent mode (plan/build/yolo) for new issues created via the TUI dialog or `bork issue create`/`bork issue start` when no `--mode` is given. Scalar keys can be read/written with `bork config get|set|list`.
- `~/.config/bork/projects.json` — registry of all bork projects (auto-registered, auto-pruned, managed artifact)
- `~/.config/bork/bork.pid` — flock-based single instance lock

Legacy: `~/.config/bork/agents.toml` is no longer read (bork-119). A one-line stderr warning is printed if it still exists.

## Build & Run

```bash
cd main && cargo build --release
```

The binary is symlinked to `/opt/homebrew/bin/bork`.

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
- `Orchestrator` — launches a coordinating agent at the project root with no worktree and no GitHub PR field. Its prompt comes from `orchestrator_prompt` (project overrides global, falls back to `DEFAULT_ORCHESTRATOR_PROMPT` in config.rs) instead of `default_prompt`, and bork appends the planning file path `plans/{issue-id}/planning.md`. The agent breaks the goal into issues, spawns them via `bork issue start`, and monitors/nudges their agents via `bork issue list --json` and tmux. Cards render with a magenta border and an `◆ orch` badge. `IssueKind::is_agentic()` is true for both `Agentic` and `Orchestrator`. Kind changes go through `Issue::set_kind()`, which clears all agent sessions when crossing the orchestrator boundary and additionally drops the worktree and PR links when becoming an orchestrator (`bork integration attach-pr` also rejects orchestrators).

## Agent Sessions

Each `Issue` stores agent session IDs in `sessions: BTreeMap<AgentKind, String>`, keyed by the agent that created them; launch/resume only reads the current agent's entry, so switching agents starts fresh while other agents' sessions stay resumable on switch-back. Agent changes go through `Issue::set_agent_kind()`, which (like `set_kind()`) tells the caller to kill the live tmux session but never clears the map. The worktree setup script runs once per worktree: `Issue::setup_ran` is set only after the tmux shell acknowledges a successful setup exit (independent of session-id capture, which can miss), cleared by `attach_worktree()` so a fresh checkout re-runs setup, and seeded from any recorded session id when migrating legacy states. Failed or interrupted setup returns a launch error and remains eligible for retry. The edit dialog marks resumable agents with `↺`, derived live from the issue's sessions map. Session teardown (kill + status-file removal) goes through `agent::terminate_session()`, which reports whether a live session actually died.

## Integration Data Model

Each `Issue` can link to multiple Linear issues and GitHub PRs via Vec fields:

- `linear_links: Vec<LinkedLinear>` — each has `id`, `identifier`, `url`, `imported`
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

## Automatic stack review imports

- Review discovery uses `review-requested` (direct and team requests), not `involves`. Return PRs and their stack numbers atomically, after all pages succeed.
- Resolve distinct stacks before importing cards. A known stack member must never fall back to an individual automatic card while metadata is pending. Preserve explicit PR-only attachments and edited cards.
- Track automatic stack review lifecycle separately from manual stack attachments. Complete only from a successful discovery with no outstanding requests; reopen on a new member request.
- `stack_review_prompt` has the same global/project precedence as `review_prompt`. Keep them independent and preserve per-card prompt edits.
