use std::collections::BTreeSet;

use serde::Deserialize;

use super::{
    DELTA_PAYLOAD_FORMAT, DeltaBasis, DeltaCandidatePayload, DeltaCommitSelection,
    DeltaImportReceipt, DeltaIntent, DeltaPlanning, ManagedCustodyCheckpoint, MembershipSnapshot,
    OriginQualifiedCut, OutputMembership,
};
use crate::task_sync::{
    FORMAT, ProjectDocument, TaskSyncRefusal,
    ledger::{Binding, Direction, absolute, attribution, cut, fingerprint, oid, require, text},
    progress::ExportCut,
};
use crate::{
    CanonicalObject, FeedId, FeedPosition, ObjectId, ProjectId, WorkId, storage::StoreError,
};

pub(super) const MAX_CHANGED_MEMBERS: usize = 200_000;
const MAX_CHECKPOINT_ARTIFACTS: usize = 100_000;

// Keep the existing domain type while giving the new wire contract a strict
// project-only envelope. Historical FeedPosition deserialization is unchanged.
pub(super) fn project_position<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<FeedPosition, D::Error> {
    #[derive(Deserialize)]
    #[serde(
        tag = "kind",
        content = "id",
        rename_all = "snake_case",
        deny_unknown_fields
    )]
    enum ProjectFeed {
        Project(ProjectId),
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Position {
        feed: ProjectFeed,
        position: i64,
    }
    let position = Position::deserialize(deserializer)?;
    let ProjectFeed::Project(project) = position.feed;
    Ok(FeedPosition {
        feed: FeedId::Project(project),
        position: position.position,
    })
}

impl OriginQualifiedCut {
    /// Check declared binding/origin/feed/restore consistency only. The storage
    /// writer must still prove the actual enrollment, anchor and observation.
    ///
    /// # Errors
    /// Refuses a missing origin or a mismatched/malformed bound observation.
    pub fn validate(&self, binding_id: &ObjectId, binding: &Binding) -> Result<(), StoreError> {
        binding.validate()?;
        if binding.store.origin.as_ref() != Some(&self.origin) {
            return Err(TaskSyncRefusal::IncrementalBasisUnavailable
                .invalid("strict delta requires the binding's explicitly enrolled origin"));
        }
        require(
            &self.binding == binding_id
                && self.position.feed == FeedId::Project(binding.project.clone())
                && self.restore_occurrence == binding.store.restore_occurrence
                && self.restore_occurrence.is_none_or(|id| !id.is_nil()),
            "strict delta binding, project feed or restore occurrence differs",
        )?;
        cut(&ExportCut {
            position: self.position.position,
            anchor: self.anchor.clone(),
        })
    }
}

pub(super) fn membership(value: &OutputMembership) -> Result<(), StoreError> {
    value
        .profile
        .validate()
        .map_err(|_| crate::task_sync::invalid("invalid delta export profile"))
}

pub(super) fn snapshot(value: &MembershipSnapshot) -> Result<(), StoreError> {
    value.commit.validate()?;
    membership(&value.membership)
}

fn git_format(intent: &DeltaIntent, width: usize) -> Result<(), StoreError> {
    require(
        intent
            .expected_head
            .as_ref()
            .is_none_or(|id| id.len() == width)
            && intent
                .common_commit
                .as_ref()
                .is_none_or(|commit| commit.oid.len() == width),
        "mixed delta Git object formats",
    )
}

impl DeltaIntent {
    /// Validate the exact request's shape, with source and destination bases
    /// kept separate. This neither reads retained bytes nor admits a graph.
    ///
    /// # Errors
    /// Refuses mismatched direction, profile, operation or qualified cut and
    /// malformed comparison/provenance fields.
    pub fn validate(&self, binding_id: &ObjectId, binding: &Binding) -> Result<(), StoreError> {
        self.validate_shape(binding_id, binding)?;
        if let DeltaBasis::Incoming { source_after, .. } = &self.basis {
            require(
                source_after.membership.inventory_root == self.candidate.output.inventory_root,
                "incoming delta source/destination membership differs",
            )?;
        }
        Ok(())
    }

    /// A new successor keeps the original source while resolving local planning
    /// against a fresh destination. Ordinary delta validation remains unchanged.
    pub(crate) fn validate_successor(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
    ) -> Result<(), StoreError> {
        require(
            self.direction == Direction::Incoming,
            "planning successor must be incoming",
        )?;
        self.validate_shape(binding_id, binding)
    }

