# Bork

Terminal kanban board for orchestrating OpenCode/Claude coding sessions across git worktrees and tmux.

## Architecture

- **Language**: Rust (no async runtime, pure `std::thread` + `mpsc`)
- **TUI**: ratatui + crossterm
- **External tools**: tmux, gh, linear, git, opencode/claude (all via `std::process::Command`)

### Threading Model

```
Main Thread (50ms tick event loop)
├── Tmux Status Worker (persistent, polls every 2s)
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
├── main.rs           # Event loop, threading, terminal setup
├── app.rs            # App state struct
├── handler.rs        # Action dispatch, state mutations
├── config.rs         # Config/state persistence (atomic writes)
├── types.rs          # Domain types (Issue, Column, AgentKind, etc.)
├── error.rs          # Error types
├── input/
│   ├── mod.rs
│   ├── action.rs     # Action enum
│   └── keybindings.rs # KeyEvent → Action mapping
├── external/
│   ├── mod.rs
│   ├── tmux.rs       # Tmux session management
│   └── opencode.rs   # Agent session launcher
└── ui/
    ├── mod.rs         # Root render, layout composition
    ├── board.rs       # 4-column kanban board
    ├── card.rs        # Issue card widget
    ├── status_bar.rs  # Header + footer
    └── styles.rs      # Colors, styles
```

## Project Layout

Bork uses a container directory pattern. The project root is NOT a git repo. It holds:

```
bork/                           # container (opencode's cwd)
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
- Tmux sessions named: bork-{issue-id}
- Opencode launched at project root with --prompt for issue context
