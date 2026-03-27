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

## Quickstart

You need [tmux](https://github.com/tmux/tmux), [git](https://git-scm.com/), a [Rust toolchain](https://rustup.rs/), and at least one AI coding agent ([OpenCode](https://opencode.ai) or [Claude Code](https://docs.anthropic.com/en/docs/claude-code)).

**1. Install bork**

```bash
git clone https://github.com/villads-valur/bork.git
cd bork
cargo build --release
# Add to PATH (pick one)
ln -sf "$(pwd)/target/release/bork" /opt/homebrew/bin/bork   # macOS
sudo ln -sf "$(pwd)/target/release/bork" /usr/local/bin/bork # Linux
```

**2. Set up a project**

```bash
bork init owner/repo          # GitHub shorthand
bork init git@github.com:owner/repo.git   # or SSH/HTTPS URL
```

This clones the repo, scaffolds the `.bork/` directory, and installs agent status hooks automatically.

**3. Launch**

```bash
cd repo
bork
```

Press `n` to create an issue, `Enter` to launch an agent session. You're up and running.

## Features

- **4-column kanban board** &mdash; To Do, In Progress, Code Review, Done
- **AI agent sessions** &mdash; Launch OpenCode or Claude Code per issue in tmux popups
- **Session resumption** &mdash; Closing a tmux popup and reopening it continues the same conversation, not a fresh one
- **Real-time status monitoring** &mdash; See agent state on each card (Idle, Busy, Waiting, Error)
- **GitHub PR status** &mdash; Background polling shows checks, review status, and diff stats on cards
- **Git worktree tracking** &mdash; Live staged/unstaged change counts and branch names
- **Tmux integration** &mdash; Auto-wraps in tmux, sessions open as 90% screen popups
- **Plan, Build, and Yolo modes** &mdash; Toggle between modes per issue; Claude also supports Yolo (skips all permission prompts)
- **Vim-style navigation** &mdash; h/j/k/l, g/G, and familiar modal keybindings
- **ANSI 16 colors** &mdash; Adapts to any terminal theme, no hardcoded RGB
- **Zero-dependency state** &mdash; JSON file persistence with atomic writes, no database

## Requirements

| Dependency | Purpose |
|------------|---------|
| [tmux](https://github.com/tmux/tmux) | Session management and popup overlays |
| [git](https://git-scm.com/) | Worktree status and branch detection |
| [gh](https://cli.github.com/) | GitHub PR status polling (optional) |
| [OpenCode](https://opencode.ai) or [Claude Code](https://docs.anthropic.com/en/docs/claude-code) | AI coding agent (at least one) |
| [gh](https://cli.github.com/) | GitHub PR status (optional) |
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

Verify it works:

```bash
bork --help
```

## Usage

| Command | Description |
|---------|-------------|
| `bork` | Launch the TUI kanban board |
| `bork init <repo>` | Set up a new bork project from a git repo |
| `bork install` | Install agent status hooks |
| `bork uninstall` | Remove agent status hooks |

### `bork init`

Sets up a new bork project by cloning a repo and scaffolding the container directory structure.

```bash
bork init owner/repo                      # GitHub shorthand (clones via HTTPS)
bork init git@github.com:owner/repo.git   # SSH URL
bork init https://github.com/owner/repo   # HTTPS URL
bork init owner/repo myproject            # Custom directory name
bork init owner/repo --agent claude       # Use Claude Code instead of OpenCode
```

This creates:

```
repo/                        # Container directory
├── .bork/                   # Config, state, agent status
│   ├── config.toml
│   ├── state.json
│   └── agent-status/
├── main/                    # Main branch worktree (the cloned repo)
├── opencode.jsonc           # OpenCode config
└── .claude/skills/worktree/ # Worktree skill for Claude Code
```

Agent status hooks are installed automatically. The directory name defaults to the repo name, or you can pass a second argument to override it.

### `bork install` / `bork uninstall`

Bork ships with hooks that report agent status (Idle, Busy, Waiting, Error) back to the board in real time.

- **OpenCode**: Installs as a plugin
- **Claude Code**: Adds hooks to `settings.json`

These are installed automatically by `bork init`. Use `bork install` / `bork uninstall` to manage them manually.

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
| `Enter` | Open session (resume or launch if none, attach if running) |
| `P` | Force-sync PR statuses from GitHub |
| `o` | Open PR in browser (if issue has a matching PR) |
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
| `Space` / `h` / `l` | Cycle mode: Plan → Build (→ Yolo for Claude) |

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

## GitHub PR Integration

Bork polls GitHub for open PRs every 60 seconds using a single GraphQL query via the `gh` CLI. PRs are matched to issues by comparing the PR's head branch name against each issue's worktree branch.

Each card shows PR status when a matching PR is found:

| Element | Meaning |
|---------|---------|
| `#42` | PR number |
| `✓` (green) | CI checks passing |
| `✗` (red) | CI checks failing |
| `◌` (yellow) | CI checks pending |
| `●` (green) | Review approved |
| `●` (red) | Changes requested |
| `○` (yellow) | Review required |
| `+12/-3` | Lines added/removed |

The `gh` CLI must be installed and authenticated. If `gh` is not available or the repo is not on GitHub, PR polling is silently skipped.

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
