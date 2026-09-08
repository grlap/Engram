//! Explicit author citations reuse immutable run evidence; never recapture it.

use super::{
    ActorContext, LocalWorkService, ObjectHash, SqliteStore, StoreError, WorkAcceptanceInput,
    WorkClaim, WorkCompleteInput, WorkItem,
};

pub(super) fn validate_shape(input: &WorkCompleteInput) -> Result<(), StoreError> {
    let reason = if input.links.len() > super::super::MAX_CRITERION_LINKS {
        Some(
            "a completion accepts at most 64 explicit criterion links; reduce the requested mapping",
        )
    } else if input.links.is_empty() && input.link_basis.is_some() {
        Some("link_basis is only meaningful with explicit links")
    } else if !input.links.is_empty() && input.link_basis.is_none() {
        Some("links require link_basis from show; reading does not save a basis for a later write")
    } else if !input.links.is_empty() && input.acceptance.is_some() {
        Some("explicit acceptance and positional links cannot be combined")
    } else {
        None
    };
    reason.map_or(Ok(()), |reason| {
        Err(StoreError::WorkCriterionLinkInvalid {
            criterion: None,
            reason,
        })
    })
}

pub(super) fn acceptance(
    store: &SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    input: &WorkCompleteInput,
    actor: &ActorContext,
    evidence: &[ObjectHash],
) -> Result<Option<Vec<WorkAcceptanceInput>>, StoreError> {
    if input.links.is_empty() {
        return Ok(input.acceptance.clone());
    }
    let first = input.links[0].criterion;
    // The caller must send the basis it READ. A fresh internal read or a
    // remembered read-side session value would not guard positional intent.
    if input.link_basis != Some(work.revision) {
        return Err(StoreError::WorkCriterionLinkInvalid {
            criterion: (first > 0).then_some(first),
            reason: "the work/acceptance basis changed since you read it; re-read show and re-link",
        });
    }
    let mut results = LocalWorkService::prevalidate_completion_acceptance(
        work,
        None,
        input.note.as_deref(),
        evidence,
        actor.assurance,
        &actor.actor_id,
    )?;
    let index = store.work_record_index(
        &work.project_id,
        work.work_id,
        crate::storage::WorkRecordKind::NotesWithGates,
    )?;
    for link in &input.links {
        let Some(result) = link
            .criterion
            .checked_sub(1)
            .and_then(|index| results.get_mut(index))
        else {
            return Err(StoreError::WorkCriterionLinkInvalid {
                criterion: (link.criterion > 0).then_some(link.criterion),
                reason: "criterion position is outside the acceptance list; read show for its one-based positions",
            });
        };
        let hash = store.resolve_criterion_evidence(
            &work.project_id,
            work.work_id,
            claim.run_id,
            link.criterion,
            &link.locator,
            &index,
        )?;
        if !evidence.contains(&hash) {
            return Err(StoreError::WorkCriterionLinkInvalid {
                criterion: (link.criterion > 0).then_some(link.criterion),
                reason: "the note is outside the explicitly requested completion evidence set; choose evidence included in that set",
            });
        }
        result.evidence.push(hash);
        result.evidence.sort();
        result.evidence.dedup();
    }
    Ok(Some(
        results
            .into_iter()
            .map(|result| WorkAcceptanceInput {
                criterion: Some(result.criterion),
                satisfied: result.satisfied,
                evidence: result
                    .evidence
                    .into_iter()
                    .map(|hash| hash.to_string())
                    .collect(),
                note: result.note,
            })
            .collect(),
    ))
}

/// Both normalization stages return to the caller's single recovery handler.
pub(super) fn validated_acceptance(
    store: &SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    input: &WorkCompleteInput,
    actor: &ActorContext,
    evidence: &[ObjectHash],
) -> Result<Vec<crate::AcceptanceResult>, StoreError> {
    acceptance(store, work, claim, input, actor, evidence).and_then(|supplied| {
        LocalWorkService::prevalidate_completion_acceptance(
            work,
            supplied.as_deref(),
            if input.links.is_empty() {
                input.note.as_deref()
            } else {
                None
            },
            evidence,
            actor.assurance,
            &actor.actor_id,
        )
    })
}

pub(super) fn frozen(input: &WorkCompleteInput) -> Result<(), StoreError> {
    if let Some(link) = input.links.first() {
        return Err(StoreError::WorkCriterionLinkInvalid {
            criterion: (link.criterion > 0).then_some(link.criterion),
            reason: "completion is frozen; new or late evidence links cannot change its seal; read the recorded links instead",
        });
    }
    Ok(())
}

/// A pending request is not a committed receipt. Recover only its original
/// read/claim basis and complete requested acceptance mapping, never whatever
/// different completion happened to seal the run after a refusal.
pub(super) fn validate_recovered_seal(
    basis: &super::WorkProtocolBasis,
    input: &WorkCompleteInput,
    seal: &super::CompletionSeal,
    actor: &ActorContext,
) -> Result<(), StoreError> {
    if input.links.is_empty() {
        return Ok(());
    }
    let refuse = || StoreError::WorkCriterionLinkInvalid {
        criterion: None,
        reason: "completion is frozen under a different intent; the pending links do not match its seal",
    };
    let work = basis.focused_work.as_ref().ok_or_else(refuse)?;
    let claim = basis.claim.as_ref().ok_or_else(refuse)?;
    if input.link_basis != Some(seal.accepted_work_revision)
        || work.revision != seal.accepted_work_revision
        || claim.run_id != seal.run_id
        || claim.claim_id != seal.claim_id
        || claim.fence != seal.claim_fence
        || actor.actor_id != seal.actor.actor_id
        || actor.session_id != seal.actor.session_id
    {
        return Err(refuse());
    }
    // The caller has proved ownership of the committed core result. Compare
    // against that seal's immutable evidence, not a later live note index that
    // could gain a conflicting prefix or fail an unrelated advisory read.
    let requested_evidence = if input.evidence.is_empty() {
        seal.evidence.clone()
    } else {
        super::parse_hashes(&input.evidence)?
    };
    let mut expected = LocalWorkService::prevalidate_completion_acceptance(
        work,
        None,
        input.note.as_deref(),
        &seal.evidence,
        actor.assurance,
        &actor.actor_id,
    )?;
    for link in &input.links {
        let result = link
            .criterion
            .checked_sub(1)
            .and_then(|index| expected.get_mut(index))
            .ok_or_else(refuse)?;
        let prefix = link.locator.to_ascii_lowercase();
        let mut matching = seal
            .evidence
            .iter()
            .filter(|hash| hash.as_str().starts_with(&prefix));
        let hash = matching.next().ok_or_else(refuse)?;
        if matching.next().is_some() || !requested_evidence.contains(hash) {
            return Err(refuse());
        }
        result.evidence.push(hash.clone());
        result.evidence.sort();
        result.evidence.dedup();
    }
    if expected != seal.acceptance {
        return Err(refuse());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
