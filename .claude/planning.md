# Current Work

> Last updated: 2026-03-27

## Active Task

**Task:** Vim-style incremental search for issue titles
**Status:** In Progress

## Plan

1. Add search action variants to `input/action.rs`
2. Add `InputMode::Search`, `search_query` field, and search helpers to `app.rs`
3. Modify `issues_in_column()` to filter by search query
4. Wire up keybindings in `input/keybindings.rs` (`/`, Esc, char input)
5. Add search action handling in `handler.rs`
6. Render search query in footer (`ui/status_bar.rs`)
7. Build + verify with `cargo check` and `cargo clippy`

## Progress

- [ ] action.rs — add SearchStart, SearchChar, SearchBackspace, SearchConfirm, SearchCancel, ClearSearch
- [ ] app.rs — InputMode::Search, search_query field, filtered issues_in_column, helpers
- [ ] keybindings.rs — map_search_key, `/` in normal, Esc in normal
- [ ] handler.rs — handle_search, ClearSearch in normal mode
- [ ] status_bar.rs — search prompt in footer
- [ ] cargo check + clippy

## Notes

- Filter mode: non-matching issues hidden from board while search active
- Incremental: filter updates as you type
- Search scope: all columns, title-only match (case-insensitive)
- Esc clears search from any mode
- `/` starts search, Enter confirms (returns to normal with filter active)
- `n` stays mapped to CreateIssue (no conflict since we use filter, not jump)
- Esc in normal mode always emits ClearSearch; handler ignores when no search active
