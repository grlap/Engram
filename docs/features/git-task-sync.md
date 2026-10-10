# Git task synchronization

> Normative contract: [specification §3.2](../spec.md#32-optional-git-task-sync).
> Related: [local work](local-work-system.md), [SQLite](sqlite-store.md),
> [work-graph snapshots](work-graph-snapshot.md), and
> [off-host backup](off-host-backup.md).

Design selected on 2026-10-09; not implemented. This replaces sequential
single-writer handoff as the selected personal cross-machine workflow.
Previous portable writer-epoch, release/acquire, dedicated-ref and scheduled
push proposals are not prerequisites for this feature. Local SQLite remains
canonical on each machine. Divergent work is allowed; an external coding
agent merges task text alongside code. Engram never calls a model.

## User contract and Git layout

Sync is optional and explicitly configured. Without configured export/sync,
Engram neither creates nor changes `.engram/`; all local work continues and
there is no background activity. A remote is unnecessary for local-only work.
Enabling export/import or sync, choosing
the repository/branch, pushing and pulling are visible deliberate actions.
Disabling sync stops exchange and leaves the local database usable and writable.
It does not delete remote history. No release, ownership transfer, startup
remote check or online lease is required. Separate machines may execute the
same task; merging records cannot undo duplicate external effects.

Use the same ordinary branch as code, not a separate data branch or plumbing
ref. The proposed versioned interchange layout is:

```text
.engram/
  project.json
  tasks/
    <full-task-uuid>.json
```

`project.json` identifies the project and exact supported interchange format,
never a machine or store identity. Task UUIDs travel unchanged; short display
refs are derived, not alternate identities. Import refuses an ambiguous short-ref
collision explicitly rather than merging distinct UUIDs.

Use pretty-printed UTF-8 JSON with LF, a final newline, stable key ordering,
one scalar field per line and multiline arrays with one element per line
(structured elements are themselves pretty-printed). Sort set-valued fields
deterministically; preserve meaningful order in acceptance and notes. Equal
shared state produces equal bytes. This is agent-editable interchange, not
compact RFC 8785 canonical-object serialization.

Enablement adds a path-scoped `.engram/** text eol=lf` Git attribute alongside
the narrow ignore allowlist. Import accepts CRLF JSON by normalizing line
endings to LF before interchange comparison; exports always emit LF. This
changes neither stored canonical object bytes nor record identities.
Line-ending normalization applies only to interchange document line endings;
decoded JSON string content, including escaped CR/LF, is preserved. It does
not change Git commit provenance or the host's existing source/freeze identity
rules.

Each task document represents its current shared
state: stable task id, title, description, acceptance criteria, planning status,
kind, priority, labels, assignment, defer-until, parent/child requirement and
prerequisite ids, blockers, supersession/disposal, machine-qualified work-activity
notices, and shared notes with stable
note ids. Preserve outcome text and explicit per-item acceptance-evaluation mode
as planning policy; importing it never imports an evaluation result or waives
a host-bound requirement.
The field schema is a typed allowlist; it is not arbitrary database-row export.
The note inclusion and new-note rule below preserves immutable original notes.
Git records document history;
the document is not an immutable canonical object and its task id is not a
canonical version id. A fingerprint compares content; it identifies no task.

The initial profile exchanges task planning and shared notes, including closed
tasks. An imported completion is attributed as a reported outcome; it must not
manufacture a local completion seal, acceptance pass or satisfied host obligation.
Runtime authority and historical evidence are separate from editable planning
state. If an import would alter a live run's governing planning basis, core
admission must invalidate/reconcile that basis or refuse the affected batch;
import is not a bypass around lifecycle rules. The lifecycle mapping below
governs terminal/reopened state; concrete typed
encoding and its tests must be specified before implementing import.

Live claims, sessions, grants, retained prompts, delivery cursors, private
scratch, credentials, machine-local path bindings and runtime tables are
excluded. Foreign-machine provenance remains machine-qualified; it never
rebinds a local session, source root or host-verification credit. Shared note text
is explicitly published content, not proof of automatic secret removal.
Enabling exchange records the chosen repository's public/private visibility
and intended readership as operator assertions. Unknown disclosure scope is
refused rather than assuming code readers may read every work note.

Note bodies are opt-in per project. Only explicitly selected generic work-note
and status-note families are eligible, with original note id, author, time and
origin-machine provenance. Gate, host verification, environment, evaluation,
execution-observation, retained-prompt and source-root/path-binding records are
never note-export candidates. Notes containing locators or machine/user-profile
paths are excluded by default, with visible omission counts/reasons; opting in
to notes is not permission to export those excluded records. Detection rules
must be explicit and tested, not advertised as comprehensive secret scanning.
Changing the selected profile requires explicit re-baselining so omitted local
notes are never interpreted as remotely deleted.

Restricted-labelled notes and task-field content are excluded by default,
with visible omission counts and reasons. Widening requires an explicit,
attributed change of the disclosure profile and re-baselining; opting in to
a note family does not widen sensitivity access. A `secret-ref` is carried
only as the labelled reference body; export never dereferences it. Enablement
and each export visibly report the configured redactor's status, including
that the V1 `DevelopmentNoopRedactor` filters nothing. Family/path filtering
and sensitivity labels do not establish automatic secret removal. Git history
persists across clones and reflogs, so exported content cannot be reliably
recalled.

An omitted restricted field is not a deletion or an empty replacement on
import. If the supported interchange schema cannot represent that omission
while preserving required fields and reference closure, refuse the affected
export with its reason; never emit a partial task as a complete final state.

An agent-changed note is a new note, not a version or edit of the original.
Keep the original unchanged, and prepare a new attributed note citing its
original id and origin machine. The agent supplies a new note without an id;
Engram's candidate preparation mints its fresh id before the resolved text is
committed. Existing imported ids are preserved; arbitrary new id-bearing draft
notes without prepared/imported provenance are refused, not accepted as ids the
agent may invent. This is structural provenance, not authenticated authorship.
Every importing machine
preserves the same shared identity. The importing session records its local
import event separately with source-commit provenance; no new local receipt id
or timestamp is serialized back into the shared note. An altered original id/body
pair is refused. An unchanged imported note retains its shared attribution.

Project memories and complete native execution-history transfer are outside
the initial task profile; unsupported data must be reported, never silently
presented as a complete backup. Existing backup/migration formats remain separate.

The initial profile has no task-deletion operation or invented tombstone:
cancelled and superseded tasks remain documents with explicit terminal states.
`forget` applies to memories, which are outside this profile. A missing task file
is not deletion; refuse unexplained removal, including a partial checkout/input.
A later deletion design must add a local source event and explicit tombstone
before exchanging deletions. References resolve against the complete resulting task
graph, and dependency/parent cycles are refused. Unknown fields/versions,
duplicate or mismatched ids and unresolved conflict markers are refused.

## Imported lifecycle and local execution

Shared task status describes planning and reported outcomes. Native execution
records remain local evidence, never editable task-document fields. Apply the
following mapping after the agent resolves conflicts and core admission passes.
Unlisted incompatible combinations refuse or enter explicit reconciliation;
they never fall through the compatible-completion case:

| Local situation / incoming final state | Import result |
| --- | --- |
| No existing task / open | Create the planning identity; readiness follows local graph and policy. No claim, session or grant is created. |
| No existing task or only imported history / completed | Retain an attributed inert completion record, display the task as reported completed, exclude it from ready work, and allow its completed planning status to satisfy an ordinary prerequisite. It conveys no native verification credit. |
| Existing native unfinished run / terminal state or incompatible governing basis | Require explicit local import reconciliation: hold new affected-run admission, drain/checkpoint in-flight activity, release the claim with an attributed reason, and append a superseded-by-import disposition retaining the run's evidence. Then apply reported planning state without fabricating successful native completion. If safe reconciliation cannot finish, retain the candidate and refuse this attempt; the state is not permanently unimportable. Independent shared notes may proceed if they do not change admission basis. |
| Existing native completed run / same completed outcome | Preserve the immutable seal and its exact original basis; record compatible shared changes as later attributed versions, never revise the seal or claim that it covers those later changes. |
| Existing completed task / reopened final state | Require an explicit locally admitted reopen with a new execution generation before applying the resolved open state. Preserve old completion and evidence as history, without crediting the new generation. |
| Cancelled/superseded final state | Keep the task document and immutable history, reconcile any unfinished local run as above, and enforce explicit required-child disposal/waiver and remaining graph-reference rules. File removal is never a waiver. |
| Missing document / existing task | Refuse the incomplete/removal candidate; preserve the local task. The initial profile has no task tombstone. |

For required-child accounting, use the existing explicit distinction between
native seals and inert restored completions: an imported reported completion
can be recorded only as `restored_child_completions`, never as a native child
seal; the existing report-assembly restriction on such ancestry remains. It
cannot satisfy a typed host-verification obligation. Cancelled/superseded
children still require the ordinary explicit disposal/waiver rules.
The superseded-by-import run disposition is a new admitted local transition to
implement and audit, not permission to silently release a claim through current
generic import code. No global host drain or remote ownership transfer is needed.
Dependent evaluations can become stale after an admitted change; hosts must
expect the normal basis-moved/refusal result rather than credit old evidence.

Imported outcome attribution is stable shared data. Local receipt timestamps,
newly minted local import-event ids, machine-local run ids and derived display
labels never rewrite the shared document during re-export. Re-export of an
unchanged imported tree must be identical, including reported completion and
cancelled/superseded status. Concurrent disagreement about terminal/open state is given to the
agent; the importer applies the chosen result only through the above rules.

## Remote-agent collaboration through TermAl

The selected workflow also supports a cooperating remote agent. An explicitly
configured remote-session/control route coordinates agents; Git carries the
task projection. TermAl already has SSH remote sessions and routes ordinary
operator prompts to them. Agent mailbox participation by remote-backed sessions
is a separate planned host capability: its remote-access design keeps mailboxes
on one control-plane host and relays calls and wakes for that host's SSH
executor remotes. That eligibility/relay implementation must land and the actual
sender/receiver route must be verified before enabling these notifications.
It does not provide peer-to-peer mailbox federation between two independent
control-plane hosts; such hosts need a separately supported notification route.
With Git alone, remote activity becomes visible on the next explicit fetch,
not through polling or an assumed immediate message. The remote agent
claims a task in its own machine's Engram, explicitly syncs its task data, then
sends the local peer a notification naming project, configured branch,
published commit and task id. The local peer fetches, merges its pending task
and code changes, and imports the resolved tree in its main checkout. Neither
notification receipt nor fetch alone means import succeeded.

Actual claim ids/fences, grants, session bindings and execution authority remain
local. To make the remote claim visible, include a dedicated informational
work-activity projection in the task document, as a map keyed by stable origin
machine id, then by activity id, rather than an append-to-one-array layout.
Sort those keys deterministically. Each origin updates only its own reported
activity entries; peers preserve other origins' entries when merging. This
reduces incidental text conflicts, but ordinary Git conflicts still go to the
agent. Each entry contains stable activity id, task id,
asserted actor, origin-machine identity and its reported started/released/finished
state. The exporting machine prepares this from local claim/lifecycle events;
it contains no live claim token or usable grant. Activity is a typed shared
coordination field, not a generic note, so it works even with note export off.
It is reported remote state, not authenticated proof or local verification.

Starting, releasing, abandoning or finishing local work must make this task
eligible for incremental export. Every activity has its own stable identity;
a release/finish refers to that activity rather than clearing every actor's
activity on the task. Preserve source-local ordering/provenance for successive
states of that activity; do not order different machines by wall clocks.
Concurrent activity on two machines remains visible for agent reconciliation.
Imported notices never become new local claim events or re-export as a fresh
activity from the importing machine.
Claim renewal without a shared activity-state change produces no activity
update or Git commit. Expiry without an explicit recorded release/abandonment
likewise exports no invented event: the notice remains last-known state and
is not evidence of a currently valid lease.

After import, the local agent sees "being worked on by <actor> on <machine>"
and coordinates with that peer instead of routinely duplicating the task.
This is a scheduling signal, not a global lock: disconnected machines can both
claim before either sees the other's publication. A notice is last-known state
as of its source commit, never proof that the remote agent is still running.
Stale or competing activity requires peer/user reconciliation; do not silently
expire it into a claimed global ownership guarantee. Shared task completion
still uses the lifecycle mapping above, not the activity notice as evidence.

Notify after publication is confirmed; an unknown push is not a published-state
notification. Duplicate or delayed notifications cannot roll the local tree
backward: require the notified commit to be reachable from the fetched tip of
the already configured remote branch; otherwise ignore/report that notification
as outside the current authorized branch history. Reconcile against the retained
base/current tip and acknowledge
application only after a successful import receipt. A notification is a hint
to fetch from an already configured authorized target, never permission to
trust an arbitrary URL, command or task-body instruction in a message. A local
agent may perform the pull/merge under an explicit user action or a deliberately
enabled bounded remote-collaboration workflow. No polling/background network
is introduced for projects that have not enabled it, and the sender does not
grant the receiver new Git or host authority.
The peer routes exchange to the main-checkout sync session; a coding session
in a linked worktree does not import there. Notifications cannot bypass the
existing main-checkout gate/turn serialization, and activity notices are tracked
task bytes subject to the same frozen-input rules as every task-file update.

## Main checkout and linked worktrees

One main checkout per local project is the synchronization workspace. It owns
task-file export, import and conflict resolution on its selected code branch.
All local sessions and linked worktrees keep using the same SQLite project
store, selected by project identity. No database is created per branch.

Linked worktrees normally omit `.engram/` from their materialized files and do
not export/import/edit it. Use per-worktree cone-mode sparse-checkout with
`extensions.worktreeConfig`: create with `git worktree add --no-checkout`, set
the per-worktree sparse selection, then perform checkout (or explicitly enroll
an existing clean worktree). Select the
code directories while excluding `.engram/`, preserve top-level files, and
refresh the selection when top-level code directories are added. Cone mode
uses positive directory selection, so a new top-level directory is absent until
that selection is refreshed; it is not an automatic "everything except" rule.
The main checkout stays
unsparsified. This is checkout exclusion, not `.gitignore`; the subtree remains
tracked and its absence must never be staged as deletion.

Sparse checkout is not an access boundary: merge/rebase conflicts can materialize
excluded task files in the linked worktree. The selected workflow refuses task
import/export there and preserves the conflict. With ordinary Git permission,
abort that operation and perform integration/task resolution in the main
checkout; do not claim an ongoing merge can move between worktrees.
If the user instead explicitly resolves it in place, that is an exception to
the ordinary worktree workflow and still does not import into SQLite there.
Reapply sparsity only after the conflict/edits are safely settled. This behavior
follows the [Git sparse-checkout manual](https://git-scm.com/docs/git-sparse-checkout);
supported Git clients/workflows still require implementation conformance tests.
The existing `.engram-project` identity marker remains in every worktree.

This repository currently ignores `/.engram/` as local state. Enablement must
explicitly replace that blanket rule with a narrow tracked allowlist for
`project.json` and `tasks/`, keeping all other local contents ignored and never
staging them through a broad force-add. Existing local files occupying reserved
paths cause refusal until an explicit relocation is agreed; setup never
overwrites or automatically publishes them. No ignore/configuration changes
are made before opt-in, and this design edit does not change `.gitignore`.

The main export includes changes from all local worktrees. Consequently task
data is a project-wide snapshot carried on the chosen code branch, not a claim
that every task change belongs to that branch's code. Switching branches does
not automatically replace the database or change sync progress. A deliberate
branch rebind must reconcile its tree with current local state; it cannot treat
another branch's cursor as its own or silently roll the project backward.

## Finding local changes

Keep durable local sync progress bound to project/store identity, repository
and branch. It records independently:

- the named project-feed position exported through, and its recoverable local
  Git commit;
- the Git commit whose task tree was last successfully imported;
- the retained common task-tree base used for three-way reconciliation;
- any prepared operation needed to retry without losing local changes.

These are local bookkeeping, not shared Git payload. A feed cursor is valid
only in its named local store/feed; it is never compared to another machine's
cursor. Replacing/restoring the store requires explicit re-baselining when
that identity or cursor basis cannot be established.

The existing project feed is the starting point for change collection. Read
the bounded interval after the export cursor through a captured head, collect
affected task ids, deduplicate, and serialize their final state from that same
SQLite read snapshot. Twenty edits to one task produce one task-file update.
Use structured event-to-task mapping, not note-text searches or wall-clock
timestamps. Every exported field, eligible note, activity and relation must participate
in change coverage transactionally. Implementation must prove that coverage;
missing families require extending the change index before shipping, not an
assumption that every current feed entry already provides it. An unavailable
cursor interval requires an explicit full comparison/re-baseline.

Imported changes may appear on the local feed, but an identical serialized
task tree produces no new Git commit. An import must not advance an export
cursor past unrelated, unexported local changes. Unchanged output, including
excluded runtime-only events, may advance progress only with a durable mapping
to the already committed task tree.

## Initial push: a project with 1,000 tasks

For a configured empty remote branch:

1. Capture all 1,000 tasks and the project-feed head in one SQLite read
   snapshot. Include terminal tasks, opted-in eligible notes and graph links
   in the selected profile; report anything outside that profile.
2. Produce `project.json` and the task documents deterministically in the main
   checkout. Capture its branch, Git task tree, task-path index/worktree state
   and recorded reconciliation basis. Preserve and reconcile every unconsumed
   task-tree change, including clean committed changes not yet imported as well
   as staged/unstaged edits. Validate the entire exported graph.
3. Create one ordinary Git commit with the initial task tree. A commit may
   include intentionally selected code changes, but sync never stages unrelated
   user files implicitly. No agent needs to read 1,000 tasks to export them.
4. Record that the captured local feed position is represented by this
   recoverable commit. If interrupted between commit creation and bookkeeping,
   recovery verifies the prepared operation before advancing the cursor.
5. Push that commit. A failed or unknown push keeps the commit and retries or
   checks the remote; it does not discard exported data or assume publication.
   New SQLite writes after the read snapshot stay pending for the next export.

If the remote already has a task tree, first fetch and reconcile it. Initial
push never replaces it by force. Git history starts with the chosen current
task projection; this does not replay every historic local event into Git.

The second machine fetches the initial commit, validates the full task tree
and imports the 1,000 task identities in a transaction, then records the
imported commit. Existing local tasks require reconciliation rather than
replacement. An empty destination gets no copied live execution authority.

## Incremental push, pull and agent merge

For push, collect changed local task ids since the export cursor and update
only their documents after reconciling the current Git task tree with the
recorded base and SQLite snapshot. A clean Git status does not establish that
the committed task tree was imported: never overwrite a committed title change
with a stale database title while exporting a separate priority change. Bind
the outgoing candidate/receipt to the captured task tree and task-path
index/worktree state; recheck them and branch identity before publication of
that candidate. A changed basis is preserved and reconciled, not overwritten.
Unrelated code edits need not invalidate a task-only candidate. Retain a durable
commit before advancing local export progress. Git handles transfer of commits
and ordinary non-fast-forward
refusal. A rejected push fetches and reconciles the competing commits; this
workflow does not require force-push or remote writer ownership.

For pull, fetch and inspect committed task-tree differences against the last
imported commit. Diff the old and new trees, not just the final commit's patch:
this includes all intervening commits and merge commits. A fetched commit is
not an imported commit. The main checkout preserves any pending task-file and
SQLite edits before Git integration. Export pending local task changes to a
recoverable candidate before resolving overlaps, or capture equivalent local
text and its exact read basis without advancing export progress.

If the imported commit is not an ancestor of the incoming tip, do not assume
that changed ancestry authorizes replacement. Reconcile using the retained
common task tree and both current sides. If that base is unavailable, require
an explicit full re-baseline with all local state preserved; never guess a
base or force-push. The successful receipt records the newly resolved commit.

Provide the agent with base, local and incoming documents for overlapping
changes. Distinct ids and one-sided changes can be carried forward mechanically;
the agent resolves meaning, including terminal/open and graph conflicts, and
writes final task documents alongside its code merge. It can inspect any
affected dependencies, not just the files with textual conflict markers.
Engram validates the resulting graph and applies the selected final state.
The resolved task tree must be committed; import progress names exactly that
commit, including a locally created merge commit, not an arbitrary fetched tip.

Apply import and advance its local receipt/cursor atomically. Match each
affected task's captured local basis under the write transaction; if a local
agent changed that basis while the merge was prepared, preserve the candidate
and require renewed reconciliation. In that same write transaction revalidate
the complete resulting relevant graph, target lifecycles and live-run admission,
including the union of parent/required-child and prerequisite edges. An unchanged
A row is insufficient if candidate A→B races with local B→A; connected tasks
are not unrelated merely because they are absent from the imported delta.
Equivalent fencing must cover the full relevant read set. Truly unrelated
local changes need not block the batch. Repeating the same committed result
is idempotent. Append attributed
versions/events to existing immutable history and update current projections;
never overwrite canonical objects. No partial graph or premature cursor
advance is visible after refusal or crash. Git publication and SQLite import
are separate durable steps, so interrupted/unknown results are recovered from
their recorded candidate and receipts rather than a fictitious atomic
Git-plus-SQLite transaction.

## Data commits and source landing policy

Task data can share a commit with deliberately selected code, but automatic
export normally prepares a path-scoped data-only commit. It must not stage
unrelated code/index changes. When a project serializes source landings,
serialize data-branch updates with that same coordinator/branch critical
section and recheck its head immediately before committing/pushing. Never
move the branch underneath a source freeze or claim an old review covers a
new tree. A data-only change can still invalidate a whole-tree fingerprint.

Generated data exchange should have a project-approved validation path distinct
from source-code delivery: schema/graph validation and reviewed sync tooling,
not a fresh Rust build and code-review pair per 1,000-task export. This is a
policy integration prerequisite, not an exemption granted by this document.
The current Engram authority rules still govern; an unattended data-only
exception requires their separate exact-wording approval route before enabling
it. Without that exception, obey the existing gates/authority or retain the
prepared output for a separately authorized operation. Mixed source/data commits
remain source changesets. No commit/push, force-push or test waiver is authorized
merely by the presence of a sync configuration or this specification.

## Host binding and validation identity

Import is an admitted local planning mutation. Engram distinguishes compatible
metadata/notes from changes to the run's governing criteria, blockers or status;
field names alone do not prove semantic compatibility. Compatible revision
reconciliation retains the existing claim id; a changed governing basis uses
explicit invalidation/reconciliation, never an imported or silently replaced
claim. The lifecycle table's terminal reconciliation is a separate explicit
transition, with ordinary claim release consequences. Exact revision/fence
mapping is part of that transition's implementation contract.

Changed bindings reach sessions through the host's existing refresh/rebind path
at the next admission. A begun turn keeps its original grant binding until
checkpoint/settlement. Retained prompts and uncertain begin/evaluate requests
retain their prepared identity; import cannot clear them, rewrite them or claim
they were delivered. If incompatible import cannot wait for safe settlement,
it refuses with its candidate retained. No automatic host Stop is authorized.
Held/read responses name the source commit, changed planning families, affected
items/sessions and when the new basis applies. Refusals name the required local
reconciliation, not a generic host-unavailable error. Imported notes never mint
host verification or source-observation credit or rebind a named source root;
an explicit local claim release still has its ordinary root-lifetime effect.

Preserve existing validation identities. Materialized tracked task-file changes
affect `content-v1`; a HEAD-only change need not affect that content identity,
but the current review freeze also includes HEAD and therefore changes. There
is no `.engram/` exclusion from source coverage, the watcher or whole-input
review. Frozen detached source roots remain unchanged until explicit integration;
do not claim evidence carries when their input changes. Imported planning and
evaluation freshness is checked separately even if a detached source root is
unchanged. A future non-source data profile needs a new versioned coverage
contract and its own approval; it cannot redefine existing `content-v1`.

For TermAl, gated code roots stay in separate detached worktrees. Serialize
main-checkout exchange with main-checkout Git operations and wait while an
affected live turn or carried gate is rooted there; inform that session before
its input changes. Ordinary source invalidation remains visible with a named
cause. This is a local exchange boundary, not a global host pause or remote
single-writer protocol.

## Implementation and validation boundary

This brief does not ship commands, change existing admission policy, or prove
schema compatibility. Local progress/receipt storage and import mapping need
concrete schema design; apply the existing full-store migration rule if they
change the durable store format. Do not promise no migration merely because
the exchanged representation is JSON. No global writer guard, remote epoch,
host release/acquire or TermAl restart is inherent in this selected design.

Required implementation cases cover: initial 1,000-task export/import;
incremental repeated edits and no-op re-export; every field/note/activity/relation
change family; two independent offline writers; agent-resolved same-task and
terminal/open conflicts; graph validation and completion provenance; concurrent
local edits during merge; interrupted commit/import/push with exact retry;
non-fast-forward remote changes; clean committed incoming edits followed by
local edits or a crash before import; A→B/B→A concurrent graph changes; every
lifecycle-table case; divergent branch rebinding; and linked
worktree exclusion without staged deletions or lost task-tree changes.
Also cover restricted omission, explicit widening and re-baselining;
`secret-ref` non-dereference; no-op redactor status at enablement and export;
and Windows `core.autocrlf` checkout with no-op re-export. Restricted
required-field omission is refused without clearing the local value;
escaped CR/LF is preserved through the Windows no-op round-trip.
Also cover remote claim → confirmed push → notification → local fetch/merge/import,
release/finish updates with notes disabled, concurrent remote/local claims,
stale and out-of-order notices, unknown push, notification commits outside the
configured branch history, unchanged renewals, source-separated activity maps,
and no transfer of live authority. Verify the configured host notification route
and its supported topology separately; Git-only discovery remains available.

The acceptance comparison must establish that an initial import followed by
incremental export produces the same task projection, without feedback-loop
commits. Test terminal-task semantics and excluded live authority separately.
These are future implementation proofs, not results of reviewing this design.
