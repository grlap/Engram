//! Ephemeral live currency for native registered commit evidence. Capture
//! immutable DB facts, read Git without a transaction, then revalidate those
//! facts when the decision consumes the measurement. Historical reads use no
//! such context and never consult the filesystem.

use super::{
    feeds::{latest_named_root_binding_on, load_typed_work_object},
    load_verification_producer_on,
    query::{load_work_item, on_one_snapshot},
};
use crate::storage::{SqliteStore, StoreError};
use crate::{
    ObjectId,
    domain::{
        ExecutionOutcome, NamedRootBindingKind, VerificationEvidence, VerificationSourceIdentity,
        WorkId, WorkRunId,
    },
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;

struct Candidate {
    id: ObjectId,
    evidence: VerificationEvidence,
    root: Option<ObjectId>,
}

impl Candidate {
    fn is_commit(&self) -> bool {
        self.evidence
            .source_basis
            .legacy_revision()
            .is_some_and(|revision| revision.starts_with("git-commit:"))
    }

    fn is_live(&self) -> bool {
        self.is_commit() || self.evidence.source_basis.source_id().is_some()
    }
}

struct SharedMeasurement {
    source_id: crate::source::SourceId,
    measured_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Default)]
pub(in crate::storage) struct LiveCurrency {
    work: Option<(WorkId, WorkRunId, i64)>,
    candidates: Vec<Candidate>,
    roots: BTreeMap<String, Result<crate::committed_input::CommittedInput, String>>,
    shared_roots: BTreeMap<String, Result<SharedMeasurement, String>>,
}

