# Full store migration

This feature moves a complete store across an explicitly supported durable
format change. It is separate from [graph save and load](work-graph-snapshot.md).
A graph snapshot is not a full migration archive.

## Current implementation

The exporter, archive check, source comparison, raw layout reconstruction and
the aggregate-root importer are implemented. Before publishing a result, import
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

## Import boundary

The importer accepts only the explicit aggregate-root source profile. Ordinary
store open remains strict and does not run migrations. No promise is made that
an arbitrary future schema can be imported.

Every original canonical object must remain recoverable. New representations
must use explicit identity mappings and named transformations. Historical prose
must not be rewritten by a generic hash replacement. Data that needs no change
must also be accounted for. Sessions, delivery receipts and execution authority
are part of this accounting, not disposable caches.

The importer retains the original manifest, every original canonical object and
the original rows of all other tables in durable migration tables. A total
source-to-target map includes unchanged objects. Each map entry has a canonical
binding that names its source, target, kind and conversion profile. This is
content-integrity evidence, not authentication. Future migration profiles must
preserve and compose these records; this importer refuses an already migrated
source rather than inventing that composition.

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

There is one explicit replay exception. Pre-migration `complete_work` results
contain an old seal by value. Replaying their original key returns the same
historical facts in the converted seal representation, without completing the
work again. It does not return the original response bytes. Those full original
bytes and their digest remain in a per-key audit alongside the converted result
and the source/target seal addresses. Missing audit or mapping refuses replay.
This exception does not apply to new operations or other replay families.

Import builds an unpublished file, checks foreign keys and logical FTS content,
then requires strict current opening, doctor without repair, and validation of
pending delivery payloads. Only then does it publish with a non-replacing hard
link. Preserved authority records are data for controlled offline same-host
resume. Never activate the new copy alongside its source or on a second host.

Acceptance requires a real complete export-to-empty-store import, content and
relation comparison, and successful ordinary resume, history and doctor without
repair. Fault injection must leave no partially installed destination. Actual
installation is a separate controlled same-host offline operation with rollback.
