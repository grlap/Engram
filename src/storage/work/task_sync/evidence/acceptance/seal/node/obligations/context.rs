//! Original feed context for the existing root classifier and obligation matcher.
use crate::domain::{
    EnvironmentEvidence, ExecutionSourceBasis, NamedRootBindingEvent, NamedRootBindingKind,
    SourceObservation, SourceRootState, VerificationSourceBasis, WorkClaimId, WorkEvent,
    WorkTransition,
};
use crate::storage::work::acceptance_evaluation::source::within_named_root;
use crate::storage::work::completion::{ForeignChange, classify_change_with_history};
use crate::storage::work::task_sync::evidence::acceptance::movement;
use crate::storage::work::task_sync::evidence::acceptance::{History, Result, invalid};
use crate::storage::work::task_sync::evidence::store_error;
use crate::{FeedId, ObjectId};

pub(super) struct Root {
    pub(super) position: i64,
    pub(super) id: ObjectId,
    pub(super) event: NamedRootBindingEvent,
}

pub(super) fn root_at(
    history: &History<'_>,
    feed: &FeedId,
    claim: WorkClaimId,
    through: i64,
) -> Result<Option<Root>> {
    let mut selected = None;
    if !history.walk(feed, through, |position, id, kind| {
        if kind == "named_root_binding" {
            let event: NamedRootBindingEvent = history.load(id, kind)?;
            if event.claim_id == claim {
                if event.project_id != *history.project || *feed != FeedId::RunExecution(event.run_id) {
                    return Err(invalid("historical named root crosses its feed scope"));
                }
                selected = Some(Root { position, id: id.clone(), event });
            }
        } else if kind == "work_event" {
            let event: WorkEvent = history.load(id, kind)?;
            if matches!(event.transition, WorkTransition::Released { claim_id, .. } if claim_id == claim) {
                selected = None;
            }
        }
        Ok(())
    })? {
        return Err(invalid("historical named-root interval is incomplete"));
    }
    Ok(selected.filter(|root| root.event.kind == NamedRootBindingKind::Bound))
}

fn stated_root(
    history: &History<'_>,
    feed: &FeedId,
    change: &SourceObservation,
    through: i64,
) -> Result<Option<String>> {
    let Some(basis) = change.source_basis.as_ref() else {
        return Ok(None);
    };
    let Some(generation) = basis.source_root_generation else {
        return Ok(None);
    };
    if basis.source_root_state != Some(SourceRootState::Named) {
        return Ok(None);
    }
    let mut bound = None;
    let mut ended = false;
    let mut released = None;
    if !history.walk(feed, through, |position, id, kind| {
        if kind == "named_root_binding" {
            let event: NamedRootBindingEvent = history.load(id, kind)?;
            if event.claim_id == change.binding.claim_id && event.generation == generation {
                match event.kind {
                    NamedRootBindingKind::Bound => bound = Some((position, event.workspace_id)),
                    NamedRootBindingKind::Ended => ended = true,
                }
            }
        } else if kind == "work_event" {
            let event: WorkEvent = history.load(id, kind)?;
            if matches!(event.transition, WorkTransition::Released { claim_id, .. } if claim_id == change.binding.claim_id) {
                released = Some(position);
            }
        }
        Ok(())
    })? { return Err(invalid("historical root-state interval is incomplete")); }
    Ok(bound
        .filter(|(at, _)| !ended && released.is_none_or(|released| released <= *at))
        .map(|(_, workspace)| workspace))
}

pub(super) fn classify(
    history: &History<'_>,
    feed: &FeedId,
    root: Option<&Root>,
    change: &SourceObservation,
    position: i64,
) -> Result<ForeignChange> {
    Ok(classify_change_with_history(
        root.map(|root| &root.event),
        change,
        || {
            root_at(history, feed, change.binding.claim_id, position)
                .map(|root| root.is_some())
                .map_err(store_error)
        },
        || stated_root(history, feed, change, position).map_err(store_error),
    )?)
}

pub(super) struct Sources {
    pub(super) latest_mutation: Option<(i64, SourceObservation)>,
    pub(super) latest_sighting: Option<(i64, SourceObservation)>,
    pub(super) unknown_change: Option<i64>,
    pub(super) measured: Option<(i64, VerificationSourceBasis)>,
    pub(super) judged: Option<ExecutionSourceBasis>,
}

pub(super) fn sources(
    history: &History<'_>,
    feed: &FeedId,
    root: Option<&Root>,
    cut: i64,
) -> Result<Sources> {
    let mut context = Sources {
        latest_mutation: None,
        latest_sighting: None,
        unknown_change: None,
        measured: None,
        judged: None,
    };
    if !history.walk(feed, cut, |position, id, kind| {
        if !matches!(
            kind,
            "execution_observation" | "unadmitted_execution_observation"
        ) {
            return Ok(());
        }
        let Some(observation) = movement::observation(history, id, kind)? else {
            return Ok(());
        };
        let in_root = root.is_none_or(|root| {
            observation
                .source_basis
                .as_ref()
                .is_some_and(|basis| within_named_root(&root.event, basis))
        });
        if in_root {
            if observation.source_changed {
                context.latest_mutation = Some((position, observation.clone()));
            }
            context.latest_sighting = Some((position, observation.clone()));
        }
        if let Some(root) = root
            && position > root.position
            && observation.source_changed
            && observation.source_basis.is_none()
        {
            context.unknown_change = Some(position);
        }
        Ok(())
    })? {
        return Err(invalid("historical source-match interval is incomplete"));
    }
    if root.is_some() {
        context.judged = context
            .latest_sighting
            .as_ref()
            .and_then(|(_, sighting)| sighting.source_basis.as_ref())
            .and_then(VerificationSourceBasis::legacy_basis);
    } else {
        let workspace = context
            .latest_mutation
            .as_ref()
            .filter(|(_, mutation)| !mutation.admitted)
            .and_then(|(_, mutation)| mutation.source_basis.as_ref())
            .map(|basis| basis.workspace_id.as_str());
        if !history.walk(feed, cut, |position, id, kind| {
            let basis: Option<VerificationSourceBasis> = if kind == "environment_evidence" {
                let evidence: EnvironmentEvidence = history.load(id, kind)?;
                Some(evidence.source_basis.into())
            } else if matches!(
                kind,
                "execution_observation" | "unadmitted_execution_observation"
            ) {
                movement::observation(history, id, kind)?
                    .and_then(|observation| observation.source_basis)
            } else {
                None
            };
            if let Some(basis) = basis {
                context.judged = basis.legacy_basis();
                if workspace.is_some_and(|workspace| basis.workspace_id == workspace) {
                    context.measured = Some((position, basis));
                }
            }
            Ok(())
        })? {
            return Err(invalid("historical measured-source interval is incomplete"));
        }
    }
    Ok(context)
}
