# Engram

Host-local work, behavioral control, and audit-grade memory for coding agents
— from decomposition and ready-work selection through evidence-backed
completion, with optional external intake, durability, and publication.

Engram is a standalone Rust tool evolving into a first-class local work system
and behavioral/coordination decision plane backed by concurrent execution
memory. In the
target architecture, a conforming host asks Engram before each agent turn and,
at the strongest assurance level, before each declared material action.
Engram then grants a bounded execution envelope or returns typed recovery
directives. The host remains the actuator and reference monitor; an agent
cannot authorize itself through MCP.

The target core has a local work graph plus two deliberate lifecycle layers:

1. **Local work** — roots, bounded decomposition, prerequisites, ready views,
   assignment, fenced claims, evidence, and completion. Work may originate
   locally; an external snapshot is optional.
2. **Local working memory** — agent-private scratch plus a task-shared working
   set for constraints, decisions, evidence, ownership, handoffs, and
   intermediate findings. The shared task ring is the operational source of
   truth during execution.
3. **An optional durable final report** — at root completion, Engram assembles
   working memory, required child seals, and root contributions under a
   separate fenced report-assembly claim for one polish/freeze step, then
   may publish the immutable report through a separately authorized adapter
   under an idempotent, receipted handoff. Intake, backup/portable/sync, and
   publication are independent optional capabilities; publication is never a
   live mirror.

The local-work/control design is in the [specification](docs/spec.md) (Draft 0.8).
Start with the [vision](docs/vision.md) for the short version.

## Highlights

- **Typed memory, not a bag of strings** — every memory has a `kind`
  (constraint / decision / convention / fact / preference / episode), an
  `authority` (hard / firm / soft), and a delivery mode derived from both.
  See [typed memory model](docs/features/typed-memory-model.md).
- **Budgeted context packets** — three retrieval rungs under hard byte
  budgets: pinned constraints (complete or fail-closed), a titles-only index,
  and on-demand full-text search. Every packet has a reproducible content hash
  plus an ordered event cursor for peer deltas. See
  [context packets](docs/features/context-packets.md).
- **A local work system, not an external-tracker binding** — the target owns
  decomposition, dependencies, priority, readiness, assignment, claims,
  evidence, and completion. Its six-operation ambient model protocol avoids
  turning a CLI into agent ceremony. See
  [local work system](docs/features/local-work-system.md).
- **Planned multi-session coordination** — root-shared memory, one
  executor/claim per child run, atomic resource leases with fencing, explicit
  handoff, typed acknowledged peer deltas, a root contribution/child-seal
  barrier, and separate fenced report assembly.
- **Behavior that memory can govern** — the shipped host-private alpha binds
  sessions, attaches transactional context to durable turn grants, rechecks
  freshness at begin, and requires restart-safe checkpoints. Single-use
  action grants, scoped ownership, and controlled finalization are next. See
  [behavioral control plane](docs/features/behavioral-control-plane.md).
- **One write, many views** — low-friction prose capture feeds task state,
  handoffs, and the final report instead of creating another status ledger.
- **Append-only truth** — immutable, content-addressed versions and events;
  state is derived, history is the audit log. Contradictions become visible
  *contested* state, never last-writer-wins.
- **Completion seal plus optional report pipeline** — the shipped zero-linked-
  state path validates evidence, required children, contributions, and the
  accepted claim fence, refuses live descendant claims, handoffs, or open run
  obligations, then atomically freezes a `CompletionSeal` with the exact
  definition/resolution pairs at its dense cut. Old child runs are fenced by the
  closed ancestor/root-execution generation and cannot cross a root reopen. A
  durable
  `completion_pending` drain for linked actions and resource leases is the next
  control-plane stage. Optional `finalization_pending → report_ready →
  publishing → published` consumes the seal without a second drain. A separately requested
  publication binds target and idempotency key, and only an adapter receipt
  marks `published`. See
  [local tasks & reports](docs/features/local-tasks-and-reports.md).
- **Vendor-neutral optional adapters** — source snapshots, restore-only backup,
  sequential portable handoff, later concurrent sync, and publication are
  independent capabilities. The dummy publication path proves deterministic
  local receipts. See
  [tracker adapter](docs/features/tracker-adapter.md).
- **Honest trust model** — actor identity is asserted runtime context (tool +
  skill instruction metadata), recorded with an assurance level, never
  overclaimed. See [security & trust](docs/features/security-and-trust.md).

## Architecture at a glance

```
optional source ─────────────────────┐
Host + Enforcement SDK ── control ───┼──► Engram core ──► canonical local SQLite
CLI / agent MCP ── local work ───────┘       │
                                             ├─ optional backup/portable/sync
                                             └─ optional frozen publication
```

