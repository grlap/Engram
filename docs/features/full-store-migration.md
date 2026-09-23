# Full store migration

A store moves to a new durable format in two plain steps: export every row to a
JSON file, then import that file into a new store in the current format. This
is separate from [graph save and load](work-graph-snapshot.md), which shares a
selected part of a project's work and is not a whole store.

```text
engram migration export --database OLD.db --out STORE.jsonl
engram migration import --file STORE.jsonl --out NEW.db
```

Both commands take explicit paths. They never resolve the active project,
create a home, or replace an existing file. Neither installs or activates
anything: the operator decides when the new file becomes the store.

## What the design rests on

- **A record's id is a random UUID**, minted when the record is stored. It does
  not depend on the record's bytes. Links between records are those ids.
- **Ids travel unchanged.** Import never recomputes an id and never rewrites a
  link because a record changed shape. A record may be re-expressed in a new
  shape under the id it already has.
- **Nothing of the old format is kept.** The new store holds the current format
  only. The old file is the backup.
- **A hash is a content fingerprint**, used to compare content: idempotency
  intents, snapshot bodies, build identity. It is never an id, a link, or a
  corruption check. SQLite guards the bytes on disk; the doctor checks what the
  rows mean.

Ids written before this design are 64 hex digits; minted ids are 32. Both are
opaque strings and live side by side in one store.

## Export

Export opens the source read-only and reads every table inside one SQLite
transaction, so the file is one coherent moment of the store. It does not use
the current-build store opener, so it reads a store that this build would
refuse to open. SQLite may still touch WAL coordination sidecars on a read-only
open; export from a coherent copy when the store is in use.

The file is JSON Lines:

```text
{"engram_export":{"format":"engram-json-export","exported_at":"…","tables":[{"name":"objects","columns":["object_id","object_kind","canonical_json","created_at"],"rows":3}],"sequences":{"control_changes":48},"left_out":[…]}}
{"row":{"table":"objects","values":{"object_id":"…","object_kind":"work_event","canonical_json":{"json":{…}},"created_at":"…"}}}
{"end":{"rows":3}}
```

Each value keeps its SQLite storage class. Text is a JSON string, integers and
reals are numbers, and a blob is an object that names how it is written:

| Blob content | Written as |
|---|---|
| JSON that re-serializes to the same bytes | `{"json": …}`, nested and readable in place |
| other UTF-8 text | `{"text": "…"}` |
| anything else | `{"hex": "…"}` |

Rows of a table that keeps insertion order are written in that order, so a
query for "the latest row" finds the same row afterwards. `AUTOINCREMENT`
high-water marks travel in the header, so no id is handed out twice.

A search index and the shadow tables SQLite keeps for it are left out and
rebuilt by import. Which tables those are comes from SQLite's own
classification, never from a name: an ordinary table that merely looks like part
of an index, such as `object_fts_notes`, is copied like any other, and a virtual
table this build does not know — even one whose name begins with a known index —
is refused by name rather than dropped.

Indexes and triggers are not carried either, because the new store's schema is
authoritative and no source DDL is ever executed. One this build declares is
simply recreated there. One it does not declare is named: an undeclared index is
derived data and is reported as left out, while an undeclared trigger is
behaviour the new store would not reproduce, so export refuses it.

The report names every table and schema object that was left out, with its row
count and the reason. The file holds private scratch, restricted bodies and host
authority records. Protect it like the store itself.

## Import

Import creates a new store with the current schema, then inserts the file's
rows by column name inside one transaction. References between rows are checked
once, at commit. It then rebuilds the search indexes and runs the full doctor.
The new file is published, by a non-replacing hard link, only when:

- the file has its header and end line and every declared row count matches;
- every table and column has a place in the current format or is explicitly
  named as retired below;
- no reference between rows is broken;
- SQLite's integrity check passes and the doctor finds the store healthy;
- every delivery page staged but not yet acknowledged can be admitted (below).

Anything else is refused with the table, column or record named, and no output
file exists afterwards. A refusal names the place and the shape it met, never
the value in a cell: the file holds private bodies, and a refusal reaches the
operator's terminal. A table or column that the current format has no place
for is never dropped in silence. Explicit retirement is limited to:

- `work_session_state.tentative_delivery_payload_hash`: reported under
  `retired_fields` with its non-null value count; the rest of each row is
  imported.
