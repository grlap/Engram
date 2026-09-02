---
name: review-changes
description: Run Engram quality gates, freeze the worktree, and obtain independent Codex and Claude reviews.
metadata:
  termal:
    title:
      strategy: default
---

Review all staged, unstaged, and untracked changes from the existing writable
parent session.

**Do not delegate `/review-changes` itself.** The parent owns build artifacts,
quality gates, the worktree freeze, fan-in, and recording findings in
Engram. Only the two `/review-code` children are delegated, both with
`writePolicy: readOnly`.

**Never commit, push, rebase, or sync remotes without explicit user
authority.**

This workflow requires TermAl MCP delegation tools. Attempt exactly two child
spawns: one Codex and one Claude. Do not substitute platform subagents, shell
processes, raw HTTP, or nested TermAl review sessions.

## 1. Confirm the target

Run:

```bash
git status --short
git diff --name-only
git diff --cached --name-only
git ls-files --others --exclude-standard
```

If there are no changes, report that and stop.

## 2. Run parent-owned gates

Run, in order:

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

On Windows, run `pwsh -NoProfile -File scripts/test-rust.ps1` in place of
`scripts/test-rust.sh`; it runs the same ordinary and scale-test
phases without the Unix-only file-descriptor-limit adjustment.

On any failure, do not spawn reviewers. A failed gate is an investigation,
never a stop: classify every failing test or check in this same turn.
Record every executed gate on the focused item you hold: `engram work gate
NAME` for a pass, or `engram work gate NAME --failed FAILURE --ref
opaque-reference` for bounded failure evidence. Bare `gate NAME` always means
pass; when a failed check has no test id, use the check command or check name
as its `--failed` label.

- Test or environment defect (wrong assertion, stale fixture, host
  contention, missing prerequisite): fix it in the current changeset, rerun
  the gates, and continue the review.
- Product defect: file one Engram item per defect with the failing test as
  its acceptance criterion (`engram work add "…" --accept "<test> passes"
  --kind bug --label gate --under <current item>`), mark the current item
  blocked on it when landing depends on it, and fix it now when it is in
  scope.

End the turn with the classification of every failure (test name, cause,
action); "the suite failed" alone is not a report.

## 3. Freeze the review input

Run:

```bash
node scripts/review-freeze-fingerprint.mjs --write .git/engram-review-freeze.json
```

The snapshot covers HEAD, the index, tracked worktree changes, untracked file
contents, symlink targets, and executable modes. The file is kept under `.git`
so it never becomes review input.

## 4. Spawn exactly two reviewers

Use `termal_spawn_session` twice from the current parent:

1. Codex: prompt `/review-code`, mode `reviewer`, `writePolicy: readOnly`, title
   `Codex /review-code`.
2. Claude: prompt `/review-code`, mode `reviewer`, `writePolicy: readOnly`, title
   `Claude /review-code`.

If one spawn fails after the other succeeds, continue waiting for the created
reviewer and report the missing one as unavailable.

## 5. Wait through TermAl fan-in

Call `termal_resume_after_delegations` with the created delegation ids and
`mode: "all"`. Report the wait id and child session ids, then end the turn.
Do not continue until TermAl resumes the parent with the fan-in prompt.

## 6. Verify the freeze and collect results

Before accepting reviewer output, run:

```bash
node scripts/review-freeze-fingerprint.mjs --check .git/engram-review-freeze.json
```

If it reports drift, stop: the reviewers did not inspect the current input and
the review must be restarted from the gates.

Fetch both structured result packets using `termal_get_session_result`.
Validated structured submissions are authoritative. If a submission is
missing or failed, report that reviewer as unavailable; never infer a clean
review from prose output.

Present:

```markdown
# Delegated Review

## Codex /review-code
- Status:
- Findings:
- Changed files:
- Commands run:

## Claude /review-code
- Status:
- Findings:
- Changed files:
- Commands run:

## Consolidated Action
- Critical/High:
- Medium/Low:
- Notes:
```

Deduplicate overlapping findings and tracker suggestions.

## 7. Record findings in Engram from the parent

Only after consolidation, search Engram for each actionable finding
(`engram work ls --search "<phrase>" --all`). Add an item only when it is
not already tracked (`engram work add "<finding>" --kind bug --label review
--priority <0 for Critical … 3 for Low> --under <item under review>`);
otherwise `note` the evidence on the existing item. Do not
complete implementation items here — record evidence and leave `done` to
the implementer. Informational notes need no tracker mutation.

Do not fix source files during this command. Review findings begin a separate
implementation iteration followed by fresh gates and review.