    fn validate_shape(&self, binding_id: &ObjectId, binding: &Binding) -> Result<(), StoreError> {
        self.local_cut.validate(binding_id, binding)?;
        require(!self.operation.is_nil(), "missing delta operation")?;
        text(&self.key, 4096)?;
        attribution(&self.actor)?;
        fingerprint(&self.producer)?;
        fingerprint(&self.candidate.content_fingerprint)?;
        absolute(&self.candidate.directory)?;
        membership(&self.candidate.output)?;
        require(
            self.candidate.operation == self.operation
                && self.candidate.output.profile == binding.profile
                && self.candidate.changed.len() <= MAX_CHANGED_MEMBERS,
            "delta candidate operation, profile or changed-member bound differs",
        )?;
        if let Some(head) = &self.expected_head {
            oid(head)?;
        }
        if let Some(commit) = &self.common_commit {
            commit.validate()?;
        }
        if let Some(head) = &self.expected_head {
            git_format(self, head.len())?;
        }
        match (&self.direction, &self.basis) {
            (
                Direction::Incoming,
                DeltaBasis::Incoming {
                    source_before,
                    source_after,
                    source_changed,
                    local_before,
                },
            ) => {
                snapshot(&source_before.snapshot)?;
                snapshot(source_after)?;
                git_format(self, source_after.commit.oid.len())?;
                require(
                    source_before.snapshot.commit.oid.len() == source_after.commit.oid.len(),
                    "mixed delta Git object formats",
                )?;
                membership(&local_before.membership)?;
                require(
                    source_changed.len() <= MAX_CHANGED_MEMBERS
                        && source_before.snapshot.membership.profile.include_notes
                            == source_after.membership.profile.include_notes
                        && source_after.membership.profile.include_notes
                            == binding.profile.include_notes
                        && local_before.membership.profile.include_notes
                            == binding.profile.include_notes,
                    "incoming delta source/destination membership or note profile differs",
                )
            }
            (
                Direction::Outgoing,
                DeltaBasis::Outgoing {
                    export_before,
                    confirmed_before,
                },
            ) => {
                export_before.cut.validate(binding_id, binding)?;
                membership(&export_before.membership)?;
                require(
                    export_before.cut.position.position <= self.local_cut.position.position
                        && export_before.membership.profile.include_notes
                            == binding.profile.include_notes,
                    "outgoing delta prior preparation cut or note profile differs",
                )?;
                if let Some(confirmed) = confirmed_before {
                    snapshot(&confirmed.snapshot)?;
                    git_format(self, confirmed.snapshot.commit.oid.len())?;
                    require(
                        confirmed.snapshot.membership.profile.include_notes
                            == binding.profile.include_notes,
                        "outgoing delta confirmed note profile differs",
                    )?;
                }
                Ok(())
            }
            _ => Err(crate::task_sync::invalid(
                "delta direction and basis disagree",
            )),
        }
    }

    fn payload_keys(&self) -> BTreeSet<WorkId> {
        let mut keys = self.candidate.changed.clone();
        if let DeltaBasis::Incoming { source_changed, .. } = &self.basis {
            keys.extend(source_changed);
        }
        keys
    }
}

impl DeltaCandidatePayload {
    pub(crate) fn validate_successor(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        intent: &DeltaIntent,
    ) -> Result<(), StoreError> {
        intent.validate_successor(binding_id, binding)?;
        self.validate_declared(intent)
    }
    /// Check the selected payload against its exact descriptor, without
    /// resolving graph edges, source members, origin custody or current state.
    ///
    /// # Errors
    /// Refuses mismatched fields/keys/bytes or invalid selected documents.
    pub fn validate(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        intent: &DeltaIntent,
    ) -> Result<(), StoreError> {
        intent.validate(binding_id, binding)?;
        self.validate_declared(intent)
    }

    /// Canonical document owners share the exact payload comparison without
    /// constructing a binding or claiming storage admission.
    pub(crate) fn validate_declared(&self, intent: &DeltaIntent) -> Result<(), StoreError> {
        #[cfg(test)]
        super::validation_probe::count();
        self.validate_historical_declared(intent)?;
        crate::task_sync::validate_selected_documents(
            &ProjectDocument {
                format: FORMAT.into(),
                project_id: self.project_id.clone(),
                notes_included: self.output.profile.include_notes,
            },
            self.documents.values().filter_map(Option::as_ref),
        )
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
        require(
            CanonicalObject::freeze(self)?.key() == &intent.candidate.content_fingerprint,
            "retained delta payload differs from its content fingerprint",
        )
    }

