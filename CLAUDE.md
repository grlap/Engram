# Engram — Standing Instructions for Claude

Read this before changing the repository. Engram is a local-first work,
behavioral-control, and execution-memory system for coding agents. SQLite is
canonical on the active host; agent-private scratch and live execution
authority stay there. External intake, backup/portable/sync, and publication
are independent optional capabilities.

## Authority and Git

- Never commit, push, rebase, force-push, or run remote Beads sync without
  explicit user permission. Read-only Git commands are allowed.
- `bd init` created the initial Git commit automatically. It grants no ongoing
  commit authority.
- Implementers claim their own beads and leave completed work `in_progress`
  with completion/validation comments. Greg owns final `bd close` actions.
- Never place Beads ids in source comments, identifiers, docs prose, or
  user-facing output.

## Architecture Boundaries

- `src/domain.rs`: substrate-neutral memory, task, report, and actor types.
- `src/canonical.rs`: RFC 8785 canonical bytes and SHA-256 object identity.
- `src/storage.rs`: V1 SQLite persistence and integrity checks.
- `src/storage/work.rs`: local-work projections, feeds, claims, and seals.
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
  and evidence, or a human-authorized waiver. Planned report assembly
  consumes that seal under a separate fenced `ReportAssemblyClaim`. One
  capture feeds deltas, handoffs, evidence, and report input; a future portable
  projection remains a dormant transfer/restore head rather than a second live
  ledger.
- `report_ready` freezes report bytes and hash. A separately requested
  publication freezes target and idempotency key; retry uses the same payload,
  while revision creates a superseding report and intent.
- Tool/skill-provided actor context is asserted, not authenticated.
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
- Read `.agents/skills/engram-repo/SKILL.md` before changing core behavior.
- Use Beads for this repository's task tracking until an explicit Engram
  dogfood cutover. Do not create Markdown TODO lists.
- `/review-changes` runs gates in the writable parent and delegates exactly one
  Codex and one Claude `/review-code` reviewer through TermAl in read-only mode.
- `/review-code` is inspection-only and never edits, runs gates, or calls `bd`.

## Required Quality Gates

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
scripts/test-rust.sh
node --test scripts/review-freeze-fingerprint.test.mjs
node --test scripts/mcp-dogfood.test.mjs
node --test scripts/control-dogfood.test.mjs
node scripts/check-doc-links.mjs
```

Investigate every failure. Intermittence is a symptom to diagnose, not a
reason to retry until green or quarantine a test.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:6cd5cc61 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->
