# Work-Graph Snapshot

> Normative reference: [spec §3](../spec.md#3-storage--sync) and
> [spec §7](../spec.md#7-audit-security--compliance).
> Related briefs: [local work system](local-work-system.md),
> [sqlite store](sqlite-store.md), [external adapters](tracker-adapter.md),
> [security & trust](security-and-trust.md), and
> [development](../development.md).
>
> Status: save shipped; load, `--dry-run`, and restored-record recreation are
> designed but not yet shipped. The installed inventory is
> [shipped today](../shipped.md).

A work-graph snapshot is one deterministic, human-readable file that holds
the planning state of one project — its work graph, blockers, source
provenance, and permanent keyed project memories — and the read-only history of that work,
and that a fresh store can load. It is the deterministic, human-readable
work-graph recovery snapshot the [sqlite store](sqlite-store.md) brief
promises. The shipped exporter replaces archive-and-script capture for
current stores; the still-planned loader will replace recreation scripts. It
restores planning, never
execution: no run, root execution, claim, lease, seal, checkpoint, waiver,
or evidence object is ever created by a load. It is not `portable`, not a
live sync path, and not canonical-object interchange.

## What it is for

- **Recreation across builds.** Every store schema marker stays 1 until
  release and a store from a different build is refused generically. Save
  on the build that wrote the store, `engram init` on the new build with the
  same `--required-assurance … --authorized-by … --reason …` bootstrap the
  project had, re-apply any obligation rule set, load. The file carries no
  control policy — that is host authority the operator asserts, never a
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
  `BackupAdapter.put_snapshot(project, manifest, bytes)` expects, so a
  configured copy of a save can later raise `local_backed_up`. Every save is
  referentially complete for the work graph and permanent keyed project-memory
  surface — no node, edge, or key in those surfaces is ever dropped — so every
  save is a recovery snapshot of record; a redacted save restores a typed
  placeholder where a sensitivity label excluded a text, and its body and
  manifest say so. A hand-run save by itself reduces no risk until the file
  leaves the host.

## File layout

The file is one JSON document with two top-level members. The `body` is the
canonical RFC 8785 encoding and the sole subject of determinism and identity.
Every field a loader or auditor decides on lives in the body; the `manifest`
only repeats the body's own summary for adapters and readers.

| Member | Fields | Role |
| --- | --- | --- |
| `body` | `schema_version: 1`, snapshot format fingerprint, project id, as-of cut, `widened` with its reason, redacted count per section, redactor status at save, and the five ordered sections below | Canonical bytes; identical store state at the same cut and the same widening yields identical bytes and the same SHA-256 on every build that shares the format fingerprint, so the digest is a content identity, not a build identity |
| `manifest` | exported-at, exporting build, body SHA-256, and a verbatim copy of the body's summary fields (format fingerprint, project id, as-of cut, widening and its reason, redacted counts, redactor status, per-section counts) | Backup metadata and human preflight; the loader re-derives the body's RFC 8785 bytes, recomputes the digest and every summary field, and refuses a manifest that disagrees |

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

| Section | Carried | Not carried |
| --- | --- | --- |
| `items` | work id, short ref, title, outcome, acceptance, kind, priority, labels, origin, source snapshot id, lifecycle, child requirement, parent, prerequisites, supersession, assignment, defer-until, disposal reason | runs, root executions, claims, fences, checkpoints, seals, required-child waivers (execution-generation state, kept in records as history), obligation pages, control bindings; the project's control policy and obligation rule sets, which the operator re-applies at `init` |
| `blockers` | per item, every active `WorkBlocker`: blocker id, kind, detail, creator, time | cleared blockers (they remain in records) |
| `sources` | every `WorkSourceSnapshot` cited by an item, verbatim canonical JSON | nothing; no source bears a label today, and the build that first labels sources defines their exclusion |
| `records` | per item, an ordered list of history layers, oldest first, each layer carrying its generation index: every `RestoredRecord` the item already carries, verbatim, then the store's own **native layer** — notes (evidence kind, summary, gate name / failures / opaque ref, recorded-at), compact events (transition kind, time, reason, including waivers with the child's exact disposed revision), and for a completed item its completion summary and time — each entry carrying the original `ActorContext` verbatim (actor id, kind, assurance, session, context), so asserted and stronger attribution stay distinguishable | evidence object hashes as authority (they may appear as provenance strings), verification and environment evidence bodies, delivery cursors, session focus, handoff offers |
| `memories` | every permanent project-memory key, body, sensitivity label, remembered-at, and the original `ActorContext` verbatim; retired keys as tombstones with their retiring `ActorContext` and time | unkeyed typed project-scope observations and agent-private scratch; `restricted` bodies unless widened; the store-side `restored` link, which is write-only |

Items are ordered by short ref, blockers by item then blocker id, sources by
hash, records by item then generation index, memories by key. Work ids,
short refs, blocker ids, and source snapshot hashes are Engram's own and are
preserved; nothing in the file is a foreign identifier.

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
under its label, never widened and never dereferenced or inspected for
shape. An excluded text lands in the file as a
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
stays under the security brief's authorized-destination rule — a destination
not authorized for placeholder metadata receives the marked-truncated export
that brief defines, never this file. Save commits one audit event on the
source store — as-of cut, `widened` and its reason, redacted counts, body
hash, destination kind, and the saving actor — before any byte reaches the
destination. Audit attribution is retained verbatim only after each text
field passes the 256-byte control-and-format-free bound, the provenance chain
fits 16 links, and the serialized actor stays within 4 KiB; this keeps routine
`doctor` text and JSON bounded without weakening the immutable audit. Its
durable fact is that a disclosure was attempted, which is the conservative
fact `doctor` should show, and a destination failure after it is `save`'s
own reported failure with the attempt on record. If the event cannot be
written, nothing is written or printed.

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
body's RFC 8785 bytes — the container's whitespace and member order carry no
identity — and refuses as a corrupt file a manifest whose digest or summary
fields disagree with them. Validation runs before the write: a dangling
parent, prerequisite, supersession, blocker, source, or record target, or a
duplicate work id, short ref, blocker id, or memory key is a typed refusal;
the imported graph must pass the same invariant validation ordinary
mutations apply under the destination's policy — one parent per item, no
cycle through parents, prerequisites, or supersession, the depth, fanout,
and open-descendant bounds, origin and source consistency, scalar and
timestamp constraints — and a violation is a typed refusal, because a body
digest anyone can recompute makes relations verifiable, not trustworthy;
lifecycle and proof must agree both ways — an item is `completed` exactly
when its newest layer carries a completion summary and time, `cancelled` or
`superseded` exactly when it carries a disposal reason or a successor, and
any other pairing is a corrupt file; every carried text field — titles,
outcomes, acceptance, details, summaries, gate names, failure labels, refs,
reasons, actor ids, actor contexts, memory bodies — must pass the same
control-and-format-character and length checks the live words apply on
write, and one failing field refuses the whole file as corrupt, because
records are content-addressed and nothing is normalized on load. A refused
load leaves the destination exactly as it found it. The write is one
IMMEDIATE transaction: either every item, blocker, source, record, and memory
lands, or nothing does.

- Items land with their original ids, refs, relations, lifecycle, blockers,
  origin, and source snapshot id, plus a `restored` provenance marker.
  Source snapshots are re-inserted verbatim; because canonical objects are
  content-addressed, every exported `source_snapshot_id` resolves to the same
  hash it had before. A placeholder lands as a placeholder with a `redacted`
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
- **History lands as inert `RestoredRecord`s**, content-addressed and minted
  only by load. Each inherited layer is re-inserted verbatim, so its identity
  is unchanged across generations, and the file's native layer becomes one
  new record derived solely from the work id, the generation index, and the
  layer's history payload — not from the file, the cut, or the load — so the
  same history yields the same record whichever save carried it and however
  many times it is loaded; the load audit event alone carries the body hash,
  loading actor, session, and time. Nothing in a record becomes
  `WorkEvidence`, a `WorkEvent`, a run, or a feed entry, so it can never
  enter a completion cut, a seal, or a peer's `next` changes; `show` renders
  the records oldest first as `restored history: N entries` with each
  entry's original `ActorContext`, and `doctor` verifies them like any other
  canonical object.
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
  is itself `restored` — so `show`, `done`, `doctor`, and report assembly
  read one field instead of walking the tree; report assembly consumes seals
  only and refuses a `restored` seal with the typed `report_input_restored`;
  reopen creates a new run exactly as it does after a seal. Wherever a seal
  would otherwise be the basis, the newest `RestoredRecord` is: `show`
  reports `completed (restored)`, a late `note` or `gate` binds to the
  record as its historical basis, `done` refuses as on any completed item,
  reopen supersedes the record with a new run, and a root whose own
  completion is restored — which has no seal at all — is refused by report
  assembly with `report_input_restored`, never with a missing-seal error. A
  restored child that is reopened and genuinely re-sealed appears under
  `required_child_seals` in its parent's next seal instead of
  `restored_child_completions`, and the parent's `restored` flag follows
  that recomputation.
- Memories land as project memories with their original label and asserted
  `ActorContext` carried as-is — a session id that exists in no destination
  table included, because attribution is asserted everywhere — plus one
  store-side `restored` link naming the snapshot they came from; that link
  is never exported, so a memory's provenance chain lives in the save and
  load audit events, not in the file. A redacted body lands as the typed
  placeholder under its `redacted` marker, and the project-memory shape
  admits both. Tombstones land as tombstones, so a retired key stays
  permanently reserved.
- No claim, lease, session, cursor, grant, or scratch is created.
- One audit event records the load: snapshot body hash, as-of cut, exporting
  build, `widened`, redacted counts, loading actor, session, and load time.
  `doctor` reports it.

`--dry-run` runs the same validation and prints what would land (counts by
section and lifecycle, refs that would be created, placeholders that would
stay placeholders, items that would load as completed-by-record) without
writing.

A store that was loaded and then worked on saves both histories: the
inherited `RestoredRecord`s verbatim and its own native layer, so the chain
build A → B → C keeps A's restoration provenance, B's work, and nothing
twice — a load of the same file, or of a later save carrying the same
layers, into another fresh store re-inserts the same content-addressed
records rather than minting new ones.

## Words

Operator words, not agent words; the thirteen-word agent surface is unchanged.

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
bytes. `save` never replaces an existing file — it stages and publishes
exactly as `backup` does, and an existing path holding the same bytes is
reported as already saved; it prints the path. `--out` chooses another file
under the same no-replace rule, and `--stdout` is the explicit pipe form,
because stdout is a
disclosure boundary and the default must not cross one. Both words use the
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
- Not canonical-object interchange for work: canonical bytes and hashes are
  passed as provenance strings where useful but never re-minted or verified
  on load; only source snapshots and restored records are carried verbatim
  because their identity is their content. Identity in the new store is
  otherwise new identity.
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
  history with each entry's original `ActorContext`, its source snapshots at
  the same hashes, and the project memories with their labels and
  `ActorContext`; completed items load completed with a `RestoredRecord`,
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
  depth, fanout, or open-descendant bounds, a lifecycle that disagrees with
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
  `restored: true` without any restored child of its own; report assembly
  refuses both, and a root whose own completion is restored, with
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
  verbatim and mints exactly one new record for the native layer; two loads
  of the same file into two fresh stores, or of two saves carrying the same
  layers, mint byte-identical records.
- A CLI integration test covers the operator words; a storage test covers
  the transaction boundary, the digest check, and every validation refusal;
  a redaction test covers the typed placeholder, the vault-reference rule,
  and the body's redactor status and widening flag. The parity suite stays
  scoped to the thirteen agent words.
