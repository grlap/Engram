//! Small recovery projection for compact peek; ordinary delivery stays intact.

use super::receipts::CompactNextReceipt;
use super::{
    Serialize, WorkFocusView, WorkLifecycle, WorkPrerequisiteState, WorkSectionOmissionReason,
};

pub(super) const DETAILS_COMMAND: &str = "engram work next --peek --verbose";
pub(super) const CATALOG_COMMAND: &str = "engram work ls --all --limit 20";

#[derive(Clone, Serialize)]
pub(super) struct Recovery {
    objective: String,
    objective_complete: bool,
    acceptance_count: usize,
    acceptance_shown: usize,
    detail: String,
    evidence_detail: String,
    blockers: usize,
    unresolved_prerequisites: usize,
    unfinished_children: usize,
    dependency_preview: Vec<String>,
}

impl Recovery {
    pub(super) fn from_focus(focus: &WorkFocusView) -> Self {
        let omitted = |reason| {
            focus
                .omissions
                .iter()
                .filter(|row| row.reason == reason)
                .map(|row| row.omitted_count)
                .sum::<usize>()
        };
        let prerequisites = focus
            .prerequisites
            .iter()
            .filter(|row| {
                matches!(
                    row.prerequisite_state,
                    Some(WorkPrerequisiteState::Pending | WorkPrerequisiteState::Dead)
                )
            })
            .collect::<Vec<_>>();
        let children = focus
            .children
            .iter()
            .filter(|row| matches!(row.lifecycle, WorkLifecycle::Open | WorkLifecycle::Proposed))
            .collect::<Vec<_>>();
        let dependency_preview = prerequisites
            .iter()
            .map(|row| {
                format!(
                    "{} ({})",
                    row.short_ref,
                    if row.prerequisite_state == Some(WorkPrerequisiteState::Dead) {
                        "dead prerequisite"
                    } else {
                        "pending prerequisite"
                    }
                )
            })
            .chain(
                children
                    .iter()
                    .map(|row| format!("{} (unfinished child)", row.short_ref)),
            )
            .chain(focus.blockers.iter().map(|row| super::short(&row.detail)))
            .take(3)
            .collect();
        Self {
            objective: focus.outcome.clone(),
            objective_complete: focus.outcome.len() == focus.outcome_stored_bytes,
            acceptance_count: focus.status.work.acceptance_count,
            acceptance_shown: 0,
            detail: format!("engram work show {} --full", focus.status.work.short_ref),
            evidence_detail: format!(
                "engram work show {} --notes --gates",
                focus.status.work.short_ref
            ),
            blockers: focus.blocker_count,
            unresolved_prerequisites: prerequisites.len()
                + omitted(WorkSectionOmissionReason::PendingPrerequisiteCountLimit)
                + omitted(WorkSectionOmissionReason::DeadPrerequisiteCountLimit),
            unfinished_children: children.len()
                + omitted(WorkSectionOmissionReason::UnfinishedChildCountLimit),
            dependency_preview,
        }
    }

    pub(super) fn shed_preview(&mut self) -> bool {
        self.dependency_preview.pop().is_some()
    }

    pub(super) fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!(
                "  objective{}: {}",
                if self.objective_complete {
                    ""
                } else {
                    " (preview)"
                },
                super::terminal_safe_line(&self.objective)
            ),
            format!(
                "  acceptance: {} criteria, none shown; {} (a new read)",
                self.acceptance_count, self.detail
            ),
            format!(
                "  evidence and recorded constraints: {} (a new read)",
                self.evidence_detail
            ),
            format!(
                "  dependencies: {} blockers, {} unresolved prerequisites, {} unfinished children ({} previews shown)",
                self.blockers,
                self.unresolved_prerequisites,
                self.unfinished_children,
                self.dependency_preview.len()
            ),
        ];
        lines.extend(
            self.dependency_preview
                .iter()
                .map(|row| format!("    {}", super::terminal_safe_line(row))),
        );
        lines
    }
}

