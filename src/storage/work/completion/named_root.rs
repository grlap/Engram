//! Named-root freshness at completion: the claim's active source-root binding,
//! which obligations it governs, and how stock source-change obligations from
//! another workspace are displaced or kept open.

use super::{
    CompletionObligationBinding, Connection, DateTime, NamedRootBindingEvent, NamedRootBindingKind,
    ObjectId, SCHEMA_VERSION, StoreError, Transaction, Utc, WorkClaimId, WorkItem, WorkObligation,
    WorkObligationResolution, WorkObligationResolutionEvent, WorkObligationState, WorkRunId,
    append_obligation_resolution_on, latest_claim_release_on, latest_named_root_binding_on,
    latest_named_root_sighting_on, latest_unlocated_source_change_on, load_source_observation_on,
    load_typed_work_object, load_work_obligation_records_on, named_root_event_on,
};
use crate::domain::SourceObservation;

/// How the active root accounts for a source change that no check in the root
/// verified. The class depends only on where and when the change was sighted,
/// never on which rule opened its obligation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ForeignChange {
    /// Recorded in the root's workspace, or with no source basis while no
    /// root was bound on its claim: the root's own rules apply, or with no
    /// active root the rules for a claim without one.
    NotForeign,
    /// Captured in another workspace before the root: before the claim named
    /// any root, or in the workspace of an earlier name that was still bound.
    /// Displaced and disclosed, neither verified nor waived.
    Displaced,
    /// Captured in a workspace foreign to the root that was bound when it was
    /// recorded, now or under an earlier name. Only an explicit human waiver
    /// accounts for it; a later name, an end or a release never does.
    Open,
    /// Recorded on the run feed after its generation ended or was released:
    /// unbound, and the rules for a claim without a root apply. A later
    /// matching check satisfies it; without one the stock rule waives it as
    /// untested at completion, and an operator rule keeps it open.
    Unbound,
    /// Recorded with no source basis while a root was bound on its claim.
    /// Only a fresh check in the claim's active named root after it, or an
    /// explicit human waiver, accounts for it, whatever the claim names later.
    Unlocated,
}

/// Classifies `change`, the source change that opened a stock obligation at
/// `change_position`, against the active `root`, or against its history alone
/// while the claim names no root. Whether a generation had ended or been
/// released is read from the events recorded before the change on the run
/// feed.
pub(super) fn classify_change_on(
    connection: &Connection,
    root: Option<&NamedRootContext>,
    change: &SourceObservation,
    change_position: i64,
) -> Result<ForeignChange, StoreError> {
    let Some(basis) = change.source_basis.as_ref() else {
        // A change whose root cannot be established is placed by its run-feed
        // position: recorded while a root was bound, it keeps needing a fresh
        // named-root check after an end, a release or a later name.
        let bound = latest_named_root_binding_on(
            connection,
            change.binding.run_id,
            change.binding.claim_id,
            change_position,
        )?
        .is_some_and(|(_, _, binding)| binding.kind == NamedRootBindingKind::Bound);
        return Ok(if bound {
            ForeignChange::Unlocated
        } else {
            ForeignChange::NotForeign
        });
    };
    let Some(root) = root else {
        // With no active root only the history remains: a change foreign to
        // the root bound when it was recorded keeps needing its human waiver
        // after that root ended or the claim was released.
        return Ok(match stated_root_on(connection, change, change_position)? {
            Some(bound_workspace) if bound_workspace != basis.workspace_id => ForeignChange::Open,
            _ => ForeignChange::NotForeign,
        });
    };
    // The current root's workspace decides only what its history leaves
    // open: a change foreign to the root bound when it was recorded stays
    // foreign even when the claim later names that very workspace.
    let in_root = basis.workspace_id == root.binding.workspace_id;
    let outside_root = |class| {
        if in_root {
            ForeignChange::NotForeign
        } else {
            class
        }
    };
    let Some(generation) = basis.source_root_generation else {
        return Ok(outside_root(ForeignChange::Displaced));
    };
    if generation >= root.binding.generation {
        return Ok(outside_root(ForeignChange::Open));
    }
    // Under a still-bound earlier name, a change already foreign to that
    // name's own root keeps needing the human waiver it needed then, whatever
    // the claim names now; a later name displaces a change made in that root.
    Ok(match stated_root_on(connection, change, change_position)? {
        None => outside_root(ForeignChange::Unbound),
        Some(bound_workspace) if bound_workspace == basis.workspace_id => {
            outside_root(ForeignChange::Displaced)
        }
        Some(_) => ForeignChange::Open,
    })
}

