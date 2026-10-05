//! Declared tool arguments, shared by routing, schemas and read-only admission.

use crate::WorkItemKind;
use rmcp::schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct NextArgs {
    /// Read-only orientation: no staging, acknowledgement, focus or cursor changes.
    /// Repeated peeks repeat unacknowledged signals; use memories to read the notes.
    pub(super) peek: Option<bool>,
    /// Maximum changes (default 20); compact ready candidates are capped at five.
    pub(super) limit: Option<u32>,
    /// Return rich structured output, including raw identity and integrity metadata.
    /// Terse show and compact rows omit selected fields; this is not a global security boundary.
    pub(super) verbose: Option<bool>,
    /// Asserted host/client context generation, a plain token; until a memories listing carries it, a peek directs the session to list them.
    #[schemars(length(max = 256))]
    pub(super) context_generation: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LsArgs {
    /// Case-insensitive text over refs, titles, outcomes, and labels.
    pub(super) search: Option<String>,
    /// Only items with an active blocker or incomplete prerequisite.
    pub(super) blocked: Option<bool>,
    /// Only ready candidates in priority then work-id order; excludes blocked. Inspect an item before claiming it.
    pub(super) ready: Option<bool>,
    /// Only items assigned to this actor or held by this session.
    pub(super) mine: Option<bool>,
    /// Include completed, cancelled, and superseded items. ready selects open work only, so all adds nothing to it; blocked excludes ended items even with all.
    pub(super) all: Option<bool>,
    /// Exact case-insensitive label.
    pub(super) label: Option<String>,
    /// Direct children of this parent.
    pub(super) under: Option<String>,
    /// Only optional direct children; requires under, excludes required.
    pub(super) optional: Option<bool>,
    /// Only required direct children; requires under, excludes optional.
    pub(super) required: Option<bool>,
    /// Continuation encoding filters and project/session context, not confidential; stale cursors refuse.
    pub(super) after: Option<String>,
    /// Maximum items to return (default 20).
    pub(super) limit: Option<u32>,
    /// Return rich structured output, including raw identity and integrity metadata.
    /// Terse show and compact rows omit selected fields; this is not a global security boundary.
    pub(super) verbose: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ShowArgs {
    /// Short work ref or full UUID; reading changes neither focus nor claims.
    pub(super) work_ref: String,
    /// Newest notes/observations, excluding gates, with exact omissions.
    pub(super) notes: Option<bool>,
    /// Include gate evidence in the notes window; requires notes:true.
    pub(super) gates: Option<bool>,
    /// Newest history window, using the same bounded continuation contract.
    pub(super) history: Option<bool>,
    /// Item/kind-bound continuation; readable query context, not confidential.
    /// With note, continues a verification record's obligation assessment.
    pub(super) after: Option<String>,
    /// Complete note body beyond the window ceiling: record id or `RECORD_ID:INDEX`.
    /// An inherited event or completion: `RECORD_ID:event-INDEX` or `RECORD_ID:completion`,
    /// returning the complete member. A verification record also shows its
    /// reconstructed obligation assessment.
    pub(super) note: Option<String>,
    /// Complete stored title, outcome, and acceptance; exclusive of windows.
    pub(super) full: Option<bool>,
    /// The evaluation records of the item's run in a bounded window, oldest to newest; after continues it.
    pub(super) evaluations: Option<bool>,
    /// One evaluation record complete, by its full record id from the evaluations window.
    pub(super) evaluation: Option<String>,
    /// The source observations of the item's run in a bounded window, oldest to newest; after continues it. Exclusive of the other windows.
    pub(super) observations: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AddArgs {
    /// Opaque external planning linkage, not an imported snapshot.
    pub(super) external: Option<String>,
    /// Ordered initial notes, committed atomically with creation.
    pub(super) notes: Option<Vec<String>>,
    /// Only required field.
    pub(super) title: String,
    /// Defaults to the title.
    pub(super) outcome: Option<String>,
    /// Acceptance criteria `done` is checked against, kept in the order given
    /// without repeats; defaults to one criterion "<title> is done".
    pub(super) acceptance: Option<Vec<String>>,
    /// Bind criteria to typed host verification, as `POSITION=KIND[:FINGERPRINT]`
    /// (kind: test, build, lint, review or acceptance; positions count the
    /// acceptance list as typed; FINGERPRINT is a check's command fingerprint,
    /// and a stored record's id is refused); a bound criterion passes only on
    /// host-observed verification of that kind, never on judgment.
    pub(super) bindings: Option<Vec<String>>,
    /// Add as a child of this item instead of a root.
    pub(super) under: Option<String>,
    /// Make the child optional for parent completion. Requires `under`.
    pub(super) optional: Option<bool>,
    /// 0 (highest) through 4. Omitted, a root or an optional child gets the
    /// project default (1) and a required child its parent's priority.
    pub(super) priority: Option<i32>,
    pub(super) labels: Option<Vec<String>>,
    pub(super) assignee: Option<String>,
    /// task, bug, feature, epic, chore, or research.
    pub(super) kind: Option<WorkItemKind>,
    /// Pin the acceptance-evaluation mode from creation: same-session, sub-agent, or independent-session.
    pub(super) evaluation_mode: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkClaimArgs {
    /// Short work ref or full UUID; omit it with `under`.
    pub(super) work_ref: Option<String>,
    /// Hold this parent's next ready child instead, chosen in the ls --ready order and claimed in the same transaction.
    pub(super) under: Option<String>,
    /// Claim lifetime in seconds (default one hour).
    pub(super) ttl_seconds: Option<i64>,
    /// Attributed reason for recovering a lapsed prior claim.
    pub(super) recover: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum UpdateActionArg {
    Reject,
    Release,
    Blocked,
    Unblock,
    Revise,
    EvaluationMode,
    Cancel,
    After,
    DropAfter,
    Waive,
    Supersede,
    Detach,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateArgs {
    /// Replace external planning linkage; requires action revise.
    pub(super) external: Option<String>,
    /// Remove external planning linkage as an audited revision.
    #[serde(default)]
    pub(super) clear_external: bool,
    /// Item to act on; defaults to the focus.
    pub(super) work_ref: Option<String>,
    /// `release`, `blocked`, `unblock`, `revise`, `evaluation_mode`, `cancel`,
    /// `after`, `drop_after`, `waive`, `reject`, `supersede`, or `detach`.
    pub(super) action: UpdateActionArg,
    /// For the evaluation-mode action: same-session, sub-agent, or
    /// independent-session; omit to return the task to the default,
    /// independent evaluation unless the policy admits only same-session. A
    /// same-session mark set by the task's executor waives nothing.
    pub(super) evaluation_mode: Option<String>,
    /// Reason for release, cancel, waive, reject, supersede, or detach. Required
    /// for all but release; a release by a session with neither a contribution
    /// nor a waiver under the item's root needs it too, as the attributed
    /// waiver of that missing contribution.
    pub(super) reason: Option<String>,
    /// Why the item is blocked.
    pub(super) text: Option<String>,
    /// For unblock: the blocker to clear, by the selector `show` prints
    /// beside it; omit to clear the item's only active blocker.
    pub(super) blocker: Option<String>,
    pub(super) title: Option<String>,
    pub(super) outcome: Option<String>,
    /// Replace the whole acceptance list for revise, kept in the order given;
    /// reordering a bound criterion owes its verification again. Omission
    /// preserves it; an empty list or blank criterion is refused.
    pub(super) acceptance: Option<Vec<String>>,
    /// Replace the criteria bound to typed host verification for revise, as
    /// `POSITION=KIND[:FINGERPRINT]`. Positions count the acceptance list as
    /// typed when it is replaced in the same call, otherwise the stored list
    /// as show numbers it; FINGERPRINT is a check's command fingerprint, and a
    /// stored record's id is refused. Omitted while acceptance is replaced,
    /// the bindings are cleared; omitted otherwise, they are unchanged.
    pub(super) bindings: Option<Vec<String>>,
    pub(super) assignee: Option<String>,
    /// 0 (highest) through 4.
    pub(super) priority: Option<i32>,
    /// Defer until: RFC 3339, YYYY-MM-DD, or YYYY-MM-DDTHH:MM:SS (UTC).
    pub(super) defer: Option<String>,
    /// task, bug, feature, epic, chore, or research.
    pub(super) kind: Option<WorkItemKind>,
    /// Labels to add.
    pub(super) labels: Option<Vec<String>>,
    /// Labels to remove.
    pub(super) unlabels: Option<Vec<String>>,
    /// Prerequisite item for `after` or `drop_after`.
    pub(super) prerequisite: Option<String>,
    /// Cancelled or superseded required child for `waive`; its parent and
    /// ancestors must be open. Requires `reason`.
    pub(super) child: Option<String>,
    /// Replacement item for supersede.
    pub(super) replacement: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct GateArgs {
    /// Item to record the gate on; defaults to the focus.
    pub(super) work_ref: Option<String>,
    /// Stable gate name, normalized case-insensitively.
    pub(super) name: String,
    /// Failure labels (test ids or check names). Omit only when the gate passed.
    pub(super) failed: Option<Vec<String>>,
    /// Bounded opaque external-evidence reference; a path or URL by convention.
    pub(super) evidence_ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct EvaluateArgs {
    /// Item to evaluate; defaults to the focus.
    pub(super) work_ref: Option<String>,
    /// same-session, sub-agent, or independent-session; the project policy lists the allowed modes.
    pub(super) mode: String,
    /// The item revision whose criteria the verdicts address, as printed by show.
    pub(super) acceptance_basis: i64,
    /// The run-feed position the evaluator read through, as printed by show. A host check after it asks for a resubmission, unless it passed on the revision given as `source_fingerprint`; a source change after it voids the evaluation, unless it is to that revision.
    pub(super) evidence_basis: i64,
    /// One verdict per current criterion by one-based position; a pass cites note/gate locators as `show` with notes and gates prints them, or full record ids of host-minted verification or environment evidence.
    pub(super) verdicts: Vec<crate::WorkCriterionVerdictInput>,
    /// Explicit attempt key; identical resends replay, contradicting content under the same key refuses.
    pub(super) attempt: Option<String>,
    /// Host-measured source fingerprint at evaluation time.
    pub(super) source_fingerprint: Option<String>,
    /// PROVIDER/MODEL or PROVIDER/MODEL@VERSION, recorded as asserted metadata.
    pub(super) model: Option<String>,
    /// Sub-agent mode only: the evaluator's distinct execution identity.
    pub(super) execution_identity: Option<String>,
    /// Sub-agent mode only: the host-attested parent session.
    pub(super) parent_session: Option<String>,
    /// Record id of the carried failing evaluation this one acknowledges, as `show` prints it in `carried_failure`. Required after the run's executor revised the criteria that evaluation failed, and then only from an evaluator that never held the run (never `same_session`); a failing evaluation that names it keeps it carried; refused when no failure is carried.
    pub(super) supersedes: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct RememberArgs {
    /// Project note or observation. Never include credentials or secrets.
    #[schemars(length(max = 8192))]
    pub(super) text: String,
    /// Safe permanent key; omitted to derive a slug from the first words.
    #[schemars(length(max = 64))]
    pub(super) key: Option<String>,
    /// Append an attributed version to an existing key, retaining all prior versions.
    pub(super) revise: Option<bool>,
    /// Optional current revision check; stale values refuse. Omit to revise the current head.
    pub(super) expected_revision: Option<u64>,
    /// Review this memory when the named item retires: local:REF (an item in this
    /// project) or external:PROJECT#REFERENCE (asserted text of ASCII letters, digits
    /// and . _ - / : @ +). Omitted on revise, the current target is kept.
    pub(super) retires_with: Option<String>,
    /// With revise, remove the retirement target, or acknowledge one a revision
    /// dropped, and record the clear; refused without revise or when there is neither.
    pub(super) clear_retires_with: Option<bool>,
    /// With revise and `expected_revision`, append text to that revision as a
    /// paragraph instead of replacing the body.
    pub(super) append: Option<bool>,
    /// With revise and `expected_revision`, replace only the interior of the
    /// section marked `<!-- engram-section NAME -->` … `<!-- /engram-section NAME -->`.
    #[schemars(length(max = 64))]
    pub(super) section: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct MemoriesArgs {
    /// Search text, or the exact key when full is true.
    #[schemars(length(max = 256))]
    pub(super) query: Option<String>,
    /// Continue an unfiltered key-ordered listing.
    #[schemars(length(max = 64))]
    pub(super) after: Option<String>,
    /// Return one dedicated full body for the positional key.
    pub(super) full: Option<bool>,
    /// With full and an exact key, read this historical revision instead of the current one.
    pub(super) revision: Option<u64>,
    /// The host's context generation, as a peek printed it; the first page of an
    /// unfiltered listing records it, which records a listing, not a reading.
    /// Without it, memories records nothing.
    #[schemars(length(max = 256))]
    pub(super) context_generation: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ForgetArgs {
    /// Permanently reserved project-memory key to tombstone.
    #[schemars(length(max = 64))]
    pub(super) key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct NoteArgs {
    /// Record current coordination status; storage determines owner/peer qualification.
    pub(super) status: Option<bool>,
    /// Item to note on; defaults to the focus.
    pub(super) work_ref: Option<String>,
    /// What you found or decided.
    pub(super) text: String,
    /// Evidence pointers such as paths or URLs.
    pub(super) refs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct DoneArgs {
    /// At most 64 explicit author links, not verification: criterion position and an existing note/gate locator.
    pub(super) links: Option<Vec<crate::work_service::WorkCriterionLinkInput>>,
    /// Required with links; pass `acceptance_basis` from show. Any work revision change refuses.
    pub(super) link_basis: Option<i64>,
    /// Item to complete; defaults to the focus.
    pub(super) work_ref: Option<String>,
    /// What was delivered; recorded and checkpointed before sealing.
    pub(super) summary: Option<String>,
    /// Shared acceptance note; does not link evidence to individual criteria.
    pub(super) note: Option<String>,
    /// Host-measured source fingerprint at completion time; checked against the evaluated one when the policy requires source freshness.
    pub(super) source_fingerprint: Option<String>,
    /// Where the work landed, recorded in the seal as asserted provenance: `commit`, `remote`, `branch`, `pushed_at`, and `installed_build` when a binary was installed.
    pub(super) landing: Option<crate::domain::CompletionLanding>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkSearchArgs {
    /// Case-insensitive text over refs, titles, outcomes, and labels.
    pub(super) query: String,
    /// Maximum items to return (default 20).
    pub(super) limit: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum HandoffActionArg {
    Offer,
    Accept,
    Cancel,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct HandoffArgs {
    /// Item to hand off; defaults to the focus.
    pub(super) work_ref: Option<String>,
    /// offer (with to), accept, or cancel (with reason).
    pub(super) action: HandoffActionArg,
    /// Real recipient session id supplied by the host or coordinator; at most 64 UTF-8 bytes; peer display labels are refused.
    pub(super) to: Option<String>,
    /// Checkpoint summary recorded with the offer.
    pub(super) summary: Option<String>,
    /// Why an outstanding offer is cancelled.
    pub(super) reason: Option<String>,
    /// Offer lifetime in seconds.
    pub(super) ttl_seconds: Option<i64>,
}
