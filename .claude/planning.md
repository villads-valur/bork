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
Live check done: GraphQL `PullRequest.stack` works, direct requests come back with stack numbers, and the Legora board stayed clean on restart (cards with agent work were left alone).

Fixed the review queue flood: use user-review-requested instead of team-inclusive review-requested. Read-only inspection found 59 new cards in Legora. Regression test covers moving extra automatic cards to Done after successful discovery while preserving manual cards and retaining state on failed discovery. Legora state was not edited.

Fixed hidden requests: a card with your work on it (or any Done card) used to block the stack card and also stop the individual import, so the request never showed. Done cards are now ignored, and blocked requests fall back to individual cards.
