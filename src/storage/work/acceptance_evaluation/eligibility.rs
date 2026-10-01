//! Who may record and consume an evaluation: the run's holder history,
//! mode and pin policy, same-session mark authorship, session standing,
//! the shared same-session ineligibility owner, and the evaluator identity
//! admission.

use super::{
    AcceptanceEvaluationMode, AcceptanceEvaluationPolicy, ActorContext, Connection,
    DETACH_PROVENANCE_SOURCE, EligibilityContext, EvaluationEligibilityMismatch, IdentityShape,
    ObjectId, ProvenanceRelation, RecordAcceptanceEvaluationRequest, SameSessionRefusal, SessionId,
    StoreError, WorkEvent, WorkItem, WorkRunId, WorkTransition, canonical_work_events_for_item,
    load_typed_work_object, params, refused,
};

/// Every session that ever held the run, from its immutable history: claim,
/// renewal, recovery, and handoff transitions on the run feed. The mutable
/// claim row forgets former holders; this does not.
pub(super) fn run_holder_history(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<Vec<SessionId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object_id FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = 'work_event'
         ORDER BY position",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut holders = Vec::new();
    for stored in rows {
        let hash =
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
        let event: WorkEvent = load_typed_work_object(connection, &hash, "work_event")?;
        if event.run_id != Some(run_id) {
            continue;
        }
        if let Some(claim) = &event.claim {
            holders.push(claim.holder.clone());
        }
        match &event.transition {
            WorkTransition::Claimed { claim, .. } | WorkTransition::ClaimRenewed { claim } => {
                holders.push(claim.holder.clone());
            }
            WorkTransition::HandedOff { from, to, .. } => {
                holders.push(from.clone());
                holders.push(to.clone());
            }
            _ => {}
        }
    }
    holders.sort_by(|left, right| left.0.cmp(&right.0));
    holders.dedup();
    Ok(holders)
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ModePolicyMismatch {
    DisallowedMode,
    SelectedPinMismatch(AcceptanceEvaluationMode),
}

/// Current policy membership and task-pin agreement, shared by recording and
/// consumption. Enabling evaluation and evaluator affiliation are separate
/// checks; each phase keeps its own outward refusal or stale cause.
pub(super) fn assess_mode_policy(
    item: &WorkItem,
    policy: &AcceptanceEvaluationPolicy,
    mode: AcceptanceEvaluationMode,
) -> Result<(), ModePolicyMismatch> {
    if !policy.allows(mode) {
        return Err(ModePolicyMismatch::DisallowedMode);
    }
    if let Some(selected) = item.evaluation_mode
        && selected != mode
    {
        return Err(ModePolicyMismatch::SelectedPinMismatch(selected));
    }
    Ok(())
}

pub(super) fn admit_mode(
    item: &WorkItem,
    policy: &AcceptanceEvaluationPolicy,
    mode: AcceptanceEvaluationMode,
) -> Result<(), StoreError> {
    let context = EligibilityContext {
        item,
        policy,
        mode,
        evaluator: None,
        parent: None,
    };
    if policy.is_self_asserted() {
        return Err(context.refused(
            EvaluationEligibilityMismatch::EvaluationDisabled,
            "the project policy does not enable acceptance evaluation; completion stays self-asserted",
            None,
        ));
    }
    match assess_mode_policy(item, policy, mode) {
        Err(ModePolicyMismatch::DisallowedMode) => Err(context.refused(
            EvaluationEligibilityMismatch::ModeDisallowed,
            format!(
                "mode {} is not allowed by the project policy; allowed: {}",
                mode.word(),
                policy
                    .allowed_modes
                    .iter()
                    .map(|mode| mode.word())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None,
        )),
        Err(ModePolicyMismatch::SelectedPinMismatch(selected)) => Err(context.refused(
            EvaluationEligibilityMismatch::TaskPinMismatch,
            format!(
                "this task is marked for mode {}; evaluate in that mode",
                selected.word()
            ),
            None,
        )),
        Ok(()) => Ok(()),
    }
}

/// The evaluation a refused same-session evaluator can request instead, in
/// a mode the project admits: independent where admitted, else a sub-agent
/// one; `None` where the project admits only same-session.
fn host_evaluation_words(policy: &AcceptanceEvaluationPolicy) -> Option<&'static str> {
    if policy.allows(AcceptanceEvaluationMode::IndependentSession) {
        Some("request an independent evaluation from the host")
    } else if policy.allows(AcceptanceEvaluationMode::SubAgent) {
        Some("request a sub-agent evaluation from the host")
    } else {
        None
    }
}

/// Refusal words for a same-session evaluation of a task no one marked for
/// it, in a project that admits another mode.
fn unmarked_same_session(policy: &AcceptanceEvaluationPolicy) -> String {
    format!(
        "this task is not marked for same-session evaluation and the project admits another mode: {}; same-session needs the task marked for it by someone other than its executor",
        host_evaluation_words(policy).unwrap_or("request an evaluation in that mode from the host")
    )
}

/// Refusal words for a same-session mark that waives nothing: `why` says
/// whose or what mark it is. The mark is fixed by having a session that
/// never held or executed the run clear it and set it again, or, where the project admits
/// one, by marking the task for another mode and asking the host. Where the
/// project admits only same-session, clearing the mark alone is enough: an
/// unmarked task there takes its executor's own evaluation.
fn ineligible_mark(policy: &AcceptanceEvaluationPolicy, why: &str) -> String {
    let instead = match host_evaluation_words(policy) {
        Some(words) => format!(", or have the mark changed to that mode and {words}"),
        None if policy.admits_only_same_session() => {
            "; in this project, which admits only same-session, clearing the mark alone lets its executor evaluate".into()
        }
        None => String::new(),
    };
    format!(
        "this task's same-session mark {why}, so it cannot waive independent evaluation: have a session that never held or executed this run clear the mark and set it again{instead}"
    )
}

/// The sessions around one evaluation of a run: who evaluates, if known,
/// and who holds, executes or has held it.
pub(super) struct SessionStanding<'a> {
    pub(super) evaluator: Option<&'a SessionId>,
    pub(super) holder: Option<&'a SessionId>,
    pub(super) executor: Option<&'a SessionId>,
    pub(super) history: &'a [SessionId],
}

