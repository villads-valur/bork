# Decisions & Learnings

> Last updated: 2026-03-27
>
> Capture decisions, gotchas, and lessons learned for future reference.

## Key Decisions

### Test structure: inline #[cfg(test)] modules

**Decision:** Put tests in `#[cfg(test)] mod tests` at the bottom of each source file
**Rationale:** Idiomatic Rust, gives access to private functions, no new files needed
**Date:** 2026-03-27

### TDD for new features, test existing pure functions too

**Decision:** Write failing tests for the 3 new features plus tests for existing untested pure functions
**Rationale:** Establishes test coverage baseline and validates new feature contracts before implementation
**Date:** 2026-03-27

### done_at as Option<u64> on Issue struct

**Decision:** Track when an issue moved to Done via a unix timestamp on the Issue struct
**Rationale:** Persisted across restarts, simple, no separate tracking needed
**Date:** 2026-03-27

### done_session_ttl configurable, default 300s

**Decision:** Add done_session_ttl to config.toml, default 5 minutes
**Rationale:** Different projects may want different cleanup windows
**Date:** 2026-03-27

## Gotchas & Warnings

- Tests that reference new fields (done_at) or new config fields (done_session_ttl) will fail to compile until the struct changes are made
- Need to ensure backwards compat: deserializing old state.json without done_at should default to None

## Lessons Learned

- (to be filled during implementation)
