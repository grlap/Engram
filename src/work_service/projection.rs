//! Shared projection helpers: text compaction, actor labels, bounded
//! summaries, evidence and obligation pages, and response-budget fitting.

use super::{
    ActorContext, AgentWorkSession, CompletionRecoverySnapshot, CompletionSeal, ControlWorkBinding,
    DateTime, MAX_ACCEPTANCE_ITEMS, MAX_ACTOR_CONTEXT_BYTES, MAX_AGENT_WORK_RESPONSE_BYTES,
    MAX_FOCUS_RELATIONS, MAX_LABEL_ITEMS, MAX_OBLIGATION_PAGE_BYTES, MAX_SUMMARY_BYTES,
    MemorySummary, ObjectHash, ReadyWork, ReadyWorkSummary, RequiredChildWaiverCandidate,
    RestoredWorkEvidence, Serialize, SessionId, SqliteStore, StoreError, Utc, WorkClaim,
    WorkClaimState, WorkCompletionRecoveryCause, WorkDecomposition, WorkDecompositionChildSummary,
    WorkDecompositionSummary, WorkEvidence, WorkEvidenceKind, WorkEvidenceProjectionSummary,
    WorkEvidenceSummary, WorkFocusView, WorkGateEvidenceSummary, WorkHandoffOffer,
    WorkHandoffSummary, WorkId, WorkItem, WorkItemSummary, WorkLifecycle, WorkMemoryIndexEntry,
    WorkMutationReceipt, WorkNextSection, WorkNextView, WorkObligationGuidance, WorkObligationPage,
    WorkObligationResolution, WorkObligationState, WorkObligationSummary, WorkPrerequisiteState,
    WorkRun, WorkRunId, WorkRunState, WorkRunSummary, WorkSectionOmission,
    WorkSectionOmissionReason, WorkSessionState, is_unsafe_rendered_text_char,
    validate_gate_evidence_payload,
};

pub(super) fn selected_work_next_sections(requested: &[WorkNextSection]) -> Vec<WorkNextSection> {
    let mut sections = if requested.is_empty() {
        vec![
            WorkNextSection::Focus,
            WorkNextSection::Ready,
            WorkNextSection::Catalog,
            WorkNextSection::Changes,
            WorkNextSection::Memories,
        ]
    } else {
        requested.to_vec()
    };
    sections.sort_by_key(|section| match section {
        WorkNextSection::Focus => 0,
        WorkNextSection::Ready => 1,
        WorkNextSection::Catalog => 2,
        WorkNextSection::Changes => 3,
        WorkNextSection::Memories => 4,
    });
    sections.dedup();
    sections
}

pub(super) fn agent_work_session(state: &WorkSessionState) -> AgentWorkSession {
    AgentWorkSession {
        project_id: state.project_id.clone(),
        session_id: state.session_id.clone(),
        focused_work_id: state.focused_work_id,
        confirmed_project_cursor: state.project_cursor,
        pending_delivery: state.tentative_project_cursor.is_some(),
        updated_at: state.updated_at,
    }
}

pub(super) fn compact_text(value: &str) -> String {
    compact_text_to(value, MAX_SUMMARY_BYTES)
}

pub(super) fn projected_actor_context(actor: &ActorContext) -> Option<String> {
    actor.attribution_context().map(str::to_owned)
}

pub(crate) fn actor_label(actor_id: &str, actor_context: Option<&str>) -> String {
    actor_context.map_or_else(
        || actor_id.to_owned(),
        |actor_context| format!("{actor_id} ({actor_context})"),
    )
}

