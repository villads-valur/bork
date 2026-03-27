<div align="center">

# bork

**Terminal kanban board for orchestrating AI coding sessions across git worktrees and tmux.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2021-orange.svg)](https://www.rust-lang.org/)
[![Built With Ratatui](https://img.shields.io/badge/Built_With-Ratatui-C43AC3.svg)](https://ratatui.rs/)

</div>

---

<!-- TODO: Add demo GIF here -->
<!-- Record with: vhs, asciinema, or ttygif -->
<!-- Recommended size: 80x24 terminal, 15-30s loop showing: -->
<!--   1. Board overview with issues in different columns -->
<!--   2. Creating a new issue -->
<!--   3. Launching an agent session (tmux popup) -->
<!--   4. Returning to the board with live status updates -->

## Overview

Bork is a terminal UI for managing multiple AI coding sessions. It gives you a 4-column kanban board where each issue maps to a git worktree and a tmux session running [OpenCode](https://opencode.ai) or [Claude Code](https://docs.anthropic.com/en/docs/claude-code). Switch between sessions with a keypress, see agent status at a glance, and keep your work organized.

## Features

- **4-column kanban board** &mdash; To Do, In Progress, Code Review, Done
- **AI agent sessions** &mdash; Launch OpenCode or Claude Code per issue in tmux popups
- **Real-time status monitoring** &mdash; See agent state on each card (Idle, Busy, Waiting, Error)
- **Git worktree tracking** &mdash; Live staged/unstaged change counts and branch names
- **Tmux integration** &mdash; Auto-wraps in tmux, sessions open as 90% screen popups
- **Plan and Build modes** &mdash; Toggle between planning and building per issue
- **Vim-style navigation** &mdash; h/j/k/l, g/G, and familiar modal keybindings
- **ANSI 16 colors** &mdash; Adapts to any terminal theme, no hardcoded RGB
- **Zero-dependency state** &mdash; JSON file persistence with atomic writes, no database

## Requirements

| Dependency | Purpose |
|------------|---------|
| [tmux](https://github.com/tmux/tmux) | Session management and popup overlays |
| [git](https://git-scm.com/) | Worktree status and branch detection |
| [OpenCode](https://opencode.ai) or [Claude Code](https://docs.anthropic.com/en/docs/claude-code) | AI coding agent (at least one) |
| [Rust toolchain](https://rustup.rs/) | Building from source |

## Installation

### From source

```bash
git clone https://github.com/villads-valur/bork.git
cd bork
cargo build --release
```

Then symlink or copy the binary somewhere on your `$PATH`:

```bash
# macOS (Homebrew prefix)
ln -sf "$(pwd)/target/release/bork" /opt/homebrew/bin/bork

# Linux
sudo ln -sf "$(pwd)/target/release/bork" /usr/local/bin/bork
```

### Agent status hooks

Bork ships with hooks that report agent status back to the board in real time. Install them after building:

```bash
bork install      # Installs hooks for OpenCode (plugin) and Claude Code (settings.json)
bork uninstall    # Removes hooks
```

## Quick Start

1. **Initialize** &mdash; Create a `.bork/` directory in your project root (this is where bork stores config and state)

   ```bash
   mkdir .bork
   ```

2. **Launch** &mdash; Run `bork` from anywhere inside the project tree

   ```bash
   bork
   ```

   If you're not already in tmux, bork wraps itself in a tmux session automatically.

3. **Create an issue** &mdash; Press `n` to open the new issue dialog. Fill in a title, prompt, and worktree path.

4. **Start coding** &mdash; Press `Enter` on an issue to launch an AI agent session in a tmux popup. Press `Ctrl+q` to return to the board.

## Usage

| Command | Description |
|---------|-------------|
| `bork` | Launch the TUI kanban board |
| `bork install` | Install agent status hooks (OpenCode plugin + Claude Code hooks) |
| `bork uninstall` | Remove agent status hooks |

## Keybindings

### Board Navigation

| Key | Action |
|-----|--------|
| `j` / `Down` | Move selection down |
| `k` / `Up` | Move selection up |
| `h` / `Left` | Focus column left |
| `l` / `Right` | Focus column right |
| `Tab` | Jump to next column |
| `Shift+Tab` | Jump to previous column |
| `g` | Scroll to top |
| `G` | Scroll to bottom |
| `Enter` | Open session (launch if none, attach if exists) |
| `q` / `Ctrl+c` | Quit |

### Issue Management

| Key | Action |
|-----|--------|
| `n` | Create new issue |
| `e` | Edit selected issue |
| `d` | Delete issue (with confirmation) |
| `x` | Kill session (with confirmation) |
| `H` | Move issue to previous column |
| `L` | Move issue to next column |

### Dialog Mode

| Key | Action |
|-----|--------|
| `Tab` / `Enter` | Next field (auto-submits from last field) |
| `Shift+Tab` | Previous field |
| `Shift+Enter` | Submit from any field |
| `Esc` / `Ctrl+c` | Cancel |
| `Space` / `h` / `l` | Toggle Plan/Build mode (on mode field) |

### Confirm Mode

| Key | Action |
|-----|--------|
| `y` / `Enter` | Confirm |
| `n` / `Esc` | Cancel |

## Configuration

Bork looks for a `.bork/` directory by walking up from the current working directory. Configuration lives at `.bork/config.toml`:

```toml
project_name = "myproject"       # Issue ID prefix (e.g. myproject-1, myproject-2)
agent_kind = "opencode"          # Default agent: "opencode" or "claude"
default_prompt = "Check AGENTS.md for project context and start working on the issue."
```

### State

Issue data is stored in `.bork/state.json` as a flat JSON array. Writes are atomic (write to temp file, then rename) so state is never corrupted even if bork crashes.

Agent status files are written to `.bork/agent-status/` by the hooks installed with `bork install`.

## Agent Status Indicators

Each issue card shows the current agent status:

| Symbol | Status |
|--------|--------|
| `◌` | Stopped (no session) |
| `○` | Idle |
| `●` | Busy |
| `◈` | Waiting for input |
| `✗` | Error |

## Project Layout

Bork uses a container directory pattern where the project root is not itself a git repo:

```
myproject/                   # Container directory (bork's working directory)
├── .bork/                   # Config, state, agent status
│   ├── config.toml
│   ├── state.json
│   └── agent-status/
├── main/                    # Main branch worktree
│   └── src/
├── myproject-1/             # Issue worktree
│   └── src/
└── myproject-2/             # Another issue worktree
    └── src/
```

Each issue gets its own git worktree. Tmux sessions are named `bork-{issue-id}` with two windows: one for the AI agent and one bare terminal.

## Built With

- [ratatui](https://ratatui.rs/) &mdash; TUI framework
- [crossterm](https://github.com/crossterm-rs/crossterm) &mdash; Terminal backend
- [serde](https://serde.rs/) &mdash; Serialization
- [anyhow](https://github.com/dtolnay/anyhow) + [thiserror](https://github.com/dtolnay/thiserror) &mdash; Error handling

## License

This project is licensed under the [MIT License](LICENSE).
