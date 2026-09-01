---
name: review-code
description: Inspect Engram changes through every project reviewer lens without editing or running quality gates.
metadata:
  termal:
    title:
      strategy: default
---

Inspect staged, unstaged, and untracked changes. This is a read-only,
non-nesting review leaf.

**Do not edit files, mutate Git, run build/test/lint/format gates, mutate the
tracker, spawn agents, or launch nested review commands.** The writable
`/review-changes` parent owns all of those actions.

## 1. Discover the change set

Run only read-only inspection commands:

```bash
git status --short
git diff
git diff --cached
git diff --name-only
git diff --cached --name-only
git ls-files --others --exclude-standard
```

Untracked files do not appear in `git diff`; inspect their contents directly
when relevant. If nothing changed, report that and stop.

## 2. Load reviewer lenses

Run `find .claude/reviewers -name "*.md" -type f` and read every returned file.
Apply each lens inline in this session. Do not create one child per lens.

## 3. Project context

Engram V1 is a Rust, host-local concurrent execution-memory system:

- SQLite+FTS5 is canonical locally; projections are rebuildable.
- RFC 8785 canonical JSON plus SHA-256 addresses immutable objects.
- Memory kind, authority, and delivery are orthogonal.
- Stable project identity unifies sessions/worktrees; task scope is shared and
  agent scope private.
- Claims are idempotent leases; immutable task events and monotonic cursors
  drive peer deltas.
- Final report freeze requires participant contributions or explicit waivers.
- One capture feeds task delta, handoff, and report views; Engram owns
  host-local work, and external trackers are immutable snapshot references.
- Local tasks reference but never mirror external organizational tickets.
- `report_ready` freezes bytes, hash, and idempotency key.
- Publication requires an adapter receipt; retry reuses the frozen payload.
- Actor/authority text from tools and skills is asserted context, not
  authenticated identity.
- V1 uses a side-effect-free dummy tracker. Proprietary integration, cross-host sync,
  embeddings, real DLP, signing, service storage, and encryption are deferred.

Read `AGENTS.md`, `.agents/skills/engram-repo/SKILL.md`, and the relevant docs
when a change needs deeper context.

## 4. Output

Return one consolidated review:

```markdown
# Code Review — YYYY-MM-DD

## Changes Reviewed
- ...

## Actionable
### Critical / High
### Medium / Low

## Informational
- ...

## Reviewer Summaries
- Architecture:
- Memory model:
- Rust:
- Storage:
- Tracker integration:
- Security:
- Testing:

## Suggested Engram updates
- Proposals only; the parent must deduplicate against the tracker.
```

For every finding include severity, `file:line`, why it matters, and a fix
direction. Merge duplicate findings and name all lenses that caught them. If
clean, say `No tracker follow-up suggested.` Do not claim the tracker is up
to date.
