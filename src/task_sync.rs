//! Explicit planning-state interchange. Runtime authority never enters this profile.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    ActorContext, ChildRequirement, ProjectId, WorkBlockerKind, WorkId, WorkItemKind,
    WorkLifecycle,
    domain::{AcceptanceBinding, AcceptanceEvaluationMode},
    storage::StoreError,
};

pub mod activity;
pub mod directory;
pub mod exchange;
pub mod notes;
pub use activity::TaskSyncExportOptions;
pub mod git;
pub mod progress;
pub mod settings;
pub mod setup;

/// The supported planning-only profile; other profiles require explicit admission.
pub const FORMAT: &str = "engram-task-planning-v1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDocument {
    pub format: String,
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub notes_included: bool,
}

/// Shared attribution deliberately has no session, run, tool, path or authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SharedActor {
    pub actor_id: String,
    pub actor_kind: String,
}

impl SharedActor {
    pub(crate) fn from_actor(actor: &ActorContext) -> Self {
        Self {
            actor_id: actor.actor_id.clone(),
            actor_kind: actor.actor_kind.clone(),
        }
    }

    pub(crate) fn reported(&self) -> ActorContext {
        ActorContext {
            actor_id: self.actor_id.clone(),
            actor_kind: self.actor_kind.clone(),
            assurance: crate::domain::AssuranceLevel::Asserted,
            run_id: None,
            // Historical storage requires a binding. This fixed inert marker
            // records omitted provenance; it never registers or restores a session.
            session_id: Some(crate::SessionId(
                crate::domain::TASK_SYNC_REPORTED_SESSION.into(),
            )),
            source_tool: None,
            source_skill: None,
            provenance_chain: Vec::new(),
            reason: "reported task planning state".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SharedCompletion {
    pub summary: String,
    pub completed_at: DateTime<Utc>,
    pub actor: SharedActor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SharedBlocker {
    pub blocker_id: String,
    pub kind: WorkBlockerKind,
    pub detail: String,
    pub created_by: SharedActor,
    pub created_at: DateTime<Utc>,
}

/// One complete planning identity with optional reported shared notes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDocument {
    pub id: WorkId,
    pub external_ref: Option<String>,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<String>,
    #[serde(deserialize_with = "strict_bindings")]
    pub acceptance_bindings: Vec<AcceptanceBinding>,
    pub evaluation_mode: Option<AcceptanceEvaluationMode>,
    pub kind: WorkItemKind,
    pub priority: i32,
    pub labels: Vec<String>,
    pub assigned_to: Option<String>,
    pub deferred_until: Option<DateTime<Utc>>,
    pub lifecycle: WorkLifecycle,
    pub parent_id: Option<WorkId>,
    pub child_requirement: ChildRequirement,
    pub prerequisites: Vec<WorkId>,
    pub blockers: Vec<SharedBlocker>,
    pub superseded_by: Option<WorkId>,
    pub disposal_reason: Option<String>,
    pub completion: Option<SharedCompletion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<notes::SharedNote>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub activity: activity::ActivityMap,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTree {
    pub project: ProjectDocument,
    pub tasks: Vec<TaskDocument>,
}

impl TaskTree {
    /// Validates the complete candidate, including mixed dependency/hierarchy cycles.
    ///
    /// # Errors
    /// Refuses unsupported profiles, malformed policy, incomplete graphs or collisions.
    pub fn validate(&self) -> Result<(), StoreError> {
        self.validate_stored()?;
        self.validate_note_profile()?;
        for task in &self.tasks {
            notes::validate_notes(&task.notes, &task.labels)?;
            activity::validate_disclosure(&task.activity, &task.labels)?;
        }
        Ok(())
    }

    fn validate_note_profile(&self) -> Result<(), StoreError> {
        if self.project.notes_included && self.tasks.is_empty() {
            return Err(invalid("notes_included requires at least one task"));
        }
        Ok(())
    }

    /// Retained notes keep their recorded meaning when admission policy changes.
    pub(crate) fn validate_stored(&self) -> Result<(), StoreError> {
        if self.project.format != FORMAT {
            return Err(invalid("unsupported task format"));
        }
        if self.project.project_id.0.trim().is_empty() {
            return Err(invalid("empty project id"));
        }
        let mut ids = HashSet::new();
        let mut blockers = HashSet::new();
        let mut note_ids = HashSet::new();
        let mut activity_ids = HashSet::new();
        for task in &self.tasks {
            if !ids.insert(task.id) {
                return Err(invalid("duplicate task id"));
            }
            if !self.project.notes_included && !task.notes.is_empty() {
                return Err(invalid("shared notes require explicit notes_included"));
            }
            notes::validate_note_structure(&task.notes)?;
            activity::validate_structure(&task.activity, task.id)?;
            for (machine, entries) in &task.activity {
                for id in entries.keys() {
                    if !activity_ids.insert((machine, id)) {
                        return Err(invalid("duplicate origin-qualified activity id"));
                    }
                }
            }
            for note in &task.notes {
                if !note_ids.insert(&note.id) {
                    return Err(invalid("duplicate shared note id"));
                }
            }
            if (task.lifecycle == WorkLifecycle::Completed) != task.completion.is_some() {
                return Err(invalid("completion and planning lifecycle disagree"));
            }
            if let Some(completion) = &task.completion {
                validate_shared_actor(&completion.actor)?;
                if completion.summary.trim().is_empty() {
                    return Err(invalid("empty reported completion"));
                }
                if completion.summary.trim() != completion.summary {
                    return Err(invalid("reported completion summary must be normalized"));
                }
            }
            let mut positions = HashSet::new();
            for binding in &task.acceptance_bindings {
                if binding.criterion == 0
                    || binding.criterion > task.acceptance.len()
                    || !positions.insert(binding.criterion)
                {
                    return Err(invalid("invalid or duplicate acceptance binding position"));
                }
            }
            if task
                .acceptance_bindings
                .windows(2)
                .any(|p| p[0].criterion >= p[1].criterion)
            {
                return Err(invalid("acceptance bindings must be position ordered"));
            }
            if task.prerequisites.windows(2).any(|p| p[0].0 >= p[1].0) {
                return Err(invalid("prerequisites must be sorted and unique"));
            }
            if task
                .blockers
                .windows(2)
                .any(|p| p[0].blocker_id >= p[1].blocker_id)
            {
                return Err(invalid("blockers must be sorted and unique"));
            }
            for blocker in &task.blockers {
                if !blockers.insert(&blocker.blocker_id) {
                    return Err(invalid("duplicate blocker id"));
                }
                validate_shared_actor(&blocker.created_by)?;
            }
        }
        let items = self.snapshots()?;
        let blockers = self
            .tasks
            .iter()
            .flat_map(TaskDocument::snapshot_blockers)
            .collect::<Vec<_>>();
        crate::storage::graph_snapshot::validate_planning_graph(&items, &blockers)
    }

    /// Deterministic pretty JSON documents, always UTF-8 with LF and a final newline.
    ///
    /// # Errors
    /// Refuses structurally invalid data; retained notes are not filtered again.
    pub fn documents(&self) -> Result<BTreeMap<String, Vec<u8>>, StoreError> {
        self.validate_stored()?;
        self.validate_note_profile()?;
        let mut result = BTreeMap::new();
        result.insert("project.json".into(), pretty(&self.project)?);
        for task in &self.tasks {
            result.insert(format!("tasks/{}.json", task.id.0), pretty(task)?);
        }
        Ok(result)
    }

    /// Parses strict JSON, rejecting duplicate members at every depth.
    ///
    /// # Errors
    /// Refuses unsupported fields, partial graphs, mismatched filenames and conflict text.
    pub fn from_documents(documents: &BTreeMap<String, Vec<u8>>) -> Result<Self, StoreError> {
        let tree = Self::from_stored_documents(documents)?;
        tree.validate()?;
        Ok(tree)
    }

    pub(crate) fn from_stored_documents(
        documents: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Self, StoreError> {
        let project = parse(
            documents
                .get("project.json")
                .ok_or_else(|| invalid("project.json missing"))?,
        )?;
        let mut tasks = Vec::new();
        for (path, bytes) in documents {
            if path == "project.json" {
                continue;
            }
            let task: TaskDocument = parse(bytes)?;
            if path != &format!("tasks/{}.json", task.id.0) {
                return Err(invalid("task filename/id mismatch"));
            }
            tasks.push(task);
        }
        tasks.sort_by_key(|task| task.id.0);
        let tree = Self { project, tasks };
        tree.validate_stored()?;
        Ok(tree)
    }

    pub(crate) fn snapshots(&self) -> Result<Vec<crate::WorkGraphSnapshotItem>, StoreError> {
        let by_id = self
            .tasks
            .iter()
            .map(|task| (task.id, task))
            .collect::<HashMap<_, _>>();
        let mut roots = HashMap::new();
        for task in &self.tasks {
            let mut cursor = task.id;
            let mut path = Vec::new();
            let mut visited = HashSet::new();
            let root = loop {
                if let Some(root) = roots.get(&cursor) {
                    break *root;
                }
                if !visited.insert(cursor) {
                    return Err(invalid("parent cycle"));
                }
                let item = by_id
                    .get(&cursor)
                    .ok_or_else(|| invalid("dangling parent"))?;
                path.push(cursor);
                match item.parent_id {
                    Some(parent) => cursor = parent,
                    None => break cursor,
                }
            };
            for id in path {
                roots.insert(id, root);
            }
        }
        self.tasks
            .iter()
            .map(|task| {
                let root = roots
                    .get(&task.id)
                    .ok_or_else(|| invalid("task root missing"))?;
                Ok(task.snapshot_with_root(*root))
            })
            .collect()
    }
}

impl TaskDocument {
    pub(crate) fn snapshot_with_root(&self, root: WorkId) -> crate::WorkGraphSnapshotItem {
        crate::WorkGraphSnapshotItem {
            external_ref: self.external_ref.clone(),
            work_id: self.id,
            short_ref: format!("w-{}", &self.id.0.simple().to_string()[20..]),
            root_id: root,
            parent_id: self.parent_id,
            child_requirement: self.child_requirement,
            title: self.title.clone(),
            outcome: self.outcome.clone(),
            acceptance: self.acceptance.clone(),
            kind: self.kind,
            priority: self.priority,
            labels: self.labels.clone(),
            origin: crate::WorkOrigin::Local,
            source_snapshot_id: None,
            lifecycle: self.lifecycle,
            prerequisites: self.prerequisites.clone(),
            superseded_by: self.superseded_by,
            assigned_to: self.assigned_to.clone(),
            deferred_until: self.deferred_until,
            evaluation_mode: self.evaluation_mode,
            disposal_reason: self.disposal_reason.clone(),
        }
    }

    pub(crate) fn snapshot_blockers(&self) -> Vec<crate::WorkGraphSnapshotBlocker> {
        self.blockers
            .iter()
            .map(|blocker| crate::WorkGraphSnapshotBlocker {
                work_id: self.id,
                blocker_id: blocker.blocker_id.clone(),
                kind: blocker.kind,
                detail: blocker.detail.clone(),
                created_by: blocker.created_by.reported(),
                created_at: blocker.created_at,
            })
            .collect()
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> StoreError {
    StoreError::InvalidWork(message.into())
}

/// The family of a task export or import refusal. Each one has a stable
/// machine-readable code; its human text is unchanged by the classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskSyncRefusal {
    /// The target store already holds native work or execution state.
    NativeState,
    /// The candidate changes planning state the store already holds.
    DivergentPlanning,
    /// The candidate omits a task the store already holds.
    RemovedPlanning,
    /// A candidate id already belongs to another record, task or project.
    IdCollision,
    /// The candidate is not a valid task graph.
    InvalidGraph,
    /// The candidate's project or an import origin binding does not match.
    OriginMismatch,
    /// The export destination already holds a different candidate.
    DestinationOccupied,
    /// The export destination appeared while the export was being prepared.
    DestinationAppeared,
    /// The export destination has no parent directory.
    DestinationNoParent,
    /// A candidate path is a symbolic link.
    DestinationLink,
    /// A candidate directory is not an ordinary directory.
    DestinationNotDirectory,
    /// A candidate file is not an ordinary file.
    DestinationNotFile,
    /// A candidate file is larger than the per-file limit.
    DestinationFileLimit,
    /// A candidate file grew past the per-file limit while it was read.
    DestinationFileGrew,
    /// The candidate holds more tasks than the task count limit.
    DestinationTaskLimit,
    /// The candidate is larger than the tree byte limit.
    DestinationTreeLimit,
    /// A task filename is not valid UTF-8.
    DestinationNonUtf8Name,
    /// The candidate directory holds an entry the format does not define.
    DestinationUnknownEntry,
    /// The candidate directory has no tasks directory.
    DestinationMissingTasks,
}

impl TaskSyncRefusal {
    /// Every refusal family, in documentation order.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 19] = [
        Self::NativeState,
        Self::DivergentPlanning,
        Self::RemovedPlanning,
        Self::IdCollision,
        Self::InvalidGraph,
        Self::OriginMismatch,
        Self::DestinationOccupied,
        Self::DestinationAppeared,
        Self::DestinationNoParent,
        Self::DestinationLink,
        Self::DestinationNotDirectory,
        Self::DestinationNotFile,
        Self::DestinationFileLimit,
        Self::DestinationFileGrew,
        Self::DestinationTaskLimit,
        Self::DestinationTreeLimit,
        Self::DestinationNonUtf8Name,
        Self::DestinationUnknownEntry,
        Self::DestinationMissingTasks,
    ];

    /// The stable machine-readable code of this refusal.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NativeState => "task_sync_native_state",
            Self::DivergentPlanning => "task_sync_divergent_planning",
            Self::RemovedPlanning => "task_sync_removed_planning",
            Self::IdCollision => "task_sync_id_collision",
            Self::InvalidGraph => "task_sync_invalid_graph",
            Self::OriginMismatch => "task_sync_origin_mismatch",
            Self::DestinationOccupied => "task_sync_destination_occupied",
            Self::DestinationAppeared => "task_sync_destination_appeared",
            Self::DestinationNoParent => "task_sync_destination_no_parent",
            Self::DestinationLink => "task_sync_destination_link",
            Self::DestinationNotDirectory => "task_sync_destination_not_directory",
            Self::DestinationNotFile => "task_sync_destination_not_file",
            Self::DestinationFileLimit => "task_sync_destination_file_limit",
            Self::DestinationFileGrew => "task_sync_destination_file_grew",
            Self::DestinationTaskLimit => "task_sync_destination_task_limit",
            Self::DestinationTreeLimit => "task_sync_destination_tree_limit",
            Self::DestinationNonUtf8Name => "task_sync_destination_non_utf8_name",
            Self::DestinationUnknownEntry => "task_sync_destination_unknown_entry",
            Self::DestinationMissingTasks => "task_sync_destination_missing_tasks",
        }
    }

    /// Refuse with this family, keeping the invalid-input text unchanged.
    pub(crate) fn invalid(self, reason: impl Into<String>) -> StoreError {
        self.wrap(invalid(reason))
    }

    /// Classify an invalid-input, invalid-graph or oversized-note refusal
    /// from shared validation. Any other error, including one already
    /// classified, passes through unchanged.
    pub(crate) fn classify(self, error: StoreError) -> StoreError {
        match error {
            StoreError::InvalidWork(_)
            | StoreError::InvalidGraphSnapshot(_)
            | StoreError::WorkNoteTooLarge { .. } => self.wrap(error),
            other => other,
        }
    }

    fn wrap(self, error: StoreError) -> StoreError {
        StoreError::TaskSyncRefused {
            refusal: self,
            inner: Box::new(error),
        }
    }
}

fn validate_shared_actor(actor: &SharedActor) -> Result<(), StoreError> {
    if [&actor.actor_id, &actor.actor_kind]
        .iter()
        .any(|text| text.trim().is_empty())
    {
        return Err(invalid("invalid shared attribution"));
    }
    Ok(())
}

pub(crate) fn pretty(value: &impl Serialize) -> Result<Vec<u8>, StoreError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
    let value = crate::graph_snapshot::json_input::parse(bytes)
        .map_err(|error| invalid(error.to_string()))?;
    serde_json::from_value(value).map_err(|error| invalid(error.to_string()))
}

fn strict_bindings<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<AcceptanceBinding>, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Requirement {
        check_kind: crate::domain::VerificationKind,
        check_fingerprint: Option<crate::ObjectId>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Binding {
        criterion: usize,
        requirement: Requirement,
    }
    let bindings = Vec::<Binding>::deserialize(deserializer)?;
    Ok(bindings
        .into_iter()
        .map(|binding| AcceptanceBinding {
            criterion: binding.criterion,
            requirement: crate::domain::VerificationRequirement {
                check_kind: binding.requirement.check_kind,
                check_fingerprint: binding.requirement.check_fingerprint,
            },
        })
        .collect())
}

#[cfg(test)]
mod refusal_tests;