pub(crate) fn terminal_safe_actor_label(actor_id: &str, actor_context: Option<&str>) -> String {
    let label = actor_label(actor_id, actor_context);
    let mut safe = String::with_capacity(label.len());
    for character in label.chars() {
        if is_unsafe_rendered_text_char(character) {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

pub(super) fn normalize_actor_context(actor_context: Option<String>) -> (Option<String>, bool) {
    let Some(original) = actor_context else {
        return (None, false);
    };
    let mut stripped = String::with_capacity(original.len());
    let mut stripped_unsafe_run = false;
    for character in original.chars() {
        if is_unsafe_rendered_text_char(character) {
            stripped_unsafe_run = true;
            continue;
        }
        if stripped_unsafe_run {
            let previous_is_space = stripped
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
            if !stripped.is_empty() && !previous_is_space && !character.is_whitespace() {
                stripped.push(' ');
            }
            stripped_unsafe_run = false;
        }
        stripped.push(character);
    }
    let trimmed = stripped.trim();
    let mut end = trimmed.len().min(MAX_ACTOR_CONTEXT_BYTES);
    while !trimmed.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let normalized = trimmed[..end].trim_end();
    let changed = normalized != original;
    (
        (!normalized.is_empty()).then(|| normalized.to_owned()),
        changed,
    )
}

pub(super) fn compact_text_to(value: &str, max_bytes: usize) -> String {
    let value = value.trim();
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes.saturating_sub(3).min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &value[..end])
}

pub(super) fn work_item_summary(work: &WorkItem) -> WorkItemSummary {
    WorkItemSummary {
        work_id: work.work_id,
        short_ref: work.short_ref.clone(),
        root_id: work.root_id,
        parent_id: work.parent_id,
        child_requirement: work.child_requirement,
        title: compact_text(&work.title),
        outcome: compact_text(&work.outcome),
        acceptance: work
            .acceptance
            .iter()
            .take(MAX_ACCEPTANCE_ITEMS)
            .map(|criterion| compact_text(criterion))
            .collect(),
        acceptance_count: work.acceptance.len(),
        kind: work.kind,
        priority: work.priority,
        labels: work
            .labels
            .iter()
            .take(MAX_LABEL_ITEMS)
            .map(|label| compact_text(label))
            .collect(),
        assigned_to: work.assigned_to.as_deref().map(compact_text),
        lifecycle: work.lifecycle,
        restored: work.restored,
        revision: work.revision,
        active_run_id: work.active_run_id,
        superseded_by: work.superseded_by,
        prerequisite_state: None,
        updated_at: work.updated_at,
    }
}

pub(super) const fn child_lifecycle_is_unfinished(lifecycle: WorkLifecycle) -> bool {
    match lifecycle {
        WorkLifecycle::Open | WorkLifecycle::Proposed => true,
        WorkLifecycle::Completed | WorkLifecycle::Cancelled | WorkLifecycle::Superseded => false,
    }
}

pub(super) const fn child_lifecycle_priority(lifecycle: WorkLifecycle) -> u8 {
    if child_lifecycle_is_unfinished(lifecycle) {
        0
    } else {
        1
    }
}

fn work_item_summary_with_prerequisite_state(
    work: &WorkItem,
    state: WorkPrerequisiteState,
) -> WorkItemSummary {
    let mut summary = work_item_summary(work);
    summary.prerequisite_state = Some(state);
    summary
}

pub(super) fn bounded_prerequisite_summaries(
    prerequisites: Vec<(WorkItem, WorkPrerequisiteState)>,
    omitted_by_state: [usize; 3],
) -> (Vec<WorkItemSummary>, Vec<WorkSectionOmission>) {
    let reasons = [
        WorkSectionOmissionReason::DeadPrerequisiteCountLimit,
        WorkSectionOmissionReason::PendingPrerequisiteCountLimit,
        WorkSectionOmissionReason::SatisfiedPrerequisiteCountLimit,
    ];
    let omissions = reasons
        .into_iter()
        .zip(omitted_by_state)
        .filter_map(|(reason, omitted_count)| {
            (omitted_count != 0).then_some(WorkSectionOmission {
                section: WorkNextSection::Focus,
                reason,
                omitted_count,
            })
        })
        .collect();
    let summaries = prerequisites
        .into_iter()
        .map(|(work, state)| work_item_summary_with_prerequisite_state(&work, state))
        .collect();
    (summaries, omissions)
}

pub(super) fn ready_work_summary(status: ReadyWork) -> ReadyWorkSummary {
    let blocker_count = status.blockers.len();
    ReadyWorkSummary {
        work: work_item_summary(&status.work),
        availability: status.availability,
        blocking_parent: status.blocking_parent,
        reason_codes: status.reason_codes,
        why: status
            .why
            .into_iter()
            .take(MAX_FOCUS_RELATIONS)
            .map(|reason| compact_text(&reason))
            .collect(),
        blocked_by: status
            .blocked_by
            .into_iter()
            .take(MAX_FOCUS_RELATIONS)
            .collect(),
        blocker_count,
    }
}

pub(super) fn work_run_summary(run: &WorkRun) -> WorkRunSummary {
    WorkRunSummary {
        root_execution_id: run.root_execution_id,
        work_id: run.work_id,
        run_id: run.run_id,
        generation: run.generation,
        executor: run.executor.clone(),
        state: run.state,
        revision: run.revision,
        last_checkpoint: run.last_checkpoint.clone(),
        completion_seal: run.completion_seal.clone(),
    }
}

pub(super) fn owned_control_work_binding(
    work: &WorkItem,
    run: &WorkRun,
    claim: Option<&WorkClaim>,
    session_id: &SessionId,
    now: DateTime<Utc>,
) -> Option<ControlWorkBinding> {
    let claim = claim?;
    (work.lifecycle == WorkLifecycle::Open
        && work.active_run_id == Some(run.run_id)
        && run.work_id == work.work_id
        && matches!(run.state, WorkRunState::Claimed | WorkRunState::Active)
        && claim.work_id == work.work_id
        && claim.run_id == run.run_id
        && claim.accepted_work_revision == work.revision
        && claim.holder == *session_id
        && claim.state == WorkClaimState::Active
        && claim.expires_at > now)
        .then_some(ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: work.revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        })
}

pub(super) fn work_handoff_summary(offer: &WorkHandoffOffer) -> WorkHandoffSummary {
    WorkHandoffSummary {
        offer_id: offer.offer_id,
        from: offer.from.clone(),
        to: offer.to.clone(),
        state: offer.state,
        expires_at: offer.expires_at,
    }
}

pub(super) fn required_child_waiver_candidate(work: WorkItem) -> RequiredChildWaiverCandidate {
    RequiredChildWaiverCandidate {
        work_id: work.work_id,
        short_ref: work.short_ref,
        lifecycle: work.lifecycle,
    }
}

pub(super) fn work_memory_index(memory: MemorySummary) -> WorkMemoryIndexEntry {
    WorkMemoryIndexEntry {
        memory_id: memory.memory_id,
        version: memory.version,
        status: memory.status,
        kind: memory.kind,
        title: compact_text(&memory.title),
        sensitivity: memory.sensitivity,
        created_at: memory.created_at,
    }
}

pub(super) fn work_decomposition_summary(
    decomposition: &WorkDecomposition,
) -> WorkDecompositionSummary {
    let mut parent = work_item_summary(&decomposition.parent);
    minimize_work_item_summary(&mut parent);
    WorkDecompositionSummary {
        parent,
        child_count: decomposition.children.len(),
        children: decomposition
            .children
            .iter()
            .map(|child| WorkDecompositionChildSummary {
                work_id: child.work_id,
                short_ref: child.short_ref.clone(),
                revision: child.revision,
            })
            .collect(),
        details_omitted: true,
    }
}

fn minimize_work_item_summary(work: &mut WorkItemSummary) {
    work.title = compact_text_to(&work.title, 64);
    work.outcome.clear();
    work.acceptance.clear();
    work.labels.clear();
}

pub(super) fn compact_obligations(status: &ReadyWork) -> Vec<String> {
    status
        .why
        .iter()
        .take(MAX_FOCUS_RELATIONS)
        .map(|reason| compact_text(reason))
        .collect()
}

pub(super) fn compact_mutation_receipt(
    work: &WorkItem,
    control_binding: Option<ControlWorkBinding>,
    receipt: serde_json::Value,
) -> WorkMutationReceipt {
    if let Ok(existing) = serde_json::from_value::<WorkMutationReceipt>(receipt.clone()) {
        return existing;
    }
    let result = match receipt {
        serde_json::Value::Object(object) => {
            let allowed = [
                "attached",
                "blocker_id",
                "checkpoint",
                "claim_id",
                "completion_seal",
                "evidence",
                "evidence_kind",
                "expires_at",
                "fence",
                "generation",
                "lifecycle",
                "offer_id",
                "revision",
                "run_id",
                "state",
                "superseded_by",
                "work_revision",
            ];
            let selected = object
                .into_iter()
                .filter(|(key, value)| {
                    allowed.contains(&key.as_str())
                        && (value.is_string()
                            || value.is_number()
                            || value.is_boolean()
                            || value.is_null())
                })
                .map(|(key, value)| {
                    let value = value.as_str().map_or(value.clone(), |text| {
                        serde_json::Value::String(compact_text(text))
                    });
                    (key, value)
                })
                .collect();
            serde_json::Value::Object(selected)
        }
        serde_json::Value::String(value) => serde_json::Value::String(compact_text(&value)),
        scalar @ (serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)) => scalar,
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .filter(|value| value.is_string() || value.is_number() || value.is_boolean())
                .take(MAX_FOCUS_RELATIONS)
                .map(|value| {
                    value.as_str().map_or(value.clone(), |text| {
                        serde_json::Value::String(compact_text(text))
                    })
                })
                .collect(),
        ),
    };
    WorkMutationReceipt {
        work_id: work.work_id,
        work_ref: work.short_ref.clone(),
        revision: work.revision,
        control_binding,
        result,
    }
}

