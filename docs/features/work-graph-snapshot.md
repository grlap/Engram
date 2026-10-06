# Work-Graph Snapshot

> Normative reference: [spec §3](../spec.md#3-storage--sync) and
> [spec §7](../spec.md#7-audit-security--compliance).
> Related briefs: [local work system](local-work-system.md),
> [sqlite store](sqlite-store.md), [external adapters](tracker-adapter.md),
> [security & trust](security-and-trust.md), and
> [development](../development.md).
>
> Status: save, load, `--dry-run`, and restored-record recreation are shipped.
> The source-tree inventory is
> [shipped today](../shipped.md).

A work-graph snapshot is one deterministic, human-readable file that holds
the planning state and read-only work history of one project — its work graph,
blockers, source provenance, and permanent keyed project memories — and that a
fresh store can load. It is the deterministic work-graph recovery snapshot the
[sqlite store](sqlite-store.md) brief promises. The shipped save/load pair
replaces archive-and-script capture for stores whose builds share the
runtime-derived format fingerprint. It restores planning, never execution: no
run, root execution, claim, seal, checkpoint, waiver, or native evidence
object is ever created by a load. It is not `portable`, not a live sync path,
and not canonical-object interchange.

## What it is for

- **Recreation across builds.** Every store schema marker stays 1 until
  release and a store from a different build is refused generically. Save
  on the build that wrote the store, `engram init` on the new build with the
  same `--required-assurance … --authorized-by …` bootstrap the project had,
  re-apply any obligation rule set, load. The file carries no control policy —
  that is host authority the operator asserts, never a
  file — so a plain `init` would leave the recreated project on the fresh
  default `turn_gated`. No migration chain, no carry script. Stores written
  by builds that predate the exporter keep the manual path in
  [development](../development.md).
- **Sequential multi-machine work, by hand.** Save on host A, copy the file,
  load into a fresh store on host B, work there. Nothing detects that A kept
  mutating; that detection is what `portable` release/acquire adds later.
  Until then the roadmap's dogfood-risk sentence stands: one active host, by
  discipline.
- **A recovery artifact with a manifest.** The file carries the manifest that
  `BackupAdapter.put(project, manifest, artifact)` expects, so a
  configured copy of a save can later raise `local_backed_up`. Every save is
  referentially complete for the work graph and permanent keyed project-memory
  surface — no node, edge, or key in those surfaces is ever dropped — so every
  save is a recovery snapshot of record; a redacted save restores a typed
  placeholder where a sensitivity label excluded a text, and its body and
  manifest say so. A hand-run save by itself reduces no risk until the file
  leaves the host. The [off-host backup](off-host-backup.md) brief designs
  that configured copy.

## File layout

The file is one JSON document with two top-level members. The `body` is
canonical RFC 8785 JSON; its bytes determine the snapshot's content fingerprint.
Every field a loader or auditor decides on lives in the body; the `manifest`
only repeats the body's own summary for adapters and readers.

| Member | Fields | Role |
| --- | --- | --- |
| `body` | `schema_version: 1`, snapshot format fingerprint, project id, as-of cut, `widened` with its reason, redacted count per section, `secret_ref_bodies` count, redactor status at save, and the five ordered sections below | Canonical bytes; identical store state at the same cut and the same widening yields identical bytes and the same SHA-256 on every build that shares the format fingerprint, so the digest fingerprints content, not the build |
| `manifest` | exported-at, exporting build, body SHA-256, and a verbatim copy of the body's summary fields (format fingerprint, project id, as-of cut, widening and its reason, redacted counts, `secret_ref_bodies`, redactor status, per-section counts) | Backup metadata and human preflight; the loader re-derives the body's RFC 8785 bytes, recomputes the digest and every summary field, and refuses a manifest that disagrees |

The **as-of cut** is a vector, not one number: the project work-feed head
plus the project-memory change position, because memories advance their own
position and never enter the work feed. Save reads everything inside one
read transaction and binds that transaction's cut into the body, so items,
blockers, sources, records, and memories describe one state even while other
sessions keep mutating the store. The save and load audit events described
below are project-level audit records in the stream that also records policy
administration; they are not work events, never enter the work feed or the
exported cut, and never count against the load emptiness rule, so saving
twice on an idle store yields the same cut, the same body, and the same
digest.

The **snapshot format fingerprint** is derived at runtime from the format
definition the exporter compiled with; the loader derives its own the same
way and refuses a mismatch with the one generic different-build refusal. No
fingerprint is pinned in source or tests. This narrows the promise honestly:
a file loads on any build whose snapshot format matches, and the format
changes far less often than the store schema.
The fingerprint includes a normalized JSON Schema generated by the exactly
pinned `schemars` dependency. Its generator version is therefore part of the
format definition: dependency upgrades are deliberate format changes, not
routine semver drift.

The record-id cleanup (`ObjectHash` to `ObjectId`) also changes this format
fingerprint: generated schema definition names and references participate in
the fingerprint even when the serialized field names and values stay the same.
This is a deliberate pre-release format boundary. This build refuses snapshot
files saved before that rename; keeping schema version 1 does not make them
compatible. Do not edit a saved fingerprint or digest to bypass admission.

For this boundary, convert the source database with
[full-store migration](full-store-migration.md), importing with the last build
that still carries the record-field conversion; the current build no longer
converts that format. That build is the parent of the commit that deleted
`src/storage/migration/record_fields.rs`, found with
`git log --diff-filter=D -1 --format=%H -- src/storage/migration/record_fields.rs`.
If the current build refuses the converted store because its schema has
changed since, export that store and import it with the current build too.
Then save a new graph snapshot with the current build. If only an old graph
file remains, first load it into a disposable, empty store with the matching
old build and the project's explicitly chosen bootstrap policy; export that
store and convert it the same way, then save a new graph file with the
current build.
That recovers only what the old graph carried, not omitted execution state or
redacted bodies. Keep the old build and original file until recovery is verified;
an unsupported file still requires its matching build, not a guessed migration
chain. None of these steps authorizes a live store swap or concurrent consumers.

| Section | Carried | Not carried |
| --- | --- | --- |
| `items` | work id, short ref, title, outcome, acceptance, kind, priority, labels, origin, source snapshot id, lifecycle, child requirement, parent, prerequisites, supersession, assignment, defer-until, the pinned [acceptance-evaluation mode](acceptance-evaluation.md) when the task has one, disposal reason | runs, root executions, claims, fences, checkpoints, seals, required-child waivers (execution-generation state, kept in records as history), obligation pages, control bindings; the project's control policy and obligation rule sets, which the operator re-applies at `init` |
| `blockers` | per item, every active `WorkBlocker`: blocker id, kind, detail, creator, time | cleared blockers (they remain in records) |
| `sources` | every `WorkSourceSnapshot` cited by an item or its retained source notices, verbatim canonical JSON | nothing; no source bears a label today, and the build that first labels sources defines their exclusion |
| `records` | per item, an ordered list of history layers, oldest first, each restored layer binding its project, full planning-item cut, relations, and generation index: every `RestoredRecord` the item already carries, verbatim, then the store's own **native layer** — notes (evidence kind, summary, gate name / failures / opaque ref, recorded-at), compact events (transition kind, time, reason, including waivers with the child's exact disposed revision), and for a completed item its completion summary and time — each entry carrying the original `ActorContext` verbatim (actor id, kind, assurance, session, context), so asserted and stronger attribution stay distinguishable | evidence object ids as authority (they may appear as provenance strings), verification and environment evidence bodies, delivery cursors, session focus, handoff offers |
| `memories` | every permanent project-memory key, body, sensitivity label, remembered-at, and the original `ActorContext` verbatim; retired keys as tombstones with their retiring `ActorContext` and time | unkeyed typed project-scope observations and agent-private scratch; `restricted` bodies unless widened; the restored-source provenance marker in each version's source snapshot, which is never exported |

Items are ordered by short ref, blockers by item then blocker id, sources by
source snapshot id, records by item then generation index, memories by key.
The source entry's `hash` member holds that record id, not a content
fingerprint. Work ids, short refs, blocker ids, and source snapshot ids are
Engram's own and are preserved; nothing in the file is a foreign identifier.

Each live memory also carries `history`: its superseded attributed versions
in dense revision order, oldest first, with each body's sensitivity label,
timestamp and original actor. The top-level memory body remains current.
Every version, historical or current, also carries its optional
`retiring_target` and a `retiring_target_cleared` flag when it recorded an
explicit clear; a local target must name an item in the file, and a clear
must carry no target and follow a target still in force, one an earlier
version named that no clear has ended since.
Retired keys carry an empty history and only their tombstone: no current or
superseded body crosses the host boundary. Native canonical history remains
on the origin host, inaccessible through retired-key reads. Redaction applies
independently to each live historical body; the memory redaction count counts
keys with any redacted version, not individual versions.

[Source-change notices](source-intake.md) are carried in each item's history,
in recorded order, with original attribution and both source snapshot ids.
The source section includes those snapshots as well as the item's citation.
Load refuses missing, duplicate or mismatched notice bindings and retains the
notices as inert history, never native proposal feeds or execution authority.
Later saves preserve inherited notices once; ordinary item reads still expose
their exact count and latest notice after recovery.

Non-holder work observations are carried as native-layer notes with their
original non-holder provenance marker; on load they become inert history, not
execution credit. Native notes use their shared dense project-feed order,
including same-timestamp initial observations; timestamps and hashes never
break ordering ties. Inherited generations retain their saved order.
Claim renewals use the compact `claimed` event with a renewal
reason; no live claim identity or expiry is restored.

Sensitivity follows [security & trust](security-and-trust.md) and applies
exactly where the store carries a label. Today only memory versions bear
one, and the `remember` word writes project memories as `internal`; work
items, blockers, evidence, and source snapshots carry no label and export in
full. Wherever a label exists the rule is fixed: `restricted` text is
excluded unless the operator widens the save with `--include-restricted
--reason "<why>"`, and that reason travels verbatim in the body and in the
save audit event, because widening is a disclosure decision like every other
attributed authority in the system; `secret-ref` is an asserted label — the
writer asserts that the body is a vault reference, Engram defines and
validates no reference syntax in V1 — and save carries that body verbatim
under its label, independently of `--include-restricted`, and never
dereferences it or validates reference syntax. `secret_ref_bodies` counts
present bodies labelled `secret-ref` in current and historical revisions of
live memories. Tombstones carry no bodies and contribute zero. The required
count appears in both body and manifest; load recomputes it and refuses a
mismatch. An excluded text lands in the file as a
typed placeholder that keeps the entry present with its key and relations,
never as silent absence, and the body counts every placeholder per section
as `redacted`. A placeholder is inert: it is never a claimable item and never
satisfies readiness or completion. The Redactor inspects every saved text
exactly as it inspects a write, and the shipped development redactor is a
visible no-op: save prints that status before writing and records it in the
body, and a file produced under a no-op redactor implies no compliance
assurance. The file is a disclosure: stdout, a configured remote, and a
repository are each a disclosure boundary, repository read access is not
automatically work-state access, and committing a snapshot is a deliberate
decision, not a default. `save` itself writes only to the local host, and
its default file is created owner-only: mode `0600` on Unix, and on Windows
the ACL inherited from `ENGRAM_HOME`, which is why the host checklist wants
a user-private `ENGRAM_HOME`; `backup` makes no such promise today and that
stays its own decision. A configured off-host copy is `BackupAdapter` work and
stays under the security brief's authorized-destination rule: an
[off-host backup](off-host-backup.md) target receives this file only under
the operator's recorded authorization of that destination for it, placeholder
metadata included. A destination without that authorization is not
configured as a target; the marked-truncated export the security brief names
for such a destination is not designed, and such a destination never
receives this file. Save commits one audit event on the
source store — as-of cut, `widened` and its reason, redacted counts,
`secret_ref_bodies`, body
hash, destination kind, and the saving actor — before any byte reaches the
destination. Audit attribution is retained verbatim only after each text
field passes the 4,096-byte control-and-format-free bound, the provenance chain
fits 32 links, and the serialized actor stays within 64 KiB. Routine `doctor`
text and JSON expose only a 256-byte actor-id projection and mark the remaining
actor details omitted, keeping diagnostics bounded without weakening the
immutable audit. Its
durable fact is that a disclosure was attempted, which is the conservative
fact `doctor` should show, and a destination failure after it is `save`'s
own reported failure with the attempt on record. If the event cannot be
written, nothing is written or printed.

Every new save audit records the secret-reference count, including zero.
Audits written before that measurement retain their original bytes and omit
the field; `doctor` text reports **not recorded**, and its JSON leaves the
field absent. Absence never means zero. This additive audit read does not
require converting existing durable rows. Older binaries with strict audit
decoders cannot read a newly written audit containing the field: that is a
one-way audit-read boundary, independent of schema readiness. Do not run a
new save on a live store still consumed by those binaries.

The required snapshot summary count changes the runtime-derived format
fingerprint. Files from the preceding format are refused as
`graph_different_build`; there is no guessed compatibility conversion.

## Load

Load targets an empty project store: no work items, no work events, no
project memories, and no memory tombstones under the destination project id
(a freshly initialized store, with its policy rows and audit records, is
empty). Emptiness is checked when validation starts and re-verified inside
the write transaction, so a concurrent `add` or `remember` that lands in
between produces the same refusal, never a store that mixes native and
loaded state. A nonempty destination is refused with the typed
`graph_destination_not_empty`; a file whose project id differs from the
destination's is refused with `graph_project_mismatch` (there is no
cross-project opt-in in V1); a format-fingerprint mismatch is the generic
different-build refusal. Before any write the loader re-derives the parsed
body's RFC 8785 bytes — the container's whitespace and member order do not
change those bytes or their content fingerprint — and refuses as a corrupt
file a manifest whose digest or summary fields disagree with them. Validation
runs before the write: a dangling
parent, prerequisite, supersession, blocker, source, or record target, or a
duplicate work id, short ref, blocker id, or memory key is a typed refusal;
duplicate JSON object members at any depth (including carried canonical JSON)
are refused before build discrimination or canonical hashing, not collapsed
by a last-value-wins decoder;
the imported graph must pass the same invariant validation ordinary
mutations apply under the destination's policy — one parent per item, no
cycle through parents, prerequisites, or supersession, the depth bound (a
bounded walk up each item's parent chain) and the open-descendant bound (one
pass over the snapshot's items, closed ones included, so that check grows
with the snapshot's size and not with its number of roots), origin and source
consistency, scalar constraints,
and the format's stable section ordering — and a violation is a typed refusal,
because a body
digest anyone can recompute makes relations verifiable, not trustworthy;
each item's relation basis (its prerequisites sorted by id and its blockers in
blocker-id order) is built once on first demand from the item and blocker
sections, used to mint the item's native record, and compared with the item's
newest record by the lifecycle check; a carried record keeps its own recorded
relations byte for byte, and load never searches the whole item or blocker
section per record;
the per-item prerequisite bound is an admission limit of the planning routes,
not a snapshot invariant, so an item's recorded prerequisites are restored as
stored, above that bound included, rather than dropped or refused;
lifecycle and proof must agree both ways — an item is `completed` exactly
when its newest layer carries a completion summary and time, `cancelled` or
`superseded` only with a disposal reason and the appropriate successor shape.
Every cancelled or superseded history layer must end in a `disposed` event
matching its lifecycle, reason, and successor. Conversely, a layer whose
newest lifecycle transition is disposal must have that terminal lifecycle;
an older layer or a disposal followed by reopening cannot supply missing
proof. Any other pairing is a corrupt file. A supersession target may itself
be disposed later; target liveness is an admission rule, not a perpetual
snapshot constraint. A snapshot accepts exactly the text the store holds, byte
for byte: every carried prose field — titles, outcomes, acceptance, details,
summaries, refs, reasons — must be non-empty and carry no leading or trailing
whitespace, as every writer of those fields stores it, and is otherwise carried
unchanged, terminal controls and format characters included. Terminal safety
belongs to rendering, which escapes those characters on every read, so save
and load never refuse text an ordinary write admitted. A memory body keeps the
whitespace its author wrote and must only be non-blank within its live byte
bound. Gate history passes the stored gate rules: a non-empty bounded name,
bounded failure labels in strictly sorted order, a bounded reference, and a
pass exactly when no failure is recorded. A gate note's refs are exactly what
the store holds for its reference: none when the gate has no reference, and
that one reference otherwise; anything else is a corrupt file, refused by load
and its dry run and by save's check of its own document, never normalized, so
a loaded store saves the same reference again. A work-history record's actor
needs a non-empty actor id and session binding and valid attribution provenance; a
project-memory version or tombstone actor is held to the attribution
`remember` and `forget` admit (non-blank, bounded actor, kind, reason and
session fields and bounded provenance), so a dry run refuses whatever the real
load would.
Labels and history refs
must be sorted and unique; acceptance keeps the order its author typed and must only
be unique. One failing field refuses
the whole file as corrupt, because records are stored as written and nothing
is normalized on load. A refused
load leaves the destination exactly as it found it. The configured Redactor
inspects every string in the raw document before the write transaction;
rejection leaves no loaded state or load audit. The write is one
IMMEDIATE transaction: either every item, blocker, source, record, and memory
lands, or nothing does.

