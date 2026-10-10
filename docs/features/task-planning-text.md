# Task planning text

The explicit directory profile is the first implemented slice of
[Git task synchronization](git-task-sync.md). It exchanges current task
planning between isolated SQLite stores. It does not configure Git, exchange
notes or activity, reconcile an existing graph, or enable background work.
Full native history remains in [graph snapshots](work-graph-snapshot.md) and
[store migration](full-store-migration.md), separate formats with different scope.

## Commands and directory layout

Select a project with its ordinary `.engram-project` marker and an explicit
host-local store home. The directory argument is deliberate and may be relative
to the current directory. Its parent must already exist. Initialize each local
store with `engram init` before using these commands.

```text
engram --home SOURCE_HOME --project-file PROJECT/.engram-project task-sync export DIRECTORY
engram --home TARGET_HOME --project-file PROJECT/.engram-project task-sync import DIRECTORY --dry-run
engram --home TARGET_HOME --project-file PROJECT/.engram-project task-sync import DIRECTORY
```

The two homes contain isolated stores for the same stable project id. Export
publishes a new directory containing `project.json` and `tasks/<full-uuid>.json`.
An existing equivalent candidate is left byte-for-byte unchanged; a different
candidate is refused. Publication prepares a sibling staging directory and
moves it without replacing an existing destination. An operating system that
cannot perform that directory move without replacement refuses publication.

`project.json` contains only `format: "engram-task-planning-v1"` and `project_id`.
There is no inventory, machine identity, store identity or runtime cursor.
Import requires both `project.json` and the `tasks` directory, rejects unknown
entries and symbolic links, and checks every task filename against its UUID.
The reader and exporter limit each file to 1 MiB, the tree to 128 MiB, and the
task count to 100,000. Task graph limits also apply.

Export writes pretty UTF-8 JSON, LF and a final newline. Typed field order and
set ordering are deterministic. Acceptance criteria retain their authored
order. CRLF JSON is accepted and compared after decoding; escaped CR and LF
inside string values remain unchanged. Duplicate JSON members, unknown fields
at any depth, unsupported profiles and unresolved conflict text are refused.

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
scratch, project memories, shared notes, native verification/evaluation records
and completion seals are outside this profile. There is no remote relay,
enablement configuration, Git command, ignore/attribute edit or polling.
Export visibly reports the development no-op redactor: it filters nothing.
Authored planning fields are shared text, not automatically scrubbed secrets.
Items imported from an external source with an immutable source snapshot are
refused by this profile; graph export retains that provenance instead.
The CLI accepts `--actor-context` (or `ENGRAM_ACTOR_CONTEXT`) and
`--source-skill` for local import attribution; these never enter shared files.

## Atomic import and reported lifecycle

Import validates the complete graph before mutation and checks the target
inside one SQLite write transaction. Parent, prerequisite and replacement
references must resolve; hierarchy/dependency cycles and ambiguous short refs
refuse. A UUID already owned by another project in the store refuses.

The initial target task graph must be empty. Exact repeat of the same imported
planning state is a no-op, including when the importing actor or clock changes:
no new records, events, receipts or timestamps. A different candidate or missing
existing task refuses. Existing native work refuses even if its editable fields
look equal. Dry-run executes the same inserts and constraint checks, then rolls
back, retaining no task, origin or execution records. Store initialization is a separate
operator operation; a missing home is not silently configured by task import.

Without an inventory or prior local state, an initial import cannot detect an
omitted isolated task. It can detect an omitted referenced task. On repeat,
comparison against the existing graph detects any missing task. File removal
never disposes a task or waives a required child.

Open and proposed tasks become inert planning identities. Completed tasks keep
an attributed restored completion, stay out of ready work and can satisfy an
ordinary prerequisite. Cancelled and superseded tasks retain explicit disposal
state and reasons. No local completion seal, acceptance pass or host proof is
manufactured; native execution still uses its ordinary admission and policy.
Changes to an existing graph await later reconciliation slices.
The profile contains no original disposal actor or time. Its local history
labels disposal as `disposal_reported`, attributed to the importing actor as a
report at import time, rather than claiming they performed the original act.

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
