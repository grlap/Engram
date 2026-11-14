# Relations in a resolved task folder

The [complete-folder import](task-planning-text.md) applies an eligible task's
prerequisite and active blocker changes together with its scalar edits,
additions and admitted removals. Its receipt reports each changed task as
`applied`, `held` or `refused`, with a reason for each unapplied change. A held
or refused task does not reject independent admitted changes. Import does not
recover a claim or grant completion authority.

Any held or refused task makes the CLI import exit `3`, including a dry run.
The complete receipt stays on stdout and applied tasks remain applied. Stderr
names the unapplied task files (or omitted tasks) and directs the caller to keep
the incoming commit before exporting. After a fresh local export, re-merge that
commit using the common commit before importing with the fresh basis; replaying
old incoming documents can overwrite newer local edits. A routine local export
would overwrite those edits; never advance the incoming-commit base on this partial status.
Whole-import refusal uses exit `1`, while a complete import exits `0`.

Open and Proposed tasks may be native or restored on this host; prerequisite
and blocker changes require an Open owner. Creation origin and past execution
do not prohibit shared planning. A live unexpired
claim or engaged run holds the owner as `active_run`; an initial dormant run,
or a released or expired claim, does not. Completed and other terminal owners
retain their lifecycle refusal. Unchanged tasks gain no event.

An unchanged Open prerequisite endpoint is a read-only reference, including a
native or held task in an unrelated root. It gets no revision, event or
runtime update. A shared folder may carry an edge added before its endpoint
completed, was cancelled or was superseded elsewhere. Import preserves that
edge as history when the incoming and effective endpoint lifecycles agree;
it neither changes that endpoint nor grants execution authority. Ordinary local
edge creation still requires an Open endpoint. An incoming Open endpoint whose
change is refused cannot mask a locally terminal endpoint; that owner refuses
as `already_satisfied` or `prerequisite_lifecycle`. Proposed endpoints also
refuse. Retaining an existing edge is event-free, and an eligible owner may
explicitly remove it. Self/ancestor dependencies, real graph cycles and
additions beyond the retained prerequisite bound refuse. Removing edges never
consumes capacity; a retained historical set over the current bound stays
readable and removable.

A relation owner's affected ancestor chain and root must remain free of
engaged execution. `ancestor_binding` names the blocking ancestor or active root
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
admission and affected execution/graph checks. Endpoint lifecycle is checked
again against the effective local values after held or refused edits. The shared collector retains
one reason per task in task-id order. Every requested task has an outcome in
the receipt; the strict delta candidate error retains its bounded diagnostic.
A later-stage owner refusal also refuses any deletion that its retained local
references would otherwise leave dangling.
Malformed input keeps its earlier `task_sync_invalid_graph` refusal.

Within one transaction, import removes obsolete existing-owner prerequisites
and clears removed blockers before restoring additions. It then applies scalar
edits and new relation events, followed by admitted postorder disposal. Added
tasks already carry their complete restored relation snapshots; import does
not duplicate those as native events. Existing owners receive the ordinary
`PrerequisiteAdded/Removed` under project authority and `Blocked/Unblocked`
events, each with its current full item snapshot, sequential revision and
exact relation fingerprint. An existing dormant run retains its run context in
the event; import grants no claim, handoff or root execution authority.
The baseline records applied incoming documents and effective local support
for unapplied members, with reference closure and final export checks. It never
consumes a held or refused incoming value; a fresh import retries that value
after local execution ends. A partial receipt is retained when changes applied,
so an exact retry recovers the result after a lost response. A fresh export basis
uses a new intent and reconsiders unapplied changes; a no-change partial result
stays retryable on the same basis without even a first baseline write. Dry run rolls the same path back. An exact
replay never reapplies relations over later local work. A store, writer or final integrity failure
still rolls back the entire transaction. Strict delta reconciliation continues
to defer or refuse the whole candidate; it does not consume partial progress.

Three views stay distinct: the prospective export includes retained local
nonmembers; the durable graph includes retired task rows and their historical
relations; the immutable origin union keeps every original imported document
and the native creation closure its references need. Unrelated native history
is not imported into that union. New native support is attributed to the
incoming tasks that introduce it when locating a refusal. All must remain valid. `retained_graph` and `durable_graph` name failures in
their respective current views; `origin_graph` retains the additions' origin
admission. A graph is not made valid by hiding a retired row or rewriting its
origin. A surviving reference to a removed task needs an explicit admitted
relation removal; import never clears it automatically.

Native creation, retained execution and waiver history stay immutable and do
not create a creator-host restriction. An addition beneath an ancestor with a
relation change in the same folder is held as `ancestor_binding`; the admitted
ancestor edit can apply, and a fresh import can retry the child. Disposal keeps
its stricter persisted-run check, parent/child completion barriers and explicit
waiver requirement. Projection repair rebuilds only declared disposable
projections and preserves durable events, origins, relation history, results
and runtime state.