pub(super) fn bounded_ready_prefix(
    source: Vec<ReadyWorkSummary>,
    budget: usize,
) -> Result<Vec<ReadyWorkSummary>, StoreError> {
    let mut bounded = Vec::new();
    for mut item in source {
        bounded.push(item.clone());
        if serde_json::to_vec(&bounded)?.len() <= budget {
            continue;
        }
        bounded.pop();
        if bounded.is_empty() {
            minimize_work_item_summary(&mut item.work);
            item.why.clear();
            item.blocked_by.clear();
            item.reason_codes.truncate(MAX_FOCUS_RELATIONS);
            bounded.push(item);
            if serde_json::to_vec(&bounded)?.len() > budget {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "minimal work summary exceeds its {budget}-byte section budget"
                )));
            }
        }
        break;
    }
    Ok(bounded)
}

pub(super) fn count_omission(
    section: WorkNextSection,
    omitted_count: usize,
) -> WorkSectionOmission {
    WorkSectionOmission {
        section,
        reason: WorkSectionOmissionReason::CountLimit,
        omitted_count,
    }
}

pub(super) fn prioritized_focus_evidence(
    candidates: Vec<WorkEvidenceProjectionSummary>,
    obligation_page: &WorkObligationPage,
) -> Vec<ObjectHash> {
    let required_environments = obligation_page
        .items
        .iter()
        .filter(|obligation| obligation.state == WorkObligationState::Open)
        .filter_map(|obligation| obligation.requirement.required_environment.clone())
        .collect();
    prioritized_focus_evidence_hashes(candidates, required_environments)
}

