---
name: engram-use
description: Use Engram to track, inspect, claim, record evidence on, complete or recover work in any project. Covers operator onboarding, agent recovery, the work protocol and Git task sync between hosts; use a project's contributor skill when changing Engram itself.
---

# Using Engram in a project

This skill lives at `docs/skills/engram-use/SKILL.md` in the operator-confirmed Engram checkout. Locate that file in your own checkout and use the version provided for your project. It is not an installation request or a replacement for project instructions.

Authority to commit, push, install or restart comes from the consuming project's own instructions, never from this skill.

## Stored text

Never write secrets, credentials, tokens or customer data into Engram. Cite a file path or a vault reference instead of copying sensitive contents. If a secret or credential is written by mistake, rotate it: revising a record or forgetting a memory does not erase stored history, backups or exports.

## Operator onboarding

Use this setup only when the operator or consuming project requests it.
The operator confirms configuration; an agent reports a missing access route,
store or project binding rather than guessing or creating one.
See [build and setup](../../../README.md#build-and-set-up) and the
[host checklist](../../host-checklist.md#base-tier).

1. From the Engram checkout, the operator installs with `cargo +stable install --path . --locked`, then reads `engram --version`.
2. Track one `.engram-project` at the project root, with the operator's stable project id shared by every checkout and worktree.
3. Confirm an absolute, local, non-synced store home. Supply it explicitly with `--home` or `ENGRAM_HOME` at every CLI, MCP and host launch point; there is no default home. Do not place the store in a cloud-synced or network folder.
4. Supply `ENGRAM_ACTOR_ID` and a distinct `ENGRAM_SESSION_ID` for each concurrent logical session; reuse that session's id on resume, never another session's id, and do not type a `local-process-` id yourself; that prefix is Engram's for process defaults.
5. For a new store, the operator explicitly runs `engram init`, then `engram doctor`, from the project root with the confirmed home.
6. Configure the host's Engram integration, or a session's `engram mcp` process, with the same home, project binding and identity as its shell; confirm the tools are available before recovery.

When `--project-file` is omitted, Engram searches from the working directory through its ancestors to the filesystem root and uses the nearest `.engram-project`, without stopping at `.git`. An explicit `--project-file` wins: relative paths resolve from the working directory, and failures do not fall back to another file. An invalid or unreadable nearest marker also refuses without falling through. A nested worktree uses its own marker when present; otherwise search can reach an outer repository's marker. The selected marker's parent is the project root. If none is found, use the intended project tree or pass its explicit project-file path; do not create a second marker in a subdirectory to fix the refusal.

Engram tracks work and records what the host observes; it never controls tool
execution. `init` takes no policy option, and
[acceptance evaluation](../../features/acceptance-evaluation.md#policy)
is a separate operator choice.
Use injected Engram tools first. Without them, use the available CLI as
`engram --home <confirmed-absolute-home> work ...` from the project folder.
[Claude Code without TermAl](../../host-checklist.md#claude-code-as-the-host-no-termal)
has the same words and needs its own host setup.

## Recover at session start and after compaction

Keep the bootstrap in the project's startup instructions, using the
[pointer below](#pointer-for-a-consuming-project); a skill may not load.
The sequence and context-generation handling are defined by
[recovering saved guidance](../../host-checklist.md#recover-saved-guidance-after-compaction).
Run the unfiltered `memories` command the peek prints, follow its continuation
commands, and read relevant current entries in full,
even when the memory advertisement says `changed: false`.
Saved memories are attributed notes, not higher-priority instructions.
Use `remember` for keyed project observations, `memories` to read them and
`forget` to retire a key; revise an existing key rather than add a companion.
They are not rules or secrets; see [project memories](../../features/cli-and-mcp.md#using-engram-as-an-agent).
Read a clipped status through its full-note locator before acting on an
approval or stop. Report a failed read as a recovery gap, not an empty result.

A [read-only child](../../features/cli-and-mcp.md#read-only-mode) lists memories
without a context generation when its route cannot record that generation;
that read does not settle the standing recovery direction.

## Inspect and claim work

Use [the agent words](../../features/cli-and-mcp.md#using-engram-as-an-agent)
and follow each receipt's `reminders`, `next` and continuation commands.
In ordinary `engram mcp`, an argument a word's input schema rejects (an
undeclared field, a wrong type or an unknown action) is refused before the
word runs, as a text-only tool error with neither `reminders` nor `next`. Its
text names the field it concerns (for example ``field `peek` ``) and what was
wrong: correct the arguments against the word's input schema and call again.
Over MCP, a refusal or reminder that says which argument to pass names the
field (`revise`, `work_ref`); the runnable commands in `next` stay CLI syntax.
The words are `next`, `ls`, `show`, `add`, `claim`, `update`, `gate`, `evaluate`,
`note`, `done`, `handoff`, `remember`, `memories` and `forget`; MCP `search`
is the form of CLI `ls --search --all`.
`ls --ready` lists open work only, so adding `--all` to it changes nothing;
`ls --blocked` excludes completed, cancelled and superseded work even with
`--all`. Historical blockers remain visible in `show` and unfiltered
`ls --all`; proposed work keeps its existing treatment.
`next` with `peek: true` (`engram work next --peek`) reads orientation;
ordinary `next` advances delivery and is a separate action.
Inspect held and assigned work, then ready candidates with `show REF`.
`next` may also name stranded children beneath completed work this session
participated in. Read each blocked reason and its remedy; detach is suggested
only when admitted at that read. This advice changes neither focus nor claims.
Read the item's full acceptance and status before `claim REF` and execution.
For a lapsed claim, inspect the offered recovery command and record its reason.

`add` creates local work; `--under` makes a required child by default,
while `--optional` makes one that does not gate the parent.
Required children and prerequisites are dependencies, not provenance links.
See [claims](../../features/local-work-system.md#work-claims) and
[lifecycle](../../features/local-work-system.md#lifecycle-and-derived-readiness).

## Evidence, gates, review, evaluation and completion

Use [the work protocol](../../features/local-work-system.md#agent-native-protocol)
for the operation details. Record findings, decisions and changed duties or
waits as notes on the held item, once; use a status note for the next action.
Keep bulk logs in the project's permitted scratch location and cite them.
Write criteria an evaluator can judge from the repository and the item's own
holder notes; if another route supplied a decision, quote it with its source.

Run the consuming project's required checks. `gate` records a check result;
it does not run a check. Record failures as failures, investigate their cause,
and retain the command, result and evidence. A criterion bound to a host check
needs [host-observed evidence](../../features/acceptance-evaluation.md#reading-what-satisfied-a-bound-criterion),
not an agent note or a manually recorded passing gate.

Apply the consuming project's review and approval rules to its changes.
When the [evaluation policy](../../features/acceptance-evaluation.md#independent-by-default)
requires independent verdicts, the host or operator arranges an eligible
evaluator before completion; an executor's assertion is not that evaluation.
Follow the policy's source-freshness and evidence requirements.

`done` seals the delivered result in a
[CompletionSeal](../../features/local-work-system.md#completion-seal-and-report-assembly-claim).
If it refuses, resolve the specific owed action and follow its next command.
Copy any permitted landing values from command output: commit, remote,
branch, push time and, when installed, the reported build fingerprint.
Inspect the completion receipt; late evidence does not reopen its seal.

## Handoff and blockers

[Handoff](../../../README.md#hand-work-to-another-session) offers live work to
a real recipient session supplied by the host; the recipient accepts it.
Until acceptance, the offer does not transfer the holder's claim.
Use `update REF --blocked "reason"` for a concrete blocker, and record the
next action or wait in a status note. Inspect the offered unblock or recovery
command when the condition changes; do not invent a substitute operation.
Send Engram problems to Engram's maintainers and host problems to the host's
maintainers through the project's supported route, with the call and error.
Send a decision reserved to the operator through the project's agreed route.

## Synchronize project tasks between hosts

Git task sync is optional. Each host keeps its own canonical SQLite store and
exchanges task text with other hosts through one data-only orphan branch,
normally `engram-data`, checked out in the persistent `MAIN/.engram-data/`
worktree of the main code checkout. Exported data never goes on a code branch.
The contract is in [task planning text](../../features/task-planning-text.md)
and [Git task sync](../../features/git-task-sync.md); this section is the
procedure. Every `engram` command below runs from the main code checkout with
the confirmed `--home` and the project's marker, as in
[operator onboarding](#operator-onboarding).

This section grants no authority. Enabling sync, committing in the data
worktree and pushing the data branch follow the consuming project's own
instructions, which name the approved repository, branch and who may push.
Where they are silent, ask the operator and act only within the scope the
operator states.

### What is exchanged

- Exchanged: task planning (title, outcome, acceptance, kind, priority,
  labels, assignment, parent, prerequisites, blockers and reported lifecycle,
  completed and cancelled tasks included).
- Exchanged only when an export opts in: notes (`--include-notes`) and
  informational notices of who claimed or finished work
  (`--origin-machine ALIAS`). A notice is last-known state, not a lock.
- Not exchanged by this folder route: live claims, runs, sessions, grants,
  delivery state, evidence and verification records (gates, host checks,
  evaluations, observations) and private scratch.
- Not exchanged yet: project memories. Do not report memories as synced.
- Not filtered for secrets: task text is exported as stored. Native notes
  pass only a path and locator filter, and notes imported from another host
  are exported as stored. No filter detects secrets. A key, password or
  personal detail written into a task or an included note travels with it;
  the [disclosure check](#disclosure-check) is the safeguard.

An import creates planning only. An imported completion is a reported
outcome: it grants no local seal, acceptance pass or host-verification credit.

### Use planning-only exports

Notes and activity notices arrive with a task the folder adds. On a task both
stores already hold, a new or changed note or notice is an unsupported field:
the import refuses that task with reason `unsupported_fields` and exit `3`,
even when the same document reports the task's completion. A refused task is
not retried by itself. Routine exchange therefore uses planning-only exports,
with neither `--origin-machine` nor `--include-notes`. A planning-only export
captures no native activity, so claiming and releasing work does not change
it; activity already imported from another host still travels unchanged under
its original alias. `--include-notes` requires `--origin-machine`, so a
notes-included exchange always carries native activity as well.

Choose planning-only from the first publish. Import refuses changed or
removed activity entries on a task it already holds. Once an exchange
baseline carries a host's native activity, that host's switch to
planning-only exports, or to another alias, removes or changes those
published entries, so the other host refuses those tasks as well. Import
cannot yet update a notice on an existing task, so such an exchange stalls on
every task whose activity changes. Reconciling it needs the operator, for
example a fresh planning-only baseline on the receiving host.

Every export keeps the note selection of the store's last exchange baseline;
an export under another selection refuses. Changing the selection is a
deliberate re-baselining, not a routine step.

### Disclosure check

A data branch has its repository's visibility: in a public repository,
everything pushed is public and stays in Git history. Look for email
addresses, keys, tokens, passwords, connection strings, private host names,
IP addresses and private personal data, at two points in every flow:

- Before every commit in the data worktree: stage the folder with
  `git -C .engram-data add .engram` and read
  `git -C .engram-data diff --cached`, which shows new files and removed
  lines.
- Before every push: read every commit the remote does not have yet,
  removed lines and merge commits included. Right after
  `git -C .engram-data fetch origin engram-data`, read
  `git -C .engram-data log -p -m FETCH_HEAD..HEAD`; when the remote has no
  data branch yet, read `git -C .engram-data log -p HEAD`.

Keep notes off unless the operator has had them reviewed for this
destination. The export redactor filters nothing, and a completed item cannot
be revised. If the check finds sensitive text before a commit, do not commit
it. If a local commit already holds it, stop: neither push that commit nor
build on it. Either way, report it to the operator. Rotate any secret a push
has already exposed; `forget`, revising an item or a later export does not
remove text from an export already pushed.

### Keep the exchange state

Each host keeps two records privately, outside the data worktree, in a
scratch location the project permits:

- Its basis: a byte-for-byte copy of `.engram-data/.engram/project.json`,
  made with a file copy (`cp`, `Copy-Item`) right after its own export. Never
  take it from another host's export. Do not capture it through a shell
  redirect such as `git show ... > file`: Windows PowerShell 5.1 rewrites
  redirected output as UTF-16.
- Its imported commit: the data-branch commit this store is known to hold,
  namely its own export commit or a commit whose import finished with exit
  `0` on this host.

Before starting a refresh or an exchange, and before any push, check all
three of these:

- `git -C .engram-data rev-parse HEAD` is the recorded imported commit. An
  unborn data branch has nothing to import.
- No merge is in progress: `git -C .engram-data rev-parse -q --verify MERGE_HEAD`
  prints nothing.
- The data worktree is clean: `git -C .engram-data status --porcelain`
  prints nothing.

If a merge is in progress, abandon it with `git -C .engram-data merge --abort`,
then stop and report: committing over an unfinished merge records the other
host's commit as merged while discarding its edits. If the worktree is not
clean, stop and report its status. If HEAD is another commit (an interrupted
run, an import refused with exit `1`, or exit `3`), do not export, commit or
push on top of it. A fresh export there would silently
revert the other host's changes and, once pushed, make that host cancel the
tasks it added. Keep that commit and the old basis. Rerun the same import
with the same basis when the refusal was transient, such as a lock or I/O
failure. Otherwise stop and report to the operator; reconcile through the
re-merge the
[folder exchange](../../features/task-planning-text.md#git-folder-exchange)
describes, without writing an export over the unimported commit. See
[incremental push, pull and agent merge](../../features/git-task-sync.md#incremental-push-pull-and-agent-merge).

### Refresh the export

Export publishes a whole new `.engram` folder. It refuses
(`task_sync_destination_occupied`) while that folder exists with different
content, even when the folder holds only evidence packs or the `.gitignore`
and `.gitattributes` policy files. Those entries are not planning and must
survive a refresh, so the folder is removed and they are restored afterwards:

1. Pass the three checks in [keep the exchange state](#keep-the-exchange-state):
   HEAD is the recorded imported commit, no merge is in progress, and the
   data worktree is clean. Then list what HEAD tracks:
   `git -C .engram-data ls-tree --name-only HEAD .engram/`. Note every entry
   other than `.engram/project.json` and `.engram/tasks`.
2. `git -C .engram-data rm -r -q .engram`. If it fails or the folder still
   exists afterwards, stop and report `git -C .engram-data status`.
3. Export planning only: `engram task-sync export .engram-data/.engram`. If
   the export fails, restore the folder with
   `git -C .engram-data restore --source=HEAD --staged --worktree -- .engram`
   and stop.
4. Restore every entry noted in step 1 with
   `git -C .engram-data checkout HEAD -- <entry>...`, for example
   `.engram/evidence`.
5. Copy the new basis, as [kept above](#keep-the-exchange-state).
6. `git -C .engram-data add .engram`, then read
   `git -C .engram-data diff --cached --name-status`. It may list only
   `.engram/project.json` and files under `.engram/tasks/`; anything else
   means a step went wrong, so restore the folder as in step 3 and stop. If
   it lists nothing, the store has no change: skip the commit, and HEAD stays
   the imported commit. Otherwise run the disclosure check, commit, and
   record that commit as the imported commit.

### Publish from the first host

1. Opt in on this host, quoting the user's request:
   `engram task-sync enable --user-instruction "<the user's request>"`
   (MCP: `task_sync_settings` with `action: enable`). Enabling alone writes
   no task text.
2. The code checkout must ignore the data folder, through the project's
   `.gitignore` or `.git/info/exclude`. `data-init` adds the local ignore
   rule itself when it can. If it refuses because the data worktree is not
   ignored by the code checkout, add `/.engram-data/` to `.git/info/exclude`
   and run it again.
3. `engram task-sync data-init --branch engram-data` creates the locked data
   worktree on an unborn branch. It does not commit, fetch or push. Leave
   the worktree in place during ordinary cleanup.
4. If the remote already has the data branch, this is not a first host:
   [join a new host](#join-a-new-host) instead.
5. Export planning only: `engram task-sync export .engram-data/.engram`.
6. Copy the basis, as [kept above](#keep-the-exchange-state).
7. Run the disclosure check before committing, commit only `.engram/` in the
   data worktree, and record that commit as this host's imported commit.
8. Run the disclosure check before pushing. Push only as the consuming
   project's rules allow: fast-forward, never a force-push, rebase or history
   rewrite.

### Join a new host

Follow the consuming project's rules for creating or writing a store. The
operator runs `engram init` with the confirmed home, as in
[operator onboarding](#operator-onboarding).

1. After `engram init`, opt in with
   `engram task-sync enable --user-instruction "<the user's request>"`, then
   add the ignore line and run `data-init` as in steps 2 and 3 of
   [publishing from the first host](#publish-from-the-first-host).
2. In the data worktree, fetch the published branch into the unborn branch:
   `git -C .engram-data fetch origin engram-data`, then
   `git -C .engram-data merge --ff-only FETCH_HEAD`.
3. `engram task-sync import .engram-data/.engram --empty-basis --dry-run --json`.
   Read the receipt; a dry run writes nothing. Continue only if it exits
   `0`; otherwise stop and report.
4. Run the same import without `--dry-run`. On exit `0`, record the fetched
   commit as this host's imported commit, then run `engram readiness`.

The first import needs an empty destination. A host that already holds its
own tasks for the project needs reconciliation, not `--empty-basis`. Later
exports from this host keep the note selection of the folder it imported.

### Exchange in both directions

Another session on the same host that adds, edits or completes a task
between this host's export and its import makes the import refuse as
`task_sync_divergent_planning`, writing nothing. With several sessions
active, that refusal is likely; run the exchange when no other session is
changing this project's tasks.

1. Pass the three checks in [keep the exchange state](#keep-the-exchange-state).
2. [Refresh the export](#refresh-the-export). Afterwards, HEAD is again the
   recorded imported commit.
3. `git -C .engram-data fetch origin engram-data`. If the fetched commit is
   already in HEAD
   (`git -C .engram-data merge-base --is-ancestor FETCH_HEAD HEAD`), go to
   step 7. Otherwise run `git -C .engram-data merge --no-commit FETCH_HEAD`;
   a fast-forward needs no commit. Resolve any conflicts: keep each side's
   independent edits and decide overlapping task edits by meaning. Run the
   disclosure check on the staged result, then
   `git -C .engram-data commit --no-edit`. If you stop before that commit,
   for example because the disclosure check finds sensitive text or a
   conflict cannot be resolved, first abandon the merge with
   `git -C .engram-data merge --abort`, then report.
4. Check for removals. A task file missing from the folder is a removal:
   import cancels that task irreversibly, retires its id, and lists it in the
   receipt as an ordinary applied change.
   `git -C .engram-data diff --no-renames --name-status --diff-filter=D <imported commit> HEAD -- .engram/tasks`
   must list nothing, unless the operator has stated that removal is
   intended. Otherwise stop and keep the commit.
5. Dry-run the import with your own basis:
   `engram task-sync import .engram-data/.engram --basis <private>/basis.json --dry-run --json`.
   Never use another host's `project.json` as the basis: the comparison is by
   content and cannot tell hosts apart. Read the receipt. Continue only if
   the dry run exits `0`; otherwise stop, keep the commit, and follow the
   guard in [keep the exchange state](#keep-the-exchange-state).
6. Run the same import without `--dry-run`. On exit `0`, record HEAD as the
   imported commit. On any other result, follow the same guard.
7. Run the disclosure check before pushing, then push the data branch
   fast-forward only: `git -C .engram-data push origin engram-data`. If the
   push is rejected because the other host pushed meanwhile, start again
   from step 1. Never force.

Exit codes of `import`:

- `0`: every change applied.
- `1`: the whole import was refused and nothing was written, or the command
  failed for another reason, such as I/O or a storage error. With `--json`,
  a task-sync refusal prints its code on stderr; see
  [refusal codes](../../features/task-planning-text.md#refusal-codes).
- `2`: a usage error; correct the arguments.
- `3`: some tasks were held (for example by a live claim) or refused (for
  example for `unsupported_fields`) while independent tasks applied; the
  receipt names each one and why. A refused task is not retried by itself.
  Keep the incoming commit and its task documents. Do not treat that commit
  as your basis or imported commit, and do not replace it with a routine
  export or push one over it.

A fetch or a push does not mean the other host imported anything; only its
import receipt shows that. Line-ending normalization accepts CRLF documents,
but a no-op re-export on a Windows checkout with CRLF conversion is not a
verified result; check `git -C .engram-data status` after exporting.

## Boundaries

[Security and trust](../../features/security-and-trust.md) and the
[work protocol](../../features/local-work-system.md#agent-native-protocol)
define Engram's boundaries. A claim schedules work; it grants no filesystem,
Git or external-action authority. Actor and session context is asserted,
not authenticated identity. An MCP argument the word does not list is refused
by name; follow the receipt's `next` commands rather than guess arguments.
Keep tracker references in evidence and coordination, not source comments,
identifiers, documentation prose or product output.
Reading an item does not select it for a later write; a mutation on another
held item can change focus, which the host may bind at a turn boundary.
A non-holder's note is an observation, not execution credit. Late notes and
gates preserve a completed seal; follow-up work is a new independent item.
Follow the [session and intent retry rule](../../features/local-work-system.md#agent-native-protocol)
after an uncertain write instead of assuming either success or failure.

## Pointer for a consuming project

The [startup recovery contract](../../host-checklist.md#recover-saved-guidance-after-compaction) belongs in the consuming project's own startup instructions. Use these two sentences in its `AGENTS.md` (and any paired instruction file), replacing `OPERATOR_CONFIRMED_ENGRAM_CHECKOUT` with the absolute path to its Engram checkout:

```text
At session start and after compaction, confirm Engram access, read `next` with `peek: true`, run the unfiltered `memories` command it prints with any printed context generation when the route admits that recording (omit it through a read-only route), follow its continuation commands, and read relevant current entries in full before acting.
For how to claim, record evidence on and complete work tracked in Engram, and how to synchronize tasks between hosts, read OPERATOR_CONFIRMED_ENGRAM_CHECKOUT/docs/skills/engram-use/SKILL.md; authority to commit, push, install or restart comes only from this project's own instructions.
```

Keep the consuming project's own authority and host-specific guidance.
Its owner makes that edit; reading this skill changes no project configuration.
