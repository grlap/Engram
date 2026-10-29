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

Run each of these as its own standalone command from the working directory,
one command per call. A Claude reviewer uses the Bash tool; the read-only
policy refuses its PowerShell tool. A Kimi reviewer runs shell commands only
through the Bash tool, which is bash, approved by the host gate for read-only
commands.
A Codex reviewer uses the default shell
of its `exec_command` tool (PowerShell on Windows) and does not ask for shell
`bash` or a Git Bash path: on Windows those can resolve to WSL, which may be
missing, and then every command fails before it runs. A read-only reviewer
delegated by TermAl runs under a read-only command policy. It governs which
commands run, not a Codex reviewer's shell:

- Do not add helpers the policy does not list, such as `cmp`, to a command or
  chain. A Claude or Kimi reviewer reads files with the Read, Grep and Glob
  tools; a Codex reviewer reads them with `Get-Content` or `rg` in its default
  shell.
- Do not retarget Git with `git -C`, `--git-dir`, `--work-tree` or
  `--namespace`. These are refused by design, because a repository chosen
  that way can carry configuration that runs programs.
- Do not use `git hash-object`; it can run configured clean filters. Use
  `git diff --no-index FILE_A FILE_B` to inspect content differences. Inside
  a repository it may apply end-of-line conversion and is not a byte-identity
  check. When byte identity matters, compare a per-file hash of both sides.

Untracked files do not appear in `git diff`; inspect their contents directly
when relevant. If nothing changed and no committed input was supplied, report
that and stop. For a supplied committed input, read its complete change against
the supplied base, including added files, even though the checkout is clean.

For a frozen input, follow the Git identity checks in
[/review-changes section 3](review-changes.md#3-freeze-the-review-input).
The parent stages the input and records `TREE` with `git write-tree`;
reviewers never run that writing command. At the start and end, run separately:

```bash
git diff --cached --quiet TREE
git diff --quiet
git ls-files --others --exclude-standard
```

Require zero exit statuses and an empty untracked listing; confirm the supplied
HEAD and report the tree id and `index == TREE, clean` at both boundaries.
For a committed input, confirm HEAD equals the supplied commit, report its
tree id with `git show -s --format=%T COMMIT`, and require empty output from
`git status --porcelain --untracked-files=all`, with zero exit statuses.
Both boundary tree ids must match the parent's recorded id. Raw capture and
the compiled host freeze checker are optional only for the concrete raw-byte
exceptions recorded under section 3; neither is an ordinary review prerequisite.

If Git inspection is still refused and you cannot see some or all of the diff,
say so under Coverage in the result. Name what you could not see and how you
reviewed it instead, for example by reading the changed files. Matching boundary
identities do not show what changed or exclude transient edits, so they never
replace reading the diff. Report a refused identity command as an access gap;
do not invent a successful comparison or replace it with mandatory raw capture.

## 2. Load reviewer lenses

List every `.md` file under `.claude/reviewers/` and read each one. A Claude
or Kimi reviewer lists them with the Glob tool; a Codex reviewer runs
`rg --files .claude/reviewers -g '*.md'`, which works in any shell. Apply each
lens inline in this session. Do not create one child per lens.

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
- The supplied Git tree id and the independent start/end identity results.

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
