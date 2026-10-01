//! An evaluation's named source root and judged revision: the root
//! selected at a cut, sighting horizons, the root binding and source
//! assessments, declaration confirmation, and stale bound citations.

use super::{
    AcceptanceVerdict, Citation, Connection, CriterionVerdict, EvaluationRootMismatch,
    ExecutionObservation, ExecutionSourceBasis, NamedRootBindingEvent, NamedRootBindingKind,
    ObjectId, OptionalExtension, SourceRootState, StoreError, WorkId, WorkItem, WorkRunId,
    admission, citation_position, classify_citation, latest_named_root_binding_on,
    latest_named_root_sighting_on, load_typed_work_object, load_work_claim_optional, params,
};

pub(super) struct NamedEvaluationRoot {
    pub(super) position: i64,
    pub(super) event_id: ObjectId,
    pub(super) event: NamedRootBindingEvent,
}

pub(super) fn named_root_at_on(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
) -> Result<Option<NamedEvaluationRoot>, StoreError> {
    let Some(claim) = load_work_claim_optional(connection, run_id)? else {
        return Ok(None);
    };
    let Some((position, event_id, event)) =
        latest_named_root_binding_on(connection, run_id, claim.claim_id, through)?
    else {
        return Ok(None);
    };
    Ok(
        (event.kind == NamedRootBindingKind::Bound).then_some(NamedEvaluationRoot {
            position: position.position,
            event_id,
            event,
        }),
    )
}

/// Whether `observation` sights a source other than the claim's named root:
/// another workspace, another generation, or a root not in the named state.
/// A foreign sighting cannot claim the named source moved. Without a named
/// root nothing is foreign.
pub(super) fn off_named_root(
    root: Option<&NamedEvaluationRoot>,
    observation: &ExecutionObservation,
) -> bool {
    root.is_some_and(|root| {
        observation.source_basis.as_ref().is_some_and(|basis| {
            basis.workspace_id != root.event.workspace_id
                || basis.source_root_generation != Some(root.event.generation)
                || basis.source_root_state != Some(SourceRootState::Named)
        })
    })
}