impl LiveCurrency {
    pub(in crate::storage) fn scope_on(
        connection: &Connection,
        id: &ObjectId,
        project: &crate::ProjectId,
    ) -> Result<Option<(WorkId, WorkRunId)>, StoreError> {
        let kind: Option<String> = connection
            .query_row(
                "SELECT object_kind FROM objects WHERE object_id=?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if kind.as_deref() != Some("verification_evidence") {
            return Ok(None);
        }
        let evidence: VerificationEvidence =
            load_typed_work_object(connection, id, "verification_evidence")?;
        Ok((evidence.project_id == *project)
            .then_some((evidence.binding.work_id, evidence.binding.run_id)))
    }
    pub(in crate::storage) fn measure(
        store: &SqliteStore,
        work: WorkId,
        run: WorkRunId,
    ) -> Result<Self, StoreError> {
        // This assertion prevents an accidental new caller from moving the
        // filesystem operation into a long-running SQLite transaction.
        if !store.connection.is_autocommit() {
            return Err(StoreError::InvalidWorkProjection(
                "live Git currency must be measured outside a SQLite transaction".into(),
            ));
        }
        let mut result = on_one_snapshot(&store.connection, |connection| {
            let item = load_work_item(connection, work)?;
            let mut statement = connection.prepare(
                "SELECT entry.object_id FROM work_feed_entries entry JOIN objects object ON object.object_id=entry.object_id WHERE entry.feed_kind='run_execution' AND entry.feed_id=?1 AND object.object_kind='verification_evidence' ORDER BY entry.position")?;
            let ids = statement
                .query_map(params![run.0.to_string()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut candidates = Vec::new();
            for id in ids {
                let id =
                    ObjectId::from_stored(id.clone()).ok_or(StoreError::InvalidStoredKey(id))?;
                let evidence: VerificationEvidence =
                    load_typed_work_object(connection, &id, "verification_evidence")?;
                let producer = load_verification_producer_on(connection, &evidence)?;
                if producer.registered_git_basis(&evidence).is_none()
                    && producer.registered_source_id_basis(&evidence).is_none()
                {
                    continue;
                }
                let root = latest_named_root_binding_on(
                    connection,
                    run,
                    evidence.binding.claim_id,
                    i64::MAX,
                )?
                .filter(|(_, _, event)| {
                    event.kind == NamedRootBindingKind::Bound
                        && event.workspace_id == evidence.source_basis.workspace_id
                        && Some(event.generation) == evidence.source_basis.source_root_generation
                })
                .map(|(_, id, _)| id);
                candidates.push(Candidate { id, evidence, root });
            }
            Ok(Self {
                work: Some((work, run, item.revision)),
                candidates,
                roots: BTreeMap::new(),
                shared_roots: BTreeMap::new(),
            })
        })?;
        for candidate in result
            .candidates
            .iter()
            .filter(|candidate| candidate.is_commit() && candidate.root.is_some())
        {
            let workspace = &candidate.evidence.source_basis.workspace_id;
            result.roots.entry(workspace.clone()).or_insert_with(|| {
                crate::committed_input::read(std::path::Path::new(workspace))
                    .map_err(|error| format!("{error:#}"))
            });
        }
        for candidate in result.candidates.iter().filter(|candidate| {
            candidate.evidence.source_basis.source_id().is_some() && candidate.root.is_some()
        }) {
            let workspace = &candidate.evidence.source_basis.workspace_id;
            result
                .shared_roots
                .entry(workspace.clone())
                .or_insert_with(|| {
                    let observation = crate::source::inspect(
                        std::path::Path::new(workspace),
                        crate::source::MAX_BUDGET,
                    );
                    observation
                        .source_id
                        .map(|source_id| SharedMeasurement {
                            source_id,
                            measured_at: chrono::Utc::now(),
                        })
                        .ok_or_else(|| {
                            observation.refusal.map_or_else(
                                || "source identity unavailable".into(),
                                |refusal| refusal.message,
                            )
                        })
                });
        }
        Ok(result)
    }

    fn is_current(&self, candidate: &Candidate) -> bool {
        let basis = &candidate.evidence.source_basis;
        if let Some(source_id) = basis.source_id() {
            self.shared_roots
                .get(&basis.workspace_id)
                .and_then(|result| result.as_ref().ok())
                .is_some_and(|measured| &measured.source_id == source_id)
        } else {
            self.roots
                .get(&basis.workspace_id)
                .and_then(|result| result.as_ref().ok())
                .is_some_and(|measured| {
                    measured.clean
                        && basis.legacy_revision()
                            == Some(format!("git-commit:{}", measured.commit).as_str())
                })
        }
    }

    /// The sole live commit currency decision. `false` means the record
    /// belongs to another evidence lane, never that an unavailable root passed.
    pub(super) fn check_on(
        &self,
        connection: &Connection,
        id: &ObjectId,
    ) -> Result<bool, StoreError> {
        let evidence: VerificationEvidence =
            load_typed_work_object(connection, id, "verification_evidence")?;
        let producer = load_verification_producer_on(connection, &evidence)?;
        let shared = producer.registered_source_id_basis(&evidence);
        if shared.is_none()
            && producer
                .registered_git_basis(&evidence)
                .is_none_or(|basis| !basis.source_revision.starts_with("git-commit:"))
        {
            return Ok(false);
        }
        let work = evidence.binding.work_id;
        let refused = |reason: String| StoreError::WorkCompletionRefused { work, reason };
        let Some((sampled_work, sampled_run, revision)) = self.work else {
            return Ok(false);
        };
        let item = load_work_item(connection, work)?;
        if work != sampled_work
            || evidence.binding.run_id != sampled_run
            || item.active_run_id != Some(sampled_run)
            || item.revision != revision
        {
            return Err(refused(
                "the work/run binding changed during the Git read; read the current item and retry"
                    .into(),
            ));
        }
        let candidate = self.candidates.iter().find(|candidate| &candidate.id == id && candidate.evidence == evidence)
            .ok_or_else(|| refused("the selected registered check changed during the Git read; retry with current evidence".into()))?;
        let root = latest_named_root_binding_on(
            connection,
            sampled_run,
            evidence.binding.claim_id,
            i64::MAX,
        )?;
        if candidate.root.is_none()
            || root.as_ref().map(|(_, id, _)| id) != candidate.root.as_ref()
            || root
                .as_ref()
                .is_none_or(|(_, _, event)| event.kind != NamedRootBindingKind::Bound)
        {
            return Err(refused("the registered check's named root is unavailable or has been rebound; name the intended root and run a new check".into()));
        }
        let input_matches = shared.map_or_else(
            || {
                evidence
                    .source_basis
                    .legacy_revision()
                    .is_some_and(|revision| {
                        producer.matches_registered_git_input(&evidence, revision) == Some(true)
                    })
            },
            |basis| basis == &evidence.source_basis,
        );
        if !input_matches
            || producer.outcome != ExecutionOutcome::Succeeded
            || producer.run_outcome != Some(ExecutionOutcome::Succeeded)
        {
            return Err(refused(
                "the registered run did not pass on an eligible input; run a passing check".into(),
            ));
        }
        refuse_newer_failed_on(connection, id, &evidence, i64::MAX)?;
        if let Some(expected) = evidence.source_basis.source_id() {
            let measured = self
                .shared_roots
                .get(&evidence.source_basis.workspace_id)
                .ok_or_else(|| {
                    refused("the registered source root was not measured; retry".into())
                })?
                .as_ref()
                .map_err(|reason| {
                    refused(format!(
                        "registered source root is unavailable: {reason}; restore access and retry"
                    ))
                })?;
            if expected != &measured.source_id {
                return Err(refused("the named root's full source_id changed since the registered check; run a check on the current source state".into()));
            }
            return Ok(true);
        }
        let measured = self
            .roots
            .get(&evidence.source_basis.workspace_id)
            .ok_or_else(|| refused("the registered root was not measured; retry".into()))?
            .as_ref()
            .map_err(|reason| {
                refused(format!(
                    "registered root {} is unavailable: {reason}; restore access and retry",
                    evidence.source_basis.workspace_id
                ))
            })?;
        if !measured.clean {
            return Err(refused(format!(
                "registered root {} is dirty; commit or revert the changes and run a check on the intended commit",
                evidence.source_basis.workspace_id
            )));
        }
        if evidence.source_basis.legacy_revision()
            != Some(format!("git-commit:{}", measured.commit).as_str())
        {
            return Err(refused(format!(
                "registered check ran on {}, but the named root is at git-commit:{}; run a check on the current commit",
                evidence.source_basis.legacy_revision().unwrap_or_default(),
                measured.commit
            )));
        }
        Ok(true)
    }

    pub(super) fn active(&self) -> bool {
        self.work.is_some()
    }

    pub(super) fn require_declared_on(
        &self,
        connection: &Connection,
        identity: Option<&VerificationSourceIdentity>,
        cut: i64,
    ) -> Result<bool, StoreError> {
        if !self.candidates.iter().any(Candidate::is_live) {
            if let Some((work, run, _)) = self.work {
                let entered = has_committed_run_on(connection, run, cut)?;
                if entered {
                    return Err(StoreError::WorkCompletionRefused {
                        work,
                        reason: "the registered committed-input run supplied no executed check; run a passing check on the intended commit before evaluating".into(),
                    });
                }
            }
            return Ok(false);
        }
        let work = self.work.expect("measured candidate scope").0;
        let Some(identity) = identity.filter(|identity| {
            matches!(identity, VerificationSourceIdentity::SourceId { .. })
                || identity
                    .legacy_revision()
                    .is_some_and(|revision| revision.starts_with("git-commit:"))
        }) else {
            return Err(StoreError::WorkCompletionRefused {
                work,
                reason: if self
                    .candidates
                    .iter()
                    .any(|candidate| candidate.evidence.source_basis.source_id().is_some())
                {
                    "this run has registered source_id evidence; declare the intended complete source with --source-id-file before evaluating".into()
                } else {
                    "this run has registered committed-input evidence; declare the intended git-commit input with --source-fingerprint before evaluating".into()
                },
            });
        };
        if !self.matching_on(connection, identity, cut)? {
            return Err(StoreError::WorkCompletionRefused {
                work,
                reason: "no registered check at the evaluated cut tested the declared commit; run a passing check on the intended current commit".into(),
            });
        }
        Ok(true)
    }

    pub(super) fn select_on(
        &self,
        connection: &Connection,
        requirement: &crate::domain::VerificationRequirement,
        cut: i64,
    ) -> Result<Option<crate::domain::RegisteredInputDecision>, StoreError> {
        let relevant = |candidate: &&Candidate| {
            candidate.evidence.check_kind == requirement.check_kind
                && requirement
                    .check_fingerprint
                    .as_ref()
                    .is_none_or(|required| required == &candidate.evidence.check_fingerprint)
        };
        // A later registered staged check keeps the existing staged lane.
        // An older commit check must not impose its live-root refusal on it.
        for candidate in self.candidates.iter().rev().filter(relevant) {
            let position = super::feeds::run_feed_position_for_object_on(
                connection,
                candidate.evidence.binding.run_id,
                &candidate.id,
            )?
            .position;
            if position <= cut {
                if !candidate.is_live() {
                    return Ok(None);
                }
                break;
            }
        }
        // Select the newest relevant result at the measured commit, not a
        // result at a different commit the worktree has since left behind.
        for candidate in self
            .candidates
            .iter()
            .rev()
            .filter(|candidate| candidate.is_live())
            .filter(relevant)
            .filter(|candidate| self.is_current(candidate))
        {
            let position = super::feeds::run_feed_position_for_object_on(
                connection,
                candidate.evidence.binding.run_id,
                &candidate.id,
            )?
            .position;
            if position <= cut {
                return self.decision_on(connection, &candidate.id);
            }
        }
        // If there is evidence but none at the current clean input, name the
        // real dirty/unavailable/wrong-commit refusal rather than going legacy.
        for candidate in self
            .candidates
            .iter()
            .rev()
            .filter(|candidate| candidate.is_live())
            .filter(relevant)
        {
            let position = super::feeds::run_feed_position_for_object_on(
                connection,
                candidate.evidence.binding.run_id,
                &candidate.id,
            )?
            .position;
            if position <= cut {
                return self.decision_on(connection, &candidate.id);
            }
        }
        Ok(None)
    }

    pub(super) fn decision_on(
        &self,
        connection: &Connection,
        id: &ObjectId,
    ) -> Result<Option<crate::domain::RegisteredInputDecision>, StoreError> {
        if !self.check_on(connection, id)? {
            return Ok(None);
        }
        let candidate = self
            .candidates
            .iter()
            .find(|candidate| &candidate.id == id)
            .expect("checked candidate");
        Ok(Some(crate::domain::RegisteredInputDecision {
            verification: id.clone(),
            named_root_binding: candidate.root.clone().expect("checked root"),
            source: if let Some(source_id) = candidate.evidence.source_basis.source_id() {
                crate::domain::RegisteredDecisionSource::SourceId {
                    source_id: source_id.clone(),
                }
            } else {
                crate::domain::RegisteredDecisionSource::Legacy {
                    source_revision: candidate
                        .evidence
                        .source_basis
                        .legacy_revision()
                        .expect("checked legacy input")
                        .to_owned(),
                    clean: true,
                }
            },
            measured_at: if candidate.evidence.source_basis.source_id().is_some() {
                self.shared_roots
                    .get(&candidate.evidence.source_basis.workspace_id)
                    .and_then(|result| result.as_ref().ok())
                    .expect("checked measurement")
                    .measured_at
            } else {
                self.roots
                    .get(&candidate.evidence.source_basis.workspace_id)
                    .and_then(|result| result.as_ref().ok())
                    .expect("checked measurement")
                    .measured_at
            },
        }))
    }

    /// Optional accounting of a source-change obligation. A stale or failed
    /// unrelated check must not turn a recording-only obligation into a gate.
    /// Required criterion selection continues to use `select_on`.
    pub(super) fn select_current_on(
        &self,
        connection: &Connection,
        requirement: &crate::domain::VerificationRequirement,
        cut: i64,
    ) -> Result<Option<crate::domain::RegisteredInputDecision>, StoreError> {
        for candidate in self.candidates.iter().rev().filter(|candidate| {
            candidate.is_live()
                && candidate.root.is_some()
                && candidate.evidence.check_kind == requirement.check_kind
                && requirement
                    .check_fingerprint
                    .as_ref()
                    .is_none_or(|required| required == &candidate.evidence.check_fingerprint)
                && self.is_current(candidate)
        }) {
            let position = super::feeds::run_feed_position_for_object_on(
                connection,
                candidate.evidence.binding.run_id,
                &candidate.id,
            )?
            .position;
            if position <= cut && passing_candidate_on(connection, candidate)? {
                return self.decision_on(connection, &candidate.id);
            }
        }
        Ok(None)
    }

    pub(super) fn matching_decision_on(
        &self,
        connection: &Connection,
        identity: &VerificationSourceIdentity,
        through: i64,
    ) -> Result<Option<crate::domain::RegisteredInputDecision>, StoreError> {
        for candidate in self.candidates.iter().rev().filter(|candidate| {
            candidate
                .evidence
                .source_basis
                .identity
                .same_source(identity)
        }) {
            let position = super::feeds::run_feed_position_for_object_on(
                connection,
                candidate.evidence.binding.run_id,
                &candidate.id,
            )?;
            if position.position <= through {
                if !passing_candidate_on(connection, candidate)? {
                    continue;
                }
                return self.decision_on(connection, &candidate.id);
            }
        }
        Ok(None)
    }

    pub(super) fn matching_on(
        &self,
        connection: &Connection,
        identity: &VerificationSourceIdentity,
        through: i64,
    ) -> Result<bool, StoreError> {
        Ok(self
            .matching_decision_on(connection, identity, through)?
            .is_some())
    }
}

pub(super) fn has_committed_run_on(
    connection: &Connection,
    run: WorkRunId,
    cut: i64,
) -> Result<bool, StoreError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_feed_entries entry JOIN objects object ON object.object_id=entry.object_id WHERE entry.feed_kind='run_execution' AND entry.feed_id=?1 AND entry.position<=?2 AND object.object_kind='registered_host_run' AND json_extract(object.canonical_json, '$.request.input.kind') IN ('commit','source_id'))",
        params![run.0.to_string(), cut], |row| row.get(0))?)
}

