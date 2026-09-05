# CLI & MCP Surface

> Normative reference: [spec §8](../spec.md#8-interfaces). Related briefs:
> [context packets](context-packets.md),
> [local work system](local-work-system.md),
> [local tasks & reports](local-tasks-and-reports.md), and
> [behavioral control plane](behavioral-control-plane.md).

One core library owns classification, object storage, scope authorization,
task binding, deltas, and context-packet construction. The CLI and MCP server
are thin faces over it; transport code does not redefine memory policy. The
agent sees thirteen words; every host and operator control lives under
[Host integration](#host-integration).

## Using Engram as an agent

Engram tracks the work of this repository. You use thirteen words; everything
else is the host's business. The host sets `ENGRAM_HOME` and normally injects
`ENGRAM_ACTOR_ID` plus `ENGRAM_SESSION_ID`; it may also set the optional
`ENGRAM_ACTOR_CONTEXT` to bounded free text such as
`model=opus-4.1;reasoning=high`. You type only the word. Actor context is
attribution, not a principal: assignment, `--mine`, handoff, and claim/session
authority continue to use the unchanged actor and session ids. Context never
refuses the session: Engram replaces each unsafe-control run with one space,
trims the value, and cuts it at a UTF-8 boundary to 256 bytes; altered input
receives an explicit `actor_context:normalized` provenance marker, and an
empty result is absent. It is excluded from retry/idempotency identity, so a
replay retains the original operation's attribution instead of duplicating or
refusing it.
A local shell that omits either principal value remains usable: actor derives
from the first nonblank conventional OS-user environment variable and session
defaults to one stable id for that `engram` process. The actor derivation is
asserted context, not an authenticated OS identity; if no conventional user
variable exists, Engram uses a synthetic process actor instead of refusing.
Durable actor provenance distinguishes `defaulted:os_user_environment`,
`defaulted:process_actor`, and `defaulted:process_session`. Explicit actor and
session ids are recorded verbatim. Because separate shell invocations are
separate processes, multi-command ambient workflows still need a host-injected
stable session id. The `local-process-` prefix is reserved for generated
process-default work sessions; a `local-process-v1-*` id may be reused for
seven days, after which the caller must omit `--session-id` to receive a fresh
process default. Every defaulted-session invocation prints its generated id and
that exact reuse instruction. A successful mutating word with `--json` also
returns that id as top-level `effective_session_id`; read receipts, explicitly
bound CLI or MCP receipts, and the host-only `work core` protocol retain their
existing shape.
`next` can stage a delivery cursor, but remains a read receipt by this
contract: compact `next` relies on the stderr notice, while verbose `next`
already returns its session object.

```bash
engram work next [--verbose]      # what is ready, what you hold, what others changed
engram work ls [--search TEXT] [--blocked] [--mine] [--label L] [--all] [--verbose]
engram work show REF              # one item: outcome, acceptance, holder, blockers, reminders
engram work add "Title" [--outcome "..."] [--accept "criterion"]... [--under REF [--optional]] [--priority 0-4] [--kind KIND] [--label L]
engram work claim REF [--ttl SECONDS] [--recover "why"]   # same holder renews; --recover is for another prior holder
engram work update REF [--release | --blocked "why" | --unblock | --cancel "why" | --after OTHER | --drop-after OTHER | --waive CHILD --reason "why" | --supersede-with NEW --reason "why" | --assignee A | --priority N | --defer DATE | --title "..." | --kind KIND | --label L | --unlabel L]
engram work gate NAME [--work-ref REF] [--failed FAILURE]... [--ref opaque-reference]
engram work note [REF] "What you found or decided" [--ref path-or-url]
engram work done ["What was delivered"]
engram work handoff REF --to ACTOR | --accept | --cancel "why"
engram work remember "Project note" [--key KEY]
engram work memories [QUERY] | engram work memories --after KEY | engram work memories KEY --full
engram work forget KEY
```

Add `--json` to any word for its structured receipt. Agent reads are short by
default in text, JSON, and MCP: `next` and `ls` return only navigation rows
and one-line changes, while `show REF` returns one safe detail view. Structured
`show` keeps short refs, planning state, holder words, relations, blocker and
note summaries with their evidence kind, meaningful history, a superseded
item's successor short ref, and allowed actions. Its exact note total and
latest note are independent of the bounded evidence page; latest means the
highest dense run-feed position for execution evidence, or their shared
root-feed position when non-holder observations are present. Evidence
timestamps are asserted metadata, never ordering authority. Observations fill
spare evidence-page slots without displacing selected execution evidence.
The latest note is emitted last in `notes`; on a full page, it replaces the
least-priority selected note.
`notes_omitted` is the exact remainder after all fitting, while
`evidence_count_limit` reports its count-limit share. Open or proposed children
precede terminal children inside the bounded relation page,
so terminal history cannot hide unfinished work while page capacity remains.
Text prints `(+N more)` and structured output carries the exact
`children_omitted` total; typed count omissions distinguish unfinished from
terminal children that did not fit. Actor and session references are relative
words such as `you`, `another actor`, and `another session`; when present,
bounded actor context is shown parenthetically
(`you (model=opus-4.1;reasoning=high)`) on note and
history attribution. Raw actor/session identifiers are not part of this view.
It also omits canonical UUIDs and hashes, revision and fence
counters, and host-only run, claim, control-binding, obligation-page, and
memory-version fields. Humans and hosts that need the rich projection use
host-only `work core focus`; full list projections remain available through
`next --verbose` and `ls --verbose` (or the equivalent MCP arguments). Compact
rows retain up to 80 UTF-8 bytes of title, omit redundant lifecycle and blocked
fields, cap labels, and report `labels_omitted`. When fitting an oversized
advisory response, `next` sheds labels from the least-important navigation rows
before dropping rows; `ls` does not shed labels. Compact `next` uses the same
12 KiB agent-response ceiling as its core view, so the default limit of 20
remains meaningful. Section removal is recorded in explicit `omissions`
instead of failing.

Rules that matter:

- `add` needs only a title. Outcome and acceptance criteria are welcome; they
  are what `done` is checked against.
- Claim before execution. `claim REF --ttl SECONDS` renews your live claim
  with the same identity and fence; expiry becomes the later of its existing
  expiry and now plus the requested TTL (one hour by default).
  Open-work `gate` and `done` require the holder. A non-holder may `note`
  open work, including blocked work or a child of a completed parent: this
  produces a marked observation, never execution or completion credit.
  After completion, any project-bound session may use
  `note` or `gate` for a late finding without claiming or reopening the item;
  the existing seal stays frozen.
- `update --kind`, repeatable `--label`, and repeatable `--unlabel` revise
  indexed planning metadata through the existing audited planning path;
  unclaimed planning updates remain allowed.
- `note` is for decisions, findings, and evidence pointers. A holder note
  feeds peers, handoff, and the final report. A non-holder observation feeds
  project/root peers without a checkpoint, claim renewal, or run credit.
  Its immediate receipt marks `non_holder: true` and says
  `(observation, no run credit)`. A late note feeds peers but remains outside
  the frozen seal; never repeat either elsewhere.
- `done` completes the item you hold. If something is still owed, the answer
  is one sentence saying what and a command that resolves it. Do it and run
  `done` again.
- Every answer ends with `reminders` (what is owed, in words) and `next`
  (commands you can run now). Nothing asks you to copy hashes, fences, or
  idempotency keys; if you see one, it is a bug. Safe project-memory keys are
  intentional navigation tokens for `memories` and `forget`. JSON retains the
  complete command list; the text renderer shows at most four and prints
  `(+N more)` when it omits any.
- With a host-injected or explicitly reused stable session, a lost-response
  retry of the same command replays, except `claim` renews a live claim again
  and a late `gate` on completed-by-record
  restored work: every call appends an observation, so inspect `show` before
  repeating an uncertain call. If a shell used the process default and
  lost the entire notice too, inspect with `ls`/`show` before repeating a
  mutation; exact replay cannot cross processes without the printed session.
- A failed gate is work, not a stop — `gate NAME --failed FAILURE`
  records the failures as evidence on held open work, or as a late finding on
  completed work selected by focus or `--work-ref`, nothing more. With no
  focus, use `gate NAME --work-ref REF`; no last-completed item is inferred.
  Gate names follow the repository's
  [quality gates](../development.md#quality-gates).
  Classification stays your judgment: a product defect gets a required
  child through the ordinary `add … --accept "<test> passes" --kind bug
  --label gate`, and test or environment findings go into the durable note.
  `gate NAME` alone always records a pass. Every failure supplies at least one
  bounded `--failed` label; when no test id exists, use the check command or
  check name. A consecutive identical result replays;
  the same result after an intervening different result records a fresh gate
  transition.
- `remember` stores a retrievable project note — an attributed
  observation, never a rule or a decision record, kept in full until an
  explicit `forget`. `next` only signals how many notes exist and whether
  any changed; `memories` is the source of truth.

The same thirteen words are MCP tools (`next`, `ls`, `show`, `add`, `claim`,
`update`, `gate`, `note`, `done`, `handoff`, `remember`, `memories`, and
`forget`) with the same flat arguments, plus `search` — fourteen tools,
with no new work-core operation — `gate` wraps the existing evidence path, and
`remember`/`memories`/`forget` are a thin project-memory surface outside the
six-operation work core (no focus mutation, no claim renewal). Reads require
the cooperative asserted project binding. `remember` and `forget` validate the
same non-empty actor/session binding inside the write transaction;
`memory_binding_invalid` means that binding is absent or inconsistent. The normative
spec, tool count, and every agent instruction file move atomically with the
code.

If you know Beads, the words map one to one:

| Beads | Engram |
| --- | --- |
| `bd ready` | `engram work next` |
| `bd list` | `engram work ls` |
| `bd show <id>` | `engram work show REF` |
| `bd create --title=T [--parent=<id>]` | `engram work add "T" [--under REF]` |
| `bd update <id> --claim` | `engram work claim REF` |
| `bd update <id> --status=blocked` | `engram work update REF --blocked "why"` |
| `bd update <id> --notes=N` | `engram work note "N"` |
| `bd close <id>` | `engram work done ["what was delivered"]` |

`done` differs from `bd close` in one way: it is checked against the item's
acceptance and against anything the host recorded as owed, and it tells you
what is missing instead of closing anyway.

## Host integration

The [host checklist](../host-checklist.md) is the authoritative base-tier
recipe, including the Claude Code hooks form; this section keeps the
two-tier contract and the control-plane details. Integration has two tiers,
and a host picks per project:

- **Base** — the tracker for agents: the `engram mcp` server injected into
  every session of a declared project (the repository declares with its
  tracked `.engram-project`; the host supplies the stable project identity
  and asserted actor/session binding to the MCP child), plus a
  start-of-session nudge that runs `engram work next` on session start and
  after compaction and injects its text as context. Any host that can register
  an MCP server and run a hook can do this. It carried every benefit measured
  so far.
- **Turn-gated** (`turn_gated`, the optional tier of the checklist and of
  [shipped today](../shipped.md)) — behavioral control: the host-private
  JSON-lines turn channel below (bind, evaluate, begin, checkpoint), dispatch
  withheld until a turn is granted, resource leases for writers, obligations
  before completion. Opt-in per project, off by default, for hosts that need
  enforcement rather than coordination.

The agent-facing MCP interface is **advisory**. A model can omit an MCP call,
so that surface alone cannot enforce synchronization, ownership, or
finalization. The separate host-private JSON-lines channel implements the turn
lifecycle; grants are not exposed as tools with which the agent can authorize
itself. Everything below is the host's and operator's business: the agent
words above never require it.

### Host and operator CLI

```bash
export ENGRAM_HOME=/absolute/host-local/path
engram init --required-assurance advisory \
  --authorized-by host-operator \
  --reason "bootstrap this project for an advisory host"
engram doctor

# When ordinary open refuses a corrupt control-policy chain, inspect only that
# immutable family. This mode is read-only, enables no service/mutation API,
# and never selects or rewrites a policy head.
engram doctor --recover-policy [--json]

# Explicitly rebuild only declared indexes, triggers, and FTS projections.
# Ordinary open never performs this repair implicitly.
engram doctor --repair-projections [--json]

# Host/operator boundary: activate a new immutable policy version. The
# optional expected hash is the `id=` reported by doctor and prevents a stale
# operator from overwriting a concurrent policy update.
engram control-policy set-required-assurance turn_gated \
  --authorized-by host-operator \
  --reason "enable mandatory host turn mediation" \
  --idempotency-key enable-host-turn-mediation \
  --expected-policy-hash <active-policy-hash>

# Select a bounded typed obligation set. The required environment must already
# be a canonical EnvironmentEvidence hash returned by a host checkpoint.
engram control-policy set-obligation-rule-set \
  --input @obligation-rules.json \
  --authorized-by host-operator \
  --reason "pin the repository test command and environment" \
  --idempotency-key pin-repository-verification \
  --expected-policy-hash <active-policy-hash>

engram mcp \
  --actor-id codex \
  --session-id session-unique-id \
  --actor-context 'model=opus-4.1;reasoning=high' \
  --source-skill engram-repo
engram control \
  --actor-id codex \
  --session-id session-unique-id \
  --source-skill engram-repo

# Host-local loss recovery: a verified full copy of the store, and the way
# back. A backup carries host-private state and private scratch, so it is exactly as
# sensitive as the store; schedule it on the host, never publish it.
engram backup                      # → <home>/backups/<project>/engram-<utc>.db + manifest
engram restore --from <backup-file> [--replace]   # stop other Engram processes first

# Deterministic planning/history disclosure. The default path is
# <home>/snapshots/<project>/graph-<work-cut>-<memory-cut>-<first-12-body-digest>.json.
# --include-restricted deliberately widens restricted project-memory bodies and
# therefore requires a disclosure reason carried in the body and save audit.
engram graph save [--out <snapshot.json> | --stdout] \
  [--include-restricted --reason "<why>"]
engram graph load <snapshot.json> [--dry-run]

# Agent words use the stable project plus asserted actor/session binding.
engram work --actor-id codex --session-id session-unique-id next
engram work --actor-id codex --session-id session-unique-id \
  claim <short-ref>
engram mcp --actor-id codex --session-id session-unique-id

# Host/operator escape hatch: the six-operation JSON protocol from the shell.
engram work --actor-id codex --session-id session-unique-id \
  core focus <short-ref>
```

`graph save` and `graph load` are operator-only CLI surfaces; neither is an MCP
tool or changes the thirteen agent words. Save reads the work graph, native
history, source provenance, and keyed project memories at one transaction cut,
then commits an immutable disclosure-attempt audit before publishing bytes.
The canonical body excludes the exporting build, so its digest remains content
identity across builds with the same runtime-derived format fingerprint. The
default output is owner-only where the platform has file modes, no save may
target an Engram project-store directory, and neither the default path nor
`--out` replaces different bytes. Load requires an empty destination project,
revalidates the fingerprint, canonical body digest, manifest, relations,
history proofs, and memories before one atomic recreation transaction, and
records a separate immutable load audit. `--dry-run` performs the same
validation and reports the landing plan without writing. See the
[work-graph snapshot](work-graph-snapshot.md).

Actor context currently binds only the work/MCP service. The behavioral
control plane keeps its existing actor/session and environment-evidence
attribution contract.

The optional actor context can equivalently arrive through
`ENGRAM_ACTOR_CONTEXT`; it is fixed when the CLI invocation or MCP connection
binds its session. The MCP process retains one `LocalWorkService` and its
lazily opened SQLite connection for its lifetime. All fourteen MCP tools use
that service. A failed operation rolls back before the next call uses the
connection.

`--project-file` defaults to the tracked `.engram-project`. Its stable project
identity resolves to the same opaque SQLite path for every worktree and
session on the host. `doctor` verifies every canonical object plus
hash-bound control record, reports the active immutable policy hash, epoch,
required assurance, selected obligation-rule-set hash, built-in effect
envelope, and live
issued/begun turns, and visibly warns that action gating, organizational
authority mediation, and action-outcome reconciliation are unavailable. V1's
development no-op redactor provides no secret or PII protection.
`engram doctor --json` performs the same checks and keeps those warnings on
stderr while emitting a machine-readable report on stdout. Its `project_id`
and canonical absolute `database` path give a host the stable pair used to key
project-local authority queries. The database path is absolute with symlinks
resolved. On Windows it never exposes the verbatim-path prefix (`\\?\`), and
UNC paths use their ordinary `\\server\share` form. When integrity is
unhealthy and the active control envelope cannot be decoded, the report still
prints with `healthy: false`, `control: null`, and a `control_error`; independent
limitations remain on stderr before the command exits nonzero.

If normal open itself refuses because the active control-policy chain is
corrupt, `engram doctor --recover-policy` uses a separate existing-file,
read-only/query-only path. It verifies the active selector and every projected
policy version together with the canonical authority and selected rule-set
objects, emits a typed finding for each invalid binding, and exits nonzero
while the store remains unusable. `--json` reports
`mode: "control_policy_recovery"` and `mutation_enabled: false`. This mode
cannot start MCP/control/work, issue grants, initialize schema, or repair/select a
policy; its guidance is limited to restoring verified bytes or explicit
operator inspection. Because the connection is read-only, it also cannot run
SQLite crash recovery for an uncheckpointed WAL. If SQLite itself requires
recovery, stop writers and diagnose a byte-consistent verified copy of the
database together with its sidecars; the command fails without changing them.

Missing or malformed declared indexes, triggers, or FTS tables also make
ordinary open fail without DDL. `engram doctor --repair-projections` is the
separate mutating operator path: it first fingerprints the complete exact-current
durable definitions and validates control-policy bindings, recreates every
declared rebuildable object, repopulates FTS from verified durable rows in one
transaction, and then runs full integrity verification. It never recreates
missing durable state or rewrites canonical objects.

On a fresh store, plain `engram init` defaults to `turn_gated`;
`--required-assurance` may instead select `advisory`, `turn_gated`, or
`action_gated` for that first policy and requires `--authorized-by` plus
`--reason`; the resulting epoch-one authority object records that operator
choice as asserted context. Plain `engram init` remains an
idempotent create-or-verify operation and preserves any existing active
policy. Explicitly passing a different bootstrap value for an existing store
fails instead of silently changing policy.
`engram control-policy set-required-assurance` records asserted operator
attribution and a reason,
creates immutable authority and policy objects, atomically advances the active
policy hash and epoch, and supports an optional compare-and-swap hash while
preserving the selected obligation rule set. Its required idempotency key
binds the complete normalized intent and persists the exact receipt in that
same transaction. A retry after restart or an uncertain response returns the
original receipt even though its expected policy hash is now stale; reusing
the key for another intent is a typed conflict.

The sibling operator-only `set-obligation-rule-set` command selects a
validated canonical rule set with the same atomic policy successor,
compare-and-swap, attribution, and replay contract; it is not an MCP tool or
host turn-protocol operation. Its `--input <JSON|@file>` is limited to 64 KiB,
must be UTF-8, rejects unknown fields at every nested V1 object, and passes
through the same typed validator used by storage. `check_fingerprint` and
`required_environment` are exact canonical object hashes, not shell commands
or environment descriptions. Re-supplying the active set under a fresh key
records an exactly replayable `changed=false` receipt. Rollback likewise
re-supplies the desired prior JSON; a rule-set hash alone is never accepted as
activation authority. Reapplying the active assurance under a fresh key also
records an exactly replayable no-op receipt. Issued grants from the prior
epoch fail begin with `policy_epoch_changed` and require one fresh evaluation;
if the new requirement exceeds the host's declaration, that fresh evaluation
instead fails `control_assurance_insufficient` because assurance is checked
before epoch adoption. Already-begun grants remain checkpointable under their
frozen basis. Selecting `action_gated` through either initialization or the
setter prints a warning that no V1 host can bind at that level plus the
`set-required-assurance turn_gated` recovery command. The operator identity is
asserted host context, not authenticated administration.

`engram work` exposes the thirteen agent words; `--json` after any word prints
the structured receipt (the existing shape plus `reminders` and `next`)
instead of text. A successful mutation with a process-defaulted session also
adds top-level `effective_session_id`. `done` exits with status 2 when the typed
`open_work_obligations` refusal says something is still owed. The
six-operation JSON protocol stays reachable for hosts and operators as
`engram work core {next,focus,propose,update,complete,handoff}`, whose
mutation payloads accept an inline JSON object or `@path`; that host/operator
surface retains the core-only explicit delivery acknowledgement, typed evidence
attach, and reopen operations alongside typed forms of the ordinary lifecycle
words. Typed `gate` and atomic `note` are word-only `work_update:gate` and
`work_update:note` suboperations, not variants callers can reach directly
through `work core update`; they still use the same service and storage core as
MCP. Stats/import/export and the remaining broad administrative CLI in the
specification are still planned.

The operator-intended shell command can waive one exact open run obligation
with an attributed reason and retry key:

```bash
engram authority waive-obligation \
  --obligation-id <uuid> \
  --expected-definition <hash> \
  --waived-by host-operator \
  --reason "accepted without the required test" \
  --idempotency-key <retry-key>
```

The equivalent native-host operation is `obligation_waive` on the private
JSON-lines channel. Its request names the routing token, exact obligation and
definition hashes, asserted `waived_by` human, bounded reason,
and idempotency key. The control session must be bound to that obligation's
live run. Policy outcomes are typed as `waiver_not_admitted`,
`obligation_not_open`, or `definition_changed`; transport, token, and
same-key/different-intent faults remain request errors. A committed retry
replays the exact result. The canonical resolution keeps the server-fixed
session actor beside the asserted human attribution, while the receipt omits
the reason.

MCP and `work_update` cannot request a work-obligation waiver, and agent-facing
projections omit its reason. That surface separation is not authentication:
the shell command has no credential or run-binding check, so any local process
with the binary and store access can invoke it. Only the private JSON-lines
operation enforces the live control-session/run binding described above.

### MCP tools

`engram mcp` registers exactly the fourteen agent-facing tools below.

| Tool | Purpose |
| --- | --- |
| `next` | What is ready, what this session holds, and the changes since its previous call |
| `ls` | Open items with flat `search`, `blocked`, `mine`, `all`, and `label` filters |
| `show` | One item in safe agent detail; selects it as focus without claiming |
| `add` | A root from a title, or one child with `under`; `optional` makes that child non-blocking; outcome and acceptance default from the title |
| `claim` | Hold an item; later calls default to it |
| `update` | One `action`: `release`, `blocked`, `unblock`, `revise`, `cancel`, `after`, `drop_after`, `waive`, or `supersede` |
| `gate` | Record one bounded pass/fail observation; completed work accepts it as a late finding without a claim or reopen |
| `note` | Record evidence and checkpoint open work; completed work records only late evidence, both keyless |
| `done` | Complete the held item; an open obligation returns the typed `open_work_obligations` result |
| `search` | `ls` over every lifecycle |
| `handoff` | `offer`, `accept`, or `cancel` the unique checkpoint-coupled handoff |
| `remember` | Store one attributed project episode under a safe permanent key |
| `memories` | List/search compact rows or fully read one exact live key |
| `forget` | Append an attributed terminal tombstone; never erase or reuse the key |

Every agent tool result keeps its structured shape and adds two fields.
`reminders` holds words only, derived by a fixed table from the readiness
`obligations` strings, open `obligation_page` items, active blockers, and the
claim holder. `next` holds literal `engram work …` commands derived by a fixed
table from `allowed_next`: at most one dead-prerequisite `--drop-after`
recovery followed by lifecycle moves in priority order (`handoff --accept`,
`claim`, `note`, `done`, `update --unblock`), with three commands total and
one trailing `show REF`, so no receipt lists more than four. Other planning
edits (`--blocked`, `--release`, `handoff --to`, `add --under`, `--title`,
`--cancel`, `--after`, `--waive`, `--supersede-with`) are not synthesized as
general next commands; their tags stay in `allowed_next` on the structured
receipt. A required-child completion refusal supplies the exact waiver command.
The host-only reopen operation remains structured-only. A holder-only mutation
against completed work instead names `note` as the late-finding path and never
suggests reopening merely to record evidence.
Errors keep their stable code and details and add the same two fields. The
shell prints a one-line receipt followed by `reminders:` and `next:`; `--json`
prints the structured receipt plus `effective_session_id` only for a successful
default-session mutation. Text output never contains a 64-hex hash, fence
number, or idempotency key. `scripts/parity.test.mjs` checks that on a fresh
store and counts `add → claim → done` at three commands and at most three
agent-supplied fields.

`ls --mine` returns items assigned to the actor plus the session's focused
item when this session holds it; claims on other items are visible through
`show`. `add --under` selects the parent and submits one required child
through `work_propose:decompose`; adding `--optional` instead records an
optional child that is shown as such and does not gate parent completion (a
decomposition admits one through 16 children). Either form then focuses that
child exactly as a root `add` focuses the new root. On open work, `note`
records evidence and then checkpoints the run's current evidence set. On
completed work, `note` records only attributed late evidence after the frozen
completion cut; it creates no checkpoint, does not reopen or reseal the run,
and cannot enter the existing `CompletionSeal`. Repeating an identical `note`,
`add`, `claim`, or `done` replays its receipt because the core derives the
idempotency key.

### Work protocol contract

The fourteen tools use the six ambient work operations: `work_next`,
`work_focus`, `work_propose`, `work_update`, `work_complete`, and
`work_handoff`. `remember`, `memories`, and `forget` are a separate thin
project-memory surface, not new work-core operations. Hosts and operators may
call the six work operations directly through `engram work core`; they are not
additional MCP tools. Session binding supplies
project, actor, current work,
and cursors, so update/complete/handoff do not repeatedly shuttle ids. Ambient
state contains no authority token; each mutation rechecks the project, item,
claim, and fence state. `work_next` returns only the selected
`focus`, `ready`, `catalog`, `changes`, and/or `memories` sections; omitting
`sections` selects all five. CLI callers use
`--sections focus,ready,catalog,changes,memories` and
MCP callers pass a string array. Selecting no `changes` section never stages or
advances project delivery, including when a prior page remains pending.
Ready and catalog candidates are filtered and limited by maintained SQLite
projections before their compact item rows are decoded. Assignment and label
filters use NFC plus full Unicode case folding, and catalog text search uses a
trigram index over the short reference, title, outcome, labels, and active
blocker detail. These views remain advisory; lifecycle mutations revalidate
their canonical work-event basis under the write lock.
The `--blocked`/`blocked_only` filter is independent of derived availability:
it returns work with an active blocker or incomplete prerequisite even when
the item is deferred or its lifecycle is closed.
Hash-verified source changes retain dense positions and explicit compact
summaries instead of canonical work snapshots or memory bodies. A change's
`object_hash` identifies the verified canonical source and intentionally is not
a hash of its summary. Restricted
work memory, and work memory outside the session's currently focused verified
root, is replaced by a typed `omission` marker at its original dense position;
the protected body and structured fields never cross the agent boundary. The
largest dense prefix fitting the fixed change budget is staged durably; each
call returns the changes since the session's previous call, and the previous
page counts as delivered when the session asks again. A response lost between
Engram and the agent is therefore not redelivered; the section is advisory and
canonical state stays readable through focus and catalog views. A host that
needs exact delivery acknowledges explicitly by returning the exact
`delivered_through` value and opaque `delivery_token` as `acknowledge_through`
and `acknowledge_token`; both fields are absent when changes were not
delivered, and a guessed pair is refused without disclosure. Concurrent
appends wait for the next page. Every successful work response is at most
12,288 serialized JSON bytes; typed `omissions` report advisory sections
shortened by count or byte budget. A `staged` changes omission instead means
those dense entries remain unconsumed for the next page; it is not a
byte-budget discard. `work_focus` is navigation only and never claims/releases
as a side effect. Its host-only core result returns an exact history count with
a bounded newest-event summary tail, the latest run even after completion, and
a body-free actor-filtered memory index. The `show` word projects that result
into the safe agent detail view described above; event summaries name what
happened instead of repeating only the transition kind and title. A staged
page never blocks a focus change or a mutation; changing focus discards the
un-delivered page and the next call recomputes the same interval under the new
focus.
`work_propose` atomically handles roots and bounded decomposition. `work_update`
carries a typed transition such as claim/release, checkpoint, blocker, cancel,
supersede, deferral, assignment, revision, or prerequisite change. Update and
handoff success responses contain only a compact receipt, one bounded
`obligation_page`, generic readiness `obligations`, and `allowed_next`;
their size does not grow with item history. A successful holder note, update,
evidence, checkpoint, or handoff on open work advances claim expiry to at
least one hour after that mutation without shortening a longer explicit TTL;
successful completion terminalizes it. A completed `note` or `gate` instead
appends attributed evidence without a live claim, claim renewal, checkpoint,
reopen, or reseal. A holder mutation on a lapsed claim is
refused with exactly one next command: `engram work claim <ref>`. Once the work
is ready, that ordinary claim command retakes the same holder's claim, advances
the fence, preserves an active run, and needs
no recovery reason. Every handoff offer expires no later than its source claim.
A different prior holder still requires the explicit recovery path. Each entry
names the exact tool and tagged operation. For example,
`allowed_next: ["work_update:claim(recovery_reason_required)"]` directs the
agent to submit the `work_update` claim variant with an attributed
`recovery_reason`; ordinary
claiming is `work_update:claim`. A successful claim receipt includes
`control_binding { root_execution_id, work_id, run_id, work_revision,
claim_id, claim_fence }`, ready to pass unchanged to host-private
`session_bind`. The same live tuple appears as `focus.control_binding`;
`focus.run` also exposes `root_execution_id` and `work_id`, while
`focus.claim` exposes the claim and fence components. `work_revision` is the
focused work item's revision—the claim receipt's top-level `revision`—not the
claim projection's own revision counter.

`work_complete` can consume evidence/checkpoint state created through explicit
`work_update` calls, or accept `capture { summary, refs }` to record evidence,
checkpoint its exact evidence set, and seal in one model-level call. Completion
is refused while blockers, prerequisites, required child seals or explicit
completion waivers, live handoffs, capture requirements, or run obligations
remain unresolved. Open obligations are not an MCP error envelope.
`work_complete` returns a typed `open_work_obligations` result with the same
`obligation_page` used by `work_focus`, nested `work_next.focus`, and
`work_update`. A completed receipt also returns that page reconstructed from
the exact terminal obligation hashes bound into the seal. Each page is count-
and byte-bounded, reports an explicit `omitted_count`, and carries immutable
obligation/definition identities, the required exact rule-set hash, state,
rule, requirement, trigger, terminal
resolution/evidence when present, and deterministic typed guidance. An open
verification requirement directs the caller to record matching host
verification, checkpoint it, then complete, or request a host/operator waiver.
Trimming retains open obligations before satisfied or waived history and keeps
deterministic trigger/resolution ordering within those state groups. Focus
evidence uses the same actionable-first rule: environments required by visible
open obligations are retained first, and a visible verification summary keeps
its referenced environment summary ahead of it. Count and byte trimming remove
unrelated or dependent evidence before breaking that visible typed closure.
Generic readiness strings remain a separate compatibility field. A successful
completion result is durably replayable under the same idempotency key;
recoverable refusals are recomputed from current state. The pending attempt
retains the original request and work target, so a lost refusal cannot redirect
the same caller key after focus moves. An interrupted
capture-backed completion reuses committed evidence, and reuses an exact
checkpoint while its acknowledged run-feed cut remains current. If the feed
advances, the retry writes a new checkpoint under a cut-derived substep key.
Cut selection and that checkpoint append share one SQLite write transaction.

For a cancelled or superseded required child without a seal or waiver, the
refusal names that lifecycle and returns the runnable agent command
`engram work update PARENT --waive CHILD --reason "why"`. The matching MCP
update uses `action: "waive"`, `child`, and `reason`; both translate into the
existing typed `work_update:waive_required_child` operation. The project-bound
session records the reason-attributed, audited waiver, after which retrying
`done` re-evaluates the current completion barrier.

Recoverable completion refusals add `recovery { cause, item, command }` to the
receipt. `cause` is a tagged value for `open_obligation`,
`required_child_unsealed`, `missing_contribution`, or `missing_acceptance`,
including the exact blocker identity. `item` carries the
affected full id, short ref, title, and lifecycle-backed state. `command` is
deliberately a single next command, and the `done` verb exposes exactly that
one entry in its `next` list. Recovery guidance is not a replayable result: it
is rebuilt from a coherent current snapshot so a retry observes a child,
contribution, obligation, or acceptance barrier that moved. Native `done` and
the fourteen-tool MCP surface return this as a typed refusal receipt. The JSON
core prints the same typed refusal receipt on stdout and exits with status 1;
it does not wrap the refusal in an error envelope. Short-ref ambiguity likewise
returns a stable
`work_reference_ambiguous` error with up to eight ordered candidates, an exact
`more` count, and full-id retry guidance on every JSON front door.

The host-only JSON core schemas for `work_propose`, `work_update`,
`work_complete`, and `work_handoff` use typed discriminated inputs.
Each accepts an optional `work_ref`; the target is resolved and bound inside
the mutation, so a concurrent focus change by the same session cannot redirect
it, and it becomes the ambient focus as a side effect. `idempotency_key` is
optional on every mutating branch: when omitted, the server derives one from
the session, operation, focused work, the item's current claim/handoff basis,
and canonical intent, so an identical repeated call replays its receipt while
nothing about the item changed and is a new attempt once it moved; a supplied
key keeps the explicit contract. Durable attempts bind caller intent separately
from the current focused work/claim/handoff basis. A pending refused completion
may refresh its live claim basis only after the original target binding is
verified; committed successes replay, and an interrupted attempt cannot mutate
a newly focused item. Omitting
`work_complete.acceptance` asserts every current criterion with the note
`accepted by <actor_id> via work done` (or the supplied `note`); omitting
`work_update:checkpoint.evidence` acknowledges every evidence object already on
the live run.

Work search/lifecycle filters, paged catalog results, and item history ship in
the ambient query/focus views. Stats, stale/orphan diagnostics, approval
decisions, import/export, and report publication remain administrative tools
over the same core. Successful model responses are terse; refusals return a
stable code plus a satisfiable remedy. Full durable receipts go to the host. No
replayable control-plane turn/action grant token appears in model-visible MCP
output. Agent-facing work itself has no grant token.

One capture powers peer context and the ordered feed. The host may use a
mailbox as a doorbell, but must not relay full state or make the agent repeat
the same fact into another status ledger.

### Shipped host-private turn channel

`engram control` is a long-lived stdio process. It accepts one JSON object per
line and returns one `{ "status": "ok", "result": ... }` or typed error line.
The runtime session and asserted actor are fixed by process arguments. The
shipped operations are:

| Operation | Durable effect |
| --- | --- |
| `session_bind` | Start/join the compatibility task, optionally bind an exact live `WorkRun` claim, rotate a routing token, reset to `sync_required` |
| `session_status` | Read current phase, cursors, epochs, mediation declaration, optional work binding, revision, `open_grant_id` plus `open_grant_state`, and any safely redeliverable partial recovery grant |
| `lease_acquire` | Atomically grant or defer a normalized resource lease and append its fenced task event |
| `lease_release` | Release a lease held by this session and append its fenced task event |
| `turn_evaluate` | Derive membership/context/head/policy from SQLite and persist a decision plus optional grant |
| `turn_begin` | Recheck freshness and exact delivery token, then consume the issued grant |
| `turn_checkpoint` | Promote tentative delivery, atomically append bound execution observations, complete the grant, and append a canonical control checkpoint event |
| `obligation_waive` | Resolve one exact open obligation on the session's bound run under dedicated human-attributed waiver authority |

The bind response supplies the `routing_token` used on later calls. A granted
turn carries an exact dense task delta under `grant.delivery.delta`. The final
page also carries the bounded context packet under `grant.delivery.context`;
earlier bounded pages set `context` to `null`, `has_more` to `true`, and grant
only an observe-only `recovery` turn. The host must inject the supplied payload
and cite `grant.delivery.page.delivery_token` in `turn_begin` before
dispatching the prompt. Checkpointing a partial page leaves the session
`sync_required`; finite recovery pages drain the backlog before an ordinary
turn can grant. A single canonical task event is size-limited at capture, so a
page always makes progress. Exact retry evidence remains canonical across
process restart, but a newly opened control connection invalidates unbegun
grants and returns the session to `sync_required`; old results never resurrect
authority. The new connection also fences a still-live predecessor, whose next
operation fails with `control_connection_superseded`. A begun grant is not
silently replayed or discarded: `session_status.open_grant_id` identifies the
required checkpoint and `open_grant_state` distinguishes `issued` from
`begun`. A fresh `turn_evaluate` key atomically supersedes an
issued-but-unbegun grant and records an immutable transition bound to the
replacement decision; an already-begun grant instead refuses with
`turn_already_open`. When the begun grant contains an observe-only partial
recovery page, `session_status.recoverable_grant` returns the exact canonical
grant and delivery bytes; the replacement host redelivers that payload and
then checkpoints the already-begun grant. The confirmed cursor does not move
until that checkpoint. Other begun turns expose no replayable prompt because
their outcome may be uncertain. Reusing a key for a different intent fails.

A native local-work bind supplies `work_binding` with
`root_execution_id`, `work_id`, `run_id`, `work_revision`, `claim_id`, and
`claim_fence`. Storage verifies that exact tuple against the session's live
claim and copies it into every turn-grant basis. Ownership means
`claim.holder == session_id` for the MCP actor session that claimed the run;
the asserted `actor_id` is audit context and never substitutes for that holder
check. A malformed, cross-project, or currently peer-owned bind fails as
`work_claim_mismatch`. A tuple that canonical history proves belonged to this
session but whose revision, fence, claim, handoff, run, root execution, or
expiry moved before bind fails as `stale_fence`, telling the adapter to reread
and rebind. The same movement after bind refuses evaluation or begin with
`stale_fence`. Omitting `work_binding` retains the compatibility task-only
channel, but that session cannot append run execution observations.

`turn_checkpoint.observations` accepts at most 64 host facts containing
`observation_id`, `action_fingerprint`, `effect`, `outcome`, and
`source_changed`. An observation may also carry
`source_basis { workspace_id, source_revision }` and `observed_at`.
`source_revision` is the host's fingerprint of the full relevant content,
including committed and dirty bytes. `workspace_id` is retained for audit but
does not participate in anti-stale equality. Engram supplies the authoritative
project, frozen work binding, session, grant, actor, and recording timestamp,
then appends each canonical observation to the project, root-work, and
run-execution feeds in the same transaction as the control checkpoint.

`turn_checkpoint.verification_evidence` accepts at most 16 host-minted checks.
Each entry supplies `producer_observation` as either
`{ kind: "object_hash", object_hash }` or
`{ kind: "observation_id", observation_id }`, plus `check_kind`, optional
`summary`, and bounded `refs`. Storage derives the check fingerprint, outcome,
source/run/session binding, and timestamps from the producer; an unknown
producer returns `verification_producer_not_found`. Up to four
`environment_evidence` entries bind an environment identity to an exact source
basis. The opaque form supplies only a host-produced fingerprint.
The component form adds
`components { toolchain, sandbox?, workspace_id, capability_map_revision }`;
Engram derives the canonical component fingerprint, requires the component
workspace to equal the source workspace, and requires the capability-map
revision to equal the bound session. Component strings are trimmed,
nonempty, limited to 256 bytes, inspected by the configured redactor, and are
asserted host context rather than attestation. Do not place secrets in them.

A verification may cite an environment as
`{ kind: "object_hash", object_hash }` or
`{ kind: "index", index }`, where the index addresses the same request's
ordered environment list. The referenced object must belong to the same run
and source revision. The current built-in test requirement does not require a
particular environment, but its optional link is retained for audit and future
typed policies. Mismatched derived bytes, a missing reference, or a run/source/
session basis mismatch return `environment_fingerprint_mismatch`,
`environment_evidence_not_found`, or `environment_basis_mismatch`.

The receipt returns all three typed hash lists. Begin and checkpoint keys are
each scoped to the exact grant and canonical request intent. An exact retry returns those
hashes without another feed append; changing any ordered list under the same
checkpoint key fails with `control_operation_idempotency_conflict`.

Agent-facing `work_update:evidence` retains its generic form. It also
accepts the attach-only form
`{ kind: "evidence", attach: { evidence: <typed-hash> }, idempotency_key }`.
Attach validates that the hash is verification/environment evidence on the
focused run and does not mint another canonical object or feed entry. Generic
evidence can be cited for context and completion, but cannot satisfy a typed
verification requirement.

Every work-bound observation with `source_changed: true` atomically opens one
built-in test obligation, irrespective of `outcome` and irrespective of
whether `source_basis` is present. A passed typed test satisfies open
obligations only against the newest mutation source revision at the evaluated
run-feed cut. Thus a newest basisless mutation makes the open set waiver-only
until a later basis-bearing mutation plus passed test arrives; that later test
may satisfy both the earlier and newer definitions. `work_focus` exposes the
canonical bounded `obligation_page`, the same field appears inside
`work_next.focus`, and `work_next` deltas use
`obligation_opened`, `obligation_satisfied`, or `obligation_waived` without
leaking host authority.

Every new completion seal declares obligation schema V1 and freezes the exact
definition/resolution hash pairs applicable at its dense pre-seal cut. The
final checkpoint must acknowledge the matching typed verification evidence.
A later `note` or `gate` from any project-bound session is marked in the
evidence actor's existing provenance chain and appended after that cut. It is
visible in `show` notes and peer `next` changes but never changes the frozen
seal or adds a completion barrier.
A new seal also declares environment schema V1 and cites the sorted, distinct
environment-evidence hashes at or before that cut. It refuses more than 64
environment records and never copies the component payload into the seal. A
parent verifies required child seals transitively; every accepted seal carries
the current obligation and environment schema bindings.

An observation effect absent from the frozen grant is a request error with
code `observation_scope_mismatch`. Checkpointing an issued-but-unbegun grant
returns `grant_not_begun` with host transition guidance: bind/recover the
runtime as needed, evaluate a fresh turn, begin that exact grant, and only then
checkpoint it. `grant_scope_mismatch` remains the general frozen-basis mismatch.
Checkpointing an already-begun grant compares its frozen session/grant binding
but deliberately does not recheck claim expiry or live ownership: begin already
consumed authority, and checkpoint records what happened rather than granting a
new action.
This control operation is deliberately named `turn_checkpoint`; the local-work
lifecycle operation `checkpoint_work` remains the separate run-progress and
evidence checkpoint.

The built-in alpha policy grants `observe`, `communicate`, Engram-internal
`coordinate` leases, and lease-backed `mutate_local` to a session whose
declared assurance first meets the active project requirement, whose
effect-specific assurance floor is then met, and whose mediated effects cover
the request. `coordinate` is accepted only at the lease boundary and is not a
model-turn capability. The bind receipt exposes
`effective_mediated_effects`: the declared set capped by the host's assurance.
`observe` and `communicate` may remain effective for an advisory host;
`coordinate` and `mutate_local` require at least `turn_gated` even when the
project floor is advisory. Before reserving anything, `lease_acquire` applies
that same policy ladder: the project floor, then the effect floor, the declared
and assurance-capped mediation sets, the active policy's supported effects,
and the session policy epoch. A project-floor refusal carries no effect; an
effect-floor refusal names the first failing effect and its intrinsic floor. A
stale epoch returns `policy_epoch_changed` and is adopted atomically; retrying
that exact key still returns the stored refusal, while a fresh key re-evaluates
and may proceed. Acquisition fingerprints have their own schema version and
include the current bind generation, so a pre-upgrade retry or reuse after
`session_bind` is an explicit idempotency conflict. Within one bind, do not
reuse a successful acquisition key after release; mint a new key for a new
reservation. A local-mutation intent must name one or more
normalized resources, all covered by a live exclusive `execution` lease held
by that session. The grant freezes the lease fence and `turn_begin` rechecks
it. Shared mutation, external, and lifecycle requests fail closed, and an
`action_gated` bind is rejected because per-tool action authorization is not
shipped. A decision-service process does not by itself prove control: the
embedding runtime may claim `turn_gated` only when it withholds every prompt
until this sequence succeeds.

For a repository path, the lease subject uses the stable project id and path
segments, for example:

```json
{
  "operation": "lease_acquire",
  "routing_token": "from-session_bind",
  "kind": "execution",
  "mode": "exclusive",
  "subject": {
    "kind": "path",
    "project_id": "value-from-.engram-project",
    "segments": ["src"],
    "coverage": "tree"
  },
  "ttl_seconds": 60,
  "idempotency_key": "claim-src-for-turn-42"
}
```

The matching `turn_evaluate` supplies exact or tree `resource_intents` beneath
that subject. The core rejects a different embedded project id and
NFC-normalizes every segment. The CLI resolves the project root's filesystem
identity before opening the store: `--host-path-policy case_fold|case_sensitive`
(or `ENGRAM_HOST_PATH_POLICY`) when the host knows it, otherwise a probe that
writes one uniquely named file into the project root and looks it up under
the opposite case. The first resolved opener persists that policy; later
resolved openers must present the same one, and a mismatch names both. An
opener that could not resolve the identity (unwritable or missing root) still
reads and tracks work, but every path lease is refused with
`host_path_identity_unresolved` instead of guessing. Windows alias rules
(reserved names, alternate data stream syntax, trailing-dot/space aliases,
known 8.3 aliases) follow the running operating system. `doctor` reports the
persisted and resolved policy. Lease expiry immediately removes authority;
releasing and later reacquiring an overlapping subject advances the
project-wide resource fence even when the new holder belongs to another task.
A session must release active leases before rebinding to another task; expired
rows are audited and terminalized automatically, and an `Exit` checkpoint
releases live rows. The
alpha conservatively treats all overlapping leases from different holders as
conflicts even when `mode: shared`; mutation grants still require
`mode: exclusive`. Renewal, intent-to-exclusive
conversion, explicit handoff, suspension, and audited recovery remain planned.

Action authorization/begin/completion, standalone delivery acknowledgement,
lease renewal/handoff/recovery, heartbeat, and independent exit remain
planned protocol operations.

Hooks can integrate the shipped turn boundary. Full action gating needs a wrapper,
gateway, or native host integration around every declared material tool. If a
shell or network path remains unmediated, the session must not claim
`action_gated` assurance. See the
[control-plane host contract](behavioral-control-plane.md#host-integration-contract).

### Host configuration

Build an executable and configure one stdio MCP process per agent session:

```json
{
  "mcpServers": {
    "engram": {
      "command": "/absolute/path/to/engram",
      "args": [
        "--project-file",
        "/absolute/project/.engram-project",
        "--home",
        "/absolute/host-local/engram-data",
        "mcp",
        "--actor-id",
        "codex",
        "--session-id",
        "replace-with-this-runtime-session-id",
        "--actor-context",
        "model=opus-4.1;reasoning=high",
        "--source-skill",
        "engram-repo"
      ]
    }
  }
}
```

The proprietary runtime supplies actor/session/tool/skill instruction context.
This process exposes exactly the fourteen MCP tools. V1 records host
context with `asserted` assurance; configuration text is not authentication.
Distinct concurrent sessions need distinct `--session-id` values. The database
is shared; the MCP processes are not.

### Dogfood contract

`scripts/parity.test.mjs` runs the real binary against a fresh home with
`engram init` as host setup outside the count,
then drives `add → claim → done` and fails if the agent needed more than three
commands or three supplied fields, typed JSON, or saw a hash, fence, or key in
text output. It also checks that an unheld `note` records a marked observation
without execution credit, while an unnoted `done` supplies its resolving
command even when observations exist.

`scripts/mcp-dogfood.test.mjs` launches real stdio MCP processes against a
fresh home. Its main lifecycle uses only the agent-facing MCP tools: one session
creates, claims, blocks/unblocks, notes, and offers a root; a peer accepts the
checkpoint-coupled handoff, notes, and seals it with `done`. Keyless replay,
`reminders`/`next` derivation, catalog and search filters, cancellation,
compact completion, child creation under a parent, and field revision are
asserted along the way, and no `reminders` or `next` line ever carries a hash,
fence, or key. The CLI path drives the same lifecycle through the words in text
and `--json` modes and keeps one `engram work core focus` call. Both scripts are
part of `scripts/check.sh`.

`scripts/control-dogfood.test.mjs` launches the real bounded JSON-lines service,
bootstraps an advisory policy, activates a turn-gated successor through the
operator CLI, verifies both versions through `doctor`, binds, evaluates,
restarts before begin, proves the old grant cannot begin,
resynchronizes, checkpoints, checks mutation denial, and probes a wrong
routing token. Host action control,
report finalization/publication, scoped lease renewal/handoff/release, review
actions, history, explicit contradiction resolution, and the remaining
administrative CLI remain planned surfaces. They must reuse this core rather
than fork its semantics.
