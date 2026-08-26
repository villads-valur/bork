# bork-146: clean up zsh and other processes when killing an issue

> Last updated: 2026-08-26

## Findings

- zsh itself is not leaking: the apparent PID 1 children belong to active
  pane TTYs and exit when tmux closes the pane.
- Codex, OpenCode, and self-daemonizing commands can outlive the tmux session.
- `bork issue delete` did not terminate its issue session.
- `tmux::kill_session` discarded failures and always reported success.

## Implementation

- Snapshot the POSIX session ID for every tmux pane before teardown, then kill
  survivors that retain those IDs after the pane closes.
- On Linux, also identify survivors through the inherited `BORK_SESSION`
  environment marker so descendants that call `setsid` remain discoverable.
- Escalate survivor cleanup from SIGTERM to SIGKILL after a grace period.
- Route issue kill, delete, archive, kind-change, and TTL cleanup through one
  cleanup function.
- Validate tmux exit status and clean transient status/prompt files.
- Preserve issues when teardown fails: CLI delete/archive now return the
  failure, and TUI delete waits for successful asynchronous cleanup before
  removing the card.

## Status

- [x] Investigation and isolated reproduction
- [x] Initial implementation and unit tests
- [x] Sync worktree with current `origin/main`
- [x] Review cleanup safety and portability
- [x] Code-review and simplification pass
- [x] Formatting, clippy, and all 787 tests pass
- [x] End-to-end test: detached `nohup` processes from two tmux windows are
      both terminated by `bork issue delete`

## Limitation

On macOS, a process that deliberately calls `setsid` before tmux teardown can
escape the pane's POSIX session, and the OS does not expose detached-process
environments to recover the `BORK_SESSION` marker. Such fully daemonized
services still require the project's `teardown_script`. Normal background and
`nohup` jobs, agent subprocesses, zsh helpers, and every tmux window are covered.