/// Keep actionable duties, replace repeated broad history with inspection routes.
pub(super) fn prepare(compact: &mut CompactNextReceipt) {
    compact.discovery.participated_omitted += compact.discovery.participated.len();
    compact.discovery.participated.clear();
    // Select before receipt fitting or held-focus consolidation sheds rows.
    let relevant = compact
        .focus
        .iter()
        .map(|row| row.work_ref.as_str())
        .chain(compact.held.iter().map(|row| row.work_ref.as_str()))
        .chain(
            compact
                .discovery
                .incoming_handoffs
                .iter()
                .map(|row| row.work_ref.as_str()),
        )
        .chain(
            compact
                .discovery
                .assigned
                .iter()
                .map(|row| row.work_ref.as_str()),
        )
        .collect::<Vec<_>>();
    let before = compact.changes.len();
    compact.changes.retain(|change| {
        change
            .subject
            .as_deref()
            .is_some_and(|subject| relevant.contains(&subject))
    });
    compact.changes.truncate(4);
    let changes = before - compact.changes.len();
    if changes > 0 {
        if let Some(peek) = &mut compact.peek {
            peek.more_changes_available = true;
        }
        omit(compact, "changes", changes);
    }
    let ready_limit = if compact
        .focus
        .as_ref()
        .is_some_and(|focus| focus.holder.as_deref() == Some("you"))
    {
        1
    } else {
        super::MAX_NEXT_READY_CANDIDATES as usize
    };
    let ready_omitted = compact.ready.len().saturating_sub(ready_limit);
    compact.ready.truncate(ready_limit);
    if ready_limit == 1
        && let Some(navigation) = &mut compact.ready_navigation
    {
        navigation.limit = 1;
    }
    if ready_omitted > 0 {
        omit(compact, "ready", ready_omitted);
    }
    if let Some(focus) = &mut compact.focus {
        shorten(&mut focus.current_status, &mut focus.status_observation);
        compact
            .held
            .retain(|row| row.work_ref != focus.work_ref || !same_statuses(row, focus));
    }
    for row in &mut compact.held {
        shorten(&mut row.current_status, &mut row.status_observation);
    }
    for row in &mut compact.discovery.assigned {
        shorten(&mut row.current_status, &mut row.status_observation);
    }
    let mut next = vec![super::memory_recovery::listing_command(
        compact.peek.as_ref(),
        compact.context_generation.as_deref(),
    )];
    if let Some(focus) = &compact.focus {
        for status in focus
            .current_status
            .iter()
            .chain(&focus.status_observation)
            .filter(|status| !status.complete)
        {
            next.push(format!(
                "engram work show {} --note {}",
                focus.work_ref, status.locator
            ));
        }
    }
    // Full scope and evidence reads already live in recovery's detail fields.
    // Keep lifecycle actions ahead of catalog navigation, preserving their
    // existing order. Only the focus's redundant plain show is replaced.
    let redundant_show = compact
        .focus
        .as_ref()
        .map(|focus| format!("engram work show {}", focus.work_ref));
    for reads in [false, true] {
        for command in &compact.guidance.next {
            if command.starts_with("engram work show ") == reads
                && redundant_show.as_ref() != Some(command)
                && !next.contains(command)
            {
                next.push(command.clone());
            }
        }
    }
    compact.guidance.next = next;
}

pub(super) fn incoming_handoffs_value(compact: &CompactNextReceipt) -> Option<super::Value> {
    let discovery = &compact.discovery;
    if discovery.incoming_handoffs.is_empty() && discovery.incoming_handoffs_omitted == 0 {
        return None;
    }
    let rows = discovery
        .incoming_handoffs
        .iter()
        .map(|offer| {
            let mut value = super::json!(offer);
            value["detail"] = super::json!(format!("engram work show {}", offer.work_ref));
            value
        })
        .collect::<Vec<_>>();
    Some(super::json!({
        "items": rows,
        "omitted": discovery.incoming_handoffs_omitted,
        "catalog_detail": CATALOG_COMMAND,
    }))
}

pub(super) fn incoming_handoffs_lines(compact: &CompactNextReceipt) -> Vec<String> {
    let discovery = &compact.discovery;
    if discovery.incoming_handoffs.is_empty() && discovery.incoming_handoffs_omitted == 0 {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "incoming handoffs ({} shown):",
        discovery.incoming_handoffs.len()
    )];
    for offer in &discovery.incoming_handoffs {
        lines.push(format!(
            "  {}: {} — expires {}; engram work show {}",
            offer.work_ref,
            super::terminal_safe_line(&offer.title),
            offer.expires_at.to_rfc3339(),
            offer.work_ref
        ));
    }
    if discovery.incoming_handoffs_omitted > 0 {
        lines.push(format!(
            "  ({} more incoming handoffs not shown); inspect catalog: {}",
            discovery.incoming_handoffs_omitted, CATALOG_COMMAND
        ));
    }
    lines
}

fn shorten(
    current: &mut Option<crate::work_service::WorkCurrentStatus>,
    peer: &mut Option<crate::work_service::WorkCurrentStatus>,
) {
    for status in current.iter_mut().chain(peer) {
        if status.body_or_first_line.len() > 192 {
            let mut end = 192 - '…'.len_utf8();
            while !status.body_or_first_line.is_char_boundary(end) {
                end -= 1;
            }
            status.body_or_first_line.truncate(end);
            status.body_or_first_line.push('…');
            status.complete = false;
        }
    }
}

fn same_statuses(a: &super::receipts::CompactWorkRow, b: &super::receipts::CompactWorkRow) -> bool {
    [
        (&a.current_status, &b.current_status),
        (&a.status_observation, &b.status_observation),
    ]
    .iter()
    .all(|(a, b)| match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.identity.is_some() && a.identity == b.identity,
        _ => false,
    })
}

fn omit(compact: &mut CompactNextReceipt, section: &str, omitted_count: usize) {
    compact
        .omissions
        .push(super::receipts::CompactSectionOmission {
            section: section.into(),
            reason: WorkSectionOmissionReason::CountLimit,
            omitted_count,
        });
}
