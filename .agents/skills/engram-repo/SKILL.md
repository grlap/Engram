---
name: engram-repo
description: Work in the Engram repository when changing its typed memory model, local task lifecycle, canonical object storage, SQLite backend, report finalization, tracker adapters, CLI/MCP surfaces, or repository review system. Do not use for unrelated Rust projects.
---

# Engram Repository

Engram is a local-first work, behavioral-control, and execution-memory service
for multiple agent sessions. It owns local work from creation/decomposition
through evidence-backed completion. SQLite is canonical on the active host;
agent-private scratch and live execution authority remain there. External
snapshot intake, backup/portable/sync, and frozen publication are independent
optional capabilities. Preserve that boundary in code, tests, docs, and commands.

## Read the Relevant Contract

- Start with `docs/architecture.md` for component and data-flow boundaries.
- Read `docs/features/typed-memory-model.md` for kinds, authority, delivery,
  visibility, versioning, and contradiction behavior.
- Read `docs/features/local-tasks-and-reports.md` when changing tasks, report generation,
  publication, retry, receipts, or retention.
- Read `docs/features/local-work-system.md` when changing work items,
  decomposition, dependencies, readiness, assignment, claims, completion, or
  external work migration.
- Read `docs/features/security-and-trust.md` for identity assurance, redaction, secrets, and
  irreversible publication constraints.

If a referenced document does not exist yet, use `AGENTS.md` as the active
contract and keep the change narrow.

## Hard Invariants

- Memory kind, authority, and delivery are orthogonal fields.
- Immutable versions supersede; they are never edited in place.
- Canonical object identity is SHA-256 over RFC 8785 UTF-8 JSON bytes.
- Unknown schema versions may be retained but are never activated.
- Applicable hard/firm pinned contradictions and pinned-budget overflow fail
  context assembly before an agent acts.
- Local work needs no external reference. Explicit imports preserve immutable
  source snapshots and never silently mirror external state.
- Local does not mean single-session: one stable project id resolves every
  session and worktree to the same active-host store. Optional portable
  handoff may restore it on the next active host.
- Agent scope is private; task scope is shared among participants and is the
  default for execution findings.
- Assignment is future intent; fenced work claims schedule execution; fenced
  resource leases authorize mutation. Never infer one from another. Every
  handoff/recovery transition emits an immutable event.
- Packet hashes reproduce content; typed dense positions in named project,
  root-work, and run-execution feeds order deltas. A session's dense delivery
  position is distinct from its source-feed progress vector. Never substitute
  a hash or global row id for either.
- V1 has one ordinary executor/claim per `WorkRun`; parallel sessions claim
  distinct child runs under a `RootExecution` aggregate.
- Root completion requires a `CompletionSeal` over the dense run-feed cut,
  required child seals, contributions, reconciled actions/leases, acceptance,
  and evidence, or a human-authorized waiver. Planned report assembly
  consumes the seal under a separate fenced `ReportAssemblyClaim`, without
  retaining completed-run authority or draining execution again.
- One capture must feed work/peer deltas, handoffs, evidence, and report
  assembly. A future portable projection is a dormant transfer/restore head, not a
  second live status ledger.
- Planned `portable` mode is one-active-host handoff: scheduled push, writer-epoch
  release/acquire under remote-head CAS, explicit restore, and divergence
  refusal. Release freezes old-host mutation; acquire must succeed before
  new-host mutation, and portable startup/resume validates the remote epoch.
  Never restore live work claims, resource leases, control grants/delivery
  state, or agent-private scratch. Portable executable shared state must be
  transitively closed; excluded provenance uses explicit stubs/placeholders,
  never dangling references or rewritten canonical bytes.
- `report_ready` freezes report bytes and hash. A separately requested
  publication freezes target and idempotency key. Failed publication returns
  to the same frozen report; revision creates a superseding report and intent.
- No adapter receipt means the task is not published.
- Host-provided actor/authority text is asserted context unless a stronger
  assurance mechanism actually verified it.
- Do not persist secrets. The V1 redactor may be a visibly labeled no-op, but
  no code or documentation may imply that provides compliance assurance.

## Ownership Boundaries

- `domain`: substrate-neutral meaning and state transitions.
- `canonical`: serialization and content identity only.
- `storage`: SQLite transactions, migrations, immutable rows, integrity, and
  rebuildable indexes.
- `storage/work`: local-work projections, typed feeds, claims, and seals.
- `control`: pure control-policy evaluation without I/O.
- `host`: host-private transport without policy forks.
- `work_service`: six-operation ambient protocol translation into canonical
  storage operations.
- external adapters: backend-neutral source snapshots, backup, portable
  handoff, later concurrent sync, frozen publication, idempotency, and receipt
  capabilities.
- `verbs`: the eight-word agent surface; flat CLI flags and MCP arguments
  translate into the unchanged six-operation core and every receipt gains
  `reminders` and `next` from fixed tables.
- CLI/MCP front doors translate requests; they do not redefine domain rules.

Keep proprietary tracker types, authentication schemes, and organization
policy outside the core. Extend ports using neutral request/response records.

## Using Engram as an agent

Engram tracks the work of this repository. You use eight words; everything
else is the host's business. The host sets `ENGRAM_HOME`, `ENGRAM_ACTOR_ID`,
`ENGRAM_SESSION_ID`, and `ENGRAM_WORK_AUTHORITY_GRANT` in your environment;
you type only the word.

```bash
engram work next                  # what is ready, what you hold, what changed
engram work ls [--search TEXT] [--blocked] [--mine]
engram work show REF              # one item: outcome, acceptance, holder, blockers, reminders
engram work add "Title" [--outcome "..."] [--accept "criterion"]... [--under REF] [--priority 0-4] [--label L]
engram work claim REF [--recover "why"]   # you now hold it; later commands default to it
engram work update REF [--release | --blocked "why" | --unblock | --cancel "why" | --assignee A | --priority N | --defer DATE | --title "..."]
engram work note "What you found or decided" [--ref path-or-url]
engram work done ["What was delivered"]
engram work handoff REF --to ACTOR | --accept | --cancel "why"
```

Add `--json` to any word for the exact structured receipt.

Rules that matter:

- `add` needs only a title. Outcome and acceptance criteria are welcome; they
  are what `done` is checked against.
- `claim` before you change anything. Only the holder can `note` and `done`.
- `note` is for decisions, findings, and evidence pointers. One note feeds
  peers, handoff, and the final report; never repeat it elsewhere.
- `done` completes the item you hold. If something is still owed, the answer
  is one sentence saying what and a command that resolves it. Do it and run
  `done` again.
- Every answer ends with `reminders` (what is owed, in words) and `next`
  (commands you can run now). Nothing asks you to copy hashes, fences, or
  keys; if you see one, it is a bug.
- Lost response? Run the same command again. Identical calls are safe.

The same eight words are MCP tools (`next`, `ls`, `show`, `add`, `claim`,
`update`, `note`, `done`) with the same flat arguments, plus `search` and
`handoff`.

## Verification

Run the smallest focused test while iterating, then finish with:

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

Use `/review-changes` for the two-agent read-only review after the gates pass.
Do not commit, push, close beads, or sync remotes without explicit authority.
