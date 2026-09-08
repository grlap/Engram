use crate::work_service::identity::DisplayIdentity;
use std::fmt::Write as _;

use super::child_obligations::ShowChildObligations;
use crate::work_service::{WorkNextSection, WorkSectionOmissionReason};

use super::{
    ChildRequirement, DateTime, Holder, ReadyWorkSummary, Serialize, Utc, WorkAvailability,
    WorkBlockerKind, WorkChangeProjection, WorkClaim, WorkClaimState, WorkEvidenceKind,
    WorkFocusView, WorkHandoffState, WorkItemKind, WorkItemSummary, WorkLifecycle,
    WorkPrerequisiteState, WorkSectionOmission, actor_label, availability_words, blocker_word,
    child_summary_line, clock, evidence_kind_word, kind_word, lifecycle_word, short,
    short_ref_for_work_id, strip_kind_prefix,
};

/// Measure the actual safe projection, including guidance, before shedding.
/// Hidden core metadata must not consume an agent receipt's byte budget.
pub(super) fn fit_show_receipt(
    mut view: WorkFocusView,
    render: impl Fn(&WorkFocusView) -> Result<super::Receipt, super::VerbError>,
    max_bytes: usize,
) -> Result<super::Receipt, super::VerbError> {
    // Normalize the independently loaded latest note exactly as show does,
    // so byte shedding cannot count a page row already hidden by replacement.
    view.evidence_items = show_evidence(&view);
    loop {
        let receipt = render(&view)?;
        if show_fits(&receipt, max_bytes)? {
            return Ok(receipt);
        }
        if !shed_show_context_once(&mut view) {
            return fit_acceptance_prefix(&mut view, &render, max_bytes);
        }
        record_show_omission(&mut view, 1);
    }
}

fn show_fits(receipt: &super::Receipt, max_bytes: usize) -> Result<bool, super::VerbError> {
    // Match compact next's conservative strict ceiling for both formats.
    Ok(receipt.text().len() < max_bytes
        && serde_json::to_vec_pretty(&receipt.value)?.len() < max_bytes)
}

fn record_show_omission(view: &mut WorkFocusView, count: usize) {
    if let Some(omission) = view.omissions.iter_mut().find(|entry| {
        entry.section == WorkNextSection::Focus
            && entry.reason == WorkSectionOmissionReason::ByteBudget
    }) {
        omission.omitted_count += count;
    } else {
        view.omissions.push(WorkSectionOmission {
            section: WorkNextSection::Focus,
            reason: WorkSectionOmissionReason::ByteBudget,
            omitted_count: count,
        });
    }
}

fn fit_acceptance_prefix(
    view: &mut WorkFocusView,
    render: &impl Fn(&WorkFocusView) -> Result<super::Receipt, super::VerbError>,
    max_bytes: usize,
) -> Result<super::Receipt, super::VerbError> {
    // The full list already failed. Search only whole proper prefixes, with
    // exact omission metadata in every probe, rather than rendering N tails.
    let criteria = std::mem::take(&mut view.status.work.acceptance);
    let omissions = view.omissions.clone();
    let (mut lower, mut upper) = (0, criteria.len());
    let mut best = None;
    while lower < upper {
        let visible = lower + (upper - lower) / 2;
        view.status.work.acceptance = criteria[..visible].to_vec();
        view.omissions.clone_from(&omissions);
        record_show_omission(view, criteria.len() - visible);
        let receipt = render(view)?;
        if show_fits(&receipt, max_bytes)? {
            best = Some(receipt);
            lower = visible + 1;
        } else {
            upper = visible;
        }
    }
    if best.is_none()
        && let Some(facts) = view.acceptance_evidence.take()
    {
        // Keep the frozen positions before decorative/current criterion text.
        // If even the body-free contract cannot fit, shed whole positions too.
        view.status.work.acceptance.clear();
        view.omissions.clone_from(&omissions);
        record_show_omission(view, criteria.len());
        let receipt = super::acceptance::fit_done(
            &facts,
            |page| {
                // Render bounded facts in the header, before any record window.
                // Window replacement must never remove only the text twin.
                let mut bounded = view.clone();
                bounded.acceptance_evidence = Some(page.facts());
                if page.omitted_count() > 0 {
                    record_show_omission(&mut bounded, page.omitted_count());
                }
                render(&bounded)
            },
            max_bytes,
        )?;
        if show_fits(&receipt, max_bytes)? {
            return Ok(receipt);
        }
    }
    best.ok_or_else(|| {
        super::StoreError::InvalidWorkProjection(
            "show metadata exceeds the agent response byte budget".into(),
        )
        .into()
    })
}

