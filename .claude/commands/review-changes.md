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
Engram. Delegate review only to the two `/review-code` children, both with
`writePolicy: readOnly`. A bounded test worker may execute the parent's gate
runner as described below; it does not become a reviewer or validation owner.

**Never commit, push, rebase, or sync remotes without explicit user
authority.**

This workflow requires TermAl MCP delegation tools. Attempt exactly two review
child spawns: one Codex and one Claude. Do not substitute platform subagents,
shell processes, raw HTTP, or nested TermAl review sessions for those reviewers.

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

### Completion-driven execution: do not babysit tests

Use an existing suitable runner or prepare a task-local runner for the exact
commands above, including the Windows substitution. This is a runner contract,
not a claim that `scripts/check.sh` supplies structured logging. Launch one
batch, retaining each command's full output and its exit code, start/end times,
and log path in a compact result file. Halt the remaining commands at the first
failure so the parent can investigate. Inspect the summary at completion;
read detailed logs for failures or a specific evidence question, not streams
of passing tests.

Keep the runner, logs and result files outside review input. Resolve a location
with `git rev-parse --path-format=absolute --git-path review-runs`, then use a
new run-specific subdirectory; never overwrite an earlier run's evidence.
Record the exact runner command, execution owner, completion handle and artifact
paths in the parent work status before yielding. A PID alone is not a durable
completion handle; use the host job/session identity and recorded start time.

Before launch, capture the current input with the existing freeze script's
`--write` into that run directory and retain its printed fingerprint separately
from the manifest. Record that literal and manifest path alongside the results.
Hold source and index unchanged during execution. At completion, and before
reusing recovered results, require `--check` to exit zero with stdout exactly
that saved literal plus LF. A mismatch or missing identity is an evidence gap,
not a current pass. These boundary checks do not prove the absence of transient
edits; a known intervening edit invalidates the run. This gate-input snapshot
is separate from the post-gate review freeze in section 3.

Use a completion notification or supported resume-on-completion mechanism, then
yield the turn. If none is available, wait on the same runner's completion using
a blocking tool, choosing its timeout from the last comparable run's duration
within the tool's and session's limits. Re-wait only when it returns unfinished;
do not substitute sleep-and-status loops. Do not repeatedly read growing logs,
narrate "still running", or spawn an agent merely to watch a process. Meaningful
updates report an outcome, a failure, or a decision needed.

The parent may ask a bounded execution worker to run the batch once and send
a completion message with the result/log paths. This worker is not a reviewer.
Specify the input and command, and prohibit source/index changes, duplicate
runs, automatic retries, and tracker writes. The parent remains responsible
for inspecting results, classifying failures, recording each gate once, and
freezing the reviewed input. The two read-only `/review-code` leaves never run
tests or gates.
Record worker execution as attributed evidence, naming the worker and result
file in a parent note and referencing its logs when recording the gates. Reading
those results does not turn them into parent-executed or host-attested checks.

After interruption or context recovery, recover the existing runner identity,
result file and completion state before launching anything. Do not start a
second batch because the first is quiet or its completion message was missed.
If a run's outcome cannot be established, report the evidence gap; do not call
it a pass. Rerun for a concrete correction, changed input, or stated diagnostic
question, never merely to chase green. These execution rules do not remove any
required gate or weaken assertions and failure investigation below.

On any failure, do not spawn reviewers. A failed gate is an investigation,
never a stop: classify every failing test or check in this same turn.
Record every executed gate on the focused open item you hold: `engram work
gate NAME` for a pass, or `engram work gate NAME --failed FAILURE --ref
opaque-reference` for bounded failure evidence. For a late gate on completed
work, any project-bound session records it with `engram work gate NAME
--work-ref REF ...`, without claiming or reopening the item. Bare `gate NAME`
always means pass; when a failed check has no test id, use the check command or
check name as its `--failed` label.

- Test or environment defect (wrong assertion, stale fixture, host
  contention, missing prerequisite): fix it in the current changeset, rerun
  the gates, and continue the review.
- Product defect: for open work, file one Engram child per defect with the
  failing test as its acceptance criterion (`engram work add "…" --accept
  "<test> passes" --kind bug --label gate --under <current item>`), mark the
  current item blocked on it when landing depends on it, and fix it now when
  it is in scope. For a late failed gate on completed work, record the gate
  against that item and file an independent root follow-up (`engram work add
  "Follow up the late gate failure" --accept "<test> passes" --kind bug
  --label gate`); never make completed work its parent or reopen it merely to
  file the finding.

End the turn with the classification of every failure (test name, cause,
action); "the suite failed" alone is not a report.

## 3. Freeze the review input

Run:

```bash
node scripts/review-freeze-fingerprint.mjs --write .git/engram-review-freeze.json
```