/// The workspace of the root `change` states, when that generation was bound
/// on the change's claim and neither ended nor released before the change's
/// run-feed position; `None` for a change stating no named generation.
fn stated_root_on(
    connection: &Connection,
    change: &SourceObservation,
    change_position: i64,
) -> Result<Option<String>, StoreError> {
    let Some(basis) = change.source_basis.as_ref() else {
        return Ok(None);
    };
    let Some(generation) = basis.source_root_generation else {
        return Ok(None);
    };
    if basis.source_root_state != Some(crate::domain::SourceRootState::Named) {
        return Ok(None);
    }
    let run_id = change.binding.run_id;
    let claim_id = change.binding.claim_id;
    let Some((bound, bound_workspace)) = named_root_event_on(
        connection,
        run_id,
        claim_id,
        generation,
        "bound",
        change_position,
    )?
    else {
        return Ok(None);
    };
    let ended = named_root_event_on(
        connection,
        run_id,
        claim_id,
        generation,
        "ended",
        change_position,
    )?;
    let released = latest_claim_release_on(connection, run_id, claim_id, change_position)?;
    if ended.is_some() || released.is_some_and(|released| released > bound) {
        return Ok(None);
    }
    Ok(Some(bound_workspace))
}

/// Whether the active `root` displaces `change`; see [`classify_change_on`].
pub(super) fn displaces_on(
    connection: &Connection,
    root: &NamedRootContext,
    change: &SourceObservation,
    change_position: i64,
) -> Result<bool, StoreError> {
    Ok(
        classify_change_on(connection, Some(root), change, change_position)?
            == ForeignChange::Displaced,
    )
}

pub(super) fn displaced_source_changes_on(
    connection: &Connection,
    run_id: WorkRunId,
    obligations: &[CompletionObligationBinding],
) -> Result<Vec<ObjectId>, StoreError> {
    // Several rules may open obligations for one change; the seal names it
    // once, in the order its first displaced obligation is bound.
    let mut changes = Vec::new();
    for binding in obligations {
        let resolution: WorkObligationResolutionEvent = load_typed_work_object(
            connection,
            &binding.resolution,
            "work_obligation_resolution",
        )?;
        if matches!(
            resolution.resolution,
            WorkObligationResolution::Displaced { .. }
        ) {
            let obligation: WorkObligation =
                load_typed_work_object(connection, &binding.definition, "work_obligation")?;
            if obligation.run_id != run_id || resolution.obligation_id != obligation.obligation_id {
                return Err(StoreError::InvalidWorkProjection(
                    "displaced source-change resolution crosses its obligation".into(),
                ));
            }
            if !changes.contains(&obligation.triggering_observation) {
                changes.push(obligation.triggering_observation);
            }
        }
    }
    Ok(changes)
}

pub(super) struct NamedRootContext {
    pub(super) binding_position: i64,
    pub(super) binding_id: ObjectId,
    pub(super) binding: NamedRootBindingEvent,
    pub(super) latest_sighting: Option<(i64, SourceObservation)>,
    pub(super) latest_mutation: Option<(i64, SourceObservation)>,
    pub(super) unknown_change_position: Option<i64>,
}

impl NamedRootContext {
    pub(super) fn match_input(&self) -> crate::control::NamedRootEvidenceMatch<'_> {
        crate::control::NamedRootEvidenceMatch {
            workspace_id: &self.binding.workspace_id,
            generation: self.binding.generation,
            binding_position: self.binding_position,
            latest_sighting: self
                .latest_sighting
                .as_ref()
                .map(|(position, sighting)| (sighting, *position)),
            unknown_change_position: self.unknown_change_position,
        }
    }
}

pub(super) fn named_root_context_on(
    connection: &Connection,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    cut: i64,
) -> Result<Option<NamedRootContext>, StoreError> {
    let Some((position, binding_id, binding)) =
        latest_named_root_binding_on(connection, run_id, claim_id, cut)?
    else {
        return Ok(None);
    };
    if binding.kind != NamedRootBindingKind::Bound {
        return Ok(None);
    }
    let latest_sighting = latest_named_root_sighting_on(
        connection,
        run_id,
        &binding.workspace_id,
        binding.generation,
        cut,
        false,
    )?;
    let latest_mutation = latest_named_root_sighting_on(
        connection,
        run_id,
        &binding.workspace_id,
        binding.generation,
        cut,
        true,
    )?;
    let unknown_change_position =
        latest_unlocated_source_change_on(connection, run_id, position.position, cut)?;
    Ok(Some(NamedRootContext {
        binding_position: position.position,
        binding_id,
        binding,
        latest_sighting,
        latest_mutation,
        unknown_change_position,
    }))
}

/// Whether a check may satisfy `obligation`: in the active `root`, or with no
/// root, any check the unbound rules admit. A change recorded with no source
/// basis while a root was bound needs a check in an active named root, and an
/// unbound change is satisfied by a later matching check as without a root.
pub(super) fn obligation_matches_named_root(
    connection: &Connection,
    obligation: &WorkObligation,
    root: Option<&NamedRootContext>,
) -> Result<bool, StoreError> {
    if crate::control::acceptance_binding_criterion(&obligation.rule).is_some() {
        return Ok(true);
    }
    let trigger = load_source_observation_on(connection, &obligation.triggering_observation)?;
    // A change the root accounts for itself, an unbound one, or with no root a
    // change no earlier root left foreign or unlocated, can be satisfied by a
    // check; a displaced or foreign one never is. See [`classify_change_on`].
    Ok(
        match classify_change_on(
            connection,
            root,
            &trigger,
            obligation.trigger_position.position,
        )? {
            ForeignChange::NotForeign | ForeignChange::Unbound => true,
            ForeignChange::Unlocated => root.is_some(),
            ForeignChange::Displaced | ForeignChange::Open => false,
        },
    )
}

