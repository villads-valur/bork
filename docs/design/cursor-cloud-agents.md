# bork-198: Cursor cloud agents in bork (design)

Phase 2 of bork-180. Design only, nothing here is implemented. Code references are
against `main` at `4e4bb66` (#135).

**v1 is a deliberately narrow, experimental slice.** The goal is to test Cursor cloud
agents in bork on their own, without touching how local agents behave. Everything is
off by default, lives in a new module, and touches bork core in a short, listed set of
places that only run for cloud issues. Anything not needed to prove the basic loop
(start an agent, watch it, get its PR, stop it) is listed under "Later" with enough
notes that the thinking isn't lost.

## Where the facts come from

- The endpoint and status list was checked by hand against the Cursor docs on 2026-10-08.
- On top of that I read Cursor's public OpenAPI spec
  (`cursor.com/docs-static/cloud-agents-openapi.yaml`, v1, marked public beta). I made
  no API calls and created no agents. Four things from the spec shape the design:
  1. **`POST /v1/agents` accepts a client-supplied `agentId`** (pattern `bc-<uuid>`).
     Re-POSTing the same id returns `409 agent_id_conflict`. That makes creation safe
     to retry, which matters when every duplicate costs money.
  2. **`Agent.status` is only `ACTIVE | ARCHIVED`** in the spec ("Execution status
     lives on runs"). The docs overview also lists `IDLE`. The design never depends on `IDLE`
     and treats it as `ACTIVE` if it shows up.
  3. **The request body is v1-shaped, not v0**: `prompt.text`, `repos[]` (`url`,
     optional `startingRef`), top-level `autoCreatePR`, `workOnCurrentBranch` (default
     false, meaning Cursor pushes to a new `cursor/...` branch), `mode`, `model`,
     `name` (max 100 chars). There is **no `branchName`** field.
  4. **`Agent.url`** is "URL to view the agent in Cursor Web". `GET /v1/me` returns
     key ownership info. Errors come back as `{ error: { code, message } }`, `429`
     carries `Retry-After`, and `409` covers `agent_busy`, `agent_archived` and
     `run_not_cancellable`.

## The problem

Every agentic issue in bork today means a tmux session plus (usually) a git worktree
on this machine. The UI is built on that:

- **Status** is `Project::resolved_agent_status` (`src/app.rs:711`): look up the status
  file by tmux session name, and if the tmux session is gone, the answer is `Stopped`.
- **Enter** (`Action::OpenSession`, `src/handler.rs:575`) attaches a tmux popup, or
  launches a session.
- **Branch and PR** come from the worktree (`Project::branch_for`, `src/app.rs:863`).
- **Teardown** (`agent::terminate_session`, `ops::delete_issue`, `ops::archive_issue`)
  kills a tmux session and removes a worktree.

A Cursor cloud agent has none of this. It runs on Cursor's VMs, clones the repo
itself, pushes to a branch Cursor names, and opens a PR. There's no pane, no
checkout, no status file and no local process. Its state lives behind an HTTP API
that bills per run. So the work isn't adding a sixth harness. It's fitting a remote,
billed thing into a model where "agent" has always meant "tmux session", without
disturbing the tmux path.

## v1 scope

### In

| Area | v1 does exactly this |
|---|---|
| Switch | `cloud_agents = true` in config turns it on. The default is off. Set it in one scratch project's `.bork/config.toml` for testing, not globally. |
| Key | macOS keychain only. `bork cloud login` stores the key and checks it with `GET /v1/me`. `bork cloud logout` removes it. |
| Create | CLI only: `bork issue start "Title" --cloud --prompt "..." [--base <ref>] [--yes]`. Fixed settings: `mode: "agent"`, `autoCreatePR: true`, Cursor's default model. |
| Board | Cloud cards render with a `· cloud` badge and a status. Enter opens the Cursor web page. `o` opens the PR once it's linked. That's all the TUI does with cloud cards. |
| Polling | One worker. Every 30s it does `GET /v1/agents/{id}` and, if needed, one run GET per tracked agent. |
| PR | `prUrl` from the run response becomes a normal `github_pr_links` entry. |
| Stop | `bork issue cancel <id>` cancels the run. `bork issue archive` and `bork issue delete` cancel, then archive the Cursor agent. Nothing ever calls `DELETE`. |

### Out (see "Later")

TUI create, TUI cancel, follow-up prompts, `--mode`/`--model`/`--no-pr`, a
`cloud_prompt` preamble, the `CURSOR_API_KEY` env var, Linux, list-based or adaptive
polling, SSE, sidebar activity markers, a staleness marker, finding a PR by branch,
importing agents bork didn't create, multi-repo, artifacts, environments/secrets,
and anything that lets an agent session start a cloud run.

### Isolation guarantee

With `cloud_agents` off and no Cloud issues in `state.json`, bork behaves exactly as
it does today. Here's how that holds:

- All cloud logic lives in a new `src/cloud/` module (`mod.rs`, `auth.rs`, `http.rs`,
  `poll.rs`, `ops.rs`).
- Core files get a short, listed set of touch points (table below). Each one is a
  `kind == IssueKind::Cloud` branch, or is gated on the config flag. A Cloud issue can
  only be created through `--cloud`, which refuses to run when the flag is off.
- The cloud worker isn't spawned unless the flag is on *and* a key is in the keychain.
- **The existing test suite must pass with no edits to existing tests.** If a PR has
  to change an existing test, that's a sign it has leaked into the local path, and it
  goes back for rework.

Core touch points (the whole list):

| File | Change |
|---|---|
| `src/types.rs` | `IssueKind::Cloud`, `CloudAgent`, `Issue.cloud`; `set_kind` refuses to convert to or from Cloud |
| `src/app.rs` | one `Cloud` branch at the top of `resolved_agent_status`/`resolved_activity`; `merge_issue_fields` gets the new field; `LiveState.cloud_runs`; worktree auto-detect skips Cloud (`app.rs:805`) |
| `src/handler.rs` | a `Cloud` branch before the `is_agentic()` check in `OpenSession`/`StartSession` (open the URL); `x` on a Cloud card shows "use `bork issue cancel`" |
| `src/main.rs` | `--cloud` on `issue start`, `issue cancel`, `bork cloud login|logout`, `parse_issue_kind` accepts nothing new (Cloud isn't user-selectable), worker spawn gated on flag + key, drain the worker's channel |
| `src/ops.rs` | `delete_issue`/`archive_issue` call into `cloud::ops` for Cloud issues |
| `src/ui/card.rs` | badge arm in the existing `match` |
| `src/ui/dialog.rs`, `src/dialog_state.rs` | the kind selector never offers Cloud; on a Cloud card the kind is locked |
| `src/config.rs` | the `cloud_agents` bool |

`is_agentic()` returns `false` for Cloud, so bulk start (`handler.rs:199`) skips cloud
cards on its own. Starting several cloud agents with one key press is bulk spending,
and that's exactly what v1 shouldn't allow.

## Design

### 1. Data model: `IssueKind::Cloud` plus an `Issue.cloud` field

```rust
// src/types.rs
pub struct CloudAgent {
    pub agent_id: String,             // "bc-<uuid>", minted by bork before the POST
    pub created: bool,                // false until Cursor confirms the agent exists
    pub run_id: Option<String>,       // latest run bork knows about
    pub url: Option<String>,          // Agent.url, the Cursor web page
    pub repo_url: String,             // what we sent in repos[0].url
    pub starting_ref: Option<String>, // what we sent in repos[0].startingRef
    pub branch: Option<String>,       // from git.branches[], shown on the card only
    pub last_run_status: Option<CloudRunStatus>, // last terminal status seen
    pub archived: bool,
    pub gone: bool,                   // GET returned 404 (deleted in Cursor)
}

pub struct Issue {
    // ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudAgent>,
}
```

`agent_kind` is set to `AgentKind::Cursor` for display, and nothing launches from it.

**Why `IssueKind` and not `AgentKind`.** `AgentKind` answers "which binary goes in
the tmux pane". It drives the provider registry (`agent::binary`, `mode_flag`, the
launch and resume strings), PATH detection, the `agents` allowlist and the `sessions`
map key. A `CursorCloud` variant would need a fake binary and a fake launch string in
every provider `match`, which drags cloud straight into the local launch path. That's
the opposite of isolation. `Issue::set_agent_kind` also assumes switching agents is
cheap ("kill the tmux session"). For cloud, a switch would leave a billed agent
running with nothing pointing at it. `IssueKind` answers "what kind of card is this":
what Enter does, whether it gets a worktree, whether it gets a PR. Those are the
things that differ, and Orchestrator was added the same way for the same reason.

**Why not a flag on an Agentic issue.** Every branch would have to check both `kind`
and the flag, and the compiler couldn't find the ones that forgot. With a new variant,
every exhaustive `match` fails to compile until someone decides what Cloud does there.

**Kind changes are locked in v1.** You can't turn a card into Cloud from the dialog or
`bork issue update --kind`, and a Cloud card can't become anything else. That removes
a whole class of "what happens to the remote agent" cases from the trial.

**Persisted vs live.** `state.json` holds only durable facts: ids, URL, repo/ref,
branch, the last *terminal* run status, archived/gone. In-flight run status, the
`result` text and poll errors live in `LiveState.cloud_runs: HashMap<String,
CloudRunInfo>`, keyed by agent id. `merge_issue_fields` merges `cloud` file-wins when
memory's copy is `None` or not yet created (the CLI creates it while the TUI is open).

### 2. Status mapping

The agent axis (`ACTIVE | ARCHIVED`) only says whether the agent is archived, so the
run decides everything else. `resolved_agent_status` resolves Cloud issues from
`cloud_runs` and `issue.cloud`, and never looks at tmux.

| Cloud state | bork `AgentStatus` | Activity text |
|---|---|---|
| `created == false` (POST in flight or failed) | `Busy`, or `Error` if the last attempt failed | "submitting" / the error |
| flag off or no key, but Cloud issues exist | `Stopped` | "cloud agents disabled" |
| `gone` (404) | `Stopped` | "deleted in Cursor" |
| agent `ARCHIVED` | `Stopped` | "archived" |
| run `CREATING` | `Busy` | "starting VM" |
| run `RUNNING` | `Busy` | "running" |
| run `FINISHED` | `Idle` | none |
| run `ERROR` | `Error` | the error message, redacted |
| run `CANCELLED` | `Idle` | "cancelled" |
| run `EXPIRED` | `Stopped` | "expired" |
| unknown status | `Idle` | the raw status string |

- `FINISHED` → `Idle`, because locally `Idle` means "done, your turn".
- `CANCELLED` → `Idle`, because the agent is still `ACTIVE` and could take a follow-up.
  `EXPIRED` → `Stopped` until someone confirms what an expired run allows (open
  question 3). Both get checked during the test run.
- Unknown statuses → `Idle`, not `Error`. The API is beta, and a new status shouldn't
  turn cards red or break deserialization.

**Unreachable:** `WaitingInput`, `WaitingPermission` and `WaitingApproval`. Cloud runs
have no approval gates, so the moment you need to act on is `FINISHED`, which is
`Idle`. This matters less than it did for the local Cursor harness, where losing
`WaitingPermission` could hide an agent stuck on a prompt. Here there is no such
blocked state to miss.

Poll failures (offline, 401, 5xx) don't touch card status. Cards keep their last
status, and the error shows in the status bar, the same way `github_error` does.
`Error` on a card always means the *run* failed.

### 3. Polling

**Polling, not SSE.** Polling fits bork's `std::thread` + `mpsc` model and answers
v1's only questions: is it done, and where's the PR.

One worker, `spawn_cloud_worker`, in `SharedWorkers`. It's app-wide rather than per
project because the key belongs to an account, and an app-wide worker also covers
projects that aren't on screen. It's spawned only when the flag is on and the key
resolves. It reads the key once at start (so `logout` takes effect on the next restart,
which is fine for a trial).

- **Targets**: `Arc<Mutex<Vec<CloudTarget { project_id, agent_id, run_id }>>>`,
  rebuilt by the main loop the way `linked_prs` is (`main.rs:1774`), from Cloud issues
  that are `created`, not `archived` and not `gone`.
- **Each cycle, per target:** `GET /v1/agents/{id}` for `status`, `latestRunId` and
  `url`. Then, if `latestRunId` has changed or its run isn't terminal yet,
  `GET /v1/agents/{id}/runs/{runId}`. A 404 sets `gone`. A follow-up sent from the
  Cursor web page shows up as a new `latestRunId`, so bork follows it without
  sending follow-ups itself.
- **Fixed 30s interval.** No requests when there are no targets. It honours
  `poll_suspended`, takes a wake channel (so `issue start` and `cancel` trigger an
  immediate poll), and skips a cycle on `429`, waiting out `Retry-After`. One active
  agent costs about 4 requests a minute.
- **Results** go back as `CloudPollResult { runs, durable_updates, error }`. The main
  loop applies `durable_updates` (new run id, branch, PR link, archived, gone) to issues
  and swaps `runs` into `live.cloud_runs`.

The fetch function is passed in as a closure, the way `github_poll::poll` does it, so
`cloud::poll` is unit tested against canned JSON from the OpenAPI spec with no network.

### 4. Enter opens the Cursor web page

On a Cloud card, Enter and Shift+Enter call `browser::open_url(cloud.url)`. If the URL
isn't known yet, they show "cloud agent is still being created". `o` opens the PR
through the existing path once it's linked.

That page is the only place you can watch the agent and talk to it, which makes it
the honest equivalent of attaching. The other options:

- **Render the conversation in a bork pane:** v1 of the API has no history endpoint,
  only the SSE stream inside its retention window. That would be SSE plus a new widget.
- **Open the PR:** there's no PR until the run finishes, and `o` already does this.
- **Do nothing:** Enter is the main action on a card, so a dead key reads as a bug.

### 5. The API key: macOS keychain only

`bork cloud login` runs `security add-generic-password -U -s bork.cursor-api-key -a
"$USER" -w`, with `-w` last and the terminal inherited. That way `security` prompts
for the key itself, and **the key never passes through bork's memory, argv or stdin
while it's being stored**. It then reads the key back and calls `GET /v1/me` to check
it, printing the key's name and owner email. `bork cloud logout` deletes the item.
Reading the key is `security find-generic-password -s bork.cursor-api-key -w`.

**Why not `CURSOR_API_KEY` in v1.** Anything exported in the shell that starts bork
ends up in the tmux server's environment, so every local agent pane can read it. The
local `cursor-agent` CLI also reads `CURSOR_API_KEY`, which may switch the *local*
harness over to API-key auth and API pricing (I haven't verified that). In other words,
the env var would leak the cloud trial into the local Cursor harness, which breaks the
isolation this scope exists for. **Why not `config.toml`**: `bork config list` prints
values, global config often lives in a dotfiles repo, and `AppConfig` is `Serialize`.
**Why not Linux**: one keychain backend is enough for a trial. On Linux, cloud stays
off, with a clear message.

**How we guarantee it never leaks:**

1. **A newtype `ApiKey(String)`** with no `Serialize`/`Deserialize`, a `Debug`/`Display`
   that prints `ApiKey(***)`, and no `Deref`/`AsRef<str>`. The only way to get the
   string out is `pub(super) fn expose(&self)` inside `src/cloud/http.rs`.
2. **The key is never stored in `AppConfig`, `Issue`, `LiveState` or `App`.** It lives
   only inside the cloud worker thread and the CLI process for one command.
3. **It never goes in argv.** HTTP is `curl -K -`, with the key on stdin as a config line
   (`user = "<key>:"`). `ps` shows only `curl -K - ...`.
4. **The cloud path never goes near the tmux launcher.** No `agent::launch`, no prompt
   file, no `export`. bork never sends `envVars` or `mcpServers`, so nothing local reaches
   the VM.
5. **Error text is redacted** (the key and its base64 Basic form become `***`) before
   it becomes a status-bar message, a `CloudPollResult.error` or CLI stderr.
6. **Tests.** A sentinel key is run through create and poll against a fake fetch
   closure. Then `state.json`, `AppConfig` and the debug-inspector JSON are serialized,
   and the test asserts the sentinel isn't in any of them. A second test feeds in an API
   error that echoes the key and checks the output is redacted.

**HTTP client: `curl` as a subprocess, no new crate.** bork has no HTTP client
(`Cargo.toml` has none). Linear goes through the `linear` CLI and GitHub through `gh`,
so `curl` matches that pattern. Each call is `curl -sS --proto =https --max-time 20
-K -`, with the URL, method, headers, key and body all in the stdin config (the body as
an escaped `data-binary = "..."` string, so there's no temp file). The status code
comes back via `-w '\n%{http_code}'`. The alternative, `ureq`, pulls in
rustls/ring/webpki-roots, which is a big jump for a crate with eight dependencies. It
only becomes worth it if SSE gets scheduled.

### 6. PR attach through `github_pr_links`

When a run carries `git.branches[]`:

1. Take the entry whose `repoUrl` matches the project's origin, normalised to
   `owner/name`: strip `.git`, accept ssh or https, compare case-insensitively. This
   reuses `RepoIdentity` parsing from `src/external/github.rs`. Ignore entries for any
   other repo.
2. Store `branch` in `cloud.branch` (display only in v1).
3. If `prUrl` parses as `https://github.com/{owner}/{name}/pull/{n}` for this repo, push
   `LinkedGithubPr { number: n, imported: false, import_source: None }` unless it's
   already there.

From there the existing PR poller handles status, review state, merge and `o`, with
no parallel field. `autoCreatePR` is always true in v1, so there's no need to find PRs
by branch. Cards are not moved to Code Review automatically, which matches how local
issues behave.

### 7. CLI surface

```
bork cloud login
bork cloud logout
bork issue start "Title" --cloud --prompt "..." [--base <ref>] [--project ..] [--link ..] [--yes]
bork issue cancel <id>
```

`--cloud` (not `--agent cursor-cloud`) because cloud is a kind of issue, not an agent
binary (section 1). Flags under `--cloud`:

| Flag | Behaviour |
|---|---|
| `--base` | kept, maps to `repos[0].startingRef` (default: the repo's default branch) |
| `--project`, `--link` | unchanged |
| `--no-worktree` | accepted, does nothing (it's simply true) |
| `--slug` | rejected: "Cursor names cloud branches itself" |
| `--agent`, `--mode` | rejected: v1 runs Cursor in agent mode only |

**Spend guards:**

- **Confirmation.** Before the POST, the CLI prints the resolved scope (project, repo
  URL, ref, the full prompt text that will be sent, "billed at Cursor API pricing") and
  asks `[y/N]`. If stdin isn't a TTY it requires `--yes`.
- **No agent-started runs.** If `BORK_ISSUE_ID` is set (bork exports it into every agent
  pane, `agent/mod.rs:329`), `--cloud` refuses, with no override in v1. That stops an
  orchestrator from fanning out paid runs.
- **Flag off → refuse** with "cloud agents are off; set `cloud_agents = true`".

**Prompt sent:** the issue title and `--prompt`, nothing else. Not `default_prompt`
(it refers to `main/`, `bork issue start` and tmux, none of which exist on the VM), not
Linear data, not PR links. `name` is `"{issue-id}: {title}"` cut to 100 chars.

**Idempotent create** (`cloud::ops::start`), so a crash or retry can't bill twice:

1. Mint `agent_id = "bc-" + uuid4` (16 bytes from `/dev/urandom`, no `uuid` crate) and
   **save it to `state.json` with `created: false` before the POST**.
2. `POST /v1/agents` with that `agentId`.
3. On `201`, set `created`, `run_id` and `url`. On `409 agent_id_conflict`, the earlier
   attempt landed, so `GET` the agent and set `created`. On any other error, leave
   `created: false` and print the error. Running `bork issue start` again on the same
   card isn't possible, but `bork issue cancel`/`delete` handle a not-yet-created agent
   by first doing a `GET` to find out whether it exists.

### 8. Lifecycle

| bork action | Cursor call | Notes |
|---|---|---|
| `bork issue cancel <id>` | `POST .../runs/{runId}/cancel` | `409 run_not_cancellable` prints "run already finished" and exits 0 |
| `bork issue archive <id>` | cancel if a run is active, then `POST /v1/agents/{id}/archive` | same meaning as for local issues: stop the work, keep history, move to Done |
| `bork issue delete <id>` (and TUI delete) | cancel if active, then **archive**. **Never `DELETE`** | see below |
| move to Done | nothing | same as local |
| archived or deleted in Cursor | none, the poller notices | card shows `Stopped`, "archived" / "deleted in Cursor", and polling stops |

**Deleting a bork issue archives the Cursor agent. It doesn't delete it.** `DELETE`
is permanent and loses the conversation, and nothing in bork needs that. Archive is
reversible in Cursor and leaves the branch and PR alone. The confirm text says so:
"Deletes bork issue X. The Cursor agent will be cancelled and archived (you can
restore it in Cursor). Its branch and PR are not touched."

If a run is active and the cancel fails (offline, 5xx), **the delete is refused**,
because deleting locally would leave a billed run with nothing pointing at it.
`--force` deletes locally anyway and prints the agent URL so you can stop it in Cursor.
If only the archive fails, bork deletes locally and warns, since an idle agent costs
nothing. With the flag off or no key, deleting a Cloud issue deletes locally and prints
the agent URL.

## Test plan

**Without spending anything** (CI, every PR):

- `cloud::poll`, `cloud::ops` and the status mapping against canned JSON taken from the
  OpenAPI spec, through the fake fetch closure: every run status, a `latestRunId`
  change, 404 → gone, 409 on create, 409 on cancel, 429, and a PR URL for another repo
  (ignored).
- The key-leak and redaction tests from section 5.
- `curl` config escaping: a round-trip test on prompts with quotes, backslashes,
  newlines and non-ASCII.
- Repo URL normalisation: ssh, https, `.git`, mixed case.
- The isolation check: the existing suite passes untouched, plus one new test that loads
  a pre-cloud `state.json` and checks it round-trips byte-for-byte.

**One paid manual run** (needs your approval first, open question 1):

1. A scratch bork project pointing at a throwaway GitHub repo with Cursor's GitHub App
   installed. `cloud_agents = true` in that project's `.bork/config.toml` only.
2. `bork cloud login`, then `bork issue start "readme line" --cloud --prompt "Append one
   line to README.md"`.
3. Check: the card goes `Busy` → `Idle`; Enter opens the Cursor page; the PR shows up in
   `github_pr_links` and `o` opens it; `bork issue archive` archives the agent in Cursor.
4. Check the failure paths on the same agent where possible: `cancel` on a finished
   run, `delete` while offline. Note what `CANCELLED`/`EXPIRED` allow (open question 3).
5. While it runs, confirm a local Cursor (or Claude) issue in another project behaves
   exactly as before.

## PR breakdown (dependency order)

```
PR1 ─┐
     ├─> PR3 ─> PR4 ─> PR5
PR2 ─┘
```

Each PR keeps the flag-off behaviour identical and leaves existing tests untouched.

1. **Model and gating** (no network). `IssueKind::Cloud`, `CloudAgent`, `Issue.cloud`,
   the `cloud_agents` flag, locked kind changes, `merge_issue_fields`,
   `LiveState.cloud_runs`, the Cloud branch in `resolved_agent_status` (reads an empty
   map for now), the card badge, the Enter branch, the `x` message, and the
   backward-compatible state round-trip test.
2. **Key and HTTP** (parallel with PR1). `src/cloud/{auth,http}.rs`: `ApiKey`, keychain
   read/write, `redact`, the `curl -K -` client, error envelope parsing,
   `bork cloud login|logout` (`GET /v1/me` only). Leak tests.
3. **Create.** `cloud::ops::start` with the idempotent `agentId` flow, repo URL
   normalisation, `bork issue start --cloud` with flag validation, the confirmation and
   the `BORK_ISSUE_ID` guard. **The first PR that can spend money**, and where the paid
   test run happens.
4. **Poller and PR attach.** `spawn_cloud_worker`, targets plumbing, the status mapping
   wired to real data, the poll error in the status bar, `git.branches[]` →
   `github_pr_links`.
5. **Stop and docs.** `bork issue cancel`, cloud handling in `archive`/`delete`
   (refuse-on-failed-cancel, `--force`), archived/gone from the poller. A short
   "Experimental: Cursor cloud agents" section in README (setup, what's sent, the spend
   guards) and a Cloud entry in CLAUDE.md's Issue Kinds.

## Open questions (need a human decision)

1. **Approval for one paid test run** during PR3 (section "Test plan"). Nothing has been
   checked against a live response yet, and I haven't made any calls.
2. **Security sign-off.** Repo code gets cloned onto Cursor's VMs through Cursor's GitHub
   App, and prompt text goes to Cursor. Is that allowed for your organisation's repos
   under its Cursor agreement, which key type (user vs service account) is allowed, and
   does privacy mode apply to cloud agents? That's a call for whoever owns security
   where bork is used. For the trial, a throwaway repo with no real code sidesteps it.
3. **`CANCELLED`/`EXPIRED` semantics**: can either take a follow-up? This only changes
   one row of the mapping table, and can be answered during the paid run.

## Risks

- **The API is beta.** Response types use `#[serde(default)]`, and unknown enum values
  deserialize into `Unknown(String)`, so a change degrades one card instead of the
  poller. The docs overview and the spec already disagree on `IDLE`.
- **Accidental spend.** Covered by: off by default, CLI-only create with a confirmation
  showing the exact prompt, idempotent `agentId`, no bulk start, no agent-started runs,
  and delete refusing to orphan an active run.
- **Orphaned agents** if `.bork/` is lost. They're still visible and stoppable in
  Cursor's web UI.
- **Key exposure on the machine.** Any process running as you, including local agents,
  can read a keychain item created with `security`'s default access. The keychain keeps
  the key out of files, dotfiles and env, but it isn't a sandbox. Restricting the item
  to the bork binary (`-T`) would break on every rebuild.
- **Unknown rate limits.** About 4 requests a minute per active agent, honouring
  `Retry-After`.
- **Nothing polls while the TUI is closed.** Cards catch up on next start.
- **Repo URL matching.** If normalisation gets it wrong, the PR doesn't attach (the
  agent page still shows it). Covered by unit tests.

## Later (designed, not in v1)

In rough order of value. Each item is a separate PR after the trial.

- **TUI create and cancel.** A kind option shown only when the flag and key are present,
  a confirm dialog with the same resolved scope as the CLI, and `x` → cancel with a
  confirm.
- **Follow-ups from bork.** `bork issue followup <id> --prompt` → `POST /runs`. This would
  let orchestrators drive cloud agents, alongside an opt-in `cloud_allow_from_agents`.
- **Options**: `--mode plan` (and mapping a finished plan-mode run to `WaitingApproval`),
  `--model`, `--no-pr`, a `cloud_prompt` preamble.
- **Better polling**: one `GET /v1/agents?limit=100` per cycle instead of one per agent,
  15s while running and 60s otherwise, and sidebar activity markers from `cloud_runs`.
- **A staleness marker** on cards when the last good poll is older than 3× the interval.
- **Find a PR by branch**: have `branch_for` return `cloud.branch` so the PR poller picks
  up PRs opened by hand (needed once `--no-pr` exists).
- **Pull down**: check out `cloud.branch` into a worktree and continue with a local
  agent. Needs the kind-lock rules revisited.
- **SSE**: live `tool_call` activity text and a read-only conversation pane. One
  `curl -N` child per running run, resumed with `Last-Event-ID`. Worth it together
  with the pane, not on its own.
- **Linux** (`secret-tool`) and **`CURSOR_API_KEY`** as an override, with the tmux-env
  caveat documented.
- **Import agents** bork didn't create, **multi-repo**, **`repos[].prUrl`** (send a stack
  member to a cloud agent), **artifacts**, **environments/secrets**, **usage/cost display**.
- **Never planned:** webhooks (they'd need an inbound listener on a dev machine), and
  `DELETE` from bork without a separate, type-the-id-back command.

## Rejected alternatives

- **`AgentKind::CursorCloud`**: pulls cloud into the provider registry and tmux launch
  path, and agent switching would orphan billed agents.
- **`runs_in_cloud: bool` on Agentic issues**: two checks everywhere, and the compiler
  can't find the ones that were missed.
- **Fake status files for cloud issues**: "status file but no tmux session" already reads
  as `Stopped`, so it needs the same branch anyway.
- **A per-project cloud worker**: the key belongs to an account, and off-screen projects
  wouldn't be polled.
- **SSE in v1**: long-lived processes and reconnect logic for a question polling
  already answers.
- **Key in `config.toml` or env in v1**: printed by `bork config list` and committed to
  dotfiles repos (config), or leaked into tmux panes and the local Cursor harness (env).
- **`ureq`/`reqwest`**: a large dependency tree when `curl` works. `reqwest` also needs
  tokio.
- **Delete issue → `DELETE` agent**: irreversible, and nothing needs it.

## Security posture

bork would hold a key for a third-party service that can spend money and run code
against a repo, and it would make outbound HTTPS calls to `api.cursor.com` from a
developer machine. Here's exactly what leaves the machine, and when.

**Nothing at all** unless `cloud_agents = true`, a key is in the keychain, *and* you've
run a command or have a Cloud issue that's still active. With the flag off, bork makes
no calls to Cursor.

| When | Request | What's sent |
|---|---|---|
| `bork cloud login` | `GET /v1/me` | the key |
| You confirm `issue start --cloud` | `POST /v1/agents` | the key, `agentId`, `name` (`issue-id: title`), `prompt.text` (title + `--prompt`), `repos[0].url` (origin URL), `startingRef`, `mode: "agent"`, `autoCreatePR: true` |
| Every 30s while Cloud issues are active and the TUI is open | `GET /v1/agents/{id}`, `GET .../runs/{runId}` | the key, agent and run ids in the URL |
| `cancel` / `archive` / `delete` | `POST .../cancel`, `POST .../archive` | the key, agent and run ids |

**Never sent by bork:** local files, diffs, worktree contents, git credentials,
environment variables, `envVars`/`mcpServers`/secrets, `default_prompt`, Linear data,
other issues, status files. bork uploads no code. **Cursor clones the repo itself**
through its GitHub App, so whether repo code may sit on Cursor's VMs depends on that
app's install and permissions, not on bork (open question 2).

**What comes back and is kept:** agent and run ids, the web URL, the branch name and
the PR number (in `state.json`). The run's `result` text (the agent's final reply,
which can quote code) is kept **in memory only**.

**Sensitive data in prompts.** The prompt is the one free-text field that leaves the
machine. If customer or client data gets pasted into a cloud prompt, it goes to Cursor.
That's why the confirmation prints the full prompt text before sending, and the README
will say not to put customer or client data in cloud prompts. Whether that needs a
technical control on top is part of open question 2.

**Trust boundary.** Outbound HTTPS only, to one fixed host (`--proto =https`, base URL
not configurable). No webhooks, so no listening port. bork never sets
`workOnCurrentBranch`, so the agent only ever pushes to a fresh `cursor/...` branch and
can't write to `main` through bork. Branch protection on GitHub is still the real
safeguard.

**Key handling.** Stored in the macOS keychain and entered at `security`'s own prompt.
It's held in memory only inside the worker thread or the one CLI command, wrapped in a
type that can't be serialized or printed, passed to `curl` on stdin and never in argv,
and redacted from every error string. It's never written to `state.json`,
`config.toml`, a tmux pane, a prompt file, the process environment or the VM. Tests
check serialized state, config and debug output against a sentinel key.
