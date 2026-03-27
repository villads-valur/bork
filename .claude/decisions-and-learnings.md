# Decisions & Learnings

> Last updated: 2026-03-27

## Key Decisions

### Use GraphQL instead of `gh pr list --json`

**Decision:** Use `gh api graphql` with a single query to fetch all open PRs
**Rationale:** Single API point for up to 100 PRs. Gets statusCheckRollup, reviewDecision, and diff stats in one call. Same approach as tdeck.
**Date:** 2026-03-27

### Branch-name matching for PR association

**Decision:** Match PRs to issues via worktree -> branch name -> PR headRefName
**Rationale:** Already have worktree_branches populated by the git worker. No need to store PR numbers on issues. Ephemeral matching is simpler and always up-to-date.
**Date:** 2026-03-27

### Bump CARD_HEIGHT to 5

**Decision:** Add a dedicated line for PR info instead of cramming it onto the status line
**Rationale:** The status line is already dense (session indicator + agent status + branch + git changes). A separate PR line gives room for PR number, checks, review, and diff stats.
**Date:** 2026-03-27

### PR data is ephemeral (not persisted)

**Decision:** PR statuses are only kept in memory, re-fetched on startup
**Rationale:** PRs change frequently. Persisting stale data adds complexity for little benefit. First poll happens immediately on startup.
**Date:** 2026-03-27

### Wake-up channel for force-sync

**Decision:** Use a separate `mpsc::Sender<()>` to wake the PR worker from its sleep early
**Rationale:** The worker sleeps in 500ms increments checking the wake channel, so pressing P triggers an immediate poll without complex thread synchronization.
**Date:** 2026-03-27

## Gotchas & Warnings

- `gh` CLI must be authenticated and the cwd must be a git repo with a GitHub remote
- GraphQL statusCheckRollup can be null if no CI is configured
- reviewDecision can be null if no reviewers are assigned
- OnceLock caches repo identity for the entire process lifetime

## Lessons Learned

- tdeck caches repo identity since it never changes within a session, OnceLock is the idiomatic Rust equivalent
- `recv_timeout` in a loop gives interruptible sleep without unsafe or extra crates
