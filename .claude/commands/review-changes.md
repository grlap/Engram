---
name: review-changes
description: Run Engram quality gates, freeze the worktree, and obtain two independent reviews from different vendors (Codex and Claude, with Kimi standing in for an unavailable Codex).
metadata:
  termal:
    title:
      strategy: default
---

Review all staged, unstaged, and untracked changes from the existing writable
parent session.

**Do not delegate `/review-changes` itself.** The parent owns build artifacts,
quality gates, the worktree freeze, fan-in, and recording findings in
Engram. Delegate review only to `/review-code` children: the pair and, when
the parent commissions it, the optional third review below, each with
`writePolicy: readOnly`. A bounded test worker may execute the parent's gate
runner as described below; it does not become a reviewer or validation owner.

**Never commit, push, rebase, or sync remotes without explicit user
authority.** That is Greg's word or, for commit, push and install only, the
standing approval in AGENTS.md "Authority and Git".

This workflow requires TermAl MCP delegation tools. Obtain the review pair —
at most two reviews that count, from different vendors: spawn one Codex and
one Claude; when Codex is unavailable (a usage limit or outage met in this
round, with its refusal text recorded on the item), spawn Kimi in its place
with the title `Kimi /review-code`. The stand-in replaces the Codex reviewer,
whether its spawn was refused or its result was lost to Codex's usage limit or
outage, so a round never has more than two reviews that count. An unavailable
Claude reviewer is reported as unavailable; no stand-in replaces it. A round
that ends with fewer than two reviews that count reports the missing reviewer
as unavailable, spawns no further reviewer, and does not satisfy the standing
approval's review condition. Do not substitute platform subagents, shell
processes, raw HTTP, or nested TermAl review sessions for those reviewers.

Kimi may give an optional third read-only review (Greg, 2026-09-29, 'mozemy
trzymac Kimi jako 3 reviewer'a, decyzja dla rady obu projektow, moze byc
niezalezna'; Engram's council chose optional on that date). The parent may
commission it beside the pair on a round's frozen input; it is recommended for
the first round of a changeset touching authority text, acceptance-evaluation
enforcement, storage or migration, or a new subprocess or external surface,
and decided case by case for a security-sensitive changeset, because the host
cannot gate Kimi's network tools. It is spawned with the title
`Kimi /review-code (optional third review)` and waited for through its own
fan-in, apart from the pair's. It never counts toward the two reviews the
standing approval requires, its absence or failure never blocks a landing, and
none is commissioned on an input whose round has finished. Before landing it
has returned, failed or been cancelled; the parent may cancel it to land. A
justified in-scope finding of Medium or higher from it is fixed and reviewed
again by the pair like any other finding; a justified Low or Note is handled
as the standing approval's review condition describes. A finding of Medium or
higher the parent refutes on evidence is recorded on the item with that
evidence and goes to Greg, as the standing approval's review condition
requires; showing it to the pair first is optional.

Greg's 2026-09-26 decisions also cover the work around each review:

- Before coding an item, run a design review with Engram::Fable and a
  read-only Codex explorer, covering edge cases and real host (TermAl)
  behavior. Run it during the previous item's gate or review. (coordinators'
  decision of 2026-09-29, advisory) When Codex is unavailable (a usage limit
  or outage met at that time, recorded on the item with the refusal text), a
  read-only Kimi explorer stands in for it, decided case by case for a
  security-sensitive item, because the host cannot gate Kimi's network tools.
- During an item's gate or review, do only non-mutating work on the next
  one: no worktree or index edits, and no claim or other change of this
  session's focus. Reading, planning and a read-only design review stay
  allowed. When two or more code items are ready, a TermAl worker session
  with its own claim and worktree implements the second, because one session
  has one focus.

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

