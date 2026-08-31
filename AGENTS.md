# Agent Instructions

## Engram Standing Instructions

Engram is a local-first work, behavioral-control, and execution-memory system
for coding agents. SQLite is canonical on the active host; agent-private
scratch and live execution authority stay there. External intake,
backup/portable/sync, and publication are independent optional capabilities.

### Authority and Git

- Never commit, push, rebase, force-push, or run `bd dolt push/pull` without
  explicit user permission. Read-only Git inspection is always allowed.
- The initial `bd init` command created the repository's first Git commit
  automatically. Do not treat that as standing commit authority.
- Implementers claim their own beads. When implementation is complete, leave
  the bead `in_progress` with a completion and validation comment; Greg owns
  final `bd close` actions.
- Beads ids belong in tracker metadata, commits, and review notes—not source
  comments, identifiers, documentation prose, or user-facing output.

### Architecture Boundaries

- `src/domain.rs` owns substrate-neutral memory, task, report, and actor types.
- `src/canonical.rs` owns RFC 8785 canonical bytes and SHA-256 object identity.
- `src/storage.rs` owns V1 SQLite persistence and integrity verification.
- `src/storage/work.rs` owns local-work projections, feeds, claims, and seals.
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
  and evidence, or a human-authorized waiver accounts for an omission.
  Planned report assembly consumes that seal under a separate fenced
  `ReportAssemblyClaim`, without retaining completed-run authority or draining
  execution again.
- One capture should generate work/task deltas, handoff material, evidence,
  and report input. A future portable projection is a dormant
  transfer/restore head, not a second live ledger.
- Once a report reaches `report_ready`, its bytes and hash are frozen. A
  separately requested publication freezes target and idempotency key; retry
  sends the same payload. A revision creates a superseding report and intent.
- Actor/authority text supplied through tools and skills is asserted context,
  not authenticated identity. Never claim stronger assurance than recorded.
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
- Track this repository's implementation work in Beads until an explicit
  Engram dogfood cutover; never use Markdown TODO lists as a tracker.

### Required Quality Gates

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

On Windows, use `pwsh -File scripts/test-rust.ps1` in place of
`scripts/test-rust.sh`; it preserves the same ordinary and performance-test
phases without the Unix-only file-descriptor-limit adjustment.

After any gate failure, investigate the failing path and classify/fix or track
the actual defect. Do not normalize retries or call an intermittent failure an
acceptable flaky test.

### Review Cadence

- `/review-changes` runs parent-owned quality gates, freezes the worktree, and
  delegates exactly one Codex and one Claude `/review-code` reviewer through
  TermAl with `writePolicy: readOnly`.
- `/review-code` is a read-only, non-nesting leaf. It does not edit files, run
  quality gates, inspect Beads, or mutate the tracker.

This repository tracks its work in Engram (see Work Tracking below);
Beads (`bd`) is read-only history and persistent memories.

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
injected `engram` MCP tools (next, ls, show, add, claim, update, note,
done, handoff, search) ARE the words — use them directly. The shell form
below serves humans and hosts; it needs `engram` on PATH and the host
environment (`ENGRAM_HOME`, `ENGRAM_ACTOR_ID`, `ENGRAM_SESSION_ID`,
`ENGRAM_WORK_AUTHORITY_GRANT` — the grant value comes from a host-private
file and is never typed or logged):

```bash
engram work next                  # what you hold, what is ready, what changed
engram work ls | show REF
engram work add "Title" [--under REF]
engram work claim REF
engram work note "what you found or decided"
engram work done ["what was delivered"]
```

- Claim before you change anything; note decisions and evidence once;
  `done` tells you what is still owed. Receipts carry `next:` commands —
  follow them.
- Beads (`bd`) is read-only history and persistent memories
  (`bd remember` / `bd memories`). Do not create, update, or close beads;
  file follow-up work with `engram work add` instead.
- Never place work refs in source comments, identifiers, or docs prose.

At session end: run the quality gates if code changed, update your Engram
items (`note`, `done`), report changed files and validation, and wait for
explicit authority before any commit or push.
