//! Host-local preparation progress. This is neither a store nor Git progress.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use super::{TaskTree, directory, invalid, notes::NoteOmissions, pretty};
use crate::{ObjectId, ProjectId};

mod journal;
mod lock;
mod prepared;
mod resume;

const PENDING_PROFILE_GUIDANCE: &str = concat!(
    "resume at the original destination with the original include-notes setting ",
    "and original origin-machine selection (including no origin-machine), without --rebaseline"
);

pub(crate) use resume::resume_pending;

pub use prepared::{PreparedCandidate, PreparedProvenance, PreparedSelection, read_prepared};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportProfile {
    pub include_notes: bool,
    pub origin_machine: Option<String>,
}

impl ExportProfile {
    pub(crate) fn options(&self) -> super::TaskSyncExportOptions {
        super::TaskSyncExportOptions {
            include_notes: self.include_notes,
            origin_machine: self.origin_machine.clone(),
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        Ok(self.options().validate()?)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeedAnchor {
    pub object_kind: String,
    pub object_id: ObjectId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportCut {
    pub position: i64,
    pub anchor: Option<FeedAnchor>,
}

/// All fields were read inside the same SQLite read snapshot.
pub struct ExportCapture {
    pub tree: TaskTree,
    pub omissions: NoteOmissions,
    pub activity_omissions: NoteOmissions,
    pub cut: ExportCut,
    /// A missing/gapped/changed feed basis requests automatic full comparison.
    pub full_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PrepareReceipt {
    pub directory: String,
    pub operation: uuid::Uuid,
    pub prepared: bool,
    pub recovered: bool,
    pub path: String,
    pub reason: String,
    pub project_position: i64,
    pub serialized_tasks: usize,
    pub omissions: NoteOmissions,
    pub activity_omissions: NoteOmissions,
}

/// Prepare an immutable complete directory with locked, recoverable local progress.
///
/// # Errors
/// Refuses contention, profile drift, unsafe paths, malformed graphs and overwrite.
pub fn prepare(
    destination: &Path,
    progress: &Path,
    store: &Path,
    project: &ProjectId,
    profile: &ExportProfile,
    rebaseline: bool,
    capture: impl FnOnce(Option<&ExportCut>) -> Result<ExportCapture>,
) -> Result<PrepareReceipt> {
    // Identity reads the schema reference; do it outside the storage snapshot.
    let producer = crate::build_identity::current()
        .build_fingerprint
        .clone()
        .ok_or_else(|| invalid("producer identity unavailable for retained byte reuse"))?;
    prepare_with_producer(
        destination,
        progress,
        store,
        project,
        profile,
        rebaseline,
        &producer,
        capture,
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_with_producer(
    destination: &Path,
    progress: &Path,
    store: &Path,
    project: &ProjectId,
    profile: &ExportProfile,
    rebaseline: bool,
    producer: &ObjectId,
    capture: impl FnOnce(Option<&ExportCut>) -> Result<ExportCapture>,
) -> Result<PrepareReceipt> {
    profile.validate()?;
    let destination = resolved_child(destination)?;
    let progress = resolved_child(progress)?;
    let store = std::fs::canonicalize(store).context("resolving source store")?;
    if destination.starts_with(&progress)
        || progress.starts_with(&destination)
        || store.starts_with(&progress)
        || progress.starts_with(&store)
        || destination.starts_with(&store)
        || progress
            .components()
            .map(std::path::Component::as_os_str)
            .collect::<Vec<_>>()
            .windows(2)
            .any(|parts| {
                parts[0].eq_ignore_ascii_case(".engram") && parts[1].eq_ignore_ascii_case("tasks")
            })
    {
        return Err(invalid(
            "progress directory must be separate from candidate, store and .engram/tasks",
        )
        .into());
    }
    let _lock = lock::ProgressLock::acquire(&progress)?;
    let loaded = journal::load(&progress)?;
    let mut full_reason = loaded.reason;
    let previous = loaded.current;
    if let Some(previous) = &previous {
        if !previous.complete && (previous.meta.project != *project || previous.meta.store != store)
        {
            return Err(invalid(
                "pending preparation must finish with its original store and project binding",
            )
            .into());
        }
        if previous.meta.project != *project || previous.meta.store != store {
            full_reason = Some("store or project binding changed".into());
        } else {
            if !rebaseline && previous.meta.profile != *profile {
                let remedy = if previous.complete {
                    "use --rebaseline to change it"
                } else {
                    PENDING_PROFILE_GUIDANCE
                };
                return Err(invalid(format!(
                    "prepared profile or origin-machine is pinned; {remedy}"
                ))
                .into());
            }
            if !previous.complete {
                if rebaseline {
                    return Err(invalid(format!(
                        "pending preparation must finish before rebaseline; {PENDING_PROFILE_GUIDANCE}"
                    ))
                    .into());
                }
                if previous.meta.destination != destination {
                    return Err(invalid(format!(
                        "pending preparation must resume at {}",
                        previous.meta.destination.display()
                    ))
                    .into());
                }
                directory::write_documents(&destination, &previous.documents)?;
                previous.finish()?;
                return Ok(previous.receipt(true));
            }
        }
        if previous.meta.producer != *producer {
            full_reason = Some("producing build changed".into());
        }
        if directory::read_documents(&previous.meta.destination)
            .ok()
            .as_ref()
            != Some(&previous.documents)
        {
            full_reason = Some("previous output missing or divergent".into());
        }
    } else if full_reason.is_none() {
        full_reason = Some("first preparation".into());
    }
    if rebaseline {
        full_reason = Some("explicit rebaseline".into());
    }
    let basis = previous.as_ref().filter(|_| full_reason.is_none());
    let captured = capture(basis.map(|previous| &previous.meta.cut))?;
    if captured.tree.project.project_id != *project
        || captured.tree.project.notes_included != profile.include_notes
    {
        return Err(invalid("captured project or profile mismatch").into());
    }
    if let Some(reason) = captured.full_reason {
        full_reason = Some(reason);
    }
    let (documents, serialized_tasks) = select_documents(
        &captured.tree,
        previous
            .as_ref()
            .filter(|_| full_reason.is_none())
            .map(|previous| &previous.documents),
    )?;
    // A known overwrite refusal must not pin an operation that cannot publish.
    // Publication checks again after journaling to refuse concurrent changes.
    directory::matching_destination(&destination, &documents)?;
    let operation = uuid::Uuid::now_v7();
    let meta = journal::Metadata {
        sequence: loaded.next_sequence,
        operation,
        predecessor: previous.as_ref().map(|previous| previous.meta.operation),
        project: project.clone(),
        store,
        destination,
        profile: profile.clone(),
        producer: producer.clone(),
        cut: captured.cut,
        documents_fingerprint: documents_fingerprint(&documents),
        path: if full_reason.is_some() {
            "full"
        } else {
            "incremental"
        }
        .into(),
        reason: full_reason.unwrap_or_else(|| "verified feed and snapshot inventory".into()),
        serialized_tasks,
        omissions: captured.omissions,
        activity_omissions: captured.activity_omissions,
    };
    let pending = journal::publish(&progress, meta, documents)?;
    directory::write_documents(&pending.meta.destination, &pending.documents)?;
    pending.finish()?;
    Ok(pending.receipt(false))
}

fn select_documents(
    tree: &TaskTree,
    retained: Option<&BTreeMap<String, Vec<u8>>>,
) -> Result<(BTreeMap<String, Vec<u8>>, usize)> {
    tree.validate_stored()?;
    if tree.project.notes_included && tree.tasks.is_empty() {
        return Err(invalid("notes_included requires at least one task").into());
    }
    let old = retained.map(TaskTree::from_stored_documents).transpose()?;
    let old_tasks = old.as_ref().map(|tree| {
        tree.tasks
            .iter()
            .map(|task| (task.id.0, task))
            .collect::<BTreeMap<_, _>>()
    });
    let mut documents = BTreeMap::new();
    let mut serialized = 0;
    documents.insert("project.json".into(), pretty(&tree.project)?);
    for task in &tree.tasks {
        let path = format!("tasks/{}.json", task.id.0);
        let bytes = if old_tasks
            .as_ref()
            .and_then(|tasks| tasks.get(&task.id.0))
            .is_some_and(|old| *old == task)
        {
            retained
                .and_then(|documents| documents.get(&path))
                .cloned()
                .ok_or_else(|| invalid("retained task document missing"))?
        } else {
            serialized += 1;
            pretty(task)?
        };
        documents.insert(path, bytes);
    }
    TaskTree::from_stored_documents(&documents)?;
    Ok((documents, serialized))
}

pub(crate) fn documents_fingerprint(documents: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hash = Sha256::new();
    for (path, bytes) in documents {
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    format!("{:x}", hash.finalize())
}

fn resolved_child(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    if absolute.exists() {
        let meta = std::fs::symlink_metadata(&absolute)?;
        if meta.file_type().is_symlink() {
            return Err(invalid("prepare path must not be a symbolic link").into());
        }
        return Ok(std::fs::canonicalize(absolute)?);
    }
    let parent = absolute
        .parent()
        .ok_or_else(|| invalid("prepare path has no parent"))?;
    let name = absolute
        .file_name()
        .ok_or_else(|| invalid("prepare path has no filename"))?;
    Ok(std::fs::canonicalize(parent)?.join(name))
}

#[cfg(test)]
mod tests;