    /// Historical replay preserves the admission decision and fingerprint of
    /// the original writer. Only declared structure and links are checked here.
    pub(crate) fn validate_historical(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        intent: &DeltaIntent,
    ) -> Result<(), StoreError> {
        intent.validate(binding_id, binding)?;
        self.validate_historical_declared(intent)
    }

    pub(crate) fn validate_historical_successor(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        intent: &DeltaIntent,
    ) -> Result<(), StoreError> {
        intent.validate_successor(binding_id, binding)?;
        self.validate_historical_declared(intent)
    }

    pub(crate) fn validate_historical_declared(
        &self,
        intent: &DeltaIntent,
    ) -> Result<(), StoreError> {
        let FeedId::Project(project) = &intent.local_cut.position.feed else {
            return Err(crate::task_sync::invalid(
                "delta payload requires a project feed",
            ));
        };
        require(
            self.format == DELTA_PAYLOAD_FORMAT
                && &self.project_id == project
                && self.operation == intent.operation
                && self.output == intent.candidate.output
                && self.documents.len() <= MAX_CHANGED_MEMBERS
                && self.documents.keys().copied().collect::<BTreeSet<_>>() == intent.payload_keys(),
            "delta payload envelope or selected keys differ",
        )?;
        for (id, value) in &self.documents {
            require(
                value.as_ref().is_none_or(|task| task.id == *id),
                "delta task key differs",
            )?;
        }
        Ok(())
    }
}

impl DeltaCommitSelection {
    /// Check a declared outgoing commit against its original preparation.
    /// Retention and storage must still establish the actual Git object/ref.
    ///
    /// # Errors
    /// Refuses a different preparation, direction, Git format or operation ref.
    pub fn validate(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        intent: &DeltaIntent,
        prepared: &ObjectId,
    ) -> Result<(), StoreError> {
        intent.validate(binding_id, binding)?;
        self.commit.validate()?;
        git_format(intent, self.commit.oid.len())?;
        require(
            intent.direction == Direction::Outgoing
                && &self.prepared == prepared
                && self.commit.retention_ref
                    == format!("refs/engram/task-sync/exchange/{}/commit", intent.operation),
            "delta commit selection differs from its outgoing preparation",
        )
    }
}

impl DeltaPlanning {
    /// Validate the source-relative key set only. Storage validates every
    /// referenced canonical member and its independent local planning record.
    ///
    /// # Errors
    /// Refuses an outgoing request or different project/preparation/source keys.
    pub fn validate(
        &self,
        project: &ProjectId,
        prepared: &ObjectId,
        intent: &DeltaIntent,
    ) -> Result<(), StoreError> {
        let DeltaBasis::Incoming { source_changed, .. } = &intent.basis else {
            return Err(crate::task_sync::invalid(
                "outgoing delta has no import planning",
            ));
        };
        require(
            &self.project_id == project
                && &self.prepared == prepared
                && self.source_changes.keys().copied().collect::<BTreeSet<_>>() == *source_changed,
            "source-relative delta planning differs from its preparation",
        )
    }
}

impl DeltaImportReceipt {
    /// Validate bounded outcome shape and the declared local feed movement.
    /// Canonical planning, graph, run-fence and source-member admission remain
    /// the atomic storage writer's responsibility.
    ///
    /// # Errors
    /// Refuses outgoing requests, changed cut observations, backwards cuts,
    /// duplicate members or malformed local revision observations.
    pub fn validate(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        intent: &DeltaIntent,
    ) -> Result<(), StoreError> {
        intent.validate(binding_id, binding)?;
        self.validate_shape(binding_id, binding, intent)
    }

    pub(crate) fn validate_successor(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        intent: &DeltaIntent,
    ) -> Result<(), StoreError> {
        intent.validate_successor(binding_id, binding)?;
        self.validate_shape(binding_id, binding, intent)
    }