fn shed_show_context_once(view: &mut WorkFocusView) -> bool {
    if crate::work_service::shorten_status_previews(
        &mut view.status.work.current_status,
        &mut view.status.work.status_observation,
    ) {
        return true;
    }
    // Omit the whole recoverable reason before sacrificing useful context or
    // the item's own contract. Its origin and navigation remain visible.
    if let Some(origin) = view.detached_from.as_mut()
        && !origin.reason.is_empty()
    {
        origin.reason.clear();
        // On the full-text path the carrier's loss flag means whole omission.
        origin.reason_truncated = true;
        return true;
    }
    // Only remove fields actually emitted by show. In particular, memories,
    // obligations, and child acceptance metadata are not presentation rows.
    if let Some(index) = view
        .history
        .items
        .iter()
        .rposition(|entry| matches!(entry.delivery, WorkChangeProjection::Visible(_)))
    {
        view.history.items.remove(index);
        view.history.omitted += 1;
        return true;
    }
    if view.restored_history.items.pop().is_some() {
        view.restored_history.omitted += 1;
        return true;
    }
    // Blockers and prerequisites are already count/field bounded. Preserve
    // their blocking context; every successful shed must remove a real row.
    if view.children.pop().is_some() {
        return true;
    }
    // Summary refs are expendable whole rows, but their exact totals and
    // scoped navigation survive even when no ordinary child row remains.
    if let Some(groups) = &mut view.child_obligations
        && (groups.open_optional.items.pop().is_some()
            || groups.required_owed.items.pop().is_some())
    {
        return true;
    }
    // Keep the independently loaded latest note until the other note rows go.
    if let Some(index) = view.evidence_items.iter().rposition(|entry| {
        view.latest_evidence_item
            .as_ref()
            .is_none_or(|latest| latest.evidence != entry.evidence)
    }) {
        view.evidence_items.remove(index);
        return true;
    }
    if view.latest_evidence_item.take().is_some() {
        view.evidence_items.clear();
        return true;
    }
    false
}

/// Agent-detail work fields for `show`. Canonical ids, revision counters,
/// run bindings, and content hashes remain on the host-only core view.
#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowWorkSummary {
    pub(super) short_ref: String,
    pub(super) title: String,
    pub(super) outcome: String,
    pub(super) acceptance: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) acceptance_omitted: Option<usize>,
    pub(super) kind: WorkItemKind,
    pub(super) priority: i32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) assigned_to: Option<String>,
    pub(super) lifecycle: WorkLifecycle,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) restored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) superseded_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) child_resolution: Option<super::child_obligations::ShowChildSuccessor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) child_requirement: Option<ChildRequirement>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowStatus {
    pub(super) work: ShowWorkSummary,
    pub(super) availability: WorkAvailability,
}

