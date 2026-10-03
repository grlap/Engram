# Checkpoint and seal evidence membership

Decision, 2026-10-03: keep the `CompletionSeal`'s materialized list of selected
evidence record ids. Investigate repeated default-all checkpoint capture as a
separate growth problem. This note changes no object shape or implementation.

Related contracts: [local tasks and reports](features/local-tasks-and-reports.md),
[local work](features/local-work-system.md), and
[acceptance evaluation](features/acceptance-evaluation.md).

## Current representation

[`WorkCheckpoint` and `CompletionSeal`](../src/domain/work.rs) each hold an
`evidence` list of `ObjectId` values. These are minted record identities, not
content hashes. A checkpoint also records an acknowledged position in its
named run feed. A seal records its completion cut, accepted work revision,
acceptance results and citations, and required child completion proofs. Its
`RootExecutionRef` identifies the exact historical root accounting state;
an auditor does not substitute the root's later current projection.

The seal also names its final checkpoint. That checkpoint must acknowledge
every selected seal evidence id and reach the pre-seal run-feed cut.
Completion-checkpoint replay compares its exact evidence selection.

Evidence membership and a feed boundary answer different questions:

| Representation | What it records |
| --- | --- |
| Named run-feed cut | How far this capture acknowledged the run feed |
| Checkpoint evidence list | Which run evidence this checkpoint includes |
| Seal evidence list | Which evidence this completion selected |
| Acceptance citations | Which selected records support each criterion |

[`checkpoint_work`](../src/storage/work/execution.rs) accepts an explicit
evidence selection; when the selection is omitted, it captures all available
run evidence. An explicit empty checkpoint selection remains distinct from
an omitted selection. The
[completion service](../src/work_service/completion.rs) also admits a supplied
subset, while an empty completion evidence argument defaults to available
run evidence. A cut alone cannot represent these different selections.

## Why the seal keeps its list

Retaining the selected list preserves exact membership without requiring
auditors to infer selection from every event at or before a feed cut. The
cut still orders capture and bounds completion; it does not mean that every
earlier evidence record was selected.

Replacing the seal list with only a cut and criterion citations would lose
selected evidence that supports the overall outcome but is not cited by an
individual criterion. It would also need rules for explicit subsets, retained
feed membership, required child generations and replay. No such replacement
is adopted here.

The seal's list grows with the selected evidence for that completion. The
root accounting reference already avoids copying the full contributor state
into each seal. This decision preserves both representations.

## Audit and report consequences

An audit resolves the recorded evidence ids and criterion citations against
the immutable seal, its named run cut and historical root accounting reference.
It checks the accepted work revision and required child proofs at that
completion, and resolves the bound final checkpoint to check its evidence
acknowledgement and run-feed boundary. Late notes, later root accounting and
a reopened run do not amend the old seal's membership.

[Report assembly remains target design](features/local-tasks-and-reports.md).
Its evidence input is the frozen seal's selected membership, acceptance
citations and completion accounting. Assembly must not replace them with the current run
evidence list or root head. Evidence added after completion can be reported
as later context with its own attribution, never as evidence the old seal
selected. Retry of an assembly generation consumes the same frozen input.

## Checkpoint growth to measure

The holder-note path currently captures all accumulated run evidence into a
new checkpoint after recording each note. If each of N notes adds one record,
the checkpoints copy progressively larger id lists: 1 + 2 + ... + N, apart
from evidence already present. This is a structural explanation of aggregate
id copying, not a measured store-size or runtime result. A single seal's
selected list does not by itself create that repeated checkpoint cost.

Before proposing a checkpoint format change, measure distinct cases:

- N and 2N holder notes with growing unique evidence; record canonical
  checkpoint bytes, operation-result bytes, retained history and total store
  growth separately from read and hashing time.
- Repeated checkpoints with an unchanged selection, an explicit subset, an
  explicit empty selection, and default-all capture.
- Handoff and completion capture, including required child reopen across
  generations, to identify which histories and membership rules must survive.

A future proposal may represent implicit-all checkpoint membership through
the exact acknowledged run-feed cut while retaining explicit selections.
It must specify which evidence events that cut includes, preserve citation
closure and historical readability, preserve the seal-to-final-checkpoint
acknowledgement check and completion-checkpoint replay, and show the measured
cost reduction.
Periodic snapshots need their own demonstrated benefit and bounded total
cost; a snapshot at every checkpoint or seal would repeat the copying problem.

## Boundaries for subsequent work

The current checkpoint and seal records stay as stored. No migration,
projection rebuild, live-store measurement or new report API is delivered by
this documentation decision. A later object-shape proposal is separate work
with a named import conversion and the recorded format decision before the
shape changes, following [the migration contract](features/full-store-migration.md).
Existing record ids and links travel unchanged.

Validation of any later implementation must cover exact subsets, empty versus
omitted checkpoint evidence, citation inclusion, replay identity, late evidence
exclusion and child reopen. References are derived at runtime; no record id or
fingerprint is pinned in source or tests.