/// The revision the run was last seen at, at or before `through`: that of
/// the newest execution observation there that carries one. Verification
/// and environment records are left out: they carry the basis of the check
/// they describe, not where the source was when they were recorded.
pub(super) fn revision_seen_through(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<String>, StoreError> {
    if let Some(root) = root {
        return Ok(latest_named_root_sighting_on(
            connection,
            run_id,
            &root.event.workspace_id,
            root.event.generation,
            through,
            false,
        )?
        .and_then(|(_, observation)| observation.source_basis.map(|basis| basis.source_revision)));
    }
    Ok(connection
        .query_row(
            "SELECT json_extract(object.canonical_json, '$.source_basis.source_revision')
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position <= ?2
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_basis.source_revision')
                   IS NOT NULL
             ORDER BY entry.position DESC LIMIT 1",
            params![run_id.0.to_string(), through],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

/// The source an evaluation judged: the revision it declared, in the
/// workspace it declared when it named one, or else the revision the run was
/// last seen at through its cut.
pub(super) struct JudgedSource {
    pub(super) revision: String,
    pub(super) workspace: Option<String>,
    /// Whether the evaluation declared this source rather than taking the
    /// run's newest sighting.
    pub(super) declared: bool,
}

impl JudgedSource {
    /// Whether a check that ran on `basis` checked this source.
    fn checked_by(&self, basis: &ExecutionSourceBasis) -> bool {
        basis.source_revision == self.revision
            && self
                .workspace
                .as_ref()
                .is_none_or(|workspace| *workspace == basis.workspace_id)
    }
}

/// The source the evaluation cut at `through` judged, or `None` when it
/// declared none and the run carries no revision through the cut. A
/// declaration always wins: an older sighting never stands in for the tree
/// the evaluator says it judged.
pub(super) fn judged_source(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
    declared: Option<&crate::domain::AcceptanceSourceBasis>,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<JudgedSource>, StoreError> {
    Ok(match declared {
        Some(declared) => Some(JudgedSource {
            revision: declared.fingerprint.clone(),
            workspace: root
                .map(|root| root.event.workspace_id.clone())
                .or_else(|| declared.workspace_id.clone()),
            declared: true,
        }),
        None => {
            revision_seen_through(connection, run_id, through, root)?.map(|revision| JudgedSource {
                revision,
                workspace: root.map(|root| root.event.workspace_id.clone()),
                declared: false,
            })
        }
    })
}

/// Which reading of a named root's source an assessment makes, and so how
/// far along the run feed it reads.
#[derive(Clone, Copy, Debug)]
pub(super) enum RootPhase {
    /// Recording an evaluation: the root must have been sighted through the
    /// cut. The judged source must be that sighting, or a revision the
    /// evaluation declared that the root's newest sighting after the cut does
    /// not contradict, since the requesting turn may report it after the cut.
    Admission,
    /// Consuming a recorded evaluation: a declared revision must be the
    /// root's newest sighting through the head, since the host may have
    /// reported it after the cut; an undeclared evaluation must be the
    /// root's newest sighting through its cut.
    Consumption,
}

/// Where a named root's source stands against the source an evaluation
/// judged, in one phase.
#[derive(Debug, PartialEq)]
pub(super) enum RootSource {
    /// The root agrees with the judged source.
    Confirmed,
    /// The root has no sighting through the cut, so no source is anchored.
    /// Only admission reads this: consumption never holds a record that was
    /// admitted without one.
    NoInitialSighting,
    /// The root's newest sighting within the phase's horizon is not the
    /// judged source; `reported` is that sighting's revision, if any.
    Unconfirmed { reported: Option<String> },
}

/// Where a named root's binding stands for one evaluation, in one phase.
pub(super) enum RootBinding<'a> {
    /// The evaluation stands on the binding the run holds now.
    Held,
    /// Admission only: the evaluation declares a workspace other than this
    /// named root's.
    DeclaredWorkspaceMismatch(&'a NamedEvaluationRoot),
    /// The binding the evaluation stood on is not the one the run holds now.
    Rebound,
}

/// The one decision on a named root's binding, used when an evaluation is
/// recorded and when it is consumed. `evaluated` is the binding at the
/// evaluation's cut; `current` reads the binding at the head, and is called
/// only once the declared workspace is settled. Only admission compares a
/// declared workspace: a recorded evaluation already passed that check, and
/// consumption must not refuse records it cannot re-admit.
pub(super) fn assess_named_root_binding<'a>(
    phase: RootPhase,
    evaluated: Option<&ObjectId>,
    current: impl FnOnce() -> Result<Option<ObjectId>, StoreError>,
    root: Option<&'a NamedEvaluationRoot>,
    declared_workspace: Option<&str>,
) -> Result<RootBinding<'a>, StoreError> {
    if matches!(phase, RootPhase::Admission)
        && let (Some(root), Some(workspace)) = (root, declared_workspace)
        && workspace != root.event.workspace_id
    {
        return Ok(RootBinding::DeclaredWorkspaceMismatch(root));
    }
    Ok(if evaluated == current()?.as_ref() {
        RootBinding::Held
    } else {
        RootBinding::Rebound
    })
}

/// The one assessment of a named root's source, used when an evaluation is
/// recorded and when it is consumed. Each phase reads only its own horizon.
pub(super) fn assess_named_root_source(
    connection: &Connection,
    run_id: WorkRunId,
    cut: i64,
    root: &NamedEvaluationRoot,
    judged: Option<&JudgedSource>,
    phase: RootPhase,
) -> Result<RootSource, StoreError> {
    Ok(match phase {
        RootPhase::Admission => {
            // A root the host has not yet sighted anchors no evaluation: its
            // first sighting could show any source.
            let Some(latest) = revision_seen_through(connection, run_id, cut, Some(root))? else {
                return Ok(RootSource::NoInitialSighting);
            };
            match judged {
                Some(judged)
                    if judged.revision == latest
                        || (judged.declared
                            && declared_not_contradicted(
                                connection,
                                run_id,
                                cut,
                                root,
                                &judged.revision,
                            )?) =>
                {
                    RootSource::Confirmed
                }
                _ => RootSource::Unconfirmed {
                    reported: Some(latest),
                },
            }
        }
        RootPhase::Consumption => {
            let declared = judged.is_some_and(|judged| judged.declared);
            let horizon = if declared { i64::MAX } else { cut };
            let latest = revision_seen_through(connection, run_id, horizon, Some(root))?;
            if latest.is_some()
                && latest.as_deref() == judged.map(|judged| judged.revision.as_str())
            {
                RootSource::Confirmed
            } else {
                RootSource::Unconfirmed { reported: latest }
            }
        }
    })
}

/// Admission's reading of the named root, refused in the root family.
pub(super) fn require_named_root_judged_source(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
    root: Option<&NamedEvaluationRoot>,
    judged: Option<&JudgedSource>,
    work_id: WorkId,
    declaration: Option<&crate::domain::AcceptanceSourceBasis>,
) -> Result<(), StoreError> {
    let Some(root) = root else {
        return Ok(());
    };
    match assess_named_root_source(
        connection,
        run_id,
        through,
        root,
        judged,
        RootPhase::Admission,
    )? {
        RootSource::Confirmed => Ok(()),
        RootSource::NoInitialSighting => Err(admission::root_refusal(
            work_id,
            root,
            through,
            EvaluationRootMismatch::NoInitialSighting,
            declaration,
            None,
            "the named root has no sighting yet; capture that root, then evaluate it",
        )),
        RootSource::Unconfirmed { reported } => Err(admission::root_refusal(
            work_id,
            root,
            through,
            EvaluationRootMismatch::JudgedSourceMismatch,
            declaration,
            reported,
            "the evaluated source does not match the named root's newest sighting; capture and evaluate that root",
        )),
    }
}

/// Whether a revision declared under a named root, other than the one the
/// root was sighted at through the cut, is still open: the requesting turn
/// may report it after the evaluator's cut. It is, unless the root's newest
/// sighting after the cut shows another revision. Completion waits for the
/// host to sight the root there (`staleness`).
pub(super) fn declared_not_contradicted(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
    root: &NamedEvaluationRoot,
    declared: &str,
) -> Result<bool, StoreError> {
    Ok(
        match latest_named_root_sighting_on(
            connection,
            run_id,
            &root.event.workspace_id,
            root.event.generation,
            i64::MAX,
            false,
        )? {
            None => true,
            Some((position, _)) if position <= through => true,
            Some((_, sighting)) => sighting
                .source_basis
                .is_some_and(|basis| basis.source_revision == declared),
        },
    )
}

/// Each passing verdict's one-based criterion position and citations.
pub(super) fn passing_citations(
    verdicts: &[CriterionVerdict],
) -> impl Iterator<Item = (usize, &[ObjectId])> + '_ {
    verdicts
        .iter()
        .enumerate()
        .filter(|(_, verdict)| verdict.verdict == AcceptanceVerdict::Pass)
        .map(|(index, verdict)| (index + 1, verdict.evidence.as_slice()))
}

/// A citation of a pass on a bound criterion that does not show its check
/// ran on the source the evaluation judged, and why.
pub(super) struct StaleCitation {
    pub(super) criterion: usize,
    pub(super) citation: ObjectId,
    pub(super) cause: StaleCause,
    pub(super) producer: Option<ObjectId>,
}

pub(super) enum StaleCause {
    /// The check ran on another source than the judged one.
    OtherSource(ExecutionSourceBasis),
    /// The check ran on the judged revision, but by the cut the run had moved
    /// away from it.
    MovedAfter { checked: String, moved: Moved },
    /// The citation is not a passed check, or no judged source exists to
    /// match. A pass admitted here never reaches this: `bind_verdicts` admits
    /// only passed checks for a bound criterion, and each check's producer is
    /// a sighting on the run, with a revision, before the check. Only a record
    /// written some other way can.
    Unverifiable,
}

/// How the run left the revision a check ran on.
pub(super) enum Moved {
    /// Its newest sighting after the check is at this other revision.
    To(String),
    /// It reported a change that carries no revision, which may have moved
    /// the source anywhere.
    Unrevised,
}

impl StaleCitation {
    /// Why record admission refuses the evaluation.
    pub(super) fn refusal(
        &self,
        item: &WorkItem,
        judged: Option<&JudgedSource>,
        cut: i64,
    ) -> Result<String, StoreError> {
        let kind = item
            .acceptance_bindings
            .iter()
            .find(|binding| binding.criterion == self.criterion)
            .map(|binding| super::super::planning::encode_state(binding.requirement.check_kind))
            .transpose()?
            .unwrap_or_default();
        let (criterion, citation) = (self.criterion, &self.citation);
        Ok(match (&self.cause, judged) {
            (StaleCause::OtherSource(checked), Some(judged)) => {
                let (ran, evaluated) = if checked.source_revision == judged.revision {
                    (
                        format!("in workspace {}", checked.workspace_id),
                        format!(
                            "workspace {}",
                            judged.workspace.as_deref().unwrap_or_default()
                        ),
                    )
                } else {
                    (
                        format!("on source revision {}", checked.source_revision),
                        format!("revision {}", judged.revision),
                    )
                };
                let declaration = if judged.declared {
                    ", or, when the declared fingerprint is not the source revision the host reports, declare that revision"
                } else {
                    ""
                };
                format!(
                    "criterion {criterion} is bound to {kind} verification, and {citation} ran {ran}, not the {evaluated} this evaluation judged; run the check on the current source, then evaluate again citing it{declaration}"
                )
            }
            (StaleCause::MovedAfter { checked, moved }, _) => {
                let moved = match moved {
                    Moved::To(seen) => format!("was last seen at revision {seen}"),
                    Moved::Unrevised => "reported a source change without a revision".to_owned(),
                };
                format!(
                    "criterion {criterion} is bound to {kind} verification, and {citation} ran on source revision {checked}, but the run {moved} after it, before evidence basis {cut}; run the check on the current source, then evaluate again citing it"
                )
            }
            _ => format!(
                "criterion {criterion} is bound to {kind} verification, and {citation} cannot be shown to be a passed check of the source this evaluation judged; run the check on the current source, then evaluate again citing it"
            ),
        })
    }
}

/// The first citation of a pass on a bound criterion that does not show its
/// check ran on the source the evaluation judged, or `None` when every one
/// does.
///
/// A bound criterion rests on a typed check of the work as it was judged, so
/// the check must have run on that source, and the source must still be there
/// at the cut `through`: a passed check of an earlier revision says nothing
/// about a later edit the source still holds, even one an older declaration
/// leaves out. The obligation path does not
/// always catch such a check at completion (a binding whose obligation was
/// waived is never matched to the run's latest change), so record admission
/// (R5) applies this rule, and completion applies it again to the record it
/// consumes, which covers a record admitted before the rule existed.
pub(super) fn stale_bound_citation<'a>(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    judged: Option<&JudgedSource>,
    through: i64,
    passes: impl IntoIterator<Item = (usize, &'a [ObjectId])>,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<StaleCitation>, StoreError> {
    for (criterion, citations) in passes {
        if !item
            .acceptance_bindings
            .iter()
            .any(|binding| binding.criterion == criterion)
        {
            continue;
        }
        for citation in citations {
            let mut producer_observation = None;
            let cause = match classify_citation(connection, run_id, citation)? {
                Some(Citation::VerificationPassed {
                    source_basis,
                    producer,
                    ..
                }) => {
                    // Under a named root both the check and the observation
                    // that produced it must be in the root's workspace and
                    // generation, and both must follow the binding on the run
                    // feed, as the obligation matcher requires.
                    producer_observation = Some(producer.clone());
                    let same_named_root = match root {
                        None => true,
                        Some(root) => {
                            let in_root = |basis: &ExecutionSourceBasis| {
                                basis.workspace_id == root.event.workspace_id
                                    && basis.source_root_generation == Some(root.event.generation)
                                    && basis.source_root_state == Some(SourceRootState::Named)
                            };
                            let producer_basis = load_typed_work_object::<ExecutionObservation>(
                                connection,
                                &producer,
                                "execution_observation",
                            )?
                            .source_basis;
                            in_root(&source_basis)
                                && producer_basis.as_ref().is_some_and(in_root)
                                && citation_position(connection, run_id, &producer)?
                                    .is_some_and(|position| position > root.position)
                                && citation_position(connection, run_id, citation)?
                                    .is_some_and(|position| position > root.position)
                        }
                    };
                    match judged {
                        Some(judged) if judged.checked_by(&source_basis) && same_named_root => {
                            match moved_after_check(
                                connection,
                                run_id,
                                citation,
                                &producer,
                                &source_basis.source_revision,
                                through,
                                root,
                            )? {
                                None => continue,
                                Some(moved) => StaleCause::MovedAfter {
                                    checked: source_basis.source_revision,
                                    moved,
                                },
                            }
                        }
                        Some(_) => StaleCause::OtherSource(source_basis),
                        None => StaleCause::Unverifiable,
                    }
                }
                _ => StaleCause::Unverifiable,
            };
            return Ok(Some(StaleCitation {
                criterion,
                citation: citation.clone(),
                cause,
                producer: producer_observation,
            }));
        }
    }
    Ok(None)
}

/// Whether the run left `checked`, the revision `producer` ran the check
/// `citation` on, between that check and `through`, inclusive, read as F3
/// reads the source after a cut. The newest execution observation there
/// that carries a revision decides where the source is, whatever change it
/// reports: the revision fingerprints the full content, so a move and its
/// revert leave the check standing. A reported change that carries no
/// revision may have moved the source anywhere, and no later sighting
/// clears it. `None` when the source is still where the check ran.
fn moved_after_check(
    connection: &Connection,
    run_id: WorkRunId,
    citation: &ObjectId,
    producer: &ObjectId,
    checked: &str,
    through: i64,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<Moved>, StoreError> {
    // The checkpoint admits a check only with a producer on the same run.
    let ran = citation_position(connection, run_id, producer)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!(
            "verification evidence {citation} names producer observation {producer}, which is not on its run feed"
        ))
    })?;
    let run = run_id.0.to_string();
    let unrevised: bool = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position > ?2 AND entry.position <= ?3
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_basis.source_revision') IS NULL
               AND json_extract(object.canonical_json, '$.source_changed') = 1
         )",
        params![run, ran, through],
        |row| row.get(0),
    )?;
    if unrevised {
        return Ok(Some(Moved::Unrevised));
    }
    let newest: Option<String> = connection
        .query_row(
            "SELECT json_extract(object.canonical_json, '$.source_basis.source_revision')
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position > ?2 AND entry.position <= ?3
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_basis.source_revision')
                   IS NOT NULL
               AND (?4 IS NULL OR (
                   json_extract(object.canonical_json, '$.source_basis.workspace_id') = ?4
                   AND json_extract(object.canonical_json, '$.source_basis.source_root_generation') = ?5
                   AND json_extract(object.canonical_json, '$.source_basis.source_root_state') = 'named'
               ))
             ORDER BY entry.position DESC LIMIT 1",
            params![
                run,
                ran,
                through,
                root.map(|root| root.event.workspace_id.as_str()),
                root.map(|root| root.event.generation),
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(newest.filter(|revision| revision != checked).map(Moved::To))
}

/// Whether a source change left the source at the revision the evaluation
/// declared it judged, in the declared workspace when one was named.
pub(super) fn judged_revision(
    declared: Option<&crate::domain::AcceptanceSourceBasis>,
    observation: &ExecutionObservation,
) -> bool {
    let (Some(declared), Some(basis)) = (declared, observation.source_basis.as_ref()) else {
        return false;
    };
    declared.fingerprint == basis.source_revision
        && declared
            .workspace_id
            .as_ref()
            .is_none_or(|workspace| *workspace == basis.workspace_id)
}