impl SessionStanding<'_> {
    fn includes(&self, session: &SessionId) -> bool {
        self.evaluator == Some(session) || self.holds_or_held(session)
    }

    /// Whether `session` holds or executes the run, or held it earlier.
    fn holds_or_held(&self, session: &SessionId) -> bool {
        self.holder == Some(session)
            || self.executor == Some(session)
            || self.history.contains(session)
    }

    /// Independence requires a known evaluator that has never held or executed
    /// this run. Admission and consumption adapt this same relationship.
    pub(super) fn evaluator_is_independent(&self) -> bool {
        self.evaluator
            .is_some_and(|session| !self.holds_or_held(session))
    }
}

/// Who set the task's current same-session mark: the session of the
/// Created or Revised event that turned it on, found by walking the item's
/// own events in order. Revisions that keep the mark keep its author;
/// clearing or changing it ends the mark, and setting it again authors a new
/// one. `None` when the mark's author has no recorded session, or no native
/// event of the item shows the mark being turned on, as for an item whose
/// earlier history was restored rather than recorded here, or a successor
/// whose creation only carried the mark over from the item it was detached
/// from: the session that detached it did not set the mark.
fn same_session_mark_author(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Option<SessionId>, StoreError> {
    Ok(mark_author(
        canonical_work_events_for_item(connection, item.work_id)?
            .into_iter()
            .map(|event| MarkStep {
                transition: match event.transition {
                    WorkTransition::Created { .. } if carried_over(&event.actor) => {
                        MarkTransition::Other
                    }
                    WorkTransition::Created { .. } => MarkTransition::Created,
                    WorkTransition::Revised { .. } => MarkTransition::Revised,
                    _ => MarkTransition::Other,
                },
                marked: event.work.evaluation_mode == Some(AcceptanceEvaluationMode::SameSession),
                session: event.actor.session_id,
            }),
    ))
}

/// Whether a creation copied the item from another one, as a detach does,
/// rather than taking its fields from the creating session.
fn carried_over(actor: &ActorContext) -> bool {
    actor.provenance_chain.iter().any(|link| {
        link.relation == ProvenanceRelation::DerivedFrom && link.source == DETACH_PROVENANCE_SOURCE
    })
}

/// How one of an item's events can bear on its evaluation-mode mark.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MarkTransition {
    Created,
    Revised,
    Other,
}

/// One of an item's events, in order, as the mark's authorship reads it.
pub(super) struct MarkStep {
    pub(super) transition: MarkTransition,
    /// Whether the item is marked for same-session after the event.
    pub(super) marked: bool,
    pub(super) session: Option<SessionId>,
}

/// The author of the current same-session mark over the item's native
/// events in order: the session of the Created event that made the item
/// marked, or of the Revised event whose previous native event showed it
/// unmarked. A mark first seen on any other step, or on a Revised event with
/// no native event before it, has no provable author.
pub(super) fn mark_author(steps: impl IntoIterator<Item = MarkStep>) -> Option<SessionId> {
    let mut previously_marked: Option<bool> = None;
    let mut author: Option<Option<SessionId>> = None;
    for step in steps {
        if !step.marked {
            author = None;
        } else if author.is_none() {
            let turned_on = match step.transition {
                MarkTransition::Created => true,
                MarkTransition::Revised => previously_marked == Some(false),
                MarkTransition::Other => false,
            };
            author = Some(turned_on.then_some(step.session).flatten());
        }
        previously_marked = Some(step.marked);
    }
    author.flatten()
}

