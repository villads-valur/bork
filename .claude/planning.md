# Current Work

> Last updated: 2026-03-27

## Active Task

**Task:** Background PR status polling via `gh api graphql`, match PRs to branches, show PR status on cards
**Status:** Done

## Plan

1. Add PR domain types to `types.rs` (PrState, ChecksStatus, ReviewDecision, PrStatus)
2. Create `external/github.rs` - GraphQL-based PR fetching via `gh api graphql`
3. Add PR poll worker in `main.rs` (60s interval, wake-up channel for force-sync)
4. Add `pr_statuses: HashMap<String, PrStatus>` to `App` + `pr_for()` lookup method
5. Bump `CARD_HEIGHT` from 4 to 5, add dedicated PR line to card rendering
6. Add PR display helpers in `ui/styles.rs` (checks icon/color, review icon/color)
7. Pass `PrStatus` through `CardContext` in `board.rs`
8. Add `SyncPRs` (P) and `OpenPR` (o) actions
9. Update status bar hints

## Progress

- [x] Types (PrState, ChecksStatus, ReviewDecision, PrStatus)
- [x] GitHub module (GraphQL fetch, repo identity caching, browser open)
- [x] PR poll worker (60s interval with wake-up channel)
- [x] App state integration (pr_statuses HashMap, pr_for() lookup)
- [x] Card rendering (CARD_HEIGHT=5, dedicated PR line)
- [x] Keybindings (P=sync, o=open PR)
- [x] Status bar hints (contextual "open pr" when PR exists)
- [x] Build verification (cargo check + clippy + fmt)
