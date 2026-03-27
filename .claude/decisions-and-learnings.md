# Decisions & Learnings

> Last updated: 2026-03-27

## Key Decisions

### Filter mode over highlight+jump

**Decision:** Search hides non-matching issues rather than just highlighting matches
**Rationale:** User preference, simpler UX for a kanban board context
**Date:** 2026-03-27

### Esc always emits ClearSearch in normal mode

**Decision:** Rather than passing app state to the keybinding mapper, Esc in normal mode always emits ClearSearch and the handler ignores it when no search is active
**Rationale:** Keeps `map_key_to_action` signature simple (no app state dependency)
**Date:** 2026-03-27

### No n/N cycling

**Decision:** No next/prev match cycling since filter mode makes it unnecessary
**Rationale:** With filtering, j/k navigation works naturally. Avoids remapping `n` from CreateIssue
**Date:** 2026-03-27

## Gotchas & Warnings

- `issues_in_column()` is used everywhere (navigation, rendering, selection) so filtering there propagates automatically
- `clamp_all_rows()` must be called whenever search_query changes to prevent out-of-bounds selection

## Lessons Learned

- (to be filled during implementation)
