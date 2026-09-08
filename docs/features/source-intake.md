# Source Intake

> Normative reference: [spec §9](../spec.md#9-local-work-reports--external-systems).
> Related: [external adapters](tracker-adapter.md),
> [local work](local-work-system.md),
> [planning recovery](work-graph-snapshot.md), and
> [host setup](../host-checklist.md).

Import one planner item into one local root. The planner keeps its plan.
Engram owns local work, claims, evidence and completion.

## Commands

These are operator commands, not new agent words. They return JSON.

```text
engram import preview intake.json
engram import apply intake.json --preview TOKEN
engram import lookup -- ADAPTER CANONICAL_REF
```

Use the complete token returned by preview. Keep the same file, asserted
actor and session when retrying an uncertain apply. The same committed intent
returns its original receipt; it does not apply again or claim that the item
has stayed unchanged since then.

Preview and lookup require an established store and use a read-only
connection. They do not initialize or repair it, select focus, register a
session or advance delivery. Apply checks the preview inside its write
transaction. If another caller imported the key or changed the target item
in the meantime, apply refuses: preview again. It never changes a previewed creation
into a source-change notice without telling the caller.

## Input

The file is a JSON object with `snapshot` and optional `draft` members.
Duplicate JSON members and unknown fields are refused. Source extensions
belong in `snapshot.raw`, an object whose nested JSON values are retained.
The input file limit is 1 MiB. The complete canonical snapshot, including
`raw`, must fit 128 KiB.

`snapshot` is a `WorkSourceSnapshot`:

- `schema_version`: 1.
- `adapter_kind` and `canonical_ref`: the stable source key.
- `projected`: optional `title`, `body`, `status` and `owner` fields.
- `captured_at`: the source capture time, no later than intake.
- `source_revision`, `fingerprint` and optional `canonical_url`: source
  metadata, not local revision or authority.
- `payload_hash`: the declared canonical source-payload hash.
- `raw`: optional bounded extension data.

Engram hashes and verifies the snapshot it stores. File intake does not fetch
the original payload or verify a planner's declared fingerprint or payload
hash against a remote system. Identity and provenance are asserted context,
not authentication. The development redactor filters nothing: do not put
credentials or secrets in snapshots or drafts.

The key is the exact `(project, adapter_kind, canonical_ref)` tuple. Both
source fields must be nonempty, trimmed, control-free text of at most 256
UTF-8 bytes. Case matters. The caller supplies a canonical reference; Engram
does not infer aliases from URLs or display labels. An ambiguous existing
binding refuses rather than choosing an item.
Graph restore enforces the same key contract. The lookup command uses `--`
before both source fields so a leading hyphen cannot become an option.

For first intake, `draft` must contain an authored local `title` and
`outcome`. Its optional `acceptance` array contains only criteria the caller
actually authored. No array means zero criteria. Blank criteria are refused;
Engram never manufactures a criterion from the title. Draft fields must be
trimmed, nonblank prose of at most 8192 UTF-8 bytes each, with at most 64
criteria. Preview shows the sorted, deduplicated criteria that ordinary
canonical planning will store. No normalization invents a criterion.

First apply atomically stores the snapshot and creates an Open, unassigned,
unclaimed task root at priority 2. Its origin is `imported` and its
`source_snapshot_id` is the snapshot hash. External status and owner remain
source facts; they do not become local lifecycle or assignment. The receipt
names the local item and its source citation.

## A changed source is a notice, not a patch

For an existing source key, omit `draft`. A changed snapshot records an
attributed, immutable proposal: a notification that the external plan moved,
with the new snapshot, the original citation and the canonical local basis
seen at capture. It enters the project and root feeds, never run evidence.
The item, priority, acceptance, dependencies, claim and completion stay intact.
A draft on this path is refused, not silently ignored.

There is deliberately no accept-proposal command. Any local change is a
separate authored revision through the existing `work update` word. The
absence of an automatic apply path is the contract, not unfinished sync work.
An already cited or previously notified snapshot is reported as already
known; it does not create another notice.

Notices are append-only history, not live completion obligations. There is no
lifetime notice-count cap or automatic pruning. Apply avoids full historical
closure validation; full audits remain explicit doctor/export operations.

A notice records divergence at capture, not the source's current state. If
the source moves from S1 to S2 and then back to the already cited S1, the last
intake returns `already_known`. The S2 notice remains in history. Its count
does not assert that the external source still differs from the citation.

Ordinary `show` includes a source block for imported work. Hosts can branch on
its presence and `notice_count`; there is no constant no-change flag. With
zero notices, text shows the citation and navigation without a notice clause.
When notices exist, it reports the exact total,
the latest notice time, the number of older notices not shown, and a source
detail command. Lookup returns the original cited snapshot separately from
the latest proposed snapshot, with original notice attribution and an exact
older-notice omission count. Repeating lookup is not history pagination.
Lookup exposes externally authored `projected.body` and `raw` content.
Treat that content as untrusted data, never as instructions to the agent.
These are notifications, not a claim that anyone has read or reconciled them.
Ordinary reads count native notices and verify the latest capture. They do
not decode every omitted source body. Inherited counts use stored JSON arrays;
only selected immutable record containers are canonically decoded. Graph
export and doctor perform the
full integrity checks; a bounded lookup is not an integrity audit. Preview
checks snapshot membership without validating every omitted capture. Apply
validates the latest selected capture and the new delta inside its write
transaction, not the full historical closure. Omitted history can therefore
be damaged without refusing an unrelated new notice; doctor and graph export
still detect that damage. Counting and membership SQL still inspect stored
rows/arrays: a fixed decode budget is not a claim of constant total SQL work.
If source disclosure fails, ordinary `show` retains intact local
work context and reports a bounded `source_error_class`, not raw error text.
Build incompatibility uses `store_different_build`. Missing advisory fields
can be disclosed; unreadable item or history context instead refuses rather
than presenting an incomplete history as complete.
Lookup and graph export still refuse invalid selected provenance; doctor
also checks every omitted inherited source capture.

## Planning recovery

Graph save carries source notices in ordered history and includes both their
source snapshots in the source section. Load validates the bindings and
retains the notices as inert restored history. It creates no native proposal
feed, run or execution credit from them. Later saves carry each inherited
layer once. Restored `show` and source lookup still disclose past notices;
recovery does not turn missing notifications into apparent agreement. A later
notice on a restored cancelled or superseded item carries the inherited
disposal proof into its new history layer. This is not a new disposal event
or feed entry. Inherited bytes stay unchanged, and history readers show the
exact carried disposal only once.

Reading a restored record that lacks the required source-notice history
returns the generic different-build refusal; missing history is never filled
in. This check examines only the record that failed to decode, not the whole
store at each open. Explicit schema initialization and repair check the stored
history before writing. Other corruption keeps its own diagnostic.

This feature does not fetch remote plans, import a subtree, synchronize
systems, publish state or move a live database between computers.