- Uncompared control checksums: `control_turn_grants.grant_hash`,
  `control_turn_grant_supersessions.supersession_hash`, and `result_hash` in
  `control_operation_results` and `control_policy_operation_results`.
  Each is reported under `retired_fields` with its non-null value count.
  Their JSON payloads, replay intents and semantic validation are retained.
  The turn-result decision and replacement-decision fingerprints are not
  retired: supersession validation still compares those bindings.
- `task_claims`, `task_claim_intents`, and `publication_intents`: obsolete
  whole-task advisory claims and unwired publication scaffolding. Import reports
  each table under `left_out` with its row count and retirement reason. It does
  not recreate those tables or turn their rows into live work claims.
- `control_observations`, `memory_contradictions`,
  `memory_contradiction_edges`, and `contradiction_intents`: shadow turn
  observations and memory contradictions, which no production path ever
  wrote. Import reports each under `left_out` with its row count and does not
  recreate it.
- `task_participants` and `session_bindings`: the redundant compatibility-task
  roster and binding projection. Current membership comes directly from
  `control_sessions`. Import reports their row counts and refuses a binding
  not represented by the retained control session, rather than silently losing
  access to its scope. Conversely, every retained control session must have
  both a matching participant and binding row in the old format; inconsistent
  membership refuses rather than silently granting access to a scope.
- `tasks.state`, `tasks.event_cursor`, `tasks.created_at_ms`, and
  `tasks.updated_at_ms`: redundant task lifecycle metadata, reported with
  non-null value counts. Only the supported `active` state is admitted.

Resource-lease retirement is explicit: only an empty `control_work_leases`
table is omitted, with its row count reported. Empty `basis.leases` fields
in saved grants and decision copies are removed, and decision/supersession
content comparisons are updated together. A populated table or nonempty
lease basis refuses; neither is discarded. On this refusal, keep using the
source build and store; do not remove historical rows to force conversion.
Work claims and acceptance-revision
waiver history are preserved. Historical `lease_required` refusal decisions
remain readable and replay unchanged, including their attribution and compared
fingerprints. Current evaluation never produces that code; retaining saved
decisions does not restore resource leases or their host operations.
Historical `control_operation_results` receipts for `lease_acquire`,
`lease_release`, and `obligation_waive` are retained unchanged and checked by
doctor. These are saved history, not supported host operations or live authority.

The direct-binding conversion maps `tasks` to `control_anchors` with the same
id, project, external reference and title. `task_control_state.admission_epoch`
is merged into that anchor and its row count is reported under `left_out` with
the merge reason. `task_changes` becomes `control_changes`, preserving every
row, local cursor, object reference and sequence high-water mark. The importer
refuses overlapping old/new table declarations. This scope-binding change rewrites no canonical object,
and no old task table remains in the destination. Host `task_id` fields still
name the same scope, so retained grants, cursors and memory references
need no identity translation.

The header, copied rows, epochs and membership marks come from one validated
input stream. Conversion data is staged only in transaction-local temporary
tables and merged after row validation, regardless of source table order; import
does not reopen the file to obtain a second version of those values.

Export still carries all retired data unchanged. Import validates retired rows
and their declared counts, and refuses any column outside the explicit retired
column set for that table. Canonical objects, including historical task claim
and report records, keep their ids and links. Bytes change only for the named
stored reply fields below. The new database holds
only the current schema, not an archived copy of the retired tables. Keep the
source database and JSON export to retain those operational rows. Every other
unknown table or column still refuses by name. A store written by a design
this build no longer knows is refused the same way; the build that still
reads it is kept beside its backups.

The record-id cleanup explicitly maps the previous record-link column names
from `*_hash` to `*_id`, including `objects.object_hash` to `object_id`.
The handoff record link becomes `offer_object_id`, distinct from the existing
operational `offer_id`. Only the per-table mappings in the importer are
accepted; arbitrary suffix substitutions are not. Declaring both old and new
names for one destination column refuses. Id values are unchanged, and missing
required destination columns refuse by name. The Rust record-reference type
is `ObjectId`. Host and graph fields now use `object_id`, and full-memory
replies use `version_id` and `assertion_id`. No old-name aliases remain.
Coordinate the host protocol cutover with TermAl.

