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
then repair the task/cursor/lease/finalization prerequisites, then mediate
freshness, and only then enable a replay-proven refusal set and action gates.
This keeps false refusals and hook latency measurable before Engram can block
work.

Current milestone: agents use thirteen words — `next`, `ls`, `show`, `add`,
`claim`, `update`, `gate`, `note`, `done`, `handoff`, `remember`, `memories`,
`forget` — as flat CLI commands and MCP tools over the unchanged six-operation
core; `add → claim → done` is
measured at three commands and no JSON, hashes, fences, or keys, and every
receipt ends with `reminders` and `next`. The separate JSON-lines host service
process-tests a
restart-safe `session_bind → turn_evaluate → turn_begin → turn_checkpoint`
loop with transactional context and stale-grant refusal. It also process-tests
exclusive resource acquisition/release, overlap fencing, denial of unleased
local mutation, and a lease-backed local-mutation turn. Per-action mediation,
the full lease recovery/handoff lifecycle, and controlled finalization remain
on the V1 path below.

- Rust core; local SQLite canonical store (append-only, content-addressed)
  with stable project identity, WAL multi-process access, ordered task events,
  and derived FTS5 tables — [SQLite store](features/sqlite-store.md)
- First-class local work graph: parent forest, transactionally cycle-checked
  completion-dependency DAG (explicit prerequisites plus required-child
  edges), priority, assignment, labels, deferral, derived readiness, fenced
  claims, acceptance evidence, human decisions, and the six-operation ambient
  model protocol —
  [local work system](features/local-work-system.md)
- Behavioral control: deterministic turn decisions, typed recovery
  directives, inline packet/delta delivery, recovery/finalizer grants,
  checkpoints, effect-specific degraded debt, mediation coverage reporting,
  and honest advisory/turn-gated/action-gated assurance —
  [behavioral control plane](features/behavioral-control-plane.md)
- Same-host multi-session roots: one executor/claim per child `WorkRun` under a
  `RootExecution`, work claims distinct from normalized resource-scoped fenced
  leases, suspension-aware expiry, explicit handoff, root-shared memory,
  contribution/child-seal barrier, and a separate fenced report-assembly claim
- Context packets: budgets, fail-closed pinned tier, omission manifest,
  packet hash + explain, typed source-feed vectors plus independent
  per-session delivery positions, peer deltas, and review counts —
  [context packets](features/context-packets.md)
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
  dummy publication under idempotent receipts —
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
  claims/leases/grants/private scratch. `doctor` reports `local`,
  `local_backed_up`, `portable`, or later `synchronized` honestly.

The local-work acceptance test is operational and running: this repository
and one migrated project use Engram as their only writable local tracker,
with the previous tracker's archive kept for comparison. The second project
was migrated by hand with the ordinary agent words — no adapter was
involved. Every fallback
becomes a missing-primitive finding. The broader replacement claim — off-host
durability through the selected mode's restore path, plus control binding —
is declared only after that dogfood passes without an unmodeled workflow.
Accepted risk while the dogfood runs: manual backup/restore exists, but
backups stay host-local unless copied off-host by hand, `doctor` reports no
backup freshness, and nothing runs automatically — so there is no off-host
recovery guarantee until `local_backed_up` ships, and losing the active
host can lose local work state. The designed
[work-graph snapshot](features/work-graph-snapshot.md) is the first artifact
on that path: one deterministic file that recreates a store on a build whose
snapshot format matches and moves a project between machines by hand; it
reduces the risk only once a copy leaves the host.

## V1.x — improve the loop

- Session-end distillation into working memory (proposer + dedup)
- Episodic compaction automation
- Post-publication retention compaction
- Budget tuning from retrieval decision logs
- Work-graph snapshot export/restore
  ([designed](features/work-graph-snapshot.md)), then optional configured
  external backup automation: an off-host copy with `doctor` freshness
  reporting for `local_backed_up`

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
| Environment policy beyond shipped exact-hash obligation pins | Acceptance needs signed attestation, component predicates, or environment families rather than one previously recorded identity — [execution pipeline](features/execution-pipeline.md) |
| External intake system (enrichment, planning, sufficiency check) | The manual import → execute → publish loop has closed several times and the manual steps are the bottleneck — [execution pipeline](features/execution-pipeline.md) |
| General obligation rule language beyond the shipped policy-selected typed rule sets | More projects need triggers, conditions, evidence kinds, blocking phases, and waiver authority that cannot be represented by the bounded V1 schema — [execution pipeline](features/execution-pipeline.md) |

## Open decisions

- Default grace period for post-publication retention — pick during V1
  implementation.
- Optional backup recovery-point objective and portable push cadence.
- See [spec §12](spec.md#12-decisions) for the resolved decision record.
