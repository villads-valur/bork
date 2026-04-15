---
name: worktree
description: Create git worktrees and register them with bork for issue tracking
---

# Worktree Management

Create and manage git worktrees in a bork project.

## Context

This is a bork project. The directory layout is:

```
project/                    # container root (this is where you are)
├── main/                   # main branch worktree (owns .git/)
├── {issue-id}-{slug}/      # issue worktrees (siblings of main/)
├── .bork/                  # bork state (do not modify directly)
├── AGENTS.md               # project instructions
└── opencode.jsonc           # opencode config
```

The `main/` directory is a regular git repo. All worktrees are created from it and live as sibling directories inside this container.

## Creating a worktree

**Always use `bork worktree` to create worktrees.** This creates the git worktree AND registers it with bork's state so the TUI can track it.

```bash
bork worktree {issue-id} {slug}
```

**Example:**
```bash
bork worktree bork-14 add-worktree-support
```

This creates:
- Directory: `bork-14-add-worktree-support/` (sibling of `main/`)
- Branch: `bork-14/add-worktree-support` (branched from current HEAD of main)
- Updates `.bork/state.json` to link the issue to the worktree

To also create the issue if it doesn't exist:
```bash
bork worktree bork-14 add-worktree-support --title "Add worktree support"
```

## Naming conventions

- **Worktree directory**: `{issue-id}-{slug}` when a slug is provided (e.g. `bork-14-add-worktree-support`), or just `{issue-id}` without a slug
- **Branch name**: `{issue-id}/{kebab-case-slug}` (e.g. `bork-14/add-worktree-support`)
- Issue IDs follow the pattern `{project-name}-{number}` (e.g. `bork-1`, `bork-14`)

## Linear issues

When an issue was imported from Linear, the bork issue ID is the Linear identifier in lowercase (e.g. `vil-123` for Linear issue `VIL-123`). The worktree follows the same naming pattern:

```bash
bork worktree vil-123 fix-auth-flow
```

This creates:
- Directory: `vil-123-fix-auth-flow/`
- Branch: `vil-123/fix-auth-flow`

When Linear is not used, issue IDs are regular bork IDs (e.g. `bork-14`).

## After creating a worktree

1. Create a planning file:
   ```bash
   mkdir -p {worktree-dir}/.claude
   ```
   Then create `{worktree-dir}/.claude/planning.md` with the task plan.

2. Do all work for the issue inside the worktree directory, not in `main/`.

## Listing worktrees

```bash
git -C main worktree list
```

## Removing a worktree

When work is done and merged:

```bash
git -C main worktree remove ../{worktree-dir}
```

Or forcefully if there are uncommitted changes:

```bash
git -C main worktree remove --force ../{worktree-dir}
```

## Managing issues from CLI

Use `bork issue` commands to manage issues without the TUI:

```bash
bork issue create "Fix the bug" --prompt "Details..."
bork issue list
bork issue move bork-1 in-progress
bork issue show bork-1
```

See the `bork-cli` skill for full CLI reference.

## Important

- Never commit directly to `main` from a worktree. Always use feature branches.
- The `main/` worktree should stay on the `main` branch.
- Multiple worktrees can exist simultaneously for parallel work streams.
- Each worktree is a full checkout sharing the same git objects (disk efficient).
