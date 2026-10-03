//! A blocking evaluation stands until something that could change it is
//! recorded: a later evaluation replaces the newest `fail`,
//! `insufficient_evidence` or `needs_human` on its run only when evidence of a
//! qualifying kind lies after that evaluation's cut and within its own. So a
//! re-roll on the same evidence, or after an edit of the title, the mode or
//! the policy, cannot replace a fail, while a correction note followed by a
//! new evaluation still can. A change of the criteria or their bindings is
//! the carried-failure rule's to judge, not this one's.
//!
//! One assessment serves both readers: the record transaction asks it at the
//! submitted evidence basis, and the evaluation status asks it at the run
//! feed's head, so a host can see a standing refusal before it starts an
//! evaluator. The record transaction always asks again; a status read is
//! never an admission.

use rusqlite::{Connection, params};

use super::{
    AcceptanceEvaluation, NamedEvaluationRoot, ObjectId, StoreError, WorkItem, WorkRunId,
    citation_position, judged_bindings, judged_source, latest_on, named_root_at_on, off_named_root,
};
use crate::domain::{
    EvaluationAdmissionRemedy, EvaluationRerollMismatch, FeedId, RerollAdmissionCause,
};
use crate::storage::work::UNADMITTED_OBSERVATION_KIND;
use crate::storage::work::feeds::source_observation_if_accounted_on;

/// Run-feed kinds that record evidence for or against a verdict: notes and
/// gates, host verification and environment evidence, and observations of
/// the source, admitted or accounted. Evaluation records and the claim,
/// renewal, handoff, revision, checkpoint and obligation bookkeeping are left
/// out.
const EVIDENCE_KINDS: &str = "'work_evidence', 'verification_evidence', 'environment_evidence',
     'execution_observation', 'unadmitted_execution_observation'";

/// The blocking evaluation that stands against an evaluation cut at `cut`,
/// or `None` when one may replace the newest evaluation on the run: there is
/// none, it passes, it judged other criteria or bindings than the item has
/// now, or qualifying evidence was recorded after its cut and at or before
/// `cut`.
pub(super) fn reroll_assessment(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    cut: i64,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<RerollAdmissionCause>, StoreError> {
    let Some((newest, record)) = latest_on(connection, run_id)? else {
        return Ok(None);
    };
    let Some(blocking) = record.first_blocking() else {
        return Ok(None);
    };
    if contract_changed(connection, item, run_id, &newest, &record)? {
        return Ok(None);
    }
    if new_evidence_between(connection, run_id, &record, cut, root)? {
        return Ok(None);
    }
    let criterion = record
        .verdicts
        .iter()
        .position(|verdict| std::ptr::eq(verdict, blocking))
        .map_or(0, |index| index + 1);
    Ok(Some(RerollAdmissionCause {
        mismatch: EvaluationRerollMismatch::BlockingEvaluationStands,
        evaluation: newest,
        feed: FeedId::RunExecution(run_id),
        after_position: record.evaluated_cut.position,
        through_position: cut,
        criterion,
        verdict: blocking.verdict,
        remedy: EvaluationAdmissionRemedy::RecordNewEvidenceThenEvaluate,
    }))
}

/// The refusal's words for a standing blocking evaluation.
pub(super) fn reroll_reason(cause: &RerollAdmissionCause) -> String {
    format!(
        "the newest evaluation on this run, {}, gave {} on criterion {}, and nothing that could change it was recorded after its evidence basis {} and within this one ({}): {}",
        cause.evaluation,
        cause.verdict.word(),
        cause.criterion,
        cause.after_position,
        cause.through_position,
        RerollAdmissionCause::REMEDY
    )
}

/// Whether the item's criteria or their bindings differ from those the
/// evaluation judged: the carried-failure rule governs that case.
fn contract_changed(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    evaluation: &ObjectId,
    record: &AcceptanceEvaluation,
) -> Result<bool, StoreError> {
    if record.criteria != item.acceptance {
        return Ok(true);
    }
    let position = citation_position(connection, run_id, evaluation)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!(
            "acceptance evaluation {evaluation} is not on its run feed"
        ))
    })?;
    Ok(
        judged_bindings(connection, item, run_id, evaluation, record, position)?
            != item.acceptance_bindings,
    )
}

/// Whether evidence of a qualifying kind lies on the run feed after the
/// blocking evaluation's cut and at or before `through`. An observation
/// qualifies only when it reports the source changed to a revision other than
/// the one last seen, starting from the source that evaluation judged: its
/// declared revision, or else the run's last sighting at its cut. A report
/// that leaves the source at that revision, or any sighting outside the
/// claim's named root, is no new evidence. A reported change that carries no
/// revision qualifies, since nothing shows it left the source as judged, and
/// so does an accounted unadmitted change whatever revision it reports: it
/// already voids the blocking evaluation, which may have judged the source
/// from before it.
fn new_evidence_between(
    connection: &Connection,
    run_id: WorkRunId,
    blocking: &AcceptanceEvaluation,
    through: i64,
    root: Option<&NamedEvaluationRoot>,
) -> Result<bool, StoreError> {
    let from = blocking.evaluated_cut.position;
    if through <= from {
        return Ok(false);
    }
    let mut statement = connection.prepare(&format!(
        "SELECT object_kind, object_id FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position > ?2 AND position <= ?3
           AND object_kind IN ({EVIDENCE_KINDS})
         ORDER BY position"
    ))?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), from, through], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    // The revision last seen, read once the first observation needs it.
    let mut seen: Option<Option<String>> = None;
    for (kind, stored) in rows {
        if kind != "execution_observation" && kind != UNADMITTED_OBSERVATION_KIND {
            return Ok(true);
        }
        let hash =
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
        // An unadmitted record that was not accounted is no evidence.
        let Some(observation) = source_observation_if_accounted_on(connection, &hash)? else {
            continue;
        };
        if off_named_root(root, &observation) {
            continue;
        }
        let last = if let Some(last) = seen.take() {
            last
        } else {
            let judged_root = named_root_at_on(connection, run_id, from)?;
            judged_source(
                connection,
                run_id,
                from,
                blocking.source_basis.as_ref(),
                judged_root.as_ref(),
            )?
            .map(|judged| judged.revision)
        };
        let revision = observation
            .source_basis
            .as_ref()
            .map(|basis| basis.source_revision.clone());
        if observation.source_changed
            && (!observation.admitted || revision.is_none() || revision != last)
        {
            return Ok(true);
        }
        seen = Some(revision.or(last));
    }
    Ok(false)
}
