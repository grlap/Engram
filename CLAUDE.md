# Engram — Standing Instructions for Claude

Read this before changing the repository. Engram is a local-first work,
behavioral-control, and execution-memory system for coding agents. SQLite is
canonical on the active host; agent-private scratch and live execution
authority stay there. External intake, backup/portable/sync, and publication
are independent optional capabilities.

## Authority and Git

- Never commit, push, rebase, or force-push without explicit user
  permission. Read-only Git commands are allowed.
- Implementers claim their own Engram items and complete them with the
  words; never place work refs in source comments, identifiers, docs
  prose, or user-facing output.

## Architecture Boundaries

- `src/domain.rs`: substrate-neutral memory, task, report, and actor types.
- `src/canonical.rs`: RFC 8785 canonical bytes and SHA-256 object identity.
- `src/storage.rs`: V1 SQLite persistence and integrity checks.
- `src/storage/work/`: local-work persistence, split by schema/session, query,
  planning, execution, feed, completion, and integrity invariants.
- `src/control.rs`: pure deterministic control-policy evaluation.
- `src/host.rs`: host-private JSON-lines transport only.
- `src/work_service.rs`: six-operation ambient work protocol translation into
  canonical storage operations.
- `src/tracker.rs`: current neutral external adapter port and side-effect-free
  dummy publication adapter.
- Engram owns host-local work. An imported item cites an immutable external
  snapshot but never silently mirrors external task state.
- A stable project id resolves concurrent sessions and worktrees to one
  active-host store. Optional portable handoff may restore it on the next
  active host. Task scope is shared by default; agent scope is private scratch.
- Assignment plans future ownership; a fenced work claim schedules live work;
  a fenced resource lease authorizes mutation. Their handoff/recovery events
  are immutable and audited.
- Packet hashes reproduce content, while typed dense positions in named
  project, root-work, and run-execution feeds order deltas. A session's dense
  delivery position is distinct from its source-feed progress vector. Global
  row ids and hashes are not safety cursors.
- V1 has one ordinary executor/claim per `WorkRun`; parallel sessions claim
  distinct child runs under a `RootExecution` aggregate.
- Root completion requires a `CompletionSeal` over the dense run-feed cut,
  required child seals, contributions, reconciled actions/leases, acceptance,
  and evidence, or an attributed, audited waiver by a project-bound session.
  Planned report assembly consumes that seal under a separate fenced
  `ReportAssemblyClaim`. One capture feeds deltas, handoffs, evidence, and
  report input; a future portable projection remains a dormant transfer/restore
  head rather than a second live ledger.
- `report_ready` freezes report bytes and hash. A separately requested
  publication freezes target and idempotency key; retry uses the same payload,
  while revision creates a superseding report and intent.
- Tool/skill-provided actor context is asserted, not authenticated.
- External publication still requires an explicit human decision. A host that
  runs the optional behavioral-control plane may independently mediate model
  turns or material external actions.
- SQLite is canonical on the active host; query projections are rebuildable.
  Planned backup may raise `local_backed_up`; planned `portable` mode provides
  one-active-host handoff with writer-epoch release/acquire, head CAS,
  divergence refusal, and no transfer of live claims, leases, grants, delivery
  state, or private scratch. Release freezes old-host mutation; acquire must
  succeed before new-host mutation; portable startup/resume validates the
  remote epoch. Portable shared executable state is transitively closed;
  excluded provenance uses explicit stubs/placeholders, never dangling refs or
  rewritten canonical bytes. Concurrent
  team sync, proprietary adapters, embeddings, real DLP, signing, service
  storage, and encryption are deferred.

## Documentation, Skills, and Review

- Architecture and behavior live under `docs/`; feature briefs live under
  `docs/features/` and should be cross-linked when related.
- The installed capability inventory is [docs/shipped.md](docs/shipped.md);
  keep shipped facts separate from roadmap and target prose.
- Read `.agents/skills/engram-repo/SKILL.md` before changing core behavior.
- This repository tracks its work in Engram (see Work Tracking below).
  Do not create Markdown TODO lists.
- `/review-changes` runs gates in the writable parent and delegates exactly one
  Codex and one Claude `/review-code` reviewer through TermAl in read-only mode.