The target architecture keeps the object model, context delivery,
coordination state, and deterministic control decisions in one core. CLI,
agent-facing MCP, and the host-private control channel remain thin faces over
it. Details in
[architecture](docs/architecture.md).

## Status

The first coding-agent memory loop is implemented and process-level tested.
That current alpha still uses an external task reference to rendezvous
sessions, captures task-shared or agent-private prose, returns bounded context
and ordered deltas, supports provenance inspection, survives MCP process
restart, and coordinates with expiring claims.
Natural rule cues become pinned constraints, claim retries preserve their
original lease, and explicitly declared pinned contradictions stop context
assembly before an agent acts.
A host-private JSON-lines service now provides a working `session_bind →
turn_evaluate → turn_begin → turn_checkpoint` loop. It can bind the session to
an exact live `WorkRun` claim and atomically record host execution observations
and host-minted verification/environment evidence on that run at the control
checkpoint. When the host supplies bounded environment components (toolchain,
optional sandbox/image identity, workspace id, and capability-map revision),
Engram derives and checks their canonical fingerprint and rejects a workspace
or bound-session capability-map mismatch. Verification may cite that exact
environment object, while its anti-stale decision remains bound to the check,
run, and full-content source revision; a later mutation makes it stale. These
values are asserted host facts, not attestation, and must not contain secrets.
Claim ownership is the exact control session id. The claim
receipt and focus view expose a paste-ready
root/work/run/claim/fence tuple, so the host never needs to query SQLite to
construct the binding. It persists routing,
session phases, exact context delivery, short-lived grants, idempotent
operation results, and canonical checkpoint events. A control-process restart
invalidates every unbegun grant and resumes at `sync_required`; retry evidence
remains durable without resurrecting authority.
The active immutable control policy now selects a canonical, hash-addressed
obligation rule set. The built-in set contains one typed rule: every host
observation that reports `source_changed=true` opens an immutable test
obligation on the exact run, even when the action failed or supplied no source
basis. Each observation and resulting obligation freeze the selected rule-set
hash from the grant's policy epoch, so a later policy change affects only new
observations and never reinterprets recorded history. A later passed test may
satisfy the stock obligation only against the newest basis-bearing source
mutation at the evaluated feed cut. The operator-only control-policy CLI can
select another bounded typed set whose test requirement pins an exact command
fingerprint and previously recorded environment-evidence hash; this is not a
general natural-language policy engine.
If the newest mutation has no basis, the open obligations remain waiver-only
until another basis-bearing mutation and test arrive. Focus, nested next views,
updates, and both completion outcomes share one bounded `obligation_page`
with typed guidance and an explicit omission count. Dense deltas still show
definition and terminal satisfaction/waiver events. Waiver remains host/
operator-only: the CLI and private JSON-lines channel require dedicated
authority, while MCP and `work_update` expose no waiver operation. Agent pages
and host receipts omit the grant and reason. `work_complete` returns a
durably replayable `open_work_obligations` result until every applicable
definition is satisfied or waived and its typed evidence is acknowledged by
the final checkpoint. New completion seals also bind the sorted, distinct
environment-evidence hashes visible at their dense run-feed cut (at most 64),
without copying toolchain or sandbox bytes into the seal.
Opening a replacement process rotates an internal connection generation, so a
still-running predecessor fails with `control_connection_superseded`. Begun
turns remain checkpoint-required. `session_status.open_grant_id` identifies
every uncertain turn; for an observe-only partial recovery page,
`session_status.recoverable_grant` also returns the exact frozen payload so a
replacement host can redeliver it without advancing the confirmed cursor.
The same channel now provides resource-scoped `lease_acquire` and
`lease_release`. Before it reserves a resource, lease acquisition enforces the
project floor first, then the effect floor, declared and effective mediation,
supported effects, and the session policy epoch. Policy refusals are durable
decisions under the request's bind-scoped idempotency key. A key from an older
bind conflicts instead of replaying obsolete authority. The alpha policy
permits a `mutate_local` turn only when the host declared that effect mediated
and every requested resource is covered by a live exclusive execution lease
held by the session. Grants capture the lease id, subject, expiry, and
monotonically increasing fence and recheck that
basis at `turn_begin`. A begun mutation turn pins those leases across release,
nominal expiry, and host restart until its checkpoint closes the uncertain
authority; conflicting acquisition reports that checkpoint obligation. Path
subjects are bound to the session project and Unicode-normalized. The first
opener atomically persists the host filesystem
identity policy: Windows and macOS defaults case-fold, Linux defaults
case-sensitive, and Windows also rejects reserved names and filename aliases.
Later openers must match it.
Context assembly and the stamped task head share one SQLite transaction;
`turn_begin` refuses a stale grant when task membership, lifecycle, policy,
capability mapping, delivery token, or task head changed. `engram doctor`
hash-verifies both the earlier shadow observations and the enforced session,
grant, decision, and operation records.