pub(super) fn prioritized_focus_evidence_hashes(
    mut candidates: Vec<WorkEvidenceProjectionSummary>,
    mut required_environments: Vec<ObjectHash>,
) -> Vec<ObjectHash> {
    candidates.sort_by(|left, right| left.hash.as_str().cmp(right.hash.as_str()));
    required_environments.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    required_environments.dedup();
    let environment_hashes = candidates
        .iter()
        .filter(|candidate| candidate.kind == WorkEvidenceKind::Environment)
        .map(|candidate| candidate.hash.clone())
        .collect::<std::collections::HashSet<_>>();
    let mut selected = Vec::new();
    for environment in required_environments {
        if environment_hashes.contains(&environment) {
            push_focus_evidence(&mut selected, &environment);
        }
        if selected.len() == MAX_FOCUS_RELATIONS {
            return selected;
        }
    }
    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.kind == WorkEvidenceKind::Verification)
    {
        match candidate.environment.as_ref() {
            Some(environment) if environment_hashes.contains(environment) => {
                let environment_is_visible = selected.contains(environment);
                let needed = usize::from(!environment_is_visible) + 1;
                if selected.len() + needed > MAX_FOCUS_RELATIONS {
                    continue;
                }
                // Environment first means byte trimming pops its dependent
                // verification before it can break visible typed closure.
                push_focus_evidence(&mut selected, environment);
                push_focus_evidence(&mut selected, &candidate.hash);
            }
            _ => push_focus_evidence(&mut selected, &candidate.hash),
        }
        if selected.len() == MAX_FOCUS_RELATIONS {
            return selected;
        }
    }
    for candidate in candidates {
        if candidate.kind != WorkEvidenceKind::Verification {
            push_focus_evidence(&mut selected, &candidate.hash);
        }
        if selected.len() == MAX_FOCUS_RELATIONS {
            break;
        }
    }
    selected
}