Use `node scripts/test-launcher.mjs full` to run, in order:

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
scripts/test-rust.sh
node --test scripts/review-freeze-fingerprint.test.mjs scripts/test-launcher.test.mjs
node --test scripts/mcp-dogfood.test.mjs
node --test scripts/control-dogfood.test.mjs
node --test scripts/parity.test.mjs
node scripts/check-doc-links.mjs
```

On Windows, run `pwsh -NoProfile -File scripts/test-rust.ps1` in place of
`scripts/test-rust.sh`; it runs the same ordinary and scale-test
phases without the Unix-only file-descriptor-limit adjustment.

### Documentation-only changesets

A changeset whose every changed path is a `.md` file (docs, AGENTS.md,
CLAUDE.md, skills, commands) runs the link check and a byte comparison of
AGENTS.md and CLAUDE.md instead of the full batch (Greg, 2026-09-26): no
build step or test reads Markdown except the link checker. Run each through
the launcher, so that its input fingerprint is checked at completion as for
the full gate and can be compared with the saved review freeze:

```bash
node scripts/test-launcher.mjs focused -- node scripts/check-doc-links.mjs
node scripts/test-launcher.mjs focused -- git diff --no-index --exit-code AGENTS.md CLAUDE.md
```

Both reviewers still review it. A changeset with any other path runs the
full gate.

Choose by file type: `.md` prose, agent instructions, skills and review
commands use these two checks; executable tooling or examples, test-only
changes, runtime code, CLI implementation and schemas use the full gate.
A Markdown code block or quoted command is prose and does not trigger the
full gate by itself. Even a comment-only change in a `.rs` test uses the full
gate. Mixed or uncertain changes use the stronger applicable checks; focused
correction checks never replace landing validation.

### Gate and review in parallel

Run the foreground launcher under the host's background execution (for example
a background shell task that reports when it exits), then freeze the review
input (section 3) and spawn both reviewers (section 4) on the same input while
the gate runs (Greg, 2026-09-26). The launcher refuses `--detach` without a
different session to notify, so `--detach --notify PARENT_SESSION` is only for
a bounded worker that delivers completion to the parent. The yield or blocking
wait described below comes under section 5, only after the reviewers are
spawned. A host without background execution cannot overlap them: it runs the
gate first, then sections 3 and 4. For the same tree, the gate's
`expectedFingerprint` and the review freeze print the same value. A failed
gate discards the reviews of that input. When a review finding will change the
input while the gate still runs, the gate's result can no longer be used: the
parent stops or discards it and starts the next round on the corrected input.
It notes which stages finished and on which fingerprint, and does not record
them as gate passes for the corrected input.

### Completion-driven execution: do not babysit tests

Use the maintained `scripts/test-launcher.mjs` entrypoint, including its Windows
substitution; `scripts/check.sh` forwards to its full mode. See
[launcher usage](../../docs/development.md#test-launcher) for focused checks,
detached root-worker delivery and notification-only retries. Launch one
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

The launcher captures the input using the existing freeze implementation in
`input.json`, saves the expected value in `request.json`, and checks it before
execution and at completion. Before yielding, the parent retains that exact
`expectedFingerprint` literal in its own work status, alongside the run directory
and manifest path. Foreground and detached launch receipts print it for this
purpose, before waiting for completion.
Do not recover the comparison value from the run's files at completion.
Hold source and index unchanged during execution. At completion, and before
reusing recovered results, require the existing freeze script's `--check` to
exit zero with stdout exactly the parent-held literal plus LF. A mismatch or
missing independently retained identity is an evidence gap, not a current pass.
These boundary checks do not prove the absence of transient
edits; a known intervening edit invalidates the run. This gate-input snapshot
is separate from the review freeze in section 3, although both cover the same
tree.
Tracked content follows Git's clean/eol normalization, so changes erased by
that conversion (including line-ending-only edits) are not detected by the
fingerprint on any platform. Do not claim raw-byte coverage for tracked files.

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
freezing the reviewed input. The read-only `/review-code` leaves, the optional
third review included, never run tests or gates.
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

On any failure, the reviews of that input no longer count. A failed gate is
an investigation, never a stop: classify every failing test or check in this
same turn.
Record every executed gate on the focused open item you hold: `engram work
gate NAME` for a pass, or `engram work gate NAME --failed FAILURE --ref
opaque-reference` for bounded failure evidence. For a late gate on completed
work, any project-bound session records it with `engram work gate NAME
--work-ref REF ...`, without claiming or reopening the item. Bare `gate NAME`
always means pass; when a failed check has no test id, use the check command or
check name as its `--failed` label.

- Test or environment defect (wrong assertion, stale fixture, host
  contention, missing prerequisite): fix it in the current changeset and
  start a new round on the corrected input: gate, freeze and both reviewers
  in parallel.
- Product defect: for an in-scope defect on open work, record the failed
  check, diagnosis, correction and verification on the held item. Create a
  required child when separate ownership, independently scoped work or a real
  dependency warrants it, with the failing test as its acceptance criterion;
  block landing on that child when necessary. A small correction stays on the
  held item. Every in-scope defect must still be fixed before completion.
  Track a pre-existing defect outside the changed scope as an independent
  root, with its evidence and provenance. For a late failed gate on completed
  work, record the gate against that item and file an independent root
  follow-up (`engram work add
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
the manifest's absolute path to both reviewers, and to the optional third
review when commissioned, as review context. Retain the value until fan-in: a
later run or another session can overwrite the manifest. Do not replace this
saved value with a fingerprint read back from that file.

