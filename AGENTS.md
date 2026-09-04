# Agent Instructions

## Engram Standing Instructions

Engram is a local-first work, behavioral-control, and execution-memory system
for coding agents. SQLite is canonical on the active host; agent-private
scratch and live execution authority stay there. External intake,
backup/portable/sync, and publication are independent optional capabilities.

### Authority and Git

- Never commit, push, rebase, or force-push without explicit user
  permission. Read-only Git inspection is always allowed.
- Implementers claim their own Engram items and complete them with the
  words; never place work refs in source comments, identifiers,
  documentation prose, or user-facing output.

### Architecture Boundaries

- `src/domain.rs` owns substrate-neutral memory, task, report, and actor types.
- `src/canonical.rs` owns RFC 8785 canonical bytes and SHA-256 object identity.
- `src/storage.rs` owns V1 SQLite persistence and integrity verification.
- `src/storage/work/` owns local-work persistence, split by schema/session,
  query, planning, execution, feed, completion, and integrity invariants.
- `src/control.rs` owns pure deterministic control-policy evaluation.
- `src/host.rs` owns the host-private JSON-lines transport only.
- `src/work_service.rs` owns the six-operation ambient work protocol and its
  translation into canonical storage operations.
- `src/tracker.rs` currently owns the neutral external adapter port and dummy
  publication adapter; vendor-specific types stay outside the core.
- Engram owns host-local work from creation/decomposition through completion.
  An item may cite an immutable external snapshot, but Engram never silently
  mirrors external task state.
- One stable project id must resolve concurrent sessions and worktrees to the
  same active-host SQLite store. Local never means single-session; optional
  portable handoff may restore that project on the next active host.
- Task scope is shared among participants and is the default for execution
  findings. Agent scope is private scratch.
- Packet hashes reproduce content; typed dense positions in named project,
  root-work, and run-execution feeds order deltas. A session's dense delivery
  position is distinct from its source-feed progress vector. Global row ids
  and hashes are not safety cursors.
- Assignment is future intent; a fenced work claim schedules live execution;
  a fenced resource lease authorizes mutation. Never conflate them. Handoff
  and recovery are explicit and audited.
- V1 has one ordinary executor/claim per `WorkRun`; parallel sessions claim
  distinct child runs under a `RootExecution` aggregate.
- Do not complete a root until `CompletionSeal` binds the dense run-feed cut,
  required child seals, contributions, reconciled actions/leases, acceptance,
  and evidence, or an attributed, audited waiver by a project-bound session
  accounts for an omission. Planned report assembly consumes that seal under a
  separate fenced `ReportAssemblyClaim`, without retaining completed-run
  authority or draining execution again.
- One capture should generate work/task deltas, handoff material, evidence,
  and report input. A future portable projection is a dormant
  transfer/restore head, not a second live ledger.
- Once a report reaches `report_ready`, its bytes and hash are frozen. A
  separately requested publication freezes target and idempotency key; retry
  sends the same payload. A revision creates a superseding report and intent.
- Actor/authority text supplied through tools and skills is asserted context,
  not authenticated identity. Never claim stronger assurance than recorded.
- External publication still requires an explicit human decision. A host that
  runs the optional behavioral-control plane may independently raise the bar
  for model turns or material external actions.
- SQLite is canonical on the active host. Planned external backup may raise
  `local_backed_up`; planned `portable` mode provides one-active-host handoff with
  scheduled push, writer-epoch release/acquire under remote-head CAS,
  divergence refusal, and no transfer of live claims/leases/grants/private
  scratch. Release freezes old-host mutation; acquire must succeed before
  new-host mutation; portable startup/resume must validate the remote epoch.
  The portable projection must close every executable shared-state reference;
  excluded provenance uses explicit stubs/placeholders, never dangling refs or
  rewritten canonical bytes. FTS and work/query projections are
  rebuildable. Concurrent team sync, proprietary adapters, embeddings, real
  DLP, signing, service storage, and encryption are deferred—not silently
  assumed.

### Documentation and Skills

- Architecture and behavior live under `docs/`; feature briefs live under
  `docs/features/` and should be cross-linked when they overlap.
- Use the project skill at `.agents/skills/engram-repo/SKILL.md` before changing
  Engram domain, persistence, publication, or review behavior.
- Track this repository's implementation work in Engram (see Work
  Tracking below); never use Markdown TODO lists as a tracker.

### Pre-Release Discipline

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

Run these before handing off code changes:

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

After any gate failure, investigate the failing path and classify/fix or track
the actual defect. Do not normalize retries or call an intermittent failure an
acceptable flaky test.
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

### Review Cadence

- `/review-changes` runs parent-owned quality gates, freezes the worktree, and
  delegates exactly one Codex and one Claude `/review-code` reviewer through
  TermAl with `writePolicy: readOnly`.
- `/review-code` is a read-only, non-nesting leaf. It does not edit files, run
  quality gates, or mutate the tracker.

This repository tracks its work in Engram (see Work Tracking below).

## Non-Interactive Shell Commands

**ALWAYS use non-interactive flags** with file operations to avoid hanging on confirmation prompts.

Shell commands like `cp`, `mv`, and `rm` may be aliased to include `-i` (interactive) mode on some systems, causing the agent to hang indefinitely waiting for y/n input.

**Use these forms instead:**
```bash
# Force overwrite without prompting
cp -f source dest           # NOT: cp source dest
mv -f source dest           # NOT: mv source dest
rm -f file                  # NOT: rm file

# For recursive operations
rm -rf directory            # NOT: rm -r directory
cp -rf source dest          # NOT: cp -r source dest
```

**Other commands that may prompt:**
- `scp` - use `-o BatchMode=yes` for non-interactive
- `ssh` - use `-o BatchMode=yes` to fail instead of prompting
- `apt-get` - use `-y` flag
- `brew` - use `HOMEBREW_NO_AUTO_UPDATE=1` env var

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
