use crate::domain::{
    AcceptanceBasis, AcceptanceEvaluation, AcceptanceEvaluationPolicy, AcceptanceSourceBasis,
    AcceptanceStaleReason, AcceptanceVerdict, NamedRootBindingEvent, NamedRootBindingKind,
    SCHEMA_VERSION, WorkEvent, WorkEvidence, WorkTransition,
};
use crate::storage::work::task_sync::evidence::acceptance::{
    History, Refusal, Result, params, same_governing,
};
use crate::storage::work::task_sync::evidence::verification::applicability::{
    Destination, VerificationUse, eligible_for_on,
};
use crate::{CanonicalObject, FeedId, ObjectId};

pub(super) fn inspect(
    history: &History<'_>,
    destination: &Destination,
    policy: &AcceptanceEvaluationPolicy,
    source: &crate::source::SourceId,
    evaluation: &ObjectId,
    witness: &ObjectId,
) -> Result<Option<Refusal>> {
    let record: AcceptanceEvaluation = history.load(evaluation, "acceptance_evaluation")?;
    let Some(governing) = history.governing(evaluation, &record, witness)? else {
        return Ok(Some(Refusal::GoverningSnapshotUnavailable));
    };
    if record.schema_version != SCHEMA_VERSION
        || record.project_id != *history.project
        || record.root_id != governing.root_id
        || record.criteria != governing.acceptance
        || !same_governing(&governing, &destination.item)?
    {
        return Ok(Some(Refusal::GoverningContentDiffers));
    }
    let feed = FeedId::RunExecution(record.run_id);
    let Some(cut) = history.cut(&feed) else {
        return Ok(Some(Refusal::OriginContextUnavailable));
    };
    if record.evaluated_cut.feed != feed
        || !history.member(&feed, evaluation, "acceptance_evaluation", cut.through)?
    {
        return Ok(Some(Refusal::OriginContextDiffers));
    }
    if let Some(refusal) =
        assess_standing(history, destination, policy, &record, evaluation, &feed)?
    {
        return Ok(Some(refusal));
    }
    if !record.all_pass() {
        return Ok(Some(Refusal::BlockingVerdict));
    }
    if record
        .source_basis
        .as_ref()
        .and_then(AcceptanceSourceBasis::source_id)
        != Some(source)
    {
        return Ok(Some(Refusal::SourceDiffers));
    }
    let Some(named_root) = &record.named_root_binding else {
        return Ok(Some(Refusal::OriginContextUnavailable));
    };
    let root: NamedRootBindingEvent = history.load(named_root, "named_root_binding")?;
    if root.kind != NamedRootBindingKind::Bound
        || root.run_id != record.run_id
        || root.work_id != record.work_id
    {
        return Ok(Some(Refusal::OriginContextDiffers));
    }
    if let Some(refusal) = super::movement::inspect(history, &record, &root, &feed)? {
        return Ok(Some(refusal));
    }
    if let Some(refusal) = cited_gate_superseded(history, &record, &feed)? {
        return Ok(Some(refusal));
    }
    assess_citations(history, destination, &record, source, &feed)
}

/// A newer record of a gate a passing verdict cites replaces that citation,
/// whatever its result, as it does natively. The whole current origin feed is
/// read: an older requested cut cannot hide the newer record.
fn cited_gate_superseded(
    history: &History<'_>,
    record: &AcceptanceEvaluation,
    feed: &FeedId,
) -> Result<Option<Refusal>> {
    let mut names = Vec::new();
    for verdict in record
        .verdicts
        .iter()
        .filter(|verdict| verdict.verdict == AcceptanceVerdict::Pass)
    {
        for id in &verdict.evidence {
            let (kind, bytes) = super::super::verification::applicability::qualified_bytes(
                history.connection,
                history.project,
                &history.request.origin,
                id,
            )?;
            if kind == "work_evidence"
                && let Some(gate) = serde_json::from_slice::<WorkEvidence>(&bytes)?.gate
            {
                names.push(gate.name);
            }
        }
    }
    if names.is_empty() {
        return Ok(None);
    }
    let mut superseded = false;
    if !history.walk_current(feed, |at, id, kind| {
        if !superseded && at > record.evaluated_cut.position && kind == "work_evidence" {
            let later: WorkEvidence = history.load(id, kind)?;
            superseded = later.gate.is_some_and(|gate| names.contains(&gate.name));
        }
        Ok(())
    })? {
        return Ok(Some(Refusal::OriginContextUnavailable));
    }
    Ok(superseded.then_some(Refusal::EvaluationBasisMoved))
}

