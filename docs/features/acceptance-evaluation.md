# Acceptance evaluation

> Normative section: [spec § Local work system](../spec.md); related briefs:
> [Local work system](local-work-system.md) (completion seals, obligations),
> [Execution pipeline](execution-pipeline.md) (typed evidence),
> [Behavioral control plane](behavioral-control-plane.md) (host channel),
> [CLI & MCP](cli-and-mcp.md) (agent words),
> [Turn gate assessment](turn-gate-assessment.md) (why the evaluation request
> stays out of the turn gate).

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
| `sub_agent` | an evaluator spawned under the completing session | same or host-assigned session id, plus a distinct `execution_identity` and a host-attested `parent_session` | asserted; a host channel may later raise it, the core never infers it |
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
an evaluation must use exactly that mode; when unset, any policy-allowed mode
is acceptable. A task cannot select a mode the policy disallows at record time;
the refusal names the allowed set. Selecting a mode never downgrades the policy.

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
  request a new one. The one source change that does not count is a change to
  the revision the evaluation declared it judged (below), together with the
  obligation it opened; a later check, including the test that resolves that
  obligation, still asks for a resubmission. Likewise the source fingerprint
  is the value measured for the evaluated content, not one taken at
  submission. It is the host's source revision, as the host reports it on turn
  observations; a declared workspace id must match the observation's workspace
  too.
- **R4 independence.** `independent_session`: the evaluator session differs
  from the claim holder and from every recorded executor session of the run.
  `sub_agent`: `execution_identity` and `parent_session` are present and the
  parent equals the current holder or executor session. `same_session`: the
  evaluator session equals the current holder or executor session.
- **R5 pass citations.** Every `pass` carries a non-empty rationale and at
  least one citation. `observed`: each citation is host-minted
  `VerificationEvidence` bound to this run whose result is `passed`.
  `asserted`: each citation is a gate record on this run with no failure
  labels; refused when policy `mechanical_basis` is `observed`. `judgment`:
  each citation is run evidence (note, gate, verification, or environment
  evidence). `human_required` can never be a `pass`. A criterion bound to a
  typed verification requirement (`--bind`) passes only on `observed`, and
  every citation must be verification evidence of the bound kind (and pinned
  check) with a passed result; `asserted` and `judgment` are refused for it
  by name.
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

## Freshness

Completion consults the newest evaluation on the run feed. It is **fresh** only
when all of the following hold; otherwise it is treated as absent with the
stale reason named.

- **F1 revision.** `work_revision_hash` equals the current item's; any revision
  (criteria, outcome, title, mode, or other planning fields) invalidates.
- **F2 run.** `run_id` is the completing run; a new run generation starts
  without an evaluation.
