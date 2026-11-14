# Task planning text

The explicit directory profile is the first implemented slice of
[Git task synchronization](git-task-sync.md). It exchanges current task
planning, explicitly selected shared notes and informational machine activity
between isolated SQLite stores. It does not configure Git, reconcile an existing
graph or enable background work.
Full native history remains in [graph snapshots](work-graph-snapshot.md) and
[store migration](full-store-migration.md), separate formats with different scope.

## Commands and directory layout

Select a project with its ordinary `.engram-project` marker and an explicit
host-local store home. Ordinary export and import accept only `PROJECT/.engram-data/.engram`
in that project's dedicated orphan data worktree. A relative spelling is allowed;
parent traversal, linked directories, another checkout or branch, and a folder in
the code checkout refuse before export capture or import reads. Initialize each
local store with `engram init` and provision its data worktree explicitly with
`task-sync data-init` before using these commands. `--branch` selects the data
branch and defaults to `engram-data`; it must match the observed worktree.

```text
engram --home HOME --project-file PROJECT/.engram-project task-sync export PROJECT/.engram-data/.engram
engram --home HOME --project-file PROJECT/.engram-project task-sync export PROJECT/.engram-data/.engram --include-notes --origin-machine host-A
engram --home HOME --project-file PROJECT/.engram-project task-sync import PROJECT/.engram-data/.engram --json
engram --home HOME --project-file PROJECT/.engram-project task-sync import PROJECT/.engram-data/.engram --empty-basis
engram --home HOME --project-file PROJECT/.engram-project task-sync import PROJECT/.engram-data/.engram --basis OWN_EXPORT/project.json
engram --home HOME --project-file PROJECT/.engram-project task-sync export PROJECT/.engram-data/.engram --rebuild
engram --home HOME --project-file PROJECT/.engram-project task-sync import PROJECT/.engram-data/.engram --rebuild --basis OWN_EXPORT/project.json
```

Each host selects its own main-code marker, store home and dedicated data
worktree for the same stable project id. The ordinary cycle is: export, commit
the folder in the data worktree, merge the other host's data branch and commit
the merge, then import. Both commands are incremental by default: they write or
read only the tasks that changed since this host's last export or import, so
their cost follows the change, not the size of the folder. `--rebuild` is the
explicit complete route; each receipt names its `route`.

A folder holds `project.json` and `tasks/<full-uuid>.json`, beside ordinary
`.gitignore` and `.gitattributes` policy files that export leaves alone.
`project.json` holds three members. `project` is the planning document:
`format: "engram-task-planning-v1"`, `project_id`, and `notes_included: true`
for an opt-in note export (otherwise the field and task note arrays are
absent). `profile` is the disclosure profile the export used: `include_notes`,
which both hosts share, and the exporting host's own asserted `origin_machine`
alias, or `null`. `folder_exchange` marks the folder `complete` and carries its
`incremental` membership basis: the order-independent `inventory_root` and
`task_count` of the task files and the exporter's own project-feed `cut`. A
complete export also carries `export_basis.inventory_fingerprint`, the ordered
content fingerprint of every document, which an incremental export cannot keep
without reading every task. A build that knows only the fingerprint refuses
the membership basis rather than misreading it. Activity is independent of note
selection. There is no store identity or runtime cursor.

## Git folder exchange

`task-sync export` and `task-sync import` are the folder route; no other
command exchanges this folder profile. Private snapshots may be retained under
`target/` for merge bases; they are not accepted as public import/export paths.