    fn validate_shape(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        intent: &DeltaIntent,
    ) -> Result<(), StoreError> {
        self.after_cut.validate(binding_id, binding)?;
        require(
            intent.direction == Direction::Incoming
                && self.after_cut.position.position >= intent.local_cut.position.position
                && (self.after_cut.position.position != intent.local_cut.position.position
                    || self.after_cut.anchor == intent.local_cut.anchor),
            "delta import receipt direction or local cut differs",
        )?;
        require(
            self.before.len() <= MAX_CHANGED_MEMBERS
                && self.tasks.len() <= MAX_CHANGED_MEMBERS
                && self.holds.len() <= MAX_CHANGED_MEMBERS
                && self.terminal_evidence.len() <= MAX_CHANGED_MEMBERS,
            "delta import receipt exceeds supported bound",
        )?;
        let mut work = BTreeSet::new();
        let mut runs = BTreeSet::new();
        for before in &self.before {
            require(
                work.insert(before.work_id)
                    && before.revision > 0
                    && before.runs.len() <= MAX_CHANGED_MEMBERS,
                "delta import receipt has repeated work or invalid revision",
            )?;
            for run in &before.runs {
                require(
                    runs.len() < MAX_CHANGED_MEMBERS
                        && runs.insert(run.run_id.0)
                        && run.generation > 0
                        && run.revision > 0
                        && run.claim_fence >= 0,
                    "delta import receipt has repeated run or invalid run basis",
                )?;
            }
            require(
                before
                    .active_run_id
                    .is_none_or(|active| before.runs.iter().any(|run| run.run_id == active)),
                "delta import receipt active run is absent from its basis",
            )?;
        }
        let mut tasks = BTreeSet::new();
        let mut records = BTreeSet::new();
        for task in &self.tasks {
            require(
                tasks.insert(task.task) && task.local_record_ids.len() <= MAX_CHANGED_MEMBERS,
                "delta import receipt has repeated task or excessive local records",
            )?;
            for id in &task.local_record_ids {
                require(
                    records.len() < MAX_CHANGED_MEMBERS && records.insert(id),
                    "delta import receipt has repeated or excessive local records",
                )?;
            }
        }
        require(
            self.holds.iter().collect::<BTreeSet<_>>().len() == self.holds.len()
                && self.terminal_evidence.iter().collect::<BTreeSet<_>>().len()
                    == self.terminal_evidence.len(),
            "delta import receipt has repeated hold or evidence",
        )
    }
}

impl ManagedCustodyCheckpoint {
    /// Validate declared cold-checkpoint metadata. This does not audit files,
    /// settle uncertainty, publish acknowledgement or create an exchange event.
    ///
    /// # Errors
    /// Refuses a malformed cut, reused artifact key or invalid comparison value.
    pub fn validate(&self, binding_id: &ObjectId, binding: &Binding) -> Result<(), StoreError> {
        self.local_cut.validate(binding_id, binding)?;
        require(
            !self.operation.is_nil(),
            "missing custody checkpoint operation",
        )?;
        text(&self.key, 4096)?;
        fingerprint(&self.request_fingerprint)?;
        if let Some(seed) = &self.seed {
            seed.validate(binding)?;
        }
        require(
            self.artifacts.len() <= MAX_CHECKPOINT_ARTIFACTS,
            "custody checkpoint artifact bound exceeded",
        )?;
        let mut previous = None;
        for artifact in &self.artifacts {
            if matches!(
                artifact.kind,
                super::CustodyArtifactKind::AbandonedFetchedCommit
                    | super::CustodyArtifactKind::AbandonedDeltaConstruction
                    | super::CustodyArtifactKind::AbandonedEvidenceConstruction
                    | super::CustodyArtifactKind::AbandonedSuccessorConstruction
            ) {
                require(
                    artifact.record.is_some(),
                    "abandoned artifact has no original binding",
                )?;
                require(
                    artifact.operation != self.operation || self.seed.is_none(),
                    "abandoned artifact cannot seed membership",
                )?;
            }
            if artifact.kind == super::CustodyArtifactKind::IncomingDataCommit {
                require(
                    artifact.record.is_none(),
                    "data commit custody uses its checkpoint seed owner",
                )?;
            }
            if artifact.kind == super::CustodyArtifactKind::IncomingDataCommit
                && artifact.operation == self.operation
            {
                let Some(super::CheckpointSeed::Incoming { snapshot, .. }) = &self.seed else {
                    return Err(super::super::invalid(
                        "data commit artifact has no incoming seed",
                    ));
                };
                require(
                    snapshot.commit.retention_ref
                        == format!("refs/engram/task-sync/exchange/{}/commit", self.operation)
                        && artifact.content_fingerprint
                            == *crate::CanonicalObject::freeze(&snapshot.commit)?.key(),
                    "data commit artifact differs from its checkpoint seed",
                )?;
            }
            require(
                !artifact.operation.is_nil(),
                "missing custody artifact operation",
            )?;
            fingerprint(&artifact.content_fingerprint)?;
            let key = (artifact.operation, artifact.kind);
            require(
                previous.is_none_or(|old| old < key),
                "custody artifacts must be ordered and unique",
            )?;
            previous = Some(key);
        }
        Ok(())
    }
}