The built-in enforced policy is deliberately limited to `observe`,
`communicate`, turn-gated Engram-internal `coordinate` leases, and lease-backed,
turn-gated `mutate_local`. `coordinate` is not a model-turn capability. Shared
mutation, external effects, and lifecycle requests fail closed. The
agent-facing MCP loop remains **advisory**, and shipping the decision service
alone does not
make a deployment `turn_gated`: the embedding host must actually withhold
prompts unless it receives and begins a grant. Per-tool action mediation,
lease renewal/handoff/recovery, report assembly/publication, and the broader
administrative CLI remain under development. The shipped policy CLI can
select `advisory`, `turn_gated`, or fail-closed `action_gated` as the project
requirement; a host declaration of `action_gated` is still rejected because
per-tool action mediation is not shipped.

The first-class local work graph and six-operation ambient protocol are now
shipped through one service core with CLI and MCP translations. It includes
bounded root/decomposition admission, typed prerequisites/blockers,
deterministic readiness, assignment/deferral revision, fenced claims,
checkpoint/evidence, explicit handoff, evidence-gated completion/reopen,
project-bound planning with reason-attributed waivers, dense
project/root/run feeds with acknowledged exact-page replay, ambient per-session
focus/cursors, typed work-scoped shared/private memory, query/history views,
honest cancel/supersede lifecycles, one-call evidence capture/completion, and
work-projection integrity verification. Agent work responses are capped at
12 KiB: `work_next` supports selectable compact sections, stages only the dense
summary prefix actually delivered, and exposes full memory content on demand
rather than repeating canonical snapshots. Update/handoff responses remain
constant-sized as history grows. CLI and two-process MCP dogfood tests
exercise the full lifecycle without manually shuttling work/run/claim/offer
ids.

The Host Enforcement SDK binding, action/resource outcomes linked to work
runs, recovery snapshots/portable handoff, Beads round-trip migration, and
optional publication remain the next implementation slices. Until the host
SDK mediates prompt/action dispatch, agent-facing MCP work calls are still an
advisory interface even though their lifecycle transactions enforce claims,
fences, and project binding.

The CLI requires `ENGRAM_HOME` (or `--home`) and resolves this repository's
tracked `.engram-project` identity to one database shared across worktrees.
Initialize with `engram init` or make an attributed bootstrap choice with
`engram init --required-assurance <level> --authorized-by <actor> --reason
<text>`, verify with `engram doctor`, change the immutable policy through
`engram control-policy set-required-assurance --idempotency-key <key>` (the
durable key replays the exact receipt after an uncertain response), or select a
validated typed obligation set with
`engram control-policy set-obligation-rule-set --input <JSON|@file>
--idempotency-key <key>`. Ordinary open never repairs indexes, triggers, or FTS;
the explicit `engram doctor --repair-projections` path recreates them and
repopulates FTS from exact-current durable rows without rewriting those rows.
Run the MCP
server
with `engram mcp --actor-id <agent> --session-id <session>`, or run the
host-private service with `engram control --actor-id <agent> --session-id
<session>`, or run `engram work --actor-id <agent> --session-id <session>`.
See the
[CLI and MCP guide](docs/features/cli-and-mcp.md) for host configuration and
the exact shipped tool set.

## Documentation

| Doc | What it covers |
| --- | --- |
| [Shipped today](docs/shipped.md) | Exact installed-build capability inventory, kept separate from planned work |
| [Host checklist](docs/host-checklist.md) | The base-tier integration any host needs: identity injection, one MCP child, the `next` hook, honest assurance |
| [Vision](docs/vision.md) | Why Engram exists; behavioral control, memory, and reporting |
| [Architecture](docs/architecture.md) | Components, object model, ports, data flow |
| [Specification](docs/spec.md) | The normative Draft 0.8 working design |
| [Feature briefs](docs/features/README.md) | Per-feature design briefs |
| [Local work system](docs/features/local-work-system.md) | Decomposition, readiness, claims, evidence, agent protocol, optional adapters |
| [Behavioral control plane](docs/features/behavioral-control-plane.md) | Turn/action gates, coordination protocol, host contract |
| [Development](docs/development.md) | Dev & review workflow, quality gates, conventions |
| [Roadmap](docs/roadmap.md) | V1 cut, V1.x, and what is deliberately deferred |