/// Validate recorded provenance only. No filesystem read, discovery of a
/// replacement check, or source-report currency rule belongs in reconstruction.
pub(super) fn validate_decision_on(
    connection: &Connection,
    decision: &crate::domain::RegisteredInputDecision,
    work: WorkId,
    run: WorkRunId,
    cut: i64,
    requirement: Option<&crate::domain::VerificationRequirement>,
) -> Result<VerificationEvidence, StoreError> {
    let invalid = || {
        StoreError::InvalidWorkProjection("registered input decision does not bind a clean, passed check and its exact named root at the recorded cut".into())
    };
    let evidence: VerificationEvidence =
        load_typed_work_object(connection, &decision.verification, "verification_evidence")?;
    let producer = load_verification_producer_on(connection, &evidence)?;
    let root: crate::domain::NamedRootBindingEvent = load_typed_work_object(
        connection,
        &decision.named_root_binding,
        "named_root_binding",
    )?;
    let evidence_position =
        super::feeds::run_feed_position_for_object_on(connection, run, &decision.verification)?
            .position;
    let producer_position = super::feeds::run_feed_position_for_object_on(
        connection,
        run,
        &evidence.producer_observation,
    )?
    .position;
    let root_position = super::feeds::run_feed_position_for_object_on(
        connection,
        run,
        &decision.named_root_binding,
    )?
    .position;
    let identity_matches = match &decision.source {
        crate::domain::RegisteredDecisionSource::Legacy {
            source_revision,
            clean,
        } => {
            *clean
                && source_revision.starts_with("git-commit:")
                && producer.matches_registered_git_input(&evidence, source_revision) == Some(true)
        }
        crate::domain::RegisteredDecisionSource::SourceId { source_id } => {
            producer
                .registered_source_id_basis(&evidence)
                .is_some_and(|basis| basis == &evidence.source_basis)
                && evidence.source_basis.source_id() == Some(source_id)
        }
    };
    if !identity_matches
        || evidence.binding.work_id != work
        || evidence.binding.run_id != run
        || evidence_position > cut
        || producer_position > cut
        || root_position > cut
        || root.project_id != evidence.project_id
        || root.work_id != work
        || root.run_id != run
        || root.root_execution_id != evidence.binding.root_execution_id
        || root.claim_id != evidence.binding.claim_id
        || root.kind != NamedRootBindingKind::Bound
        || latest_named_root_binding_on(connection, run, root.claim_id, cut)?
            .as_ref()
            .map(|(_, id, _)| id)
            != Some(&decision.named_root_binding)
    {
        return Err(invalid());
    }
    let selected = crate::domain::VerificationRequirement {
        check_kind: evidence.check_kind,
        check_fingerprint: Some(evidence.check_fingerprint.clone()),
    };
    crate::control::match_verification_evidence(&crate::control::VerificationEvidenceMatchInput {
        judged_basis: evidence.source_basis.legacy_basis().as_ref(),
        candidate_kind: crate::domain::WorkEvidenceKind::Verification,
        evidence: Some(&evidence),
        producer: Some(&producer),
        latest_mutation: None,
        measured_sighting: None,
        named_root: Some(crate::control::NamedRootEvidenceMatch {
            workspace_id: &root.workspace_id,
            generation: root.generation,
            binding_position: root_position,
            latest_sighting: None,
            unknown_change_position: None,
        }),
        evidence_position,
        producer_position: Some(producer_position),
        requirement: requirement.unwrap_or(&selected),
    })
    .map_err(|_| invalid())?;
    refuse_newer_failed_on(connection, &decision.verification, &evidence, cut)?;
    Ok(evidence)
}

