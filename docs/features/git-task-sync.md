# Git task synchronization

> Normative contract: [specification §3.2](../spec.md#32-optional-git-task-sync).
> Related: [local work](local-work-system.md), [SQLite](sqlite-store.md),
> [work-graph snapshots](work-graph-snapshot.md), and
> [off-host backup](off-host-backup.md).

Design selected on 2026-10-09. The [explicit directory profile](task-planning-text.md)
implements planning text, opt-in shared notes and informational machine activity.
The [host-local opt-in and explicit prepared materialization](task-planning-text.md#materializing-a-selected-preparation)
are implemented, together with the internal exchange foundation below.
Automated exchange and reconciliation remain planned. The
contract below describes that broader target. This replaces sequential
single-writer handoff as the selected personal cross-machine workflow.
Previous portable writer-epoch, release/acquire, dedicated-ref and scheduled
push proposals are not prerequisites for this feature. Local SQLite remains
canonical on each machine. Divergent work is allowed; an external coding
agent resolves task text independently of code. Engram never calls a model.

The model is host-neutral: N independently writable hosts on Windows, macOS or
Linux reconcile through the same configured Git remote and dedicated orphan data branch,
then import locally. The shared Git remote is their meeting point, not an Engram authority;
Engram has no hub. Platform transport/subprocess support must be verified per OS
but does not change this model or transfer claims/runtime authority.

The ordinary CLI folder route is confined to the selected project's
`MAIN/.engram-data/.engram`. `task-sync export` and `task-sync import` first
observe the dedicated worktree, orphan branch and data-only paths; they refuse
an arbitrary candidate directory or the code checkout's `.engram/`. Their
`--branch` defaults to `engram-data`. Folder imports accept the data folder's
ordinary policy files; an exact export retry preserves them. The low-level
filesystem-neutral directory port remains available for private candidates,
while `task-sync prepare` retains its separate private preparation contract.
None of these folder commands advances a branch or publishes to a remote.

## Reconciliation storage foundation

The Model A storage slice retains immutable `task_sync_shared_import_version`
and `task_sync_reconciliation_hold` objects. Durable imported-head rows point to
the original import origin, an admitted folder-baseline delta, or its latest
shared-version successor. Folder application advances heads only for changed
task documents or shared membership, atomically
with its baseline delta and result. Project-feed order selects admitted
baselines; a previously shared member omitted from later complete membership
has an explicit removed baseline. A local task that was never imported stays
headless even when a folder contains it. Original
creation records and links stay unchanged; a shared baseline is separate from
current native planning edits. A reconciled outcome is a
`ReconciledImportApplied` transition in the existing exchange ledger, beside the
historical `ImportApplied` transition, with one outcome per prepared operation.
It links the admitted folder-baseline delta rather than duplicating the shared
graph inventory. Every versioned task appears in that same delta, including a
version whose document is unchanged. The receipt binds the exact retained
candidate fingerprint and resolved commit. Its after-basis is the delta
inventory root, an internal membership comparison; it is distinct from the
retained candidate planning fingerprint and supplies no authority assurance.

One candidate hold retains the complete connected task scope and every run
generation. Many durable run slots may name that same hold. A hold over tasks
without runs still exists as a canonical fact; an empty slot query proves
nothing about host responsibility. New affected claims, handoffs, governing
planning and evaluations refuse while held.
Original holders may renew, checkpoint, record evidence and release their own
claims. A hold neither freezes files nor takes over a claim. The Work validator
checks retained scope and slots inside the writer transaction; the core must
separately prove admitted owners, enrollment and host fencing.

`SupersededByImport` ends a run without successful completion, a completion
seal or verification credit. Its audit event names the hold, prepared operation
and reconciled receipt. Retired open work waits with explicit reopening guidance;
reopening starts a fresh unclaimed generation. A retired tree can reopen its
root first and then its retired children under that fresh root execution.
This storage foundation supplies no public active-native apply or settlement
shortcut. Responsible or unknown host execution still needs the joint host/core
reconciliation path before final apply.

The two durable tables and changed active-run index require full-store
migration before installation. Ordinary migration preserves existing durable
facts; it adds no guessed older-format conversion or compatibility path.
A separate rebuildable active-work lookup includes zero-run holds. Placement
and exact-hold resolution maintain it in the same transaction as their
canonical facts; guarded deletion cannot silently admit unresolved work.
Admission reads only its selected active hold. Doctor and explicit projection
repair validate or reconstruct complete membership from canonical holds and
their exact terminal outcomes. Supporting indexes bound selected-hold lookups;
ordinary admission does not decode the project's historical hold ledger.
Ordinary opening refuses an
unconverted store without writing; projection repair cannot seed durable heads
or invent holds, receipts or execution outcomes. Current-format export/import
preserves actual heads and unresolved holds.

## Internal exchange foundation

The library stores host-local bindings and attributed progress as
`task_sync_binding` and `task_sync_exchange_event` canonical objects in the
existing object table. These records are excluded from task-text export. They
pin the repository enrollment, the store's optional sync origin, branch,
captured target configuration and profile, and record the store path, restore
occurrence and feed cut as the caller observed them. Those observations are
compared, never treated as proof of continuity on their own. Explicit producing
origin enrollment additionally requires the private continuity checks below;
the library does not yet read the shared restore-occurrence record. Steps are ordered with explicit
predecessors and sequences. Exact request retries compare the full attributed
request. Competing activations, including a second enrollment for the same
repository, and successors that bypass unresolved progress refuse.
The original object-only slice added no SQL table, column, index or global
writer fence. The keyed historical views below add rebuildable SQL. Ordinary local
writers remain usable; an exchange rechecks its complete planning basis rather
than treating an old feed position as proof that nothing changed.

Exchange-owned raw candidates outlive preparation's current/predecessor journal.
Exact Git commit bytes, tree and ordered parents are pinned under operation-owned
private refs. Missing, partial or divergent required artifacts need reconciliation;
the library never recaptures a candidate or constructs a replacement commit as
a recovery shortcut. A read-only inspection checks the selected branch, captured
target and exact retained task blobs before a request can enter the ledger, and
the same Git observations are made again immediately before every durable
transition, so drift after inspection refuses rather than being recorded.

The explicit `DataWorktree::create_retained` helper constructs the caller's
exact staged data commit before the separate `advance_retained` step. Its
request fixes the tree, ordered parents, author, whole-second date and message;
it does not consult user/global Git identity, stage files, change the index,
advance a branch or contact a remote. The first parent is the selected local
head; an optional second parent must also have data-only orphan ancestry.
Only the operation's existing private commit retention owner and ref are used.
Replay verifies the original metadata, pin and raw bytes instead of generating
another commit. Changed selections or missing pins refuse without repair.
`engram task-sync data-commit --branch BRANCH --request @FILE` exposes this
operation through the writable, attributed, opted-in service. The bounded JSON
request contains `operation`, `tree`, `expected_head`, ordered `parents`,
`author_name`, `author_email`, whole-second `authored_at`, and `message`.
The main checkout's project marker must match the selected project before Git
is read. Keep the same request file for replay; the result is retained commit
metadata, not publication or branch advancement. No SQLite store is opened.
Invocation supplies no Git authority. Staging, explicit branch advancement and
the ledger/publication command workflow remain separate steps.

Initial import and exact-repeat import use one SQLite writer transaction for
graph validation, original provenance, the compact source-commit receipt and
progress. Dry-run rolls back every staged database effect. Before an incoming
request is prepared, an immutable private construction plan selects its record
ids, actor, timestamps and canonical body comparisons. A retry reproduces those
identified effects or refuses; a missing plan never permits reminting. Its build
identity guard refuses reinterpretation by another encoder: any change of
installed build, not only a format change, therefore strands a prepared but
unapplied import, and so does a crash after a plan, candidate or witness file is
written but before its SQL commit. Both block later operations on that binding
until a reconciliation workflow exists, which this slice does not ship. An
exact retry of an already-applied import compares the running build's planning
basis with the stored one, so after a build change it may refuse too. An
import plan is bounded at 2 MiB, about 5,000 tasks; a larger project cannot yet
prepare an incoming exchange. The doctor checks stored receipts structurally;
content fingerprints are compared only when a receipt is written. A detected restore,
external state ahead of the ledger or local divergence requires explicit
reconciliation. Replaying an indistinguishable pre-commit state with the same
identified effects does not certify that no restore occurred.

Outgoing progress records observations only. Recording `PushStarted` flushes an
immutable private witness before committing its SQL event; a surviving witness
without that exact event, or an event whose witness disappeared, preserves
uncertainty. An absent or different current remote tip never proves that an
earlier publication did not apply. It remains unresolved until an exact positive
observation is recorded. Observations carry the caller's attribution, rather than
independent network assurance. These calls dispatch no transport and publish no branch.

The retained-candidate library can record one attributed outgoing reconciliation
with `OutgoingProgress::Reconciled`. A known ancestry failure or non-fast-forward
refusal selects a fresh retained recipe whose commit has the original local tip
and fetched data tip as its two ordered parents. The original prepared event and
push witness stay unchanged. The retry has a separate publication-start event
and witness, while progress and historical lookup retain the original ledger
operation. Its final positive observation must name the exact resolved commit.
A second known failure records a stop without a publication receipt or export
cursor advance. An unknown result stays pending. Exact committed-step replay
returns the original event after reopening; a witness ahead of SQL still refuses
and preserves uncertainty. Retry artifact operations and keys cannot become new
ledger requests. This library path does not reinterpret a managed delta as a
full retained candidate; proportional outgoing dispatch remains a separate seam.

The foundation is library-only. It does not expose an exchange CLI/MCP workflow,
merge divergent/native graphs, reconcile real branch/index/shared files, enable a
remote or transfer private execution state. Existing directory import retains
its native/divergent-target refusals. The broader workflow below remains the
target contract.

Its import admits only an empty target or an exact repeat. That is a limit of
this first slice, not of the product: two-way exchange between two active
stores, each with its own local tasks and concurrent edits, is the required
target. It records its outcome on the same binding and event chain, as a
separate reconciled-import event beside the initial one, so no second receipt or
binding registry is needed.

### Keyed historical exchange lookup

The explicit retained-candidate publication adapter joins the existing keyed
operation reader and atomic progress writer. `publish_task_sync_outgoing` admits
the service's project, actor, session and opt-in, then uses a separate connection
while Git is awaited. `publish_outgoing` checks the selected data branch and
target, fetches the exact observed remote tip and rechecks ancestry before a
push. Only the invocation that commits a fresh publication-start event dispatches
that push. Replaying an earlier start or unknown observation can observe the
remote tip, but never sends the push again. Exact tip confirmation uses the
ordinary canonical publication outcome; absence, another tip or a transport
failure stays unresolved.

A known ancestry mismatch or a completed exact non-fast-forward rejection
returns the fetched tip for agent reconciliation. The agent supplies the one
retained two-parent recipe through the existing reconciliation transition. A
second known failure records the explicit stopped transition and returns it;
another retry is refused. The rejection itself is recorded as `PushRejected`,
naming its exact start, before the replacement tip is fetched. A repeat, a
reopen or a lost fetch or response then resumes only that fetch and returns the
same reconciliation step, or a retry's stop. It never pushes again and is never
recorded as unknown, and no observation is admitted after it. A genuinely
uncertain result keeps its unresolved observation. No merge text, commit
metadata or publication success is invented by the adapter. The original
request and each publication witness
remain retained. This is an explicit library/service path for complete retained
candidates; managed proportional publication and the ordinary CLI
exchange workflow remain separate implementation work. The transport still has
the existing Windows-only process-confinement boundary.

`SqliteStore::completed_task_sync_exchange` is a read-only library seam taking
`CompletedExchangeRequest { binding, intent }`. It compares the complete original
intent, including actor, producer, retained candidate, Git selection and planning
basis. It returns the original prepared id, outcome id and canonical outcome
event for an applied initial or reconciled import, or a confirmed outgoing push.
Preparation, local commit, push start and unknown observations are unfinished;
an explicitly stopped outgoing operation also has no publication outcome.
An absent or unfinished matching operation returns `None`. A retry must retain
the original binding: absence under a successor binding is not permission to
reapply an earlier operation. A reused key or
operation with different content refuses.

The lookup reads one snapshot and uses keyed canonical indexes plus derived
operation/head rows. Missing rows, stale selected links or a mismatched canonical
tail refuse with explicit projection-repair guidance; they never trigger a full
ledger scan. Work grows with the selected operation's own progress, not unrelated
exchanges. No retained files, task documents, import origins or current planning
graph are read. Later local edits, later exchanges and a successor binding do not
invalidate a historical outcome. This is evidence of the earlier operation, not
authority to reapply it or publish anything. Existing mutators keep their current
admission and retained-file checks; ordinary CLI/MCP exchange orchestration does
not yet call this seam.

`work_task_sync_exchange_operations` and `work_task_sync_exchange_heads`, and the
`objects_task_sync_exchange_*` indexes, are rebuildable projections. They store
canonical links and transition state, never another intent or receipt body.
Existing event writers update the affected links inside their canonical writer
transaction. The canonical transition reducer is shared with full ledger
validation, projection derivation and doctor comparison. Repair rejects malformed,
forked or incomplete history and rolls back rather than manufacturing provenance.
Canonical records, old fingerprint meanings, ids and receipt bytes are unchanged.

This projection layout requires explicit `engram doctor --repair-projections`
on every live store. The new binary refuses an unrepaired layout without writing;
the old binary refuses the repaired layout. The two-build fixture is
`scripts/task-sync-ledger-cutover.fixture.mjs`; run it with actual old/new artifact
paths in `ENGRAM_TASK_SYNC_OLD_BINARY` and `ENGRAM_TASK_SYNC_NEW_BINARY`, and the old
artifact's full revision in `ENGRAM_TASK_SYNC_OLD_REVISION`.

The ledger foundation lands together with the completed indexed graph-closure
projection set, under one coordinated repair cutover. Before that landing the
integration owner verifies readiness and no-write refusal on copies of every live
store and prepares backup, install and rollback. Every consumer of the old build
must be gone before repair; Greg performs the agreed TermAl restart. After repair,
the new build must report every live store ready before sync resumes. Before
repair, rollback may restore the old executable. After repair, that executable
refuses the new layout: rollback must restore the pre-repair store backup too,
and discard or explicitly reconcile any intervening writes. There is no
implicit first-open projection fill, old-record conversion or full-store migration
for these derived structures. The incoming delta writer described below joins
graph admission and its outcome; historical lookup cannot record an applied
outcome independently of those changes.

### Selected origin ledger reads

Origin repository, anchor, recovery, settled-state and custody checks share the
bounded canonical binding/operation readers used by historical lookup. The
`objects_task_sync_binding_*` and `objects_task_sync_exchange_push_started`
indexes select canonical scope, successors, repository spellings and historical
publication starts. Selected operations pass through the existing reducer and
their operation rows must agree exactly. Settled tails recover their prepared
cut through their canonical transition; no second lifecycle field is stored.

These readers validate selected facts. Doctor and explicit repair validate
disconnected history and full receipt provenance. A missing or stale required
projection refuses without a full-history fallback. Custody includes superseded
bindings and same-root bindings from other projects, and confirmed historical
pushes still require their original publication witnesses. Legacy custody still
enumerates retained files, and existing exchange writers still perform their
full admission. These changes alone do not provide the ordinary strict-delta
I/O bound.
The existing full-admission writer supplies its admitted ledger and filesystem
alias comparator to the same custody validator; the indexed origin reader keeps
its existing stored-spelling comparison.

### Producing-origin enrollment and continuity

`engram task-sync origin` reads this project's current logical producing origin
as JSON, or `null` before enrollment. It does not enroll, repair, or opt in.
`engram task-sync origin-enroll --operation UUID` explicitly enrolls the
project-local store. It requires task-sync opt-in and a writable service. The
operation UUID identifies the exact request; retrying the current enrollment
returns its original record rather than minting another origin. Both commands
use the ordinary home, project and actor options.

To fork an existing lineage, use `origin-enroll --operation UUID --predecessor
ORIGIN_ID` with a new operation and the current predecessor. The predecessor and
all prior records remain unchanged. Forking requires settled exchange progress
and coherent private custody; it never reactivates a superseded origin. A
retained pending enrollment blocks exchange effects until its exact retry
finishes or an explicit uncommitted cancellation succeeds. Competing requests
refuse. Enrollment emits a
project-feed entry visible to `work next`, with an attributed enrollment summary.

`origin-cancel --operation UUID [--predecessor ORIGIN_ID]` cancels only the exact
uncommitted private enrollment intent. It requires a writable, attributed
service and remains available after task-sync opt-out. Under the project
reservation and a SQL writer, it checks
that neither the retained minted ID nor its enrollment operation is canonical,
the current predecessor and private witness still match, and the witnessed feed
and exchange anchors remain present. It returns JSON `{"cancelled": true}` when
the slot is removed, or false when no slot remains and the operation is still
uncommitted. A committed enrollment must be recovered with `origin-enroll`.
Cancellation preserves the witness, including a pending exchange reservation,
and every retained candidate, import plan and publication witness. This lets a
candidate retained after an interrupted enrollment proceed under the unchanged
origin; it does not admit unknown artifacts or repair divergent custody. The
proof is absence from the current store, not a claim that the enrollment never
committed before a rollback of every independent proof together.

The canonical `task_sync_producing_origin` record is scoped to one project. Its
minted ID, asserted author, operation and predecessor describe the lineage;
machine aliases, host opt-in and restore occurrences remain separate. Enrollment
changes binding admission: new bindings must name the current origin, and a
settled legacy binding needs an explicit successor under that origin. Existing
bindings are never rewritten. There is no un-enroll operation; disabling task
sync retains enrollment and custody history.

The installation-local `task-sync-origin-pending.json` and
`task-sync-origin-registration.json` files live beside the project database.
The pending slot retains an enrollment's minted ID, body and expected witness
before SQL mutation. The registration file records its feed/exchange references
and any exact pending exchange request. The project reservation spans writer
admission and post-commit acknowledgement; uncertain effects preserve their
pending state for exact recovery. These files provide cooperating local custody,
not authentication, cross-host execution authority or a publication receipt.

Task-text export and full-store export do not transfer those private files.
A copied, restored or migrated store can retain the same canonical origin while
lacking its installation witness. Exchange-ledger writers (binding activation,
preparation, application and progress) then refuse as quarantined; they never
adopt that origin from a loaded record or an alias. Ordinary folder `import`,
`export` and `prepare` routes are not yet guarded by origin continuity.
Ordinary local work and origin inspection remain separate. A missing or divergent
witness requires explicit cold recovery, rather than ordinary enrollment.
`task-sync origin-recover --operation UUID --predecessor ORIGIN_ID
--cause db-only-clone|same-origin-rewind|migration-import` appends a new local
successor and an attributed `task_sync_origin_recovery` fact. It never adopts
the predecessor's custody. A retained exact recipe fixes the successor and
recovery UUIDs before SQL; a crash retries those same bytes. Original private
enrollment, registration and audit bytes are retained in a bounded no-follow
archive under `task-sync-origin-recovery/OPERATION/` beside the database before
active slots are replaced. Receipt replay alone never recreates a lost witness.
The canonical receipt is history, not proof of current exchange admission.
`origin-recovery-cancel` takes the same operation, predecessor and cause and
releases only that exact recovery recipe after a writer proves its canonical
origin/recovery IDs and operation absent and its predecessor/private custody
unchanged. It preserves the archive and all predecessor intents/artifacts. This
allows a fresh request after unrelated local work invalidates an uncommitted
recipe's captured cut. A committed recovery cannot be cancelled this way.

The predecessor's bindings and pending effects remain historical/quarantined,
never falsely settled or cancelled. Explicit successor binding admission uses
the recovery relation rather than repurposing settled-only `supersedes` fields.
Known unresolved effects on the same target remain held for explicit ledger/Git
reconciliation. Reported newer witness references missing from a rewound DB
remain visible as unresolved history; absence of those rows is not proof that
nothing happened. Ordinary local work and passive retention remain available.
Full-store migration import preserves canonical history and does not copy private
custody; recovery explicitly creates that installation's own successor afterwards.
Projection repair reconstructs canonical views only and cannot create a witness.
Likewise custody still requires historical repository paths and publication
witnesses, including those of superseded bindings; relocation or retirement
needs a later explicit cold-audit remedy. No such command is supplied yet.

### Indexed graph capture foundation

The library exposes `capture_task_sync_graph_component` for complete connected
components of immutable origins or the retained exported baseline,
`capture_task_sync_native_component` for current durable native adjacency,
`capture_task_sync_native_component_with_profile` for the same connected read
with an explicit notes and machine profile, and
`capture_task_sync_baseline_overlay` for an overlay checked through the existing
graph validator. These reads are scoped to one snapshot and traverse forward and
reverse parent, prerequisite and replacement relations. The native route uses
the shared per-task exporter, defaulting to notes off unless a profile is
selected. Disconnected task documents are not loaded. Connection-level helpers
let the atomic owner capture and validate inside its existing writer; captured
public observations are never admission proofs. Oversized connected scopes
refuse explicitly.

The views stay distinct: native durable state, exported baseline and immutable
origin are not substitutes for an admitted incoming source snapshot. Captured
documents and components grant no application, publication or execution
authority. Original import identities and current graph-member identities have
separate indexed lookups. Historical planning or checkpoint copies alone do not
reserve a local addition's identity. Source/export projection replacement checks
retained current owners while excluding members the same delta replaces or
removes. Explicit cold source bootstrap is described below; the incoming writer
uses these selected views for fresh graph admission. The document, shared-identity and graph
tables are explicitly rebuildable; ordinary native note, claim, gate and planning
events add no embedded-document projection work. Their complete projection set
joins the single coordinated cutover described above.

The folder admission owner also accepts private connected document maps and
validated snapshots, with its caller supplying fresh immutable-origin and
durable graph checks. It shares the complete-folder conflict ordering and
relation-removal, addition, scalar, relation-addition and disposal phases.
Task construction consumes project metadata and a task snapshot. These internal
seams alone do not establish source membership, origin continuity or an exchange
receipt; the incoming atomic writer joins those checks and records.

New delta receipt task outcomes name the local imported head explicitly. Several
tasks can share one baseline-planning record, whose work-keyed member can be a
tombstone; a native task can retain no imported head. Storage must check the exact
canonical owner and work member. Historical reconciliation receipts still name
their original shared-version records and keep their existing wire shape.

The reconciliation-basis reader follows indexed root membership and prerequisite
edges in both directions. It retains each reached task's generations and claim
fences without loading disconnected project tasks or unrelated prerequisite
edges. This remains a local work observation, independent of source or export
membership.

`capture_task_sync_membership_component` reads the binding's incoming source or
prepared export membership separately from that local baseline. It returns the
canonical head and comparison root with the selected connected component.
Canonical head metadata comes from a rebuildable expression index for cold
checkpoints, delta preparations and applied source outcomes. An applied source
reads its original preparation's metadata too, without loading either task
payload or the full bootstrap inventory; selected members use the same forward
and reverse graph traversal. Missing membership refuses with explicit bootstrap guidance.
These observations make no application or confirmed-publication claim. The
incoming writer advances membership only with its committed applied outcome.

The internal delta membership projector and its cold canonical replay use one
source-relative recipe: incoming outcomes resolve `source_changes` through
their canonical member owners, independently of the local planning delta;
outgoing preparation advances the prepared export role only. Root and
predecessor comparisons finish before updates, and projection writes stay in
the caller's transaction. Canonical incoming admission validates the atomic
writer's planning and receipt links. The local library operation
`prepare_task_sync_delta_export` captures only the declared native changes,
checks their connected graph and membership against the current export basis,
then retains the exact request and bytes under producing-origin custody and
appends `DeltaPrepared` in one writer transaction. The prepared export projection
advances with that event; confirmed publication stays separate. Exact retry
returns the original event and bytes even after later native edits. A SQL
failure preserves retained bytes for the same intent, and missing or changed
bytes refuse rather than being recaptured. This operation creates no Git commit,
ref or transport effect. The same-named service method enforces opt-in,
write access and the current project, binding, actor and session before opening
SQLite. It keeps the existing full-ledger capacity and custody
admission costs. The explicit command steps below expose the same owner;
they do not automate proportional selection or dispatch transport.

The library's explicit `record_delta_export_commit` inspection binds an already
retained data-only Git commit to that exact preparation with the existing
`DeltaLocalCommitRecorded` event. It checks the dedicated data worktree, commit
pin, complete selected snapshot membership and retained changed bytes before
the storage writer checks canonical preparation and origin custody again.
This inspection reads the complete selected snapshot within the caller's time
budget; it is not the ordinary proportional path. It creates no commit, branch
advance or publication. Exact retries preserve the event identity, including
recovery after SQL committed but its private acknowledgement was lost. A failed
SQL write retains the original selection for the same request. The command
step records this selected commit; publication remains a separate explicit act.

The service entry `record_task_sync_delta_export_commit` applies write-access,
opt-in and current project, binding, actor and session checks before opening
SQLite or reading Git. The awaited inspection owns its connection and does not
hold the ambient service mutex. The entry retains the library operation's
complete-snapshot cost; `task-sync step` delegates to this same owner.

The explicit `record_delta_publication` library entry and
`record_task_sync_delta_publication` service entry record a caller-owned start,
exact refusal or observation against that original delta preparation and
selected commit. Both use the `PushStarted`, `PushRejected` and `PushObserved`
transitions and the shared reducer.
A start's fresh flag is returned only after the SQL commit; replay returns the
original event with a false flag. The publication witness is flushed before
SQL commits, so rollback or rewind retains uncertainty and refuses another
dispatch. An observation is the owner's own read-only read of the selected
remote tip through the same confined transport the publication driver uses;
a caller states only that it observed. An absent or different observed tip
remains unresolved across reopen, and only the selected remote tip being the
exact retained commit confirms publication. A caller's confirmation that the
read contradicts refuses and records nothing. These operations never push and
grant no Git authority. The service applies the same early
opt-in, writer and actor scope checks. The library checks selected Git metadata
and retained delta bytes, relying on the canonical commit binding for complete
snapshot membership, while retaining exhaustive ledger custody costs.

The library's `publish_delta_outgoing` and the service's
`publish_task_sync_delta_outgoing` drive the first selected delta publication
through the existing bounded, confined Git transport. They check the exact
canonical delta commit, target configuration, data-only history and retained
bytes, and fetch and check the selected remote tip before starting. Only the
invocation that commits a fresh start may push. An existing start or unknown
outcome observes without resending; a confirmed canonical outcome returns its
original event even with retained files unavailable. A known ancestry change
or completed non-fast-forward refusal returns the explicit agent reconciliation
step. This entry does not admit a resolved delta retry. The service applies the
same early opt-in, writer and attribution checks, and invocation grants no Git
authority. The explicit command steps record agent-resolved retry decisions;
they do not expose this automated publication driver. Linux/macOS automated transport remains refused by the existing
transport implementation; scripted decision tests do not prove platform support.

The additive `DeltaResolved` ledger recipe and common reducer retain the
original native preparation and cut, alongside the agent's separately selected
merged Git membership and changed bytes. Its data commit retains both original
local and fetched parents, in that order. Both known failure paths consume the
same single retry; a second known failure records a stop without a publication
outcome. Unknown publication cannot select another recipe. Keyed canonical
lookups reserve the retry's artifact operation and key without creating another
pending exchange. The explicit reconciliation entries below admit the selected merged graph
and record retry progress. Command invocations never dispatch a push. Historical confirmation reads bind the merged membership
and commit to the exact canonical recipe and retry start, through any intervening
unknown observations. They retain the original native preparation as the next
native export basis. Covering metadata reads omit the recipe's task documents;
rebuilding projections and later preparations preserve that distinction.
Cold and keyed custody reads
account for the original and resolved candidates, selected commits and separate
publication witnesses. Each started attempt requires its own witness; a missing
witness or a retained retry ahead of canonical history remains uncertainty.
The cold audit compares a retry's selected commit to its resolved recipe, without
substituting the original commit or creating a second pending operation.

The library's read-only `inspect_delta_resolution` binds an opaque observation
to the original canonical preparation and selected commit, the two retained
payloads and commits, and the fetched failure pin. It checks data-only history
and target configuration, then reads both complete Git task snapshots under
the caller's time bound. The actual merged graph, recomputed membership and
exact task additions, changes and removals must match the recipe. Embedded
inventory text supplies no proof. This explicit cold inspection changes no
SQL, files, refs or publication state, and does not recapture native planning.
`record_delta_resolution` repeats this inspection before its storage writer
rechecks both retained payloads and commits, the canonical failure stage and
private origin custody. Its memory-only artifact exception binds the complete
original request, selection and merged recipe to the private pending request;
an ordinary candidate fingerprint cannot admit merged artifacts. The canonical
append retains the original pending preparation, and an exact retry recovers
a lost private acknowledgement without replacing native planning or granting
transport permission. SQL rollback preserves retained files; unknown transport
and ahead publication witnesses remain uncertainty.

`record_delta_retry_progress` repeats the same bounded inspection and records
the admitted recipe's start, observation or explicit stop under the original
pending preparation. Only a newly committed start returns fresh permission;
replay returns the original event without permission. The retry has its own
flushed publication witness, and a failed SQL commit leaves that witness ahead
of the ledger and blocks another start. Unknown results remain observable,
while a second known refusal can stop without confirming publication. Exact
private acknowledgement recovery binds the original recipe and progress step.
`publish_delta_retry` uses the existing confined Git transport after inspecting
the admitted recipe. A new durable start permits one push. Existing starts and
Unknown results only observe the remote, and confirmed or stopped canonical
outcomes replay without Git or retained files. A second ancestry or strict
non-fast-forward refusal records a stop, without another recipe or publication
confirmation. Scripted transport fixtures exercise this orchestration with real
SQLite/private custody; they do not prove live transport on Windows. The service
entries `record_task_sync_delta_resolution`,
`record_task_sync_delta_retry_progress` and `publish_task_sync_delta_retry`
require writable access, opt-in and matching original and resolved project,
binding, actor and session before opening SQLite or reading Git. Each awaited
operation owns its connection. The complete unattended workflow still needs
wiring and end-to-end Windows proof.
Complete Git snapshot and exhaustive ledger/custody reads in these explicit
entry points remain proportionality follow-up work.

### Producing-origin lookup projections

`work_task_sync_producing_origin_members` retains canonical enrollment links,
project, predecessor, operation and project-feed position.
`work_task_sync_producing_origin_heads` selects each project's current head.
The `objects_task_sync_producing_origin_*` indexes provide keyed canonical
project, predecessor and operation probes. Enrollment appends update the links
in the same transaction as the canonical record and feed entry.

A head read decodes its selected enrollment and checks its feed proof and link
metadata. Missing or stale links refuse without replaying history. Doctor and
explicit projection repair validate all enrollments, operation uniqueness,
predecessors, cycles and competing heads through one shared derivation. Completed
ancestry is reused across that traversal. These projections preserve enrollment
bytes and IDs and do not replace the private continuity witness. Their cutover
joins the other task-sync projections; no ordinary open fills missing rows.

### Typed delta payloads

The library defines additive delta intent, retained changed-document payload,
source-planning references, import receipt and managed-custody checkpoint types.
The types provide pure shape and content comparisons; the incoming library
application operation below separately supplies storage admission. The explicit
cold audit below uses the checkpoint payload. These types do not
change the ordinary export, import or preparation routes.

`InventoryRoot` uses the existing folder-baseline member-leaf and modular-sum
algorithm. Historical baseline fields keep their string representation and
meaning. A root compares membership content; it is neither a record identity
nor an integrity guarantee.

Incoming source-before, local destination-before and the explicitly admitted
resolved source-after are separate. Their changed-key sets can differ. One
retained payload holds the union of selected documents, while a canonical intent
holds only its descriptor. Source-planning links refer to the existing canonical
task owners instead of copying bodies into a second canonical inventory.

`DeltaPrepared` owns its exact selected payload beside the intent. Its minted
event ID, rather than the payload fingerprint or retained filename, owns each
present document. The document/identity projections and their explicit rebuild
resolve these members from canonical bytes without retained files. A prepared
owner does not make a document an imported origin, an applied source member or
a local destination member. The incoming writer preflights the complete
wrapped event's byte bound before reserving its private exchange effect.

Prepared-export and confirmed-publication bases have different typed anchors.
Preparation does not assert delivery or require inventing a Git commit. An
actual retained commit selected later has a separate payload linked to its
prepared request; historical local-commit event bodies remain unchanged.
The additive `DeltaPrepared`, `DeltaLocalCommitRecorded` and `DeltaImportApplied`
variants share the existing ledger reducer and keyed operation readers. Legacy
and delta requests cannot use each other's commit or import transitions. Outgoing
delta preparation has no selected commit until its local-commit event; confirmation
requires that exact selected tip. Unknown publication remains unresolved.

Canonical incoming admission joins planning, canonical member owners and affected
projections in one transaction. Outgoing canonical admission remains disabled.
Pure replay and selected historical reads do not supply that admission. An
incoming outcome's qualified cut preserves the binding, origin and restore
observation and cannot move backwards; its canonical planning links and graph
effects still require writer validation.

Cold custody inventory recognizes complete retained delta requests and selected
payloads through the existing retention owner. It rejects partial or unknown
files, operation aliases, and competing legacy/delta candidates. Custody compares
the binding and full typed intent against its canonical preparation. A fresh
writer's exception requires the exact typed request fingerprint; an operation
UUID or a legacy request alone does not admit a delta candidate. Cold audits
account for these candidates and their canonical commit/publication provenance.
These checks retain the exhaustive history/filesystem cost and keep repository
heads audit-only; they do not activate managed writes.

`completed_task_sync_delta_import` is a receipt-first historical observation.
It compares the complete incoming intent using canonical covering metadata,
with a separate exact-request column so ordinary head reads stay small,
checks its predecessor and exact operation/outcome links, and returns the original
outcome after later ledger movement. It decodes the binding and receipt, without
opening the prepared task payload, bootstrap task documents or retained files.
It refuses changed requests and missing or stale selected links. Like the existing
historical lookup, it relies on previously admitted canonical records; doctor
retains exhaustive payload and projection validation. It supplies no application
or private-witness recovery authority. The metadata index joins the same single
projection cutover as the other task-sync views.

`apply_delta_exchange` first returns an exact historical receipt before reading
Git or retained payloads. For a new incoming operation, it uses the existing
configured-target and retained-commit inspection owner and hardened commit-delta
reader, compares the exact selected documents, and passes a private inspected
input to storage. Storage rechecks selected retained files, enrolled origin and
private continuity, the shared restore record, project cut and destination
baseline. Connected source, destination, native and immutable-origin views feed
the existing folder admission rules inside the same writer transaction.

That transaction records the selected preparation, native graph edits, local
baseline changes, source member links, imported heads and one applied receipt
with their projections. An error rolls them all back. Execution reconciliation
still belongs to its existing owner; this writer cannot manufacture holds or
terminal evidence. Exact receipt replay is an observation of the original commit
and does not itself acknowledge a private reservation left after a process loss.

`recover_task_sync_delta_import` explicitly finishes that acknowledgement through
the same origin-custody owner. It requires the original committed request and
canonical receipt, current enrolled origin and restore occurrence, and complete
retained custody under the existing project lock and SQL writer. Only the exact
pending request may advance to its committed outcome; a different reservation,
missing pending proof or visible rewind refuses without clearing it. Recovery
creates no SQL records or task effects. An already acknowledged repeat preserves
the current private witness. Unlike offline receipt lookup, this operation can
refuse when retained files or restore provenance are unavailable.

This first incoming writer retains exhaustive ledger and filesystem custody
validation. Its local baseline also retains the existing full destination
BEFORE-export comparison, requiring a complete export capture. Those costs have
not become proportional to the changed documents. Repository heads remain
audit-only, with no ordinary CLI/MCP delta route or managed-writer activation.

`inspect_task_sync_delta_export` separately checks a declared outgoing request
on one read snapshot. It reuses selected native capture and validates the
prepared-export head, current origin-qualified cut, restore state, selected
document bytes, output membership and connected export/native graphs. Confirmed
publication remains a separate canonical basis; a legacy full-tree fingerprint
cannot stand in for its missing membership root. The observation creates no
retained candidate, preparation, progress or publication record and grants no
later write authority. The writable outgoing lifecycle and its exact publication
witness recovery remain pending.

The library's selected planner reuses the complete-folder field comparison
rules for explicit local delta keys. Context omitted from that selection is
retained; an explicit removal requires known local exchange membership. This
classification does not validate connected graphs or admit a write. Complete
folder import keeps its existing whole-project contract.

A strict cut names a binding, a non-null producing origin, a project feed and
anchor, and the observed restore occurrence. Pure validation checks declared
consistency only: storage must establish actual origin custody, restore state,
feed continuity, graph admission and writer freshness. Checkpoint metadata alone
does not audit retained artifacts, settle uncertain effects, create a checkpoint
event or supply a repair remedy. The cold audit below separately establishes its
storage and custody admission; ordinary delta application remains pending.

A checkpoint can declare one explicit membership seed: an incoming exact Git
snapshot or this host's prepared-export membership at its local cut. Incoming
members can name a complete initial-import receipt; a reconciled receipt cannot
supply that source basis. Alternatively a complete captured map contains
canonical document links and documents that have no suitable canonical owner.
It contains present members only, with no tombstones. Storage must resolve the
links, admit the complete graph, recompute its membership root and establish the
exact snapshot or export cut. Embedded documents pass the existing field and
disclosure checks; when every member is embedded, pure validation also compares
their root. A matching root alone proves no storage or Git admission.

The containing checkpoint event owns newly embedded documents, so
repair can reconstruct admitted membership without retained Git files. No seed
means custody-only metadata, not an empty source. An export seed does not assert
confirmed publication.

### Explicit cold custody audit

`engram task-sync custody-audit --request @REQUEST.json` runs one named cold
audit through the same writable, attributed service and opt-in checks as origin
enrollment. `--request` also accepts inline JSON. Input is a strict
`CustodyAuditRequest`, bounded to 16 KiB: binding, operation UUID, request key,
reason (`named_audit`, `named_rebuild` or `bootstrap`), previous checkpoint,
expected settled ledger tail, origin-qualified local cut and seed selection.
The caller retains that exact request for recovery. This is an explicit
library/CLI operation; opening a store, local work and ordinary sync never run
it implicitly. It performs no Git transport or task-text publication.

The seed is `{"kind":"custody_only"}`, `{"kind":"local_export"}`,
`{"kind":"initial_import","outcome":"RECORD_ID"}`, or
`{"kind":"incoming_commit","commit":"FULL_OID","prepared":"RECORD_ID"}`.
Initial import resolves the original complete admitted receipt, independently
of later local planning. A historical reconciled receipt has no reconstructable
incoming source basis and refuses explicitly. Local export captures present
documents at the named local cut. Incoming commit reads the complete task tree
through the hardened local Git reader and requires the existing canonical
prepared event, retained selection and exact retention ref; a raw observed
commit cannot invent retention authority. Source and export membership stay
separate, and custody-only creates neither an empty source nor an export basis.

For a wrapped folder-envelope commit on the dedicated data branch, select
`{"kind":"incoming_data_commit","commit":RETAINED_COMMIT}`. First retain that
exact commit with the audit request's operation UUID through `retain_commit`.
The checkpoint's incoming seed owns this commit-only pin; no legacy raw-document
preparation is created. The CLI uses `audit_incoming_data_commit`, which observes
the actual data worktree, HTTPS target configuration and retained Git selection
before the storage audit. The cold reader verifies the complete folder and exact
profile, commit bytes and private ref. Later audits retain the same typed pin
artifact, while unrelated or conflicting artifacts still refuse. A completed
request replays before observing today's branch or configuration.

This seed records source membership and does not apply the initial task tree.
When starting an empty destination, explicitly apply the complete observed
folder through the existing folder-import owner before incremental application.

A fetched carrier that never acquired a ledger request can be explicitly
abandoned through the same custody-audit command, selecting
`{"kind":"abandon_fetched_commit","binding":"ORIGINAL_BINDING_ID","commit":RETAINED_COMMIT}`.
Use the fetched operation UUID as the audit operation, a named audit or rebuild
reason, and the current active binding and settled tail. The active binding must
be the original binding or its successor in the same repository, store and
origin. The audit checks the exact commit-only private selection, Git bytes and
pin, refuses canonical exchange progress or pending publication/execution
uncertainty, and preserves every file, ref and original record. Its checkpoint
has no membership seed and grants no task, evidence or publication coverage.
The abandoned operation cannot subsequently become an exchange request; a new
exchange uses a new operation. Transient pre-ledger failures still allow an
ordinary corrected retry of the same operation until abandonment is requested.
Unrelated unknown retained operations continue to block the audit.

Under the project reservation and sorted repository reservations, the audit
examines retained candidates, commits, import plans, publication witnesses and
earlier audits, including superseded bindings. Unknown, partial or unresolved
effects refuse. Historical repository relocation and missing origin-witness
adoption remain unsupported. The shared restore owner must report no occurrence
for an unbound store, or the exact completed occurrence named by the binding;
pending and unreadable records refuse, including a pending replacement with a
previous completed occurrence. Copy and local-file restore sources use the same
occurrence comparison. The changing live database is never compared with the
restore's acquisition SHA-256.

Before SQL append, the installation-local
`task-sync-custody-audit-pending.json` slot retains the complete request and
exact minted checkpoint/body comparison. The repository's
`engram-task-sync-custody-audits/` namespace retains the full event, bounded to
16 MiB. Capacity and shape are checked before retaining it. One SQL transaction
appends the existing ledger's additive `CustodyCheckpoint` event and updates its
exchange and source/export projections. Origin acknowledgement precedes the
repository's immutable acknowledgement and `head.json`; private cleanup comes
last. Repository heads remain explicitly `audit_only`: ordinary managed delta
writes are not enabled by B1.

An interrupted operation blocks another exchange or enrollment. Its exact
retry uses the original retained event and cut, without recapturing newer task
text or minting another checkpoint. Missing or partial retained bytes refuse
and are preserved. Recovery accepts only the exact predecessor witness or that
checkpoint's exact acknowledged successor; divergence and visible SQL rewind
refuse. A completed historical request returns the original checkpoint;
`repeated` reports replay and `acknowledged` observes its local immutable
acknowledgement. It never resets a later source/export head. A result with
`acknowledged: false` grants no authority to settle custody.

Doctor and explicit projection repair derive source/export membership from
canonical initial-import links or embedded checkpoint documents, without Git.
Repair creates no private continuity proof. This full projection set joins the
single coordinated every-store cutover; B1 has no separate landing or repair
window. Managed artifact writers, outgoing atomic delta application and ordinary
sync integration remain pending; `task-sync step` exposes the existing
selected incoming application owners without enabling unattended integration.

## User contract and Git layout

Sync is optional and explicitly configured. Without configured export/sync,
Engram neither creates nor changes `.engram/`; all local work continues and
there is no background activity. A remote is unnecessary for local-only work.
Enabling export/import or sync, choosing
the repository/branch, pushing and pulling are visible deliberate actions.
Disabling sync stops exchange and leaves the local database usable and writable.
It does not delete remote history. No release, ownership transfer, startup
remote check or online lease is required. Separate machines may execute the
same task; merging records cannot undo duplicate external effects.

A joining host configures its own origin/store identity, stable project binding,
authorized remote and branch, and explicit local opt-in/payload profile. It fetches
published planning in the data worktree, validates project/profile, and performs
an empty-target import preserving shared IDs and provenance. Its own origin stays
local; the import receipt and applied commit/tree establish its comparison basis.
Existing local tasks require reconciliation rather than replacement. No claim,
grant, private scratch or native runtime authority transfers. Exact configuration
syntax follows the implementing surface; this paragraph introduces no new shipped
command. A TermAl enrollment/display association is not required for Git task sync.

Use the dedicated orphan data branch and its persistent `.engram-data/`
worktree, separate from every code branch. Inside that worktree the versioned
interchange layout is:

```text
.engram/
  .gitignore
  .gitattributes
  project.json
  tasks/
    <full-task-uuid>.json
```

`project.json` identifies the project and exact supported interchange format,
never a machine or store identity. Task UUIDs travel unchanged; short display
refs are derived, not alternate identities. Import refuses an ambiguous short-ref
collision explicitly rather than merging distinct UUIDs.

Use pretty-printed UTF-8 JSON with LF, a final newline, stable key ordering,
one scalar field per line and multiline arrays with one element per line
(structured elements are themselves pretty-printed). Sort set-valued fields
deterministically; preserve meaningful order in acceptance and notes. Equal
shared state produces equal bytes. This is agent-editable interchange, not
compact RFC 8785 canonical-object serialization.

Enablement adds a path-scoped `.engram/** text eol=lf` Git attribute alongside
the narrow ignore allowlist. Import accepts CRLF JSON by normalizing line
endings to LF before interchange comparison; exports always emit LF. This
changes neither stored canonical object bytes nor record identities.
Line-ending normalization applies only to interchange document line endings;
decoded JSON string content, including escaped CR/LF, is preserved. It does
not change Git commit provenance or the host's existing source/freeze identity
rules.

Each task document represents its current shared
state: stable task id, title, description, acceptance criteria, planning status,
kind, priority, labels, assignment, defer-until, parent/child requirement and
prerequisite ids, blockers, supersession/disposal, machine-qualified work-activity
notices, and shared notes with stable
note ids. Preserve outcome text and explicit per-item acceptance-evaluation mode
as planning policy; importing it never imports an evaluation result or waives
a host-bound requirement.
The field schema is a typed allowlist; it is not arbitrary database-row export.
The note inclusion and new-note rule below preserves immutable original notes.
Git records document history;
the document is not an immutable canonical object and its task id is not a
canonical version id. A fingerprint compares content; it identifies no task.

The initial profile exchanges task planning and shared notes, including closed
tasks. An imported completion is attributed as a reported outcome; it must not
manufacture a local completion seal, acceptance pass or satisfied host obligation.
Runtime authority and historical evidence are separate from editable planning
state. If an import would alter a live run's governing planning basis, core
admission must invalidate/reconcile that basis or refuse the affected batch;
import is not a bypass around lifecycle rules. The lifecycle mapping below
governs terminal/reopened state; concrete typed
encoding and its tests must be specified before implementing import.

Live claims, sessions, grants, retained prompts, delivery cursors, private
scratch, credentials, machine-local path bindings and runtime tables are
excluded. Foreign-machine provenance remains machine-qualified; it never
rebinds a local session, source root or host-verification credit. Shared note text
is explicitly published content, not proof of automatic secret removal.
Enabling exchange records the chosen repository's public/private visibility
and intended readership as operator assertions. Unknown disclosure scope is
refused rather than assuming code readers may read every work note.

Note bodies are opt-in per project. Only explicitly selected generic work-note
and status-note families are eligible, with original note id, author, time and
origin-machine provenance. Gate, host verification, environment, evaluation,
execution-observation, retained-prompt and source-root/path-binding records are
never note-export candidates. Notes containing locators or machine/user-profile
paths are excluded by default, with visible omission counts/reasons; opting in
to notes is not permission to export those excluded records. Detection rules
must be explicit and tested, not advertised as comprehensive secret scanning.
Changing the selected profile requires explicit re-baselining so omitted local
notes are never interpreted as remotely deleted.

Restricted-labelled notes and task-field content are excluded by default,
with visible omission counts and reasons. Widening requires an explicit,
attributed change of the disclosure profile and re-baselining; opting in to
a note family does not widen sensitivity access. A `secret-ref` is carried
only as the labelled reference body; export never dereferences it. Enablement
and each export visibly report the configured redactor's status, including
that the V1 `DevelopmentNoopRedactor` filters nothing. Family/path filtering
and sensitivity labels do not establish automatic secret removal. Git history
persists across clones and reflogs, so exported content cannot be reliably
recalled.

An omitted restricted field is not a deletion or an empty replacement on
import. If the supported interchange schema cannot represent that omission
while preserving required fields and reference closure, refuse the affected
export with its reason; never emit a partial task as a complete final state.

An agent-changed note is a new note, not a version or edit of the original.
Keep the original unchanged, and prepare a new attributed note citing its
original id and origin machine. The agent supplies a new note without an id;
Engram's candidate preparation mints its fresh id before the resolved text is
committed. Existing imported ids are preserved; arbitrary new id-bearing draft
notes without prepared/imported provenance are refused, not accepted as ids the
agent may invent. This is structural provenance, not authenticated authorship.
Every importing machine
preserves the same shared identity. The importing session records its local
import event separately with source-commit provenance; no new local receipt id
or timestamp is serialized back into the shared note. An altered original id/body
pair is refused. An unchanged imported note retains its shared attribution.

Project memories use the separately selected
[memory profile](#project-memory-text-exchange). Complete native execution-history
transfer remains outside the initial task profile; unsupported data must be
reported, never silently presented as a complete backup. Existing backup/migration
formats remain separate.

For the first import into an empty, unenrolled store, a missing task file
cannot dispose an existing task: no prior exchange inventory exists. An
omitted isolated task cannot be detected; omitted referenced tasks refuse.
Cancelled and superseded tasks carried in the folder remain documents with
explicit terminal states. With the memory profile, `forget` travels as a permanent
body-free tombstone; omission of a memory never means forget.
An enrolled destination instead applies the
[folder-removal rules](task-planning-text.md#edits-on-an-enrolled-destination):
an omitted eligible member is cancelled by a local event and its id is retired
from the exchange permanently (`id_collision` if reintroduced). A partial
checkout or incomplete input whose remaining graph is valid can therefore
cause irreversible cancellation. Review an import with `--dry-run` before
applying it. References resolve against the complete resulting task
graph, and dependency/parent cycles are refused. Unknown fields/versions,
duplicate or mismatched ids and unresolved conflict markers are refused.

## Project memory text exchange

`task-sync export MAIN/.engram-data/.engram --include-memories` selects the
`engram-project-memories-v1` capability independently of `--include-notes`.
The folder project's `memory_profile` field advertises it; older readers refuse
the unknown field, and current task-only imports require explicit memory opt-in.
Keep this host's exported task `project.json` as its pre-merge basis as usual.
Export captures task and memory text from the same SQLite snapshot and withholds
a whole key when one of its historical task targets is outside the selected task
profile. After the ordinary fetch and agent merge of the data branch, use
`task-sync import MAIN/.engram-data/.engram --basis BASIS/project.json
--include-memories`. Tasks apply before their dependent memories. This route
does not fetch, merge, commit or push. Git authority remains separate.

Each key has `.engram/memories/KEY/head.json` and immutable `ID.json` revision
files. The head selects one ordered predecessor chain, or an attributed
tombstone with no revision bodies. The selected tree contains no unreferenced
revision files. A revision's UUID is its authoring native version's minted
ObjectId; historical 64-hex stored ObjectIds travel unchanged too. Imported
native versions retain that shared identity in recognized
provenance and receive fresh native ids. Shared revision numbers are chain
positions; local numbers may differ after a merge or seed. Identities are never
derived from body hashes. Existing file bytes cannot change under the same UUID.

Only Internal active histories are admitted initially. A Restricted, SecretRef,
Public or mixed local history is withheld as a whole key with a reason, even
when a caller asks for wider disclosure. A withheld key does not erase a
previous selection. Historical bodies and reported attribution are included
within the ordinary body and actor bounds, the 1 MiB per-file bound and the
128 MiB memory-tree bound. Tombstone metadata needs disclosure too. The
development redactor is a labelled no-op, with no compliance assurance.

The same whole-key disclosure check applies to tombstones. Windows device
names (including names with extensions), and keys ending in a dot or space,
are withheld visibly at export and refused by shared readers.

An export into an existing data checkout takes its memory basis from that
checkout's committed HEAD. Task re-export updates task files in place and carries
the committed memory component unchanged. Missing or modified memory components
refuse with `memory_export_component_missing` or
`memory_export_component_changed`; restore or commit the intended component
before retrying. Recreating the folder never turns an existing committed history
into an empty bootstrap. Only an unborn data branch has an empty bootstrap basis.
An older incoming selection that is a prefix of local history is a no-op,
including after the local host has forgotten that key.

Export checks the effective ignore policy through bounded `git check-ignore`.
An existing committed blanket rule that excludes memory paths refuses with
`memory_ignore_policy_excludes_tree`. Update the managed block in the data
checkout's `.engram/.gitignore` explicitly, before its END marker, with:

```text
!/memories/
/memories/*
!/memories/*/
/memories/*/*
!/memories/*/head.json
```

Then add two revision allow rules of the form
`!/memories/*/PATTERN.json`: replace PATTERN with `[0-9a-f]` repeated exactly 32
times for one rule and 64 times for the other. Retain the existing task/evidence
rules and unrelated operator text. Under the operator's separate data-commit
authority, stage and commit that policy file in the data worktree. An uncommitted
policy change refuses with `memory_ignore_policy_uncommitted`. Export does not
edit policy or commit it. Already prepared setup intents retain their exact
recorded edits; this update applies to newly prepared policy blocks.

Ordinary prefix import appends missing revisions in order through the local
remember writer. The actual importer remains the local recorder; original
author, model context, time and reason are asserted source metadata. Receipt
session/time never become new shared edits on relay. The newest matching local
native id or imported shared-id tag is the anchor; export appends only later
authored local versions and retains the selected shared prefix. An unchanged
relay writes no native revision. Foreign times do not control local timestamps;
the ordinary refusal of a backward local clock remains.

Concurrent heads require an explicit agent merge. The chosen chain is the common
shared prefix plus one freshly authored merged result, retaining both original
branches in Git and their authoring stores. Independent same-key creation has
an empty common prefix. A structurally clean Git union is still refused when
it is not one predecessor chain. `--memory-resolution FILE` supplies per-key
`ImportOptions` JSON: compared `common`, `local` and `incoming` histories under
`merge`, or an exact `seed` manifest. The selected result and current local input
are checked against that evidence; a stale input requires a new agent merge.
On the authoring host, record the chosen result as an ordinary local revise,
then export with `--include-memories --memory-resolution FILE`.
Use `--expected-revision` for that revise so an identical chosen body still
records the intended new merge revision. The export's compared
inputs bind the local history before that newly authored head. Export proceeds
only while the current selection equals one of the exact compared inputs or
the identical already-published result for a retry; a newer incoming suffix
requires another agent merge. Export writes
the fresh native version UUID with the common-prefix predecessor on its first
materialization, rather than first publishing a divergent predecessor and later
rewriting immutable bytes. Receiving hosts select their own local compared
input in the import resolution.

Every selected historical retirement target must resolve to the exact local
task UUID/ref, or the affected key is held. Import translates the resulting
target into local Set/Clear/Keep while retaining the source clear marker.
Completing a target never automatically forgets a memory. Known and unknown
tombstones reserve the key permanently, and an active import/export cannot
replace a learned tombstone. Unknown keys use the existing body-free terminal
shape. SQLite history, FTS and schema remain unchanged.

For a first host seed, create the memory with its source tag already present:

```powershell
engram work remember --key $memoryKey --text $memoryBody --import-provenance $memoryTag
```

`$memoryTag` is `engram:project-memory-import:v1:` followed by compact JSON with
`format`, `project_id`, `key`, `revision_id`, `previous`, `body_sha256`,
`retiring_target`, `source_clear`, and `source`. `source` contains `actor_id`,
`actor_kind`, `actor_context`, `reason` and original RFC3339 `remembered_at`.
The complete marked string is at most 4096 UTF-8 bytes. The revision id and
predecessor come from Central's selected shared chain; the hash confirms the
exact UTF-8 body. When a target is present, supply ordinary `--retires-with`
too. `--actor-context` continues to describe the local recorder.

An explicit seed manifest binds the project/key, exact seeded local version id,
recorded Central shared revision id and body SHA-256. Rebaseline validates that
record, its flat tag and selected chain content, then appends only the shared
suffix. Earlier shared revisions remain in Git. An older untagged seed may gain
one ordinary same-body tagged revision only with operator-selected
`--mark-untagged-seeds` and its manifest. No automatic fallback, body banner,
old-record rewrite or private mapping checkpoint is used.

Task application and memory checks share one transaction, with a savepoint for
each key. `--dry-run` leaves newly imported task targets visible to memory
admission until all checks finish, then rolls tasks, memories, baselines, receipts
and feed entries back together. A refused key rolls back all of its revisions
while other admitted keys can apply.

New immutable revision files are staged, synced and linked into place without
replacement; heads are replaced atomically only after revision files exist.
If export is interrupted, normal import/export refuses incomplete or pending
files. Retain the complete pre-export `.engram` folder, then explicitly run the
same export with `--include-memories --memory-recovery-basis RETAINED/.engram`.
Recovery derives the intended output again from canonical local history and
that selected basis. It checks the whole directory before cleaning only matching
pending files or a torn newly created revision. Every current head must match
the retained or intended selection. Unknown files, stale heads, missing retained
keys and altered previously selected immutable bytes refuse without cleanup.
The explicit recovery also handles a partial `project-UUID.pending` capability
file: the current envelope must equal the retained envelope or its intended
memory-capable form, and staging bytes must prefix the intended envelope.
Other root files still refuse.
There is no private mapping/checkpoint, body banner or rewrite of stored history.
An unchanged recovery retry is idempotent. The combined task/memory text remains
bounded by 128 MiB, with the existing per-file limits.
Recovery streams at most the bounded union of the retained and intended trees
plus one staged revision and head; these transient bytes are never an admitted
shared tree.

Memory receipts report per-key application/refusal; a partial import returns
exit 3 and must not be treated as a fully consumed incoming basis. Exact repeats
append nothing. Claims, live runs,
grants, session registrations, private scratch, cursors, context generations,
credentials and host verification credit never travel. Reported rules remain
attributed notes. The ledger/delta step surface does not yet account for memory
coverage; use the explicit folder route above and retain its memory receipt.

## Imported lifecycle and local execution

Shared task status describes planning and reported outcomes. Native execution
records remain local evidence, never editable task-document fields. Apply the
following mapping after the agent resolves conflicts and core admission passes.
Unlisted incompatible combinations refuse or enter explicit reconciliation;
they never fall through the compatible-completion case:

| Local situation / incoming final state | Import result |
| --- | --- |
| No existing task / open | Create the planning identity; readiness follows local graph and policy. No claim, session or grant is created. |
| No existing task or only imported history / completed | Retain an attributed inert completion record, display the task as reported completed, exclude it from ready work, and allow its completed planning status to satisfy an ordinary prerequisite. It conveys no native verification credit. |
| Existing native unfinished run / terminal state or incompatible governing basis | Require explicit local import reconciliation: hold new affected-run admission, drain/checkpoint in-flight activity, release the claim with an attributed reason, and append a superseded-by-import disposition retaining the run's evidence. Then apply reported planning state without fabricating successful native completion. If safe reconciliation cannot finish, retain the candidate and refuse this attempt; the state is not permanently unimportable. Independent shared notes may proceed if they do not change admission basis. |
| Existing native completed run / same completed outcome | Preserve the immutable seal and its exact original basis; record compatible shared changes as later attributed versions, never revise the seal or claim that it covers those later changes. |
| Existing completed task / reopened final state | Require an explicit locally admitted reopen with a new execution generation before applying the resolved open state. Preserve old completion and evidence as history, without crediting the new generation. |
| Cancelled/superseded final state | Keep the task document and immutable history, reconcile any unfinished local run as above, and enforce explicit required-child disposal/waiver and remaining graph-reference rules. File removal is never a waiver. |
| Missing document / first import into an empty, unenrolled store | There is no prior exchange inventory or existing task to dispose. Missing referenced tasks refuse; an omitted isolated task is undetectable. On an enrolled destination, omission instead follows the [folder-removal rules](task-planning-text.md#edits-on-an-enrolled-destination), including permanent id retirement; inspect `--dry-run` before applying partial input. |

For required-child accounting, use the existing explicit distinction between
native seals and inert restored completions: an imported reported completion
can be recorded only as `restored_child_completions`, never as a native child
seal; the existing report-assembly restriction on such ancestry remains. It
cannot satisfy a typed host-verification obligation. Cancelled/superseded
children still require the ordinary explicit disposal/waiver rules.
The superseded-by-import run disposition is a new admitted local transition to
implement and audit, not permission to silently release a claim through current
generic import code. No global host drain or remote ownership transfer is needed.
Dependent evaluations can become stale after an admitted change; hosts must
expect the normal basis-moved/refusal result rather than credit old evidence.

Imported outcome attribution is stable shared data. Local receipt timestamps,
newly minted local import-event ids, machine-local run ids and derived display
labels never rewrite the shared document during re-export. Re-export of an
unchanged imported tree must be identical, including reported completion and
cancelled/superseded status. Concurrent disagreement about terminal/open state is given to the
agent; the importer applies the chosen result only through the above rules.

## Remote-agent collaboration through TermAl

The selected workflow supports a cooperating remote agent with an independently
writable Engram store on each host. The remote agent claims locally, exports
activity and task changes, and pushes the data branch. The local peer
fetches/pulls, merges code and task text, and imports the resolved tree in its
data worktree. Remote activity becomes visible through explicit Git exchange.
Fetch alone does not mean import succeeded; the ImportReceipt stays local.

The [cross-host design](cross-host-task-sync.md) separates this Git workflow from
TermAl coordination. One explicitly configured central TermAl host per
coordination domain owns session coordination and all mailbox authority.
Each satellite host runs native sessions through the central host's API over
OpenSSH. The central TermAl host is TermAl's hub for sessions and mailboxes;
satellite hosts are spokes.
Each host's Engram remains
independently writable. TermAl mailboxes carry ordinary coordination only; they
carry no task-sync protocol. There are no typed publication hints, application
responses or Engram mailbox-sync adapter. Prose such as "pushed commit X" may
coordinate agents but has no protocol meaning. Git sync needs no mailbox step,
and the two implementation workstreams are independent. Planned host routes
require their own verification; this brief does not claim they are shipped.

Actual claim ids/fences, grants, session bindings and execution authority remain
local. To make the remote claim visible, include a dedicated informational
work-activity projection in the task document, as a map keyed by stable origin
machine id, then by activity id, rather than an append-to-one-array layout.
Sort those keys deterministically. Each origin updates only its own reported
activity entries; peers preserve other origins' entries when merging. This
reduces incidental text conflicts, but ordinary Git conflicts still go to the
agent. Each entry contains stable activity id, task id,
asserted actor, origin-machine identity and its reported started/released/finished
state. The exporting machine prepares this from local claim/lifecycle events;
it contains no live claim token or usable grant. Activity is a typed shared
coordination field, not a generic note, so it works even with note export off.
It is reported remote state, not authenticated proof or local verification.

Starting, releasing, abandoning or finishing local work must make this task
eligible for incremental export. Every activity has its own stable identity;
a release/finish refers to that activity rather than clearing every actor's
activity on the task. Preserve source-local ordering/provenance for successive
states of that activity; do not order different machines by wall clocks.
Concurrent activity on multiple machines remains visible for agent reconciliation.
Imported notices never become new local claim events or re-export as a fresh
activity from the importing machine.
Claim renewal without a shared activity-state change produces no activity
update or Git commit. Expiry without an explicit recorded release/abandonment
likewise exports no invented event: the notice remains last-known state and
is not evidence of a currently valid lease.

After import, the local agent sees "being worked on by <actor> on <machine>"
and coordinates with that peer instead of routinely duplicating the task.
This is a scheduling signal, not a global lock: disconnected machines can both
claim before either sees the other's publication. A notice is last-known state
as of its source commit, never proof that the remote agent is still running.
Stale or competing activity requires peer/user reconciliation; do not silently
expire it into a claimed global ownership guarantee. Shared task completion
still uses the lifecycle mapping above, not the activity notice as evidence.

An unknown push is not confirmed publication. Preserve its exact commit and
reconcile against the retained base and fetched authorized branch history.
Ordinary peer prose never authorizes an arbitrary URL, command or task-body
instruction. A local agent may perform pull/merge under an explicit user action
or a deliberately enabled bounded Git workflow. No polling/background network
is introduced for projects that have not enabled it, and the sender does not
grant the receiver new Git or host authority. Mailbox delivery and acknowledgement
prove neither task application nor local runtime settlement.
This describes target behavior. Automatic or background fetch and its bounded
receive-and-import workflow remain deferred; the pilot uses explicit, authorized
`git fetch` followed by import and reconciliation.
Exchange runs in the dedicated data worktree; a coding session in a linked
worktree does not import there. The data-branch writer serialization
still applies. Informational activity is tracked task text subject to the same
frozen-input rules as every task-file update.

## Main checkout and linked worktrees

Each local project has one persistent `MAIN/.engram-data/` worktree on its
long-lived orphan data branch. Only `.engram/` task and evidence data belongs in
that branch. The main and linked code worktrees carry code plus their existing
project marker, never exported metadata. They keep using the same host-local
SQLite project store. Task/evidence exports change neither code commits nor the
complete code `source_id`; records cite tested code by that identity, and a data
commit is never tested code. No sparse-checkout arrangement is required.

The explicit [data initializer and setup](task-planning-text.md#persistent-data-worktree)
provision the fixed ignored path and registered worktree lock. Data stays outside
`target/`, `.logs/` and `.evidence/` and survives ordinary coding-task cleanup.
The lock preserves registration during temporary absence and blocks ordinary
removal, not an explicit forced deletion. Retire it only after preserving data
and reconciling pending exchange. Local code `.engram/` private state is never
published or rewritten by data setup.

Hosts fetch the bound data ref, use Engram's merge/reducer and create a new
fast-forward data commit on top of the fetched tip. No Git merge, rebase, forced
push or history rewrite is part of this exchange. Data pushes require their
explicit Git authority; local initialization and preparation grant none.
Retain one branch per project; epoch rollover or history squash is a separate
retention decision. Project/store progress binds the selected data history;
changing a code branch never replaces the database or rolls sync backward.
Complete historical evidence packs use the same data carrier, with their
separate disclosure and local reuse rules; they transfer no live authority.

## Finding local changes

Keep durable local sync progress bound to project/store identity, repository
and branch. It records independently:

- the named project-feed position exported through, and its recoverable local
  Git commit;
- the Git commit whose task tree was last successfully imported;
- the retained common task-tree base used for three-way reconciliation;
- any prepared operation needed to retry without losing local changes.

These are local bookkeeping, not shared Git payload. A feed cursor is valid
only in its named local store/feed; it is never compared to another machine's
cursor. Replacing/restoring the store requires explicit re-baselining when
that identity or cursor basis cannot be established.

The existing project feed is the starting point for change collection. Read
the bounded interval after the export cursor through a captured head, collect
affected task ids, deduplicate, and serialize their final state from that same
SQLite read snapshot. Twenty edits to one task produce one task-file update.
Use structured event-to-task mapping, not note-text searches or wall-clock
timestamps. Every exported field, eligible note, activity and relation must participate
in change coverage transactionally. Implementation must prove that coverage;
missing families require extending the change index before shipping, not an
assumption that every current feed entry already provides it. An unavailable
cursor interval requires an explicit full comparison/re-baseline.

Imported changes may appear on the local feed, but an identical serialized
task tree produces no new Git commit. An import must not advance an export
cursor past unrelated, unexported local changes. Unchanged output, including
excluded runtime-only events, may advance progress only with a durable mapping
to the already committed task tree.

## Initial push: a project with 1,000 tasks

For a configured empty remote branch:

1. Capture all 1,000 tasks and the project-feed head in one SQLite read
   snapshot. Include terminal tasks, opted-in eligible notes and graph links
   in the selected profile; report anything outside that profile.
2. Produce `project.json` and the task documents deterministically in the
   dedicated data worktree. Capture its data branch, Git task tree, task-path index/worktree state
   and recorded reconciliation basis. Preserve and reconcile every unconsumed
   task-tree change, including clean committed changes not yet imported as well
   as staged/unstaged edits. Validate the entire exported graph.
3. Create one data-branch Git commit with only the initial `.engram/` tree.
   The code checkout's branch, index and worktree remain unchanged. No agent
   needs to read 1,000 tasks to export them.
4. Record that the captured local feed position is represented by this
   recoverable commit. If interrupted between commit creation and bookkeeping,
   recovery verifies the prepared operation before advancing the cursor.
5. Push that commit. A failed or unknown push keeps the commit and retries or
   checks the remote; it does not discard exported data or assume publication.
   New SQLite writes after the read snapshot stay pending for the next export.

If the remote already has a task tree, first fetch and reconcile it. Initial
push never replaces it by force. Git history starts with the chosen current
task projection; this does not replay every historic local event into Git.

The second machine fetches the initial commit, validates the full task tree
and imports the 1,000 task identities in a transaction, then records the
imported commit. Existing local tasks require reconciliation rather than
replacement. An empty destination gets no copied live execution authority.

## Incremental push, pull and agent merge

For push, collect changed local task ids since the export cursor and update
only their documents after reconciling the current Git task tree with the
recorded base and SQLite snapshot. A clean Git status does not establish that
the committed task tree was imported: never overwrite a committed title change
with a stale database title while exporting a separate priority change. Bind
the outgoing candidate/receipt to the captured task tree and task-path
index/worktree state; recheck them and branch identity before publication of
that candidate. A changed basis is preserved and reconciled, not overwritten.
Unrelated code edits need not invalidate a task-only candidate. Retain a durable
commit before advancing local export progress. Git handles transfer of commits
and ordinary non-fast-forward
refusal. A rejected push fetches and reconciles the competing commits; this
workflow does not require force-push or remote writer ownership.

For pull, fetch and inspect committed task-tree differences against the last
imported commit. Diff the old and new trees, not just the final commit's patch:
this includes all intervening commits and merge commits. A fetched commit is
not an imported commit. The main checkout preserves any pending task-file and
SQLite edits before Git integration. Export pending local task changes to a
recoverable candidate before resolving overlaps, or capture equivalent local
text and its exact read basis without advancing export progress.

If the imported commit is not an ancestor of the incoming tip, do not assume
that changed ancestry authorizes replacement. Reconcile using the retained
common task tree and both current sides. If that base is unavailable, require
an explicit full re-baseline with all local state preserved; never guess a
base or force-push. The successful receipt records the newly resolved commit.

Provide the agent with base, local and incoming documents for overlapping
changes. Distinct ids and one-sided changes can be carried forward mechanically;
the agent resolves meaning, including terminal/open and graph conflicts, and
writes final task documents alongside its code merge. It can inspect any
affected dependencies, not just the files with textual conflict markers.
Engram validates the resulting graph and applies the selected final state.
The resolved task tree must be committed; import progress names exactly that
commit, including a locally created merge commit, not an arbitrary fetched tip.

Apply import and advance its local receipt/cursor atomically. Match each
affected task's captured local basis under the write transaction; if a local
agent changed that basis while the merge was prepared, preserve the candidate
and require renewed reconciliation. In that same write transaction revalidate
the complete resulting relevant graph, target lifecycles and live-run admission,
including the union of parent/required-child and prerequisite edges. An unchanged
A row is insufficient if candidate A→B races with local B→A; connected tasks
are not unrelated merely because they are absent from the imported delta.
Equivalent fencing must cover the full relevant read set. Truly unrelated
local changes need not block the batch. Repeating the same committed result
is idempotent. Append attributed
versions/events to existing immutable history and update current projections;
never overwrite canonical objects. No partial graph or premature cursor
advance is visible after refusal or crash. Git publication and SQLite import
are separate durable steps, so interrupted/unknown results are recovered from
their recorded candidate and receipts rather than a fictitious atomic
Git-plus-SQLite transaction.

## Data commits and source landing policy

Task data and evidence use only the dedicated orphan data branch. A data
commit contains only `.engram/`; a changed, indexed or ignored file outside
that directory refuses exchange. The data worktree has its own Git index and
HEAD. Recheck its branch and head immediately before committing/pushing.
Export does not stage code, move a code branch or change a code source freeze.

Generated data exchange should have a project-approved validation path distinct
from source-code delivery: schema/graph validation and reviewed sync tooling,
not a fresh Rust build and code-review pair per 1,000-task export. This is a
policy integration prerequisite, not an exemption granted by this document.
The current Engram authority rules still govern; an unattended data-only
exception requires their separate exact-wording approval route before enabling
it. Without that exception, obey the existing gates/authority or retain the
prepared output for a separately authorized operation. Mixed source/data commits
are refused. No commit/push, force-push or test waiver is authorized
merely by the presence of a sync configuration or this specification.

## Host binding and validation identity

Import is an admitted local planning mutation owned by Engram. An incoming
change to work with an active local claim or unfinished run is deferred, with
its candidate retained and affected scope held until execution ends normally.
That holds for a claim taken after the request's local cut too; only a change
applied now requires the affected native work unchanged since that cut.
Releasing or expiring a claim alone does not finish its run. A planning
successor uses a new operation and key, names the original deferred source
selection, and rechecks the hold and fresh local basis in one SQL transaction.
No TermAl attestation, drain, intent recovery or host settlement is required.
The original request continues to return its original deferred receipt.

A fresh evidence recipe or planning successor fixes its place in the binding's
chain, so it is selected and retained together, under the same origin
reservation every exchange writer holds for its whole effect, after any Git
observation and before any selected bytes are read. While another exchange
writer is in flight it refuses with nothing retained; retry it afterwards. It
never names a predecessor that another writer has since replaced.

Retained evidence recipes, planning successors and delta candidates keep their
original historical project cut and identities across retries. Unrelated local
project events do not invalidate them: admission checks the historical anchor
and the complete selected native readset, together with the current binding,
baseline, ledger tail, origin and restore guards. A change to affected native
planning or execution still refuses admission and preserves the retained bytes.
Retry the exact retained request after an interrupted SQL write; do not remove
or reconstruct its private artifacts. Evidence imports, which only retain inert
originals, need no quiet native work scope. Successful completion or a known
evidence refusal accounts for the retained construction, allowing a different
exchange operation or custody audit to proceed normally. An evidence request
whose selected data-branch head is observed at another commit than it expects
ends with the known `selection_changed` refusal under its retained identities,
whether the head moved before or after its construction was retained, so its
already fetched carrier never holds custody from later exchanges; a new request
then selects the current head. Likewise, a request whose selected immutable
carrier definitely disagrees with its frozen selection, in its commit bytes,
disclosure profile, pack path, declared size, fingerprint, record count or pack
structure, ends with the known `invalid_payload` refusal under its retained
identities, whether it is fresh or an exact retry. Uncertain Git observations,
including an unavailable, interrupted or late read, stay pending for an exact
retry and grant no coverage. An exchange operation with a total deadline, such as reading a
selected evidence carrier or recording a delta commit or publication, spends
that one deadline on its data-worktree observations too, rather than giving
each of their local Git queries a fresh fixed bound.

A new incoming request whose planning scope overlaps an active reconciliation
hold is refused with `task_sync_reconciliation_held` before any candidate is
retained. That scope is the affected closure the deferral path computes from
the changed tasks and the targets of their edges. It could neither apply nor defer until that hold
resolves, and its retained bytes would otherwise stand in the way of the
hold's own planning successor. Once the hold resolves, the request is retained
normally. A carrier already fetched for the refused request is not removed;
like any fetched carrier unknown to the ledger, it holds custody until its
explicit audited abandonment.

A planning successor applies only its checked planning change on the fresh
core basis; it does not settle, rebind, rewrite or transfer existing host
requests or runtime evidence. Those retain their original identity and follow
the existing operation-specific replay/validation rules. Ordinary context
refresh remains the visibility mechanism, not an atomic host acknowledgment
of the import. Queued user prompts remain queued until actual delivery;
import cannot clear or rewrite them or claim they were delivered. No automatic
host Stop is authorized. The checked successor writer applies retained native
reopen, revision, prerequisite and blocker transitions in one transaction after
affected execution ends normally. Task creation/removal, parent or required-child
changes, and note/activity changes remain explicit refusals on this successor
path; other planning import paths retain their own contracts.
Reopening a restored completed child can initialize its first local execution
aggregate even when its restored root has never executed. The retained recipe
reserves that aggregate's identity under the actual root, once for all selected
children of that root; reopening the root and child together shares it. An
existing active aggregate is reused. Ordinary native child reopening still
requires an active root execution. Retained recipes and historical identities
are read unchanged; an incomplete older recipe refuses rather than minting
missing identities during retry.
Held/read responses name the source commit, changed planning families, affected
items/sessions and when the new basis applies. Refusals name the unfinished local
execution or changed basis. Imported notes never mint
host verification or source-observation credit or rebind a named source root;
an explicit local claim release still has its ordinary root-lifetime effect.

Preserve existing code validation identities. The entire `.engram-data/`
directory is ignored by the code checkout before initialization, and has its
own registered Git administration. Exported files therefore never enter the
code checkout's complete `source_id`. This does not create an exclusion for
tracked task files on a code branch, or change how code identity is computed.
Materialized tracked task-file changes affect that code `source_id`, and so
does a HEAD-only change, because `source_id` names the commit; the current
review freeze also includes HEAD and therefore changes.
Frozen detached source roots remain unchanged until explicit integration.
Imported planning and evaluation freshness is checked separately even when
the code source remains unchanged; a data commit is never evidence of tested code.

For TermAl, gated code roots stay in separate detached worktrees. Serialize
updates of the data branch in its own worktree. Code operations retain their
ordinary source invalidation rules. This is a local data writer boundary,
not a global host pause or remote single-writer protocol.

## Explicit exchange command steps

`engram task-sync step --request @selection.json` invokes one shared service
operation outside the fourteen work words. Each request has an `action` tag;
unknown tags and undeclared fields refuse before the selected owner runs.
The CLI accepts inline JSON too, bounded to 2 MiB, and uses its ordinary actor
and session options. Mutation requires explicit
host-local opt-in, a writable connection and matching project/actor/session.
Historical reads do not enable sync or create a missing store.
`engram task-sync progress --binding ID` reads family capture and coverage
without advancing a cursor. Exchange steps and progress use the CLI/shared
service; MCP retains only the separate `task_sync_settings` configuration tool.

For example, inspect a configured target without contacting it:

```json
{
  "action": "inspect_target",
  "project_file": "/repo/.engram-project",
  "branch": "engram-data",
  "remote": "origin",
  "reference": "refs/heads/engram-data"
}
```

Use the actual project-marker path on each host. The receipt contains the
observed data checkout, Git administration, branch, head and configuration
fingerprint, omitting the URL and credential configuration. `activate` takes
`project_file` and the existing complete `ledger::Binding` as `binding`; its
repository fields must select that project's dedicated data checkout. Keep
that exact body for replay. `read_binding` takes the returned immutable
`binding` id and returns the original canonical binding even when its checkout
is unavailable. This read grants no current Git or execution authority.

The transient request types are defined in `task_sync::driver`; their nested
selection types are the existing ledger and delta formats:

| Action | Explicit selection | Existing owner and result |
| --- | --- | --- |
| `capture_native` | `binding`, `request: NativeEvidenceCaptureRequest` | Freeze selected existing native roots and feed cuts under the binding's disclosure profile; return capture, pack and witness ids. |
| `export_native` | `binding`, `project_file`, `branch`, retained `cursor`, `maximum_packs` | Check the binding's disclosure profile and write one bounded page of exact retained packs into its dedicated data checkout; return file metadata and the next cursor. |
| `qualify_verification`, `qualify_acceptance` | Existing `ApplicabilityRequest` or `AcceptanceUseRequest` under `request` | Retain the existing owner's destination assessment and return its decision id. |
| `verification_summary`, `acceptance_summary` | Original `decision` id | Read bounded eligibility/source summary; no complete code identity or new verdict. |
| `consume_verification`, `consume_acceptance` | `request: {consumption, decision, binding}` using `ConsumptionRequest` | Explicitly admit the prepared local use through the existing native owner; exact retry returns its original ids. |
| `retain_data` | `project_file`, `branch`, operation UUID, exact `commit` OID | Retain an existing data-only commit; return its `RetainedCommit`. |
| `advance_data` | `project_file`, `branch`, `retained`, `expected_head` | Fast-forward only to that operation-owned retained staged tree; repeat observes the same result. |
| `prepare_delta` | `request: {binding, intent: DeltaIntent}` | Retain the selected native export bytes and return the original prepared event. |
| `record_export_commit` | `request: {delta: {binding, intent}, selection: DeltaCommitSelection}` | Bind the retained data commit to the original preparation. |
| `record_publication` | That same commit `request`, plus `progress: OutgoingProgress` | Record a caller-owned start or refusal, or the owner's own read of the selected remote tip; never push. |
| `retain_incoming_delta` | `request: {binding, intent: DeltaIntent}` | Derive and retain the exact incoming payload from the selected retained Git commits; return its unchanged candidate descriptor without applying planning. |
| `apply_delta` | `request: {binding, intent: DeltaIntent}` | Apply the selected incoming planning change, or retain its explicit deferred outcome. |
| `apply_evidence` | `request: {binding, intent: EvidenceIntent}` | Retain selected incoming packs or the original stopped outcome; no verification credit. |
| `apply_successor` | `request: {binding, request: PlanningSuccessorRequest, payload: DeltaCandidatePayload}` | Admit an explicit successor against fresh local state without rewriting the original deferred outcome. |
| `record_resolution` | `request: {original: {delta, selection}, recipe: OutgoingReconciliation}` | Inspect the agent-selected merge and retain its existing canonical resolution. |
| `record_retry_progress` | That same resolution `request`, plus `progress` | Record the one selected retry's start or stop, or the owner's own read of the selected remote tip against the merged commit. |
| `read_delta_progress` | Original `{binding, intent}` under `request` | Return canonical event ids, sequences and transition tags. |
| `completed_delta`, `completed_evidence`, `completed_successor` | The corresponding original application `request` | Read original prepared/outcome ids and bounded meaning, or no recorded outcome. |

For an incoming delta, explicitly retain the selected data commit first, then
call `retain_incoming_delta` before `apply_delta`. The retention step reads the
exact prior-to-incoming Git delta and a bounded complete incoming snapshot to
check membership and supply destination-only changed members. It validates the
declared payload fingerprint and uses the applying writer's origin, local-basis
and custody reservation. It creates no canonical planning event or native credit,
fetches nothing and changes no branch. Exact retry preserves the same intent and
bytes; a different request under that operation refuses. A retained candidate is
not an applied delta: the separate `apply_delta` writer still owns admission and
its historical receipt.

An outgoing sequence is explicit origin enrollment, target inspection and
binding activation, custody audit, selected delta preparation, staging the
chosen data snapshot, `data-commit`, `advance_data`, and
`record_export_commit`. Keep every immutable selection. Publication start and
observation are separate calls to `record_publication`; the caller performs
any authorized fetch or push, and an observation reads the selected remote tip
itself, so it confirms only what the remote actually holds. A newly recorded start is reported only after
its existing SQL owner commits. A replayed start returns `newly_recorded:
false` and `start_recorded: false`; neither grants another dispatch. A receipt
omits `newly_recorded` where its existing owner returns only an event identity,
so an exact repeat is never presented as a new effect.

Evidence publication starts with `capture_native`: retain the minted capture id,
producing origin, selected root ids, exact named feed cuts, disclosure `profile`
and pack-record bound. The profile must match the selected binding. Notes-off
captures refuse a closure containing work memory or narrative proof text,
including verification summaries, evaluator rationales, gate details, commands,
blocker details and source payload prose;
they never remove required originals to make that closure publishable. Native
activity requires an explicit machine alias, and the existing task-label,
author and note-disclosure exclusions apply. Capture and export both check
current task labels. A later restriction can block export without changing the
retained capture. Export requires the same profile and the binding's data checkout.
The same conservative prose and author exclusions cover retained proof records
and their attribution, including nested source payload text. Explicit notes
disclosure does not override path or reference exclusions. Planning titles,
outcomes and criteria remain shared with notes off; structured record links and
source identities retain their identity semantics. Refusal preserves the whole
original closure rather than redacting or reserializing its records.
An exact retry keeps the original pack and witness ids; a changed selection under
that capture id refuses. `export_native` reads whole packs from that immutable
selection and stages their original encoded bytes under `.engram/evidence/` in
the selected data checkout. Keep the cursor until its page succeeds. Exact files
replay; changed or partial files refuse and remain available for inspection.
An I/O interruption may leave only part of a page. Neither step commits, pushes,
advances coverage nor grants receiving-host verification credit. Receipts contain
ids, paths and bounded metadata, never pack bodies or complete code identities.
Stage the chosen data snapshot and use the existing data commit/publication steps
for transport; incoming packs then pass through `apply_evidence` before explicit
qualification and native consumption by their existing owners.

Retain the original verification and origin-cut ids when preparing
`qualify_verification`. Read `verification_summary` before an explicit
`consume_verification`. For an original evaluation or native seal, prepare
`qualify_acceptance` with its historical proof, origin cuts and the previously
prepared verification decisions, then read `acceptance_summary` and explicitly
`consume_acceptance`. Each consumption carries its own minted id and the actual
destination `ControlWorkBinding`. The existing owners recheck source, policy,
claim, named root, generation, closure and terminal conflicts. Retention alone
never creates a decision or a use. A consumption is neither a new execution nor
a new verdict, and frozen original evaluations and seals stay unchanged.

For a known non-fast-forward result, preserve the original native preparation
and fetched failure pin. Resolve the text explicitly, retain the agent-selected
merged data commit and recipe, then call `record_resolution` followed by
`record_retry_progress`. These commands reuse the one-retry reducer and custody
checks. An unknown result remains unresolved; it is not permission to select a
new merge or dispatch again. Read original progress before deciding what to do.
No step merges text, runs network, enables sync, changes Git authorization,
transfers live claims, or treats a data commit as tested code. Native capture
and imported-evidence qualification/consumption delegate to their separate owners.

## Implementation and validation boundary

The explicit commands above delegate to the existing admission and persistence
owners; their presence does not prove schema compatibility. Judge the actual
durable rows and rebuildable projections under the existing cutover rules;
apply full-store migration when the durable store format changes. Do not promise
no migration merely because
the exchanged representation is JSON. No global writer guard, remote epoch,
host release/acquire or TermAl restart is inherent in this selected design.

Required implementation cases cover: initial 1,000-task export/import;
incremental repeated edits and no-op re-export; every field/note/activity/relation
change family; multiple independent offline writers; agent-resolved same-task and
terminal/open conflicts; graph validation and completion provenance; concurrent
local edits during merge; interrupted commit/import/push with exact retry;
non-fast-forward remote changes; clean committed incoming edits followed by
local edits or a crash before import; A→B/B→A concurrent graph changes; every
lifecycle-table case; divergent branch rebinding; and linked
worktree exclusion without staged deletions or lost task-tree changes.
Also cover restricted omission, explicit widening and re-baselining;
`secret-ref` non-dereference; no-op redactor status at enablement and export;
and Windows `core.autocrlf` checkout with no-op re-export. Restricted
required-field omission is refused without clearing the local value;
escaped CR/LF is preserved through the Windows no-op round-trip.
Also cover remote claim → activity export → confirmed push → local fetch/merge/import,
release/finish updates with notes disabled, concurrent remote/local claims,
repeated fetch/import, unknown push, commits outside the configured branch
history, unchanged renewals, source-separated activity maps, and no transfer of
live authority. This Git pilot has no mailbox step or mailbox implementation
dependency; TermAl verifies its ordinary coordination route separately.

The minimum cross-host pilot uses at least three hosts. For example, A is Windows,
B macOS, and C Linux or a second Windows host; the roles are OS-independent.
After initial publish and empty-target imports, C goes offline while A and B make
concurrent disjoint/overlapping edits and reconcile through Git. C also retains
local edits, rejoins, fetches and merges its base/local/incoming task trees, then
publishes/imports the resolved result. A and B fetch/merge/import that result.
Verify shared projection convergence once all edits are reconciled, stable repeat
imports, visible origin-qualified duplicate work, and no foreign authority.
There is no mailbox step. Onboarding each participating Engram host verifies its Engram
binary and automated Git transport/subprocess support on macOS, Linux or Windows.
The current Mac endpoint is the first instance, not a special case. Once platform
support exists, another Engram host joins through configuration and verification.
Authorized manual Git is a bounded pilot route, not proof of automated platform
support. Additional Engram peers join by the same process.
A future capability/capacity cluster scheduler is separate deferred scope.

The acceptance comparison must establish that an initial import followed by
incremental export produces the same task projection, without feedback-loop
commits. Test terminal-task semantics and excluded live authority separately.
These are future implementation proofs, not results of reviewing this design.
