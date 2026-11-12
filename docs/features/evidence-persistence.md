# Retained evidence storage

The [imported verification core](imported-verification.md) builds explicit local
applicability and a terminal-fact union on this inert retained graph. The
[historical acceptance core](imported-acceptance.md) separately assesses retained
evaluations and native completion proofs without installing local completion.

The full-evidence [Git task exchange](git-task-sync.md) retains imported facts
without activating them as destination execution. The storage port keeps original
record IDs (including historical 64-character IDs), kinds and canonical bytes
in `objects`. Each original is stored once.
A pack's original UUID identifies its immutable header and ordered membership;
encoding that header with the original bodies reproduces the original pack.
A separately minted local admission records which originals first arrived through
that import. It grants no verification, completion, claim or session authority.

Retention runs under the caller's transaction with its own savepoint. A same-ID
kind or byte conflict rolls back the whole selected batch even when the caller catches the
refusal and commits the surrounding transaction. Exact replay returns the original
admission without creating another receipt. The exchange ledger owns operation
retry and the single pending slot; this storage port does not dispatch exchanges.

The bounded batch port takes selected packs together with their preallocated
admission UUIDs. It stages all original objects and qualified memberships before
validating origins, memory dependencies, feed segments, cuts and governing
witnesses. Related records may span packs in either transport order, including a
component larger than one pack. Every pack keeps its original UUID and encoding;
the port never repacks a component or fabricates missing context. The one-pack
port uses the same transaction. Cold projection repair likewise restores all
memberships before checking cross-pack dependencies. Normal admission remains
bounded to the selected batch and keyed retained dependencies.

The canonical exchange state model gives incoming evidence its own preparation
and applied-outcome variants on that same chain. Each preparation fixes pack and
local admission UUIDs, exact encoded-byte fingerprints and profile bounds.
Evidence completion settles its operation without advancing either planning
head. Keyed completed-request reads return the original outcome before opening
current Git or pack files. The incoming library path retains an exact construction
recipe before reading selected pack blobs: its preparation/outcome UUIDs, time,
predecessor and admission map survive an unfinished retry. A different or partial
recipe refuses without replacement. An exact pending-recipe exception is confined
to that request's private origin continuity; ordinary custody still refuses files
ahead of canonical progress. The two canonical events, inert records, admissions
and keyed progress share one admitted SQL writer. Neither the recipe nor import
grants destination verification credit.

A known dependency, witness or identity rejection stores a refused terminal under
the recipe's original outcome UUID and event envelope. A measured profile excess
stores an oversized terminal with its bound and actual size. Any payload rows
written before a late rejection roll back before that terminal is recorded.
Neither terminal advances planning or evidence coverage. Exact retries return the
same recorded rejection; they do not replace the recipe with a new operation.
Storage failures, private uncertainty and unclassified structural errors remain
unresolved rather than being relabeled as known refusals.

Selected pack locators fix the publishing origin, pack UUID and data carrier.
The reader opens those exact ordinary Git blobs, validates their complete encoded
bytes and keeps the original bodies. Evidence blob reads have their own bounded
limit up to the pack profile; ordinary small Git queries keep their existing
limit. They change no code files, index, branch or remote. Fresh dispatch retains
the current confined-Git platform boundary; historical completed replay precedes
that boundary and current files.

Typed dependency inventory distinguishes original-object links from content
fingerprints, Git identities and inert execution scalars. Batch planning checks
required context even for an already-known selected original and orders complete
components across selected packs. Explicit project fields and shared work-memory
scope must match. Import-supersession and reconciliation-hold-abandonment work
events retain their original ids, canonical bytes and embedded custody references.
Their hold, prepared-operation and receipt references are inert provenance from
the originating store, not evidence dependencies to follow. Retention creates no
stub or imported custody authority and does not claim to revalidate that store's
custody. The events' actual execution, root and evidence dependencies still must
close; missing or wrongly typed required originals refuse the whole batch.
Incoming planning blocked by a live claim records a distinct deferred terminal
on the same exchange chain. Preparation, the existing affected-scope hold and
that terminal share one SQL transaction. The terminal retains the original
candidate and immutable claim proofs, clears only its own pending operation and
advances neither planning head nor membership coverage. Its local capture cut
includes the hold's bookkeeping event without treating that cut as applied
planning. The original holder can renew and release; those later acts never
change the old request's deferred receipt. Evidence has its own subsequent
operation, while the unresolved hold still fences planning in its scope.
An explicit checked successor can apply retained reopen, revision, prerequisite
and blocker transitions after affected local execution ends normally. It keeps
the original deferred receipt and separately records its own applied outcome.
Family progress is derived from those canonical preparations and outcomes;
retained evidence never covers deferred planning. Explicit exchange steps drive
receive/import and delegate `consume_verification` and `consume_acceptance` to
the native evidence owners. Retention and qualification alone grant nothing;
explicit local consumption can satisfy obligations and feed ordinary completion
after its current binding, source, governing and policy checks. Originals remain
inert historical facts.

`engram task-sync progress --binding ID` and its read-only shared service report
planning and evidence capture separately in each direction. Each frontier
names a canonical preparation or outcome in that binding's exchange chain, not
a project, root or run-feed cursor. Coverage means applied selected planning,
confirmed selected publication, or retained selected evidence, explicitly named
in the response. It is not current native evidence eligibility. Uncovered request
counts retain earlier pending, deferred or refused selections even when a later
selection succeeds. An applied successor accounts for its original deferred
selection without rewriting or counting that original as applied. An empty
family row makes no claim that an outgoing workflow is implemented or that a
project is fully synchronized. These reads create no cursor, store, Git effect
or verification credit, and remain usable while sync is disabled.

Foreign enrollment records establish qualified historical namespaces. They do
not become the destination's active producing origin, private continuity witness
or project feed. An identical original already stored natively remains native.
Feed segments, cuts and governing-snapshot witnesses are retained separately from
native feeds, with exact membership and predecessor checks. Missing context is a
refusal, never fabricated history. Selected work-scoped memory can travel as an
evidence dependency; private and project-memory dumps cannot. Native capture
binds the explicit task-sync disclosure profile to its immutable selection.
Notes-off selections refuse closures containing work memory or ordinary note
summaries. Restricted labels, sensitivity, note text and author exclusions also
refuse the whole closure, preserving original bytes and feed-witness continuity.
Export rechecks the selected binding's profile and current task labels before
staging files; a retained capture cannot bypass a later restriction.

Keyed pack, membership, imported-original, origin and witness views derive from
the immutable admissions and originals. Ordinary native lookups check the selected
partition row against its canonical admission. A missing partition row also checks
the selected member/pack links and refuses when their admission identifies it as
foreign. This reads bounded selected metadata, not the entire admission history.
Simultaneous erasure of all provenance indexes requires the exhaustive doctor or
explicit projection repair; a keyed lookup does not claim to detect that case.
Repair rebuilds the foreign partition before deriving native projections. Full
store migration retains the originals and admissions and rebuilds these views.

Export captures an immutable local pack endpoint and seeks by project and dense
pack position. Each page contains whole packs within the common evidence profile's
byte and object bounds. Later imports cannot move an existing cursor's endpoint.
Limits and typed oversize results are owned by `task_sync::evidence::limits`;
no record is truncated. Dependency closure, complete-component admission and the
data-worktree workflow remain with the full exchange owner. Retention alone is
not a claim that those workflows or imported-proof applicability are complete.

Adding the derived views requires explicit projection repair for existing stores
under the repository cutover rules. It does not rewrite existing canonical rows
or restore retired live operations. It is integrated and validated with the joint
sync package, rather than installed independently.