/// Resolves the source-change obligations still open on `run_id`: no matching
/// passing check followed their change. Each is classified by
/// [`classify_change_on`], whichever rule opened it: a displaced change is
/// resolved as displaced under the stock rule and an operator-selected one
/// alike. Only the stock rule records the rest: an open foreign change, which
/// a root bound when it was recorded left foreign, and an unknown-root change
/// recorded while a root was bound stay open for the barrier that follows,
/// even if that root has since ended or been renamed, and every other one is
/// waived in the completing actor's name, with a reason naming the change and
/// its source revision for the host record. An operator rule's obligation
/// stays open for its check or an operator waiver.
pub(super) fn resolve_source_change_obligations_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    actor: &crate::domain::ActorContext,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let root = named_root_context_on(transaction, run_id, claim_id, i64::MAX)?;
    for record in
        load_work_obligation_records_on(transaction, run_id, Some(WorkObligationState::Open))?
    {
        if !crate::control::is_source_change_obligation(&record.obligation.rule) {
            continue;
        }
        let stock = crate::control::is_stock_source_change_obligation(
            &record.obligation.rule,
            &record.obligation.requirement,
        );
        let change =
            load_source_observation_on(transaction, &record.obligation.triggering_observation)?;
        let class = classify_change_on(
            transaction,
            root.as_ref(),
            &change,
            record.obligation.trigger_position.position,
        )?;
        let displacement = match (class, root.as_ref(), change.source_basis.as_ref()) {
            (ForeignChange::Displaced, Some(root), Some(basis)) => {
                Some(WorkObligationResolution::Displaced {
                    binding: root.binding_id.clone(),
                    trigger_workspace_id: basis.workspace_id.clone(),
                })
            }
            _ => None,
        };
        if displacement.is_none() && !stock {
            // An operator rule was selected for a reason: its obligation
            // keeps blocking until its check or an operator waiver.
            continue;
        }
        if matches!(class, ForeignChange::Open | ForeignChange::Unlocated) {
            // A foreign change recorded under a bound name, or an unknown-root
            // change captured while a root was bound, has no automatic
            // disposition: it cannot be relabeled as an ordinary untested
            // change. The barrier that follows refuses completion until a
            // fresh named-root check or an explicit human waiver accounts for
            // it, as its class allows.
            continue;
        }
        let revision = change.source_basis.as_ref().map_or_else(
            || "no recorded source revision".to_owned(),
            |basis| format!("source revision {}", basis.source_revision),
        );
        let event = WorkObligationResolutionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            obligation_id: record.obligation.obligation_id,
            definition: record.definition_id.clone(),
            run_id,
            resolution: displacement.unwrap_or_else(|| WorkObligationResolution::Waived {
                waived_by: actor.actor_id.clone(),
                reason: format!(
                    "completed at revision {} with no matching passing test after source change {} ({revision})",
                    item.revision, change.label
                ),
            }),
            actor: actor.clone(),
            created_at: now,
        };
        append_obligation_resolution_on(transaction, &record, &event)?;
    }
    Ok(())
}

pub(super) fn refuse_unresolved_named_root_changes_on(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
) -> Result<(), StoreError> {
    let root = named_root_context_on(connection, run_id, claim_id, i64::MAX)?;
    for record in
        load_work_obligation_records_on(connection, run_id, Some(WorkObligationState::Open))?
    {
        if !crate::control::is_stock_source_change_obligation(
            &record.obligation.rule,
            &record.obligation.requirement,
        ) {
            continue;
        }
        let change =
            load_source_observation_on(connection, &record.obligation.triggering_observation)?;
        let class = classify_change_on(
            connection,
            root.as_ref(),
            &change,
            record.obligation.trigger_position.position,
        )?;
        let reason = match (class, change.source_basis.as_ref()) {
            (ForeignChange::Open, Some(basis)) => {
                format!(
                    "source change {} was captured in foreign workspace {} while a named root was bound; a named-root check cannot satisfy it, so an explicit human waiver is required",
                    change.label, basis.workspace_id
                )
            }
            (ForeignChange::Unlocated, _) if root.is_some() => format!(
                "source change {} has unknown workspace and was recorded while a named root was bound; run a fresh check in the named root or record an explicit human waiver",
                change.label
            ),
            (ForeignChange::Unlocated, _) => format!(
                "source change {} has unknown workspace and was recorded while a named root was bound; name a root and run a fresh check in it, or record an explicit human waiver",
                change.label
            ),
            _ => continue,
        };
        return Err(StoreError::WorkCompletionRefused {
            work: item.work_id,
            reason,
        });
    }
    Ok(())
}
