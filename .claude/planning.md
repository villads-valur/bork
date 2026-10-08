# bork-196: automatic stack reviews

Follow-up to merged PR #133. New branch/PR only.

- Fetch direct personal review requests and stack numbers atomically; paginate and dedupe.
- Resolve each referenced stack once before importing it. Unknown membership must not become standalone cards.
- One card per stack; existing attachments win. Preserve edited/manual cards and consolidate safe automatic imports.
- Complete/reopen automatic stack cards based on outstanding member requests.
- Add stack_review_prompt to global/project config and CLI; append ordered membership and requested markers.
- Preserve background discovery, error/backoff/cache behavior, and explicit individual picker workflow.
- Test transitions, pagination/duplicates/failures, config precedence; rebuild debug, commit/push new PR.

Implemented and validated: 973 tests passed, 2 ignored; formatting and Clippy clean; debug binary rebuilt at target/debug/bork.
GitHub access became unavailable during final validation (DNS failure for github.com; gh cannot connect to api.github.com). Final live smoke test and PR publication still pending.

Fixed the review queue flood: use user-review-requested instead of team-inclusive review-requested. Read-only inspection found 59 new cards in Legora. Regression test covers moving extra automatic cards to Done after successful discovery while preserving manual cards and retaining state on failed discovery. Legora state was not edited.