The one-time stored reply conversion renames `object_hash` to `object_id` in
`work_protocol_result` objects' `focus.history.items[].entry`, matching
`work_protocol_attempts.result_json`, staged work delivery `changes[].entry`,
and saved control grants' `delivery.delta.changes[]` (also inside saved turn
decisions). Checkpoint retry intents convert the tagged producer/environment
references in `verification_evidence`. Record ids, links, cursor positions and
delivery tokens stay unchanged. Arbitrary user bodies and unrelated object
kinds are not traversed. Both spellings at a converted field refuse.
Delivery-content, decision and checkpoint-intent fingerprints are recomputed
for the changed serialized content, with matching supersession comparison bindings updated;
these are comparisons, not record identities. Import reports the affected
table, column, object kind, field and value count under `rewritten_fields`.
There is no retained old-format copy in the destination.

This rename also changes the separate work-graph snapshot
format fingerprint. The generated JSON Schema includes the renamed Rust type's
definition name and references, so this cleanup deliberately changes that
fingerprint and the new build refuses older graph snapshot files. Use the
whole-store conversion here, then save a new graph file. For recovery when only
an old graph file remains, see the matching-old-build procedure in the
[snapshot format contract](work-graph-snapshot.md#file-layout). Never edit the
file's fingerprint to bypass the different-build refusal.

The work schema marker is not imported; the new store keeps its own.
`control_policy_state.schema_version` is checked explicitly and a mismatch
refuses by its named format-marker field, before publishing a destination.
A table this build drops and recreates whenever it repairs a store — the
project-memory state, the restored-record and observation projections of the
work schema, and the delivery bookkeeping — is derived state: export names
it as left out with its row count and does not write it, and repair derives
it again in the new store, so the report never counts a row the published
store does not hold. The delivery bookkeeping alone is not derived again; it
starts empty, which causes one harmless memory re-announcement per session.

The new file is built inside the private file the transfer reserves for it,
which is opened in place rather than deleted and recreated. On a system with
file modes, opening the reserved file in place keeps the store, the journals
written beside it, and the published link owner-only from before the first row
is written. An imported
store is therefore owner-only where a store created by `engram init` is not.

### A delivery page staged before the transfer

A session can hold a page of changes staged but not yet acknowledged. The next
core retry re-reads that page at the cursor it already confirmed, and the doctor
does not look at one, so import reads every session row that carries any part
of a pending delivery the way that retry reads it, before publishing anything.
A row the file left with its cursor, its delivery token and its page not
present together is refused by session, since that retry could not read it.

The page must say what the source says about which of its changes are the
receiving session's own. A page that claims a change the record attributes to
another session, or leaves out the attribution the record proves, is refused
by session; nothing is supplied or rewritten on its behalf. The report counts
the session rows checked.

### Changing the format

A format change edits the schema in place and teaches import the difference:
a renamed column is mapped, a reshaped record is rewritten from its nested JSON
under its existing id, and retired data is named in the explicit column/table
lists. Export never performs these changes.
There is no profile detection, no archive format, and no chain of versions to
maintain. What the current build cannot name, it refuses, and the operator
converts with the last build that could.

## Operator workflow

1. Stop every consumer of the store: hosts, agents and the CLI.
2. `engram migration export` from the store file. It reads the store's
   write-ahead log too, so committed data a crashed consumer left there is in
   the export; the report says how large that log is, and the command warns
   when it is not empty.
3. `engram migration import` into a new file with the new build. Import
   refuses a destination that has a `-wal`, `-shm` or `-journal` file beside
   it before it stages anything; the swap in the next step is the operator's
   and no check here can see it.
4. Inspect the import report, including `left_out`, `retired_fields` and
   `rewritten_fields`, before
   deciding to activate it. Move the old file aside as the backup **together
   with its `-wal` and `-shm` files**: they hold committed data until the next
   checkpoint and belong to that file alone. Put the new file in its place
   with nothing of the old one beside it; SQLite applies whatever log it
   finds at a database's name to that database.
5. Start the consumers again.

Current work claims, grants and delivery state retain their identities; the
explicit field conversions above update their supported shapes. The empty
resource-lease table and retired whole-task advisory claim tables are omitted
as specified above. The imported
store is the same store on the same host: do not run it beside its source,
and do not treat it as authority on another host.
