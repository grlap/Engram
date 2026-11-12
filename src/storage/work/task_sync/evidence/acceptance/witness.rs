//! Immutable registered-input witnesses use the native binding and movement rules.
use super::movement;
use crate::FeedId;
use crate::domain::{
    AcceptanceEvaluation, NamedRootBindingEvent, NamedRootBindingKind, VerificationRequirement,
};
use crate::storage::work::acceptance_evaluation::source::{
    registered_input_change_conflicts, registered_witness_matches, within_named_root,
};
use crate::storage::work::task_sync::evidence::acceptance::{History, Result, invalid};
use crate::storage::work::task_sync::evidence::verification::applicability::candidate_facts_on;

pub(super) fn validate(
    history: &History<'_>,
    record: &AcceptanceEvaluation,
    root: &NamedRootBindingEvent,
    feed: &FeedId,
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    let Some(witness) = &record.registered_input_witness else {
        return Ok(None);
    };
    let declared = record
        .source_basis
        .as_ref()
        .ok_or_else(|| invalid("historical witness has no declared source"))?;
    let cut = record.evaluated_cut.position;
    let (check, producer, execution) = candidate_facts_on(
        history.connection,
        history.project,
        &history.request.origin,
        witness,
    )?;
    let evidence_at = movement::position(history, feed, witness)?;
    let producer_at = movement::position(history, feed, &check.producer_observation)?;
    let root_id = record
        .named_root_binding
        .as_ref()
        .ok_or_else(|| invalid("historical witness has no named root"))?;
    let root_at = movement::position(history, feed, root_id)?;
    let requirement = VerificationRequirement {
        check_kind: check.check_kind,
        check_fingerprint: Some(check.check_fingerprint.clone()),
    };
    if record.registered_input_decision.is_some()
        || !record.evidence_basis.contains(witness)
        || record.evaluated_cut.feed != *feed
        || root.kind != NamedRootBindingKind::Bound
        || root.project_id != record.project_id
        || root.work_id != record.work_id
        || root.run_id != record.run_id
        || evidence_at > cut
        || producer_at > cut
        || root_at > cut
        || evidence_at <= root_at
        || producer_at <= root_at
        || declared
            .workspace_id
            .as_ref()
            .is_some_and(|workspace| workspace != &root.workspace_id)
        || !registered_witness_matches(declared, root, &check, &producer, record.run_id)
        || !within_named_root(root, &check.source_basis)
        || producer
            .source_basis
            .as_ref()
            .is_none_or(|basis| !within_named_root(root, basis))
        || crate::control::check_verification_result(&check, &producer, &requirement).is_err()
        || !movement::latest_root(history, feed, root.claim_id, cut, Some(root_id))?
    {
        return Err(invalid(
            "historical input witness does not corroborate its declared cut",
        ));
    }
    if let Some(key) = execution
        && super::super::verification::union_on(history.connection, &key)?.conflicted
    {
        return Err(invalid(
            "historical witness terminal execution is conflicted",
        ));
    }
    let mut moved = false;
    if !history.walk(feed, cut, |at, id, kind| {
        if !matches!(
            kind,
            "execution_observation" | "unadmitted_execution_observation"
        ) {
            return Ok(());
        }
        let Some(change) = movement::observation(history, id, kind)? else {
            return Ok(());
        };
        if change
            .source_basis
            .as_ref()
            .is_some_and(|basis| !within_named_root(root, basis))
        {
            return Ok(());
        }
        moved |= (!change.admitted
            && change.source_changed
            && !crate::storage::work::feeds::unadmitted_check_follows(
                (producer_at, evidence_at),
                check.completed_at,
                at,
                &change,
            ))
            || registered_input_change_conflicts(
                &check,
                (producer_at, evidence_at),
                Some(root),
                Some(&declared.identity),
                at,
                &change,
            );
        Ok(())
    })? || moved
    {
        return Err(invalid(
            "historical input witness crosses a source change at its cut",
        ));
    }
    Ok(Some(check.run_time()))
}
