---
name: bork-cli
description: Manage bork issues, integrations and backups from the command line
---

# Bork CLI

Use `bork issue` and `bork integration` commands to manage the kanban board programmatically, and `bork backup` / `bork import` to back it up and restore it. Changes are picked up by the TUI within 2 seconds.

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
- `--agent`: opencode (default from project config), claude, codex, pi
- `--mode`: plan (default), build, yolo
- `--kind`: agentic (default), todo (non-agentic)
- `--prompt`: agent prompt text
- `--model`: model passed to the agent's `--model` flag (e.g. `sonnet`). Errors for opencode, which has no such flag. Changing the agent clears it.

### Start an issue agent

Use this when you want to spin off a new issue and have another agent work on it immediately.

```bash
bork issue start "Implement search" --prompt "Add search using the existing indexing code"
bork issue start "Fix auth bug" --agent claude --mode build --slug fix-auth-bug
bork issue start "Review PR 42" --agent claude --model sonnet --prompt "Review the diff"
bork issue start "Add API route" --project my-api --prompt "Implement this in my-api"
bork issue start "Investigate flaky test" --no-worktree --prompt "Find the root cause"
bork issue start "Fix login" --linear VIL-789 --prompt "Details..."  # link a ticket before launch
```

This creates an agentic issue, creates a git worktree by default, moves the issue to In Progress, and starts the configured agent in a tmux session. If the project configures a `setup_script`, it runs inside the worktree before the agent starts.

Options:
- `--prompt`: agent prompt text
- `--agent`: opencode (default from project config), claude, codex
- `--mode`: plan, build (default), yolo
- `--model`: model passed to the agent's `--model` flag (e.g. `sonnet`). Errors for opencode, which has no such flag. Changing the agent clears it.
- `--slug`: worktree/branch slug (generated from title by default)
- `--no-worktree`: launch without creating a worktree
- `--project`: registered project name or path (defaults to current project)

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
bork issue update bork-1 --model opus    # used from the next launch or resume
bork issue update bork-1 --model ""      # clear the model
```

Only provided fields are changed; others remain untouched.

### Move an issue

```bash
bork issue move bork-1 in-progress
bork issue move bork-1 done
bork issue move bork-1 todo
```

Moving to `done` sets a completion timestamp. Moving away from `done` clears it.

### Archive an issue

Use this to clean up when an issue's work is merged. It kills the agent's tmux session, runs the project's `teardown_script` (if configured) inside the worktree, removes the worktree, and moves the issue to Done.

```bash
bork issue archive bork-1
bork issue archive bork-1 --force    # proceed past a failing teardown, discard uncommitted changes
```

Without `--force`, the archive aborts if the teardown script fails or the worktree has uncommitted changes.

### Run an issue's agent in the current terminal

bork types this into each issue's tmux pane. It runs the setup script if it's due, then replaces itself with the agent (resuming the stored session when there is one). Don't run it from inside an agent session; use `bork issue start` to spin off work.

```bash
bork issue launch bork-1
```

### Start a fresh agent session

Forget the issue's stored session so the next launch starts a new conversation with the issue's prompt. Kills the live session if it runs that agent.

```bash
bork issue reset-session bork-1
bork issue reset-session bork-1 --agent codex    # a different agent's stored session
```

### Attach an existing agent session

Store a session id on an agentic issue so the next launch resumes that conversation (e.g. a recreated issue, or a session started outside bork). It goes under the issue's agent, or under `--agent` when passed too. The id must exist for that agent; a live session running that agent is killed.

```bash
bork issue update bork-1 --session 3f2c9a1e-...                  # the issue's current agent
bork issue update bork-1 --agent claude --session 3f2c9a1e-...   # switch to claude and attach
```

### Delete and restore an issue

A delete keeps the issue restorable (with its id, sessions, PR/Linear links and the links on other issues) for `keep_deleted_days` (30 by default). In the TUI, `u` undoes a delete for 10 seconds.

```bash
bork issue delete bork-1
bork issue list --deleted          # restorable issues (--json for machine output)
bork issue restore bork-1
bork issue purge bork-1            # or --all; also removes plans/<id>/
```

## Integrations

### Link a Linear ticket

```bash
bork integration attach-linear bork-1 VIL-123
```

This sets the Linear identifier on the issue so the TUI can display the link and sync status.

### Link a Linear project (orchestrators only)

```bash
bork integration attach-linear-project bork-1 https://linear.app/acme/project/auth-1a2b3c4d5e6f
bork integration detach-linear-project bork-1 1a2b3c4d5e6f
```

Takes a project URL or slug. Fails on issues that aren't orchestrators.

### Link a GitHub PR

```bash
bork integration attach-pr bork-1 42
bork integration attach-pr 42          # inside an agent session: uses BORK_ISSUE_ID
```

This sets the PR number on the issue so the TUI can display PR status (checks, reviews, etc.). Always run it right after you open a PR for an issue, including stacked or follow-up PRs.

## Backups

### Back up

```bash
bork backup                     # all opted-in projects -> <path>/bork-backup-<host>.tar.gz
bork backup --project bork      # just one project -> <path>/bork-backup-<host>--bork.tar.gz
bork backup --dry-run           # print the file path and its contents, write nothing
bork backup status              # opted-in projects, last run, result and size per file
```

The file holds what git doesn't: each opted-in project's board and config, container files (`AGENTS.md`, `plans/`, `.claude/`, ...) and every live worktree's `.claude/planning.md`, plus the global `projects.json` and `config.toml`. It goes to the global `[backup] path`. Code in worktrees and agent transcripts are not backed up. The TUI also backs up once a day (`Shift+B` runs one now).

### Restore

```bash
bork import <backup-file>                 # every project, back to its recorded path
bork import                               # this Mac's backup file in the configured path
bork import <backup-file> --root ~/code   # restore to ~/code/<name> instead
bork import <backup-file> --project bork  # just one project
bork import <backup-file> --dry-run       # print what would be cloned, written and registered
bork import <backup-file> --force         # restore over existing directories
```

Rebuilding a wiped laptop:

1. Install bork
2. `bork import --dry-run <backup-file>` and check the output with the user
3. `bork import <backup-file>`: clones each repo into `main/`, restores boards, docs and plans, registers the projects
4. Worktrees aren't recreated; the summary lists the affected issues as `<issue-id> (<branch>)`. Reopen one from its pushed branch with `bork worktree <issue-id> <slug> --base origin/<branch>`, where `<slug>` is the part of `<branch>` after `<issue-id>/` (omit it when the branch is just `<issue-id>`). Without `--base` the branch starts from `main`. Issues listed without a branch have none recorded

### Rules for agents

- Run `bork backup --dry-run` before telling the user what is or isn't backed up
- Never edit `[backup]` config or turn backups on/off for a project unless the user asks. Some projects (e.g. work projects) are deliberately opted out
- Never pass `--force` to `bork import` without the user's explicit OK

## Typical workflow

1. Spin off work: `bork issue start "Implement feature X" --prompt "Details..."`
2. The new agent works in the created worktree
3. Open a PR and link it straight away: `bork integration attach-pr bork-1 123`
4. Move to review: `bork issue move bork-1 code-review`
5. When merged, clean up: `bork issue archive bork-1` (kills session, runs teardown, removes worktree, marks Done)

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
