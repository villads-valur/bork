# bork-13: Backup core (config, `bork backup` CLI, board + container files)

Part of bork-7 (plans/bork-7/planning.md in the container).

## Module layout
- `src/backup/mod.rs`: public API (`plan_project`, `run_project`, `run_global`, `run_all`), outcome types, WIP extension point
- `src/backup/config.rs`: `[backup]` section, read from global + project `config.toml` via `toml_lite`
- `src/backup/select.rs`: which container files get backed up (worktree detection + ephemeral excludes)
- `src/backup/manifest.rs`: `manifest.json` v1 (remotes, default branch, worktrees with HEAD + WIP ref)
- `src/backup/mirror.rs`: mirror a file list into `<dest>/<project>/` (copy changed, delete stale, atomic copies)
- `src/backup/git_dest.rs`: git destination (init if missing, stage subdir, commit only if changed, push if remote, never force)
- `src/backup/status.rs`: `.bork/backup.json` last-run record
- CLI in `main.rs`: `bork backup [--project <name>] [--dry-run]`, `bork backup status`

## Config semantics
- `enabled` is only honoured from the project layer (per-project opt-in), default false
- `destination`, `path`, `interval_minutes`, `wip`: project overrides global
- Global files (`projects.json`, `config.toml`) go to `<global path>/_global/` when global `[backup]` has a `path`
- `path` supports `~` expansion

## Selection
- Container: every top-level entry except `.bork/`, `main/`, worktree dirs (union of `git worktree list` paths, `issue.worktree` names in state, and dirs containing a `.git` entry), ephemeral names (`.DS_Store`, `*.lock`, `*.tmp`, `*.tmp.<pid>`, `*.corrupt`)
- `.bork/`: only `state.json` and `config.toml`
- Symlinks are copied as symlinks

## Manifest
No timestamps inside, so git mode only commits on real change.

## Status
- [x] Plan
- [x] Implement + tests
- [x] Docs (CLAUDE.md, README)
- [ ] verify.sh, PR, attach
