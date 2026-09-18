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
{"engram_export":{"format":"engram-json-export","exported_at":"…","tables":[{"name":"objects","columns":["object_hash","object_kind","canonical_json","created_at"],"rows":3}],"sequences":{"task_changes":48},"left_out":[…]}}
{"row":{"table":"objects","values":{"object_hash":"…","object_kind":"work_event","canonical_json":{"json":{…}},"created_at":"…"}}}
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
- every table and column in the file has a place in the current format;
- no reference between rows is broken;
- SQLite's integrity check passes and the doctor finds the store healthy;
- every delivery page staged but not yet acknowledged can be admitted (below).

Anything else is refused with the table, column or record named, and no output
file exists afterwards. A refusal names the place and the shape it met, never
the value in a cell: the file holds private bodies, and a refusal reaches the
operator's terminal. A table or column that the current format has no place
for is never dropped in silence. The one exception is a column this build has
explicitly retired, named in its retired-column list: it appears in the report
under `retired_fields` with the number of values it carried, its rows having
gone in without it. Any other unknown table or column refuses by name. Today
one column is retired: a fingerprint of the staged delivery page that nothing
compared. A store written by a design this build no longer knows is refused
the same way; the build that still reads it is kept beside its backups.

The store's own format marker is not imported; the new store keeps its own.
A table this build drops and recreates whenever it repairs a store — the
project-memory state and the delivery bookkeeping beside it — is derived
state: export names it as left out with its row count, and import starts it
empty and lets repair rebuild it, so the report never counts a row the
published store does not hold. The empty delivery bookkeeping causes one
harmless memory re-announcement per session.

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
under its existing id, a retired column is named in the retired-column list.
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
   it, before it stages anything and again before it publishes.
4. Move the old file aside as the backup **together with its `-wal` and
   `-shm` files**: they hold committed data until the next checkpoint and
   belong to that file alone. Put the new file in its place with nothing of
   the old one beside it; SQLite applies whatever log it finds at a database's
   name to that database.
5. Start the consumers again.

Live claims, leases, grants and delivery state are rows like any other and are
carried as they are. The imported store is the same store on the same host: do
not run it beside its source, and do not treat it as authority on another host.
