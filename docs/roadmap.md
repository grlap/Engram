# Roadmap

Target plan: [spec §11–12](spec.md#11-delivery-plan). The
[shipped inventory](shipped.md) records implemented Engram capabilities;
this page separates current pilot readiness, ongoing work and deferred
targets. A source-tree capability or passing component test does not prove
that a host is running and using it successfully.

## Current readiness

A **pilot** means installing Engram, or Engram together with TermAl, for a
colleague and observing their real work. Checks the agents run themselves,
such as test-runner recognition, scratch-repository proofs and validation on
Phoenix (a .NET repository with submodules), are **technical
qualification**: they show that a capability works, not that a pilot has
started or progressed. This page gives no target or forecast dates; dates
mark only past checkpoints and decisions, and readiness is stated only as
delivered or outstanding evidence.

**Engram alone: ready to install for a colleague.** Project resolution from
repository subdirectories and the team usage instructions are delivered and
installed. Set up with the [usage skill](skills/engram-use/SKILL.md) and the
[advisory host checklist](host-checklist.md#base-tier--advisory) (an
explicit local store home and a stable project identity), then supervise a
first task. The pilot itself, installing for a colleague and observing real
work, has not started. This path is independent of TermAl and promises no
unattended operation, off-host durability or completed team rollout.

**Engram + TermAl: technical qualification is still open.** Engram owns
durable work, evidence and deterministic control decisions; TermAl owns
session supervision, prompt dispatch, wake delivery and runtime recovery.
The table records the state at the 2026-10-08 checkpoint; owner reports and
validation evidence are kept in the work tracker. **Needed before an
integrated pilot** means the outcome must be delivered and proven on the
running host before Engram + TermAl is installed for a colleague.

| Area | Status | What remains | Owner |
| --- | --- | --- | --- |
| Engram-alone setup | Delivered and installed | Nothing technical; installing for a colleague is the pilot step. | Engram |
| Engram fixes of 2026-10-08: search in note bodies, evaluation-ID alias removal, documentation cleanup and a test-fixture race | Landed and installed | Nothing; supporting fixes, not prerequisites for either path. | Engram |
| Root-session causal diagnostics (SEV0) | In progress, in review | Needed before an integrated pilot. A held root continuation must keep a bounded, redacted record of its original failure and show it on the session card; the scope is diagnostics only. Final review, full gate and landing are outstanding. | TermAl runtime |
| Recovery of held control operations | Narrowed to proven causes | Broad automatic recovery is not being built: each future observed stop gets its own fix for its proven cause, using the diagnostics above. The earlier recovery foundation is parked, and an existing admission retry still lacks its live proof. | TermAl runtime; [Engram recovery contract](features/behavioral-control-plane.md#failure-and-recovery-behavior) |
| Declared test commands with TRX results | Landed and installed; not yet proven on the running host | Technical qualification: activation on the running host, then live PASS, FAIL and UNKNOWN, including a missing result file, for a declared `dotnet test` command. Needed before an integrated pilot for a .NET team that relies on host-recorded test evidence. | TermAl runner |
| Real C# evidence | Open | Technical qualification: real xUnit-on-VSTest TRX output from Phoenix, and correctly recorded passing and deliberately failing runs in a scratch repository. Needed before an integrated pilot for a .NET team that relies on host-recorded test evidence. NUnit, MSTest and Microsoft.Testing.Platform are separate follow-ups. | Phoenix owner with TermAl; Engram records the evidence |
| Source basis for repositories with submodules | Design accepted; implementation not started | Needed only when a pilot relies on host-recorded test evidence in a repository with submodules, such as Phoenix; not blocking otherwise. Until then only the scratch repository can earn that evidence. Git filters and LFS stay outside this design. | TermAl source accounting |
| Typed Build outcomes | Landed and installed; not yet proven on the running host | Matters only when the chosen workflow relies on host-recorded Build evidence. | TermAl build producer |
| Runtime safety requirements | Open | Second-host exclusion in the same directory, Stop and crash behavior on the current build, and ownership of child processes for profiles that admit them. Needed before an integrated pilot. | TermAl runtime |
| Git push/pull and portable handoff | Design assessment reopened | No Git transport or portable handoff ships; neither path needs one. | Engram design, with TermAl consultation |

The integrated path to a pilot: land the diagnostics; fix each observed stop
for its proven cause without losing or duplicating a prompt; activate the
landed fixes on the running host through the
[coordinated cutover](host-checklist.md#before-upgrading-cut-over-each-store);
complete the technical qualification the chosen workflow needs; then install
Engram + TermAl for a colleague and observe real work. A .NET team that
relies on host-recorded test evidence needs the declared test commands and
real C# evidence qualified, plus the submodule basis if its repository has
submodules; a workflow without host-recorded checks does not. Recovery must
preserve the original authority, retained head, Stop and pause state, and
diagnostics grant no retry authority. The statuses above waive none of the
runtime safety requirements.

Technical qualification for .NET covers xUnit on VSTest with TRX first. Other
frameworks and result formats, optional coverage of every harness, unrelated
refactoring and the wider unattended-operation backlog do not block a
supervised pilot. A pilot relies on host-recorded evidence only from
harnesses that have passed their qualification.

## V1 — close the loop

Everything in V1 serves one loop: **open/import local work → decompose and
select ready work → admit synchronized turns and coordinated actions →
complete with evidence → optionally freeze/publish → review promotion
candidates.**

Engram's advisory and turn-gated primitives are implemented. The broader
replay-proven turn-refusal set and the degraded-envelope matrix remain V1
targets in the control plane's
[delivery sequence](features/behavioral-control-plane.md#delivery-sequence).
They stay disabled until that sequence's phase 1 core prerequisites hold,
and those are still incomplete.
The current integration work validates the implemented primitives' use by
the host, including failure diagnostics and fixes for observed stops;
passing Engram protocol tests does not close that host work.
Action gates remain designed and deferred:
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
  contribution/child-seal barrier; the separate report-assembly claim is planned
- Bounded work context and ordered peer deltas through `next`, with explicit
  keyed project-memory reads through `memories`. Generic task-memory capture
  and context-packet construction are removed; their
  [historical formats](features/context-packets.md) remain readable.
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
  optional. The shipped off-host store-copy path is described below;
  sequential `portable` handoff remains a V1 target under the
  [delivery plan](spec.md#11-delivery-plan),
  but is not implemented. Its Git push/pull scope assessment has reopened at
  the operator's request. Concurrent synchronization remains deferred; neither
  is an available durability mode or needed for either pilot path.

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
had not changed at that checkpoint; this is not a fresh inventory of live
backup configuration. Once a target is configured, `engram backup push`
copies the whole store, verified and gzip-compressed, to that directory
target whenever the host's trigger runs it.
`backup status` and `doctor` report `local_backed_up` only while the
freshness rule holds, and always say on what the off-host part rests;
`next` reminds when a target is configured and its mode is `local`, or a push
failed; it emits no backup reminder without a configured target.
`engram backup restore`
installs a copy onto a clean home. Together these protect against losing
the active host's store, and against damage to it that the full check of
each copy detects, provided a qualifying copy sits at a target that really
leaves the machine. They do not protect against:

- an off-host claim that is false: for a directory target it is the
  operator's assertion, shown as unverified;
- losing work recorded after the newest copy's cut;
- the target losing or altering its copies between checks: ordinary
  `backup status` reports recorded evidence; only the newest copy is
  checked by the next push or `backup status --check-target`, so older
  retained copies are not re-checked;
- disclosure at the target: Engram does not encrypt copies, and a store
  copy is readable by anyone who can read the target, so the target must
  be readable only by the operator's own account;
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

Ongoing optimization aims at simpler tools and working procedures, with less
context, repeated output and coordination overhead. Compact recovery and
test-completion notifications are delivered; further optimization and
refactoring are non-blocking unless a concrete pilot failure depends on them.
The following remain targets, not current pilot prerequisites; the backup
item is partly shipped:

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
| Concurrent team-sync backend | A future authorized cross-host scope needs concurrent live writers; neither it nor sequential portable handoff is needed for either pilot path. |
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
- Git push/pull scope: distinguish ordinary backup/restore from transfer of
  the active writer between hosts. The existing portable draft is not a
  shipped transport or a concurrent-writer design. Portable push cadence
  depends on that scope. The copy kinds, freshness window and other
  defaults of the separate backup capability are decided in
  [off-host backup](features/off-host-backup.md).
- See [spec §12](spec.md#12-decisions) for the resolved decision record.
