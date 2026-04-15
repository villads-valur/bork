# Bork

Terminal kanban board for orchestrating OpenCode/Claude coding sessions across git worktrees and tmux.

## Architecture

- **Language**: Rust (no async runtime, pure `std::thread` + `mpsc`)
- **TUI**: ratatui + crossterm
- **External tools**: tmux, git, gh, linear (optional), opencode/claude/codex (all via `std::process::Command`)

### Threading Model

```
Main Thread (50ms tick event loop)
├── Primary ProjectWorkers (for focused project)
│   ├── Session Status Worker (polls every 2s - tmux sessions + agent status files)
│   ├── Port Poll Worker (polls every 5s - listening TCP ports)
│   ├── Git Status Worker (polls every 3s - worktree changes + branches)
│   ├── PR Status Worker (polls every 60s - GitHub PRs via gh api graphql)
│   └── Linear Worker (polls every 45s - assigned Linear issues, conditional on `linear` CLI)
├── Swimlane Workers (one ProjectWorkers set per visible swimlane, excluding focused)
├── Activity Poller (polls every 5s - agent status dirs for all registered projects)
└── Action Threads (fire-and-forget per user action)
```

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
├── agent_config.rs   # Agent preferences from ~/.config/bork/agents.toml + PATH detection
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
│   ├── opencode.rs   # Agent session launcher (opencode/claude/codex)
│   ├── git.rs        # Git worktree status polling
│   ├── github.rs     # GitHub PR polling via gh api graphql (per-repo identity cache)
│   ├── linear.rs     # Linear CLI integration (assigned issues via graphql)
│   └── hooks.rs      # Agent status hooks (install/uninstall for opencode/claude/codex)
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
├── available_agents: Vec<AgentKind> # Resolved at startup from ~/.config/bork/agents.toml + PATH
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

- `~/.config/bork/projects.json` — registry of all bork projects (auto-registered, auto-pruned)
- `~/.config/bork/agents.toml` — optional agent preferences (available agents, default agent)
- `~/.config/bork/bork.pid` — flock-based single instance lock

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

## Integration Data Model

Each `Issue` can link to multiple Linear issues and GitHub PRs via Vec fields:

- `linear_links: Vec<LinkedLinear>` — each has `id`, `identifier`, `url`, `imported`
- `github_pr_links: Vec<LinkedGithubPr>` — each has `number`, `imported`, `import_source`

Legacy singular fields (`linear_id`, `pr_number`, etc.) are kept for deserialization backward compat but marked `#[serde(skip_serializing)]`. Migration happens automatically in `load_state()`.

The dialog picker uses multi-select in Attach mode (Enter toggles, Backspace removes last). Import mode stays single-select (creates a new bork issue per selection).
