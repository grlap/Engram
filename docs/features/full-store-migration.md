# Full store migration

This feature moves a complete store across an explicitly supported durable
format change. It is separate from [graph save and load](work-graph-snapshot.md).
A graph snapshot is not a full migration archive.

The [coordinated upgrade state contract](coordinated-upgrade-state-machine.md)
defines transition cases, invariants and boundary-test expectations. It guides
the implementation and does not claim that every case is already satisfied;
this brief owns the operator workflow and capability limitations.

## Current implementation

This section describes the current repository implementation: import supports
the current profile and the aggregate-root predecessor. An earlier bounded
full-import stage, including the current-profile extension and independent
comparator, was accepted after validation and review. That recorded acceptance
does not cover later changes or establish arbitrary profile support; the
extension is not yet installed. The broader
[supported-profile import and offline upgrade contract](#approved-supported-profile-import-and-offline-upgrade)
below was approved on 2026-09-14 and is under implementation; it is not yet an
installed capability. Commands in this section do not implement that upgrade
or establish exclusive access to an active store.

The exporter, archive check, source comparison, raw layout reconstruction and
supported-profile importer are implemented. Before publishing a result, import
requires strict current opening, integrity checks and pending-delivery validation.
An export alone is not evidence of a successful import or installation.
Do not install an archive as an Engram database.

```text
engram migration export --database SOURCE.db --out EXPORT.db
engram migration verify --archive EXPORT.db
engram migration compare --database SOURCE.db --archive EXPORT.db
engram migration unpack --archive EXPORT.db --out SCRATCH.db
engram migration import --archive EXPORT.db --out CURRENT.db
```

These commands use explicit paths. They do not resolve the active project or
create its home. Export opens the source read-only and reads all metadata and
rows inside one SQLite transaction. It does not call the current-build store
opener or decode old domain objects. SQLite may still update WAL coordination
sidecars on a read-only open. Use a preserved, coherent backup and a private
working copy for migration tests. Do not copy only the main file of a live WAL
store; make a coherent backup first.

Export writes a unique staging file beside the destination. It commits, checks
and syncs that file before publishing it with a non-replacing hard link. An
existing destination is always refused. Filesystems without hard-link support
are refused; there is no partially visible copy fallback.

`unpack` is a staging step, not an upgrade. It reconstructs the **source format**
in a new scratch file and checks every raw row against the archive. It accepts
only the compiled current schema or the explicitly supported aggregate-root
predecessor. It uses locally derived DDL, not SQL executed from the archive.
Unknown profiles are refused. Its output explicitly says that nothing was
upgraded or installed. Do not activate that scratch copy's claims or grants.
The exact `sqlite_sequence` high-water marks are restored after row insertion;
they are not inferred from surviving row IDs.

## What the archive contains

The archive is a SQLite container with a fixed migration schema. Source SQL is
stored as metadata, never executed as archive code. The manifest lists every
source table, including empty and unknown tables, plus indexes, triggers, views,
column definitions and foreign keys. It also records source encoding and schema
pragma values.

Rows preserve SQLite types: null, signed 64-bit integer, binary floating-point
value, text bytes and BLOB bytes. Text and BLOB remain distinct, including empty
values. Canonical objects remain opaque BLOBs with their original bytes and
stored identities. Rowid values are retained where applicable. Unsupported
representations, such as a table that hides all three rowid aliases, cause a
refusal rather than a silent omission.

Virtual-table control columns are described but not evaluated as stored cells.
Visible virtual-table rows and every shadow table are exported. SQLite's own
table classification is recorded. FTS physical rows are retained in the archive
for completeness; a later import must compare rebuilt FTS by logical content,
not by physical row layout.

There is no redaction. The archive includes private scratch, restricted bodies,
retired history, session state, execution state, claims, leases and grants when
present. Protect it like the source store. Possession of an archive does not
grant authority on another host.

## What validation proves

`verify` checks the archive schema, manifest identity, complete category set,
dense archive row positions, typed cell frames, row counts and row-content
identities. It detects missing or changed data relative to the manifest.

It cannot detect an archive whose data and manifest were both rewritten to
describe an incomplete export. `compare` reads the fixed source again and
compares all categories, metadata and typed row bytes against that separate
read cut. A live source change after export can therefore make comparison fail.
Independent inventory and semantic checks remain part of migration acceptance.

The result lists empty tables explicitly. Their schema coverage is checked,
but an empty category has no real rows with which to test its import transform.
Synthetic populated fixtures are needed for those categories.

### Independent same-profile comparison

The repository includes a separate operator check requiring Node.js 24. Run it
on two distinct, isolated, coherent SQLite copies with the same schema/profile.
Keep both copies fixed throughout comparison and protect them like the private
source store; do not point this command at changing live databases.

```text
node scripts/compare-migration-stores.mjs SOURCE_COPY.db IMPORTED_COPY.db
node --test scripts/compare-migration-stores.test.mjs
```

The comparator reads both databases directly, without consuming the migration
manifest or using the exporter row encoder. It compares schema/table inventory,
typed row content (including original text bytes), rowids, sequence values,
empty tables, and stored migration provenance. FTS is compared by logical
content; exempt physical shadow tables are listed in
`rebuilt_fts_physical_tables`. A successful result reports `equal: true` with
its comparison scope and per-table accounting.

This is same-profile logical equality, not proof of equality across a transformed
profile, doctor health, successful resume, or safe activation. It does not replace
the conversion-specific checks below or establish that the broader importer or
offline upgrade is ready.

## Import boundary

The CLI and `import_archive` recognize two explicit source profiles: `current`
and `aggregate-root-v1`. Ordinary store open remains strict and does not run
migrations. No promise is made that an arbitrary future schema can be imported.

Current-profile import requires all six migration provenance tables, whether
empty or populated. The otherwise current-shaped variant missing all six is
explicitly refused by import; `unpack` can still reconstruct that source layout.
Unknown or partially matching layouts are refused.

In current mode, durable table rows are copied unchanged, including restored
records, evidence, observations, private and operational state, and existing
`migration_*` provenance. No new identity mappings are created. Preserving prior
provenance in this mode is not an implementation of composition across future
transformed profiles. FTS tables are rebuilt and checked by logical content.
The import report names its `profile` and gives every source table a disposition:
`unchanged`, `transform`, or `rebuild`. It reports `installed: false`; publishing
the validated output file does not activate it.
The `conversion` counters describe conversion work, not copied-row totals:
current-profile import reports zero there even when it copies canonical objects.

### Aggregate-root conversion

Every original canonical object must remain recoverable. New representations
must use explicit identity mappings and named transformations. Historical prose
must not be rewritten by a generic hash replacement. Data that needs no change
must also be accounted for. Sessions, delivery receipts and execution authority
are part of this accounting, not disposable caches.

Aggregate-root conversion retains the original manifest, every original canonical object and
the original rows of all other tables in durable migration tables. A total
source-to-target map includes unchanged objects. Each map entry has a canonical
binding that names its source, target, kind and conversion profile. This is
content-integrity evidence, not authentication. Future migration profiles must
preserve and compose these records. The aggregate-root path refuses populated
migration provenance rather than inventing that composition; this refusal does
not apply to a supported current-profile source carrying prior provenance.

Full root aggregates become root-state deltas. A converted completion seal
references the exact observed pre-completion root state, including contributors,
contributions and waivers. The importer refuses a missing or mismatched
predecessor; it does not infer one by subtracting from a later state. Three known
historical omissions stay omitted in converted bytes: `work.restored` in events,
and `restored` and `restored_child_completions` in seals. Their existing read
defaults remain false and empty. Other unknown or implicit shape changes refuse.

Staged delivery and protocol result bytes remain unchanged. Selected runtime
read boundaries resolve their old canonical references through the recorded
map. Ordinary canonical object lookup does not become an alias lookup.
Missing or damaged required mapping provenance refuses that historical read.
Doctor checks the full retained provenance; ordinary open does not scan all
original history.

An imported pending page can lack the derived `from_current_session` bit.
Absence is ambiguous: current writers also omit false. Import derives a missing
true bit from the verified canonical source actor, not from an assumed payload
age. Each affected page receives one immutable migration audit with its source
row, project, session, token, exact interval, original payload hash, and affected
feed positions. Only import writes these audits. Both import validation and
runtime replay verify that binding and derive attribution in memory.
Core replay can therefore return the existing `from_current_session` field as
true where the original payload omitted it; no new response field is added.
Original payload bytes, hash and token remain unchanged. Explicit contradictory
bits refuse import. New pages and pages without this exact audit retain the normal
strict attribution check. Doctor checks audit completeness against the original
session rows, including after those pending pages have been acknowledged.

There is one explicit storage/core replay exception. Pre-migration `complete_work` results
contain an old seal by value. Replaying their original key returns the same
historical facts in the converted seal representation, without completing the
work again. It does not return the original response bytes. Those full original
bytes and their digest remain in a per-key audit alongside the converted result
and the source/target seal addresses. Missing audit or mapping refuses replay.
This exception does not apply to new operations or other replay families.
In particular, the ambient `work_complete` protocol receipt retains its historical
seal reference; replay does not replace that field with the mapped target seal.
Reads of its acceptance evidence resolve the reference through migration mapping
without rewriting the stored protocol result.

### Validation and publication for both profiles

Import builds an unpublished file, checks foreign keys and logical FTS content,
then requires strict current opening, doctor without repair, and validation of
pending delivery payloads. Only then does it publish with a non-replacing hard
link. Preserved authority records are data for controlled offline same-host
resume. Never activate the new copy alongside its source or on a second host.

Acceptance requires a real complete export-to-empty-store import, content and
relation comparison, and successful ordinary resume, history and doctor without
repair. Fault injection must leave no partially installed destination. Actual
installation is a separate controlled same-host offline operation with rollback.

## Approved supported-profile import and offline upgrade

Status: supported-profile import accepted but not installed; coordinated-downtime
upgrade is implemented with final corrections, gates, and review still pending.
Neither statement is a claim of installed upgrade behavior.
This extends full SQLite archives and explicit imports; it does not replace
them with graph snapshots or JSON-only export. The scoped exception to the
prerelease migration policy is reproduced in both agent instruction files.
It does not permit implicit migration during ordinary open or promise support
for every past or future schema.

Preserving and composing prior migration history is required, not prohibited by
the policy against unsupported migration chains. The approved upgrade boundary
uses operator-coordinated downtime. It requires no new TermAl integration or
Engram admission lock. The operator stops all affected consumers and keeps them
stopped through verification and the recorded resume decision. This is an
operational precondition, not machine-enforced exclusion: the upgrader does not
prevent an unrelated process or an old CLI binary from opening the store.

### Supported profiles and complete accounting

The initial support floor is the current durable layout, the existing
aggregate-root predecessor, and explicitly recognized variants carrying earlier
migration provenance. Each accepted profile needs an implementation-owned
structural definition and populated fixtures. A schema marker, build label, or
checksum alone is not a profile definition. Source/archive integrity and profile
recognition are separate checks. Unknown layouts or representations are refused
before target publication; refusal leaves the source and complete archive intact.

Export must account for every source table, including empty tables, metadata,
typed rows, private data, history, operational state, and prior migration records.
Import must give every source category a named disposition: preserved unchanged,
transformed under a specified profile, or rebuilt as a derived projection whose
logical content is verified. Original representations remain recoverable from
retained migration data. No table is silently dropped as a cache, and no unknown
table or column is silently accepted by a generic copy. Derived FTS physical
layout may change; its source rows remain in the archive and its logical content
must compare. Row counts alone do not prove content equality.

Repeated supported conversions must retain earlier manifests, canonical bytes,
identity bindings, row provenance, and protocol replay audits. New mappings name
their source profile and transformation and compose with prior mappings without
overwriting history or treating arbitrary canonical lookup as an alias lookup.
Every retained executable reference and promised historical read/replay must
resolve under the resulting provenance. Missing, ambiguous, contradictory, or
damaged required mappings refuse import. Existing replay exceptions above remain
explicit; a new profile cannot silently widen them or rewrite stored prose.

Preserving authority records is required for controlled same-host recovery. It
does not renew expired authority, bypass existing session/claim checks, authorize
a second active copy, or grant portable cross-host authority.

### Coordinated downtime

Before taking the backup, the operator stops existing CLI, MCP, background, and
other store users, including processes with an open connection, and prevents
their relaunch operationally. Maintain this downtime through export, import,
verification, activation, and the durable resume decision. Changing a schema
marker or observing zero processes once does not establish this continuing
condition. Only the migration worker may access the working files during this
interval. If downtime cannot be maintained, do not switch or resume consumers.

The same condition applies after an interrupted upgrade: keep consumers stopped
while inspecting and recovering the recorded operation. The journal records
state; it neither stops processes nor blocks store access. The upgrader must
disclose this precondition without claiming that an acknowledgement proves it.

The operation names one database and one operation directory. It does not resolve
the active project or add a separate project/host-policy binding; supported
source profiles are admitted by the importer. Report the running executable's
identity and retain the old executable. Mutating actions require its SHA-256
identity to match the upgrader that ran `prepare`. This workflow does not replace an executable or change
`PATH`; the operator must select the compatible executable before resuming any
consumer, including other stores served by a shared executable. An archive or
unpublished imported file is never a second live ledger.

The operation directory contains the full private store and retained executable.
On Windows it inherits permissions from its parent; the upgrader does not set a
separate restrictive ACL. Place it under a home with protection equivalent to
the source store.

The operation directory and database must be on the same filesystem volume,
with hard-link support. Activation links the staged file into the live database
path and retains original components by rename; placing the operation on another
volume is unsupported. Choose this layout before `prepare`; successful
preparation alone does not establish that activation can cross volumes.

The activated database retains the staged file's owner, mode and ACL, not
necessarily the original database's access descriptor. On Unix, staging files
are created by the upgrader and set to mode `0600`; on Windows they inherit
permissions at their staging location. Publishing a hard link does not recreate
the file with the old database's permissions or the live directory's default
permissions. Account for the intended consumer identity and access when choosing
the upgrader identity and protected staging location; preserved database bytes
do not establish equivalent filesystem access.

The admission contract requires conservative overlap checks for the database
and its reserved sidecar paths on every platform and filesystem. Identical
components match directly; two ASCII components compare without case; differing
components with non-ASCII or undecodable names are potentially equal unless
another shared ASCII component proves divergence beyond case. This can refuse
distinct layouts even on case-sensitive filesystems; it is an admission rule,
not a claim to reproduce filesystem identity or case-folding semantics.
Place the operation in a separate sibling subtree whose distinguishing
components are ASCII and differ beyond case, while retaining the required
access protection and avoiding the database and its sidecars.

### Implemented CLI, pending acceptance

The following interface is implemented in the repository. It is not
yet an installed command set or evidence of completed validation. Run these
commands with the compatible candidate executable; paths below are placeholders.

```text
engram migration prepare --database SOURCE.db --operation OPERATION_DIR --old-executable OLD_ENGRAM --offline-confirmed
engram migration status --operation OPERATION_DIR
engram migration activate --operation OPERATION_DIR --offline-confirmed
engram migration rollback --operation OPERATION_DIR --offline-confirmed
engram migration finalize --operation OPERATION_DIR --offline-confirmed
engram migration recover --operation OPERATION_DIR --offline-confirmed
```

These are separate actions, not a script to run sequentially. `prepare` preserves
the source and prepares backup, archive, and validated target material. `status`
reports operation state. `activate` puts the verified target at the database path
while retaining the source and its sidecars. `rollback` restores the prior
database state only while the rollback window is open. `finalize` irreversibly
records permission to resume consumers and closes that window; it does not
restart the host. `recover` reconciles an interrupted operation with the actual
files and recorded state, refusing ambiguity rather than guessing.

While rollback is in progress, only `recover` or `rollback` may continue the
transition; `activate` and `finalize` must refuse. Once rollback completes, the
operation is terminal and cannot be activated again. The operator may then resume
old consumers with the matching retained executable. Later legitimate writes to
the restored database do not turn a completed rollback into an incomplete one.

A terminal phase is not permission to remove operation artifacts. `status` and
`recover` still require the journal and the intact backup, archive, candidate,
and retained old executable, including their recorded identities. Preserve the
operation directory after finalization or rollback if those commands are needed.

Under maintained downtime, `prepare` copies the source main file and existing
WAL, SHM, and rollback-journal sidecars, then checkpoints the copy. It does not
use a live-source `VACUUM` as its backup mechanism. Keeping the source database
in place is not a promise that SQLite sidecar housekeeping cannot occur. Inspect
the detached backup and candidate after preparation instead of reopening the
source during the offline operation.

Reports expose the phase, paths, SHA-256 identities, and `rollback_closed`.
`current_executable` and `current_executable_sha256` identify the preparer's
running upgrader. `selected_executable` names the supplied old executable path;
its bytes are retained in the operation directory as `old-executable`, never
executed or installed by this workflow. During `prepare`, path admission applies
both to `--old-executable` and to the running upgrader path reported by
`std::env::current_exe()`. A symlink or reparse-point leaf is refused; on Windows,
symlink or reparse-point ancestors are also refused. On Unix, this ancestor
refusal is not applied and existing paths are canonicalized. The running path
is supplied by the runtime and may differ from the command used to launch it;
this is not a guarantee that every launch alias is detected or rejected.

Before `prepare`, place the candidate executable in a real directory and invoke
its concrete file path; on Windows, use a directory tree without junctions or
other reparse points. Supply the old executable by a concrete admitted file
path too. If admission rejects the running upgrader, changing only
`--old-executable` will not resolve it. The workflow does not discover an
executable behind a launcher or install these files. Retaining supplied old
bytes does not prove that they are the compatible executable to resume.
Reports also disclose
`operational_precondition`, `process_interruption_recovery`, and
`power_loss_durability`; the last is `"unavailable"`.
`--offline-confirmed` records the operator's assertion of maintained downtime;
it does not check or enforce exclusion. Do not resume consumers against the new
database until verification and finalization have succeeded; successful terminal
rollback instead permits resuming the old database as described above.

Between `activate` and `finalize`, verify the active target with `migration status`
and file-identity checks, relying on the candidate validation already performed
during preparation. Do not open that target with ordinary Engram APIs or
`doctor`: even an intended read can change SQLite mode or sidecars and invalidate
the expected candidate identity. Any additional strict-open or `doctor` check
must use a detached disposable copy, including any required sidecars, without
opening the active target or the preserved candidate for ordinary reads.
If the target was opened prematurely and identity checks fail, preserve the
files and maintained downtime for diagnosis. There is no force-finalize or
unknown-write bypass, and a mode change is not permission to bless arbitrary
changes as harmless.

### Backup, activation, and crash recovery

The operation directory holds journal records with sequence and transition kind,
database path, artifact and executable identities, and source-file identities.
These records do not add project/policy or source/target-profile report fields.
Journal recovery must inspect the actual files as well as the last
recorded phase: a crash can occur between a filesystem effect and its completion
record. Unknown or contradictory state refuses automated continuation and
requires reconciliation; the operator keeps consumers stopped. Recovery must
not guess which database is authoritative.

Candidate publication first copies to a private staging file. After a process
interruption during that copy, recovery may remove and recreate the partial
file only after verifying that its bytes are a prefix of the preserved candidate
and that the retained source, live files, and transition state account for the
cleanup. A foreign or inconsistent staging file is preserved and refused.
Completed staging files are published through hard links without replacing an
existing destination; the filesystem must support these links.

Journal appends write and sync a temporary record before publishing its numbered
name through a hard link. Recovery can clean up the reserved temporary name
when it matches an already published record, or an unpublished temporary file whose
bytes match a prefix of a legal next record derived from the intact journal.
For an already published record, Unix checks device and inode identity;
Windows checks regular-file contents by SHA-256, which does not prove that the
two names reference the same filesystem object.
This does not authorize dropping a malformed published record. If the initial
`Prepared` record was never published, including an interruption leaving its
temporary file, recovery reports incomplete preparation and preserves the files
for diagnosis instead of reconstructing unrecorded identities.

Process-interruption recovery and power-loss durability are different claims.
The validation scope must name the process interruptions actually exercised.
Syncing file contents alone does not prove directory-entry or rename durability;
do not claim a power-loss-safe switch without supporting directory/filesystem
evidence. A torn or unreadable journal must not reopen a finalized rollback
window; uncertainty requires reconciliation while consumers remain stopped.
Preserve all remaining files and journal records for diagnosis. Never delete the
last journal record to make recovery proceed or infer that rollback is safe from
an incomplete record sequence. Process-interruption recovery depends on intact,
consistent records and accounted-for files; it does not promise automatic repair
of a torn journal.
The test suite includes returned-error injection and child-process exit with
code 73 at retain, restore, and candidate-publication effects. These simulated
interruptions are not actual power cuts. They do not establish POSIX behavior
without an executed POSIX validation run.

1. Establish coordinated downtime. Create and verify a coherent backup of the whole
   source store, including committed WAL data, and preserve the matching old
   executable. Copying only a live database's main file is not a backup.
2. Export that fixed backup to the full archive. Verify it and compare it against
   the same backup cut, including schema, all categories, typed bytes, and empty
   categories. Keep backup and archive separate from the destination.
3. Import into a new unpublished target. Check complete table dispositions,
   provenance composition, references, foreign keys, logical projections,
   pending deliveries, and historical replay. Require strict target opening and
   doctor without repair. A failed check must not publish or activate the target.
4. Record readiness and activate the verified target during maintained downtime.
   Report the compatible running executable; do not replace executable files.
   Database, WAL/SHM sidecars, the operator's executable selection,
   and journal state must recover coherently. Do not assume that two independent
   file renames are one atomic operation. Retain the source and backup.
5. Verify the activated target using `migration status` and file identities while
   normal consumers remain stopped. Additional strict-open or `doctor` checks
   run only on a detached disposable copy. Before ordinary target opens or any
   new writes, close the rollback window with `finalize`, then
   let the operator resume consumers against the selected target. A crash after that decision
   must not reopen the rollback window merely because no writes were observed.

Rollback before the resume decision restores the verified old database state
under the same maintained downtime. The operator selects the retained matching
old executable before resuming; rollback does not install it. After new writes
are admitted, restoring an
old backup can discard work; it is not automatic rollback. Recovery then needs
an explicit reconciliation or forward-recovery procedure that accounts for those
writes. The journal is an audit and recovery aid, not by itself a store lock.

### Acceptance of the broader implementation

For each declared supported profile, require a complete export/import and
source-to-target comparison on populated data, including already migrated
provenance, private state, empty categories, pending deliveries, history, and
ordinary resume. Exercise unsupported layouts, corrupt mappings, an existing
destination, and failures at publication/activation boundaries. Crash recovery
must be checked before and after each durable transition, including the boundary
where new writes become admissible. Validate the workflow under coordinated
downtime, its explicit operator precondition, and refusal to continue on an
ambiguous recovery state. Do not claim that tests prove machine-enforced
exclusion of other processes. The operator must keep both copies from being
simultaneously active. Record which operating systems and
filesystems were actually exercised. Passing archive verification alone is not
evidence of successful import, upgrade, or safe rollback.
