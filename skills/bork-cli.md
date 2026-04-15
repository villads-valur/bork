---
name: bork-cli
description: Manage bork issues and integrations from the command line
---

# Bork CLI

Use `bork issue` and `bork integration` commands to manage the kanban board programmatically. Changes are picked up by the TUI within 2 seconds.

All commands must be run from within a bork project directory (a directory containing `.bork/`), or any subdirectory of it.

## Issues

### List issues

```bash
bork issue list                          # human-readable table
bork issue list --json                   # JSON output
bork issue list --column todo            # filter by column
bork issue list --column in-progress --json
```

### Create an issue

```bash
bork issue create "Fix authentication bug"
bork issue create "Refactor database layer" --column in-progress --agent claude --mode build
bork issue create "Update README" --kind todo    # non-agentic (manual) issue
bork issue create "Add search" --prompt "Implement full-text search using the existing index"
```

Options:
- `--column`: todo (default), in-progress, code-review, done
- `--agent`: opencode (default from project config), claude, codex
- `--mode`: plan (default), build, yolo
- `--kind`: agentic (default), todo (non-agentic)
- `--prompt`: agent prompt text

### Show issue details

```bash
bork issue show bork-1
bork issue show bork-1 --json
```

### Update an issue

```bash
bork issue update bork-1 --title "New title"
bork issue update bork-1 --column code-review
bork issue update bork-1 --agent claude --mode build
bork issue update bork-1 --prompt "Updated instructions for the agent"
bork issue update bork-1 --prompt ""     # clear the prompt
```

Only provided fields are changed; others remain untouched.

### Move an issue

```bash
bork issue move bork-1 in-progress
bork issue move bork-1 done
bork issue move bork-1 todo
```

Moving to `done` sets a completion timestamp. Moving away from `done` clears it.

### Delete an issue

```bash
bork issue delete bork-1
```

## Integrations

### Link a Linear ticket

```bash
bork integration attach-linear bork-1 VIL-123
```

This sets the Linear identifier on the issue so the TUI can display the link and sync status.

### Link a GitHub PR

```bash
bork integration attach-pr bork-1 42
```

This sets the PR number on the issue so the TUI can display PR status (checks, reviews, etc.).

## Typical workflow

1. Create an issue: `bork issue create "Implement feature X" --prompt "Details..."`
2. Create a worktree: `bork worktree bork-1 implement-feature-x`
3. Work on the issue in the worktree
4. Link a PR when ready: `bork integration attach-pr bork-1 123`
5. Move to review: `bork issue move bork-1 code-review`
6. Mark done: `bork issue move bork-1 done`

## Column values

| CLI value      | Board column  |
|----------------|---------------|
| `todo`         | To Do         |
| `in-progress`  | In Progress   |
| `code-review`  | Code Review   |
| `done`         | Done          |

## Notes

- Issue IDs are case-insensitive (`bork-1` and `BORK-1` both work)
- Issue IDs follow the pattern `{project-name}-{number}` (e.g. `bork-1`)
- Linear-imported issue IDs use the Linear identifier (e.g. `vil-123`)
- The CLI reads/writes `.bork/state.json` directly with atomic writes