Each export and import records a folder head per project and note selection:
the folder it left on disk (its membership root and count), the project-feed
cut it reflects, the last data commit an import applied completely (the next
import's diff base), the commit the folder held after an import, and the tasks
an import held. Recording a head checks, in the same transaction, that its
predecessor is still current, so a concurrent export or import refuses as a
moved head; the remedy is to export or import again. Heads are a rebuildable
projection over immutable records.

**Export.** The first export of a store, and `--rebuild`, write the complete
folder: every exported task, with every other task file removed. Otherwise
export first checks that the folder on disk is still the one the head
records: unchanged since the import's commit, or carrying the envelope the last
export wrote. It then takes the tasks the project feed changed since the head's
cut, reads the old file of only those tasks, writes only those files and
updates the membership root by arithmetic. When nothing changed it writes
nothing and reports `route: "unchanged"`. A folder that is not the recorded one
refuses as `task_sync_incremental_basis_unavailable`: commit or restore it, or
run `--rebuild`. An export under another profile than its head's, or after the
project feed lost its anchor, refuses the same way and names `--rebuild`. The
export journals its exact file operations and its next head in the data
worktree's Git directory, records the head, and only then touches the folder;
the next export finishes an interrupted publication whose head was recorded and
drops one whose head was not.

**Import.** With a head that names an applied commit, import is incremental. It
requires the folder committed in the data worktree (otherwise
`task_sync_commit_context_required`) and reads, through Git, only the task
paths changed between that commit and the worktree's HEAD. That commit must be
an ancestor of HEAD: a force-moved branch refuses as
`task_sync_source_history_diverged`, and a shallow clone that cannot prove it as
`task_sync_source_history_unavailable`; both name `--rebuild`. The changed
tasks are admitted one by one inside one transaction; a local change since the
head to any task the admission reads refuses as divergent planning: export and
commit it, merge, then import again. An incoming commit equal to the recorded
one, with nothing held, reports `route: "unchanged"`.

The complete route, the first import of a store or `--rebuild`, reads the whole
folder against a destination basis that the caller selects:

- `--basis FILE` names this host's own `project.json` from its export before
  the Git merge, for example as written by
  `git show PINNED_DATA_COMMIT:.engram/project.json`, retained privately before
  editing the data folder. This command observes Git only to validate the data
  worktree; it does not resolve the basis from `ORIG_HEAD` or any other ref.
- `--empty-basis` imports into a destination that holds no shared planning yet.
  The import checks inside its write transaction that the destination is empty.

Selecting both is a usage error; selecting neither on the complete route
refuses and names both. The folder's own embedded basis is never used for this
comparison. A basis must belong to this project and share the folder's note
selection (`include_notes`). The origin-machine alias is host-local: each host
exports under its own alias, so a folder from another host may carry a
different one. Import compares the destination's current planning, exported
under the basis's own profile, with the selected basis inside its write
transaction; any local planning change since that export refuses as divergent
planning, and nothing is written. A basis written by an incremental export
carries no fingerprint; it is compared by membership root and count instead.

The basis is a content comparison, not an authenticated host identity. Equal
bytes from another host cannot be told apart from this host's own, so
selecting this host's own copy is the caller's obligation. A basis with a
different inventory, project or note selection refuses.

A successful import records the applied folder as the exchange baseline. Each
baseline record holds only what changed since the one before it: the task
documents that joined or changed, and the members that left. It also names its
predecessor. Writing it checks, in the same transaction, that the predecessor
is still the project's latest baseline. The store keeps a rebuildable
per-task view of the latest baseline, so reading one task's baseline never
replays the history. That view also carries an order-independent content
fingerprint of the whole membership, which each record updates for exactly
the tasks it changes; projection repair rebuilds both from the records in
feed order. An exact repeat of the same import, by the same actor and session against the
same basis, replays its first receipt and records nothing more. `--dry-run`
performs the whole import under the same identity, with every check, replay
and refusal, then rolls it back explicitly: no task, baseline, receipt or feed
entry remains, and a later real import checks everything again. Its receipt
reports `dry_run: true`; a dry run of an already committed import replays
that import's receipt, as the real repeat would.

The CLI exits `0` only when no incoming task is held or refused. If any task
is unapplied, both import and `--dry-run` exit `3`, distinct from a whole-import
refusal (`1`). Stdout still contains the complete JSON receipt, including all
applied changes and each held/refused task's reason. Stderr names the task
files that need attention. Applied changes stay applied; dry run writes nothing.
On exit `3` the head keeps the earlier applied commit as its diff base and
records the held tasks. Their incoming bytes stay in the folder: no export, the
complete route included, rewrites a held task's file, and a complete export
restores one that went missing from the commit the import read. An export
refuses while a held task also changed locally. Resolve why each task was held
here, for example by releasing the claim, then import again: the next import
reads the held tasks again from the earlier commit. Only a complete import
advances the diff base.

A later export keeps the note selection its baseline was recorded with; an
export under another note selection refuses.

Candidates written by `task-sync prepare` carry no exchange envelope, so
`task-sync import` refuses them. Setup materialization retains that raw candidate
privately and publishes a folder-exchange envelope around the project document;
task document bytes stay unchanged. The plain-candidate library calls remain
for tests only. Import requires `project.json`, rejects
unknown entries and symbolic links, and checks every task filename against
its UUID. A folder with no tasks may omit its `tasks` directory, since Git
keeps no empty directories. Import reads the resolved folder's inventory from
its files: a merge that edits a task leaves the embedded fingerprint stale,
and Git cannot regenerate it. A task file missing from a resolved folder
therefore reads as the omission of that task. On a destination that holds an
exchange baseline, omitting a task the last applied exchange held is a
deletion subject to admission, as the next section describes. The reader and
exporter limit each file to 1 MiB, the tree to 128 MiB, and the task count to
100,000. Task graph limits also apply.

Export writes pretty UTF-8 JSON, LF and a final newline. Typed field order and
set ordering are deterministic. Acceptance criteria retain their authored
order. CRLF JSON is accepted and compared after decoding; escaped CR and LF
inside string values remain unchanged. Duplicate JSON members, unknown fields
at any depth, unsupported profiles and unresolved conflict text are refused.

### Edits on an enrolled destination

Once a destination holds an exchange baseline, import compares the folder with
the destination's current planning, task by task, even when that planning is
empty. The first folder import uses the same admission with empty shared
membership; it has the same per-task outcomes. A task the destination holds outside the last applied exchange, which the
folder omits, is host-local: it stays as it is and does not join the
exchange.

A task the folder adds is restored exactly as the
empty-graph import restores one: its UUID, an import origin and restored
record, its prerequisites and blockers, and its notes and activity when the
selection shares them, with no event and no run, claim or root execution. A
completed addition keeps its reported completion, without a seal. An edit of
a task's title, outcome, acceptance criteria and portable acceptance bindings,
priority, labels or assignment is recorded as an ordinary planning revision
under project authority, attributed to the importing actor. Assignment can be
cleared; record-id acceptance pins still refuse before mutation.
[Prerequisite and blocker changes](task-folder-relations.md) use their existing
planning transitions in the same transaction, alongside scalar edits. Each
added or edited task then exports exactly as the folder carries it, and an
added task joins the exchange. An eligible task omitted from the last
exchange is locally cancelled by an ordinary `Disposed` event, with no run
or other runtime state. Other unsupported fields, including structural moves,
produce a per-task refusal with their field names. A note selection that
differs from the baseline's refuses earlier as `task_sync_divergent_planning`,
or `task_sync_invalid_graph` when the folder and requested selection disagree.

Edits and returned completions currently require the task's shared notes and
activity notices to match the destination's retained values. New or changed
notes or activity on an existing task are reported as unsupported fields,
including when the same document reports completion. A planning-only exchange
omits native notes and activity; importing a new task can restore both under
their selected disclosure profile. Reporting completion never copies notes
already retained by the destination into a second inherited history.

Creation host does not control editability. Native and restored Open or
Proposed tasks accept shared planning when they have no engaged local
execution. A live unexpired claim holds an incoming change as `active_run`.
An initial dormant run does not hold it, nor does a released or expired claim;
old execution and planning history remain unchanged. A completed task keeps
its `restored_completion` refusal and other terminal states keep `lifecycle`.
A required-child waiver does not by itself hold a later description edit.
Execution of another task does not block a scalar edit. Relation edits also
check the [affected ancestor chain and root](task-folder-relations.md), where
engaged execution holds that relation change. Claims and runs stay host-local.

A reopened, unclaimed run is dormant too. A live handoff offer holds incoming
changes until it is resolved or expires. Prerequisite changes require an Open
owner; a Proposed owner's prerequisite edit is refused individually before
the ordinary writer runs. A reported completion of an existing ancestor is
held while a descendant has live execution authority, and refused while a
required child would remain Open or Proposed after admitted changes. Reporting
a child complete leaves an open root's aggregate available for its siblings.
Reporting the root complete ends its aggregate even if optional children retain
dormant runs; reopening after disposing unfinished descendants allocates the
next execution generation.

A task takes an addition only when nothing the destination holds
contradicts it:

- Every identity it brings is new to the store (`id_collision`): its UUID,
  held in any project or by a task the exchange retired; its short reference,
  within the project; its blocker ids, including cleared ones; its shared note
  ids; and its origin-qualified activity ids. Like a note id, an activity id
  that names any record the store holds collides, whatever machine alias it
  is reported under: this host reports its native activity, in any project,
  by the id of the event that recorded the claim.
- A record-id pin in its acceptance bindings refuses before anything is
  written (`acceptance_pin`).
- Existing parent, prerequisite and replacement endpoints may be native or
  imported. Native endpoints are bound to their original canonical creation
  snapshot and its prerequisite set, or their original restored record. No
  mutable current snapshot or synthetic import origin replaces that history.
  Only the outgoing native creation closure needed by retained or incoming
  references joins the origin graph; unrelated native history stays outside
  it. A native closure that would invalidate the immutable origin graph
  refuses the incoming tasks that introduce it, preserving independent work.
  Missing endpoints and invalid origin bindings still refuse.
- Its ancestors can take unfinished work, and its root has no active
  execution (`ancestor_binding`). A held ancestor must itself be free to take
  an edit, and a held ancestor whose own change in the folder is refused,
  such as a move to another parent, refuses the additions beneath it. A
  blocker that came in through an import does not refuse an
  addition, since blockers are planning, not execution. Past native execution,
  revisions and relation transitions do not themselves bar decomposition.
  A simultaneous ancestor relation change is held for a later fresh import.
  Under an existing local completed ancestor, only a finished addition may
  join, and only when that ancestor's completion was restored: a native seal
  accounts for exactly its named children. An entirely new incoming subtree
  preserves its reported lifecycle, including open descendants of cancelled,
  superseded or completed remote ancestors. The first folder import accepts
  the same reported state; it creates no run, claim or completion proof.
  A root is engaged while any task under it has engaged local execution;
  dormant runs do not engage the aggregate.
- An addition that depends on a refused addition refuses too, naming it:
  through its parent chain as `ancestor_binding`, through a prerequisite or
  replacement as `refused_reference`.
- The graph the import would leave, every current task the destination keeps
  after admitted removals plus the additions, still forms a valid task graph
  (`retained_graph`). A host-local
  task the folder omits can close a cycle that neither the folder nor the origin
  history holds.
- The store's whole import-origin history, with the additions, still forms a
  valid task graph (`origin_graph`). An origin keeps the structure its task
  was imported with, so an edge the current planning no longer holds can
  still close a cycle there.

For additions without existing-owner relation edits, each graph check first
validates its graph without the additions; a graph that
fails on its own refuses at folder level and names no addition. With the
additions, it names exactly the additions the failure involves, looking for
each kind in the order the validator checks them: a task holding a reference
to a missing task, an open descendant of a root over its limit, a member of a
dependency cycle (a required child for its parent, a prerequisite, a
replacement), or, in a graph that is otherwise valid, an addition whose root
there differs from the folder's. A task on a path between two cycles is on
neither and is not named. Each named addition gives its kind of failure.
Additions in one strongly connected component of the dependency graph, one
cycle structure that may hold several cycles, or under one root over its
limit, are named as failing together. An addition that depends on a named one is set aside
before the check goes on, and refuses for that dependence: through its parent
chain as `ancestor_binding`, through a prerequisite or replacement as
`refused_reference`. A failure that no addition accounts for refuses at folder
level, as the additions together making the graph invalid, never by blaming a
task. A composed relation change instead checks the complete patched graph,
including retained local tasks, before materializing removals or additions;
the [relation contract](task-folder-relations.md) also checks durable retired
edges separately from the immutable origin union.

Existing tasks' notes and activity are compared exactly, so an unchanged note
never blocks an addition. Admission judges the requested task changes together,
with one reason per task in task-id order. The receipt's `tasks` list gives
`applied`, `held` or `refused` for every changed task, plus `reason` and `detail`
for each unapplied task. Unsupported fields and a task's own eligibility come
before ancestor, dependency and graph reasons. Every refused task remains at
its local value while independent admitted changes apply. A missing refused
addition holds its dependents; an existing refused task remains a real local
endpoint and does not hold all tasks that reference it.

The final mixed graph must remain valid. Applied incoming documents advance
the exchange baseline; unapplied members retain effective local support,
including their current relations, so stale shared edges cannot form a cycle
with admitted changes. Their incoming values remain pending. A fresh-basis
import after execution ends retries those values without a separate durable
queue. An import that applies changes retains its partial receipt for exact
lost-response replay; a no-change partial result stays retryable on the same
basis and records no baseline, including on a first import. A completed task left unchanged gains no
event. A host-local task now carried unchanged can join the baseline, making
its later omission a deletion subject to admission. Foreign-key checks and
the final import-origin audit remain transaction rollback guards: a storage,
writer or integrity error rolls back every write, rather than becoming a
per-task refusal. Malformed whole-folder input and profile/basis mismatches
still refuse before application.

This partial application rule belongs to complete-folder import. The strict
delta ledger still defers or refuses a whole candidate when any selected task
is held or refused; it never writes a partial delta receipt or advances source
membership/progress for part of that candidate.

A removal requires an Open task, checks engaged execution and additionally
refuses any persisted run (`execution_history`). Proposed tasks may receive
scalar edits but cannot be removed. A
task-sync-restored member completed and sealed on this host refuses as
`sealed_completion`; inert imported completion
refuses as `restored_completion`. Every
affected ancestor must be eligible and the root must have no active execution
(`ancestor_binding`), including execution started by a sibling. A surviving
shared or host-local task that still names the removed task as parent,
prerequisite or replacement refuses the removal (`retained_reference`, naming
both tasks and the field). When several retained tasks name the same removed
task, the refusal names the first retained referrer in this order. First come
the current tasks other than the admitted removals, in task-id order. They
include every removal held back before this check, for its own eligibility
or for an ancestor binding, even when a retained task also names it; a
removal held back for an ancestor binding then still reports
`retained_reference`, since its binding is only applied after the check.
Then come the removals first held back during the check, because a retained
task names them, in the order they are found. These checks use retained documents after admitted prerequisite removals;
an unsupported structural edit in the folder cannot hide a reference. A refused
removal keeps its outgoing references, which can refuse further removals.
Malformed input graphs still refuse earlier as `task_sync_invalid_graph`.
Per-task reasons precede graph reasons; for a removal, a surviving reference
precedes an ancestor-execution conflict. An unrelated executing root does not
block removal. The remaining graph is validated even with no additions.

Old prerequisites and blockers are removed first, then additions are restored,
scalar edits and new relations apply, and children-before-parents removals
finish the transaction. Removal retains the task row, original parent/root
and relations, import origins, notes/activity and all prior records. The
event carries the importing actor and time, with every runtime snapshot
absent. It creates no claim, run, root execution, grant, contributor, waiver
or completion. The new complete exchange baseline excludes removed ids;
their durable history and identities remain reserved. Older retired tasks'
historical edges do not block subsequent retirement waves. A Cancelled
document retained in the folder stays a member; omitting an already terminal
member refuses under the conservative lifecycle boundary. Delete-all leaves
an enrolled empty baseline, so later additions receive full enrolled admission
and a retired id cannot be resurrected.

An incomplete checkout or input can omit an eligible member while leaving a
valid graph. Import then cancels that task irreversibly and permanently retires
its id from the exchange; adding the file back refuses as `id_collision`.
Inspect `--dry-run` before applying a folder, especially a partial input.

Removing a required child preserves the parent's required-child barrier.
The parent's ordinary completion refuses until a project-bound session
records an explicit attributed waiver through the existing waiver operation.
Import never records that waiver or fabricates a child seal. Optional disposed
children also keep their ordinary completion handling. Dry run executes the
same transaction and rolls it back; exact receipt replay never reapplies a
removal over later work. Projection repair rebuilds only declared disposable
projections and preserves durable cancellation history and old receipts.

A task created on A may be imported, claimed, executed and completed on B
while disconnected, then returned to A. A returned Completed document with a
reported summary can close an eligible Open or Proposed local task. A canonical
`CompletionReported` event links its inert restored record, retaining the
reported author, time and summary separately from the local importing actor.
An existing non-engaged run is cancelled, without a seal. Reporting the root
also cancels its aggregate after admission excludes live descendant authority;
reporting a child leaves the open root's aggregate available. No acceptance pass, host
verification credit, contribution or obligation satisfaction is fabricated.
Local obligations and prior execution history remain recorded. Doctor and
projection repair validate the exact event-to-record link; full-store
migration preserves those immutable identities.

Import carries shared state rather than making the immediate sender its
author. B's outcome relayed through C remains B's outcome, and C's independent
planning edits remain visible when A imports it. Concurrent edits must be
resolved against the last common data commit by Engram and the agent. The
local export-basis guard rejects an unresolved stale local basis without
writes and reports both local and incoming documents for the first 20 differing
tasks, with the remaining count. There is no last-writer-wins overwrite.
Incoming data must be read from the pinned resolved data commit: resetting a
worktree does not remove untracked task files left by a local export.

## Shared fields

Each task carries its unchanged UUID, optional external reference, title,
outcome, ordered acceptance, kind, priority, labels, assignment, defer time,
lifecycle, parent and child requirement, prerequisites, active blockers,
supersession and disposal reason, and optional reported completion.

Explicit acceptance-evaluation mode and typed acceptance bindings travel as
planning policy. A binding names its one-based criterion and verification
kind, with an optional content fingerprint requirement. It contains no check
result, producer, host observation or satisfaction credit. Importing a binding
does not satisfy it or waive it. Duplicate or out-of-range positions refuse.

Blocker and completion attribution carries only the asserted actor principal
and kind. Reported completion also keeps its summary and original time.
Re-export does not replace shared attribution or time with the importing
session, new local record ids or import time.

Claims, grants, sessions, run ids, delivery state, retained prompts, private
scratch, project memories, native verification/evaluation records
and completion seals are outside this profile. Export/import require the
[host-local opt-in](#project-opt-in-setting); they perform no remote relay,
ignore/attribute edit or polling.
Export visibly reports the development no-op redactor: it filters nothing.
Authored planning fields are shared text, not automatically scrubbed secrets.
Items imported from an external source with an immutable source snapshot are
refused by this profile; graph export retains that provenance instead.
The CLI accepts `--actor-context` (or `ENGRAM_ACTOR_CONTEXT`) and
`--source-skill` for local import attribution; these never enter shared files.

## Project opt-in setting

Engram owns one host-local setting per stable project id and Engram home,
shared across worktrees. It lives beside `engram.db` as `task-sync-setting.json`,
outside the repository. These operations never open SQLite, change its schema,
or create or remove shared task files. Configuration remains separate from
materialization; enabling alone does not publish task text.

Supported CLI and shared-service preparation, export, import (including dry-run),
setup preparation and new materialization require this opt-in before new state,
capture or writes. Disabled, absent, invalid and unreadable settings refuse.
An already admitted pending preparation or materialization can finish from its
retained bytes after opt-out. A completed setup intent alone does not admit a
new materialization. Pure format readers, the selected-preparation reader,
backup and migration remain independent.

```text
engram --home LOCAL_HOME --project-file MAIN/.engram-project task-sync status --json
engram --home LOCAL_HOME --project-file MAIN/.engram-project task-sync enable --user-instruction "Enable Git task sync for this project." --json
engram --home LOCAL_HOME --project-file MAIN/.engram-project task-sync disable --json
```

The separate MCP tool `task_sync_settings` takes `action: status`, `enable`
or `disable`. Enable requires nonblank `user_instruction` quoting the user's
explicit request, at most 4096 bytes. Status and disable accept only an omitted
or null instruction; any string is refused with `invalid_user_instruction`
before settings writes. Read-only mutation refusal takes precedence. CLI actor/session
flags and environment defaults supply asserted attribution; `--session-id`
is optional. MCP uses its connection's project, actor and session. This is not
authenticated human intent. Read-only MCP and the service itself refuse setters
before filesystem writes; status creates no directories, locks, files or store.

Status format `engram-task-sync-status-v1` reports `project_id`, `state`,
`reason`, `effect` and `user_instruction`. Absence is `disabled`, with reason
`not configured (off by default)`. Explicit values report `enabled` or
`disabled`; unreadable, linked, oversized or invalid input is `unknown`.
Only enabled status returns the stored instruction; other states return null.
The effect describes opt-in per project on this machine without creating or
removing shared files. Disabled and unknown are not opt-in.

Setters return the observed status. Successful CLI status reads exit 0,
including unknown; refused setters or project selection exit 1 with format
`engram-task-sync-error-v1`, `project_id`, `operation`, `error: {code, message}`
and `status` (null when no project was selected). MCP operation refusals return
the same body with `isError`; malformed MCP arguments and read-only admission
retain their existing envelopes. CLI usage errors retain parser behavior.
TermAl uses this dedicated, non-strict API; invalid replies, unsupported commands
and timeouts are unknown. Host-control operations and strict readers are unchanged.

The private document is strictly typed, duplicate-rejecting and bounded to
16 KiB. It records format `engram-task-sync-setting-v1`, project id, enabled,
actor, session, UTC timestamp and instruction. Reads use retained no-follow
ancestor handles and reject Windows reparse points. RFC 3339 timestamps are
accepted; stored attribution does not expire with a process session. New Unix
directories/files use 0700/0600, while Windows inherits the home's ACLs.
Existing permissions are not rewritten.

An attributed setter replaces malformed configuration. It writes and syncs
a complete same-directory temporary file, renames it over the setting, then
reads back: concurrent setters use last writer wins and return what they observed.
Status is an observation, not a reservation. No lock or CAS is used; power-loss
durability beyond sync and rename is not promised. Temporary files never count
as configuration. Linked/nonordinary/unreadable paths and unsupported replacement
operations refuse, with no delete-then-rename fallback.

## Persistent data worktree

Code and shared task/evidence data use separate histories. From the main code
checkout, after the host-local opt-in:

```text
engram --home LOCAL_HOME --project-file MAIN/.engram-project task-sync data-init --branch engram-data
```

This explicit command creates the fixed `MAIN/.engram-data/` linked worktree on
an unborn orphan branch, with Git's worktree lock already set. It never commits,
fetches, pushes or opens SQLite. It preserves the code HEAD, index and tracked
files. When the code checkout does not already ignore that directory, it appends
`/.engram-data/` to the repository-local Git `info/exclude` before creating it;
existing bytes are preserved, including an unterminated final line. No user or
global configuration is changed. An existing admitted data worktree is reused
and locked, without resetting its branch. An occupied path, another project's
marker, a code-history branch or an existing branch without its admitted
worktree refuses. Partial registration is preserved, never forcibly removed.
Concurrent initialization on this host refuses immediately while its private
Git-admin file lock is held.

The directory is persistent project data, not build output or task evidence
scratch. Ordinary `git worktree remove` refuses a locked worktree, and pruning
keeps its registration even while its directory is temporarily absent. The lock
is not protection against an explicit forced deletion. Retire it deliberately
after preserving the data and reconciling pending exchange; ordinary coding-task
cleanup must leave it in place. Code worktrees require no task-file sparsity.

New exchange bindings select this data checkout and its own Git administration.
The retained field `repository.main_checkout` keeps its historical wire name;
it holds the data-worktree path for a new binding. Historical records are read
unchanged. Exchange inspection refuses a binding that selects code, a changed
data branch/admin directory, or a selected incoming/outgoing commit that shares
code ancestry or has any root outside `.engram/`. Git task deltas and cold
snapshots accept the two ordinary nested policy blobs without treating them as
task documents. The separate resolved-data-folder reader also admits those
files; raw private candidates still require their strict project/tasks layout.

`DataWorktree::advance_retained` advances only the selected data branch to an
existing operation-owned commit whose raw bytes and private retention ref still
match. The staged tree must match that commit, and an existing branch head must
be its ancestor. An agent-resolved commit can retain both data parents. A retry
after a lost response observes the same retained commit rather than creating a
replacement. This library operation does not stage files, change worktree bytes,
contact a remote or append the caller's ledger outcome. Commit/push authority and
the ordinary publication workflow remain separate caller responsibilities.

## Private Git setup preparation

```text
engram --home LOCAL_HOME --project-file MAIN/.engram-project task-sync setup-prepare MAIN/.engram-data --branch engram-data --visibility private --readership "project maintainers"
```

Preparation opens no SQLite store and applies no public files. It reads the
selected main-code marker and binds its project to the fixed ignored data
worktree, its own Git administration, attached orphan branch and current HEAD
(which may be unborn). The data index and committed tree must be empty for a
new setup. Existing `.engram/project.json`, `.engram/tasks`, `.engram/.gitignore`
or `.engram/.gitattributes` are unowned occupancy even when their bytes match;
preserve and relocate them explicitly before preparing. Unrelated private
siblings within an existing `.engram` directory remain. Files outside `.engram/`,
sparse data worktrees, links/reparse ancestors and ambiguous roots refuse.
No code-checkout policy or index flag is rewritten or normalized.

Current attributes are inspected without running filters. Applied filters,
custom diff/merge drivers, encoding, ident or non-LF eol refuse with the affected
path. Unused configured filters are allowed. Preparation records proposed
`.engram/.gitignore` and `.engram/.gitattributes` bytes in an immutable private
`engram-task-sync-setup.json` in the data worktree's Git administration. The
ignore policy exposes only the project/task documents and the two policy files;
the attribute policy selects text/LF and built-in text diff/merge, and disables
filters, encoding and ident. Neither policy is applied by preparation. The code
checkout's `.gitignore` and `.gitattributes` stay untouched.

Visibility, intended readership, profile, optional notes/machine alias and
attribution are operator assertions, not authenticated disclosure consent. The
redactor is a development no-op. The marker is bounded at 4 KiB and private
records at 2 MiB. A repeated matching basis returns the original operation and
intent bytes; another basis or unowned record refuses without replacement.
Moving the data HEAD makes the intent stale. A contended private lock refuses
immediately, without age-based reclamation or an automatic retry.

Actor context is retained verbatim, including whitespace and text longer than
256 bytes. When present it must be nonblank, at most 4096 UTF-8 bytes and contain
no control characters. An exactly empty context is absent. Invalid context
refuses before private setup/materialization state is created. The CLI accepts
`--actor-context`/`ENGRAM_ACTOR_CONTEXT` and `--source-skill`; these commands have
no MCP tool and a new process session does not replace the first attribution.

## Materializing a selected preparation

```text
engram --home LOCAL_HOME --project-file MAIN/.engram-project task-sync setup-materialize MAIN/.engram-data --setup-operation SETUP_UUID --prepared-operation PREPARED_UUID --progress-directory PROGRESS --candidate-directory CANDIDATE
```

The CLI and `LocalWorkService` consume the same explicitly selected setup intent
and completed candidate. They never choose a latest operation, recapture SQLite
or reserialize a task document. Reuse the original absolute selection spellings
on recovery; `..` parent components refuse. Candidate documents must already
use LF. Whole-candidate disclosure admission precedes new private/public writes:
protected tasks and inadmissible notes/activity refuse with bounded IDs, counts
and static reasons, without quoting field values. Receipts retain omissions,
first attribution and the development no-op redactor, not real DLP assurance.

The recorded marker, data administration, branch, HEAD and index are rechecked.
Only `.engram/` changes. A fresh shared directory is published atomically with
its two policy files; when a private `.engram` already exists, absent policy
files are linked before the shared project/tasks paths are published. Every
actual task path receives an effective attribute/ignore check, including
Git-admin overrides. Before a fresh publication, the retained policy is checked
without temporarily writing it to a worktree. Case aliases, nested policy files,
unsafe attributes, ignored task paths or unowned occupancy refuse.

An exclusive private Git-admin operation directory retains raw candidate bytes,
separate publication sources, a bounded immutable journal and physical ownership
identities. Publication sources contain the derived project envelope with the
selected disclosure profile and canonical planning-content basis, while the raw
candidate keeps its original project and task bytes and preparation fingerprint.
Retries derive the same public envelope and check its retained publication link;
they never substitute it for the raw candidate. Complete records and public files are linked without replacement;
unsupported filesystems refuse. Git administration and checkout must share a
filesystem supporting hard links. Windows identities use volume plus a 64-bit
file index; wider filesystem identities such as ReFS may collide. Equal bytes
and inventory are checked too, but an equal-byte identity collision can still
misreport ownership. No power-loss guarantee beyond the recorded sync/rename
operations is promised.

An admitted journal can finish after opt-out and without its original candidate
or source store. Exact repeats require the original public and retained objects;
equal independently replaced files are not adopted. A new unowned empty tasks
directory is accepted only for an empty candidate, reported as unowned and never
cleaned up. Existing private siblings are preserved. A pre-journal interruption
is not adopted: the refusal names the whole private attempt. Preserve it and any
unrelated edits, relocate it within Git administration, then repeat preparation
on the unchanged basis and materialize a fresh attempt. No automatic deletion
or general replacement command is supplied.

Managed ignore rules admit ordinary planning paths and evidence packs under
their UUID-origin, two-character shard and UUID JSON filename hierarchy, so
plain `git add .engram` stages both. Unrelated private files remain ignored;
the evidence readers still enforce exact shard agreement, file modes and pack
contents. Private raw candidates keep their planning-only inventory.

Managed policies are hard links to the retained `after` sources. Correct a
separate unsafe override while leaving them intact; an in-place policy edit
also changes that retained source, so restore the exact journaled bytes before
retrying. Preserve an independently replaced public file before explicitly
restoring the retained link; never overwrite it blindly. These operations do not
change SQLite, Git index/configuration/branch or commits, run a network relay,
or initialize a worktree. `data-init` is the separate explicit operation above.
Disabling sync removes no shared files or persistent worktree.

## Opt-in shared notes

`--include-notes` requires `--origin-machine`, an operator-asserted alias of
1–64 ASCII letters, digits, dots, underscores or hyphens. It is not a detected
host identity or authenticated provenance. Each eligible generic/status note
carries its original canonical note id, kind, exact body, asserted author
principal and kind, original timestamp and alias. Notes are ordered by id.
Imported notes retain their original alias when exported on a different host.
Sessions, tools, runs, refs and authority are excluded from this shared DTO.
This command does not accept newly authored note input.

The export receipt reports omissions by reason; counts never enter shared files.
Gate, host verification, environment, evaluation, execution observation and
path-binding records never become note candidates. Engram stores no host-private
retained prompts: the typed allow-list provides no route for such records and
does not claim their counts. Notes with refs, notes on tasks labelled
`restricted`, `secret` or `secret-ref`, and legacy snapshot members without
their own original ids are excluded whole. No replacement id is minted.

Bodies containing any backslash, absolute or dot-relative paths, tilde-prefixed
tokens, drive or URI schemes, environment-variable forms (`%VAR%` or `$VAR`
followed by a separator), or path-shaped slash sequences are excluded whole.
A slash sequence is path-shaped if it has three or more segments, a leading
or trailing slash, or two segments containing a dot or starting with a known
repository or host prefix. Ordinary `pass/fail`, `read/write` and `and/or`
prose and labels such as `Decision:` and `Engram::Fable` remain allowed.
Detection also checks starts after assignment and prose punctuation, so forms
such as `root=/private` and `dir=$HOME/private` are excluded. Glued option,
hyphen, underscore, slash and colon starts are checked too. Markdown emphasis
and quote delimiters are handled only for detection; stored and exported text
is never rewritten. Author fields use the same checks except that a
two-segment slash-separated actor principal such as `greg/claude`,
`org.example/codex` or `team/docs-bot` remains allowed. An author value, or
any word in it, with three or more slash segments or a dot-leading slash
segment is refused, so spaces cannot split `x/My Documents/key.pem` into
allowed pieces. So is one whose last slash segment, ignoring trailing `-`,
`_`, `%`, `:` or `.`, is a known repository/evidence prefix (`src`, `docs`,
`scripts`, `.engram`, `.worktrees`, `target`, `tests`, `.git`, `Users`,
`home`, `evidence`, `scratch`, matched case-insensitively). A prefix followed
by another slash
segment is refused anywhere in the value, including after a hyphen,
underscore, percent sign or colon. Standalone
8–64 hexadecimal tokens and member locators count as `possible locator`;
this conservatively excludes commit hashes as well. Ordinary actor principals
and `w-…` task references remain allowed. Filtering removes the whole note,
never rewrites its text, and is not secret detection or a real redactor.

These disclosure checks apply to native capture and candidate import admission.
Retained imported notes are read and exported as stored, with structural checks
for identity, ordering, bounds and metadata; doctor does not reapply the current
disclosure policy. A retained export can therefore contain notes that current
import admission would refuse. Export does honor the task's current `restricted`,
`secret` or `secret-ref` label: it omits retained notes whole and counts each as
`restricted task`. This leaves the stored notes intact; comparison and doctor
continue to check their structure.

The empty-target/exact-repeat boundary still applies to the complete shared
state, including the `notes_included` profile even with zero notes.
A plain candidate's notes-included profile requires at least one task, where
its selection is retained. A folder exchange carries the profile in its
envelope instead, so an empty notes-included folder exports and imports; the
first import of any folder, an empty one included, records the exchange
baseline and reports a change.
A changed body, author, kind, time, machine alias, owning task or project
for a known note id refuses before writes. A changed id or omitted note also
makes an existing candidate divergent. Disabling notes never deletes retained
notes. An initial unknown id/body is an operator assertion, not authenticity
proof; incremental note merging is deferred.

A planning-only exact repeat ignores local native notes, while still refusing
to discard retained shared notes. When notes are included, eligible native
notes added on the target make the repeat divergent; import does not assign
them a new origin-machine alias.

Shared note bodies have the native write boundary's 64 KiB UTF-8 limit;
oversized shared bodies refuse on import and export. Notes are inline in each
task document and count toward the existing 1 MiB
encoded file limit, including JSON escaping, and the 128 MiB candidate limit.
If notes make a document exceed those limits, the whole export refuses without
publishing a directory. It does not truncate notes or count that refusal as a
per-note omission.

New imports retain shared note metadata verbatim in the task's import origin.
Before importing notes, all consumers of that store need a notes-aware build:
older running processes cannot read origins that contain the new note fields.
Their newly created restored history contains inert reported bodies for existing
history readers, without a status-owner marker or native evidence. Original
identity and machine metadata are read from the origin by primary key. No old
row or projection derivation changes, and projection repair reproduces the new
restored rows. Graph snapshots flatten note identity; a later task-text export
of that flattened graph reports those members as legacy exclusions. Full-store
migration retains origin objects and original note metadata unchanged.

## Informational machine activity

Supplying `--origin-machine ALIAS` includes native activity even without
`--include-notes`. The alias has the same asserted, unauthenticated meaning and
validation as the note alias. Without an alias, export captures no native
activity; retained imported activity still travels under its original aliases.
An empty activity map is absent from the task document.

Each task's optional `activity` map is keyed first by origin alias, then by the
original canonical start-event id. Each entry repeats that `activity_id`, its
`task_id` and `origin_machine`, and carries an asserted actor principal and kind,
`state`, positive `state_revision` and the original latest-transition `event_id`.
Record ids travel unchanged, including supported historical ids. No claim token,
session, run, fence, expiry, grant, private reason or host cursor is exported.

Projection reads canonical events in source-feed order in the export snapshot.
A claim starts an episode at revision 1. An explicit release, disposal or
completion closes its current matching episode as `released`, `abandoned` or
`finished`, incrementing its revision and preserving the start actor. Accepted
handoff releases the prior episode and starts the successor with a new identity.
Recovery starts a new episode without inventing abandonment of the old one.
Renewal, checkpoint and inferred expiry produce no shared activity change.
States on different machines are not ordered by wall clocks. A `started` entry
is last-known information, never proof of a live lease or remote ownership.

Import retains the map verbatim in the new import origin. It creates no local
claim, run, session, activity event or verification credit, and re-export does
not relabel imported entries as native activity. Maps from multiple origins
remain separate. Candidate admission requires `started` at revision 1 with the
start event id, or a closed state at revision 2 or later with a different event
id. The complete empty-target/exact-repeat boundary applies:
changed or removed entries, rollback and conflicting known identities refuse
before writes. Updating notices in an existing graph awaits reconciliation.

Restricted task labels and path/locator-shaped actor fields exclude complete
entries on export, with separate `activity_omissions` counts in the local JSON
receipt. Candidate import applies the same disclosure checks. Stored validation
checks structure without changing retained bytes; export filtering leaves them
intact. Omission counts never enter shared files. Typed event ids are provenance,
not actor text to be filtered, and filtering is not secret detection.

The map is an optional field in newly written import origins; existing canonical
objects, restored history and projection derivation do not change. Activity
requires an activity-aware reader after import: pre-activity builds refuse
task-sync reads of origins containing the new field, and their doctor reports
`corrupt_store`. Use an activity-aware build; this verdict does not call for
projection repair or restoring a backup. Installing a binary does not update
running consumers. Full-store migration retains the map through the generic
row format.
There is no TermAl remote relay or background notification in this slice.

## Atomic import and reported lifecycle

Import validates the complete graph before mutation and checks the target
inside one SQLite write transaction. Parent, prerequisite and replacement
references must resolve; hierarchy/dependency cycles and ambiguous short refs
refuse. A UUID already owned by another project in the store refuses.

The destination's planning must first equal its selected basis, as the
[folder exchange](#git-folder-exchange) describes. The first `--empty-basis`
folder import checks that empty local planning basis, then treats incoming
tasks as new under the same identity, origin-graph and held-run admission as
later imports. A refused task and its dependent additions stay unapplied while
independent tasks import. There is no separate stricter legacy folder path.
The lower-level plain candidate-import API retains its own empty-target
contract. Dry-run executes the same transaction and checks, then rolls back,
retaining no task, origin, baseline or execution records. Store initialization
is a separate operator operation; a missing home is not silently configured
by task import.

Without an inventory or prior local state, a first import into an empty,
unenrolled store cannot detect an omitted isolated task, but can refuse an
omitted referenced task. This initial path never disposes an existing task.
Once enrolled, [member omission](#edits-on-an-enrolled-destination) can cancel
a task irreversibly and retire its id permanently (`id_collision` on return),
even if the input was accidentally partial. Inspect `--dry-run` first.
File removal never waives a required child.

Open and proposed tasks become inert planning identities. Completed tasks keep
an attributed restored completion, stay out of ready work and can satisfy an
ordinary prerequisite. Cancelled and superseded tasks retain explicit disposal
state and reasons. No local completion seal, acceptance pass or host proof is
manufactured; native execution still uses its ordinary admission and policy.
An enrolled destination applies the supported planning fields, relations and
reported completion described by the
[folder exchange](#edits-on-an-enrolled-destination), with per-task outcomes.
The profile contains no original disposal actor or time. Its local history
labels disposal as `disposal_reported`, attributed to the importing actor as a
report at import time, rather than claiming they performed the original act.

## Refusal codes

With `--json`, `task-sync export` and `task-sync import` print a refusal on
stderr as `{"error": {"code", "message", "details"}}` and exit 1; export also
prints its success receipt as JSON. The message keeps the refusal's human
text unchanged; the plain form prints the same text, after the file path it
was reading when the refusal concerns one candidate file. An invalid-input
refusal repeats its reason as `details.reason`; a graph refusal from graph
validation has `details: null`. An oversized shared note has
`details: {"bytes": N, "limit": 65536, "reason": "note body exceeds the UTF-8 byte limit"}`:
the counts concern the body's UTF-8 bytes, rather than the serialized task
file's separate 1 MiB limit. Structured refusals carry neither the outer
`reading PATH` context nor an automatic per-file path field; the export
success receipt's `directory` names the destination, not individual files.
Each code below is stable. A refusal outside
these families, such as a project without the
[host-local opt-in](#project-opt-in-setting) or an invalid export alias, keeps
its generic code; no `--json` refusal of these commands carries a work-item
remedy. An I/O failure, such as a missing directory, is not a refusal and is
reported as before. SQLite lock failures retain their generic storage code;
they are not classified as task-sync refusals.

Publishing a new export requires an already-existing ordinary destination
parent directory. Relative destinations are resolved to absolute paths before
the parent is looked up. After content admission, both writers inspect an
existing destination first: a matching candidate succeeds without checking
its parent, while an invalid or divergent candidate is refused (or its
inspection fails with I/O). Parent admission applies only when publication
must continue, so it does not replace an existing-destination error.

`task_sync_destination_no_parent` is the defensive parent-lookup refusal for
a resolved path with no parent component; it does not mean that a named
parent is absent on disk. Missing or inaccessible parents produce ordinary
I/O errors at the first failing filesystem operation, even with `--json`;
they do not acquire a task-sync refusal code. An existing filesystem root is
inspected before parent lookup and may be refused as a candidate or fail with
I/O. It is therefore not a reliable end-to-end route to the `no_parent` code.
This ordering also preserves retries through a linked parent when the
existing candidate matches; a new publication still refuses a parent that
is itself a link. It does not protect against every concurrent ancestor
replacement.

| Code | Refusal |
| --- | --- |
| `task_sync_native_state` | The plain candidate import targets a project that already holds native work or execution state, or whose project feed has begun (an exchange baseline among others), so imported tasks would be invisible to change capture. Complete-folder import instead uses its selected export basis and per-task admission; existing native work alone does not cause this refusal. |
| `task_sync_reconciliation_held` | An unresolved reconciliation hold covers the work; fresh claims, handoffs, governing planning, evaluations and work-bound control admission wait while original owners settle or resolve that candidate. |
| `task_sync_divergent_planning` | The candidate changes or adds to the planning, notes or activity the store already holds without removing a task; the destination's planning differs from its selected basis; the basis or export note selection differs from the folder's or the baseline's; or export meets native activity that conflicts with a retained entry. |
| `task_sync_removed_planning` | The candidate omits a task the store already holds. |
| `task_sync_id_collision` | A candidate task, blocker, note or activity id already belongs to another record, task or project. |
| `task_sync_invalid_graph` | The candidate is malformed, or fails its graph, reference or disclosure admission; or a folder envelope or basis file is malformed, incomplete or inconsistent with its own fingerprint. |
| `task_sync_origin_mismatch` | The candidate or the selected basis names another project, an import origin binding differs, or export meets an externally imported item. |
| `task_sync_destination_occupied` | The export destination already contains a different candidate. |
| `task_sync_destination_appeared` | The export destination appeared while the export was prepared. |
| `task_sync_publication_unsupported` | The filesystem cannot atomically move a staged directory without replacing an existing entry; nothing is published. |
| `task_sync_destination_no_parent` | The resolved export destination has no parent component at the parent-lookup step. |
| `task_sync_destination_link` | A candidate directory or file is a symbolic link. |
| `task_sync_destination_not_directory` | A candidate directory is not an ordinary directory. |
| `task_sync_destination_not_file` | A candidate file is not an ordinary file. |
| `task_sync_destination_file_limit` | A candidate file exceeds the 1 MiB limit. |
| `task_sync_destination_file_grew` | A candidate file grew past the 1 MiB limit while it was read. |
| `task_sync_destination_task_limit` | The candidate exceeds the 100,000-task limit. |
| `task_sync_destination_tree_limit` | The candidate exceeds the 128 MiB limit. |
| `task_sync_destination_non_utf8_name` | A task filename is not valid UTF-8. |
| `task_sync_destination_unknown_entry` | The candidate directory holds an entry the format does not define. |
| `task_sync_destination_missing_tasks` | A plain candidate directory, as read by preparation and setup materialization, has no `tasks` directory. A folder import accepts a folder without one. |
| `task_sync_import_conflict` | A complete-folder import changes project-level fields or has a graph or batch conflict that cannot be attributed to individual tasks; the whole import is refused with exit 1. Strict delta admission also uses this code to refuse the whole candidate when any task is inadmissible. Attributable complete-folder conflicts instead leave independent changes applied and report every changed task in the receipt's `tasks` list, with `applied`, `held` or `refused` and a reason and detail for unapplied tasks; there is no 20-task receipt limit. A partial real import or dry run exits 3. [Per-task admission](#edits-on-an-enrolled-destination) supports title, outcome, acceptance and its bindings, priority, labels, assignment, prerequisite and blocker edits, additions referencing native endpoints, and reported completions under the rules above. A changed note selection refuses earlier, as `task_sync_divergent_planning` or `task_sync_invalid_graph`. |
| `task_sync_incremental_basis_unavailable` | An incremental exchange's recorded project-feed cut no longer continues the store's feed: the cut is ahead of the feed head, or its anchor names a different object. The check reads the anchor's entry alone, never the interval since the cut. A first capture, from position zero, also refuses while the project holds tasks that no project-feed entry names, as a loaded graph's tasks are. Commit-delta intake also uses this code when Git completes its lookup but the selected commit is unavailable. The changed tasks cannot be named exactly; a full scan happens only on an explicit rebuild. |
| `task_sync_feed_projection_gap` | The changed-task index does not cover every project-feed position of the interval a change capture asked for, so it cannot name every changed task. `engram doctor` names the missing rows; `engram doctor --repair-projections` rebuilds them from the feed. The capture never answers with fewer changes or a full scan. |
| `task_sync_git_query_failed` | A local Git read could not complete, including repository/process failures or the query deadline. Correct that failure and retry; this does not request a rebuild. A response over its bound instead uses the file or tree limit code, and a blob refusal names the document path. |
| `task_sync_baseline_head_moved` | An exchange baseline names a previous baseline that is no longer the project's latest one: another exchange was recorded after the one it was planned against. Each baseline continues exactly one predecessor, so the later exchange is refused and nothing is written; plan it again against the current baseline. |

## Reading a commit delta

The library-only `task_sync::git::read_task_delta` takes an absolute repository,
project id and two explicit full commit object ids. It compares the complete
old and new trees through the existing hardened, bounded local Git executor;
intervening commits and merges require no history walk. NUL-delimited changes
under `.engram` select only added or modified task blobs. A removal opens no
old task blob, and a rename is a removal plus an addition. Both `project.json`
envelopes are read to check project binding and a stable note selection.

The result is transient typed upserts and explicit removals, with the two
embedded folder bases. Their fingerprints are envelope text, not recomputed
inventory or recorded baselines, and may be stale after a task-text merge.
The prior source tree must already have been admitted. Changed
documents receive the shared document-field, note/activity disclosure and
local shape checks; graph relationships and identity collisions with retained
documents still need the caller's admission. Unchanged task blobs are not
opened or revalidated. Missing commits, malformed changed documents, unexpected
paths and non-ordinary modes refuse. The operation has a 30-second query budget,
a one-MiB bound per Git response and a 128-MiB changed-payload bound.

This seam opens no SQLite store, retains no cursor or receipt, imports nothing,
and changes no Git ref, index or worktree file. It does not wire incremental
intake into ordinary import, preparation or reconciliation, and grants no
publication authority.

## Local incremental preparation

The library's `SqliteStore::capture_task_documents` captures an explicit set of
task ids in one read snapshot. It shares the complete exporter's per-task
capture, including note and activity disclosure, but reads no unrelated task
documents. Its transient result distinguishes a present document, an absent
identity, and a task explicitly retired by the selected project's exchange
baseline. Duplicate ids are captured once. Changed baseline note selection,
cross-project selections, and selected read or capture errors refuse without
a full-export fallback. References may name unselected tasks: typed capture
does not admit document shape or graph relationships. Callers must validate
both before applying or publishing changes.
This library seam does not make the ordinary directory or preparation routes
proportional to changes.

`task-sync prepare DIRECTORY --progress-directory PROGRESS` prepares an immutable,
complete directory candidate. Add `--include-notes --origin-machine ALIAS` for
the same opt-in note profile as export. Supply `--origin-machine ALIAS` alone
to include native activity without prose notes. Imported activity retains its
original aliases in every profile. Keep the progress directory host-local
and separate from the candidate, the SQLite store and `.engram/tasks`.
Preparation does not enable Git exchange. The operator chooses the candidate
destination; preparation does not choose or configure a tracked task layout.

The first operation serializes every task. Later operations capture a dense
project-feed cut and the complete typed inventory in one SQLite read snapshot,
including imports without project-feed entries and notes whose feed entry has
no task id. They reuse the exact retained bytes of unchanged task documents and
serialize each changed task once, in its final state at that cut. Removed tasks
are absent from the next complete candidate. Runtime-only changes do not change
planning bytes. This still scans the typed inventory; it does not promise a
cost proportional only to the feed delta.
With an explicit native alias, meaningful starts and releases update shared
activity; renewals and checkpoints leave its bytes unchanged.

The receipt reports `prepared`, an operation UUID, `recovered`, the selected
`path` (`full` or `incremental`), its `reason`, the captured `project_position`,
`serialized_tasks`, note `omissions` and separate `activity_omissions`. Prepared progress means a local
candidate exists; it says nothing about Git staging, commits or remote exchange.
Original task and note identities remain in the ordinary shared documents.

The host-local journal retains a predecessor and a current generation, with
the project, store route, profile, producing build and captured cut. A changed
producer, missing or malformed generation, uncertain feed continuity, or missing or
divergent prior output selects a full capture automatically. A store path and
feed anchor are continuity checks, not a portable store identity. Changing a
valid profile or machine alias requires `--rebaseline`; it does not overwrite
an existing candidate. Unexpected progress-directory entries or more than two
generations are refused visibly, without deleting unknown content. Remove those
unexpected entries explicitly before retrying.

A process-held file lock refuses concurrent preparation immediately and names
the lock path. Process exit releases the lock; recovery never deletes a live
lock or guesses from a PID. Before publishing the output, preparation retains
the immutable pending candidate and cut. A retry finishes that same operation
at the same destination without taking a newer snapshot. Later local changes
are captured by the next operation. Divergent existing output is refused and
left intact; a known divergent destination refuses before recording new progress.
A pending operation must finish with its original store and project binding
before rebaselining or changing its destination. File flushing and no-replace
publication support process-exit recovery; this is not a power-loss durability
guarantee. A process exit during output staging may leave an ordinary
`.engram-task-candidate-<uuid>` directory beside the destination. Recovery still
publishes the retained pending bytes but does not sweep the operator's parent
directory; an operator may remove an orphan after confirming no exporter is using it.

Preparation adds no SQLite rows, tables, indexes, schema changes or projection
derivation. Its local sidecar is disposable progress, separate from canonical
storage, full-store migration and ordinary complete-candidate import.

### Reading a selected preparation

The library's `task_sync::progress::read_prepared` takes a progress directory
and a `PreparedSelection`: an operation UUID, candidate destination, project
and export profile. It returns the exact retained document bytes, a structurally
validated task tree and typed local provenance (sequence, predecessor, producer,
store route, captured cut and separate note/activity omission reports). It checks
that the selected generation is complete and its output is byte-identical;
it never substitutes a newer operation or reserializes the documents.

The reader creates no lock, marker or directory, performs no cleanup and opens
no SQLite store. Missing, pending, malformed, unknown, linked or divergent
inputs refuse with their entries and bytes preserved. Reads use bounded files
and retained directory handles without following links, including Windows
reparse points. Caller paths must have no linked ancestors. On Windows these
handles temporarily deny deletion or renaming of retained directories and their
ancestors, so concurrent preparation may refuse while pruning. Non-selected
generation handles are released after journal validation and selection.
A progress journal may prune a selected operation later;
missing retained bytes are a refusal, never a request for a new capture.
This is not an atomic filesystem snapshot or approval to publish. Retained
notes and activity keep their stored structural meaning; materialization
separately checks its disclosure policy and preserves these exact bytes.
The whole retained journal inventory must be complete and well formed; the
latest generation may start a new baseline without a predecessor after recovery.

## Explicit HTTPS process runner

The library's `task_sync::exchange` module provides typed observation, fetch to
an explicit private retention ref, and non-force push of an explicit commit to
one configured branch. It is process infrastructure, not a Git exchange CLI,
an import receipt, or permission to publish. The shared
`LocalWorkService::run_task_sync_exchange` entry refuses read-only use and
requires the project's current opt-in before any Git subprocess. The existing
offline Git builder remains separate.

The runner requires Git 2.31 or newer and checks the version before reading
configuration or starting transport. URL-scoped runtime configuration uses
separate environment keys and values, avoiding Git's `-c key=value` splitting.
The selected repository must be an ordinary absolute path without linked
ancestors. The runner observes and rechecks one identical fetch/push HTTPS URL
and its supported credential helper selection. Multiple destinations, SSH,
other transports, URL userinfo, query strings, fragments, and Git URL rewrite configuration
refuse. Refusal of URL rewrites also covers unrelated aliases: supply a direct
configured HTTPS destination. Rechecks detect observed configuration drift;
they are not a configuration lock against concurrent same-user edits. URL paths
accept ASCII letters, digits, unreserved punctuation (`-._~`), `/`, and `%`;
other bytes require percent encoding. Literal `=` and shell punctuation refuse
at admission; percent-encoded `%3D` remains literal input to Git.
Configured bundle fetch sources and default push options also refuse. The
explicit push disables tag following, mirroring, pruning and signing; fetch
does not prune refs or tags, recurse into submodules, or write `FETCH_HEAD`.

Engram launches Git directly and interpolates no caller text into a shell.
Git can use its own shell to invoke its normal credential helper. Only the
configured helper names `manager`, `manager-core`, and `wincred` are admitted,
without helper arguments or `!` scripts. All ordered unscoped helper entries
are observed, including reset entries, and participate in drift detection.
URL-scoped helper configuration refuses explicitly rather than approximating
Git's matching rules; the refusal redacts its URL scope, names its bounded
escaped file origin, and asks the operator to remove the entry or move an
admitted helper to unscoped `credential.helper`. An empty helper selection supports
anonymous HTTPS. Git uses the user's existing credential facility; Engram
stores no credentials. Terminal, askpass and interactive helper prompts are
disabled, as are trace and secret-trace diagnostics. TLS verification is
enabled and HTTP redirects are refused. Errors contain categories, safe version
numbers, and the bounded escaped file origin for scoped-helper refusals; they
never contain raw Git/helper stderr or destination URLs. Successful stdout is bounded
protocol data for the calling workflow, not a diagnostic to print unexamined.

A completed single-ref push exiting with Git's rejection code is classified as
`NonFastForward` only when its complete porcelain record names the exact selected
commit and branch and reports `non-fast-forward` or `fetch first`. Push output
uses the C locale. Other failures, multiple ref records, truncated output,
cancellation and deadlines retain their existing categories and uncertainty.
The named refusal describes that command only; it does not settle an earlier
unknown attempt or authorize a retry or another push.

On Windows, `process-wrap` starts the child suspended, assigns it to an owned
kill-on-close Job Object, and then resumes it. Git's helpers and descendants
remain in that job. Root exit, cancellation, output overflow, deadline and
dropping the execution future terminate the owned tree; concurrent bounded
pipe reads avoid waiting indefinitely for inherited handles. The default
budget is 30 seconds, 1 MiB stdout and 64 KiB stderr. Callers may select up to
120 seconds and 1 MiB per stream. These are execution/output limits, not a
download, object-store or repository-size quota. Native synchronous spawn is
not preempted by the async deadline.

Confinement is not atomic at process creation: abrupt parent death between
suspended spawn and job assignment can leave an inert suspended process.
No child code has run in that interval. There is no claim of resource cleanup
for that gap. A controller/spawn failure refuses instead of launching an
unconfined fallback. Other platforms refuse before transport execution.
Owned local subprocess tests establish controller cleanup, pipe bounds and
configuration admission. They do not establish real-network authentication,
cross-machine exchange, SSH support, or recovery of an unknown remote push.
The caller must reconcile an uncertain remote outcome explicitly.

## Storage and repair

Each imported task has a new canonical `task_sync_import_origin` object in the
existing objects table, pointing to its ordinary inert restored record and
carrying its complete planning policy plus local import attribution. Existing
graph-load records and origins keep their bytes and interpretation. The new
origin is distinct from a graph-load audit; import creates no fabricated load
receipt and no new table, column or index.
The local task's import attribution holds the minted origin id as a derived
provenance link. Reads fetch that origin by primary key and validate its restored
record back-reference. Doctor, import admission and graph-save integrity audits
enumerate all origins for duplicate and graph checks; ordinary reads, planning
export and ordinary mutations use primary-key lookup. This local link never
enters the shared task text.

Historical actor storage requires a session-shaped binding. Shared blocker and
completion attribution uses the fixed `task-sync-reported` marker locally to
represent excluded session provenance. It is not an original session, is not
registered, conveys no execution authority and never enters the shared files.
Live session admission reserves this marker. Display uses the reported actor
principal, so unrelated reported authors have distinct actor labels.

Doctor validates new origins, restored bindings and graph closure. Projection
repair retains imported task state and canonical bytes. Full-store migration
export/import preserves new object ids and links through the existing generic
row format. This slice requires no migration of live stores and changes no
existing durable row shape or projection derivation.
After a store imports planning text, a binary predating this capability cannot
read those imported items. Reverting the installed binary requires retaining a
build that understands them; a binary backup alone is not a store rollback.
