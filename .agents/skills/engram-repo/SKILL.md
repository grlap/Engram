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
- An established store opens only when every enforced schema marker and
  durable shape exactly matches the current binary; stores created by a
  different build are refused before mutation.
- Never pin a digest in source or tests. References are derived at runtime
  from the code that produces them; hashes remain only as computed canonical
  object identity.
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
  and evidence, or an attributed, audited waiver by a project-bound session.
  Planned report assembly consumes the seal under a separate fenced
  `ReportAssemblyClaim`, without retaining completed-run authority or draining
  execution again.
- External publication still requires an explicit human decision. A host that
  runs the optional behavioral-control plane may independently mediate model
  turns or material external actions.
- One capture must feed work/peer deltas, handoffs, evidence, and report
  assembly. A future portable projection is a dormant transfer/restore head,
  not a second live status ledger.
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
- `storage`: the façade and shared persistence types, with open/schema guards,
  canonical objects/task feeds, task memory/notes, project memory, control
  runtime/support, policy administration, and doctor/integrity split into
  concern modules.
- `storage/work`: local-work persistence split by schema/session, query,
  planning, execution, feeds, completion, and integrity invariants.
- `control`: pure control-policy evaluation without I/O.
- `host`: host-private transport without policy forks.
- `work_service`: six-operation ambient protocol translation split by service
  setup, next/delivery, focus, propose, update, completion, handoff, and memory
  operation families around shared projection helpers.
- external adapters: backend-neutral source snapshots, backup, portable
  handoff, later concurrent sync, frozen publication, idempotency, and receipt
  capabilities.
- `verbs`: the thirteen-word agent surface whose `mod.rs` holds shared
  vocabulary; receipt shaping, terse show rendering, and word handlers live in
  owning modules, with mirrored tests under `src/verbs/tests/`; flat CLI flags
  and MCP arguments translate into the unchanged six-operation core, public
  re-exports preserve `crate::verbs` paths, and every receipt gains `reminders`
  and `next` from fixed tables.
- CLI/MCP front doors translate requests; they do not redefine domain rules.

Keep proprietary tracker types, authentication schemes, and organization
policy outside the core. Extend ports using neutral request/response records.

## Using Engram as an agent

Record changed duties, waits, decisions, and the next permitted action with
`engram work note REF --status TEXT` (MCP `status: true`), not only in a
conversation summary; checkpoint each real duty/wait/next-step change before
going quiet. After compaction or session replacement, explicitly read `next`
before acting and follow any clipped status's full-note locator.
A coordinator without code work keeps one assigned or held coordination
item; `next` is the resume read. Current status is the
newest status qualified by storage at capture for the currently accountable
actor (live holder, otherwise assignee); peer status notes remain observations,
and ordinary notes/gates do not replace the commitment. `show --notes` retains
old statuses; oversized current status explicitly points to full note detail.
A replacement session recovers duties, never the old session's live claim.
For a wait that must survive claim expiry or session replacement, assign its
item with `add --assignee ACTOR` or `update REF --assignee ACTOR`. A held-only,
unassigned status is current only while that claim is live; expired or released
holder status remains history and is never promoted for an unassigned item.
Assignment grants no execution authority and needs no periodic renewal. A
holder's planning edit, including assignment, renews its existing live claim.
Use `add --external REF` or `update REF --external REF` for audited external
planning linkage and `ls --search REF` to find it; record source criteria in
acceptance and context in notes, since the reference alone is not immutable
intake and local completion does not close external work. Clear obsolete linkage
with `update REF --clear-external` (MCP revise `clear_external: true`), an ordinary
audited revision. Blank `--external` and simultaneous set/clear are refused.
Status labels distinguish `you`, `you (another session)`, and `another session`
without exposing actor principals.

Engram tracks the work of this repository. You use thirteen words; everything
else is the host's business. The host sets `ENGRAM_HOME` and normally injects
`ENGRAM_ACTOR_ID` plus `ENGRAM_SESSION_ID`; optional `ENGRAM_ACTOR_CONTEXT`
adds attribution without changing the actor principal. You type only the word.
A local shell may omit either attribution value and receives explicitly audited
OS-user-environment or synthetic-actor and process-session defaults. The
`local-process-` prefix is reserved for generated process-default work
sessions; a `local-process-v1-*` id may be reused for seven days, after which
the caller must omit `--session-id` to receive a fresh process default.

