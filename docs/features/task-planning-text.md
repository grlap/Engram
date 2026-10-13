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
host-local store home. The directory argument is deliberate and may be relative
to the current directory. Its parent must already exist. Initialize each local
store with `engram init` before using these commands.

```text
engram --home SOURCE_HOME --project-file PROJECT/.engram-project task-sync export DIRECTORY
engram --home SOURCE_HOME --project-file PROJECT/.engram-project task-sync export DIRECTORY --origin-machine host-A
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
field and task note arrays are absent. Activity is independent of note selection;
with neither notes nor activity, planning-only bytes remain unchanged.
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
enablement configuration, ignore/attribute edit or polling in export/import.
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
or create or remove shared task files. Git materialization and operation-start
admission remain separate; this setting alone does not ship Git exchange.

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

## Private Git setup preparation

```text
engram --home LOCAL_HOME --project-file MAIN/.engram-project task-sync setup-prepare MAIN --branch ordinary --visibility private --readership "project maintainers"
```

This separate command does not open or initialize SQLite. It verifies the
selected marker and project, the main repository root, an attached explicitly
selected branch and a nonsparse checkout. Linked worktrees, bare repositories,
ambiguous roots, links/reparse ancestors, applied unsafe attributes, and dirty,
staged or conflicted reserved/setup paths refuse before preparation. An existing
`.engram/project.json` or `.engram/tasks` is unowned occupancy, even if its bytes
match an intended export; relocate it explicitly first. Reserved occupancy in
the index or HEAD also refuses, including paths absent from disk and ASCII case
aliases. Setup-file case aliases in the index or HEAD refuse before status. Setup files
and the selected marker refuse index flags that suppress inspection
(`assume-unchanged` or `skip-worktree`). Unrelated local state is preserved.

Preparation queries current Git attributes on the manifest, a representative
task filename, the setup files and the marker before status compares their
contents. A filter, custom diff or merge driver, encoding or ident attribute, or
a non-LF eol attribute, names its path and attribute in the refusal. Built-in
text diffs (`diff`) and the boolean built-in merge attribute (`merge`) are admitted. Unused configured
filters are admitted. Nested `.engram/.gitattributes` and `.engram/.gitignore`
files refuse. The proposed ignore block reopens the root `.engram` directory
before excluding private contents and admitting the shared paths. The proposed
root block sets text and LF, enables built-in text diffs and the text merge
driver, and unsets filters, encoding and ident;
it is not yet applied. A later materializer must query every actual task path
after applying the block and before writing shared files, including overrides
from Git-admin `info/attributes`. Preparation does not prove future filenames
or the attributes of a hypothetical edited checkout.

Visibility (`public` or `private`), nonblank intended readership, attribution,
the planning profile, and optional `--include-notes --origin-machine ALIAS` are
operator assertions, not authenticated disclosure scope. An alias without notes
is accepted for future activity capture. The development redactor remains a
no-op; this command does not approve protected task-field disclosure.

The receipt names `engram-task-sync-setup.json` in the actual Git admin directory,
including a separate Git directory. The private immutable intent holds the
repository/branch/project basis, original setup bytes and proposed narrow ignore
and LF attribute edits. Setup inputs are bounded at 64 KiB (the marker at 4 KiB).
Compact JSON keeps the raw-byte arrays within the private reader's 2 MiB bound;
publication also checks that bound.
The edits are not applied: no `.engram` files, index, branch, global configuration
or SQLite rows change. Preparation enables no sync and introduces no remote
relay, sparse enrollment, commit or network operation.

A matching disclosure assertion and repository basis returns the original
operation and unchanged intent bytes. The first caller's attribution (including
actor context and source skill) is recorded separately; a new process session
does not change retry identity. A different or unowned record refuses without
replacement. Moving HEAD makes the recorded basis stale; this preparation slice
does not replace or retire it.
An immediately contended private lock refuses; it is not retried or reclaimed
by age. Lock ownership bytes and the intent are published with no-replace hard
links of complete private pending
file; unsupported filesystems refuse. An interruption can leave an owned
pending file, which is not a published intent. This is process-interruption
safety, not a power-loss guarantee. A later materialization operation must
revalidate the recorded basis before applying edits or shared task bytes.

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

## Local incremental preparation

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
notes and activity keep their stored structural meaning; a later materializer
must separately check its disclosure policy and preserve these exact bytes.
The whole retained journal inventory must be complete and well formed; the
latest generation may start a new baseline without a predecessor after recovery.

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