fn refuse_newer_failed_on(
    connection: &Connection,
    selected: &ObjectId,
    evidence: &VerificationEvidence,
    cut: i64,
) -> Result<(), StoreError> {
    if let Some(id) = newer_failed_on(connection, selected, evidence, cut)? {
        return Err(StoreError::WorkCompletionRefused {
            work: evidence.binding.work_id,
            reason: format!(
                "a newer registered check {id} on this input did not pass; run a passing check after it"
            ),
        });
    }
    Ok(())
}

fn passing_candidate_on(
    connection: &Connection,
    candidate: &Candidate,
) -> Result<bool, StoreError> {
    let producer = load_verification_producer_on(connection, &candidate.evidence)?;
    Ok(
        candidate.evidence.result == crate::domain::VerificationResult::Passed
            && producer.outcome == ExecutionOutcome::Succeeded
            && producer.run_outcome == Some(ExecutionOutcome::Succeeded)
            && newer_failed_on(connection, &candidate.id, &candidate.evidence, i64::MAX)?.is_none(),
    )
}

fn newer_failed_on(
    connection: &Connection,
    selected: &ObjectId,
    evidence: &VerificationEvidence,
    cut: i64,
) -> Result<Option<ObjectId>, StoreError> {
    let at = super::feeds::run_feed_position_for_object_on(
        connection,
        evidence.binding.run_id,
        selected,
    )?
    .position;
    let mut statement = connection.prepare("SELECT entry.object_id FROM work_feed_entries entry JOIN objects object ON object.object_id=entry.object_id WHERE entry.feed_kind='run_execution' AND entry.feed_id=?1 AND entry.position>?2 AND entry.position<=?3 AND object.object_kind='verification_evidence' ORDER BY entry.position DESC")?;
    let ids = statement
        .query_map(
            params![evidence.binding.run_id.0.to_string(), at, cut],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        let id = ObjectId::from_stored(id.clone()).ok_or(StoreError::InvalidStoredKey(id))?;
        let newer: VerificationEvidence =
            load_typed_work_object(connection, &id, "verification_evidence")?;
        if newer.check_kind != evidence.check_kind
            || newer.check_fingerprint != evidence.check_fingerprint
            || newer.source_basis != evidence.source_basis
        {
            continue;
        }
        let producer = load_verification_producer_on(connection, &newer)?;
        if producer.registered_git_basis(&newer).is_some()
            || producer.registered_source_id_basis(&newer).is_some()
        {
            if newer.result != crate::domain::VerificationResult::Passed
                || producer.run_outcome != Some(ExecutionOutcome::Succeeded)
            {
                return Ok(Some(id));
            }
            break;
        }
    }
    Ok(None)
}