- Items land with their original ids, refs, relations, lifecycle, blockers,
  origin, and source snapshot id, plus a `restored` provenance marker.
  Source snapshots are re-inserted verbatim under the id the file gives
  them, so every exported `source_snapshot_id` resolves to the same record
  it named before. A placeholder lands as a placeholder with a `redacted`
  provenance marker and stays inert.
- **Planning state survives; execution state does not.** Lifecycle,
  blockers, prerequisites, deferral, and assignment load exactly. No run,
  root execution, claim, checkpoint, or required-child waiver is restored,
  so a previously claimed or active item lands exactly like a never-claimed
  one and its availability is re-derived by the ordinary rules — ready
  unless its own blockers, prerequisites, deferral, or ancestors say
  otherwise. The first `claim` of any item takes the ordinary path for an
  unclaimed item, which creates its run and, when its root has no execution
  yet, the root execution — a child claimed before its parent included. A
  parent whose earlier generation waived a required child sees that waiver
  in its restored history only; sealing the restored generation needs a
  fresh `update --waive CHILD --reason "…"`, exactly as after a reopen.
- **History lands as inert `RestoredRecord`s**. Each item's native layer in the
  file, if present, becomes one record with a freshly minted id at each load;
  each inherited layer is re-inserted verbatim under its file-given id. The
  native record binds the project id, full planning-item snapshot, relation
  basis, generation index, and history payload — not the file, cut, or load
  operation. The newest record is therefore the immutable source against
  which restored planning projections are checked, while older generations
  keep their historical planning cut. The same bound planning/history
  generation has the same canonical bytes whichever save carried it. No
  `RestoredRecord` carries load details; the load audit event records the body
  hash, loading actor, session, and time. Nothing inside the inherited record
  becomes native `WorkEvidence`, a `WorkEvent`, a run, or a feed entry, so it
  can never enter a completion cut or seal. A late `note` or `gate` on a
  completed-by-record item is separate canonical restored evidence bound to
  that record and a dense per-item append position; it enters the ordinary
  project/root change feeds and the next save's native history layer without
  altering the completion proof. Asserted evidence timestamps never decide
  which same-time gate transition is latest. Each late restored `gate` call
  appends a new observation, including an identical repeated call, chained
  to the prior same-name gate, without durable retry bookkeeping. After an
  uncertain response, inspect `show` before repeating it: another call is
  another observation. Native gate retry semantics are unchanged. A late
  `note` repeats exactly as on a natively completed item: an identical
  repeat replays the same note and appends nothing. A note whose response
  was lost after it committed is recovered on retry, even later, when the
  stored note has the retry's content (item, status, summary and refs);
  stored content that differs under that key is refused, never adopted.
  `show`
  renders records oldest first with each entry's original `ActorContext`, and
  `doctor` verifies them like any other canonical object.
