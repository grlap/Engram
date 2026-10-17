# Cross-host task synchronization

> Related: [Git task synchronization](git-task-sync.md),
> [task planning text](task-planning-text.md), [local work](local-work-system.md),
> [acceptance evaluation](acceptance-evaluation.md), and
> [shipped capabilities](../shipped.md).

Status: design wording jointly confirmed by the Engram and TermAl coordinators
on 2026-10-17 for decentralized Engram task sync and central TermAl coordination,
with a host-neutral N-host model and the accepted
[disconnected-work decision (Q6)](#recorded-decision-and-remaining-decisions).
Greg authorized implementation of explicit Git exchange slices within their
reviewed scopes on 2026-10-17. The planned pilot uses explicit, authorized
`git fetch` followed by import and reconciliation. Automatic or background fetch,
including the bounded receive-and-import workflow proposed to support it,
remains deferred by Greg's direction of the same date.
This brief records those decisions and the selected independent-writer boundary;
it grants no authorization. Pilot activation, remote configuration, migration
cutover and each implementation or rollout step retain their separate scope and
authorization requirements. Separately authorized work retains its existing scope.
[Remaining decisions](#recorded-decision-and-remaining-decisions) stay explicit.

## Product model and relationship to the earlier portable design

Engram is decentralized. Ordinary Git merge reconciles code and task-status text;
Engram import validates and applies the resolved task graph on each host. This is
the cross-host task-status reconciliation mechanism. No central Engram claim
service, centralized runtime, global writer or cross-host exclusive claim is
introduced. Informational activity and ordinary peer messages are advisory only.

One explicitly configured central TermAl host per coordination domain owns
session coordination and all mailbox authority. Satellite hosts run native sessions
through the central host's API over OpenSSH. Mailbox storage exists only there, with no replication,
per-mailbox home election or timeout failover. This central TermAl topology does
not centralize Engram: every host retains its independently writable store.
TermAl mailboxes carry ordinary coordination only; they carry no task-sync protocol.
Quoted task text in a message is never task state. Sender outbox/intents serve
retry and queueing only, never mailbox list/read/ACK.

The model supports N independently writable hosts, including Windows, macOS and
Linux. Each host has its own canonical SQLite store. All reconcile through the
same explicitly configured Git remote and ordinary code branch, then local import.
The shared Git remote is a meeting point, not an Engram authority: Engram has no hub.
The central TermAl host is TermAl's hub for sessions and mailboxes;
each satellite host is a spoke. This topology does not make their
Engram stores subordinate or turn Git into a claim service.

Same project/task
UUIDs refer to the same shared planning identities; native execution generations and their
authority remain host-local. Git on the ordinary code branch carries the selected task
projection. External agents merge base/local/incoming text; Engram validates and
transactionally imports the result. TermAl mailboxes carry ordinary coordination only.
Git task sync and the mailbox workstream are independent; neither waits for the other
to implement a task-sync transport or notification adapter.

Retain Greg's selected independent-writer model. The earlier portable design is a different,
deferred exclusive store-handoff product: writer epochs/release/acquire would contradict
offline independent work if imposed on task sync. Reuse lessons on retained candidates,
explicit payloads and crash recovery, not its ownership protocol. Neither enabling a mailbox
nor syncing a task activates portable mode. Backup/migration remain separate and task sync
is not complete-store disaster recovery.

First-class means peers can coordinate, discover updated tasks, merge and report outcomes
through supported routes. It does not promise exactly-once work, globally exclusive claims,
or transferable local policy authority. Disconnected hosts can do duplicate work or external
actions.

## Shared payload and provenance

Keep the reviewed typed allowlist: task UUID, planning fields/acceptance/policy, graph
links, terminal/reopened reported state and outcome, optional eligible immutable shared
notes, origin-qualified informational activity. Same branch and `.engram/project.json` plus
`.engram/tasks/<UUID>.json`; main checkout performs sync, linked worktrees omit task files
under the established sparse-checkout contract.

Claims/fences, grants, sessions, retained prompts, delivery positions, private scratch,
credentials, local paths, native evaluations/verification/seals and runtime tables never
import as local authority. Project memories remain outside the initial profile.

Task IDs identify tasks, not revisions. Local change coverage uses captured project-feed
position PLUS the typed inventory where the current capture needs it; current shipped
prepare scans that inventory. Do not promise delta-only cost or substitute wall clocks.
Remote change coverage compares complete committed task trees with the last
applied/common-base tree, including merge commits.

Preserve original note IDs/body/author/time/origin; editing a note creates a new prepared ID
with explicit reference to the old one. Keep import event metadata outside shared
serialization so no echo commits appear.

Identity boundary: Engram-owned stable origin/store-instance identity, distinct from its
display alias and from process boot; it also works for CLI-only Engram. Origin identity
and project/Git binding are Engram-internal. Explicit local configuration may record
which TermAl host an origin runs on for display; it grants no authority and is not
a required task-sync wire mapping. A payload cannot enroll its own binding.
Ordinary restart preserves origin, historical authorship, native session creation
identity and deduplication scope; boot
freshness is not a new task identity. Restore/clone or broken continuity quarantines old
pending operations for explicit reconciliation and prevents reused local IDs from aliasing
prior identities/effects. New installations/clones do not silently adopt an origin's write
authority. Revocation preserves historical attribution and uncertain effects; it is not
erasure. Do not silently reinterpret today's asserted --origin-machine ALIAS as
authenticated identity or change its wire schema. Exact alias mapping, origin/store
cardinality, continuity semantics and format/migration treatment must settle
before implementation; collisions cannot overwrite another origin's activity. Task edits may
originate at any host; activity updates may advance only their origin's episode,
preserving other origins.

Transport authentication establishes a route/peer to the assurance actually provided;
asserted content authorship stays asserted unless separately verified. Record original
source attribution plus importing local actor/source commit without laundering either into
native evidence.

### Joining another host

The minimal per-host configuration is its own Engram origin/store identity, the
stable project binding, the authorized Git remote and branch, and explicit local
task-sync opt-in with the selected payload profile. Git access uses that host's
authorized credentials; task text cannot provide enrollment or permission.
Display association with a TermAl host is optional and is not required to sync.
Exact configuration/API syntax belongs to the implementation; this is the required
configuration contract, not a claim that new commands are shipped.

A new host fetches the published branch in its main checkout, validates project
identity and the supported task profile, and imports published planning into an
empty target store. It preserves shared task/note IDs and their provenance,
retains its own origin identity, and atomically records the import receipt and
applied commit/tree as the local comparison basis. Existing local tasks require
reconciliation instead of empty-target replacement. No live claims, grants,
runtime authority or private scratch arrive with the first import. Subsequent
changes use the same retained-base merge/import process as on every other host.

Evidence references: current notes exclude host-verification, gate/evaluation and
machine-path records. This design does not widen that allowlist. Ordinary narrative reports
may carry attributed result claims and project/origin-qualified opaque evidence references;
a remote locator does not guarantee retrieval or authorize a remote filesystem read.
Structured result-packet transfer and a shared evidence-reference profile are explicitly
deferred, unimplemented optional scope. Either requires a separately accepted typed
disclosure, size/bounds, artifact inventory, access and authority contract before
activation; this design authorizes neither. A possible later packet would name origin,
project/task, source commit/freeze, command/platform/toolchain and selected artifacts
without importing native credit. Unavailable references remain explicitly unavailable. No
automatic blob, private-path or full-log publication to Git.

## Authority and completion

Local claim remains an ordinary local claim. Published started/released/finished activity is
last-known coordination, not a transferable claim token or proof of liveness. Sender cannot
grant receiver Git/host privileges by a message. Explicit per-project configuration fixes
allowed repository/branch/readership/peers; task text and messages are data, not executable
instructions.

Reported completion can exclude an inert imported task from ready work and satisfy ordinary
planning prerequisites under the existing lifecycle contract. It cannot satisfy a typed
host-observed criterion. Required-child accounting preserves restored/inert completion vs
native child seal and report-assembly restrictions.

If multiple hosts complete, preserve each native seal and its exact basis locally. Shared
text chooses the reported outcome with attributed source history; do not manufacture a
combined seal. Same reported status with different outcome/acceptance/source basis is a
semantic conflict, not automatically compatible. Reopening is explicit local admission
creating a fresh execution generation with no automatic claim. Neither an imported
completion nor a mailbox report imports report-publication authority or sends a report:
publication remains a separate approved immutable payload/target/idempotency intent.

Import into an existing native run follows the approved existing-run import design
(model A), separately from the additive exchange-progress ledger. Its records are:

- `SharedImportVersion`: an immutable changed-task shared baseline and origin.
- `ImportReceipt`: an immutable receipt bound to the exact operation key,
  candidate, binding, base, resolved commit/tree, readset and result.
- `ReconciliationHold`: an immutable candidate-bound hold denying new affected
  claim, handoff, evaluation and admission, while permitting settlement,
  checkpoint, evidence and holder-only release.
- `SupersededByImport`: an audited non-success retirement retaining the old run's
  evidence, never a completion seal.

Imported-baseline heads and holds are durable. Preserve model A's full-store
migration requirement and separate cutover decision. A completed native seal is
immutable; incompatible subsequent planning is a later version/reconciliation,
not rewritten proof.

### Local host prerequisites

The following labels name design prerequisites, not shipped host APIs:

- **G1, host-proven responsibility and settlement:** a persisted,
  candidate/hold/run-bound inventory and attestation of affected local host
  execution, requests, checks and observations, renewed at final apply. Missing
  or unreachable responsibility information remains pending; an empty Engram
  query does not prove no responsibility.
- **G2, hold-aware admission:** an enforced durable fence denying new affected
  execution across queued admission, explicit prompts, boot reconstruction,
  retry workers and late dispatch, while retaining queued requests.
- **G3, original-operation recovery without first execution:** TermAl's half
  retains the exact uncertain Evaluate/Begin intent and fences dispatch/recovery.
  Engram's half serializes original application against retirement and outcome
  recovery; every first-application path checks the authoritative terminal result.
  A positively unsent request may be retired by its exact owner under the fence.
  A possibly sent request requires serialized authoritative terminal settlement;
  a missing outcome alone remains pending and cannot justify deletion or replay
  that first executes work.

Both G3 halves are required. Existing `turn_grant_read` alone cannot resolve a
lost Evaluate without a grant id or prevent late first application. A fresh ready
acknowledgement does not guarantee continuing process liveness: persistent
fencing must survive crashes, orphan processes and late dispatch.

G1/G2/G3 protect admission and settlement of affected local execution; they do not
elect a cross-host writer, synchronize claims or require unrelated remote peers
to surrender independently held work. A local admission hold is not a distributed
task lock.

Stage 1 is the model-A delivery limited to
[independently admissible planning](#admissible-divergent-import).
Its refusal is candidate/scope-local: affected native-run changes requiring TermAl
responsibility/settlement stay retained/refused. Missing positive knowledge of complete
responsibility is pending, never proof of no responsible host. Unchanged compatible
planning, independent notes and disjoint work remain admissible only where their own local
basis permits. Stage 2 is the [active-reconciliation delivery](#active-reconciliation).
It requires G1/G2
plus both G3 halves: hold -> settle exact original
requests under persistent fencing -> holders release their own claims -> settle
release-induced cleanup -> fresh scope/readset barrier -> atomic apply. Missing Begin
outcome, elapsed time, crash, a mailbox ACK or “peer ready” do not prove settlement.
Ordinary remote coordination is independent of this authority-sensitive protocol.

## Exchange, conflict and recovery

Local capture retains immutable operation/candidate, profile/binding, local snapshot basis
and common task tree. Preserve dirty files/index AND clean committed task edits not imported
yet. Never overwrite committed remote title changes from stale SQLite while exporting
another field.

Fetch only an already authorized configured target. Prepare base/local/incoming documents
and affected graph closure; mechanical one-sided merges are allowed, semantic overlaps go to
the agent. Validate duplicate IDs/altered note bodies, unsupported schema,
omissions/removals, activity regression, graph cycles and lifecycle compatibility. Missing
task files are not deletion; cancellation/supersession remain explicit documents.

Resolved bytes are committed under existing Git authority; import names that exact resolved
commit/tree. In one SQLite write transaction compare the full relevant readset, including
newly connected graph/lifecycle races, apply versions and receipt together. If local edits
raced, retain candidate and reconcile; unrelated tasks need not block. There is no atomic
Git+SQLite transaction.

Keep exact candidate/commit across unknown push; inspect remote ancestry before declaring
publication or retrying. Non-fast-forward creates another reconciliation, never force-push.
Prepared/exported/published/fetched/applied are different states; show each honestly.

Restart repeats the same operation/intention, not current recapture. Preserve the exchange's
identity-stable import plan outside SQLite rollback domain before applying; retries reuse
every record ID/body/time/link. Known restored/rewound/divergent store requires explicit
reconciliation; incomplete continuity never justifies reminting an allegedly identical
import. Lost host permits a new host to import published planning, not reconstruct
unpublished work/evidence/authority. Returning old host keeps its pending edits, reconciles
against retained common task trees without Git history rewrite; no claim takeover is
implied.

Lost base/rewritten branch/profile change => explicit full comparison/rebaseline with both
sides retained. Partial candidate/receipt refuses atomically. Faults never silently delete
unknown local content.

## Separate TermAl coordination boundary

TermAl mailboxes carry ordinary coordination only; they carry no task-sync protocol.
One explicitly configured central TermAl host per coordination domain owns session
coordination and every mailbox. Satellite hosts run native sessions through the
central host's API over OpenSSH. Non-home hosts
do not serve mailbox reads, acknowledgements or cursors from local replicas or
offline caches. No per-mailbox home election or timeout failover is introduced.

TermAl owns mailbox addressing, authentication, delivery, retry/deduplication,
wake eligibility and ordinary conversations. Its append, wake and processed-page
receipts describe mailbox behavior only. Engram owns task projection, Git binding,
local admission, exchange progress and its atomic local ImportReceipt.

There are no typed task-publication hints, application responses, sync
notification/response obligations or Engram mailbox-sync adapter. Ordinary prose
such as "pushed commit X" may coordinate agents, but has no task-sync protocol
meaning, does not trigger an automatic import and does not acknowledge task
application. Mailbox acknowledgement never settles G1/G2/G3 or grants authority.

Engram's ImportReceipt stays local and records the exact applied candidate and
resolved commit/tree. Publication and import recover from their own durable
intents/receipts without any mailbox step. Git ancestry establishes history, not
semantic application: a merge may reject or revert an ancestor's change. The
local resolved basis records that disposition rather than declaring every
reachable ancestor applied. Unknown push retains the exact commit; restart after
atomic import uses its durable receipt instead of applying the operation twice.
No clock ordering, mailbox availability or mailbox receipt substitutes for these
local facts.

The mailbox and Git-sync epics are decoupled. Engram origin identity is internal;
an optional display association with a TermAl host is not transport authentication
or a sync dependency. Host/store binding and G1/G2/G3 still protect the affected
local runtime where applicable. They do not create central Engram ownership.

## Stages, reuse and proof

### Shared design and identity

Agree Engram-internal origin identity and store continuity before the relevant
implementation. TermAl topology is a separate coordination concern. The origin
wire format and alias conversion require explicit versioning and durable-format judgment;
this brief changes no frozen interchange contract.

### Retained exchange

Reuse the existing exchange slices: durable progress and atomic initial-import
receipts; immutable capture and fetch; external-agent merge preparation and
admission; authorized exact-commit publication and recovery.

Proof: initial 1,000-task export/import, no-op and echo stability, repeated
edits serialized once per task, local changes after capture retained, crashes
at every boundary, changed-intent refusal, unknown push and non-fast-forward
recovery. The progress slice alone does not deliver continuous import into
native or nonempty graphs.

### Admissible divergent import

Reuse the approved model-A sequence: durable versions/receipts/holds, named
and counted full-store conversion, structured responsibility refusal, atomic
admissible-subset apply, and integration proof. Preserve the separate migration
cutover decision. This stage does not close active-reconciliation acceptance.

### Minimal three-or-more-host Git synchronization pilot

The proof uses at least three independently writable hosts; no role depends on
an OS or a fixed pair. One example is A on Windows, B on macOS, and C on Linux
or a second Windows machine. Further hosts join by the same configuration and
initial-import process, without topology or schema special cases.

1. Configure each host's own origin, project/Git binding and opt-in. Publish an
   initial planning tree, then perform empty-target imports on the joining hosts.
2. Take C offline. A and B make concurrent disjoint and overlapping edits, with
   local claims and activity exports. Publish through ordinary Git push and
   reconcile non-fast-forward or semantic conflicts by fetch/merge and local
   atomic import; preserve each host's pending changes and native authority.
3. Let C make local changes while offline, then rejoin. Fetch the current branch,
   reconcile its retained base/local/incoming text, push the resolved commit
   where needed and import locally. A and B then fetch/merge/import that result.
   Compare shared task projections after all local changes have been reconciled;
   local runtime state is deliberately not expected to match.

Prove repeated fetch/import is stable, interruptions retain retry identity, and
duplicate work remains visible through origin-qualified activity when exchanged.
Q6 permits duplicate work while disconnected; no global exclusive claim is
inferred. There is no mailbox step or mailbox implementation prerequisite.
Unaffected planning can sync; incompatible active cases remain visibly retained
or refused until active reconciliation is delivered.

Onboarding each participating Engram host requires an Engram binary and automated Git
exchange transport/subprocess support verified on that host's platform: macOS,
Linux or Windows. The current Mac endpoint is the first instance, not a special
case. Platform lanes provide this support without changing the model; once the
platform is supported, adding another Engram host is configuration plus verification.
Explicitly authorized manual Git
can carry a bounded pilot in the meantime; it is not proof of automated exchange.
A fake transport success never establishes a supported real platform.

### Active reconciliation

Reuse the existing Engram original-operation settlement and active-reconciliation
work with TermAl's G1/G2/G3 prerequisites. Real host proof covers late first
Begin/Evaluate versus retirement, lost reply, orphan process after a ready ACK,
boot/retry/late dispatch, holder release and cleanup races, reopen and concurrent
completions. No global pause or authority transfer is introduced.

Cross-cutting per-platform lanes cover an Engram binary and automated Git
transport/subprocess/admission on macOS, Linux and Windows, including the existing
SSH transport work. The current Windows HTTPS runner does not prove another OS
or transport. Manual authorized Git can carry a bounded pilot, but does not prove
automated exchange support. Current endpoint facts and Windows consumer
binary/gate requirements remain visible; sync does not solve release-platform validation.

Integration acceptance also covers push succeeds then crash before local confirmation;
import commits then crash before the caller receives its result; retry after participating
hosts restart; a newer conflicting candidate; a merge rejecting/reverting an
ancestor's change; and clone/restore origin collision. Recovery uses the retained
publication intent and local ImportReceipt. No mailbox outbox or receipt is involved.

Proposed additional scopes, to deduplicate against existing work after review: internal
origin/store identity and wire version; immutable
evidence-reference profile only if included; real three-or-more-host integration/recovery
proof and per-platform binary/transport support where existing scope is insufficient. Reuse the exchange-progress,
model-A and host G3 work.

## Recorded decision and remaining decisions

Independent-writer task sync is the selected first-class planning model; portable
store-handoff remains separate and inactive. The disconnected-work decision (Q6)
means that Greg accepts duplicate task work while hosts are disconnected.
Recorded Advisor relay, 2026-10-17:

> Greg accepted Q6 YES: disconnected hosts may duplicate task work.

Code and task status reconcile during ordinary Git merge plus Engram import.
Published started activity and ordinary coordination may reduce duplication but impose no
global lock or exactly-once external effects. Merging task records cannot undo
duplicate external actions.

Confirm no evidence payload widening in initial JSON, and distinguish reported outcome from
native completion in UI/API.

Agree stable Engram-internal origin identity and clone/reinstall behavior. TermAl's
central session/mailbox control plane carries ordinary coordination only, with no
task-sync protocol or dependency on mailbox delivery.

Confirm staged pilot can first coordinate/admit supported planning while retaining active
conflicts, without calling that full active reconciliation.

The [current source-landing rules](git-task-sync.md#data-commits-and-source-landing-policy)
remain: data-only commits have no new exemption; mixed task/code
input is frozen/gated together. Remote merger authority and per-platform validation are
separate reviewed decisions. New scope has no reliable calendar estimate until the
Engram origin, integration and platform decomposition settle; the earlier 12–20 Engram
engineering-day estimate covered retained Git exchange and divergent/lifecycle
reconciliation only, not macOS/Linux or multi-host rollout. TermAl mailbox implementation is
estimated and scheduled independently.

A capability- and capacity-aware cluster scheduler is a separate deferred design
after these foundations. This task-sync design adds no scheduler, task placement
authority or cross-host claim arbitration.