Keep the fingerprint printed by this successful `--write` in the parent's
review record, independently of the manifest file. Pass that exact value and
the manifest's absolute path to both reviewers as review context. Retain the
value until fan-in: a later run or another session can overwrite the manifest.
Do not replace this saved value with a fingerprint read back from that file.

The snapshot records the canonical Git worktree root and covers HEAD, the
index, tracked worktree changes, and untracked file contents. A check from
another worktree or repository refuses with both roots before comparing
content fingerprints. Running from a subdirectory still checks the whole
worktree. A manifest without a root is refused; create a new freeze.

Git index executable modes and symlink targets are covered on Windows too.
Untracked executable-mode changes and filesystem symlink properties remain
unverified on Windows; the checker reports that limitation on stderr, separate
from its fingerprint on stdout. Their filesystem tests run on other platforms.
The newline-filename test is also skipped on Windows because those names are
not valid there. These skips are not evidence of full Windows coverage.

Keep the manifest outside review input. The relative `.git` path above must
be used from the main worktree root, not a subdirectory. In a linked worktree,
`.git` is a file. From any worktree or subdirectory, resolve the manifest with
`git rev-parse --path-format=absolute --git-path engram-review-freeze.json` and
pass that absolute path to both `--write` and `--check`. Use an absolute path
to this script too when invoking it from a subdirectory. The invocation's
working directory still selects which worktree is checked.

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

Accept the check only when it exits zero and stdout is exactly the fingerprint
saved from this parent's own `--write`, followed by one newline. Never obtain
the comparison value from the manifest at check time. If the independently
saved value is lost, restart from the gates and create a new freeze and review.
A limitation notice on stderr is separate from that success output. On any
nonzero exit or unexpected stdout, do not
accept reviewer output. For a worktree mismatch, rerun the check from the
frozen worktree with its saved manifest. For an invalid or missing manifest,
create a new freeze through this workflow, restarting from the gates. If it
reports content drift, restart from the gates too: the reviewers did not
inspect the current input. Do not overwrite a failed check's manifest merely
to accept an earlier review.

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
(`engram work ls --search "<phrase>" --all`). A justified finding about the
scope this change modifies, Low included, is fixed in this slice before
completion, as the engram-repo skill (`.agents/skills/engram-repo/SKILL.md`)
requires. Only a problem that already existed and is unrelated to that scope
may be left for later.

- Fix an in-scope finding in this slice. Record the fix with a note on the
  reviewed item, or file it as a required child when a separate item helps track
  it: `engram work add "<finding>" --kind bug --label review --priority
  <0 for Critical … 3 for Low> --under <item under review>`. While another
  session holds the reviewed item, only that holder can add the required child,
  so a parent that does not hold it notes the finding on the item for the holder
  to fix.
- Never file an in-scope finding as an optional child, even when a refused
  required child suggests one: optional children do not block completion, so
  the item could close with the fix still open. An optional child is only for
  work intentionally completed inside the parent's execution window that is not
  a review finding.
- An existing problem unrelated to the changed scope, which this slice does
  not fix, is an independent root, even while the reviewed item is open. Use
  `engram work add "<finding>" --kind bug --label review --priority <0 for
  Critical … 3 for Low>` without `--under` or `--optional`, then `note` the new
  root with the reviewed item's reference and title, the review evidence, and
  why it lies outside the changed scope. Provenance belongs in that note, not in
  a parent or prerequisite edge.
- If a matching follow-up exists, note the new evidence and provenance on it
  instead of duplicating it. A match records provenance only; an in-scope
  finding is still fixed in this slice. Do not turn an existing child into an
  independent root by editing its history; use the explicit detach workflow
  separately when admitted.

Never add children to completed work or reopen it merely to record a finding.
When evidence rejects a filed finding, note that evidence and cancel with a
reason; if it is required, also record the parent's waiver with that reason.
Use `update CHILD --reject "why"` when admitted to compose those two effects
atomically; otherwise follow the conditional cancel/parent-waive remedy.
`done` is reserved for satisfied current acceptance, with the successful
receipt's visible criterion-count assertion and no-criterion-change disclosure.
An actionable finding left for later, which this step allows only for an
existing problem outside the changed scope, needs its independent work item
before closure; an informational observation that requests no action needs no
tracker mutation.

Consolidation itself records evidence, not source changes or implementation
completion. In pair work, when this writable parent is also the implementer,
continue directly into the next authorized implementation iteration without
waiting for another prompt: fix in-scope actionable findings, rerun the required
gates, freeze the corrected input, and obtain review of that input. Keep the
coordinator informed of material changes; pause for a real blocker, disputed
acceptance, or a decision outside the agreed scope or authority. A review-only
parent hands the findings to the implementer instead of assuming write authority.

After clean acceptance, the implementer records the delivered outcome and uses
`done` on owned implementation items when their acceptance and obligations are
satisfied. Review completion does not authorize Git or external actions. The
two `/review-code` children remain read-only, non-nesting leaves throughout.
