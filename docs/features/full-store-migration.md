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
for is never dropped in silence. When a format change retires one, the importer
names it explicitly: a retired table appears in the report as left out with its
row count, and a retired column appears under `retired_fields` with the number
of values it carried, its rows having gone in without it. Any other unknown
column still refuses by name. Today one column is retired: a fingerprint of the
staged delivery page that nothing compared.

The store's own format marker is not imported; the new store keeps its own.
Delivery bookkeeping that projection repair is allowed to discard starts empty,
which causes one harmless memory re-announcement per session.

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

A page from a store converted by the retired design can omit the attribution
that says a change came from the receiving session, because that design recorded
it in a separate audit table. That table is gone. Import supplies the field once
from verified source state, writing it into the page itself; the page's bytes
change, while the delivery capability it already issued does not, so the
acknowledgement the session holds still binds. The report counts the pages
checked and the pages whose attribution had to be supplied.

Only an omitted field is supplied. A page that claims a change came from the
receiving session when the record names another one says more than the source
supports, and is refused by name instead. After decoding, an omitted field and a
stored `false` are the same thing, so only that direction can be contradicted.

### Changing the format

A format change edits the schema in place and teaches import the difference:
a renamed column is mapped, a reshaped record is rewritten from its nested JSON
under its existing id, a retired table is named as left out. There is no
profile detection, no archive format, and no chain of versions to maintain.

### Stores converted by the retired design

An earlier design derived a record's id from a hash of its bytes, so a changed
record got a new id and every link to it had to be rewritten. It kept every
pre-migration record beside its converted form, with a table of old-id to
new-id pairs, and resolved old ids at read time.

Export leaves those retired copies out. It carries only the id pairs, and
import uses them once, in a named list of reference slots: the stored column and
the path inside its JSON where a converted store can still hold a
pre-migration id. A reference there is replaced with the current id. Everything
else keeps its exact bytes — an authored note body, a content fingerprint, an
opaque key, a field outside the list, a record of another kind — even where it
reads exactly like an id. That list is the whole scope of the conversion: it was
derived from what the two real converted stores actually hold, which is evidence
for those stores rather than a proof that no other store could differ.

The pairs, and the records that existed only to bind them, are then left out.
The import report counts the replacements. After one import the store holds no
trace of the retired design, and the product has no read-time resolver.

## Operator workflow

1. Stop every consumer of the store: hosts, agents and the CLI.
2. `engram migration export` from the store file.
3. `engram migration import` into a new file with the new build.
4. Move the old file aside as the backup and put the new file in its place.
5. Start the consumers again.

Live claims, leases, grants and delivery state are rows like any other and are
carried as they are. The imported store is the same store on the same host: do
not run it beside its source, and do not treat it as authority on another host.
