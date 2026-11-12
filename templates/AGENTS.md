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

## Where to Work

- Agents start in the container root, which is not a git repo. Run `bork issue show $BORK_ISSUE_ID` and check `Worktree:`.
- If your issue has a worktree, `cd` into `{issue-id}-{slug}/` and do everything there: reading, edits, git, builds and tests. Use absolute paths, since each shell call may start back in the container.
- If `bork issue show` lists a `PR:`, push to that PR's head branch instead of opening a new PR. Check `git branch --show-current` first, because the worktree branch may be named `{issue-id}/{slug}`.
- `main/` tracks the default branch. Read it for reference, but never edit, commit or switch branches there.
- Issues without a worktree (like orchestrators) don't change code directly. Spin off code changes with `bork issue start`.
- Project instructions are in `CLAUDE.md` at the repo root. Read the copy in your worktree, since it may differ from `main/`.
- Don't edit `.bork/state.json` by hand. Use the `bork issue` and `bork integration` commands.

## Worktree Conventions

- Use `bork issue start "Title" --project {project_name} --prompt "Details..."` to spin off a new issue with its own worktree and agent
- Use `bork worktree <issue-id> <slug>` when you only need to create/register a worktree for an existing issue
- Worktree directory: `{issue-id}-{slug}` (e.g. `{project_name}-1-fix-bug`)
- Branch: `{issue-id}/{slug}` (e.g. `{project_name}-1/fix-bug`)
- Issue IDs: `{project_name}-{number}`
- Linear-imported issue IDs: lowercase Linear identifier (e.g. `abc-123` for `ABC-123`)
- Tmux sessions: `{project_name}-{issue-id}`

## Pull Requests

- After opening a PR, link it to your issue: `bork integration attach-pr <pr-number>` (inside an agent session the issue ID comes from `BORK_ISSUE_ID`; elsewhere pass it first, e.g. `bork integration attach-pr {project_name}-1 42`)
- Link every PR you open for the issue, including stacked or follow-up PRs

## CLI: Managing Issues

Use `bork issue` and `bork integration` to manage the board from the command line. The TUI picks up changes within 2 seconds.

```bash
bork issue list                               # Table output
bork issue list --json                        # JSON output
bork issue list --linked {project_name}-1     # Only issues linked to {project_name}-1
bork issue create "Fix auth bug"              # Create in To Do column
bork issue create "Task" --agent claude --mode build --prompt "Details..."
bork issue start "Subtask" --project {project_name} --prompt "Implement this in a separate worktree"
bork issue start "Subtask" --project {project_name} --linear VIL-123 --prompt "..."  # Link a Linear ticket at start
bork issue show {project_name}-1              # Show details
bork issue update {project_name}-1 --title "New title" --column code-review
bork issue move {project_name}-1 done         # Move to column
bork issue archive {project_name}-1           # Kill session + teardown + remove worktree + Done
bork issue delete {project_name}-1            # Delete (restorable for 30 days)
bork issue restore {project_name}-1           # Restore a deleted issue with its links
bork issue list --deleted                     # Deleted issues that can be restored
bork integration attach-linear {project_name}-1 VIL-123   # Link Linear ticket
bork integration attach-pr {project_name}-1 42             # Link GitHub PR (issue ID optional inside an agent session)
bork integration detach-linear {project_name}-1 VIL-123   # Unlink a Linear ticket
bork integration attach-linear-project {project_name}-1 <project-url>  # Link Linear project (orchestrators only)
bork integration detach-linear-project {project_name}-1 <project-url>  # Unlink a Linear project
bork integration detach-pr {project_name}-1 42             # Unlink a PR
bork integration clear-linear {project_name}-1             # Remove all Linear links
bork integration clear-pr {project_name}-1                 # Remove all PR links
bork integration link {project_name}-1 {project_name}-3    # Tie two issues together
bork integration unlink {project_name}-1 {project_name}-3  # Remove a link
```

Create flags: `--column` (todo, in-progress, code-review, done), `--agent` (opencode, claude, codex, pi, cursor), `--mode` (plan, build, yolo), `--prompt`, `--model` (passed to the agent's `--model`, e.g. `sonnet`; not supported by opencode), `--kind` (agentic, todo, orchestrator).

`bork issue start` also takes `--slug`, `--base <branch>` (start the worktree from another branch), `--no-worktree` and `--link <issue-id>` (defaults to the spawning issue inside an agent session).
