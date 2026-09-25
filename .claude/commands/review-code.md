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

Run each of these as its own standalone Bash call from the working directory,
one command per call. A read-only reviewer delegated by TermAl runs under a
read-only command policy:

- Do not use the PowerShell tool; the policy refuses it for read-only Claude
  reviewers.
- Do not add helpers the policy does not list, such as `cmp`, to a command or
  chain; read files with the Read, Grep and Glob tools instead.
- Do not retarget Git with `git -C`, `--git-dir`, `--work-tree` or
  `--namespace`. These are refused by design, because a repository chosen
  that way can carry configuration that runs programs.
- Do not use `git hash-object`; it can run configured clean filters. Compare
  two files with `git diff --no-index FILE_A FILE_B`.

Untracked files do not appear in `git diff`; inspect their contents directly
when relevant. If nothing changed, report that and stop.

If Git inspection is still refused and you cannot see some or all of the diff,
say so under Coverage in the result. Name what you could not see and how you
reviewed it instead, for example by reading the changed files. A passing
freeze check shows only that the input did not change after it was frozen; it
does not show you what changed, so it never replaces reading the diff.

## 2. Load reviewer lenses

Run `find .claude/reviewers -name "*.md" -type f` and read every returned file.
Apply each lens inline in this session. Do not create one child per lens.

## 3. Project context

Engram V1 is a Rust, host-local concurrent execution-memory system:

- SQLite+FTS5 is canonical locally; projections are rebuildable.
- Immutable records are stored as RFC 8785 canonical JSON under a minted UUID
  id; a SHA-256 over those bytes is a content fingerprint, never an identity.
- Memory kind, authority, and delivery are orthogonal.
- Stable project identity unifies sessions/worktrees; task scope is shared and
  agent scope private.
- Claims are idempotent leases; immutable task events and monotonic cursors
  drive peer deltas.
- In the deferred report design, final report freeze requires participant
  contributions or explicit waivers.
- One capture feeds task delta, handoff, and report views; Engram owns
  host-local work, and external trackers are immutable snapshot references.
- Local tasks reference but never mirror external organizational tickets.
- In that target design, `report_ready` freezes report bytes and fingerprint; a
  separately requested publication freezes its target and idempotency key.
  Publication requires an adapter receipt; retry reuses the frozen payload.
- Actor/authority text from tools and skills is asserted context, not
  authenticated identity.
- Publication adapters (including the former unwired dummy), proprietary
  integration, cross-host sync, embeddings, real DLP, signing, service storage,
  and encryption are deferred.

Read `AGENTS.md`, `.agents/skills/engram-repo/SKILL.md`, and the relevant docs
when a change needs deeper context.

## 4. Output

Return one consolidated review:

```markdown
# Code Review — YYYY-MM-DD

## Changes Reviewed
- ...

## Coverage
- How the diff was seen (the Git commands that ran), or which changes could
  not be seen and how they were reviewed instead.

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