- `/review-code` is inspection-only and never edits, runs gates, or
  mutates the tracker.

## Pre-Release Discipline

There is no released product, therefore there is no legacy: no
compatibility shims, no old-version support, no migration chains for our
own history. Every schema marker stays 1 until release; change schemas in
place, guarded by one generic different-build refusal. No pinned hashes
anywhere in source or tests — a check derives its reference at runtime
from the same code it checks; the only hashes in the product are canonical
object identity computed at runtime. The only stability contracts are live
external consumers (today: TermAl's host protocol). Ceremony is the enemy;
speed of change is the point.

## Required Quality Gates

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
scripts/test-rust.sh
node --test scripts/review-freeze-fingerprint.test.mjs
node --test scripts/mcp-dogfood.test.mjs
node --test scripts/control-dogfood.test.mjs
node --test scripts/parity.test.mjs
node scripts/check-doc-links.mjs
```

On Windows, use `pwsh -NoProfile -File scripts/test-rust.ps1` in place of
`scripts/test-rust.sh`; it preserves the same ordinary and scale-test
phases without the Unix-only file-descriptor-limit adjustment.

Investigate every failure. Intermittence is a symptom to diagnose, not a
reason to retry until green or quarantine a test.
On the focused item you hold, record each executed gate once: `engram work
gate NAME` for a pass, or `engram work gate NAME --failed FAILURE --ref
opaque-reference` for bounded failure evidence. Bare `gate NAME` always means
pass; when a failed check has no test id, use the check command or check name
as its `--failed` label. A failed gate is an
investigation, never a stop. For every failing test or check, classify the
cause and act in the same session:

- **Test or environment defect** (wrong assertion, stale fixture, host
  contention, missing prerequisite): fix it in the current changeset and
  rerun the gates.
- **Product defect**: file one Engram item per defect with the failing
  test named as the acceptance criterion (`engram work add "…" --accept
  "<test> passes" --kind bug --label gate --under <current item>`), mark the
  current item blocked on it if landing depends on it, and fix it now when it
  is in scope. Never delete, skip, or loosen the test to pass.

Report the classification for every failure before asking for a decision;
"the suite failed" alone is not a report.

## Work Tracking

This repository tracks its work in Engram. In a TermAl-hosted session the
injected `engram` MCP tools (next, ls, show, add, claim, update, gate, note,
done, handoff, remember, memories, forget, search) ARE the words — use them
directly. The shell form below serves humans and hosts; it needs `engram` on
PATH and `ENGRAM_HOME`. Hosts normally inject `ENGRAM_ACTOR_ID` and
`ENGRAM_SESSION_ID`; optional `ENGRAM_ACTOR_CONTEXT` adds attribution without
changing the actor principal. A local shell may omit them and receives explicitly
audited OS-user-environment or synthetic-actor and process-session defaults.
The `local-process-` prefix is reserved for generated process-default work
sessions; a `local-process-v1-*` id may be reused for seven days, after which
the caller must omit `--session-id` to receive a fresh process default.

```bash
engram work next                  # what you hold, what is ready, what changed
engram work ls | show REF
engram work add "Title" [--under REF [--optional]] [--kind KIND] [--label L]
engram work claim REF
engram work update REF [--after OTHER | --drop-after OTHER | --waive CHILD --reason "why" | --supersede-with NEW --reason "why"]
engram work gate NAME [--failed FAILURE]... [--ref opaque-reference]
engram work note "what you found or decided"
engram work done ["what was delivered"]
engram work remember "project note" [--key KEY]
engram work memories [QUERY] | engram work memories --after KEY | engram work memories KEY --full
engram work forget KEY
```

- Claim before you change anything; note decisions and evidence once;
  `done` tells you what is still owed. Receipts carry `next:` commands —
  follow them.
- File follow-up work with `engram work add`; findings and decisions go
  into `note` on the item they concern.
- Never place work refs in source comments, identifiers, or docs prose.

At session end: run the quality gates if code changed, update your Engram
items (`note`, `done`), report changed files and validation, and wait for
explicit authority before any commit or push.
