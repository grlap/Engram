# Historical acceptance applicability

The [evidence store](evidence-persistence.md) retains original evaluations and
native completion seals as immutable historical facts. Retention does not
evaluate criteria, invoke a model, complete a local run or enable evaluation.
This core prepares a separate explicit qualification decision. Explicit
`consume_acceptance` then records an immutable local use after current binding,
source, policy and closure rechecks. Ordinary completion can select that use to
derive acceptance results and writes its own local seal; the foreign seal is
never installed as the local completion. Neither step transfers live authority
or enables evaluation. See [exchange steps](git-task-sync.md#explicit-exchange-command-steps).

`SqliteStore::associate_imported_acceptance` names the original proof, its
producing-origin namespace, exact origin feed cuts, the destination's current
ordinary work binding and previously prepared
[verification decisions](imported-verification.md). Each local use has its own
prepared UUID. An exact retry returns its original decision before filesystem
measurement; another intent with the same UUID refuses.
The same exact-retry check runs after acquiring the writer lock, so concurrent
qualifications return the first committed decision without minting another use.

The destination uses the verification core's ordinary claim, governing revision,
named-root and accepted-sighting validation. The shared source owner measures
the complete `source_id` before opening the writer. A root binding survives
recovery or holder change on the same claim; its historical namer and fence are
not current authority. Current holder and fence validation remains mandatory.
Release followed by reclaim needs a new name. No source identity is inferred
from a carrier commit, summary, legacy fingerprint or import time.

## Evaluation proof

An evaluation's `work_revision_hash` is a content fingerprint, never an object
link. Its separately identified governing witness retains the full original
work item, revision and fingerprint, or an explicit unavailable state. A missing
snapshot stays unavailable; criteria copied from the evaluation do not recreate
it. The original snapshot is validated before comparing destination governing
content. Host-local revision numbers, active run ids and restoration metadata
cannot establish equality.

The read checks actual destination criteria, bindings, policy and pinned mode.
Native policy, evaluator-shape, verdict-shape and affiliation predicates are
shared with ordinary evaluation. Evaluator standing is reconstructed from the
qualified origin's actual work events, not by comparing foreign session strings
with local registrations. A newer known origin evaluation supersedes an older
pass by feed position, even when their clocks disagree. Later known affiliation
or source facts cannot be hidden by requesting an older cut.

Citations must belong to the original run before its evaluated cut. Bound
criteria and observed verdicts use already recorded verification applicability,
including complete source currency and the current terminal-conflict union.
Reported gates remain asserted; judgment, notes and restored prose do not become
host verification. Local source changes or a stronger destination policy can
retire current use without rewriting the original verdict.

Completion may retire stale implicit obligation credit from an uncited local
acceptance use after source, governing revision, root or claim movement. It
rechecks that use's original proof context, including later terminal conflicts,
then requires fresh applicable native evidence under the current policy and
bindings. Explicitly citing the stale use still refuses. Retirement changes no
historical consumption, obligation resolution, evaluation or seal.

## Native completion proof

A completion seal's `accepted_work_revision_hash` is an actual work-item revision
object link. Its integer revision and the evaluation's governing fingerprint
retain their separate meanings. The original seal, completed event, claim and
run context, source, evidence, obligation resolutions, environment cut, root
heads and child closure are read within the retained origin namespace.
Imported planning can later govern a genuinely native run and completion.
Its planning restoration marker does not make that new seal a restored proof;
restored seals and restored child completions remain ineligible.

Root-state replay shares the native decoder and strict every-head validation;
schema, JSON shape, root/project/generation identity, delta continuity and exact
pre-completion predecessor checks are unchanged. Native completion event and
required-child successor predicates are also shared. A missing interior feed
member or closure dependency refuses; an endpoint alone is insufficient.
Restored completion markers and reported prose are not native proof.

This core writes no local evaluation, completion seal, obligation satisfaction,
claim, root execution, run feed or session delivery. A successful use decision
provides the original proof locator for explicit later consumption. It is not
installed as the destination run's completion. Each decision binds its actual
local run generation; reopening or replacing that authority cannot inherit it.

## Reads and persistence

`imported_acceptance_summary` distinguishes recorded eligibility from current
eligibility, rechecking actual local authority, governing content, source, policy
and retained evidence. It exposes structural reasons, record ids and
`SourceIdSummary`; complete identities stay in canonical records. A failed
writer leaves no partial use, verdict or seal, even when its caller commits an
outer transaction. Original bytes, ids, times and links are never rewritten.

The use index is rebuilt from immutable local decisions and checked by doctor.
This is a new derived table, so existing stores need projection repair for that
derivation. Existing canonical records require no conversion for this core.
The combined changeset still owns its full store-impact and cutover assessment.
Reuse does not enable evaluation or change a project's admission policy.
Engram's own project enabled independent evaluation separately; other projects
retain their explicitly configured policy.

Behavioral tests use disposable stores and Git roots. Their origin and runner
metadata are fixtures, not independent observation of execution on another host.