fn assess_standing(
    history: &History<'_>,
    destination: &Destination,
    policy: &AcceptanceEvaluationPolicy,
    record: &AcceptanceEvaluation,
    evaluation: &ObjectId,
    feed: &FeedId,
) -> Result<Option<Refusal>> {
    let mut events = Vec::new();
    let mut holder = None;
    let mut executor = None;
    let mut holders = Vec::new();
    let mut superseded = false;
    let evaluation_position:i64=history.connection.query_row("SELECT position FROM work_task_sync_evidence_feed_members WHERE project_id=?1 AND origin_id=?2 AND feed=?3 AND object_id=?4",params![history.project.0,history.request.origin.as_str(),serde_json::to_string(&feed)?,evaluation.as_str()],|row|row.get(0))?;
    let mut invalid_event = false;
    let mut budget = crate::task_sync::evidence::limits::StagingBudget::default();
    if !history.walk_current(feed, |position, id, kind| {
        match kind {
            "work_event" => {
                let bytes = history.bytes(id, kind)?;
                budget
                    .reserve(bytes.len())
                    .map_err(crate::task_sync::evidence::EvidenceError::from)?;
                let event: WorkEvent = serde_json::from_slice(&bytes)?;
                if event.project_id != *history.project
                    || event.work_id != record.work_id
                    || event.root_id != record.root_id
                    || event.run_id != Some(record.run_id)
                {
                    invalid_event = true;
                }
                if let Some(run) = &event.run {
                    executor.clone_from(&run.executor);
                }
                if let Some(claim) = &event.claim {
                    holder = Some(claim.holder.clone());
                    holders.push(claim.holder.clone());
                }
                match &event.transition {
                    WorkTransition::Claimed { claim, .. }
                    | WorkTransition::ClaimRenewed { claim } => {
                        holder = Some(claim.holder.clone());
                        holders.push(claim.holder.clone());
                    }
                    WorkTransition::HandedOff { from, to, .. } => {
                        holders.push(from.clone());
                        holders.push(to.clone());
                        holder = Some(to.clone());
                    }
                    _ => {}
                }
                events.push(event);
            }
            "acceptance_evaluation" if id != evaluation => {
                let later: AcceptanceEvaluation = history.load(id, kind)?;
                if later.run_id != record.run_id
                    || later.work_id != record.work_id
                    || later.project_id != *history.project
                {
                    invalid_event = true;
                }
                // Native consumption selects the newest verdict, never an older pass.
                if position > evaluation_position {
                    superseded = true;
                }
            }
            _ => {}
        }
        Ok(())
    })? {
        return Ok(Some(Refusal::OriginContextUnavailable));
    }
    if invalid_event || events.is_empty() {
        return Ok(Some(Refusal::OriginContextDiffers));
    }
    if superseded {
        return Ok(Some(Refusal::SupersededEvaluation));
    }
    let mark_author = if record.mode == crate::domain::AcceptanceEvaluationMode::SameSession
        && destination.item.evaluation_mode == Some(record.mode)
    {
        super::mark::author(history, record, evaluation)?
    } else {
        None
    };
    if let Some(fault) = crate::storage::work::acceptance_evaluation::historical_evaluation_fault(
        &destination.item,
        policy,
        record,
        holder.as_ref(),
        executor.as_ref(),
        &holders,
        mark_author,
    ) {
        return Ok(Some(match fault {
            AcceptanceStaleReason::Identity => Refusal::EvaluatorAffiliated,
            AcceptanceStaleReason::Policy => Refusal::EvaluationPolicy,
            _ => Refusal::EvaluationShape,
        }));
    }
    Ok(None)
}

fn assess_citations(
    history: &History<'_>,
    destination: &Destination,
    record: &AcceptanceEvaluation,
    source: &crate::source::SourceId,
    feed: &FeedId,
) -> Result<Option<Refusal>> {
    let governing = CanonicalObject::freeze(&destination.item)?.key().clone();
    for (index, verdict) in record.verdicts.iter().enumerate() {
        let binding = destination
            .item
            .acceptance_bindings
            .iter()
            .find(|binding| binding.criterion == index + 1);
        if binding.is_some() && verdict.basis != AcceptanceBasis::Observed {
            return Ok(Some(Refusal::CitationIneligible));
        }
        let mut supports = false;
        for id in &verdict.evidence {
            if !record.evidence_basis.contains(id) {
                return Ok(Some(Refusal::CitationUnavailable));
            }
            let (kind, _) = super::super::verification::applicability::qualified_bytes(
                history.connection,
                history.project,
                &history.request.origin,
                id,
            )?;
            if !history.member(feed, id, &kind, record.evaluated_cut.position)? {
                return Ok(Some(Refusal::CitationUnavailable));
            }
            match verdict.basis {
                AcceptanceBasis::Observed
                    if kind == crate::task_sync::evidence::consumption::CONSUMPTION_KIND =>
                {
                    supports |=
                        super::imported::verification(history, destination, source, id, binding)?;
                }
                AcceptanceBasis::Observed if kind == "verification_evidence" => {
                    let context = VerificationUse {
                        project: history.project,
                        origin: &history.request.origin,
                        binding: &history.request.binding,
                        original: id,
                        criterion: binding.map(|binding| binding.criterion),
                        requirement: binding.map(|binding| &binding.requirement),
                        governing: &governing,
                        root: &destination.root,
                        source,
                    };
                    for decision in &history.request.verification_decisions {
                        supports |= eligible_for_on(history.connection, decision, &context)?;
                    }
                }
                AcceptanceBasis::Asserted if kind == "work_evidence" => {
                    let note: WorkEvidence = history.load(id, &kind)?;
                    supports |= note
                        .gate
                        .as_ref()
                        .is_some_and(|gate| gate.failed.is_empty());
                }
                AcceptanceBasis::Judgment
                    if kind == crate::task_sync::evidence::consumption::CONSUMPTION_KIND =>
                {
                    supports |=
                        super::imported::judgment(history, destination, source, record, id)?;
                }
                AcceptanceBasis::Judgment => {
                    supports |= matches!(
                        kind.as_str(),
                        "work_evidence" | "verification_evidence" | "environment_evidence"
                    );
                }
                _ => {}
            }
        }
        if !supports {
            return Ok(Some(Refusal::CitationIneligible));
        }
    }
    Ok(None)
}
