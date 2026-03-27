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

## Build & Run

```bash
cargo build
cargo run
```

## Conventions

- Vim-style navigation: h/j/k/l
- Config: ~/.config/bork/config.toml
- State: ~/.config/bork/state.json (atomic writes)
- Tmux sessions named: bork-{issue-id}
