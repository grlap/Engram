# Roadmap

Normative source: [spec §11–12](spec.md#11-delivery-plan). This page tracks
the phases and — just as deliberately — what is deferred and what would
trigger revisiting it.

## V1 — close the loop

Everything in V1 serves one loop: **open/import local work → decompose and
select ready work → admit synchronized turns and coordinated actions →
complete with evidence → optionally freeze/publish → review promotion
candidates.**

Control ships progressively: first observe/replay with every decision allowed,
then repair the work/cursor/completion prerequisites, then mediate
freshness, and only then enable a replay-proven refusal set.
This keeps false refusals and hook latency measurable before Engram can block
work. Action gates are designed and deferred, not part of this progression:
see [action gates](features/action-gates.md#when-to-build-it).

Current milestone: agents use fourteen words — `next`, `ls`, `show`, `add`,
`claim`, `update`, `gate`, `evaluate`, `note`, `done`, `handoff`, `remember`, `memories`,
`forget` — as flat CLI commands and MCP tools over the unchanged six-operation
core; the baseline `add → claim → done` lifecycle is measured at three
commands with no JSON, full record ids, fences, or keys, and every receipt
ends with `reminders` and `next`. The separate JSON-lines host service
process-tests a
restart-safe `session_bind → turn_evaluate → turn_begin → turn_checkpoint`
loop with transactional context and stale-grant refusal, plus turn-gated local
mutation. Resource leases, host obligation waiver, the finalizer turn
purpose/phase, the "defer" answer and the never-written session phases have
been removed. Per-action mediation is not built; its intent is kept under
[planned interfaces](features/behavioral-control-plane.md#planned-interfaces).
Optional report assembly remains deferred.

- Rust core; local SQLite canonical store (append-only, minted record ids)
  with stable project identity, WAL multi-process access, ordered task events,
  and derived FTS5 tables — [SQLite store](features/sqlite-store.md)
- First-class local work graph: parent forest, transactionally cycle-checked
  completion-dependency DAG (explicit prerequisites plus required-child
  edges), priority, assignment, labels, deferral, derived readiness, fenced
  claims, acceptance evidence, human decisions, and the six-operation ambient
  model protocol —
  [local work system](features/local-work-system.md)
- Behavioral control: deterministic turn decisions, typed refusal
  directives, checkpoints (inline packet/delta delivery and recovery grants
  are dropped: turn grants carry no delivery page), effect-specific degraded
  debt, mediation coverage reporting, and honest advisory/turn-gated
  assurance (action-gated is not built) —
  [behavioral control plane](features/behavioral-control-plane.md)
- Same-host multi-session roots: one executor/claim per child `WorkRun` under a
  `RootExecution`, fenced work claims, explicit handoff, root-shared memory,
  contribution/child-seal barrier, and a separate fenced report-assembly claim
- Context packets: budgets, fail-closed pinned tier, omission manifest,
  packet fingerprint, typed source-feed vectors plus independent
  per-session delivery positions, peer deltas, and review counts —
  [context packets](features/context-packets.md) (not built; the `next`
  word's work context is the only delivery today)
- `engram work note` / MCP `note`: one work finding feeds peer, handoff,
  evidence, and report views
- Agent-surface Cuts A and B: gate results are auditable evidence,
  prerequisites and supersession are `update` flags, and attributed project
  episodes ship through `remember` / `memories` / `forget` with a content-free
  `next` signal —
  [local work
  system](features/local-work-system.md#gates-prerequisites-supersession-and-project-memories)
- The contention-robust scale gate asserts canonical, work-event, and item
  decode budgets—materialization work—while printing p95 wall-clock latency
  only as diagnostic evidence, so foreign host load cannot fail the test —
  [development workflow](development.md#quality-gates)
- Write policy matrix, proposal/approval, review queue,
  supersede/contradict/contested, tombstones —
  [write policy & review](features/write-policy-and-review.md)
- Optional report path: deterministic assembly, polish/freeze state machine,
  publication under idempotent receipts (all deferred; no dummy ships) —
  [local tasks & reports](features/local-tasks-and-reports.md),
  [tracker adapter](features/tracker-adapter.md)
- Audit attribution at asserted-runtime-context assurance; visibly labeled
  no-op Redactor — [security & trust](features/security-and-trust.md)
- CLI + agent-facing MCP + host-private control transport over one core;
  hostile-process tests prove turn and declared-capability bypasses fail —
  [CLI & MCP](features/cli-and-mcp.md)
- Deterministic recovery snapshot/restore, round-trip Beads migration,
  referential/projection integrity, fixture-level retrieval checks, and
  `doctor` / explicit `doctor --repair-projections`. External durability is
  optional; V1 adds
  sequential `portable` push/handoff/restore with remote-head CAS, scheduled
  cadence, visible lag/degradation, writer-epoch validation at startup/resume,
  exact-base restore, divergence refusal, complete shared-state projection with
  explicit exclusion stubs/feed placeholders, and no transfer of live
  claims/grants/private scratch. `doctor` reports `local`,
  `local_backed_up`, `portable`, or later `synchronized` honestly.

The local-work acceptance test is operational and running: this repository
and one migrated project use Engram as their only writable local tracker,
with the previous tracker's archive kept for comparison. The second project
was migrated by hand with the ordinary agent words — no adapter was
involved. Every fallback
becomes a missing-primitive finding. The broader replacement claim — off-host
durability through the selected mode's restore path, plus control binding —
is declared only after that dogfood passes without an unmodeled workflow.
Accepted risk while the dogfood runs: losing the active host loses local
work state unless an off-host copy of the store exists.
[Off-host backup](features/off-host-backup.md) narrows that risk, but only
for a project whose operator has configured a target: until then its mode
stays `local` and the risk applies in full. As of 2026-10-02 no live store
on the dogfood host has a target configured, so for the dogfood the risk
has not changed yet. Once a target is configured, `engram backup push`
copies the whole store, verified and gzip-compressed, to that directory
target whenever the host's trigger runs it.
`backup status` and `doctor` report `local_backed_up` only while the
freshness rule holds, and always say on what the off-host part rests;
`next` reminds of a `local` mode or a failed push. `engram backup restore`
installs a copy onto a clean home. Together these protect against losing
the active host's store, and against damage to it that the full check of
each copy detects, provided a qualifying copy sits at a target that really
leaves the machine. They do not protect against:

- an off-host claim that is false: for a directory target it is the
  operator's assertion, shown as unverified;
- losing work recorded after the newest copy's cut;
- damage the full check cannot see, such as wrong rows that still decode,
  once retention (three copies by default) has removed every copy taken
  before it;
- a copy that no obtainable build can restore: a restore needs a build that
  accepts the copy's format, or a migration path from it;
- a host that never runs its trigger, since Engram schedules no push of its
  own;
- a second writer: nothing detects one, so the origin must be retired
  before a restore, and sessions after a restore need identities the
  restored store has not seen, while its live claims stay held until they
  expire.

Neither the graph copy kind nor the Git adapter, the only adapter that
observes a remote's own acknowledgement, is shipped; and an acknowledgement
would not be evidence of the provider's durability.
The shipped
[work-graph snapshot](features/work-graph-snapshot.md) remains a manual
path: one deterministic file that recreates a store on a build whose
snapshot format matches and moves a project between machines by hand; it
reduces the risk only once a copy leaves the host.

## V1.x — improve the loop

- Session-end distillation into working memory (proposer + dedup)
- Episodic compaction automation
- Post-publication retention compaction
- Budget tuning from retrieval decision logs
- Optional configured external backup automation, designed in
  [off-host backup](features/off-host-backup.md): the verified full-store
  copy at a directory target, with `doctor` freshness reporting for
  `local_backed_up` and restore, is shipped; still planned are the
  work-graph snapshot as a second copy kind and the Git adapter

## V2+ — widen the loop

- Real source/publication adapters
- Concurrent Git/external-storage/service backend with org/team scopes
  ([design preserved](spec.md#33-deferred-concurrent-cross-host-sync))
- Optional embeddings for retrieval
- Wider outbound publication: comments and link-backs; no continuous mirror
- Real Redactor/DLP integration
- Postgres/service `Store` backend behind the same ports
- Signer-based attestation; envelope encryption for crypto-shredding

## Deferred — and what would revive each

| Deferred capability | Revisit when |
| --- | --- |
| Concurrent team-sync backend | Two hosts must coordinate live; sequential cross-machine handoff is V1 `portable` |
| Proprietary tracker adapter | Work authorizes real publication |
| Real DLP/redaction backend | A tool is mandated, or memory starts holding sensitive material |
| SSO/LDAP identity | Compliance-grade attribution becomes a deployment promise |
| Embeddings | FTS5 + good titles measurably stop being enough (per the evaluation harness) |
| Service backend / Signer / envelope encryption | Team scale or compliance posture demands them |
| Capability requirements on work items matched against the host's bind-time capability map | More than one hosting environment can take the same item, or a host without a required skill claims work it cannot finish — [execution pipeline](features/execution-pipeline.md) |
| Environment requirements on obligations (none can name an environment today) | Acceptance needs signed attestation, component predicates, or environment families; a requirement must name environment content, not a record of one run — [execution pipeline](features/execution-pipeline.md) |
| External intake system (enrichment, planning, sufficiency check) | The manual import → execute → publish loop has closed several times and the manual steps are the bottleneck — [execution pipeline](features/execution-pipeline.md) |
| General obligation rule language beyond the shipped policy-selected typed rule sets | More projects need triggers, conditions, evidence kinds, blocking phases, and waiver authority that cannot be represented by the bounded V1 schema — [execution pipeline](features/execution-pipeline.md) |
| Per-action gates and `action_gated` assurance | The operator explicitly reverses the decision of 2026-09-26 that the host will not block dangerous operations — [action gates](features/action-gates.md) |

## Open decisions

- Default grace period for post-publication retention — pick during V1
  implementation.
- Portable push cadence. The copy kinds, the freshness window and the other
  defaults of backup are decided in
  [off-host backup](features/off-host-backup.md).
- See [spec §12](spec.md#12-decisions) for the resolved decision record.