The snapshot records the canonical Git worktree root and covers HEAD, the
index, Git-normalized tracked worktree changes, and untracked file contents.
Git clean/eol conversion can erase tracked-byte differences; this is not
raw-byte coverage or proof against transient edits. The freeze CLI reports
this normalization limitation on stderr on every platform, separately from
the exact fingerprint on stdout. A check from
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

## 4. Spawn the two reviewers

Use `termal_spawn_session` once per reviewer from the current parent, each
with mode `reviewer` and `writePolicy: readOnly`:

1. Codex, title `Codex /review-code`.
2. Claude, title `Claude /review-code`.

When Codex is unavailable (a usage limit or outage met in this round, with
its refusal text recorded on the item), spawn Kimi in its place with the
title `Kimi /review-code`. The optional third review, when commissioned, is
spawned the same way, with its own title, and gets the same prompt.

Give each a multi-line prompt. Its first line tells the reviewer to read
`.claude/commands/review-code.md` in the worktree and follow it. The rest
gives the review context: the item, the input (files and HEAD), the
manifest's absolute path and the saved fingerprint, and what changed since the
previous round. Do not put that context after `/review-code` on one line:
TermAl expands a one-line slash command from the command file, which has no
arguments slot, so everything after the command is dropped.

Fix rounds keep both reviewers (Greg, 2026-09-26: a fix for a Low can turn
into a High). The brief may point at the change since the previous freeze,
but both reviewers review the whole input.

If one spawn fails after the other succeeds, continue waiting for the created
reviewer. When the failure is Codex's unavailability (a usage limit or
outage), record the refusal text on the item and spawn Kimi in its place on
the same freeze. For any other failure, Claude's included, or when the
stand-in fails too, report the missing reviewer as unavailable.

## 5. Wait through TermAl fan-in

Call `termal_resume_after_delegations` with the pair's delegation ids and
`mode: "all"`. A commissioned optional third review gets its own wait; its
fan-in never leads to section 6 by itself. Report the wait id and child
session ids, then end the turn.
The gate's completion may resume the parent first; handle it under section
2. On a gate failure the reviews of that input no longer count: cancel the
running reviewers or discard their results. Do not continue to section 6
until TermAl resumes the parent with the pair's fan-in prompt.

## 6. Verify the freeze and collect results

Accept reviewer output only once the gate has finished and passed, and its
input fingerprint at completion equals the saved review-freeze fingerprint.
For a changeset touching only `.md` files, that means once the link and
identity checks passed on that tree. When the reviews arrive first, verify
the freeze as below. They may be read, and a justified finding may start the
next round at once, but a clean review counts only once the gate has passed
on the same fingerprint.

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

Fetch both structured result packets using `termal_get_session_result`, and
the optional third review's once it has returned. Validated structured
submissions are authoritative. If a submission is missing or failed, report
that reviewer as unavailable; when the cause is Codex's unavailability met in
this round, record the refusal text on the item and, if no stand-in has been
spawned this round, spawn Kimi in its place on the same freeze, as section 4
describes, then wait for it through section 5 and fetch its result before
presenting; otherwise report the reviewer as unavailable. When the stand-in's
own submission is missing or failed, report that reviewer as unavailable
instead of spawning again. Never infer a clean review from prose output.

