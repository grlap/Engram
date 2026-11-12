use crate::domain::{
    CompletionObligationBinding, CompletionSeal, EnvironmentEvidence, SCHEMA_VERSION, WorkClaim,
    WorkEvent, WorkObligation, WorkObligationResolution, WorkObligationResolutionEvent,
};
use crate::storage::work::task_sync::evidence::acceptance::{History, Refusal, Result, invalid};
use crate::storage::work::task_sync::evidence::verification;
use crate::{FeedId, ObjectId};
mod context;
mod definitions;
mod resolutions;

type Definitions = std::collections::BTreeMap<uuid::Uuid, (i64, ObjectId, WorkObligation)>;
type Resolutions =
    std::collections::BTreeMap<uuid::Uuid, (i64, ObjectId, WorkObligationResolutionEvent)>;

pub(super) fn validate(
    history: &History<'_>,
    seal: &CompletionSeal,
    feed: &FeedId,
    budget: &mut crate::task_sync::evidence::limits::StagingBudget,
) -> Result<Option<Refusal>> {
    let Cut {
        last_claim,
        definitions,
        resolutions,
        mut environment,
    } = collect(history, seal, feed, budget)?;
    let Some(claim) = last_claim else {
        return Ok(Some(Refusal::IncompleteClosure));
    };
    if claim.run_id != seal.run_id
        || claim.claim_id != seal.claim_id
        || claim.fence != seal.claim_fence
        || seal.actor.session_id.as_ref() != Some(&claim.holder)
    {
        return Ok(Some(Refusal::OriginContextDiffers));
    }
    environment.sort();
    environment.dedup();
    definitions::validate_all(history, seal, feed, &definitions)?;
    let expected: Vec<_> = definitions
        .iter()
        .filter_map(|(id, (_, definition, obligation))| {
            resolutions
                .get(id)
                .map(|(_, resolution, _)| CompletionObligationBinding {
                    obligation_id: obligation.obligation_id,
                    definition: definition.clone(),
                    resolution: resolution.clone(),
                })
        })
        .collect();
    if environment != seal.environment
        || expected != seal.obligations
        || expected.len() != definitions.len()
        || resolutions.len() != definitions.len()
    {
        return Ok(Some(Refusal::IncompleteClosure));
    }
    let mut displaced = Vec::new();
    for binding in &seal.obligations {
        let Some((definition_position, definition_id, definition)) =
            definitions.get(&binding.obligation_id.0)
        else {
            return Ok(Some(Refusal::IncompleteClosure));
        };
        let Some((resolution_position, resolution_id, resolution)) =
            resolutions.get(&binding.obligation_id.0)
        else {
            return Ok(Some(Refusal::IncompleteClosure));
        };
        if definition_id != &binding.definition
            || resolution_id != &binding.resolution
            || resolution.definition != *definition_id
            || resolution.project_id != *history.project
            || resolution.run_id != seal.run_id
            || definition.project_id != *history.project
            || definition.root_id != seal.root_id
            || definition.work_id != seal.work_id
            || definition.run_id != seal.run_id
            || definition.root_execution_id != seal.root_execution_id
            || definition.schema_version != SCHEMA_VERSION
            || resolution.schema_version != SCHEMA_VERSION
            || resolution.obligation_id != definition.obligation_id
            || resolution_position <= definition_position
        {
            return Ok(Some(Refusal::IncompleteClosure));
        }
        let (kind, _) = verification::applicability::qualified_bytes(
            history.connection,
            history.project,
            &history.request.origin,
            &definition.triggering_observation,
        )?;
        if !history.member(
            feed,
            &definition.triggering_observation,
            &kind,
            definition.trigger_position.position,
        )? {
            return Ok(Some(Refusal::IncompleteClosure));
        }
        if !resolutions::validate(
            history,
            seal,
            feed,
            definition,
            resolution,
            *resolution_position,
        )? {
            return Ok(Some(Refusal::IncompleteClosure));
        }
        if matches!(
            resolution.resolution,
            WorkObligationResolution::Displaced { .. }
        ) && !displaced.contains(&definition.triggering_observation)
        {
            displaced.push(definition.triggering_observation.clone());
        }
    }
    if displaced != seal.foreign_workspace_changes {
        return Ok(Some(Refusal::IncompleteClosure));
    }
    Ok(None)
}

struct Cut {
    last_claim: Option<WorkClaim>,
    definitions: Definitions,
    resolutions: Resolutions,
    environment: Vec<ObjectId>,
}

fn collect(
    history: &History<'_>,
    seal: &CompletionSeal,
    feed: &FeedId,
    budget: &mut crate::task_sync::evidence::limits::StagingBudget,
) -> Result<Cut> {
    let mut last_claim = None;
    let mut definitions = std::collections::BTreeMap::new();
    let mut resolutions = std::collections::BTreeMap::new();
    let mut environment = Vec::new();
    if !history.walk(feed, seal.completion_cut.position, |position, id, kind| {
        match kind {
            "work_event" => {
                let event: WorkEvent = history.load(id, kind)?;
                if event.run_id == Some(seal.run_id)
                    && event.work_id == seal.work_id
                    && let Some(claim) = event.claim
                {
                    last_claim = Some(claim);
                }
            }
            "work_obligation" => {
                let bytes = history.bytes(id, kind)?;
                budget
                    .reserve(bytes.len())
                    .map_err(crate::task_sync::evidence::EvidenceError::from)?;
                let definition: WorkObligation = serde_json::from_slice(&bytes)?;
                if definitions
                    .insert(
                        definition.obligation_id.0,
                        (position, id.clone(), definition),
                    )
                    .is_some()
                {
                    return Err(invalid("historical obligation repeats its definition"));
                }
            }
            "work_obligation_resolution" => {
                let bytes = history.bytes(id, kind)?;
                budget
                    .reserve(bytes.len())
                    .map_err(crate::task_sync::evidence::EvidenceError::from)?;
                let resolution: WorkObligationResolutionEvent = serde_json::from_slice(&bytes)?;
                if resolutions
                    .insert(
                        resolution.obligation_id.0,
                        (position, id.clone(), resolution),
                    )
                    .is_some()
                {
                    return Err(invalid(
                        "historical obligation repeats its terminal resolution",
                    ));
                }
            }
            "environment_evidence" => {
                let evidence: EnvironmentEvidence = history.load(id, kind)?;
                if evidence.project_id != *history.project
                    || evidence.binding.run_id != seal.run_id
                    || evidence.binding.work_id != seal.work_id
                    || evidence.binding.root_execution_id != seal.root_execution_id
                {
                    return Err(invalid("historical environment crosses its completion run"));
                }
                budget
                    .reserve(id.as_str().len())
                    .map_err(crate::task_sync::evidence::EvidenceError::from)?;
                environment.push(id.clone());
            }
            _ => {}
        }
        Ok(())
    })? {
        return Err(invalid(
            "historical obligation feed contains an interior gap",
        ));
    }
    Ok(Cut {
        last_claim,
        definitions,
        resolutions,
        environment,
    })
}