/// A relation row that preserves the agent's navigation vocabulary without
/// exposing the relation's canonical work identity.
#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowRelation {
    pub(super) short_ref: String,
    pub(super) title: String,
    pub(super) lifecycle: WorkLifecycle,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) child_resolution: Option<super::child_obligations::ShowChildSuccessor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) child_requirement: Option<ChildRequirement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prerequisite_state: Option<WorkPrerequisiteState>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowBlocker {
    pub(super) kind: WorkBlockerKind,
    pub(super) detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowHandoff {
    pub(super) from: String,
    pub(super) to: String,
    pub(super) expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowNote {
    pub(super) kind: WorkEvidenceKind,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) non_holder: bool,
    pub(super) summary: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) refs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) by: Option<String>,
    pub(super) created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowHistoryItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) generation: Option<usize>,
    pub(super) kind: String,
    pub(super) summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) by: Option<String>,
    pub(super) created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowHistory {
    pub(super) total: usize,
    pub(super) omitted: usize,
    pub(super) items: Vec<ShowHistoryItem>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowDetachedFrom {
    #[serde(rename = "ref")]
    pub(super) work_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reason_omitted: Option<usize>,
}

/// Terse projection shared by CLI `show --json` and the agent-facing MCP
/// tool. The rich [`WorkFocusView`] remains available through `work core
/// focus` for hosts that need authority and integrity fields.
#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowReceiptValue {
    /// Explicit read-concurrency token, not read-side state or authority.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) acceptance_basis: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) acceptance_evidence: Option<super::acceptance::AcceptanceEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) acceptance_evidence_unavailable: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) acceptance_evidence_error_class: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_status: Option<crate::work_service::WorkCurrentStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) status_observation: Option<crate::work_service::WorkCurrentStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) external_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) parent_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) parent_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) parent_lifecycle: Option<WorkLifecycle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) detached_from: Option<ShowDetachedFrom>,
    pub(super) status: ShowStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) holder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) held_until: Option<DateTime<Utc>>,
    pub(super) children: Vec<ShowRelation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) children_omitted: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) child_obligations: Option<ShowChildObligations>,
    pub(super) prerequisites: Vec<ShowRelation>,
    pub(super) handoffs: Vec<ShowHandoff>,
    pub(super) blockers: Vec<ShowBlocker>,
    pub(super) notes: Vec<ShowNote>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) notes_omitted: Option<usize>,
    pub(super) history: ShowHistory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) restored_history: Option<ShowHistory>,
    pub(super) allowed_next: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) omissions: Vec<WorkSectionOmission>,
}

pub(super) fn live(claim: &WorkClaim, now: DateTime<Utc>) -> bool {
    claim.state == WorkClaimState::Active && claim.expires_at > now
}

pub(super) fn optional_child_requirement(
    requirement: ChildRequirement,
) -> Option<ChildRequirement> {
    (requirement == ChildRequirement::Optional).then_some(requirement)
}

pub(super) fn show_relation(item: &WorkItemSummary) -> ShowRelation {
    ShowRelation {
        short_ref: item.short_ref.clone(),
        title: item.title.clone(),
        lifecycle: item.lifecycle,
        child_resolution: super::child_obligations::ShowChildSuccessor::for_work(item),
        child_requirement: optional_child_requirement(item.child_requirement),
        prerequisite_state: item.prerequisite_state,
    }
}

