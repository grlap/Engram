# Task planning text

The explicit directory profile is the first implemented slice of
[Git task synchronization](git-task-sync.md). It exchanges current task
planning and explicitly selected shared notes between isolated SQLite stores.
It does not configure Git, exchange activity, reconcile an existing graph,
or enable background work.
Full native history remains in [graph snapshots](work-graph-snapshot.md) and
[store migration](full-store-migration.md), separate formats with different scope.

## Commands and directory layout

Select a project with its ordinary `.engram-project` marker and an explicit
host-local store home. The directory argument is deliberate and may be relative
to the current directory. Its parent must already exist. Initialize each local
store with `engram init` before using these commands.

```text
engram --home SOURCE_HOME --project-file PROJECT/.engram-project task-sync export DIRECTORY
engram --home SOURCE_HOME --project-file PROJECT/.engram-project task-sync export DIRECTORY --include-notes --origin-machine host-A
engram --home TARGET_HOME --project-file PROJECT/.engram-project task-sync import DIRECTORY --dry-run
engram --home TARGET_HOME --project-file PROJECT/.engram-project task-sync import DIRECTORY
```

The two homes contain isolated stores for the same stable project id. Export
publishes a new directory containing `project.json` and `tasks/<full-uuid>.json`.
An existing equivalent candidate is left byte-for-byte unchanged; a different
candidate is refused. Publication prepares a sibling staging directory and
moves it without replacing an existing destination. An operating system that
cannot perform that directory move without replacement refuses publication.

Planning-only `project.json` contains `format: "engram-task-planning-v1"` and
`project_id`. Opt-in note export adds `notes_included: true`; otherwise the
field and task note arrays are absent and planning-only bytes remain unchanged.
An absent `notes_included` and an explicit `false` are equivalent spellings of
the planning-only profile; switching between them is an exact repeat.
There is no inventory, store identity or runtime cursor.
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
scratch, project memories, native verification/evaluation records
and completion seals are outside this profile. There is no remote relay,
enablement configuration, Git command, ignore/attribute edit or polling.
Export visibly reports the development no-op redactor: it filters nothing.
Authored planning fields are shared text, not automatically scrubbed secrets.
Items imported from an external source with an immutable source snapshot are
refused by this profile; graph export retains that provenance instead.
The CLI accepts `--actor-context` (or `ENGRAM_ACTOR_CONTEXT`) and
`--source-skill` for local import attribution; these never enter shared files.

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
is never rewritten. Author fields
use the same checks except that slash-separated actor principals remain allowed;
known repository/evidence prefixes (`src`, `docs`, `scripts`, `.engram`,
`.worktrees`, `target`, `tests`, `.git`, `Users`, `home`, `evidence`, `scratch`)
are still refused there, including prefixes in later slash segments. Standalone
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
The notes-included profile requires at least one task, where its selection is
retained. Empty notes-included candidates refuse on import and export;
an empty planning-only candidate remains a no-op on an empty target.
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