fn push_focus_evidence(selected: &mut Vec<ObjectHash>, hash: &ObjectHash) {
    if selected.len() < MAX_FOCUS_RELATIONS && !selected.contains(hash) {
        selected.push(hash.clone());
    }
}

pub(super) fn compact_work_evidence(evidence: &WorkEvidence) -> Result<String, StoreError> {
    let Some(gate) = evidence.gate.as_ref() else {
        return Ok(compact_text(&evidence.summary));
    };
    validate_gate_evidence_payload(evidence).map_err(StoreError::InvalidWorkProjection)?;
    Ok(gate_evidence_summary(gate, true))
}

pub(super) fn compact_restored_work_evidence(
    evidence: &RestoredWorkEvidence,
) -> Result<String, StoreError> {
    let Some(gate) = evidence.gate.as_ref() else {
        return Ok(compact_text(&evidence.summary));
    };
    gate.validate(&evidence.refs)
        .map_err(StoreError::InvalidWorkProjection)?;
    Ok(gate_evidence_summary(gate, true))
}

pub(super) fn project_full_notes(
    page: &mut crate::storage::WorkNotePage,
) -> Result<(), StoreError> {
    for note in &mut page.items {
        if let Some(gate) = &note.gate {
            gate.validate(&note.refs)
                .map_err(StoreError::InvalidWorkProjection)?;
            note.summary = gate_evidence_summary(gate, false);
        } else if note.kind == WorkEvidenceKind::Environment {
            note.summary = "host-recorded environment identity".into();
        }
    }
    Ok(())
}

fn gate_evidence_summary(gate: &crate::GateEvidenceRecord, compact: bool) -> String {
    let project = |text: &str| {
        if compact {
            compact_text(text)
        } else {
            text.to_owned()
        }
    };
    if gate.passed {
        return project(&format!("gate {} passed", gate.name));
    }
    let count = gate.failed.len();
    let limit = if compact { 2 } else { count };
    let listed = gate
        .failed
        .iter()
        .take(limit)
        .map(|failure| project(failure))
        .collect::<Vec<_>>()
        .join(", ");
    let omitted = count.saturating_sub(limit);
    let suffix = if omitted == 0 {
        String::new()
    } else {
        format!(" (+{omitted} more)")
    };
    project(&format!(
        "gate {} failed ({count} failures): {listed}{suffix}",
        gate.name
    ))
}

