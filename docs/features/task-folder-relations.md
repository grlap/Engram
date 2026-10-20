# Relations in a resolved task folder

The [complete-folder import](task-planning-text.md) applies a restored Open
task's prerequisite and active blocker changes together with its scalar edits,
additions and admitted removals. One refusal rejects the whole folder. Import
does not recover a claim, release execution or grant completion authority.

Only existing owners restored through task sync, with no current or prior
execution, can take relation changes. Native owners refuse
`unsupported_origin`; a current run refuses `active_run`; execution history
refuses `execution_history`. Completed or other non-Open owners keep their
lifecycle refusal. Unchanged native and terminal tasks gain no event.

An unchanged Open prerequisite endpoint is a read-only reference, including a
native or held task in an unrelated root. It gets no revision, event or
runtime update. Adding a new edge to a Completed prerequisite refuses as
`already_satisfied`; retaining an existing satisfied edge is event-free, and
an eligible owner may explicitly remove it. Other non-Open endpoints refuse
`prerequisite_lifecycle`. Self/ancestor dependencies, real graph cycles and
additions beyond the retained prerequisite bound refuse. Removing edges never
consumes capacity; a retained historical set over the current bound stays
readable and removable.

A relation owner's affected ancestor chain and root must remain free of
execution. `ancestor_binding` names the blocking ancestor or active root
execution, including one created by a sibling. Unrelated scalar-only edits,
endpoint roots and claims elsewhere in the project keep their ordinary
admission. Imported text never expires or renews those claims.

Blocker ids and original shared actor/time remain unchanged. The local event
records the importing actor/time separately, with asserted historical
attribution for the incoming blocker. The local canonical blocker object has
its own minted record id. A matching active blocker is untouched, preserving
any richer local attribution. Rewriting an existing id's immutable fields
refuses `immutable_blocker`; reusing an id that the store already holds,
including a cleared id, refuses `id_collision`. Replacement clears the old id
and adds a new one. Clearing retains the original JSON and creation-event
binding, and appends its own clear-event binding.

Preflight judges the whole composed task change before any write. Unsupported
fields win, followed by immutable identity, owner eligibility, endpoint/bound
admission and affected execution/graph checks. The shared collector retains
one reason per task in task-id order, with bounded output and omission counts.
A later-stage owner refusal can leave only that owner named, rather than a
`retained_reference` conflict for the removal its rejected edit would unblock.
Malformed input keeps its earlier `task_sync_invalid_graph` refusal.

Within one transaction, import removes obsolete existing-owner prerequisites
and clears removed blockers before restoring additions. It then applies scalar
edits and new relation events, followed by admitted postorder disposal. Added
tasks already carry their complete restored relation snapshots; import does
not duplicate those as native events. Existing owners receive the ordinary
`PrerequisiteAdded/Removed` under project authority and `Blocked/Unblocked`
events, each with its current full item snapshot, sequential revision and
exact relation fingerprint. No event contains a run, claim, handoff or root
execution snapshot. Baseline, final export checks and the one result commit
together; dry run rolls the same path back, and exact replay recovers its old
receipt without reapplying relations over later local work.

Three views stay distinct: the prospective export includes retained local
nonmembers; the durable graph includes retired task rows and their historical
relations; the immutable origin union keeps every original imported document.
All must remain valid. `retained_graph` and `durable_graph` name failures in
their respective current views; `origin_graph` retains the additions' origin
admission. A graph is not made valid by hiding a retired row or rewriting its
origin. A surviving reference to a removed task needs an explicit admitted
relation removal; import never clears it automatically.

Valid run-free project prerequisite events and runtime-free blocker events
remain eligible history for later scalar, relation and deletion imports.
Execution, native creation, reopening and waivers do not become planning-only
history. An addition beneath a held ancestor with a relation change in the
same folder also refuses `ancestor_binding`, matching a later folder after
those relation events have been recorded. Disposal retains parent/child
completion barriers and the ordinary explicit waiver requirement. Projection repair rebuilds only declared
disposable projections and preserves durable events, origins, relation
history, results and runtime state.
