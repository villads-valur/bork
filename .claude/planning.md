# Current Work

> Last updated: 2026-03-27

## Active Task

**Task:** TDD test suite for session lifecycle features
**Status:** In Progress

## Plan

1. Write failing tests for auto-move to In Progress on session start
2. Write failing tests for Done session TTL (done_at timestamp, config parsing, cleanup logic)
3. Write failing tests for git polling skip/freeze for Done issues
4. Write tests for existing pure functions (parse_git_status, Column nav, Issue serde)
5. Verify all tests compile and fail for expected reasons
6. Hand off to implementation phase

## Progress

- [ ] Worktree created
- [ ] Feature 1 tests: auto-move to InProgress
- [ ] Feature 2 tests: done_at + TTL + cleanup
- [ ] Feature 3 tests: git polling skip + frozen status
- [ ] Existing logic tests: parse_git_status, Column, serde compat
- [ ] All tests compile and fail correctly

## Notes

TDD approach: write tests first, then implement features to make them pass.
Three features:
1. Auto-move issue from Todo -> InProgress when starting a session (Enter key)
2. Auto-kill tmux sessions for Done issues after configurable TTL (default 5 min)
3. Stop git polling for Done worktrees, freeze their last-known status
