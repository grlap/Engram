# Acceptance evaluation

> Normative section: [spec § Local work system](../spec.md); related briefs:
> [Local work system](local-work-system.md) (completion seals, obligations),
> [Execution pipeline](execution-pipeline.md) (typed evidence),
> [Behavioral control plane](behavioral-control-plane.md) (host channel),
> [CLI & MCP](cli-and-mcp.md) (agent words),
> [Turn gate assessment](turn-gate-assessment.md) (why the evaluation request
> stays out of the turn gate),
> [Lifecycle ownership](acceptance-evaluation-lifecycle.md) (rule owners,
> admission and completion horizons, consolidation proposals).

This brief is the contract for evidence-based acceptance evaluation before
completion. It describes the agreed design, not a claim that every row is
implemented; the [boundary matrix](#boundary-matrix) below is what tests trace
to, and the [status](#status) section names what has landed.

Today, `done` validates that acceptance results cover every criterion and that
cited evidence belongs to the run; it does not evaluate what a criterion means.
Without explicit input every criterion is sealed `satisfied: true` with a note
naming the completing actor. Links are author citations, not verification. The
change here adds a recorded, attributed, per-criterion evaluation that Engram
enforces at completion under a per-project policy, without Engram ever calling a
model itself.

## Boundary

- **The host evaluates; the core enforces.** Engram never runs a model, a
  build, or a command. Whoever evaluated submits an immutable
  `AcceptanceEvaluation`; Engram validates its structure and provenance,
  binds it to the exact criteria and run state, and refuses completion unless
  a fresh passing evaluation exists. Engram validates that citations are what
  the verdict claims; it does not prove that they are *relevant* — relevance is
  the evaluator's judgment and is recorded as such.
- **Identity stays at its actual assurance.** Evaluator identity, mode, and
  model metadata are asserted host context unless a host channel mediates them.
  Receipts say `asserted`; nothing here presents an attached string as verified
  independent execution.
- **Criteria text, outcome text, and evidence are untrusted task data.** They
  are inputs to the evaluator's judgment, never instructions for Engram and
  never authority to run commands. Evaluator budget, permissions, and workspace
  access are host-owned.
- **No automatic retry.** A failed or insufficient evaluation returns an
  actionable outcome; corrective work and an explicit new evaluation follow.
- **No human override in this slice.** `needs_human` blocks completion with
  guidance; the existing authorized criteria revision and cancellation paths
  remain the only ways forward. An explicit override authority, if ever wanted,
  is designed separately before it is implemented.
- **Default unchanged.** Projects without an acceptance-evaluation policy keep
  the existing path; its receipts now name it `self-asserted`.

## Modes

Greg requires three evaluation modes. They are alternatives, not an ordinal
security ladder; policy expresses compatibility explicitly.

| Mode | Who evaluates | Identity relation to the completing session | Assurance recorded |
| --- | --- | --- | --- |
| `same_session` | the completing session itself, as an explicit continuation | same session id | asserted self-evaluation; distinct from the self-asserted path because a real per-criterion record exists |
| `sub_agent` | an evaluator spawned under the completing session | its own host-assigned child session id, never one that holds, executes or held the run, plus a distinct `execution_identity` and a host-attested `parent_session` | asserted; a host channel may later raise it, the core never infers it |
| `independent_session` | a separate session, possibly a different model or provider | session id differs from the claim holder and from every recorded executor session of the run | asserted independence enforced structurally on session identity |

Model and provider are structured optional metadata on the record
(`evaluator_model { provider, model, version? }`), not text hidden in
provenance links.

## Policy

The project control policy gains one canonical field, recorded through the
existing immutable authority/epoch/compare-and-swap path with a new
`set_acceptance_evaluation` operation:

```text
acceptance_evaluation {
  allowed_modes: [same_session | sub_agent | independent_session]  # empty = self-asserted path
  mechanical_basis: asserted | observed     # minimum evidence for a pass on an observed-type basis
  require_source_freshness: bool            # completion must present a matching source fingerprint
}
```

- Empty `allowed_modes` is the default and keeps the self-asserted
  completion. Any non-empty set switches the project to evaluated completion.
- `allowed_modes` is a set, not a floor. "Independent only" is
  `[sub_agent, independent_session]`; "exactly this mode" is a one-element set.
- Policy changes affect later evaluations and completions; an existing
  evaluation whose mode the new policy no longer allows is not fresh.
- The field lives in the canonical policy object. Existing policy versions keep
  their bytes and ids; no durable table changes.

### Task mode

A work item may select its evaluation mode as a revision-controlled planning
field (`evaluation_mode`, visible in `show`, set with `add`/`update`). When set,
an evaluation must use exactly that mode. A task cannot select a mode the policy
disallows at record time; the refusal names the allowed set. Selecting a mode
never downgrades the policy.

### Independent by default

Evaluation is independent unless the task is marked otherwise, so that an agent
cannot close a task on its own judgment by forgetting, by a shortcut, or by
marking its own task. These rules add no policy field and change no stored
record; the mode and mark rules are checked when an evaluation is recorded
and again when completion consumes it on open work, and the failure rule
when an evaluation is recorded. Sealed completions are not reassessed.

- **Unmarked task.** A task with no mode set refuses a `same_session`
  evaluation whenever the policy admits another mode. The refusal says to
  request an independent evaluation from the host, and that same-session needs
  the task marked for it by someone other than its executor. A project that
  admits only `same_session` is unchanged: there the completing session
  evaluates its unmarked task. Other admitted modes keep their own identity
  rules.
- **Same-session mark.** A task marked `same_session` admits a same-session
  evaluation only while the session that set the current mark neither
  evaluates, holds nor executes the run, now or earlier in it. This holds in
  every policy, including a same-session-only one. The mark's author is the
  session of the `Created` or `Revised` event that turned the mark on, read
  from the item's own events: revisions of other fields, and reasserting the
  unchanged mark, keep that author; clearing or changing the mark ends it, and
  setting it again authors a new one. A mark set at creation by the later
  executor, or by the holder to escape a failed independent evaluation, is
  refused; a mark set by an operator or a peer is accepted. A mark with no
  author recorded on the item is refused with a remedy to clear the mark or
  change it to another mode the project admits; only setting `same_session`
  again needs a session that never held or executed the run: its author's
  session was not recorded, the item's earlier history was restored rather
  than recorded here, or the item is a detach successor, whose creation
  carried the mark over without its detaching session setting it. In a project
  that admits only `same_session`, clearing the mark alone is enough, since
  the unmarked task then takes its executor's own evaluation. When the mark's
  author later takes the run, completion no longer consumes an evaluation made
  under it (`stale (policy)`).
- **Sub-agent.** A `sub_agent` evaluation recorded from a session that holds
  or executes the run, or held it earlier, is the executor's own evaluation:
  it is refused at record time and not consumed at completion, like an
  unmarked `same_session` one. A `sub_agent` evaluation recorded from a
  distinct child session, with a holder or executor as its parent session,
  remains admissible where the policy admits `sub_agent`; if that child
  session later takes the run, completion no longer consumes it.
- **A failure stands until new evidence.** When the newest evaluation on the
  run is blocking (`fail`, `insufficient_evidence` or `needs_human`), a later
  evaluation records only if evidence of a qualifying kind lies on the run
  feed after that evaluation's cut and at or before its own. Qualifying kinds
  are notes, gates, host verification and environment evidence, and an
  observation that the source changed to a revision other than the one last
  seen, starting from the source the blocking evaluation judged (its declared
  revision, or else the run's last sighting at its cut); a reported change
  that carries no revision qualifies too, and so does an accounted unadmitted
  change, whatever revision it reports, since it already makes the blocking
  evaluation stale. Evaluation records, and claim,
  renewal, handoff, revision, checkpoint and obligation bookkeeping, do not
  qualify, nor does a repeat sighting of an unchanged source, an admitted
  reported change that leaves the source at the judged revision, a sighting
  outside the
  claim's named source root, or a quiet sighting at another revision without
  the change flag (which does make the blocking evaluation stale, but a note
  still has to precede the next one). A flagged change compares with the
  revision last seen, quiet sightings included, so a change to a revision
  already sighted since the failure does not count either. A host check counts
  wherever it ran, as any host check does. The rule holds for any verdict and
  any evaluator, and also when the blocking evaluation has gone stale for
  another reason, so a re-roll on the same evidence, or after an edit of the
  title, the mode or the policy, is refused with a remedy to record the
  correction first; a correction note followed by a new evaluation replaces
  the failure. The refusal carries the typed cause `reroll`, and the
  evaluation status shows the same cause through the active run's head, so a
  host can see a standing blocking evaluation before it starts an evaluator
  (shape in [CLI and MCP](cli-and-mcp.md)). When the item's criteria or their bindings differ from those
  the blocking evaluation judged, the carried-failure rule (R11 under
  [Record-time validation](#record-time-validation)) governs instead. An exact
  resend of a recorded attempt still replays it.
- **Remedies.** A refusal of a pinned mode says only to evaluate in that mode;
  it no longer suggests revising the task, which would let the executor choose
  its own evaluator. `done` without an evaluation first points at requesting
  an independent evaluation from the host, in host-neutral words; it names a
  sub-agent evaluation instead for a task marked for one or where the project
  admits no independent mode, mentions same-session only for a task marked
  for it or where the project admits no other mode, and never asks for a mode
  the project does not admit (a mark naming one is reported as such). A
  `done` refused because an evaluation went stale with reason `policy` gives
  the same remedy, from the task's current mark and the admitted modes.

**Limit.** Session and actor identities are asserted, not authenticated. These
rules stop forgetting, shortcuts and re-rolls; they do not stop deliberate
forgery of another session's identity.

## The evaluation record

`AcceptanceEvaluation` is an immutable canonical object appended to the run
execution feed (`object_kind = acceptance_evaluation`). It needs no projection
table: completion and `show` read the newest entry of that kind on the run
feed.

```text
AcceptanceEvaluation {
  schema_version, project_id, root_id, work_id, run_id
  work_revision, work_revision_hash          # exact criteria list evaluated
  criteria: [text]                           # copied verbatim at record time
  evaluated_cut: FeedPosition                # run-feed position the evaluator read through, supplied by the submission
  evidence_basis: [id]                       # run evidence at or before that cut
  source_basis: { workspace_id?, fingerprint }?   # host-measured, asserted
  mode: same_session | sub_agent | independent_session
  evaluator: ActorContext                    # session, actor, source tool, provenance
  execution_identity?: text                  # sub-agent: distinct evaluator identity
  parent_session?: SessionId                 # sub-agent: attested parent
  evaluator_model?: { provider, model, version? }
  verdicts: [ { criterion, verdict, basis, rationale, evidence: [id] } ]
  attempt_key                                # explicit or content-derived
  created_at
}
verdict  = pass | fail | insufficient_evidence | needs_human
basis    = observed | asserted | judgment | human_required
```

### Record-time validation

Every rule refuses the write before any effect; nothing is appended on refusal.

- **R1 policy.** The project policy enables evaluation (`allowed_modes` is not
  empty) and contains the submitted mode; when the task selects a mode, it is
  that mode.
- **R2 target.** The work is open with an active run; the evaluation binds to
  that run. A claim is not required to evaluate.
- **R3 criteria.** The submitted `acceptance_basis` (the work revision read from
  `show`) equals the current revision and the verdict list covers every current
  criterion exactly once, by position. A revised item refuses with "re-read
  show".
- **R3b evaluated cut.** The submission names the run-feed position the
  evaluator read through (`evidence_basis` from `show`). It must be a position
  on the run's feed, no host-observed mutation or check (the F3 kinds) may
  follow it, and no citation may lie beyond it. The record's `evaluated_cut`
  is that supplied position; the head at submission time is never substituted.
  A mutation observed between the evaluator's read and its submission
  therefore refuses the record instead of being covered by it, and the refusal
  names its class. `acceptance_evaluation_resubmit`: only a host check was
  recorded after the cut; re-read `show`, take the check into account, and
  submit again. `acceptance_evaluation_void`: the source changed after the cut
  to a revision the evaluation did not judge; the evaluation is void, so
  request a new one. When a source observation after the cut decided that
  move, the refusal names it: the message keeps its words and adds one
  sentence naming the observation's run-feed position, workspace, revision,
  reporting session and times as recorded, beside the revision the evaluation
  declared or, without a declaration, the revision it judged at its cut; the
  structured details add `deciding_observation` beside the unchanged
  `reason` and `remedy`. The observation named is the first reported change
  to an undeclared revision, which decides at once, or else the newest
  sighting at another revision that no later sighting or declared change put
  back. A check-only move, a named-root rebinding, and any other cause name
  no observation. In the sentence each host-recorded field is escaped onto
  one line and cut at 300 bytes with its stored length, and it never spells
  "database is locked", which a host may read as a locked store, even once
  its whitespace is collapsed: such a field writes every whitespace character
  as a visible escape such as `\u{20}`, counted within its bound; the CLI's
  JSON refusal writes the spaces of that phrase as `\u0020` escapes, so the
  fields still decode to what was recorded. `done`'s refusal naming the same
  observation, below the recovery causes, is guarded alike. The source also counts as changed when the newest
  execution observation after the cut that carries a revision shows it at
  another revision than the judged one, even while reporting no change (F3
  below). The one source change that does not count is a change to the
  revision the evaluation declared it judged (below), together with the
  obligation it opened. Nor does a host check that passed on that declared
  revision, with the environment record it links and the obligation
  resolution it made, while the run's newest sighting is at that revision: a
  passed check on the judged source can only support the verdicts, and a host
  that starts the evaluator during the requesting turn reports that turn's
  tests after the cut. Any other check still asks for a resubmission: a
  failed or indeterminate one, even beside a passed one in the same report,
  one on another revision or in another declared workspace, and any check
  after an evaluation that declared no revision. Likewise the source fingerprint
  is the value measured for the evaluated content, not one taken at
  submission. It is the host's source revision, as the host reports it on turn
  observations; a declared workspace id must match the observation's workspace
  too. Once the host binds a named root to the claim, the evaluated source
  must be the newest sighting in that exact workspace and generation through
  the cut, or a revision the evaluation declared that no sighting of the root
  after the cut contradicts. A host that starts the evaluator during the
  requesting turn declares the revision it measured before that turn's report
  sights it. Completion then waits until the host sights the root at the
  declared revision (stale with reason `source` until then), and a declared
  revision the host never sights blocks completion until a later evaluation
  replaces the record; an older passing evaluation does not stand in for it.
  A root the host has named but not yet sighted anchors no evaluation, since
  its first sighting could show any source: `evaluate` refuses until the host
  captures it. An explicit declaration of another workspace refuses rather
  than borrowing a matching revision from the named root.
- **R4 independence.** `independent_session`: the evaluator session differs
  from the claim holder and from every recorded executor session of the run.
  `sub_agent`: `execution_identity` and `parent_session` are present, the
  parent equals the current holder or executor session, and the evaluator
  session neither holds nor executes the run and never held it.
  `same_session`: the evaluator session equals the current holder or
  executor session, and the task admits it under
  [Independent by default](#independent-by-default).
- **R5 pass citations.** Every `pass` carries a non-empty rationale and at
  least one citation, so a seal built from a fresh all-pass evaluation links
  evidence to every criterion; that is why an evaluated item's `show` names
  no [unlinked criteria](cli-and-mcp.md#using-engram-as-an-agent) before
  completion. `observed`: each citation is host-minted
  `VerificationEvidence` bound to this run whose result is `passed`.
  `asserted`: each citation is a gate record on this run with no failure
  labels; refused when policy `mechanical_basis` is `observed`. `judgment`:
  each citation is run evidence (note, gate, verification, or environment
  evidence). `human_required` can never be a `pass`. A criterion bound to a
  typed verification requirement (`--bind`) passes only on `observed`, and
  every citation must be verification evidence of the bound kind (and pinned
  check) with a passed result; `asserted` and `judgment` are refused for it
  by name. Each of those checks must also have run on the source the
  evaluation judged: its source basis carries the judged revision, and the
  declared workspace when the evaluation declared one. The judged source is
  the declared `source_basis` when there is one, or else the revision of the
  newest execution observation at or before the cut that carries one, as in
  F3; a cited check's own observation is such a sighting. A declaration
  always decides, so an older sighting never stands in for a declared tree.
  Nor may the source have moved away after the check, read as F3 reads it
  after a cut: between the observation that ran the check and the cut, the
  newest execution observation that carries a revision must show the
  check's revision, and none may report a change without one. The revision
  fingerprints the full content, so a move and its revert leave the check
  standing. Without a named root, a later sighting of the same revision from
  any workspace changes nothing. With a named root, the cited check must
  follow the binding and carry its exact workspace, generation and `named`
  state; a pre-binding check, a foreign check or an earlier generation cannot
  pass even at the same revision. Later sightings are compared inside that
  named root. A passed check at R_d says nothing about an
  edit to R_e that the source still holds, even one a declaration of R_d
  leaves out. So a citation of a check of another revision is refused,
  naming the citation and both revisions, and so is a check the run has
  since moved away from, naming the later revision. The holder reruns the check,
  and a new evaluation cites the rerun; a declaration in another form than
  the host's source revision, such as a Git commit id, matches no check and
  is corrected instead, which the refusal names only when the evaluation
  declared a source. Completion applies the same rule again (F8).
- **R6 non-pass citations.** `fail`, `insufficient_evidence`, and `needs_human`
  carry a rationale; citations are optional but must belong to this run, and a
  failed check may be cited (a failed build supports a `fail`).
- **R7 citations are locators.** A citation is what the agent saw: a note or
  gate locator exactly as `show --notes --gates` prints it, resolved by the
  same resolver as `done --link` (a non-holder observation, an inherited
  record member, another item's or an earlier run's record, and an artifact
  path or URL all refuse, naming the locator and the reason), or the full id
  of host-minted verification or environment evidence on the run, which no
  locator window prints. The record keeps the resolved full ids.
- **R8 attempt identity.** With an explicit attempt key, the same key with the
  same payload replays the same object; the same key with a different payload
  refuses; a new key records a new object even for an identical payload.
  Explicit keys are scoped to the work item and its run
  (`explicit:<work_id>:<run_id>:<key>`), so a host may reuse per-task
  counters across items and restart them on a new run, while the same key on
  the same run replays an exact resend and refuses a changed payload.
  Explicit keys are shared per run across evaluators: a second evaluator
  reusing another evaluator's key on the same run gets a safe idempotency
  conflict, not a record, so hosts keep explicit keys unique per run or use
  keyless attempts, whose identity includes the evaluator session. Without
  a key, the attempt
  is content-derived (`content:<hash>`) from project, session, work, run,
  revision, evaluated cut, and payload, so an identical resend replays and any
  change records a new evaluation; a new run makes every earlier attempt a
  different one, whether the run came from a reopen (which also bumps the
  revision) or from a restored item's first execution (which does not). A
  recovery claim keeps the run and its attempts. The word computes the same
  identity before its preflight, so the projected attempt key is the recorded
  one.
- **R9 bounds.** Each rationale follows the existing 64 KiB note-text bound;
  at most 64 citations per verdict; a source fingerprint or workspace id is at
  most 256 bytes; an explicit attempt key is at most 256 bytes; each evaluator
  model segment is non-blank, at most 128 bytes, and free of control
  characters (checked at the core write boundary, not only by the word's
  parser); a sub-agent `execution_identity` is at most 256 bytes with no
  control characters and `parent_session` follows the live session-id bound
  (64 bytes), both asserted identifiers rather than proof of independent
  execution; and the whole frozen object is at most 1 MiB of canonical bytes.
  Every bound is checked before the write, so a refused record leaves the run
  feed and the newest record untouched. The criteria count is not capped:
  receipts carry a bounded prefix of verdict rows with an exact omitted count
  (below), never a truncated record.
- **R10 canonical checks.** An `observed` citation is classified from the
  canonical `VerificationEvidence` object; the rebuildable
  `verification_result` projection column must carry exactly the canonical
  result word (a missing value or any other variant refuses), and a
  disagreement refuses the record as an invalid projection rather than
  admitting anything from the column. Reads follow the existing strict
  contract for evidence projections rather than an advisory one: `show` and
  `next` assemble their evidence summaries through the same evidence read
  that refuses a projection disagreeing with its canonical object, and
  completion refuses likewise, so the evaluation classification adds no read
  path softer or stricter than that contract. `doctor` names the row for
  repair, and restoring the column restores the read and the observed pass.
- **R11 carried failure.** A revision retires the newest evaluation (F1), so
  without this rule an executor could reword the criteria an evaluation failed
  and complete on a fresh pass of the new wording, with the failure gone from
  every surface. When the newest evaluation on the run does not pass
  everywhere, a failure is **carried** in one of two cases:
  - **It names a failure it superseded.** That failure, the root of the
    `supersedes` chain, stays carried whatever the criteria are now: naming a
    failure with a verdict that does not pass accepts no revision. Only a
    passing evaluation that names it ends the carry, except that a
    planner-only carry also ends with a newer evaluation that does not name
    it.
  - **It names nothing.** It is itself carried when a revision after it
    changed the criteria it judged or their verification bindings (dropping
    or changing a binding weakens a criterion as surely as rewording it) and
    the current criteria or bindings still differ from them. Rewording back
    to the judged contract ends the carry, and a revision of other fields
    carries nothing.

  For example, F fails C1 and the executor rewrites it to C2, so F is
  carried. A reviewer that names F but fails C2 keeps F carried. If the
  executor then reverts to C1, F is still carried, and so it is after a second
  failing review of C1; a planner's revision in between is kept the same way.
  Only a reviewer's pass that names F ends the carry.

  Its other staleness reasons do not end the carry, since a check or source
  change after a failure is the ordinary next step. A revision counts as the
  **executor's** when it was made under the run's claim, or by its executor or
  a session that holds or held the run; any other session revised it as a
  **planner**. The classification is read at each evaluation over every such
  revision since the carried failure, so it only ever tightens: one
  executor's revision among them is enough, and a planner that revised and
  later holds the run counts as the executor. Sessions are asserted
  identities: an executor that releases the claim and revises from a session
  that never held the run is classified as a planner, and its revision is
  still disclosed.

  `show` discloses a carried failure either way, so the next evaluation sees
  the failed verdicts and the criteria and their bindings before and after,
  and judges whether the revised criteria still deliver the requested
  outcome. After an executor's revision the evaluation must name the failed
  record with `supersedes` (`--supersedes RECORD_ID`), and someone else must
  submit it. Identity admission already refuses an independent_session
  evaluation from a session that holds, held or executes the run, with its
  own reason; any other evaluation from an executor of the run, a
  same_session one or a sub_agent whose own session holds, held or executes
  the run, is refused `carried_failure_self_acknowledged`, so a sub_agent
  that shares an executor's session is that executor. An independent_session
  evaluation qualifies, and so does a sub_agent under its own session; a
  sub_agent is independent only where the host, not the executor, composes
  its brief, and an independent_session evaluator is only as distinct as the
  session id it asserts (a local CLI caller can start a fresh session).
  Engram records the evaluator's session and claims nothing stronger. The
  check runs when the evaluation is recorded; at `done` the identity rule
  rechecks an independent_session record, and the
  [Independent by default](#independent-by-default) rules recheck a
  sub_agent record, so an evaluator that later takes the run no longer
  supplies a consumable acknowledgment and a distinct session must evaluate
  again. A project whose policy or task pin allows only
  same_session cannot supersede such a failure until an authorized change
  allows a distinct evaluator or, while no later failing evaluation has named
  it, the criteria are revised back; nothing falls back to
  self-acknowledgment. After a planner's revision alone the evaluation may
  name the failure, from any evaluator, and one that does not name it ends
  that planner-only carry. The record keeps the id it names, so a seal bound
  to it names the failure it superseded. `supersedes` naming another record,
  or any record when no failure is carried, is refused.

  Anything that ends the item's run (completion, disposal or detachment) ends
  the carry, since it belongs to that run. A new run cannot shed a failure
  either: only completed work reopens, and completing needed a fresh passing
  evaluation, which names the failure after an executor's revision. Every
  revision of an item with an active run lands on that run's feed; revisions
  there that do not lead to the item's current criteria and bindings are a
  damaged projection, refused as such, never read as nothing carried.

## Freshness

Completion consults the newest evaluation on the run feed. It is **fresh** only
when all of the following hold; otherwise it is treated as absent with the
stale reason named.

- **F1 revision.** `work_revision_hash` equals the current item's; any revision
  (criteria, outcome, title, mode, or other planning fields) invalidates.
- **F2 run.** `run_id` is the completing run; a new run generation starts
  without an evaluation.
- **F3 host-observed mutation.** With no named root, no execution observation
  with `source_changed`, no verification or environment evidence, and no
  obligation definition or resolution was appended to the run feed after
  `evaluated_cut`, and the newest execution observation after it that carries
  a revision does not show the source at another revision than the judged one.
  With a named root, source sightings are compared in its workspace and
  generation; a foreign sighting cannot claim the named source moved. Host
  checks and obligation records still ask the evaluator to re-read the cut,
  and an unknown-root change is never presumed foreign. These are host-minted
  facts about the workspace and its checks, with `source_changed` recorded as
  the core's reading of the host's report: a reported change that leaves the
  source at the revision of the run's newest recorded source change, with no
  other revision seen since (in an observation or environment evidence), is
  recorded as no change. That clears only its change flag: like any
  observation, it is still compared with the judged revision below. Two
  exceptions: a check that passed on the declared revision (R3b), and a
  source change that left the source at the revision the evaluation declared
  it judged (its `source_basis`), with the obligation that
  change opened: the evaluator saw that state, so the host's late report of it
  does not void the evaluation. The source can also move without a reported
  change, as when a check runs after someone else's edit. So when the newest
  execution observation after the cut that carries a revision shows the source
  at another revision than the judged one, the evaluation is void, whatever
  that observation claims. The judged revision is the declared one, or else
  the revision the run was last seen at when the cut was taken: that of the
  newest execution observation at or before the cut that carries one. With
  neither there is nothing to compare. The revision fingerprints the full
  content, so it is compared whatever workspace reported it. Here only an
  execution observation counts as a sighting of the source: the host lists a
  turn's observations in the order it saw them, each at the revision the
  source had then. A turn reported after the cut may still hold sightings from
  before the evaluation, such as a check that ran before the edit the
  evaluator judged. So the newest sighting decides: a later sighting of the
  judged revision, or a reported change to the declared revision, puts the
  source back where it was judged, and sightings of another revision before it
  no longer count. The repeat reading above also counts environment evidence,
  which there only errs toward keeping a reported change. Verification and
  environment records describe a check and carry the content basis that check
  ran on, its producer's, which may predate the cut. So neither counts here as
  a sighting, and either one recorded after the cut asks for a resubmission,
  except a check that passed on the declared revision, with its own
  environment record and the resolution it made (R3b). A check that a pass on a
  bound criterion cites is held to the judged revision separately (R5, F8).
  The declared revision is the evaluator's assertion, recorded like its
  verdicts; Engram cannot attest what the evaluator read, so this exception
  carries the evaluation's own asserted assurance. It must be the host's
  source revision as the host reports it: a declaration in another form, such
  as a Git commit id, matches no host sighting, so the host's next sighting of
  the source voids the evaluation. A host that wants the revision to be one it
  measured passes that revision to the evaluator itself (see [turns, focus and
  evaluation timing](#turns-focus-and-evaluation-timing)). A `same_session`
  implementer gains nothing from it: it can re-read and submit at the new cut
  in any case. Under a named root, an evaluation that declared its revision
  is fresh only while the root's newest sighting, through the run feed's head,
  is at that revision (R3b).
- **F4 source fingerprint.** When policy `require_source_freshness` is on, the
  completion attempt presents a `source_fingerprint` measured by the host at
  completion time (`done --source-fingerprint F`) that equals
  `source_basis.fingerprint`; a missing or different fingerprint refuses with
  reason `source`, and a record without a source basis can never match. A
  read (`show`, `next`) measures nothing: it reports the recorded fingerprint
  as checked when `done` presents one, rather than calling the record stale.
  Equality of two caller-supplied strings is asserted freshness, not
  independent verification; under the host channel the observation's source
  basis is authoritative.
- **F5 policy.** Every effective requirement is re-read from the current
  policy at completion: the evaluation's mode is still allowed and, when the
  task selects a mode, still equals it; under `mechanical_basis: observed` no
  `pass` may rest on an `asserted` basis; and F4 applies whenever
  `require_source_freshness` is on now, even if it was off when the record was
  made. A strengthened policy never lets an older, weaker record seal.
- **F6 relied-on evidence.** No gate record with the name of a gate cited by a
  `pass` was appended after `evaluated_cut`. A newer record of the same check
  replaces the cited one whatever its result; the evaluator cannot freeze an
  older observation to evade a newer failure. Newer host verification or
  environment evidence is already covered by F3.
- **F7 independence at consumption.** An `independent_session` record stays
  fresh only while its evaluator session neither holds nor executes the run
  and never did, read from the run's immutable claim, renewal, recovery, and
  handoff events. An evaluator that later takes the run by handoff or
  recovery cannot consume its own judgment: the record is stale with reason
  `identity`, and a session that never held the run must evaluate again.
  Holding a different run does not taint independence. `same_session` and
  `sub_agent` records make no independence claim; the
  [Independent by default](#independent-by-default) rules recheck them at
  completion instead, and one they no longer admit is stale with reason
  `policy`. A record of any mode without its evaluator's session, or a
  `sub_agent` record without its parent session, or whose execution identity
  is missing, blank, longer than its bound or holds a control character, or
  whose parent session is not a session id admission accepts, is stale with
  reason `identity` as well, after the `policy` checks: admission never
  records one, so it can only have reached the store by import or edit, and
  it never completes work. The shape is all this checks; whether either
  session is registered is not asked. After that shape check, and before the
  check above that an `independent_session` evaluator never held the run, a
  record whose shape admission refuses otherwise is stale with reason
  `record_shape`: its
  verdicts are not one per recorded criterion in the recorded order, or one
  has a blank rationale, more citations than the bound, or is a pass without
  a citation or on a `human_required` basis; or a `same_session` or
  `independent_session` record carries the parent session or execution
  identity only `sub_agent` records. Admission and consumption read these
  rules from one owner.
- **F8 bound check source.** Every citation of a `pass` on a bound criterion
  is a passed check that ran on the source the evaluation judged, and the
  source had not moved away from it by the cut (R5). The rule is applied
  again when `done` consumes the record, whether the binding's obligation
  was satisfied or waived. A record that fails it is
  stale with reason `verification_source`: run the check on the current
  source, then evaluate again citing it. This covers a record admitted before
  the rule existed. It also covers a binding whose obligation was waived,
  which no obligation rule matches to the run's latest source change. F4
  does not stand in for it: the fingerprint ties the evaluated content to
  the content at completion, not to the check a pass cites.

The host-private [named-root binding](behavioral-control-plane.md#5a-bind-a-named-source-root)
is claim-scoped. Evaluation records the binding active at its cut and becomes
stale if that binding ends, by an `ended` event or the claim's release, or
changes. Its judged source and F8 check use
only sightings in the bound workspace and generation: a cited check counts
only when both the check and the observation that produced it are in that
root and the check ran after the binding. A source change from a
foreign workspace captured before the binding can be disclosed at completion
as a non-satisfaction displacement; one recorded under a still-bound name
stays open until an explicit human waiver, even after that root ended or the
claim was released. An unknown-root source change recorded while a root was
bound requires a fresh check in the claim's active named root or a waiver,
even after that root ended, was released or was renamed. Neither the
evaluator nor the core infers workspace identity from a path string.

A host may also report a source change it observed without admission
([Record execution observed without
admission](behavioral-control-plane.md#5b-record-execution-observed-without-admission)).
Accounted as a new change or a repeat, such a record is a source record like
a turn's observation in F3, F8 and the judged source. As a barrier it also
retires a cited check that does not follow it: one whose producer was
recorded before the change, or that completed before the change was
recorded, whatever revision the change reports, since the report may
describe the source from before the check. For the same reason an
accounted unadmitted change after the cut voids the evaluation even when it
reports the declared revision: the F3 exception for a change to the declared
revision covers a turn's own change only. When the run's latest change is
unadmitted and no root is named, the judged revision and the revision a cited
check must still match are those of the newest measured sighting in that
change's workspace, environment evidence included, the selection the
obligation matcher uses. Audit-only records and the checks nested in an
unadmitted record are never sightings.

What does **not** invalidate an evaluation: holder notes and their checkpoints,
gate records for checks the passing verdicts did not cite, non-holder
observations, the completion attempt's own capture and checkpoint, and
project-memory writes. Appending a later evaluation is not a mutation either,
but completion always consults the **newest** record: a later `fail`,
`insufficient_evidence`, or `needs_human` blocks, and an older `pass` is never
selected around it. Without a host channel and a fingerprint policy, Engram
cannot see corrective source edits; the documentation of a pilot must say
which of F3 and F4 were actually in force.

## Completion enforcement

Under an evaluated policy `done`:

1. refuses any explicit acceptance results or positional links: the evaluation
   is the acceptance, and the receipt names the evaluate word instead;
2. runs the existing evidence, checkpoint, obligation, child, and fence checks;
3. requires a fresh evaluation whose every verdict is `pass`;
4. derives the sealed `AcceptanceResult` vector from that evaluation
   (`satisfied: true`, evidence = the verdict citations, note = the rationale)
   and binds the evaluation id into the seal as `acceptance_evaluation`;
   the verdict citations join the completion evidence set the capture
   checkpoints, so the seal names them, and the core refuses a seal whose
   citations fall outside its evidence, the same closure the self-asserted
   route keeps;
5. reports the provenance read back from the frozen seal:
   `acceptance: evaluated (<mode>, <assurance>) by <evaluator>` in the `done`
   text and in the completed item's `show` text, with
   `acceptance: {provenance, evaluation, mode, assurance, evaluator,
   evaluator_model}` in `done` JSON and in the completed item's `show` JSON.
   Self-asserted completions report `acceptance: self-asserted` in both
   texts and `acceptance: {provenance: self_asserted}` in both `done` and
   completed `show` JSON. Every completed item has an `acceptance` block;
   missing or unreadable completion provenance is `unavailable`, not inferred
   self-assertion. The evaluator label is the
   recorded asserted display identity, nothing stronger, and the
   `<assurance>` is the completing actor's assurance as sealed in the
   acceptance vector (asserted in V1), not a claim about the evaluator.
   A completed item whose seal exists but whose bound evaluation fails the
   shared check is disclosed rather than silently omitted: `show` prints
   `acceptance: provenance unavailable (…)` with `diagnostic class: <class>`
   and its JSON carries `acceptance: {provenance: unavailable, error_class}`,
   so a broken evaluated binding never reads like self-assertion, while
   `doctor` and completion stay strict. An unreadable seal or restored history
   without a seal also reports `provenance: unavailable`; criterion-evidence
   diagnostics retain the specific read failure when available.

Refusals extend the typed recovery causes; each carries one recovery command:

| Cause | Meaning | Recovery |
| --- | --- | --- |
| `MissingAcceptanceEvaluation { criterion }` | no evaluation for this run | record one: `engram work evaluate REF …` (or the host's evaluator) |
| `AcceptanceEvaluationStale { reason }` | F1–F8 failed (`revision`, `run`, `mutation`, `source`, `policy`, `evidence`, `identity`, `verification_source`, `record_shape`) | re-evaluate against the current state; `source` uses the sibling source context below to select confirmation, measurement or new-evaluation guidance; `identity` needs a fresh evaluation recorded with every session and execution identity its mode requires, as a missing one would be requested for the task, and from a session that never held the run when the stale record was `independent_session`; `verification_source` needs the cited check run again on the current source; `record_shape` needs a fresh evaluation, since admission records only well-formed ones |
| `AcceptanceFailed { criterion }` | newest fresh evaluation has a `fail` | corrective work, then evaluate again |
| `AcceptanceInsufficientEvidence { criterion }` | newest fresh evaluation has `insufficient_evidence` | record the missing evidence, then evaluate again |
| `AcceptanceNeedsHuman { criterion }` | a criterion needs a human decision | obtain that decision; only a separately authorized revision (`update REF --accept …`) or cancellation changes the requirement, and a new evaluation follows the decision. No agent override exists. |

When `done` refuses with `AcceptanceEvaluationStale` because a source
observation after the newest evaluation's cut decided that the source moved,
the refusal names that observation beside the cause, never inside it. The
receipt's `code`, `recovery.cause`, its "not done" line and its stale
reminder keep their words. The reminder adds the same one-line sentence the
evaluate refusal adds, and the receipt's JSON, from the CLI and from MCP,
adds `recovery.deciding_observation` as `show` names it: each host-recorded
field cut at 128 bytes with its stored length, and the reporting session as
`show` labels sessions, so the receipt stays within the agent budget. A raw
storage recovery error keeps its message, which is the cause's words alone,
and adds `deciding_observation` to `error.details` beside `cause`, with the
fields whole. A stale
evaluation that no observation decided names none, nor does any other cause.
The CLI writes every JSON receipt, and a recovery error's JSON on stderr,
with the spaces of the locked-store phrase as `\u0020` escapes, so no field
spells it and every field still decodes to what was recorded; MCP returns the
structured values as recorded.

A stale `source` recovery also carries an optional sibling `source` context,
without changing `AcceptanceEvaluationStale { reason: Source }`, its raw
error message, or transport status. Core completion and agent `done` use
`recovery.source`; raw recovery errors add `error.details.source`. An ordinary
read exposes the same assessment as `acceptance_evaluation.source_recovery`.
The context names the minted evaluation id, run and evaluated cut. The deciding
snapshot supplies the root binding, workspace, declared/reported revisions and
expected/presented fingerprints only when available. Agent receipts bound each
host-recorded string at 128 bytes with its stored length; raw errors keep it
whole. CLI JSON keeps the locked-store phrase escaped without changing decoded
fields. No durable record, schema or acceptance predicate changes.

| Source mismatch | Remedy action | Safe next step |
| --- | --- | --- |
| `unconfirmed_declaration` | `end_turn_read_and_retry` | End the turn, read the same run again, then retry `done`. Matching host confirmation can preserve the judgment; the current snapshot makes no promise that another report will arrive. |
| `unconfirmed_evaluated_revision` | `read_source_and_evaluate` | Read the named root's current source and request a new evaluation. This defensive assessment fallback does not make an unsighted root admissible. |
| `completion_measurement_missing` | `measure_source_and_retry` | Obtain a fresh host measurement and present it at `done`; copying the evaluated fingerprint is not a measurement. |
| `completion_fingerprint_mismatch` | `evaluate_current_source` | Evaluate the current source with its host-measured source basis, then retry; copying an earlier fingerprint is insufficient. |
| `evaluation_source_basis_missing` | `evaluate_current_source` | Obtain a new evaluation with a host-measured source basis; a completion measurement alone cannot supply the missing evaluated basis. |

One service formatter selects both core recovery and word guidance from this
context. The word's stale-source prefix remains unchanged. `show` measures no
completion fingerprint: a record with a source basis remains pending that check,
not stale merely because the read is unmeasured. Under a freshness policy, a
record without a source basis remains stale even on a read. Revision, run,
root rebinding, policy, identity and movement still decide before source
confirmation; citation and gate freshness still decide before the completion
fingerprint. Agent `done` remains an owed receipt (CLI exit 2, MCP non-error);
a raw recovery error retains `work_completion_recovery_required`.

Admission refusals for eligibility, named-root source, citation and re-roll
checks carry `AcceptanceEvaluationAdmissionCause` beside their unchanged
reason text. CLI JSON and MCP use `acceptance_evaluation_refused`, with
`details.cause` tagged by `kind`: `eligibility`, `source_root`, `citation` or
`reroll`. The CLI still exits 1 and MCP still returns an error. The
host-private control transport retains `storage_error` for the first three
and `acceptance_evaluation_refused` for a re-roll; it does not gain an
evaluation operation.

The deciding guard supplies a typed mismatch and remedy action. Eligibility
context names the requested mode, current task mark and admitted modes,
adding the asserted evaluator, parent or mark author only when available.
Source-root context names the binding, workspace and evaluated cut, with
reported and declared revisions only when available. A declaration with no
initial root sighting reports `no_initial_sighting`; it asks the host to
capture the root before evaluating and makes no prediction about a future
sighting. Citation context names the criterion, submitted locator or record
id, active run and evaluated cut, adding the citation position, bound check
requirement, checked/judged revisions and producer when the deciding rule
knows them. `not_on_run` makes no claim about evidence on another run. When
the deciding fault is the verdict's basis, not a cited record,
(`observed_basis_required` on a bound criterion, `observed_policy_required`
under an observed mechanical policy), the citation is empty and no position is
given: none of the cited records was at fault, and a valid passed
verification is never named as the offender. The `read_run_evidence` remedy,
the one the basis faults, `not_on_run`, `passed_verification_required`,
`passing_gate_required` and `bound_verification_mismatch` give, states the
admissible pass for a bound criterion: basis `observed`, every citation a
passed host-minted verification of the bound kind, matching any pinned check,
cited by its full record id. `beyond_cut` and the wrong-source family keep
their own remedies.

Words and JSON format navigation and `details.remedy` from the same typed
context, without parsing the reason. The native CLI guards admission JSON on
stderr against the locked-store phrase while preserving every decoded field.
These causes are transient: no evaluation row, schema, admission predicate,
refusal precedence or completion rule changes. Typed basis movement and
carried-failure refusals retain their semantics. Re-roll and remaining
structural admission refusals retain their existing separate paths.

An item with no acceptance criteria has nothing an evaluation could judge, and
the host refuses to evaluate it. So under an evaluated policy `done` refuses
such an item before any recovery cause is built, with the error code
`acceptance_criteria_required`. The reason says the policy needs at least one
criterion and that the host will not evaluate without one. The remedy names,
in order, `engram work update REF --accept "criterion"`, the host evaluation,
and `engram work done REF`. The refusal captures nothing beyond the existing
pending attempt, and a retry meets the same refusal. Under a self-asserted
policy such an item completes as before.

The recovery command for the first two causes is runnable navigation,
`engram work show REF --notes --gates`: the criteria, notes, and gate
locators an evaluator reads before recording (`--full` stays a separate,
exclusive show mode). The remedy text names `evaluate`; no template
pre-fills a verdict, because the evaluation itself is judgment. The
mcp-dogfood suite parses and runs the emitted command through the real CLI
for both causes.

Evaluator unavailability or error leaves no record; completion refuses with
`MissingAcceptanceEvaluation`. There is no fallback to the self-asserted path.

Old seals without `acceptance_evaluation` remain valid and are read unchanged.
Every seal consumer applies one shared binding check: completion before the
seal is written, doctor, and the completed-item provenance read all require
that the bound evaluation exists as a canonical object, names the sealed work
and run, sits on the run feed at or before the completion cut, is the newest
evaluation on that feed at the cut (an older pass cannot be bound around a
newer record, whatever its verdict; anything appended after the cut is
outside the selection), passes every criterion, and derives exactly the
sealed acceptance vector. Doctor reports a failure as
`completion_seal:<id>:acceptance_evaluation_binding`. A policy
version whose authority decision disagrees with it on the
acceptance-evaluation settings does not load at all, so policy history, store
opening, and doctor (`control_policy_version:<id>`) refuse it alike, and
doctor decodes the audited `set_acceptance_evaluation` operation receipts like
the other policy operations.

## State transitions

Per open work item and its active run under an evaluated policy. `E` is the
newest evaluation on the run feed; "fresh" means F1–F8 hold.

| State | Condition | `done` | Leaves the state by |
| --- | --- | --- | --- |
| S0 unevaluated | no `E` for this run, or `E` not fresh | refuse `MissingAcceptanceEvaluation` / `AcceptanceEvaluationStale` | `evaluate` records a fresh `E` |
| S1 passing | `E` fresh, all verdicts `pass` | seal; binds `E` | any F1–F8 change → S0; a newer non-passing `E` → S2/S3/S4 |
| S2 failed | `E` fresh, some verdict `fail` | refuse `AcceptanceFailed` | corrective work → new `evaluate` → S1/S2/S3/S4; a revision → S0, carrying the failure (R11) when it changed the criteria `E` judged or their bindings |
| S3 insufficient | `E` fresh, some `insufficient_evidence`, none `fail` | refuse `AcceptanceInsufficientEvidence` | record evidence → new `evaluate`; a revision → S0, carrying the failure (R11) when it changed the criteria `E` judged or their bindings |
| S4 needs human | `E` fresh, some `needs_human`, none `fail`/`insufficient` | refuse `AcceptanceNeedsHuman` | a human decision, expressed as a separately authorized `update --accept` (revision → S0, carrying the failure, R11) or cancellation |
| L self-asserted | policy has no allowed modes | existing path; receipt says self-asserted | policy update |

`evaluate` itself refuses under R1–R11 without changing state. Precedence when a
mixed evaluation exists: `fail` before `insufficient_evidence` before
`needs_human`; the refusal names the first criterion in list order.

## Boundary matrix

Each row has an expected outcome independent of the production classifier;
tests cite the row identifier in a nearby comment.

| Row | Fixture | Expected |
| --- | --- | --- |
| B01 | self-asserted policy (no allowed modes); `done` without evaluation | seals as today; receipt names `self-asserted`; `evaluate` refuses "policy does not enable acceptance evaluation" |
| B02 | evaluated policy; no evaluation; `done` | refuse `MissingAcceptanceEvaluation` for criterion 1; command names `evaluate`; no seal, no capture side effects beyond the existing pending attempt |
| B03 | `same_session` allowed; evaluator = holder; all `pass` (judgment) | record accepted; `done` seals; seal binds the evaluation; receipt `evaluated (same_session, asserted)` |
| B04 | policy `[sub_agent, independent_session]`; same-session record | refuse at write; nothing appended |
| B05 | task mode `independent_session`; evaluator session = holder | refuse at write (independence) |
| B06 | `independent_session` from a distinct session; holder runs `done` | record accepted; seal |
| B07 | `sub_agent` with `execution_identity` and `parent_session` = holder, from the holder's own session / from a distinct child session | refuse at write, the executor's own evaluation (B63) / record accepted, recorded `asserted`; receipt does not claim verified independence |
| B08 | mode allowed by policy but different from the task's selected mode | refuse at write naming the selected mode |
| B09 | port-PR build criterion; `pass` + `observed` citing host-minted `VerificationEvidence(Build, passed)` on this run | accepted; seal |
| B10 | `pass` + `observed` citing `VerificationEvidence(Build, failed)` | refuse at write ("observed pass requires a passed check") |
| B11 | `fail` citing the failed build evidence | accepted; `done` refuses `AcceptanceFailed`; corrective work and a new passing evaluation then seal |
| B12 | `pass` + `asserted` citing a gate record with no failures | accepted under `mechanical_basis: asserted`; refused at write under `observed` |
| B13 | non-build natural-language criterion; `pass` + `judgment` with rationale and a note citation | accepted; seal |
| B14 | `pass` with empty citations, or empty rationale | refuse at write (no vacuous pass) |
| B15 | `insufficient_evidence` | accepted; `done` refuses `AcceptanceInsufficientEvidence`; after evidence and a new evaluation, seal |
| B16 | `needs_human` | accepted; `done` refuses `AcceptanceNeedsHuman`; `update --accept` revises → old evaluation stale (F1) and carried (R11); new evaluation seals, naming it with `--supersedes` from an evaluator that never held the run when the run's executor revised |
| B17 | citation from another run, another item, or a non-holder observation | refuse at write |
| B18 | verdict list missing a criterion, duplicate position, or `acceptance_basis` behind the current revision | refuse at write with "re-read show" |
| B19 | criteria revised after a passing evaluation | `done` refuses `AcceptanceEvaluationStale { revision }` |
| B20 | host-observed `source_changed` observation after the evaluation, or the newest execution observation after it that carries a revision shows another revision than the judged one, even when it reports no change | `done` refuses `AcceptanceEvaluationStale { mutation }` |
| B21 | `require_source_freshness`; `done` without fingerprint / with a different fingerprint / with the same fingerprint | refuse (missing) / refuse `AcceptanceEvaluationStale { source }` / seal |
| B22 | holder note, a gate the pass did not cite, and non-holder observation appended after a passing evaluation | still fresh; `done` seals |
| B23 | new run generation after reopen (a recovery claim keeps the run and its evaluation) | evaluation of the old run is absent for the new run; `done` refuses `MissingAcceptanceEvaluation`; the same payload records a fresh object on the new run rather than replaying |
| B24 | attempt identity: same key + same payload; same key + changed payload; new key + same payload; keyless identical resend; the same explicit key on another item | replay / refuse / new object / replay / a separate attempt |
| B25 | evaluated policy; `done` with explicit acceptance or `--link` | refuse with guidance naming `evaluate` (`work_criterion_link_invalid` for links); the run feed head does not move |
| B26 | evaluator failure modeled as no record | `done` refuses `MissingAcceptanceEvaluation`; no fallback |
| B27 | seals and policies recorded before this feature | read unchanged; doctor healthy; self-asserted receipts |
| B28 | `done "summary"` capture and its checkpoint after the evaluation | still fresh; seal binds the evaluation |
| B29 | `show` and `next` on an evaluated item | per-criterion newest verdict, basis, evaluator label, mode, and freshness; `done` receipt names the path |
| B30 | host-owned (not a core test): TermAl evaluator spawn, attested sub-agent identity, fingerprint at completion, observed build via the control channel | documented in the host item; end-to-end acceptance stays open until exercised |
| B31 | evaluator reads at cut `c`; a host-observed `source_changed` observation or check lands after `c`; the verdicts are submitted with `evidence_basis = c` | refuse at write: `acceptance_evaluation_resubmit` after a check, `acceptance_evaluation_void` after a source change the evaluation did not judge, or when the newest execution observation after `c` that carries a revision is at another revision than the judged one, even when it reports no change; a change to the declared judged revision does not refuse, nor does a passed host check on it with its environment record and resolution (B73); a citation beyond `c` also refuses; resubmission with the current basis records |
| B32 | passing `asserted` evaluation, then policy `mechanical_basis` → `observed`; passing evaluation without a source basis, then `require_source_freshness` → on | `done` refuses `AcceptanceEvaluationStale { policy }` / `{ source }`; no older, weaker record seals |
| B33 | `pass` cites gate `cargo-test`; a newer `cargo-test` record (any result) lands after the cut; an unrelated gate lands after another passing evaluation | `done` refuses `AcceptanceEvaluationStale { evidence }` / the unrelated gate leaves the evaluation fresh |
| B34 | passing evaluation followed by a newer `fail`, `insufficient_evidence`, or `needs_human` record | `done` refuses with the newer verdict's cause; the older pass is never selected |
| B35 | `independent_session` pass, then the evaluator takes the run by handoff or by recovery and runs `done`; a never-holding session then evaluates; the original holder completes on an independent pass; the evaluator holds a different run | refuse `AcceptanceEvaluationStale { identity }` / seal / seal / independent, seal |
| B36 | seal re-frozen to bind another run's evaluation, with its completion event and run projection re-frozen too | doctor reports `completion_seal:<id>:acceptance_evaluation_binding`; an evaluated completion is healthy end to end before the forgery |
| B37 | policy version re-frozen to disagree with its authority decision on `acceptance_evaluation` | the version does not load ("authority is invalid"); doctor reports `control_policy_version:<id>` and leaves the audited operation receipt healthy |
| B38 | evaluator model segment blank, over 128 bytes, or with a control character, submitted to the core | refuse at write; run feed unchanged |
| B39 | `set-acceptance-evaluation` with an empty mode list and non-default other fields, from self-asserted and from an evaluated policy | normalizes to the self-asserted policy; requested, stored (bytes omit the field), and read policies agree; `changed: false` when already self-asserted |
| B40 | `work_run_evidence.verification_result` disagrees with the canonical `VerificationEvidence` | `evaluate` refuses with an invalid-projection error; nothing is admitted from the column |
| B41 | citations as `show --notes --gates` prints them: a gate and a holder note together (judgment); a non-holder observation; another item's note; an artifact path | accepted, the record keeps the full ids / refused naming the locator and "observation" / refused ("not a note/gate on this item") / refused ("not the recorded evidence identity") |
| B42 | `require_source_freshness`; `show` after an evaluation with a fingerprint; `done` without one | `show` reports the fingerprint as checked at `done`, not stale; `done` refuses `source` and the remedy names `--source-fingerprint` |
| B43 | self-asserted and evaluated completions read through `done`, completed `show`, and `next --peek` on the focused evaluated item | `acceptance: self-asserted` in `done` and `show` text with `provenance: self_asserted` in both `done` and completed `show` JSON / `acceptance: evaluated (same_session, asserted) by <evaluator>` with the JSON `acceptance` block in both; `next` prints `evaluation: <mode> P/N pass, fresh` under the focus |
| B44 | `update --evaluation-mode same_session`, then `--clear-evaluation-mode`, then a clear on the already unpinned item | `show` history and a peer's `next` deltas carry two `revised` entries naming `evaluation mode` and one reading `no planning change` |
| B45 | `add` with an empty or whitespace `--evaluation-mode` for a root and for a `--under` child; omitted; a valid word | refused before any effect (no item, project feed and focus unchanged) / created without a pin / created pinned |
| B46 | completed evaluated item whose bound evaluation object no longer decodes | `show` still reads: text `acceptance: provenance unavailable (…)` with `diagnostic class: <class>`, JSON `acceptance: {provenance: unavailable, error_class}`; `doctor` reports the store unhealthy; a self-asserted completed `show` returns `acceptance: {provenance: self_asserted}` |
| B47 | direct core completion under an evaluated policy whose evidence set omits a cited object / carries it; the service `done` with a narrower explicit evidence set while the fresh pass cites a note and host-minted verification evidence | refused ("cites evidence outside the completion evidence set"), item stays open / seals with the citation in `seal.evidence`; the service unions the citations, so the seal names them and the completion checkpoint acknowledges them |
| B48 | seal re-frozen to bind an older pass while a newer `fail` or `needs_human` evaluation sits before the completion cut; the unforged seal; the bounded newest read with a cut before the newest record | doctor reports `completion_seal:<id>:acceptance_evaluation_binding`, the shared check refuses ("is not the newest evaluation … at the completion cut"), `show` reads with `provenance: unavailable` / healthy / returns the older record, excluding anything after the cut |
| B49 | MCP `update` with action `revise` and a supplied `evaluation_mode` (valid or blank); action `evaluation_mode` with a word, then omitted | `invalid_argument` on `evaluation_mode` before any effect, item, feed, and focus unchanged / pinned, then cleared, both named in history |
| B50 | criterion bound to a test; a passed check at R_d, a source edit to R_e with no check after it; a pass citing the R_d check, undeclared or declaring R_e, with source freshness off and on; the same record as an earlier build admitted it, with the binding's obligation satisfied or waived, and on the waived path also declaring R_d; its exact resend; the check rerun at R_e and cited | refuse at write naming the citation and both revisions, nothing appended / `done` refuses `AcceptanceEvaluationStale { verification_source }` on every path, even with the matching completion fingerprint / the resend replays the admitted record, and a changed resend is refused / records and seals |
| B51 | the same binding; a declared source ahead of the newest sighting at the cut; a declared workspace other than the check's; a declaration that matches the check exactly; after an edit to R_e, a declaration of R_d behind it; a quiet sighting at R_e before the cut, then one back at R_d; a reported change without a revision after the check; an R_d citation beside a fresh R_e check, cited or not; after the check, sightings of its own revision (the turn's closing one, one from another workspace); after the check, reported changes to R_e and back to R_d, both kept as changes; a check recorded in a later turn citing its earlier producer, with a sighting of its revision in between | refused naming both revisions, and the remedy names correcting the declaration, which an undeclared refusal does not / refused naming both workspaces / records / refused naming the later revision / refused as judged at R_e, then records / refused naming the change without a revision / refused naming the R_d check; the fresh check alone records / records and seals / records and stays fresh, while `done` asks for a check after the latest change under the satisfied binding's own rule / records |
| B52 | evaluated policy; an item with no acceptance criteria, as a migration imports one; `done`, then the same `done` again; then a criterion added, `done`, an evaluation, `done`; the same item under a self-asserted policy | refuse `acceptance_criteria_required` naming the missing criteria, the host's refusal to evaluate without them, and the remedy in order, with nothing captured beyond the pending attempt, never `work_projection_invalid` / the retry is refused the same way / `MissingAcceptanceEvaluation`, then seals / seals as before |
| B53 | `fail`, then the run's executor revises the criteria it judged; `evaluate` without `--supersedes`, naming another record, then naming the failed record from an evaluator that never held the run; `done` | `show` discloses `carried_failure` (`revised_by: executor`, `supersedes_required: true`); refuse at write `acceptance_evaluation_refused` with details `reason: carried_failure_unacknowledged`, `failed_evaluation`, and a remedy naming `--supersedes RECORD_ID`, nothing appended / the same / records with `supersedes`; `done` seals, and the bound evaluation names the failure |
| B54 | `fail`, then a planner (a session that never held the run) revises its criteria while the claim is released; or the executor revises and a planner revises again | `carried_failure` shows `revised_by: planner` and `evaluate` records without `supersedes` / `revised_by: executor`, and `evaluate` without `supersedes` is refused |
| B55 | `--supersedes` before any evaluation, after a passing one, after a failing one whose item was only retitled or blocked and unblocked, or after a rewording back to the judged criteria | refuse at write with `reason: nothing_to_supersede`; nothing is carried, so `evaluate` without `supersedes` records as before |
| B56 | `fail` on a criterion bound to a test, then the executor drops the binding (or `update --accept` repeats the same wording without `--bind`) or binds another kind | the failure is carried although the criterion's text is unchanged; `evaluate` without `--supersedes` is refused `carried_failure_unacknowledged` |
| B57 | `fail`, the run's executor revises its criteria; the failed record is named by a same_session evaluation, by a sub_agent that shares the executor's or a former holder's session, or by an independent_session evaluator or a sub_agent under its own session | refuse at write `acceptance_evaluation_refused` with `reason: carried_failure_self_acknowledged`, `failed_evaluation`, and a remedy naming an independent_session evaluator, a sub_agent under its own host-issued session, widening the policy, changing or clearing the task pin, or, while no later failing evaluation has named it, revising back; nothing appended / the same / the same / records with `supersedes` |
| B58 | `fail`, the run's executor revises its criteria, and an evaluator that never held the run names the failure with `fail`; the executor revises again; another such evaluation names it with `needs_human`; or a planner revises in between and the executor rewords back to the original criteria | the failure is still carried as the original record (`carried_failure.evaluation` unchanged, `revised_by: executor`, `judged_criteria` the original ones, and `newest_judged_bindings` those the naming evaluation judged); a revert to the original criteria after a failing evaluation named it stays carried, and so does a second failing review of the reverted criteria; the executor's `same_session` pass without `--supersedes` is refused `carried_failure_unacknowledged`, and naming it `carried_failure_self_acknowledged`; only a passing evaluation from such an evaluator that names it ends the carry, and `done` seals |
| B59 | a task with no mode set, a policy admitting same_session and another mode; the holder evaluates same_session / a policy admitting only same_session | refuse at write: not marked for same-session, request an independent evaluation from the host, same-session needs a mark by someone other than the executor; nothing appended / records |
| B60 | a task marked same_session at creation by the session that later executes it, or marked by the holder after a failed independent evaluation; the holder evaluates same_session, in any policy | refuse at write: the mark was set by a session that evaluates, holds or executes the run; clearing the mark leaves an unmarked task, refused as B59 |
| B61 | a task marked same_session by a peer or operator; the holder revises other fields, then evaluates same_session | records; the mark keeps its author; `done` seals with `evaluated (same_session, …)` |
| B62 | B61, then the holder releases and the mark's author claims the run | `done` refuses `acceptance_evaluation_stale` (policy); the author's own same_session evaluation refuses at write |
| B63 | policy admits `sub_agent`; a `sub_agent` evaluation recorded from the holder's own session / from a distinct child session under the holder; then that child session claims the run | refuse at write: recorded from a session that holds, executes or held the run / records, and `done` seals / `done` refuses `acceptance_evaluation_stale` (policy) |
| B64 | a child marked same_session by the session that later executes it; a peer detaches it after its parent ends; the executor claims the successor and evaluates same_session / a peer then clears the mark and sets it again | refuse at write: the mark has no author recorded on the successor / records |
| B65 | a `fail`; then an evaluation cut at the same position, at a cut advanced only by the failing record, or by a checkpoint; a failing re-roll; one cut before a later gate / after it | refuse at write ("nothing that could change it was recorded"), nothing appended / records; a later evaluation after that pass needs nothing new |
| B66 | a `fail`; the host reports a change at the revision already judged / to another revision | refuse at write / records |
| B67 | an independent `fail`; a title edit, a mode edit and a claim renewal, each followed by an independent pass; then a correction note and a pass | each refused at write; the last records and `done` seals on it |
| B68 | under a named root, a `fail`; a flagged change reported in another workspace / a change inside the root; then an evaluation | refuse at write / records |
| B69 | a `fail` declaring the revision it judged ahead of the host's report; the host then reports the change to that revision / to another revision | refuse at write / records |
| B70 | a newest `insufficient_evidence`, then a newest `needs_human`, each re-rolled without evidence; a policy edit after the `needs_human` | each refused at write |
| B71 | a `fail`; a quiet sighting at another revision; then a flagged change to that revision; then a flagged change with no revision | refused / refused / records |
| B72 | an unmarked task's own same_session pass sealed under a same-session-only policy; the policy then admits `independent_session` | a new such evaluation refuses; the seal still validates (doctor healthy) and `show` reads it unchanged |
| B73 | the requesting turn changes the source to R2, runs a passing test there and asks; the evaluator declares R2 at cut `c` and submits before the turn's report lands / after it, on `c` | records; the report (the change, its obligation, the environment, the passed test and its resolution) leaves it fresh; `done` seals on it and doctor stays healthy |
| B74 | as B73, but the report carries a failed test beside the passed one, in either order, or an indeterminate check | the evaluation is stale (`mutation`); a submission on `c` refuses `acceptance_evaluation_resubmit` |
| B75 | an evaluation that declared no revision / declared another workspace; a passed host check after its cut | stale (`mutation`), as before |
| B76 | a `fail` declaring R2 at `c`; the turn's report with a passed test on R2; a replacement on `c` / on a cut that includes the test | refused as a re-roll / records |
| B77 | a named root sighted at R1; the requesting turn changes it to R2 and tests there; the evaluator declares R2 at cut `c` before the report / a declaration of R3 on `c` after it | records, stale (`source`) until the host sights the root at R2, then fresh; `done` seals and doctor stays healthy / refused (void) |
| B78 | as B77, but the declared revision is never sighted | `done` refuses stale (`source`); an evaluation of the revision the host reports replaces it and seals |
| B79 | a named root sighted at R1; the turn's report sights it at R2 with a passed test; then the evaluator submits R2 on its earlier cut / a passed check on the root's revision reported from another workspace after an evaluation | records, fresh, `done` seals / stale (`mutation`): a check off the root is not exempt |
| B80 | a passed check verified late for an earlier run of the declared revision, after the run moved on to another revision | stale (`mutation`): a check is exempt only while the run was last sighted at the declared revision |
| B81 | an evaluation declaring the revision the run is at; a passed check of an earlier revision verified late, after its cut | stale (`mutation`): the check did not run on the declared revision, although the run's newest sighting is there |

## Agent surface

The evaluate word is a deliberate extension of the agent vocabulary; the
counts in every contract file move with it.

```text
engram work evaluate [REF] --mode MODE --acceptance-basis N --evidence-basis M \
  --verdict POSITION=VERDICT[:BASIS] --rationale POSITION=TEXT \
  [--evidence POSITION=LOCATOR]... [--attempt KEY] [--source-fingerprint F] \
  [--model PROVIDER/MODEL] [--execution-identity ID --parent-session SESSION] \
  [--supersedes RECORD_ID]
```

`POSITION` is the one-based criterion position, `--acceptance-basis` the
revision from `show` (exactly as `done --link`), `--evidence-basis` the
run-feed position `show` prints beside it under an evaluated policy, which the
evaluator read through, and `LOCATOR` a note/gate locator from `show --notes
--gates` or the full id of host-minted verification or environment evidence
(R7). `--source-fingerprint F` declares the host's source revision the
evaluator judged (F3); a value in another form, such as a Git commit id, voids
the evaluation at the host's next sighting of the source, and a pass on a
bound criterion declared that way is refused at once, since no check ran at
that revision (R5). `--supersedes RECORD_ID` names the carried failing
evaluation (R11), by the id `show` prints in
`acceptance_evaluation.carried_failure` (`evaluation`, `revised_by`,
`judged_revision`, `failing`, `supersedes_required`); `show --full` adds the
criteria it judged as `judged_criteria`, its non-passing verdicts as
`blocking: [{criterion, verdict, rationale}]` and the bindings its criteria
had as `judged_bindings`, prints those criteria with the failed verdicts and,
when a later failing evaluation named it, the newest evaluation's criteria
and, as `newest_judged_bindings`, its bindings, gives the current ones as
`work.acceptance_bindings` (for any item under any policy, omitted when there
are none), and prints a `judged bindings:` line; both reads give a record's
own `supersedes`. Open items in self-asserted projects keep their unchanged
`show` shape. MCP `evaluate` takes the same data as `mode`,
`acceptance_basis`, `evidence_basis`, `verdicts: [{criterion, verdict, basis,
rationale, evidence: [locator]}]`, and the optional fields. The receipt is a
bounded projection of the immutable record, not the record: the evaluation id,
mode, revision, run, evaluated cut, `passed`, the first blocking verdict with
its criterion compacted, `verdicts_total`, a prefix of verdict rows (position,
verdict, basis, citation count), `verdicts_omitted`, which counts exactly the
rows left out to fit the agent budget, and the attempt key exactly as
recorded. The core measures that response before the record is written, so an
admitted evaluation never commits into a failed response. Ordinary `show`
prints the same bounded prefix with the omitted count, the evaluator label,
and the recorded source fingerprint (marked as checked at `done` when the
policy requires freshness); `next` prints one `evaluation: <mode> P/N pass,
fresh|stale: R` line under the focused evaluated item (`focus.evaluation` in
JSON); `show REF --full`, the authored-contract read that may exceed 12 KiB,
returns the complete newest evaluation with every verdict's full rationale and
citations plus its freshness.

Two explicit reads cover older records; nothing else about admission,
staleness or sealing changes.
- **The history window.** `show REF --evaluations [--after CURSOR]` (MCP
  `evaluations: true`, `after`) lists every evaluation record of the item's
  active run, or of its latest run once none is active, as for a completed
  item. Records are selected newest first and shown in run-feed order, within
  the 12 KiB agent budget. Each row gives:
  - the record id and run position;
  - the mode, and the evaluator's session as the display label `show` uses
    for holders and note authors, so two records from one session, and the
    holder, compare equal (absent when the record names no session);
  - the attempt key, created time and work revision;
  - verdict words by criterion position, up to a bound, with the exact count
    of omitted verdicts;
  - the record's stale reason at the read cut when it has one, the record it
    supersedes when it names one, and whether it is the newest.

  The window gives exact total, shown, omitted, older and newer counts and a
  continuation. The cursor is bound to the item, run, the run's feed head, the
  item's revision and the acceptance policy; a cursor after any new record on
  the run, a revision, another run, policy or window kind is refused, while a
  write elsewhere in the project leaves it valid. The title is compacted as
  `show` does, with its stored length and `--full` offered when it is longer.

  While the run is the item's active run, a record's stale reason is judged as
  the newest record's is, under the current policy with the source
  unmeasured. An older record is therefore not called stale merely because a
  later one exists. Once the run has ended (completion, cancellation or
  supersession), its records are listed but not judged (`stale_judged:
  false`). Ending the run revised the item, so judging against it would call
  every record stale, including the one a seal consumed.
- **One record complete.** `show REF --evaluation RECORD_ID` (MCP
  `evaluation`) returns one record of the item, from any of its runs, with
  every verdict's criterion, full rationale and citations. It also gives its
  whole attempt key, and its evaluator model, execution identity,
  parent-session label and judged source fingerprint when recorded. While the
  item has an active run, the record is judged against it, so a record of an
  earlier run is stale for that reason. Like `--full`, it may exceed 12 KiB.

When a record reads stale because a source observation after its cut decided
that the source moved (stale `mutation`, the word unchanged), plain `show`,
`show --full`, the evaluations window row and the record's detail name that
observation the same way, one line and a `stale_observation` field, beside
the evaluated revision, with each host-recorded field cut at 128 bytes and its
stored length, so that the surface stays within its budget; `show
--observations` lists the fields whole. A record stale for another cause names
none.

- **The run's source observations.** `show REF --observations [--after
  CURSOR]` (MCP `observations: true`, `after`) lists every execution
  observation of the item's active run, or of its latest run once none is
  active, those on other workspaces included: run-feed position, record id,
  whether it reported a change, workspace, revision and root generation when
  recorded, reporting session as the display label `show` uses, and the
  observed and recorded times. Each row says its `admission`: `admitted`
  for a turn's own observation, `unadmitted` for one a host recorded without
  admission ([Record execution observed without
  admission](behavioral-control-plane.md#5b-record-execution-observed-without-admission)).
  An unadmitted row adds what was observed and its window, its cause as
  unknown or as the host's unverified assertion, its accounting, and every
  check as `observed check, uncredited`. Every host-supplied value in an
  unadmitted row is shown to at most 64 bytes once escaped, ending with its
  stored length when shortened, so any record fits; its observed time is the window's
  end, and a row that reports no source change carries no `source_changed`
  value. Rows are selected newest first and shown in run-feed order within the
  12 KiB agent budget, with exact counts and a continuation bound to the item,
  run, the window's total and boundary, which a new source record on the run
  changes and a write elsewhere in the project does not. It is exclusive of
  the other windows, and a read that records nothing.

The newest whole record still decides completion. `show` and `show --full`
keep their newest-only reads.

`done [--source-fingerprint F]` presents the
host-measured fingerprint (F4); its refusals carry the causes above, and its
success line and the completed item's `show` carry the provenance (completion
enforcement, step 5). `add --evaluation-mode MODE` pins the mode from creation
(roots and `--under` children alike), and `update` accepts `--evaluation-mode
MODE` and `--clear-evaluation-mode` as one audited revision (MCP action
`evaluation_mode`). A supplied blank or whitespace mode refuses at `add` and
`update` alike through one shared parser; only omission (no pin at creation,
the explicit clear on update) leaves the item unpinned. A pin or clear is a
planning revision that `show` history and peers' `next` deltas name as
`evaluation mode`; clearing an already unpinned item reads as `no planning
change`. A graph snapshot carries the pinned mode and restores it verbatim, so
a transfer never widens which evaluator may accept a task.

A host selects the evaluator from two reads. `show --json` carries the task's
pin as `status.work.evaluation_mode`, omitted when the task pins nothing; the
text form prints the same fact as `evaluation mode:`. `control-policy show`
prints the active policy as JSON, with `acceptance_evaluation`
(`allowed_modes`, `mechanical_basis`, `require_source_freshness`) beside the
policy id, epoch, required assurance, rule set and supported effects. It
reads the policy head only, so a host can ask it on every evaluation request;
`doctor --json` carries the same keys under `control` and the text report one
`Acceptance evaluation:` line, but `doctor` audits the whole store and can
take minutes on a large one. An empty `allowed_modes` is the self-asserted
path: no evaluator is needed.

Scoped exceptions to the terse agent surface: receipts expose the evaluation
id, the run id, and the evaluated cut as opaque correlation identifiers for
hosts that track evaluator attempts; they grant no authority and imply no
identity assurance beyond the recorded one.

Operators enable the feature with
`engram control-policy set-acceptance-evaluation --modes M[,M] --mechanical-basis asserted|observed [--require-source-freshness] --authorized-by ACTOR --idempotency-key KEY`;
an empty mode list restores the self-asserted path through the same audited
transition.
Changing this policy does not require justification text. The operator,
selected policy, compare-and-swap basis and retry key remain explicit;
evaluation verdict rationales are a separate contract and remain required.

## Host integration

Before requesting an evaluation, read the held item's `next` or `show` and
settle open obligations that require a credited check or an authorized
waiver. The agent-safe `show --json` field `evaluation_obligations` carries the
run-feed `read_cut`, exact `open_total`, exact `omitted_open`, exact
`action_required_total` computed over every open obligation, and a bounded
list of distinguishing labels, check kinds, actual remedies, and whether each
visible obligation requires action before evaluation. The host reads that
field from the same `show --notes --gates --json` request it uses for the
evaluator brief. Compact `next` carries the same typed advisory and adjusts
`omitted_open` when byte fitting removes visible rows. The post-write
`evaluate` receipt omits `read_cut` because its advisory is read after the
evaluation record is appended; its counts and warning describe that page.
Full obligation identities and waiver authority stay on the
host-only work view. A stock source-change obligation that `done` can waive
itself says that no action is needed before evaluation; its waiver inside
completion does not void the evaluation. An independently recorded check or
waiver after the evaluator's evidence basis does make it stale (F3), except a
check that passed on the revision the evaluation declared, with its own
environment record and the resolution it made (R3b). Therefore the order is:
settle required obligations, gather credited checks and evidence (a check a
bound pass cites must be recorded before the request), evaluate against a
fresh read cut, then complete. `evaluate` still records
while obligations remain open; its receipt repeats the warning and counts,
even in the minimal response. A bounded page that omits open obligations
names their exact count and never implies the visible list is complete.
When a fresh pass exists and the remaining obligations need no earlier action,
the guidance directs the holder to `done` without requesting another evaluation.

The host (TermAl) owns everything that involves a model, a workspace, or a
process:

- selecting the mode for a task and spawning the evaluator: same-session as an
  explicit continuation, sub-agent under the executing session with a distinct
  execution identity, or a separate session that may use another provider;
- giving the evaluator the task outcome, criteria, evidence index, and
  read-only workspace access at the executor's revision, with the evaluator's
  budget and permissions;
- submitting the evaluation through `evaluate` under the evaluator's own
  attributed identity;
- handing over a carried failure (R11): when `show --json` carries
  `acceptance_evaluation.carried_failure`, giving the evaluator the failed
  verdicts and the criteria and their bindings before and after the revision
  (`show --full`: `carried_failure.judged_criteria`, its `blocking` verdicts
  with their rationale and `carried_failure.judged_bindings` before, the
  newest evaluation's per-verdict `criterion` and
  `carried_failure.newest_judged_bindings` when a later failing evaluation
  named it, and `work.acceptance` and `work.acceptance_bindings` after),
  asking it whether the revised criteria still deliver the requested outcome,
  and passing `supersedes` with the failed record's id when
  `supersedes_required` is true, or when a planner revised and the evaluator
  judged the failure; without it, an evaluation after an executor's revision
  is refused `carried_failure_unacknowledged`. When `supersedes_required` is
  true the host never selects same_session: an independent_session evaluator,
  or a sub_agent under its own session with a brief the host composes, names
  the failure, and a same_session-only pin or policy surfaces the
  `carried_failure_self_acknowledged` remedy. A superseding evaluation that
  does not pass leaves the failure carried, so the host keeps choosing a
  distinct evaluator until one passes;
- measuring the source fingerprint at evaluation time and again at completion
  time, binding it to the exact attempt rather than reusing the evaluator's
  earlier string;
- giving every evaluator attempt its own identity (an explicit `--attempt`
  key) and treating an attempt that was started but has not recorded as
  pending: the host must not complete on an older pass while a newer attempt
  it requested is still running or has timed out, because Engram only sees
  recorded evaluations and the newest recorded one decides;
- minting observed build evidence through the control channel
  (`turn_checkpoint` with `verification_evidence`), which is the only way an
  `observed` pass can exist. The bootstrap policy requires `turn_gated`
  assurance, so this needs the host channel for the pilot project; lowering the
  assurance to dodge that prerequisite is not the plan.

### Reading what satisfied a bound criterion

A host reads, for one item on its active run, the evidence that satisfied
each bound criterion with the host-private `acceptance_binding_read`
operation on the [host control channel](../spec.md#83-host-control-channel).
Its request carries `routing_token`, the full `work_id`, the
`expected_work_revision`, the full `run_id` and, for a later page, `after`.
It takes no cut, idempotency key, fence or note text; a request naming any
other field, such as `run_cut`, is refused as `invalid_request`. Any bound
host session of the project may read, whatever its own work binding.

The first page captures the run's feed head as its cut. Each page then
reads one snapshot and checks, in order: that a continuation is one this
read issued and was issued for this project, item, revision and run; that
the item exists, belongs to the project and is at the expected revision;
that the run is the item's active run; and that a continuation's pinned cut
is still the run's head. A page answers with:

- `basis`: `project_id`, `work_id`, `work_revision`, `run_id` and
  `run_cut`, the cut every page of the read is pinned to;
- `total`, the item's authored criteria; `earlier`, the rows on earlier
  pages; `shown`, the rows on this page; and `omitted`, the rows after it;
- `rows`: complete rows in ascending one-based criterion order, at most
  eight to a page and at most 16 KiB of result;
- `continuation`: `null` when no row remains, otherwise an opaque token for
  the next page.

Each row names its `criterion` and its `binding`, `null` for a criterion
bound to no verification. A bound criterion's binding carries its
`requirement` and the `obligation` that answers for it: the newest one the
run opened for that criterion and requirement, as completion selects it.
`obligation` is `null` when the run's feed holds no obligation for the
binding, which says nothing of a waiver or a pass. Creation, claim and
revision open one for every binding of an item with an active run, so in a
healthy store this marks a record never opened, not one lost: an obligation
or resolution on the run's feed without its projection row is a damaged
store, and the read returns a storage error rather than `null` or an older
obligation. Completion refuses the same damage the same way, so a lost row
never lets a criterion seal without its obligation or on an older one; reads
that only show the item still render it, and doctor names the damage. An
obligation carries its `obligation_id`, its record id
as `definition`, the `work_revision` that opened it, its `rule`, its
`triggering_observation`, its `trigger_position` and `definition_position`,
its `state` at the cut, and its `resolution`, `null` exactly while it is
open. A revision that leaves the criterion and its binding unchanged keeps
the older obligation; one that rewrites the criterion, changes its
requirement or drops and re-adds the binding opens a new one.

A resolution carries its `record`, its `position` and its `kind`:
`satisfied`, `waived` or `displaced`. A waiver's reason and authority are
not part of this read. A satisfied resolution's `satisfaction` names the
`evaluated_cut` it was judged at and the `verification` that closed it: its
full `record` id, `position`, `check_kind`, `check_fingerprint`, `result`
and complete `source_basis`, and its `producer` observation's `record`,
`position` and `outcome`. Every position is on the basis run's feed and at
or before the cut.

This is the record as stored, not an assessment. A satisfied obligation
names the check that closed it even after a newer check failed or the
source moved; the read neither looks for the newest applicable check nor
judges freshness or pass admission. A host that also reads the latest checks
does so separately, validating that read against the same basis or starting
again. Prose in notes or evidence never associates a criterion with a
record.

The read writes nothing, so a host may repeat it freely. Each refusal has
its own code, and none is an empty page:

- `acceptance_binding_read_unknown_work`: the store holds no such item;
- `acceptance_binding_read_wrong_project`: the item is another project's;
- `acceptance_binding_read_wrong_revision`: the item is at another revision;
- `acceptance_binding_read_wrong_run`: the run is not the item's active run;
- `acceptance_binding_read_stale_cut`: the run's feed moved past a
  continuation's cut; read again from the first page;
- `acceptance_binding_read_invalid_cursor`: the continuation is not one
  this read issued;
- `acceptance_binding_read_cursor_basis_mismatch`: the continuation was
  issued for another item, revision or run;
- `acceptance_binding_read_page_too_large`: one complete row does not fit a
  page; rows are never clipped.

An append to another item does not move
the basis; a revision of the item or any append to its run does, so a host
paging a run that is still recording may be refused `stale_cut` again and
again, and reads from the first page each time, or once the run is quiet.
Wrong credentials keep their codes, and a record that fails its canonical
association returns a storage error.

### Listing the candidate verifications of a criterion

Beside the binding read, a host lists, for one criterion of an item on its
active run, every host verification of the criterion's bound kind that the
run recorded up to the binding read's cut, with the host-private
`acceptance_verification_read` operation. The binding read names the check
that closed an obligation; this read lists every check the consumer may
weigh, and judging which of them applies is the consumer's.

Its request carries `routing_token`, the full `work_id`, the
`expected_work_revision`, the full `run_id`, the `run_cut`, the one-based
`criterion` and, for a later page, `after`. The project is the bound
session's; a request cannot name a project, a kind or a fingerprint, and any
other field is refused as `invalid_request`, as is a criterion that is not a
non-negative integer. Any bound host session of the project may read.

Unlike the binding read, this read never captures a cut: `run_cut` must be
the run's feed head on the first page and on every continuation, so a host
reads it at the cut of the binding read it pairs with. Each page reads one
snapshot and checks, in order: that a continuation is well formed; that it
was made for this project, item, revision, run, cut and criterion; that the
item exists,
belongs to the project and is at the expected revision; that the run is the
item's active run; that `run_cut` is the run's head; that the criterion is
one of the item's authored criteria; that a continuation names the
criterion's current requirement; and that a continuation's boundary is the
candidate at its rank in this snapshot. A page answers with:

- `basis`: `project_id`, `work_id`, `work_revision`, `run_id` and
  `run_cut`, as the binding read reports them;
- `criterion`, and its bound `requirement`, `null` for an unbound criterion;
- `total`, every candidate at the cut; `earlier`, `shown` and `omitted`, the
  candidates on earlier pages, on this page and after it;
- `rows`: complete candidates in ascending run-feed position, at most eight
  to a page and at most 16 KiB of result;
- `continuation`: `null` when no candidate remains, otherwise a token for
  the next page.

A candidate is every verification on the run's feed at or before the cut
whose `check_kind` is the requirement's: passed, failed and indeterminate;
any fingerprint, including one other than a fingerprint the requirement
pins; any source revision; and checks recorded before the criterion's latest
obligation or at an older revision of the item on this run. Two checks with
equal fingerprints are two candidates. Criteria of one kind list the same
candidates, each with its own continuation. Each row has the shape of the
binding read's satisfying `verification`: its full `record` id, `position`,
`check_kind`, `check_fingerprint`, `result` and complete `source_basis`,
and its `producer` observation's `record`, `position` and `outcome`, with
the producer before the verification and both at or before the cut.

An unbound criterion answers with a `null` requirement and zero counts, and
a bound one without candidates with its requirement and zero counts.
Neither is a pass. The read computes no freshness, applicability, source
currency or satisfaction: it lists records as stored.

The run's feed is what the read enumerates, so a damaged store returns a
storage error, never a shorter list. That covers a verification the run's
evidence projection holds without its feed entry, one without its
projection row or disagreeing with it or with its producer, and a
verification or producer naming another project, item, root execution or
run. It covers a producer whose feed entry is missing, records another kind
of object, or does not lie after the feed's start and before its
verification.

A continuation is a validated position, not a capability: it is checked
against the current snapshot, and a token that passes every check is a
read under the current credentials. It carries the basis, the criterion,
the requirement, the count and the last candidate returned. The read
writes nothing. Each refusal has its own code, and none is an empty page:

- `acceptance_verification_read_unknown_work`: the store holds no such item;
- `acceptance_verification_read_wrong_project`: the item is another
  project's;
- `acceptance_verification_read_wrong_revision`: the item is at another
  revision;
- `acceptance_verification_read_wrong_run`: the run is not the item's active
  run;
- `acceptance_verification_read_stale_cut`: the run's feed is not at
  `run_cut`, on a first page or a continuation; read the bindings and the
  candidates again;
- `acceptance_verification_read_invalid_criterion`: the criterion is not one
  of the item's authored criteria;
- `acceptance_verification_read_invalid_cursor`: the continuation is not a
  position this read can resume at, including one from the binding read or
  one whose boundary or count does not match the snapshot;
- `acceptance_verification_read_cursor_basis_mismatch`: the continuation was
  made for another item, revision, run, cut, criterion or requirement;
- `acceptance_verification_read_page_too_large`: one complete candidate does
  not fit a page; rows are never clipped.

Any append to the run moves its head, so a host paging a run that is still
recording may be refused `stale_cut` again and again; an append to another
item does not. Wrong credentials keep their codes.

### Evaluation unit and re-evaluation

The evaluation unit is **one item's run at one work revision and its judged
source revision, if known**. When the newest evaluation remains non-pass or
stale under [Freshness](#freshness) and a new acceptance judgment is needed,
the host obtains a whole new record; it never patches individual verdicts.
Every verdict in the new record is a fresh judgment of all evidence up to
its cut. Earlier rationale may be quoted, but never replaces that judgment;
the core never stitches per-criterion verdicts across records. A blocking
newest record is replaced only on new evidence
([A failure stands until new evidence](#independent-by-default)).

The host may ask for that judgment in the **same independent evaluator
session**, including when only evidence has changed, while current policy and
task pin permit `independent_session`, the session has never held or executed
the item's run (R1/R4/F5/F7), and it has received no requester follow-up.
This is reuse within `independent_session`, not Engram's `same_session` mode.
The previous attempt is settled before a fresh `--attempt` key is minted
(R8 and the pending-attempt rule above). For each new judgment, the host
re-reads the inputs above, including current criteria, bases, source revision,
policy, task pin, carried failure and open obligations, then gives the
evaluator a refreshed host-authored brief.

When the source moves from the judged revision, the evaluator judges every
criterion afresh against the whole new revision. The holder reruns bound checks
there; a pass on a bound criterion cites its rerun (R5/F8). An R3b
`acceptance_evaluation_resubmit` refusal leads to a fresh judgment after
re-reading the evidence, under a new key and without silently advancing the
cut; `acceptance_evaluation_void` from a source
move likewise requires a whole new inspection. A carried failure is
acknowledged with `--supersedes` (R11).

One evaluator may serve several items delivered together, but each keeps its
own eligibility, attempt key, bases, source, task pin and receipt; one item's
result never satisfies another.

The first and refreshed briefs are host-authored from Engram's durable records,
not the requesting session's summary. The initial requester's text may appear
in the first brief as attributed context, never criterion evidence. Requester
follow-up text, if received, is likewise attributed context and ends that
session's reuse eligibility; a re-evaluation request carries no requester free
text. How a host settles attempts, builds the brief, bounds attempts per
session and pages evidence is the host's to specify.

**Rationale.** The 2026-09-29 evaluation reuse design review, recorded in
Engram's work feed, examined 38 repeated evaluator runs: 13 had changed input,
five had no evaluation recorded, and 20 had insufficient evidence. In 11 of
those 20, later evidence did not yet exist; in nine it existed but was missed,
truncated, or indirect in the brief or judgment. In four of the nine, the
missed evidence was written shortly before the run and was already in the
brief; this does not establish a brief-construction race. The counts support
fewer **new sessions** through reuse and better evidence presentation, not
fewer **judgments** or an automatic pass after a correction.

### Turns, focus and evaluation timing

A host reports each turn's execution observations, a source change among them,
against the claim the turn was bound to when it started, and it binds the
session's focused claim (the row `held` marks as focused). Engram cannot tell
which claim a changed file belongs to: the observation carries the session's
workspace, which every claim the session holds shares. Two things follow for a
session that holds several claims.

- Switch claims at a turn boundary with `claim REF`: a repeat claim on an
  item the session holds renews it and moves focus there (a host or operator
  can also use `work core focus`). Words that name
  an item move focus to it: `claim`, `update`, `add --under`, `handoff`,
  `done`, and a holder's `note`, `gate` or `evaluate`. The next turn binds
  the new focus; the current turn still reports against the claim it started
  with. So after moving focus, end the turn before editing for the newly
  focused claim. A `note`, `gate` or `evaluate` naming an item the session
  does not hold (a peer's observation, a late gate, an independent
  evaluation) leaves focus where it was. A planning `update` or `add --under`
  on a peer's item still moves focus there; `held` then marks none of the
  session's claims as focused, and the host binds by its own rule, so
  `claim` the item you are working on again before the next turn.
- Request an evaluation when the work is done, in the same turn that changed,
  tested and committed it, when every check that turn ran was on the final
  source and passed. The host fixes the evidence basis when it starts the
  evaluator and declares the source revision it measured then; the requesting
  turn's report arrives after that basis, and neither its change to the
  declared revision nor a test that passed there asks for anything (R3b).
  Under a named root, completion takes the evaluation once the host has
  reported the root at that revision. Otherwise request in the next turn: the
  host reports a change at each revision a check of the turn ran on, so a
  check before a later edit voids the evaluation, and a check on an earlier
  revision, a failed one or an indeterminate one leaves it stale. And a
  criterion bound to a check needs that check recorded before the request,
  because the evaluator can cite only evidence at or before its basis, until
  the host holds the evaluation for the requesting turn's report.

Fixture coverage of the matrix is not live proof. The end-to-end acceptance —
a real port-PR task with a real evaluator and an observed build — stays open
until it is exercised on a real task.

## Storage and schema

No durable DDL change: the evaluation is a canonical object with a run-feed
entry; the policy field is part of the canonical policy object; the task mode
lives in the item's canonical projection; the seal gains an optional field,
and so does the evaluation (`supersedes`, R11).
Each new field is omitted from canonical bytes when it holds its default value
(self-asserted policy, absent task mode, absent seal evaluation or absent
`supersedes`),
so records written without the feature re-serialize to the same bytes; the
policy-history replay and seal tests exercise that. Nothing more is claimed:
opening a store written by a different build stays governed by the generic
different-build refusal, and no conversion or compatibility shim exists.
Doctor's feed integrity check learns the new object kind.

## Status

Design agreed on 2026-09-17 between Engram::Codex (coordination, validation,
review) and Engram::Fable (implementation), under Greg's direction.

Implemented in the core and on the agent surface: the immutable evaluation
record and its record-time validation, the acceptance-evaluation policy with
its audited `set-acceptance-evaluation` transition, the task-level mode
selection, the freshness rules, completion enforcement with the recovery
causes above, the `evaluate` word on the CLI and MCP, and the `show` and
`next` disclosures. Storage tests cover the boundary matrix rows with fixtures,
including host-minted observed evidence through the control checkpoint
protocol. The bootstrap policy stays self-asserted. See the
[shipped inventory](../shipped.md) for Engram's delivered behavior.

The first read-only review pair (2026-09-17) produced twelve corrections,
each landed tests-first: independence rechecked at consumption (F7,
`identity`), citations resolved through the shared locator resolver (R7), the
shared seal-to-evaluation binding check with its doctor label and the
authority-to-policy comparison (B36, B37), completion provenance in `done`,
`show`, and `next` (B43), `done --source-fingerprint` with source checks at
`done` rather than at read (F4, B42), core evaluator-model bounds (R9),
self-asserted policy normalization (B39), the exact attempt key in the preflight and
item-scoped explicit keys with a run-scoped content identity (R8), canonical
verification classification (R10), and doctor's decoding of
`set_acceptance_evaluation` receipts, which the fixtures' positive controls
exposed as a gap in the first delivery.

TermAl is the host that evaluates Engram acceptance.