pub(super) fn work_evidence_summary(
    store: &SqliteStore,
    run_id: WorkRunId,
    hash: &ObjectHash,
) -> Result<WorkEvidenceSummary, StoreError> {
    match store.work_evidence_kind(run_id, hash)? {
        WorkEvidenceKind::Generic => {
            let evidence = store.get::<WorkEvidence>(hash)?.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "generic evidence object {hash} is missing"
                ))
            })?;
            let summary = compact_work_evidence(&evidence)?;
            let gate = evidence.gate.as_ref().map(|gate| WorkGateEvidenceSummary {
                name: gate.name.clone(),
                passed: gate.passed,
                failed_count: gate.failed.len(),
            });
            Ok(WorkEvidenceSummary {
                evidence: hash.clone(),
                evidence_kind: WorkEvidenceKind::Generic,
                non_holder: false,
                gate,
                workspace_id: None,
                source_revision: None,
                producer_session_id: evidence.actor.session_id.clone(),
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                actor_context: projected_actor_context(&evidence.actor),
                check_kind: None,
                check_fingerprint: None,
                verification_result: None,
                environment_fingerprint: None,
                environment: None,
                environment_components: None,
                summary,
                created_at: evidence.created_at,
            })
        }
        WorkEvidenceKind::Verification => {
            let evidence = store.load_verification_evidence(hash)?;
            Ok(WorkEvidenceSummary {
                evidence: hash.clone(),
                evidence_kind: WorkEvidenceKind::Verification,
                non_holder: false,
                gate: None,
                workspace_id: Some(compact_text(&evidence.source_basis.workspace_id)),
                source_revision: Some(compact_text(&evidence.source_basis.source_revision)),
                producer_session_id: Some(evidence.session_id),
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                actor_context: projected_actor_context(&evidence.actor),
                check_kind: Some(evidence.check_kind),
                check_fingerprint: Some(evidence.check_fingerprint),
                verification_result: Some(evidence.result),
                environment_fingerprint: None,
                environment: evidence.environment,
                environment_components: None,
                summary: compact_text(&evidence.summary),
                created_at: evidence.completed_at,
            })
        }
        WorkEvidenceKind::Environment => {
            let evidence = store.load_environment_evidence(hash)?;
            Ok(WorkEvidenceSummary {
                evidence: hash.clone(),
                evidence_kind: WorkEvidenceKind::Environment,
                non_holder: false,
                gate: None,
                workspace_id: Some(compact_text(&evidence.source_basis.workspace_id)),
                source_revision: Some(compact_text(&evidence.source_basis.source_revision)),
                producer_session_id: Some(evidence.session_id),
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                actor_context: projected_actor_context(&evidence.actor),
                check_kind: None,
                check_fingerprint: None,
                verification_result: None,
                environment_fingerprint: Some(evidence.environment_fingerprint),
                environment: None,
                environment_components: evidence.components,
                summary: "host-recorded environment identity".into(),
                created_at: evidence.observed_at,
            })
        }
    }
}

pub(super) fn restored_work_evidence_summary(
    hash: ObjectHash,
    evidence: &RestoredWorkEvidence,
) -> Result<WorkEvidenceSummary, StoreError> {
    let summary = compact_restored_work_evidence(evidence)?;
    let gate = evidence.gate.as_ref().map(|gate| WorkGateEvidenceSummary {
        name: gate.name.clone(),
        passed: gate.passed,
        failed_count: gate.failed.len(),
    });
    Ok(WorkEvidenceSummary {
        evidence: hash,
        non_holder: false,
        evidence_kind: WorkEvidenceKind::Generic,
        gate,
        workspace_id: None,
        source_revision: None,
        producer_session_id: evidence.actor.session_id.clone(),
        actor_id: Some(compact_text(&evidence.actor.actor_id)),
        actor_context: projected_actor_context(&evidence.actor),
        check_kind: None,
        check_fingerprint: None,
        verification_result: None,
        environment_fingerprint: None,
        environment: None,
        environment_components: None,
        summary,
        created_at: evidence.created_at,
    })
}

pub(super) fn work_observation_summary(
    hash: ObjectHash,
    observation: &crate::domain::WorkObservation,
) -> WorkEvidenceSummary {
    WorkEvidenceSummary {
        evidence: hash,
        non_holder: true,
        evidence_kind: WorkEvidenceKind::Generic,
        gate: None,
        workspace_id: None,
        source_revision: None,
        producer_session_id: observation.actor.session_id.clone(),
        actor_id: Some(compact_text(&observation.actor.actor_id)),
        actor_context: projected_actor_context(&observation.actor),
        check_kind: None,
        check_fingerprint: None,
        verification_result: None,
        environment_fingerprint: None,
        environment: None,
        environment_components: None,
        summary: compact_text(&observation.summary),
        created_at: observation.created_at,
    }
}

fn work_obligation_summary(record: &crate::storage::WorkObligationRecord) -> WorkObligationSummary {
    let evidence = record.resolution.as_ref().and_then(|event| {
        if let WorkObligationResolution::Satisfied { evidence, .. } = &event.resolution {
            Some(evidence.clone())
        } else {
            None
        }
    });
    let waived_by = record.resolution.as_ref().and_then(|event| {
        if let WorkObligationResolution::Waived { waived_by, .. } = &event.resolution {
            Some(compact_text(waived_by))
        } else {
            None
        }
    });
    let guidance = if record.state == WorkObligationState::Open {
        WorkObligationGuidance::RecordVerificationThenCheckpoint {
            requirement: record.obligation.requirement.clone(),
            host_waiver_requestable: true,
        }
    } else {
        WorkObligationGuidance::None
    };
    WorkObligationSummary {
        obligation_id: record.obligation.obligation_id,
        definition: record.definition_hash.clone(),
        rule_set: record.obligation.rule_set.clone(),
        state: record.state,
        rule: record.obligation.rule.clone(),
        requirement: record.obligation.requirement.clone(),
        triggering_observation: record.obligation.triggering_observation.clone(),
        resolution: record.resolution_hash.clone(),
        evidence,
        waived_by,
        guidance,
    }
}