- **F3 host-observed mutation.** No execution observation with
  `source_changed`, no verification or environment evidence, and no obligation
  definition or resolution was appended to the run feed after `evaluated_cut`.
  These are host-minted facts about the workspace and its checks. The only
  exception is a source change that left the source at the revision the
  evaluation declared it judged (its `source_basis`), with the obligation that
  change opened: the evaluator saw that state, so the host's late report of it
  does not void the evaluation. A check recorded after the cut, including the
  one that resolves that obligation, still asks for a resubmission. The
  declared revision is the evaluator's assertion, recorded like its verdicts;
  Engram cannot attest what the evaluator read, so this exception carries the
  evaluation's own asserted assurance. A host that wants the revision to be one
  it measured passes that revision to the evaluator itself (see
  [turns, focus and evaluation timing](#turns-focus-and-evaluation-timing)). A
  `same_session` implementer gains nothing from it: it can re-read and submit
  at the new cut in any case.
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
  `sub_agent` records make no independence claim and survive a later holder
  change.

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
| `AcceptanceEvaluationStale { reason }` | F1–F7 failed (`revision`, `run`, `mutation`, `source`, `policy`, `evidence`, `identity`) | re-evaluate against the current state; `source` names `done --source-fingerprint F` as the alternative; `identity` needs a session that never held the run |
| `AcceptanceFailed { criterion }` | newest fresh evaluation has a `fail` | corrective work, then evaluate again |
| `AcceptanceInsufficientEvidence { criterion }` | newest fresh evaluation has `insufficient_evidence` | record the missing evidence, then evaluate again |
| `AcceptanceNeedsHuman { criterion }` | a criterion needs a human decision | obtain that decision; only a separately authorized revision (`update REF --accept …`) or cancellation changes the requirement, and a new evaluation follows the decision. No agent override exists. |

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
newest evaluation on the run feed; "fresh" means F1–F7 hold.

| State | Condition | `done` | Leaves the state by |
| --- | --- | --- | --- |
| S0 unevaluated | no `E` for this run, or `E` not fresh | refuse `MissingAcceptanceEvaluation` / `AcceptanceEvaluationStale` | `evaluate` records a fresh `E` |
| S1 passing | `E` fresh, all verdicts `pass` | seal; binds `E` | any F1–F7 change → S0; a newer non-passing `E` → S2/S3/S4 |
| S2 failed | `E` fresh, some verdict `fail` | refuse `AcceptanceFailed` | corrective work → new `evaluate` → S1/S2/S3/S4; a revision → S0 |
| S3 insufficient | `E` fresh, some `insufficient_evidence`, none `fail` | refuse `AcceptanceInsufficientEvidence` | record evidence → new `evaluate` |
| S4 needs human | `E` fresh, some `needs_human`, none `fail`/`insufficient` | refuse `AcceptanceNeedsHuman` | a human decision, expressed as a separately authorized `update --accept` (revision → S0) or cancellation |
| L self-asserted | policy has no allowed modes | existing path; receipt says self-asserted | policy update |

`evaluate` itself refuses under R1–R9 without changing state. Precedence when a
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
| B07 | `sub_agent` with `execution_identity` and `parent_session` = holder, same session id | record accepted, recorded `asserted`; receipt does not claim verified independence |
| B08 | mode allowed by policy but different from the task's selected mode | refuse at write naming the selected mode |
| B09 | port-PR build criterion; `pass` + `observed` citing host-minted `VerificationEvidence(Build, passed)` on this run | accepted; seal |
| B10 | `pass` + `observed` citing `VerificationEvidence(Build, failed)` | refuse at write ("observed pass requires a passed check") |
| B11 | `fail` citing the failed build evidence | accepted; `done` refuses `AcceptanceFailed`; corrective work and a new passing evaluation then seal |
| B12 | `pass` + `asserted` citing a gate record with no failures | accepted under `mechanical_basis: asserted`; refused at write under `observed` |
| B13 | non-build natural-language criterion; `pass` + `judgment` with rationale and a note citation | accepted; seal |
| B14 | `pass` with empty citations, or empty rationale | refuse at write (no vacuous pass) |
| B15 | `insufficient_evidence` | accepted; `done` refuses `AcceptanceInsufficientEvidence`; after evidence and a new evaluation, seal |
| B16 | `needs_human` | accepted; `done` refuses `AcceptanceNeedsHuman`; `update --accept` revises → old evaluation stale (F1); new evaluation seals |
| B17 | citation from another run, another item, or a non-holder observation | refuse at write |
| B18 | verdict list missing a criterion, duplicate position, or `acceptance_basis` behind the current revision | refuse at write with "re-read show" |
| B19 | criteria revised after a passing evaluation | `done` refuses `AcceptanceEvaluationStale { revision }` |
| B20 | host-observed `source_changed` observation after the evaluation | `done` refuses `AcceptanceEvaluationStale { mutation }` |
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
| B31 | evaluator reads at cut `c`; a host-observed `source_changed` observation or check lands after `c`; the verdicts are submitted with `evidence_basis = c` | refuse at write: `acceptance_evaluation_resubmit` after a check, `acceptance_evaluation_void` after a source change the evaluation did not judge; a change to the declared judged revision does not refuse; a citation beyond `c` also refuses; resubmission with the current basis records |
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

## Agent surface

The evaluate word is a deliberate extension of the agent vocabulary; the
counts in every contract file move with it.

```text
engram work evaluate [REF] --mode MODE --acceptance-basis N --evidence-basis M \
  --verdict POSITION=VERDICT[:BASIS] --rationale POSITION=TEXT \
  [--evidence POSITION=LOCATOR]... [--attempt KEY] [--source-fingerprint F] \
  [--model PROVIDER/MODEL] [--execution-identity ID --parent-session SESSION]
```

`POSITION` is the one-based criterion position, `--acceptance-basis` the
revision from `show` (exactly as `done --link`), `--evidence-basis` the
run-feed position `show` prints beside it under an evaluated policy, which the
evaluator read through, and `LOCATOR` a note/gate locator from `show --notes
--gates` or the full id of host-minted verification or environment evidence
(R7). Open items in self-asserted projects keep their unchanged `show` shape. MCP `evaluate` takes
the same data as `mode`, `acceptance_basis`, `evidence_basis`, `verdicts:
[{criterion, verdict, basis, rationale, evidence: [locator]}]`, and the
optional fields. The receipt is a bounded projection of the immutable record,
not the record: the evaluation id, mode, revision, run, evaluated cut,
`passed`, the first blocking verdict with its criterion compacted,
`verdicts_total`, a prefix of verdict rows (position, verdict, basis, citation
count), `verdicts_omitted`, which counts exactly the rows left out to fit the
agent budget, and the attempt key exactly as recorded. The core measures that
response before the record is written, so an admitted evaluation never
commits into a failed response. Ordinary `show` prints the same bounded
prefix with the omitted count, the evaluator label, and the recorded source
fingerprint (marked as checked at `done` when the policy requires freshness);
`next` prints one `evaluation: <mode> P/N pass, fresh|stale: R` line under
the focused evaluated item (`focus.evaluation` in JSON); `show REF --full`,
the authored-contract read that may exceed 12 KiB, returns the complete
newest evaluation with every verdict's full rationale and citations plus its
freshness. `done [--source-fingerprint F]` presents the host-measured
fingerprint (F4); its refusals carry the causes above, and its success line
and the completed item's `show` carry the provenance (completion enforcement,
step 5). `add --evaluation-mode MODE` pins the mode from creation (roots and
`--under` children alike), and `update` accepts `--evaluation-mode MODE` and
`--clear-evaluation-mode` as one audited revision (MCP action
`evaluation_mode`). A supplied blank or whitespace mode refuses at `add` and
`update` alike through one shared parser; only omission (no pin at creation,
the explicit clear on update) leaves the item unpinned. A pin or clear is a
planning revision that `show` history and peers' `next` deltas name as
`evaluation mode`; clearing an already unpinned item reads as `no planning
change`. A graph snapshot carries the pinned mode and restores it
verbatim, so a transfer never widens which evaluator may accept a task.

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
- Request an evaluation at a turn boundary, not after changing the source in
  the same turn. The host fixes the evidence basis when it starts the
  evaluator; the requesting turn's own report arrives after that, and unless
  the evaluation declared the revision it judged, it voids the evaluation.
  A host can instead defer the evaluator until the requesting turn's
  checkpoint and declare the revision it measured then.

Fixture coverage of the matrix is not live proof. The end-to-end acceptance —
a real port-PR task with a real evaluator and an observed build — stays open
until the host integration exists.

## Storage and schema

No durable DDL change: the evaluation is a canonical object with a run-feed
entry; the policy field is part of the canonical policy object; the task mode
lives in the item's canonical projection; the seal gains an optional field.
Each new field is omitted from canonical bytes when it holds its default value
(self-asserted policy, absent task mode or absent seal evaluation),
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
protocol. The bootstrap policy stays self-asserted; no project has the feature
enabled.

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

Not delivered: the host integration (TermAl mode selection, evaluator
spawning, source fingerprints, observed build evidence) and the end-to-end
acceptance on a real task. Those stay open as separate required items; the
host item has no owner yet, and fixture coverage is not live proof.
