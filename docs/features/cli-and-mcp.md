# CLI & MCP Surface

> Normative reference: [spec §8](../spec.md#8-interfaces). Related briefs:
> [context packets](context-packets.md),
> [local work system](local-work-system.md),
> [local tasks & reports](local-tasks-and-reports.md), and
> [behavioral control plane](behavioral-control-plane.md).

One core library owns classification, object storage, scope authorization,
task binding, deltas, and context-packet construction. The CLI and MCP server
are thin faces over it; transport code does not redefine memory policy.

The shipped agent-facing MCP interface is **advisory**. A model can omit an
MCP call, so that surface alone cannot enforce synchronization, ownership, or
finalization. A separate host-private JSON-lines channel now implements the
first turn lifecycle; grants are not exposed as tools with which the agent can
authorize itself.

## Shipped CLI

```bash
export ENGRAM_HOME=/absolute/host-local/path
engram init
engram doctor
engram mcp \
  --actor-id codex \
  --session-id session-unique-id \
  --source-skill engram-repo
engram control \
  --actor-id codex \
  --session-id session-unique-id \
  --source-skill engram-repo

# Host/operator boundary: mint a bounded, expiring grant for one exact actor.
GRANT=$(engram authority grant \
  --subject-actor-id codex \
  --issued-by host-operator | jq -r .grant)

# Read-only work operations need no grant. Mutations resolve this host-bound hash.
engram work --actor-id codex --session-id session-unique-id next
engram work --actor-id codex --session-id session-unique-id \
  --authority-grant "$GRANT" focus <short-ref>
engram mcp --actor-id codex --session-id session-unique-id \
  --work-authority-grant "$GRANT"
```

`--project-file` defaults to the tracked `.engram-project`. Its stable project
identity resolves to the same opaque SQLite path for every worktree and
session on the host. `doctor` verifies every canonical object plus
hash-bound control record, reports the built-in policy envelope and live
issued/begun turns, and visibly warns that action gating, organizational
authority mediation, and action-outcome reconciliation are unavailable. V1's
development no-op redactor provides no secret or PII protection.

`engram work` exposes `next`, `focus`, `propose`, `update`, `complete`, and
`handoff`. Mutation payloads accept an inline JSON object or `@path` to a JSON
file. All six call the same service core as MCP. The current operator CLI also
installs and irreversibly revokes canonical work-authority grants; those
commands are not MCP tools. Search/stats/import/export and the remaining broad
administrative CLI in the specification are still planned.

## Shipped MCP tools

| Tool | Purpose |
| --- | --- |
| `task_start` | Start or ref-idempotently bind a local task |
| `task_join` | Join the same task using only its external reference |
| `memory_note` | Capture prose once with inferred classification and a required retry key; `target: task|work` is required when both contexts are active |
| `memory_contradict` | Link two visible task/work shared version hashes as explicitly incompatible |
| `memory_context` | Build a bounded packet for the session's active task and persisted work focus |
| `memory_delta` | Return only task changes after a processed cursor |
| `memory_search` | Full-text search visible memory |
| `memory_show` | Verify and inspect an authorized memory by the receipt's version hash |
| `context_explain` | Reproduce the inclusion and omission reasons for a packet |
| `task_claim` | Acquire an expiring, safely retryable task lease with typed conflict details |
| `work_next` | Return selected compact focus/catalog/ready/change sections under a 12 KiB wire ceiling; an exact staged summary page replays until acknowledged |
| `work_focus` | Select by short ref/UUID and inspect bounded acceptance, graph, claim, blocker ids, evidence, handoff, memory index, history count/tail, exact waivable-child candidates, and allowed-next state without claiming |
| `work_propose` | Create a root or atomically add bounded children/prerequisites to focused work; decomposition receipts return the complete compact child identity set under the 12 KiB ceiling |
| `work_update` | Apply claim/release, checkpoint, evidence, blocker, revision/assignment/deferral, dependency, or reopen updates under inferred fences; unblock may omit its id when exactly one blocker is active |
| `work_complete` | Seal focused work under inferred fences, optionally recording evidence and its final checkpoint in the same high-level call |
| `work_handoff` | Offer, accept, or cancel the unique checkpoint-coupled handoff without passing offer/claim ids |

Every result is structured JSON. Expected execution failures are MCP tool
errors with stable error codes and details—for example, a live claim conflict
returns `task_claim_held` with holder plus epoch and ISO-8601 expiry. Repeating
a claim with the same key and TTL returns the original lease even though
wall-clock time advanced; changing the TTL under that key is an idempotency
conflict. A private memory hash does not become a capability: `memory_show`
repeats project/task/participant/owner authorization, verifies the persisted
work focus, and returns `memory_access_denied` to a peer.

The shipped `task_claim` is a whole-task advisory lease and predates the local
work design. The target does not silently reinterpret it: a fenced work claim
schedules one work item, while a separately versioned resource-lease operation
authorizes mutation over canonical subjects. The legacy tool is deprecated
after both replacement paths ship.

## Current advisory agent loop

1. The first session calls `task_start` with the external tracker reference;
   peers call `task_join` with that reference alone. Engram persists each
   session's active binding, including across MCP process restart.
2. Call `memory_context` once and retain its `event_cursor`. Inject the pinned
   and index tiers into the working context. This remains a convention in the
   direct advisory slice. The shipped host-private control protocol instead
   persists exact deliveries and requires their acknowledgement before issuing
   a turn grant.
3. Call `memory_note` when a decision, constraint, finding, evidence pointer,
   or handoff fact becomes worth sharing. Supply a caller-stable
   `idempotency_key`; retry the exact call after a lost response, and generate a
   new key for different prose. Natural-language prefixes such as
   `Decided:` help classification but flags are not required. Natural rules
   beginning with `Never`, `Always`, `Must`, `Do not`, or `Only` become firm
   pinned constraints without flags. The receipt
   returns the inferred fields, their basis, canonical hashes, cursor, and
   idempotency key.
4. Before the next work turn or after a host wake, call `memory_delta` with the
   retained cursor. Advance it only after processing the returned changes.
5. Use `private: true` only for incomplete scratch. It remains searchable and
   inspectable by the owning agent but never enters peer packets or deltas.
6. When two shared records cannot both guide action, call
   `memory_contradict` with their version hashes and a reason. Both become
   visibly contested. If both are applicable firm/hard pinned records,
   `memory_context` fails with `pinned_contradiction` and names the edge and
   versions instead of asking the agent to invent precedence.

One capture powers peer context and the ordered feed. The host may use a
mailbox as a doorbell, but must not relay full state or make the agent repeat
the same fact into another status ledger.

## Shipped agent-native work protocol

The normal model surface is six ambient operations: `work_next`, `work_focus`,
`work_propose`, `work_update`, `work_complete`, and `work_handoff`. Session
binding supplies project, actor, current work, and cursors, so update/complete/
handoff do not repeatedly shuttle ids. Ambient state contains no authority;
the host fixes a canonical grant hash for the MCP process and each mutation
rechecks that grant and revocation state. `work_next` returns only the selected
`focus`, `ready`, `catalog`, and/or `changes` sections; omitting `sections`
selects all four. CLI callers use `--sections focus,ready,catalog,changes` and
MCP callers pass a string array. Selecting no `changes` section never stages or
advances project delivery, including when a prior page remains pending.
Hash-verified source changes retain dense positions and explicit compact
summaries instead of canonical work snapshots or memory bodies. A change's
`object_hash` identifies the verified canonical source and intentionally is not
a hash of its summary. Restricted
work memory, and work memory outside the session's currently focused verified
root, is replaced by a typed `omission` marker at its original dense position;
the protected body and structured fields never cross the agent boundary. The
largest dense prefix fitting the fixed change budget is staged durably and
replayed until
the caller returns its exact `delivered_through` value and opaque
`delivery_token` as `acknowledge_through` and `acknowledge_token`; agent-facing
change projections are stored canonically with that pair and replay byte-for-
byte even if focus or legacy-task binding changes. Concurrent appends wait for
the next page. Both acknowledgement fields
are absent when changes were not delivered, so an undisclosed or guessed cursor
cannot be acknowledged. Every
successful work response is at most 12,288 serialized JSON bytes; typed
`omissions` report advisory sections shortened by count or byte budget. A repeated
acknowledge-and-fetch request safely replays any newer staged page after a lost
response. `work_focus` is navigation only and never claims/releases as a side
effect. It returns an exact history count with a bounded newest-event summary
tail, the latest run even after completion, and a body-free actor-filtered
memory index whose version hash is the key for authorized `memory_show`.
Before changing ambient focus, callers first replay any pending page by calling
`work_next` with `changes` selected and no acknowledgement. They then pass the
`delivered_through` and `delivery_token` actually received as
`acknowledge_through` and `acknowledge_token` while selecting sections that
exclude `changes`. The typed `work_delivery_pending` response explains this
sequence without exposing the host-only tentative cursor or token.
`work_propose`
atomically handles roots and bounded decomposition. `work_update` carries a
typed transition such as claim/release, checkpoint, blocker, cancel,
supersede, deferral, assignment, revision, or prerequisite change. Update and
handoff success responses contain only a compact receipt, current obligations,
and `allowed_next`; their size does not grow with item history. Each entry
names the exact tool and tagged operation. For example,
`allowed_next: ["work_update:claim(recovery_reason_required)"]` directs the
agent to submit the `work_update` claim variant with an attributed
`recovery_reason` and a host grant that includes `claim_recovery`; ordinary
claiming is `work_update:claim`.

`work_complete` can consume evidence/checkpoint state created through explicit
`work_update` calls, or accept `capture { summary, refs }` to record evidence,
checkpoint its exact evidence set, and seal in one model-level call. Completion
is refused while blockers, prerequisites, required child seals or explicit
completion waivers, live handoffs, or capture requirements remain unresolved.
An interrupted capture-backed completion recovers committed evidence or
checkpoint timestamps for exact replay, while missing substeps use current
time and recheck the live claim.

The MCP schemas for `work_propose`, `work_update`, `work_complete`, and
`work_handoff` expose their typed discriminated inputs rather than opaque JSON.
Every mutating branch requires a caller-stable `idempotency_key`. Durable
attempts bind caller intent separately from the current focused work/claim/
handoff basis, so committed retries replay while interrupted attempts
revalidate authority and cannot mutate a newly focused item.

Work search/lifecycle filters, paged catalog results, and item history ship in
the ambient query/focus views. Stats, stale/orphan diagnostics, approval
decisions, import/export, and report publication remain administrative tools
over the same core. Successful model responses are terse; refusals return a
stable code plus a satisfiable remedy. Full durable receipts go to the host. No
replayable turn/action grant token appears in model-visible MCP output. The
work-authority hash is a host process argument, not a request field or tool
result.

## Shipped host-private turn channel

`engram control` is a long-lived stdio process. It accepts one JSON object per
line and returns one `{ "status": "ok", "result": ... }` or typed error line.
The runtime session and asserted actor are fixed by process arguments. The
shipped operations are:

| Operation | Durable effect |
| --- | --- |
| `session_bind` | Start/join the referenced task, rotate a routing token, reset to `sync_required` |
| `session_status` | Read current phase, cursors, epochs, mediation declaration, revision, and any safely redeliverable partial recovery grant |
| `lease_acquire` | Atomically grant or defer a normalized resource lease and append its fenced task event |
| `lease_release` | Release a lease held by this session and append its fenced task event |
| `turn_evaluate` | Derive membership/context/head/policy from SQLite and persist a decision plus optional grant |
| `turn_begin` | Recheck freshness and exact delivery token, then consume the issued grant |
| `turn_checkpoint` | Promote tentative delivery, complete the grant, and append a canonical checkpoint event |

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
required checkpoint. When the begun grant contains an observe-only partial
recovery page, `session_status.recoverable_grant` returns the exact canonical
grant and delivery bytes; the replacement host redelivers that payload and
then checkpoints the already-begun grant. The confirmed cursor does not move
until that checkpoint. Other begun turns expose no replayable prompt because
their outcome may be uncertain. Reusing a key for a different intent fails.

The built-in alpha policy grants `observe`, `communicate`, and turn-gated
`mutate_local` to a session declaring at least `turn_gated` assurance and the
corresponding mediated effects. A local-mutation intent must name one or more
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
NFC-normalizes every segment. The first store opener atomically persists the
host filesystem identity policy: Windows and macOS defaults case-fold, Linux
defaults case-sensitive, and Windows rejects reserved names, alternate data
stream syntax, trailing-dot/space aliases, and known 8.3 aliases. Later openers
must present the same policy. Lease expiry immediately removes authority;
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

`memory_note` never makes an ambient scope switch silently. If exactly one of
the legacy task binding or local work focus exists, that context is the
default. If both exist, the caller must select `target: task` or
`target: work`; omission is a typed `invalid_argument` refusal. Task notes
reach peers through `memory_delta`. Work notes retain the focused item as
their provenance subject, enter project/root/run feeds, and reach work peers
through `work_next` and focused context. Capture, search, and show re-read the
persisted focus at the storage boundary, so a stale or caller-supplied work id
cannot redirect access.

Hooks can integrate the shipped turn boundary. Full action gating needs a wrapper,
gateway, or native host integration around every declared material tool. If a
shell or network path remains unmediated, the session must not claim
`action_gated` assurance. See the
[control-plane host contract](behavioral-control-plane.md#host-integration-contract).

## Host configuration

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
        "--source-skill",
        "engram-repo",
        "--work-authority-grant",
        "replace-with-host-installed-grant-hash"
      ]
    }
  }
}
```

The proprietary runtime supplies actor/session/tool/skill instruction context.
V1 records it with `asserted` assurance; configuration text is not
authentication. Distinct concurrent sessions need distinct `--session-id`
values. The database is shared; the MCP processes are not.

## Dogfood contract

`scripts/mcp-dogfood.test.mjs` launches two real stdio MCP processes against a
fresh home and checks ref-only rendezvous, flag-free classification, context,
single-item delta, private-scope non-disclosure (including a raw-hash probe),
restart durability, provenance/explain, idempotent retry/conflict, and lease
contention/expiry/retry. It also verifies zero-flag natural constraint
classification and explicit pinned-contradiction fail-closed behavior. It now
also installs operator grants, drives a real two-session work root through
claim, evidence, checkpoint-coupled handoff, recipient checkpoint, and sealed
completion without shuttling lifecycle ids, and drives the same lifecycle
through the CLI translation. It is part of `scripts/check.sh`.

`scripts/control-dogfood.test.mjs` launches the real bounded JSON-lines service,
binds, evaluates, restarts before begin, proves the old grant cannot begin,
resynchronizes, checkpoints, checks mutation denial, and probes a wrong
routing token. Host action control,
report finalization/publication, scoped lease renewal/handoff/release, review
actions, history, explicit contradiction resolution, and the complete
administrative CLI remain planned surfaces. They must reuse this core rather
than fork its semantics.