pub(super) fn work_obligation_page(
    store: &SqliteStore,
    work_id: WorkId,
) -> Result<WorkObligationPage, StoreError> {
    let Some(run) = store.latest_work_run(work_id)? else {
        return Ok(WorkObligationPage::default());
    };
    work_obligation_page_from_records(store.work_run_obligations(run.run_id)?)
}

pub(super) fn work_completion_recovery_page(
    snapshot: &CompletionRecoverySnapshot,
) -> Result<WorkObligationPage, StoreError> {
    let state = matches!(
        &snapshot.recovery.cause,
        WorkCompletionRecoveryCause::OpenObligation { .. }
    )
    .then_some(WorkObligationState::Open);
    let records = snapshot
        .obligations
        .iter()
        .filter(|record| state.is_none_or(|expected| record.state == expected))
        .cloned()
        .collect();
    work_obligation_page_from_records(records)
}

pub(super) fn sealed_work_obligation_page(
    store: &SqliteStore,
    seal: &CompletionSeal,
) -> Result<WorkObligationPage, StoreError> {
    let records = store.work_run_obligations(seal.run_id)?;
    let mut bindings = records
        .iter()
        .map(|record| {
            let resolution = record.resolution_hash.clone().ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "sealed obligation {} has no terminal resolution",
                    record.obligation.obligation_id.0
                ))
            })?;
            Ok(crate::CompletionObligationBinding {
                obligation_id: record.obligation.obligation_id,
                definition: record.definition_hash.clone(),
                resolution,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    bindings.sort_by(|left, right| {
        left.obligation_id
            .0
            .as_bytes()
            .cmp(right.obligation_id.0.as_bytes())
            .then_with(|| left.definition.as_str().cmp(right.definition.as_str()))
    });
    if bindings != seal.obligations {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {:?} does not match its canonical obligation closure",
            seal.run_id
        )));
    }
    work_obligation_page_from_records(records)
}

pub(super) fn work_obligation_page_from_records(
    records: Vec<crate::storage::WorkObligationRecord>,
) -> Result<WorkObligationPage, StoreError> {
    let mut page = count_bounded_work_obligation_page(records);
    while serde_json::to_vec(&page)?.len() > MAX_OBLIGATION_PAGE_BYTES
        && trim_obligation_page_once(&mut page)
    {}
    Ok(page)
}

pub(super) fn count_bounded_work_obligation_page(
    mut records: Vec<crate::storage::WorkObligationRecord>,
) -> WorkObligationPage {
    records.sort_by(|left, right| {
        match (
            left.state == WorkObligationState::Open,
            right.state == WorkObligationState::Open,
        ) {
            (true, true) => left
                .obligation
                .trigger_position
                .position
                .cmp(&right.obligation.trigger_position.position)
                .then_with(|| {
                    left.obligation
                        .obligation_id
                        .0
                        .as_bytes()
                        .cmp(right.obligation.obligation_id.0.as_bytes())
                }),
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (false, false) => right
                .resolution_position
                .as_ref()
                .map(|position| position.position)
                .cmp(
                    &left
                        .resolution_position
                        .as_ref()
                        .map(|position| position.position),
                )
                .then_with(|| {
                    left.obligation
                        .obligation_id
                        .0
                        .as_bytes()
                        .cmp(right.obligation.obligation_id.0.as_bytes())
                }),
        }
    });
    let omitted_count = records.len().saturating_sub(MAX_FOCUS_RELATIONS);
    if omitted_count > 0 {
        records.truncate(MAX_FOCUS_RELATIONS);
    }
    WorkObligationPage {
        items: records.iter().map(work_obligation_summary).collect(),
        omitted_count,
    }
}