Present:

```markdown
# Delegated Review

## <vendor> /review-code
- Status:
- Findings:
- Changed files:
- Commands run:

## <vendor> /review-code
- Status:
- Findings:
- Changed files:
- Commands run:

## Consolidated Action
- Critical/High:
- Medium/Low:
- Notes:
```

When the optional third review was commissioned, add a section
`## Kimi /review-code (optional third review)` marked as not counting toward
the pair, and consolidate its findings with the pair's.

Deduplicate overlapping findings and tracker suggestions.

Before a landing of authority text (AGENTS.md, CLAUDE.md, or any instruction
or command file that grants or limits commit, push, tracker or approval
authority), the project's coordinator reads the final frozen diff line by
line against the sentences whose concurrence is recorded on the item and
notes on the item which recorded message governs each changed passage; a
passage no recorded concurrence quotes whole is concurred before the freeze
or taken out. Where the coordinator wrote the change, the other project's
coordinator reads the final frozen diff in its place.

## 7. Record findings in Engram from the parent

Only after consolidation, search Engram for each actionable finding
(`engram work ls --search "<phrase>" --all`). A justified finding of Medium
or higher about the scope this change modifies is fixed in this slice before
completion, as the engram-repo skill (`.agents/skills/engram-repo/SKILL.md`)
requires. Only a Low and a problem that already existed and is unrelated to
that scope may be left for later; a Note needs no action.

- Fix an in-scope finding of Medium or higher in this slice. Record the fix
  with a note on the reviewed item. Create a required child only when
  separate ownership, independently scoped work or a real dependency warrants
  it:
  `engram work add "<finding>" --kind bug --label review --priority
  <0 for Critical … 3 for Low> --under <item under review>`. While another
  session holds the reviewed item, only that holder can add the required child,
  so a parent that does not hold it notes the finding on the item for the holder
  to fix.
- Never file an in-scope finding as an optional child, even when a refused
  required child suggests one: optional children do not block completion, so
  the item could close with the fix still open. An optional child is only for
  work intentionally completed inside the parent's execution window that is not
  a review finding.
- A Low or an existing problem unrelated to the changed scope, which this
  slice does not fix, is an independent root, even while the reviewed item
  is open. Use
  `engram work add "<finding>" --kind bug --label review --priority <0 for
  Critical … 3 for Low>` without `--under` or `--optional`, then `note` the new
  root with the reviewed item's reference and title, the review evidence, and
  why it is left for later. Provenance belongs in that note, not in
  a parent or prerequisite edge.
- If a matching follow-up exists, note the new evidence and provenance on it
  instead of duplicating it. A match records provenance only; an in-scope
  finding of Medium or higher is still fixed in this slice. Do not turn an
  existing child into an independent root by editing its history; use the
  explicit detach workflow separately when admitted.

Never add children to completed work or reopen it merely to record a finding.
When evidence rejects a filed finding, note that evidence and cancel with a
reason; if it is required, also record the parent's waiver with that reason.
Use `update CHILD --reject "why"` when admitted to compose those two effects
atomically; otherwise follow the conditional cancel/parent-waive remedy.
`done` is reserved for satisfied current acceptance, with the successful
receipt's visible criterion-count assertion and no-criterion-change disclosure.
An actionable finding left for later, which this step allows only for a Low
and an existing problem outside the changed scope, needs its independent
work item before closure; an informational observation that requests no
action needs no tracker mutation.

Consolidation itself records evidence, not source changes or implementation
completion. In pair work, when this writable parent is also the implementer,
continue directly into the next authorized implementation iteration without
waiting for another prompt: fix the in-scope findings the standing rule
requires and start a new round on the corrected input, with gate, freeze and
both reviewers in parallel. Keep the
coordinator informed of material changes; pause for a real blocker, disputed
acceptance, or a decision outside the agreed scope or authority. A review-only
parent hands the findings to the implementer instead of assuming write authority.

After clean acceptance, the implementer records the delivered outcome and uses
`done` on owned implementation items when their acceptance and obligations are
satisfied. Review completion does not authorize Git or external actions. The
`/review-code` children remain read-only, non-nesting leaves throughout.
