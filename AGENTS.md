# Agent Instructions

## Engram Standing Instructions

Engram is a host-local concurrent execution-memory system for coding agents.
Agent-private scratch and task-shared working memory stay local during
execution; an immutable, polished report is the only artifact published to an
external organizational tracker.

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
- `src/tracker.rs` owns the neutral external tracker port and dummy adapter.
- The external tracker stores organizational work and published reports. A
  local task may reference a ticket; it must never mirror the ticket.
- One stable project id must resolve concurrent sessions and worktrees to the
  same host-local SQLite store. Local never means single-session.
- Task scope is shared among participants and is the default for execution
  findings. Agent scope is private scratch.
- Packet hashes reproduce content; monotonic task-event cursors order peer
  deltas. Do not use a hash as a change cursor.
- Claims are atomic idempotent leases with explicit handoff and audited
  recovery. Every lease transition emits an immutable task event.
- Do not freeze a report until every expected participant contributed and
  marked ready, or an attributed waiver accounts for the omission.
- One capture should generate task deltas, handoff material, and report input.
  Never create a second status ledger beside the external backlog tracker.
- Once a report reaches `report_ready`, its bytes, hash, and idempotency key are
  frozen. A publish retry sends the same payload. A revision creates a
  superseding report and a new key.
- Actor/authority text supplied through tools and skills is asserted context,
  not authenticated identity. Never claim stronger assurance than recorded.
- SQLite is canonical in V1. FTS and other query projections are rebuildable.
  Git/team sync, proprietary tracker integration, embeddings, real DLP,
  signing, service storage, and encryption are deferred—not silently assumed.

### Documentation and Skills

- Architecture and behavior live under `docs/`; feature briefs live under
  `docs/features/` and should be cross-linked when they overlap.
- Use the project skill at `.agents/skills/engram-repo/SKILL.md` before changing
  Engram domain, persistence, publication, or review behavior.
- Track implementation work in Beads, never Markdown TODO lists.

### Required Quality Gates

Run these before handing off code changes:

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
scripts/test-rust.sh
node --test scripts/review-freeze-fingerprint.test.mjs
node --test scripts/mcp-dogfood.test.mjs
node scripts/check-doc-links.mjs
```

After any gate failure, investigate the failing path and classify/fix or track
the actual defect. Do not normalize retries or call an intermittent failure an
acceptable flaky test.

### Review Cadence

- `/review-changes` runs parent-owned quality gates, freezes the worktree, and
  delegates exactly one Codex and one Claude `/review-code` reviewer through
  TermAl with `writePolicy: readOnly`.
- `/review-code` is a read-only, non-nesting leaf. It does not edit files, run
  quality gates, inspect Beads, or mutate the tracker.

This project uses **bd** (beads) for issue tracking. Run `bd prime` for full workflow context.

> **Architecture in one line:** Issues live in a local Dolt database
> (`.beads/dolt/`); cross-machine sync uses `bd dolt push/pull` (a
> git-compatible protocol), stored under `refs/dolt/data` on your git
> remote — separate from `refs/heads/*` where your code lives.
> `.beads/issues.jsonl` is a passive export, not the wire protocol.
>
> See [SYNC_CONCEPTS.md](https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md)
> for the one-screen overview and anti-patterns (don't treat JSONL as the
> source of truth; don't `bd import` during normal operation; don't
> reach for third-party Dolt hosting before trying the default).

## Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work atomically
bd close <id>         # Complete work
bd dolt push          # Push beads data to remote
```

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

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:970c3bf2 -->
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
   bd dolt push
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->

<!-- BEGIN BEADS CODEX SETUP: generated by bd setup codex -->
## Beads Issue Tracker

Use Beads (`bd`) for durable task tracking in repositories that include it. Use the `beads` skill at `.agents/skills/beads/SKILL.md` (project install) or `~/.agents/skills/beads/SKILL.md` (global install) for Beads workflow guidance, then use the `bd` CLI for issue operations.

### Quick Reference

```bash
bd ready                # Find available work
bd show <id>            # View issue details
bd update <id> --claim  # Claim work
bd close <id>           # Complete work
bd prime                # Refresh Beads context
```

### Rules

- Use `bd` for all task tracking; do not create markdown TODO lists.
- Run `bd prime` when Beads context is missing or stale. Codex 0.129.0+ can load Beads context automatically through native hooks; use `/hooks` to inspect or toggle them.
- Keep persistent project memory in Beads via `bd remember`; do not create ad hoc memory files.

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.
<!-- END BEADS CODEX SETUP -->