fn trim_obligation_page_once(page: &mut WorkObligationPage) -> bool {
    if page.items.is_empty() {
        return false;
    }
    page.items.pop();
    page.omitted_count = page.omitted_count.saturating_add(1);
    true
}

fn record_byte_omission(response: &mut WorkNextView, section: WorkNextSection) {
    if let Some(existing) = response.omissions.iter_mut().find(|entry| {
        entry.section == section && entry.reason == WorkSectionOmissionReason::ByteBudget
    }) {
        existing.omitted_count += 1;
    } else {
        response.omissions.push(WorkSectionOmission {
            section,
            reason: WorkSectionOmissionReason::ByteBudget,
            omitted_count: 1,
        });
    }
}

pub(super) fn fit_work_next_response(response: &mut WorkNextView) -> Result<(), StoreError> {
    while serde_json::to_vec(response)?.len() > MAX_AGENT_WORK_RESPONSE_BYTES {
        if response.memories.take().is_some() {
            // The fixed-size signal is advisory and reannounces until it is
            // delivered. Recording a larger omission row here would increase
            // the response that this pass is trying to fit.
            continue;
        }
        if let Some(catalog) = response
            .catalog
            .as_mut()
            .filter(|catalog| catalog.items.len() > 1)
            && catalog.items.pop().is_some()
        {
            catalog.next_after = catalog.items.last().map(|item| item.work.work_id);
            record_byte_omission(response, WorkNextSection::Catalog);
            continue;
        }
        if response
            .ready
            .as_mut()
            .is_some_and(|ready| ready.len() > 1 && ready.pop().is_some())
        {
            record_byte_omission(response, WorkNextSection::Ready);
            continue;
        }
        if let Some(focus) = response.focus.as_mut()
            && trim_focus_once(focus)
        {
            record_byte_omission(response, WorkNextSection::Focus);
            continue;
        }
        break;
    }
    Ok(())
}

pub(super) fn fit_focus_response(response: &mut WorkFocusView) -> Result<(), StoreError> {
    while serde_json::to_vec(response)?.len() > MAX_AGENT_WORK_RESPONSE_BYTES
        && trim_focus_once(response)
    {
        if let Some(existing) = response.omissions.iter_mut().find(|entry| {
            entry.section == WorkNextSection::Focus
                && entry.reason == WorkSectionOmissionReason::ByteBudget
        }) {
            existing.omitted_count += 1;
        } else {
            response.omissions.push(WorkSectionOmission {
                section: WorkNextSection::Focus,
                reason: WorkSectionOmissionReason::ByteBudget,
                omitted_count: 1,
            });
        }
    }
    Ok(())
}

fn trim_focus_once(focus: &mut WorkFocusView) -> bool {
    if focus.history.items.pop().is_some() {
        focus.history.omitted = focus.history.omitted.saturating_add(1);
        return true;
    }
    if focus.restored_history.items.pop().is_some() {
        focus.restored_history.omitted = focus.restored_history.omitted.saturating_add(1);
        return true;
    }
    if let Some(blocker) = focus
        .blockers
        .iter_mut()
        .rev()
        .find(|blocker| !blocker.detail.is_empty())
    {
        blocker.detail.clear();
        return true;
    }
    if focus.memories.pop().is_some() {
        return true;
    }
    if focus.children.pop().is_some() {
        return true;
    }
    focus.prerequisites.pop().is_some()
        || focus.handoffs.pop().is_some()
        || trim_obligation_page_once(&mut focus.obligation_page)
        || trim_focus_evidence_once(focus)
}

fn trim_focus_evidence_once(focus: &mut WorkFocusView) -> bool {
    if focus.evidence_items.pop().is_some() {
        focus.evidence.pop();
        true
    } else {
        focus.evidence.pop().is_some()
    }
}

pub(super) fn ensure_agent_response_budget<T: Serialize>(
    response: &T,
    operation: &str,
) -> Result<(), StoreError> {
    let size = serde_json::to_vec(response)?.len();
    if size > MAX_AGENT_WORK_RESPONSE_BYTES {
        return Err(StoreError::InvalidWorkProjection(format!(
            "{operation} response is {size} bytes, exceeding the {MAX_AGENT_WORK_RESPONSE_BYTES}-byte agent protocol limit"
        )));
    }
    Ok(())
}