/// Why an executor-affiliated evaluation of `item` is not admitted, or
/// `None` when it is. A same-session one: an unmarked task takes it only
/// where the project admits no other mode, and a marked one only while the
/// session that marked it neither evaluates, holds nor executes the run, now
/// or earlier in it. A sub-agent one must be recorded from a distinct child
/// session: one recorded from the run's holder, executor or a former holder
/// is the executor's own evaluation. Identities are asserted: this stops
/// forgetting, shortcuts and re-rolls, not deliberate forgery.
pub(super) fn same_session_ineligibility(
    connection: &Connection,
    item: &WorkItem,
    policy: &AcceptanceEvaluationPolicy,
    mode: AcceptanceEvaluationMode,
    standing: &SessionStanding<'_>,
) -> Result<Option<SameSessionRefusal>, StoreError> {
    if mode == AcceptanceEvaluationMode::SubAgent {
        return Ok(standing
            .evaluator
            .is_some_and(|evaluator| standing.holds_or_held(evaluator))
            .then(|| SameSessionRefusal {
                reason: format!(
                    "a sub_agent evaluation must be recorded from a distinct child session with a holder or executor as its parent; this one comes from a session that holds, executes or held the run, which makes it the executor's own evaluation: {}",
                    host_evaluation_words(policy)
                        .unwrap_or("record it from the sub-agent's own session")
                ),
                mismatch: EvaluationEligibilityMismatch::SubAgentEvaluatorAffiliated,
                mark_author: None,
            }));
    }
    if mode != AcceptanceEvaluationMode::SameSession {
        return Ok(None);
    }
    Ok(match item.evaluation_mode {
        None if !policy.admits_only_same_session() => Some(SameSessionRefusal {
            reason: unmarked_same_session(policy),
            mismatch: EvaluationEligibilityMismatch::SameSessionUnmarked,
            mark_author: None,
        }),
        Some(AcceptanceEvaluationMode::SameSession) => {
            match same_session_mark_author(connection, item)? {
                None => Some(SameSessionRefusal {
                    reason: ineligible_mark(
                        policy,
                        "has no author recorded on this item (it was restored, or carried over by a detach)",
                    ),
                    mismatch: EvaluationEligibilityMismatch::MarkAuthorUnrecorded,
                    mark_author: None,
                }),
                Some(author) if standing.includes(&author) => Some(SameSessionRefusal {
                    reason: ineligible_mark(
                        policy,
                        "was set by a session that evaluates, holds or executes its run",
                    ),
                    mismatch: EvaluationEligibilityMismatch::MarkAuthorAffiliated,
                    mark_author: Some(author),
                }),
                Some(_) => None,
            }
        }
        _ => None,
    })
}

pub(super) fn admit_identity(
    context: &EligibilityContext<'_>,
    request: &RecordAcceptanceEvaluationRequest,
    evaluator_session: &SessionId,
    holder: Option<&SessionId>,
    executor: Option<&SessionId>,
    history: &[SessionId],
) -> Result<(), StoreError> {
    let item = context.item;
    let executing = |session: &SessionId| holder == Some(session) || executor == Some(session);
    match request.mode {
        AcceptanceEvaluationMode::SameSession => {
            if !executing(evaluator_session) {
                return Err(context.refused(
                    EvaluationEligibilityMismatch::SameSessionNotExecuting,
                    "same_session evaluation must come from the session that holds or executes the run",
                    None,
                ));
            }
        }
        AcceptanceEvaluationMode::SubAgent => {
            let parent = IdentityShape::of_request(request)
                .child_parent()
                .ok_or_else(|| {
                    refused(
                        item.work_id,
                        "sub_agent mode needs the attested parent session",
                    )
                })?;
            if !executing(parent) {
                return Err(context.refused(
                    EvaluationEligibilityMismatch::SubAgentParentNotExecuting,
                    "sub_agent parent session must hold or execute the run",
                    None,
                ));
            }
        }
        AcceptanceEvaluationMode::IndependentSession => {
            let standing = SessionStanding {
                evaluator: Some(evaluator_session),
                holder,
                executor,
                history,
            };
            if !standing.evaluator_is_independent() {
                return Err(context.refused(
                    EvaluationEligibilityMismatch::IndependentEvaluatorAffiliated,
                    "independent_session evaluation must come from a session that neither holds nor executes the run, now or at any earlier point of this run",
                    None,
                ));
            }
        }
    }
    Ok(())
}