fn acceptance_unavailable(view: &WorkFocusView) -> Option<&'static str> {
    if view.acceptance_evidence.is_some() {
        None
    } else if view.completed_by_record {
        Some(super::acceptance::RESTORED_UNAVAILABLE)
    } else {
        view.acceptance_evidence_error_class
            .map(|_| super::acceptance::REPLAY_UNAVAILABLE)
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the terse text projection renders each bounded focus section in display order"
)]
pub(super) fn show_lines(
    view: &WorkFocusView,
    holder: Holder<'_>,
    identity: DisplayIdentity<'_>,
    now: DateTime<Utc>,
) -> Vec<String> {
    let work = &view.status.work;
    let mut lines = vec![show_item_line(
        &view.status,
        holder,
        view.completed_by_record,
        identity,
        now,
    )];
    lines.push(view.parent.as_ref().map_or_else(
        || "parent: root".into(),
        |parent| {
            format!(
                "parent: {} \"{}\" ({}), {}",
                parent.short_ref,
                short(&parent.title),
                lifecycle_word(parent.lifecycle),
                match work.child_requirement {
                    ChildRequirement::Required => "required",
                    ChildRequirement::Optional => "optional",
                }
            )
        },
    ));
    if let Some(status) = &work.current_status {
        lines.extend(super::status_text_lines(
            &work.short_ref,
            status,
            "status",
            "",
        ));
    } else {
        lines.push("status: none recorded by the current owner".into());
    }
    if let Some(peer) = &work.status_observation {
        lines.extend(super::status_text_lines(
            &work.short_ref,
            peer,
            "peer status observation",
            "",
        ));
    }
    let mut facts = vec![
        format!("kind: {}", kind_word(work.kind)),
        format!("priority: {}", work.priority),
    ];
    if let Some(external) = &work.external_ref {
        facts.push(format!("external: {}", super::terminal_safe_line(external)));
    }
    if !work.labels.is_empty() {
        facts.push(format!(
            "labels: {}",
            work.labels
                .iter()
                .map(|label| super::terminal_safe_line(label))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(assignee) = &work.assigned_to {
        facts.push(format!("assignee: {}", identity.actor(assignee)));
    }
    lines.push(facts.join("  "));
    if let Some(replacement) = work.superseded_by {
        lines.push(format!("successor: {}", short_ref_for_work_id(replacement)));
    }
    if let Some(resolution) = super::child_obligations::ShowChildSuccessor::for_work(work) {
        lines.push(resolution.line());
        if let Some(remedy) = resolution.remedy {
            lines.push(format!("  {remedy}"));
        }
    }
    if let Some(origin) = &view.detached_from {
        if origin.reason_truncated {
            lines.push(format!(
                "detached from: {} (1 detach reason not shown)",
                origin.work_ref
            ));
        } else {
            lines.push(format!(
                "detached from: {} — {}",
                origin.work_ref,
                super::terminal_safe_line(&origin.reason)
            ));
        }
    }
    lines.push(format!(
        "outcome: {}",
        super::terminal_safe_line(&view.outcome)
    ));
    lines.push("acceptance:".into());
    if work.lifecycle == WorkLifecycle::Open && work.acceptance_count > 0 {
        lines.push(format!(
            "  acceptance basis: {} (pass --link-basis with --link)",
            work.revision
        ));
    }
    for (position, criterion) in work.acceptance.iter().enumerate() {
        let safe = super::terminal_data_block(criterion);
        for (index, line) in safe.split('\n').enumerate() {
            let prefix = if index == 0 {
                format!("  {}. ", position + 1)
            } else {
                "    ".into()
            };
            lines.push(format!("{prefix}{line}"));
        }
    }
    if work.acceptance_count > work.acceptance.len() {
        lines.push(format!(
            "  ({} more not shown); hidden criteria continue from position {} in the same numbering",
            work.acceptance_count - work.acceptance.len(), work.acceptance.len() + 1
        ));
    }
    if let Some(facts) = &view.acceptance_evidence {
        lines.extend(super::acceptance::AcceptanceEvidence::new(facts).lines());
    }
    if let Some(explanation) = acceptance_unavailable(view) {
        lines.push(format!("criterion evidence: {explanation}"));
        if let Some(class) = view.acceptance_evidence_error_class {
            lines.push(format!("  diagnostic class: {class}"));
        }
    }
    if !view.blockers.is_empty() {
        lines.push("blockers:".into());
        for blocker in &view.blockers {
            lines.push(format!(
                "  - {}: {}",
                blocker_word(blocker.kind),
                super::terminal_safe_line(&blocker.detail)
            ));
        }
    }
    let children_omitted = view.child_count.saturating_sub(view.children.len());
    if !view.children.is_empty() || children_omitted > 0 {
        if view.children.is_empty() {
            lines.push(format!("children: {children_omitted} not shown"));
        } else {
            let mut children = view
                .children
                .iter()
                .map(child_summary_line)
                .collect::<Vec<_>>()
                .join(", ");
            if children_omitted > 0 {
                let _ = write!(children, " (+{children_omitted} more)");
            }
            lines.push(format!("children: {children}"));
        }
    }
    if let Some(groups) = &view.child_obligations {
        lines.extend(ShowChildObligations::new(groups, work).lines());
    }
    if !view.prerequisites.is_empty() {
        lines.push(format!(
            "prerequisites: {}",
            view.prerequisites
                .iter()
                .map(|item| format!(
                    "{} \"{}\" ({})",
                    item.short_ref,
                    short(&item.title),
                    lifecycle_word(item.lifecycle)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for offer in view
        .handoffs
        .iter()
        .filter(|offer| offer.state == WorkHandoffState::Offered && offer.expires_at > now)
    {
        lines.push(format!(
            "handoff: offered by {} to {} until {}",
            identity.session(&offer.from),
            identity.session(&offer.to),
            clock(offer.expires_at, now)
        ));
    }
    if let Some(last) = view.latest_evidence_item.as_ref() {
        let by = last
            .display_actor_id
            .as_deref()
            .or(last.actor_id.as_deref())
            .map(|actor| {
                super::terminal_safe_line(&actor_label(
                    &identity.author(actor, last.display_actor_session_id.as_ref()),
                    last.actor_context.as_deref(),
                ))
            });
        lines.push(format!(
            "notes: {} recorded; latest {}{}{}: \"{}\"",
            view.evidence_count,
            evidence_kind_word(last.evidence_kind),
            if last.non_holder { " (non-holder)" } else { "" },
            by.as_ref()
                .map(|actor| format!(" by {actor}"))
                .unwrap_or_default(),
            short(&last.summary)
        ));
    }
    if view.restored_history.total > 0 {
        lines.push(format!(
            "restored history: {} entries",
            view.restored_history.total
        ));
        for entry in &view.restored_history.items {
            let actor = actor_label(
                &identity.author(&entry.actor.actor_id, entry.actor.session_id.as_ref()),
                entry.actor.attribution_context(),
            );
            lines.push(format!(
                "  - generation {} {} by {}: {}",
                entry.generation_index,
                super::terminal_safe_line(&entry.kind),
                actor,
                short(&entry.summary)
            ));
        }
        if view.restored_history.omitted > 0 {
            lines.push(format!(
                "  ({} earlier entries not shown)",
                view.restored_history.omitted
            ));
        }
    }
    lines
}

#[allow(
    clippy::too_many_lines,
    reason = "the safe structured projection explicitly allowlists every terse show section"
)]
pub(super) fn show_receipt_value(
    view: &WorkFocusView,
    holder: Holder<'_>,
    identity: DisplayIdentity<'_>,
    now: DateTime<Utc>,
) -> ShowReceiptValue {
    let work = &view.status.work;
    let (holder, held_until) = match holder {
        Holder::You(expires_at) => (Some("you".into()), Some(expires_at)),
        Holder::Other(session, expires_at, _) => {
            (Some(identity.session(session)), Some(expires_at))
        }
        Holder::Nobody => (None, None),
    };
    let history: Vec<ShowHistoryItem> = view
        .history
        .items
        .iter()
        .filter_map(|change| match &change.delivery {
            WorkChangeProjection::Visible(summary) => Some(ShowHistoryItem {
                generation: None,
                kind: summary.change_kind.clone(),
                summary: strip_kind_prefix(&summary.summary, &summary.change_kind),
                by: summary.actor_id.as_deref().map(|actor| {
                    actor_label(
                        &change.display_producer.as_ref().map_or_else(
                            || identity.actor(actor),
                            |(actor, session)| identity.author(actor, session.as_ref()),
                        ),
                        summary.actor_context.as_deref(),
                    )
                }),
                created_at: summary.created_at,
            }),
            WorkChangeProjection::Omitted(_) => None,
        })
        .collect();
    let hidden_history = view.history.items.len().saturating_sub(history.len());
    let restored_history = (view.restored_history.total > 0).then(|| ShowHistory {
        total: view.restored_history.total,
        omitted: view.restored_history.omitted,
        items: view
            .restored_history
            .items
            .iter()
            .map(|entry| ShowHistoryItem {
                generation: Some(entry.generation_index),
                kind: entry.kind.clone(),
                summary: entry.summary.clone(),
                by: Some(actor_label(
                    &identity.author(&entry.actor.actor_id, entry.actor.session_id.as_ref()),
                    entry.actor.attribution_context(),
                )),
                created_at: entry.created_at,
            })
            .collect(),
    });
    let notes = show_notes(view, identity);
    ShowReceiptValue {
        acceptance_basis: (work.lifecycle == WorkLifecycle::Open && work.acceptance_count > 0)
            .then_some(work.revision),
        acceptance_evidence: view
            .acceptance_evidence
            .as_ref()
            .filter(|facts| facts.criteria_count > 0)
            .map(super::acceptance::AcceptanceEvidence::new),
        acceptance_evidence_unavailable: acceptance_unavailable(view),
        acceptance_evidence_error_class: acceptance_unavailable(view)
            .and(view.acceptance_evidence_error_class),
        current_status: work.current_status.clone(),
        status_observation: work.status_observation.clone(),
        external_ref: work.external_ref.clone(),
        parent_ref: view.parent.as_ref().map(|parent| parent.short_ref.clone()),
        parent_title: view.parent.as_ref().map(|parent| parent.title.clone()),
        parent_lifecycle: view.parent.as_ref().map(|parent| parent.lifecycle),
        detached_from: view.detached_from.as_ref().map(|origin| ShowDetachedFrom {
            work_ref: origin.work_ref.clone(),
            reason: (!origin.reason_truncated).then(|| origin.reason.clone()),
            reason_omitted: origin.reason_truncated.then_some(1),
        }),
        status: ShowStatus {
            work: ShowWorkSummary {
                short_ref: work.short_ref.clone(),
                title: work.title.clone(),
                outcome: view.outcome.clone(),
                acceptance: work.acceptance.clone(),
                acceptance_omitted: (work.acceptance_count > work.acceptance.len())
                    .then(|| work.acceptance_count - work.acceptance.len()),
                kind: work.kind,
                priority: work.priority,
                labels: work.labels.clone(),
                assigned_to: work
                    .assigned_to
                    .as_deref()
                    .map(|actor| identity.actor(actor)),
                lifecycle: work.lifecycle,
                restored: work.restored,
                superseded_by: work.superseded_by.map(short_ref_for_work_id),
                child_resolution: super::child_obligations::ShowChildSuccessor::for_work(work),
                child_requirement: work.parent_id.map(|_| work.child_requirement),
            },
            availability: view.status.availability,
        },
        holder,
        held_until,
        children: view.children.iter().map(show_relation).collect(),
        children_omitted: (view.child_count > view.children.len())
            .then(|| view.child_count - view.children.len()),
        child_obligations: view
            .child_obligations
            .as_ref()
            .map(|groups| ShowChildObligations::new(groups, work)),
        prerequisites: view.prerequisites.iter().map(show_relation).collect(),
        handoffs: view
            .handoffs
            .iter()
            .filter(|offer| offer.state == WorkHandoffState::Offered && offer.expires_at > now)
            .map(|offer| ShowHandoff {
                from: identity.session(&offer.from),
                to: identity.session(&offer.to),
                expires_at: offer.expires_at,
            })
            .collect(),
        blockers: view
            .blockers
            .iter()
            .map(|blocker| ShowBlocker {
                kind: blocker.kind,
                detail: blocker.detail.clone(),
            })
            .collect(),
        notes_omitted: (view.evidence_count > notes.len())
            .then(|| view.evidence_count - notes.len()),
        notes,
        history: ShowHistory {
            total: view.history.total,
            omitted: view.history.omitted.saturating_add(hidden_history),
            items: history,
        },
        restored_history,
        allowed_next: view.allowed_next.clone(),
        omissions: view.omissions.clone(),
    }
}

fn show_evidence(view: &WorkFocusView) -> Vec<crate::work_service::WorkEvidenceSummary> {
    let mut notes = view.evidence_items.clone();
    if let Some(latest) = view.latest_evidence_item.as_ref() {
        if let Some(index) = notes
            .iter()
            .position(|note| note.evidence == latest.evidence)
        {
            notes.remove(index);
        } else if notes.len() == crate::work_service::MAX_FOCUS_RELATIONS {
            notes.pop();
        }
        notes.push(latest.clone());
    }
    notes
}

pub(super) fn show_notes(view: &WorkFocusView, identity: DisplayIdentity<'_>) -> Vec<ShowNote> {
    show_evidence(view)
        .into_iter()
        .map(|note| ShowNote {
            kind: note.evidence_kind,
            non_holder: note.non_holder,
            summary: note.summary,
            refs: Vec::new(),
            by: note
                .display_actor_id
                .as_deref()
                .or(note.actor_id.as_deref())
                .map(|actor| {
                    actor_label(
                        &identity.author(actor, note.display_actor_session_id.as_ref()),
                        note.actor_context.as_deref(),
                    )
                }),
            created_at: note.created_at,
        })
        .collect()
}

pub(super) fn show_item_line(
    status: &ReadyWorkSummary,
    holder: Holder<'_>,
    completed_by_record: bool,
    identity: DisplayIdentity<'_>,
    now: DateTime<Utc>,
) -> String {
    // Unlike the shared item_line used by lists, terse show deliberately
    // renders a peer holder as relative session identity.
    let work = &status.work;
    let state = match holder {
        Holder::You(expires_at) => format!("held by you until {}", clock(expires_at, now)),
        Holder::Other(session, expires_at, _) => {
            format!(
                "held by {} until {}",
                identity.session(session),
                clock(expires_at, now)
            )
        }
        Holder::Nobody if completed_by_record => "completed (restored)".into(),
        Holder::Nobody => availability_words(status).to_owned(),
    };
    format!("{} \"{}\" — {state}", work.short_ref, short(&work.title))
}
