# Engram — Standing Instructions for Claude

Read this before changing the repository. Engram is a host-local concurrent
execution-memory system for coding agents. Agent-private scratch and
task-shared working memory remain local during execution; an immutable,
polished final report is the deliberate publication boundary to an external
organizational tracker.

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
- `src/tracker.rs`: neutral tracker port and side-effect-free dummy adapter.
- A local task may reference an external ticket but must never mirror it.
- A stable project id resolves concurrent sessions and worktrees to one local
  store. Task scope is shared by default; agent scope is private scratch.
- Claims are atomic idempotent leases with explicit handoff and audited
  recovery; their transitions append immutable task events.
- Packet hashes reproduce content, while monotonic event cursors order peer
  deltas. They are not interchangeable.
- Report freeze requires every participant contribution or an attributed
  waiver. One capture feeds deltas, handoffs, and report input instead of
  becoming another status ledger beside the external backlog tracker.
- `report_ready` freezes report bytes, hash, and idempotency key. Retry uses the
  same payload; revision creates a superseding report and a new key.
- Tool/skill-provided actor context is asserted, not authenticated.
- SQLite is canonical in V1; query projections are rebuildable. Team sync,
  proprietary adapters, embeddings, real DLP, signing, service storage, and
  encryption are deferred.

## Documentation, Skills, and Review

- Architecture and behavior live under `docs/`; feature briefs live under
  `docs/features/` and should be cross-linked when related.
- Read `.agents/skills/engram-repo/SKILL.md` before changing core behavior.
- Use Beads for task tracking. Do not create Markdown TODO lists.
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