```bash
engram work next [--verbose]      # what is ready, what you hold, what others changed
engram work ls [--search TEXT] [--blocked] [--mine] [--label L] [--all] [--verbose]
engram work show REF [--notes [--gates] | --history] [--after CURSOR]
engram work show REF --note HASH[:INDEX]  # complete immutable note detail
engram work add "Title" [--note "Initial finding"]... [--outcome "..."] [--accept "criterion"]... [--under REF [--optional]] [--priority 0-4] [--kind KIND] [--label L]
engram work claim REF [--ttl SECONDS] [--recover "why"]   # same holder renews; --recover is for another prior holder
engram work update REF [--release | --blocked "why" | --unblock | --cancel "why" | --reject "why" | --after OTHER | --drop-after OTHER | --waive CHILD --reason "why" | --supersede-with NEW --reason "why" | --assignee A | --priority N | --defer DATE | --accept "criterion"... | --title "..." | --kind KIND | --label L | --unlabel L]
engram work gate NAME [--work-ref REF] [--failed FAILURE]... [--ref opaque-reference]
engram work note [REF] "What you found or decided" [--ref path-or-url]
engram work done ["What was delivered"]
engram work handoff REF --to ACTOR | --accept | --cancel "why"
engram work remember "Project note" [--key KEY [--revise [--expected-revision N]]]
engram work memories [QUERY] | engram work memories --after KEY | engram work memories KEY --full [--revision N]
engram work forget KEY
```

Add `--json` to any word for its structured receipt. `next` and `ls` stay
short in text, JSON, and MCP; use `show REF` for safe agent detail without
canonical ids, hashes, fences, or host-control fields. `--verbose` restores
the full structured list projection for a human or host that explicitly needs
it. Host-only `work core` reads remain full.

Rules that matter:

- Reading never steers a later write: `show REF`, including notes, history,
  continuations and detail, preserves focus and staged delivery. Follow its
  explicitly targeted commands. Claiming or explicitly targeting a mutation
  establishes focus; a bare mutation keeps its existing target.

