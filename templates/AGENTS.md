# {project_name}

Managed with bork across git worktrees and tmux.

## Project Layout

```
{project_name}/                     # Container directory (NOT a git repo)
├── .bork/                          # Bork state (config.toml, state.json)
├── AGENTS.md
├── opencode.jsonc
├── main/                           # Main branch worktree (git repo)
│   └── CLAUDE.md                   # Project instructions
└── {issue-id}-{slug}/              # Issue worktrees
```

## Worktree Conventions

- Use `bork issue start "Title" --project {project_name} --prompt "Details..."` to spin off a new issue with its own worktree and agent
- Use `bork worktree <issue-id> <slug>` when you only need to create/register a worktree for an existing issue
- Worktree directory: `{issue-id}-{slug}` (e.g. `{project_name}-1-fix-bug`)
- Branch: `{issue-id}/{slug}` (e.g. `{project_name}-1/fix-bug`)
- Issue IDs: `{project_name}-{number}`
- Linear-imported issue IDs: lowercase Linear identifier (e.g. `abc-123` for `ABC-123`)
- Tmux sessions: `{project_name}-{issue-id}`

## CLI: Managing Issues

Use `bork issue` and `bork integration` to manage the board from the command line. The TUI picks up changes within 2 seconds.

```bash
bork issue list                               # Table output
bork issue list --json                        # JSON output
bork issue create "Fix auth bug"              # Create in To Do column
bork issue create "Task" --agent claude --mode build --prompt "Details..."
bork issue start "Subtask" --project {project_name} --prompt "Implement this in a separate worktree"
bork issue show {project_name}-1              # Show details
bork issue update {project_name}-1 --title "New title" --column code-review
bork issue move {project_name}-1 done         # Move to column
bork issue archive {project_name}-1           # Kill session + teardown + remove worktree + Done
bork issue delete {project_name}-1            # Delete
bork integration attach-linear {project_name}-1 VIL-123   # Link Linear ticket
bork integration attach-pr {project_name}-1 42             # Link GitHub PR
```

Create flags: `--column` (todo, in-progress, code-review, done), `--agent` (opencode, claude, codex, pi), `--mode` (plan, build, yolo), `--prompt`, `--kind` (agentic, todo).