- **A completed item lands completed**, its proof being the completion
  summary and time inside its newest `RestoredRecord`. That is the one
  completion proof that is not a `CompletionSeal`, and the
  [spec](../spec.md#3-storage--sync) and the
  [local work system](local-work-system.md) name it as such. A parent that
  later seals records each such child in its own `restored_child_completions`
  list, beside `required_child_seals` and `required_child_waivers`, so the
  seal shows exactly which children were proven, waived, or restored; the
  exact completion count — required children equal seals plus waivers —
  widens to seals plus waivers plus restored completions, and `doctor`
  recursion validates the third list like the other two. Every seal also
  materializes one `restored` flag at seal time — true when its own
  `restored_child_completions` is non-empty or when any child seal it binds
  is itself `restored` — so `show`, `done`, and `doctor` read one field instead
  of walking the tree. The designed report assembler consumes seals only and
  will refuse a `restored` seal with the typed `report_input_restored`;
  reopen creates a new run exactly as it does after a seal. Wherever a seal
  would otherwise be the basis, the newest `RestoredRecord` is: `show`
  reports `completed (restored)`, a late `note` or `gate` binds to the
  record as its historical basis, `done` refuses as on any completed item,
  reopen supersedes the record with a new run, and a root whose own
  completion is restored — which has no seal at all — will be refused by the
  report assembler with `report_input_restored`, never with a missing-seal
  error. A
  restored child that is reopened and genuinely re-sealed appears under
  `required_child_seals` in its parent's next seal instead of
  `restored_child_completions`, and the parent's `restored` flag follows
  that recomputation.
- Memories land as project memories with their original label and asserted
  `ActorContext` carried as-is — a session id that exists in no destination
  table included, because attribution is asserted everywhere. Each restored
  version, including history and tombstones, carries a store-side `restored`
  provenance marker with the snapshot body's content fingerprint. That marker
  is never exported. The save and load audit events record the transfer; the
  file does not carry the destination memory's
  provenance. The file carries no `MemoryId`, so load mints one random id per
  restored memory, shared by every version and assertion of that memory,
  tombstone included; loading the same file again mints different ids. The
  fingerprint stays provenance only. A redacted body lands as the typed placeholder under its `redacted` marker,
  and the project-memory shape admits
  both. Load always replaces restricted bodies with this placeholder, even
  from a widened file; only that file retains the human-readable plaintext,
  and the load preview and audit count the resulting placeholders.
  A redacted memory keeps its redacted marker on every later save,
  including a widened save that cannot recover missing source text. Tombstones
  land as tombstones, so a retired key stays permanently reserved.
  Live memory history recreates one linear same-key chain in saved revision
  order, with the current body current and every original actor retained.
  Missing, duplicate or out-of-order revision numbers, backwards timestamps,
  or any history on a retired key refuse the whole load before writes.
- No claim, session, cursor, grant, or scratch is created.
- One audit event records the load: snapshot body hash, as-of cut, exporting
  build, `widened` and its reason, destination redacted counts, loading actor,
  session, and load time.
  Its UUIDv7 attempt id supplies stable total ordering even when timestamps
  match, and `doctor` reports the bounded recent page in that order.

`--dry-run` runs the same validation and prints what would land (counts by
section and lifecycle, refs that would be created, placeholders that would
stay placeholders, items that would load as completed-by-record) without
writing.

A store that was loaded and then worked on saves both histories: the
inherited `RestoredRecord`s verbatim and its own native layer, so the chain
build A → B → C keeps A's restoration provenance and B's work without
duplicating a history layer. Loading one file into two fresh stores gives each
inherited record the same id and bytes in both; each item's native layer gets
a different minted id in each store with identical canonical bytes. A later
save from either loaded store carries that minted record as an inherited layer
under its minted id, alongside a native layer for any work done there.

## Words

Operator words, not agent words; the fourteen-word agent surface is unchanged.

```bash
engram graph save [--out FILE | --stdout] [--include-restricted --reason "<why>"]
engram graph load FILE [--dry-run]
```

`save` writes `snapshots/<project digest>/graph-<work-feed head>-<memory
position>-<first twelve hex digits of the body digest>.json` under
`ENGRAM_HOME`, with the same SHA-256 project digest the store and backup
paths use: no project id ever enters a path, a redacted and a widened save
at one cut differ in digest and therefore in path, and two recreation
generations that both sit at position zero cannot collide on different
bytes. File publication writes and syncs a private stage, then uses a hard
link to publish atomically without replacement. It requires filesystem
hard-link support; refusal names that requirement and preserves the OS
error, including permission failures. There is no partial-file fallback.
An existing equivalent snapshot is reported as already saved without being
rewritten. Equality ignores only manifest `exported_at` and `exporting_build`;
malformed JSON, duplicate members, or different content are refused.
Existing destinations must be regular files, not links, directories, or
special files. Inspection checks both the directory entry and opened handle;
Unix opens are nonblocking so a raced FIFO cannot stall publication. Comparison
reads are bounded to the 128 MiB load limit, including when a file grows during
inspection. An oversized existing file is refused by its named comparison limit;
it is never truncated or silently classified as different content.
Successful publication prints the path. If stage removal then fails, save
warns on stderr with the full staging path and still succeeds, including an
equivalent competing publication. A failed stage write or publication also
warns if cleanup fails: the remaining stage may contain disclosure data.
Directory sync is still attempted on Unix; its failure reports a separate
durability error and the published path even though publication occurred.

The writer retains every ancestor directory handle through staging,
publication and cleanup, refuses linked ancestors, and compares directory
identity with the protected project-store directory independently of path
case. Unix operations use directory-relative capabilities. Windows handles
deny delete sharing so ancestors cannot be replaced while library operations
reconstruct paths; observed reparse-point attributes are refused. This is
not a claim to classify every possible filesystem reparse implementation.
The refusal covers the entire path from the filesystem root, including
system or operator-created links: macOS `/tmp` and `/var`, symlinked homes
(including `ENGRAM_HOME` for the default save), and Windows cloud placeholder
directories such as OneDrive. Pass a resolved real directory path without
such ancestors instead. The diagnostic names the ancestor that could not be
bound. The writer deliberately does not canonicalize and then reopen a path.
On macOS and BSD, opening retained directory handles requires read permission
on every ancestor in addition to search permission. A search-only ancestor is
therefore refused with a cannot-open-ancestor diagnostic.
Destination spellings ending in a directory separator are refused before
normalization; on Unix a final `/.` is also refused. Supply a file name.
On Unix, parent-directory (`..`) components are refused rather than resolved
lexically across possible links.
Windows file publication requires a real volume root; drive aliases such as
SUBST that start in a subdirectory are refused before staging. Pass the real
volume path instead. The opened root is checked by comparing its directory
identity with its parent's identity.
Windows file publication supports local drive paths, normalizing a simple
verbatim drive prefix; UNC/device paths, alternate-stream components, reserved
DOS device names, separators embedded in verbatim components, and components
ending in a dot or space are refused before ordinary Win32 normalization.
Diagnostics use the walked path after resolving accepted Windows `.` and `..`
components. The writer does not promise full verbatim-path semantics.

`--out` chooses another file under the same rules. `--stdout` bypasses file
publication and emits the artifact JSON directly; it is the explicit pipe form,
because stdout is a
disclosure boundary and the default must not cross one. In the file and on
stdout alike, every terminal control or format character inside a string is
written as a JSON `\u` escape, so the output is inert in a terminal; escaping
changes no value, since load re-derives the body's canonical bytes. Both words use the
ordinary `ENGRAM_HOME` / project-file resolution and the same asserted
attribution as `engram work`. The verbs are deliberately neither
`import`/`export`, which belong to the designed external-intake and
publication surfaces (`engram import preview` / `apply` will act on a
`WorkSourceSnapshot`), nor `backup`/`restore`, which copy and replace a
whole SQLite file including grants and private scratch: `graph load` refuses
a nonempty destination where `engram restore --replace` overwrites one.

## What it deliberately is not

- Not `portable`: no remote head, no release/acquire, no writer epoch, no
  divergence refusal. When `portable` ships it reuses this file's section
  encoding and canonicalization, not the file as its head payload, because a
  portable head must also carry the executable shared state this file omits.
- Not canonical-object interchange for work: canonical bytes and content
  fingerprints are passed as provenance strings where useful; record ids are
  not derived from them. Source snapshots and inherited `RestoredRecord`s
  keep their supplied record ids and canonical bytes; each native history layer
  and each restored memory gets a newly minted id. Content fingerprints compare
  content, not record identity.
- Not execution recovery: claims, runs, root executions, waivers,
  checkpoints, seals, and evidence are never rebuilt from a file. A loaded
  store starts every item's execution from scratch with its history beside
  it.
- Not a policy carrier: required assurance and obligation rule sets are
  operator authority asserted at `init`, never restored from a file.
- Not a live sync path: two hosts loading the same file and both mutating
  produce two stores that Engram cannot reconcile. `doctor` says so; nothing
  pretends otherwise.
- Not a second tracker: a Beads or other tracker export is a
  `WorkSourceSnapshot` under the [external adapters](tracker-adapter.md)
  brief, with its own preview/apply words. Producing this file is core;
  storing it off-host remains `BackupAdapter` / `PortableStoreAdapter` work.

## Acceptance

- Save then load on a fresh store reproduces every open item with ids and
  refs preserved, its blockers and re-derived availability, its restored
  history with each entry's original `ActorContext`, its source snapshots under
  the same ids with the same canonical bytes, and the project memories with
  their labels and `ActorContext`; completed items load completed with a `RestoredRecord`,
  not a seal, and a previously claimed item loads unclaimed with the same
  availability a never-claimed item would have.
- A child of a restored, never-claimed parent can be claimed first, and that
  claim creates the parent's root execution; a restored parent whose earlier
  generation waived a required child cannot seal until it waives that child
  again, and the earlier waiver is visible in its restored history.
- Save is deterministic across builds that share the format fingerprint:
  the same store state at the same as-of cut with the same widening produces
  a byte-identical body and the same body SHA-256, and two consecutive saves
  on an idle store produce the same cut, body, digest, and path, the second
  reported as already saved; the manifest may differ only in exported-at and
  exporting build, and a manifest whose digest or summary disagrees with the
  re-canonicalized body is refused on load.
- Load refuses a nonempty destination (items, events, memories, or
  tombstones), a project-id mismatch, a format-fingerprint mismatch, a
  dangling relation or record target, a duplicate id, ref, or memory key, a
  parent or prerequisite cycle, a graph outside the destination policy's
  depth or open-descendant bounds, a lifecycle that disagrees with
  its newest layer's proof in either direction, and any carried text that
  fails the live write checks, each with its typed refusal, and a refused
  load leaves the destination unchanged; a destination that becomes nonempty
  between validation and the write is refused inside the transaction; a
  freshly initialized store that carries only policy rows and audit records
  loads; `--dry-run` reports the same counts and refusals and writes
  nothing.
- A parent whose required child loaded completed-by-record seals with that
  child in `restored_child_completions` under the widened completion count
  and with `restored: true`; a grandparent that binds that seal is
  `restored: true` without any restored child of its own; the designed report
  assembler will refuse both, and a root whose own completion is restored, with
  `report_input_restored`; a late `note` or `gate` on a restored-completed
  item binds to its record; a restored child reopened and re-sealed appears
  under `required_child_seals` in the parent's next seal; `show` and
  `doctor` report the flag without walking the tree, and `doctor` verifies a
  loaded store as healthy and reports the load audit event.
- Under a `restricted` memory version — constructed through storage test
  support, since no shipped word writes one — a save without widening
  carries a typed placeholder, a `redacted` count, and `widened: false`, and
  with widening carries the body, `widened: true`, the reason, and a path
  that differs from the redacted file's by digest rather than replacing it;
  a `secret-ref` version
  is carried byte-for-byte under its label with widening on and off; the
  audited save event exists before the file does; and the default
  destination is under the project-digest directory in `ENGRAM_HOME` with
  owner-only permission, whatever characters the project id contains.
- Save → load → work → save → load carries every inherited `RestoredRecord`
  verbatim under its file-given id. Each item with a native history layer gets
  one newly minted record at load; loading one file into two fresh stores keeps
  inherited ids and bytes equal while native ids differ and their canonical
  bytes match.
- A CLI integration test covers the operator words; storage tests cover
  transaction boundaries, digest and summary checks, and representative
  validation refusals. Existing storage tests compare inherited ids and bytes
  through sequential save and fresh-load recovery, but do not compare two
  independent fresh loads of one file or their native records' minted ids and
  canonical bytes.
  A redaction test covers the typed placeholder, the vault-reference rule,
  and the body's redactor status and widening flag. The parity suite stays
  scoped to the fourteen agent words.