- For unfinished work stranded beneath a completed, cancelled, or superseded
  ancestor, follow the exact `update CHILD --detach "why"` command offered by
  `show`, `next`, or `ls --blocked`. This atomically creates an independent
  root and supersedes the child; it does not reopen the parent or change old
  claims/fences. Claim the returned root before execution. Open descendants,
  live ownership, independent blockers, unfinished prerequisites, and future
  deferral must be resolved first. Assignment and history stay on the source;
  planning content and source provenance carry forward. After an uncertain
  response inspect the old child's successor, rather than repeating blindly.
  See [detached follow-ups](../../../docs/features/local-work-system.md#gates-prerequisites-supersession-and-project-memories).
- `add --note TEXT` is repeatable (MCP `notes` array). Creation and all initial
  observations commit together; only an exact creation replay recovers the
  original observations. See the retry rule below before repeating a call.
  These are attributed non-holder notes, not execution credit or checkpoints.
- Beneath another session's live-held parent, use `add --under REF --optional`
  for an attributed peer proposal. It is Open and unclaimed; the holder sees
  it in `next`, with their item/run/claim/checkpoint untouched. Required
  children or prerequisite changes need the holder. There is no separate
  approval or activation word.

- `show REF --notes` selects newest notes/observations, excluding structured
  gate evidence by default, and renders them chronologically within a 12 KiB
  window. Add `--gates` (MCP `gates: true`, with `notes: true`) to include gates.
  `notes_window.families` reports item-wide totals counted before filtering,
  plus shown and omitted counts for this window. `notes[].summary` is the
  complete body. Follow the printed `--after` command for older windows; it
  preserves `--gates` when selected. Exact counts distinguish older and newer
  omitted rows. Start a fresh window to change gate mode. `--history` uses the
  same continuation shape. Pages state the active byte budget and reflected
  read cut. A continuation keeps only ref/title, records, exact counts and
  navigation; follow `full_detail` for outcome, acceptance and item context.
  A too-large body stays as an explicit locator/size/detail placeholder and
  does not prevent traversal. Use `show REF --note LOCATOR` for complete detail
  beyond 12 KiB, independently of the gate filter. Native locators are unique
  hash prefixes of at least eight hex digits; inherited locators are
  `RECORD_HASH:INDEX`, where INDEX is an
  immutable one-based member position, never a display ordinal. These are
  read-only exceptions to hidden canonical identity. MCP uses `notes`,
  `history`, `after`, and `note` with the same meaning. New note bodies have a
  64 KiB UTF-8 write limit; carry bulk content as a reference. Existing larger
  bodies remain readable. See the
  [window/detail contract](../../../docs/features/cli-and-mcp.md#using-engram-as-an-agent).
- `update REF --accept "criterion"...` replaces the whole acceptance list;
  omitting it preserves the list. Empty or blank criteria are refused, and
  completed work cannot be revised. History names the revised fields.
- `ls` reports an exact filtered total and omitted count, with a `--limit`
  hint when truncated. `--mine` is the deduplicated union of assignment to
  your actor and live claims held by your session. MCP `search` corresponds
  to shell `ls --search TEXT --all` (search includes terminal work).
- `add` needs only a title. Outcome and acceptance criteria are welcome; they
  are what `done` is checked against. `--under REF` creates a required child;
  add `--optional` when that child must not gate its parent's completion.
  Omitted acceptance produces the reminder `acceptance defaulted to the title
  being done; set --accept`; explicit criteria do not. Blank criteria are refused.
  The reminder keeps the signal without repeating the title and fits the receipt.
  Completed, cancelled, and superseded parents refuse new children: file an
  independent root follow-up or add under an open ancestor. A proposed parent
  also refuses children, but directs you to inspect it because it is not open.
  Existing children and their fences remain unchanged.
- Claim before execution. `claim REF --ttl SECONDS` renews your live claim
  without changing its identity or fence, and never shortens its expiry.
  Open-work `gate` and `done` require the holder. A non-holder may `note`
  open work, including blocked work or a child of a completed parent: this is
  a marked observation, not a checkpoint, renewal, or completion credit.
  Existing unclaimed planning updates remain available. After completion,
  any project-bound session may use `note` or `gate` for a late finding
  without claiming or reopening the item;
  the existing seal stays frozen.
- Bare `gate NAME` records a pass. Repeat `--failed FAILURE` for bounded
  failure labels; when a check has no test id, use the check command or check
  name. Use `--ref` as an opaque external-evidence reference (a path or URL by
  convention); Engram does not shape-validate it. With no focus, use
  `gate NAME --work-ref REF`; there is no global last-completed target.
- `note` is for decisions, findings, and evidence pointers. A holder note
  feeds peers, handoff, and the final report. A non-holder observation feeds
  project/root peers without execution authority; its receipt explicitly says
  `(observation, no run credit)`. A late note feeds peers
  but remains outside the frozen seal; never repeat either elsewhere.
- `remember` is for attributed project notes and observations, never rules or
  secrets. `memories` is the source of truth; `forget` tombstones rather than
  erases and permanently retires the safe key.
  Correct an existing note with `remember TEXT --key KEY --revise`, not a
  companion key. Prior attributed versions stay reachable through `memories
  KEY --full --revision N`; ordinary reads return the current version. An
  optional `--expected-revision N` refuses stale writes with the current
  revision; without it the receipt states which revision was replaced and
  which was appended. Identical same-session body and supplied basis replay;
  without a basis only an identical current revision replays. Forget retires
  history reads too; snapshots carry live history but only tombstones for
  forgotten keys, never their old bodies.
- Reject an evidence-disproved finding with an evidence note, then
  `update CHILD --reject "why"` for an Open required child of an Open,
  waivable parent. It atomically composes cancellation and the parent's waiver
  with the same reason and existing authority checks. The root execution must
  be able to record the waiver, including an eligible restored bootstrap.
  Completed work keeps the late-finding `note`/`gate` refusal. For other shapes
  follow the typed conditional remedy: `update CHILD --cancel "why"`, then
  `update PARENT --waive CHILD --reason "why"` only if required and admitted.
  After a lost response, the same session and keyless intent recover both
  committed effects only for the unchanged cancelled child; changed child
  state receives inspection guidance, never stale success.
  `done` is reserved for satisfied current acceptance; its successful receipt
  visibly asserts the seal-bound criterion count and that completion changed
  no criterion. It and completed native `show` name one-based seal positions
  with "no evidence linked to this criterion", with exact omissions when
  bounded. This is not a claim that the work has no evidence. Summary and
  shared acceptance notes do not link artifacts to individual criteria;
  absence does not refuse completion, and no new hash obligation is imposed.
  Old frozen bindings remain exactly as recorded. Do not replace rejection
  with false completion.
- `done` completes the item you hold. If something is still owed, the answer
  is one sentence saying what and a command that resolves it. Do it and run
  `done` again. Successful completion also names remaining open optional
  children, with bounded rows and an exact omitted count. These do not block
  completion. Follow the offered detach command only when admitted; otherwise
  resolve the named condition first. Use the parent `show` command to inspect
  continuation and the broader `ls --blocked` view for blocked work. The receipt
  does not detach, cancel, or claim anything automatically.
- `add`, `claim`, `gate`, `note`, and `done` return one compact item summary,
  operation facts, live holder/expiry, owed counts and actionable signals.
  They do not repeat full focus/history/parent projections. Follow the single
  `full_detail` command for the full item or durable note/gate evidence;
  completion still includes optional-child follow-ups and detach commands.
- Every answer ends with `reminders` (what is owed, in words) and `next`
  (commands you can run now). Mutation words never ask for hashes, fences, or
  idempotency keys. Explicit note-detail locators are read-only exceptions.
  The `next` build token is a
  diagnostic exception: compare it with `engram --version` after an install
  to detect a stale MCP child, never copy it into a work command. See
  [build diagnostics](../../../docs/features/cli-and-mcp.md#build-identity-and-doctor-refusals).
  Safe project-memory keys are
  intentional navigation tokens for `memories` and `forget`.
- Before repeating an uncertain mutation, follow the
  [session and intent retry rule](../../../docs/features/local-work-system.md#agent-native-protocol),
  including its child-creation and append-only exceptions. If a shell used
  the process default and
  lost the entire notice too, inspect with `ls`/`show` before repeating a
  mutation; exact replay cannot cross processes without the printed session.

The same thirteen words are MCP tools (`next`, `ls`, `show`, `add`, `claim`,
`update`, `gate`, `note`, `done`, `handoff`, `remember`, `memories`,
`forget`) with the same flat arguments, plus `search`.

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

On Windows, use `pwsh -NoProfile -File scripts/test-rust.ps1` instead of
`scripts/test-rust.sh`.

Use `/review-changes` for the two-agent read-only review after the gates pass.
After consolidating review findings, deduplicate them in Engram. Only fixes
whose delivery gates landing belong as required children of its open item.
An optional child is only for work intentionally finished within the parent's
execution window whose delivery does not gate landing; nonblocking is not a
blanket instruction to make findings optional children.
Findings deferred beyond that slice are independent roots,
never optional or required children, even if the reviewed item is still open.
Add a provenance note on each new follow-up naming the reviewed item's reference
and title, the finding evidence, and the reason for deferral; do not substitute
a parent or prerequisite edge for provenance. Note matching existing follow-ups
instead of duplicating them. Informational observations need no work item.
In pair work, the implementer continues after review consolidation without
waiting for another prompt: fix in-scope actionable findings, rerun the gates,
and freeze the corrected input for review. After clean acceptance, record the
delivered outcome and complete owned implementation items when their obligations
are satisfied. Pause only for a real blocker, disputed acceptance, or a decision
outside the agreed scope or authority; reviewer leaves remain read-only.
Do not commit, push, or sync remotes without explicit authority.
