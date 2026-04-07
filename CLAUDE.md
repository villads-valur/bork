# Bork

Terminal kanban board for orchestrating OpenCode/Claude coding sessions across git worktrees and tmux.

## Architecture

- **Language**: Rust (no async runtime, pure `std::thread` + `mpsc`)
- **TUI**: ratatui + crossterm
- **External tools**: tmux, git, gh, linear (optional), opencode/claude (all via `std::process::Command`)

### Threading Model

```
Main Thread (50ms tick event loop)
├── Session Status Worker (persistent, polls every 2s - tmux sessions + agent status files)
├── Git Status Worker (persistent, polls every 3s - worktree changes + branches)
├── PR Status Worker (persistent, polls every 60s - GitHub PRs via gh api graphql)
├── Linear Worker (persistent, polls every 45s - assigned Linear issues, conditional on `linear` CLI)
└── Action Threads (fire-and-forget per user action)
```

### Data Flow

```
KeyEvent → map_key_to_action() → Action → handle_action() → App mutation
```

All rendering is pure: UI functions take `&App` and produce widgets, never mutate state.

### File Structure

```
src/
├── main.rs           # Entry point, CLI (clap), event loop, terminal setup
├── app.rs            # App state struct, navigation logic, worktree detection
├── handler.rs        # Action dispatch, state mutations, dialog submit/confirm
├── config.rs         # Config/state persistence (atomic writes)
├── types.rs          # Domain types (Issue, Column, AgentKind, IssueKind, PrStatus, etc.)
├── error.rs          # Error types
├── init.rs           # `bork init` subcommand (clone repo, scaffold .bork/ directory)
├── lock.rs           # Single-instance PID file lock + signal handlers (SIGTERM, SIGHUP)
├── worktree.rs       # `bork worktree` subcommand (create git worktree, register with state)
├── input/
│   ├── mod.rs
│   ├── action.rs     # Action enum (~63 variants)
│   └── keybindings.rs # KeyEvent → Action mapping (vim-style, per input mode)
├── external/
│   ├── mod.rs
│   ├── tmux.rs       # Tmux session management
│   ├── opencode.rs   # Agent session launcher (opencode + claude)
│   ├── git.rs        # Git worktree status polling
│   ├── github.rs     # GitHub PR polling via gh api graphql
│   ├── linear.rs     # Linear CLI integration (assigned issues via graphql)
│   └── hooks.rs      # Agent status hooks (install/uninstall for opencode + claude)
└── ui/
    ├── mod.rs         # Root render, layout composition
    ├── board.rs       # 4-column kanban board (To Do, In Progress, Code Review, Done)
    ├── card.rs        # Issue card widget (status, branch, git changes, PR badges)
    ├── dialog.rs      # New/edit issue dialog overlay
    ├── help.rs        # Help overlay (keybinding reference popup)
    ├── linear_picker.rs # Import picker for Linear issues and GitHub PRs
    ├── status_bar.rs  # Header + footer
    └── styles.rs      # Colors, styles, shared UI utilities (ANSI 16 only)
```

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

## Build & Run

```bash
cd main && cargo build --release
```

The binary is symlinked to `/opt/homebrew/bin/bork`.

## Conventions

- Vim-style navigation: h/j/k/l
- State: {project_root}/.bork/state.json (atomic writes)
- Config: {project_root}/.bork/config.toml
- Issue IDs: {project_name}-{number} (e.g. bork-1, bork-2)
- Tmux sessions named: {project_name}-{issue-id}
- Wrapper tmux session named: {project_name}
- Opencode launched at project root with --prompt for issue context
