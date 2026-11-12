# Imported verification core

This core prepares explicit local applicability for retained registered checks
and remote verification. It is part of the [full evidence exchange](evidence-persistence.md)
implementation; the [Git task sync](git-task-sync.md) workflow still owns family
operations and completion selection. Retaining a pack grants no verification
credit or execution authority.

Remote executions are keyed by project, declared runner host and remote run id.
Every supported authoritative completion contributes, whether retained in a
terminal origin or in an association's own request. A refused association,
including `ResultConflict`, can contain a different terminal from the origin it
links. Both facts are kept. Recording and publishing stores are attribution,
not extra execution identities.

Equivalent terminal bodies represent one execution with sorted original
contributor ids. Different authoritative bodies make the known union conflicted,
regardless of arrival order. Comparison includes source endpoints, mode, result,
runner session and finish time. It excludes recorder envelopes and association
targets. No body hash becomes a record identity or selects a winning result.
Adding a conflicting native or imported association updates the keyed union and
invalidates current applicability in the same transaction, including decisions
already consumed by a local run.
Historical refusals, credit, applicability records and seals remain unchanged.

`SqliteStore::associate_imported_verification` creates a separately identified
local decision. Its request names the original verification, producing-origin
namespace, witnessed origin run-feed cut, actual local binding and requirement.
The original verification and producer must belong to that witnessed interval.
The writer checks the destination's ordinary claim and fence, governing revision,
named root and accepted source sighting. It measures that root with the existing
source owner before opening the writer, then revalidates the binding and root.
An explicitly named criterion must carry its actual current requirement.

The named root survives recovery or holder change on the same claim. Its
historical naming session and fence do not replace the ordinary current-holder
and fence checks. Release followed by reclaim without a new name still has no
active root; a stale holder or registration fence remains refused.

Registered and remote producer facts use the native readers' shared extraction
helpers and common result/requirement rule. Passing checks in failed registered
aggregates do not qualify. The complete measured `source_id` must match; equal
dirty inputs are allowed, while commit, tree, changes and submodule differences
are not. A Git commit that merely carries evidence does not make an earlier
tested source current. Import timestamps never rejuvenate execution, and carriage
adds no host, environment, full-only or rerun requirement.

Reported gates and uncredited observation checks are not supported verification
producers. Unsupported or provisional remote bodies remain inert. This initial
core accepts original registered and remote verification records; it does not
create a new binding representation for historical bound copies or synthesize a
shared identity from legacy source text. Full component closure and family
admission remain with the exchange owner. Qualification alone writes no native
evidence, obligation satisfaction or feed event. Explicit `consume_verification`
records an immutable local `imported_evidence_consumption` through the native
evidence appender after rechecking the current claim, root, source, governing
content and terminal union. It can satisfy native obligations and be selected
by evaluation or completion under their existing currency checks. The original
check remains historical; it is never relabeled as locally observed execution.
See [exchange steps](git-task-sync.md#explicit-exchange-command-steps).

Exact retries return the retained decision before reading the filesystem.
Reusing its UUID for another intent refuses. A new decision retains its original
actor assurance separately from the local asserted decision, the complete local
source, governing fingerprint, local cut and known contributors. The bounded
summary API exposes only `SourceIdSummary`, record ids, contributor count and
recorded eligibility versus subsequent terminal conflict. That historical summary
does not assert current source currency at a later completion.

Canonical originals and local decisions are immutable. Terminal and applicability
tables are rebuildable views, reconstructed by explicit projection repair and
checked by doctor. Existing canonical rows need no conversion for this core;
existing stores require projection repair for the new derivation. Generic full
store export/import preserves original ids and bytes and rebuilds these views.
The joint changeset still owns its complete store-impact and cutover judgment.

Behavior tests use disposable Git roots and real storage ingress. Their host and
runner assertions are fixtures, not independently observed cross-host execution.
