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
engram --home TARGET_HOME --project-file PROJECT/.engram-project task-sync import DIRECTORY --empty-basis --dry-run
engram --home TARGET_HOME --project-file PROJECT/.engram-project task-sync import DIRECTORY --empty-basis
engram --home TARGET_HOME --project-file PROJECT/.engram-project task-sync import DIRECTORY --basis OWN_EXPORT/project.json --json
```

The two homes contain isolated stores for the same stable project id. Export
publishes a new Git folder containing `project.json` and
`tasks/<full-uuid>.json`. An existing equivalent folder is left byte-for-byte
unchanged; a different one is refused. Publication prepares a sibling staging
directory, reads it back, and moves it without replacing an existing
destination. An operating system that cannot perform that directory move
without replacement refuses publication.

A folder's `project.json` holds three members. `project` is the planning
document: `format: "engram-task-planning-v1"`, `project_id`, and
`notes_included: true` for an opt-in note export (otherwise the field and task
note arrays are absent). `profile` is the disclosure profile the export used:
`include_notes`, which both hosts share, and the exporting host's own asserted
`origin_machine` alias, or `null`.
`folder_exchange` marks the folder `complete` and carries its
`export_basis.inventory_fingerprint`, the content fingerprint of the planning
documents it holds. Activity is independent of note selection. There is no
store identity or runtime cursor.

## Git folder exchange

`task-sync export` and `task-sync import` are the folder route; no other
command exchanges a folder. Export reads the store in one snapshot and
publishes its shared planning together with that snapshot's export basis. The
first export of a store with no exchange history publishes its whole planning.

Import applies a resolved folder against a destination basis that the caller
selects. Exactly one selection is required:

- `--basis FILE` names this host's own `project.json` from its export before
  the Git merge, for example as written by
  `git show PINNED_COMMIT:PATH/project.json`. Engram runs no Git and does not
  resolve `ORIG_HEAD` or any other ref.
- `--empty-basis` imports into a destination that holds no shared planning yet.
  The import checks inside its write transaction that the destination is empty.

The folder's own embedded basis is never used for this comparison, and
omitting both flags is a usage error, not a fallback. A basis must belong to
this project and share the folder's note selection (`include_notes`). The
origin-machine alias is host-local: each host exports under its own alias, so
a folder from another host may carry a different one. Import compares the
destination's current planning, exported under the basis's own profile, with
the selected basis inside its write transaction; any local planning change
since that export refuses as divergent planning, and nothing is written.

The basis is a content comparison, not an authenticated host identity. Equal
bytes from another host cannot be told apart from this host's own, so
selecting this host's own copy is the caller's obligation. A basis with a
different inventory, project or note selection refuses.

A successful import records the applied folder as the exchange baseline. An
exact repeat of the same import, by the same actor and session against the
same basis, replays its first receipt and records nothing more. `--dry-run`
performs the whole import under the same identity, with every check, replay
and refusal, then rolls it back explicitly: no task, baseline, receipt or feed
entry remains, and a later real import checks everything again. Its receipt
reports `dry_run: true`; a dry run of an already committed import replays
that import's receipt, as the real repeat would.

A later export keeps the note selection its baseline was recorded with; an
export under another note selection refuses. Candidates written by `task-sync prepare` or
setup materialization carry no exchange envelope, so `task-sync import`
refuses them; the plain-candidate library calls remain for tests only.
Import requires `project.json`, rejects unknown entries and symbolic links,
and checks every task filename against its UUID. A folder with no tasks may
omit its `tasks` directory, since Git keeps no empty directories. Import reads
the resolved folder's inventory from its files: a merge that edits a task
leaves the embedded fingerprint stale, and Git cannot regenerate it. A task
file missing from a resolved folder therefore reads as a deletion, which a
destination holding that task refuses.
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
driver, and unsets filters, encoding and ident. Preparation leaves it unapplied.
The [explicit materializer](#materializing-a-selected-preparation) queries every
actual task path after applying the block and before writing shared files,
including overrides from Git-admin `info/attributes`. Preparation does not prove future filenames
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
Managed setup blocks may enlarge the 64 KiB inputs. Materialization checks the
complete published setup files against the recorded after-lengths and bytes;
it does not apply the before-file cap to those derived outputs or truncate them.
The edits are not applied: no `.engram` files, index, branch, global configuration
or SQLite rows change. Preparation enables no sync and introduces no remote
relay, sparse enrollment, commit or network operation.

A matching disclosure assertion and repository basis returns the original
operation and unchanged intent bytes. The first caller's attribution (including
actor context and source skill) is recorded separately; a new process session
does not change retry identity. Setup preparation and materialization preserve
actor context verbatim, including whitespace and text longer than 256 bytes.
When present, it must be nonblank, at most 4096 UTF-8 bytes, and contain no
control characters. Invalid context refuses before creating setup or
materialization state; it is never silently normalized. Both CLI
`--actor-context` and `ENGRAM_ACTOR_CONTEXT` use this contract. An exactly empty
actor context is absent; whitespace-only context is refused. These commands
have no MCP tool. Previously recorded attribution remains unchanged on retry.
A different or unowned record refuses without
replacement. Moving HEAD makes the recorded basis stale; this preparation slice
does not replace or retire it.
An immediately contended private lock refuses; it is not retried or reclaimed
by age. Lock ownership bytes and the intent are published with no-replace hard
links of complete private pending
file; unsupported filesystems refuse. An interruption can leave an owned
pending file, which is not a published intent. This is process-interruption
safety, not a power-loss guarantee. Materialization revalidates the recorded
basis before applying edits or shared task bytes.

## Materializing a selected preparation

```text
engram --home LOCAL_HOME --project-file MAIN/.engram-project task-sync setup-materialize MAIN --setup-operation SETUP_UUID --prepared-operation PREPARED_UUID --progress-directory PROGRESS --candidate-directory CANDIDATE
```

This explicit main-checkout operation consumes the immutable setup intent and
one completed, matching prepared candidate. The shared `LocalWorkService`
entrypoint performs the same actual repository admission as the CLI. There is
no separate materialization MCP tool. Selection never falls back to the latest
operation, recaptures SQLite or reserializes a task document. Recovery and exact
repeat require the same recorded absolute path spellings for the repository,
progress and candidate selection; reuse the original command paths even when
another spelling names the same filesystem objects. The CLI refuses `..` parent
components in selection paths before materialization; supply paths without
parent components on every platform. Candidate documents
must already use LF bytes; they are refused rather than normalized.

The whole candidate passes current disclosure admission before any new private
operation or public write. Tasks labelled `restricted`, `secret` or `secret-ref`
are refused, including tasks without notes. Retained notes and activity receive
their separate current admission checks. A refusal reports bounded task IDs,
counts and a static reason, without quoting field values. The receipt reports
the development no-op redactor, recorded note/activity omissions, asserted first
attribution and the count of omitted linked worktrees. Assertions and quoted
instructions are not authenticated human consent or real DLP.

The repository must retain its main nonsparse checkout, attached branch, HEAD,
project marker and relevant index basis. Setup and shared-path ASCII case
aliases (including `Project.json` or `Tasks` inside an existing `.engram`),
links/reparse points, suppressed index inspection and unowned occupancy
refuse. After applying only the recorded root ignore and attribute edits, the
operation queries effective text/LF, built-in diff/merge, filter, encoding,
ident and ignore policy on **every actual** project/task filename, including
Git-admin overrides. Unsafe policy refuses before shared task publication.

An existing process-held setup lock is acquired without initialization; a
contender creates nothing. An exclusive private operation directory in Git
administration retains the exact raw candidate separately from publication
sources, an immutable bounded journal, physical directory identities and first
attribution. Git administration and checkout must share a filesystem that
supports hard links. The Windows dependency exposes a volume serial and a file
index reported as 64-bit, compared together with exact bytes and inventory.
Filesystems with wider identities, such as ReFS with 128-bit IDs, can report
colliding indices for distinct objects. An equal-byte false match on a published
file can misreport it as already published; that check neither replaces nor
deletes the file. Before-file admission also uses index plus exact before bytes:
a false match can apply the declared setup edit to a distinct file whose bytes
are identical to the approved before-content. Its displaced bytes are always
retained privately; the journal records their location and approved before bytes.
An unowned empty directory is never adopted or removed. A saved directory ID
can collide or be reused after the original disappears, so repeat can misreport
another empty directory as equal or complete while preserving it. Mismatched
or unavailable IDs refuse. These comparisons are not collision-free ownership
proofs. No ReFS or UNC runtime assurance is claimed, and no interpreter or
volume-management subprocess is used.

Existing setup files move into fresh private replacement attempts. The arrived
file must match its recorded physical identity and before bytes; an after file
is then linked without replacing a public target. If a concurrent edit was
moved, compensation links those displaced bytes back without replacing another
target. A lost compensation reports `restore_incomplete` and names the retained
private location. Both displaced and newly occupied public bytes are preserved.
An attempt interrupted before its complete ownership marker is preserved and
refused; names, age or equal bytes never justify adoption or cleanup.

Shared directories are staged privately and renamed into the absent target.
Existing `.engram` private contents are preserved: only the tasks subtree and
project file are published there. The accepted rename race can replace an
empty target directory; a nonempty target fails without a delete fallback.
An independently present empty tasks directory with an expected empty export
is preserved and reported as equal unowned content. A nonempty expectation
refuses that directory. Independent files with different reported identities
remain unowned and refuse, including equal-byte files.
An owned freshly published `.engram` also permits unrelated private siblings
during recovery and exact repeat. Its directory identity, canonical reserved
names, project ownership and exact tasks inventory are still checked; a case
alias of `project.json` or `tasks` refuses even beside an unrelated sibling.
Before admitting new state, bounded directory scans reserve room for every
missing declared root or shared entry. An operation that would exceed the
100,000-entry scan limit refuses before publication, so its own added paths
cannot exceed the recovery and repeat reader's bound.

Recoverable publication boundaries use retained physical source links and exact
bytes. After admission, recovery needs neither the original B1 generation nor
a current SQLite snapshot, and can finish after opt-out. Completion records the
observed public directory and project/task identities; exact repeat rechecks
those identities, bytes, inventory and repository policy without writing.
Missing or edited completed state and detected independent replacement refuse.
The reported-ID collision and reuse limitations above also apply to repeat.
Ordinary public edits are covered; hostile same-user tampering with the private
operation directory, multi-file atomicity and power-loss durability are not promised.

### Resolving a materialization refusal

Before admitting `journal.json`, materialization creates both private replacement
directories and complete no-replace plan markers. The journal binds the directory
and plan identities. After admission, each setup edit uses only these bound
plans; no replacement directory or plan is created between public edits. A
missing or replaced plan after admission is a refusal for changed private state,
not an expected interruption state.

The admission journal binds an exclusively created private completion-recovery
directory and source file. If completion preparation is interrupted after public
publication, retry preserves the original `.complete.pending` bytes and identity.
It verifies the recovery source's complete prefix against the expected completion,
appends only the missing suffix, syncs it and links the complete marker without
replacement. Another interruption resumes the same bound file. A changed prefix,
identity or inventory refuses with all witnesses retained. This admitted recovery
also works after opt-out; new materialization still requires current opt-in.

Retain the printed operation paths and refusal before changing anything. Stop
concurrent writers while making a manual repair, and preserve copies of the
private operation, setup intent and public files. The command never removes an
incomplete operation or infers ownership from an empty directory.

If an interruption or refusal occurred before `journal.json` was published,
including during replacement-plan preparation, automatic recovery refuses that
incomplete private attempt. The refusal prints the full operation path, of the
form `GIT_DIR/engram-task-sync-materialize-<setup-operation>-<prepared-operation>`,
and directs the operator to preserve it, relocate the whole directory within
Git administration, rerun setup preparation, then materialize a fresh attempt.
With an unchanged repository basis, that preparation repeats the existing setup
intent before fresh materialization; it does not create a second setup intent.
This is an explicit operator repair, not an automatic retry or a retire command.
Do not delete or adopt an unfamiliar entry to make a refusal disappear.

At `private-created`, `replace-created-0`, and `replace-created-1`, no public
setup replacement has occurred: retain unrelated public files exactly as they
are. At a later pre-journal refusal, inspect the actual retained state before
repairing it. Preserve any displaced originals and unrelated public edits
separately; never blindly restore recorded bytes over a new public occupant.
Examples include setup-byte drift during admission, unavailable hard links, or
a failed Git admission query after private initialization began. A changed
index, branch or shared tree needs reconciliation before a new preparation;
repeating preparation does not normalize that drift.

After setup edits are published, `.gitignore` and `.gitattributes` are hard links
to the operation's retained `after` sources. To fix an attribute-policy refusal,
correct the separate override (for example Git-admin `info/attributes`) while
leaving those managed files intact. An in-place edit to a managed file also
changes its retained source; restore the exact `after` bytes recorded in the
immutable journal, in place, before retrying. If an editor replaced the public
file, preserve that replacement separately and explicitly restore the retained
source's hard link without overwriting it. A refusal concerning displaced bytes
keeps those bytes at the reported private path; `restore_incomplete=false`
means that observed displaced object already occupies the public path, including
on a later retry. These repairs require inspecting the actual retained state;
there is no general retire or replacement-materialization command.
If a public setup path becomes occupied after its original was moved, the
refusal names both that path and the retained displaced path. Preserve both
objects and reconcile the public entry explicitly before retrying; publication
never overwrites the new occupant.

This operation changes no SQLite rows/schema/projections, Git index, branch,
configuration or commits. It enrolls no linked worktree, runs no network relay
and adds no retire/replace or sparse-enrollment command. Disable changes only
the private opt-in setting and removes no shared files.

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
[folder exchange](#git-folder-exchange) describes. The initial target task
graph must then be empty, or already hold exactly the imported planning state,
which changes nothing. A different candidate or missing existing task refuses.
Existing native work refuses even if its editable fields look equal. Dry-run
executes the same inserts and constraint checks, then rolls back, retaining no
task, origin, baseline or execution records. Store initialization is a separate
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
| `task_sync_native_state` | The target already holds native work or execution state. |
| `task_sync_divergent_planning` | The candidate changes or adds to the planning, notes or activity the store already holds without removing a task; the destination's planning differs from its selected basis; the basis or export note selection differs from the folder's or the baseline's; or export meets native activity that conflicts with a retained entry. |
| `task_sync_removed_planning` | The candidate omits a task the store already holds. |
| `task_sync_id_collision` | A candidate task, blocker, note or activity id already belongs to another record, task or project. |
| `task_sync_invalid_graph` | The candidate is malformed, or fails its graph, reference or disclosure admission; or a folder envelope or basis file is malformed, incomplete or inconsistent with its own fingerprint. |
| `task_sync_origin_mismatch` | The candidate or the selected basis names another project, an import origin binding differs, or export meets an externally imported item. |
| `task_sync_destination_occupied` | The export destination already contains a different candidate. |
| `task_sync_destination_appeared` | The export destination appeared while the export was prepared. |
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
