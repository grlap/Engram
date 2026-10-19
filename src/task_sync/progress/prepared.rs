//! Explicit selection of completed preparation bytes. This reader never repairs a journal.

use super::{ExportCut, ExportProfile, documents_fingerprint, journal};
use crate::{
    ObjectId, ProjectId,
    task_sync::{TaskTree, notes::NoteOmissions},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub(super) mod files;
use files::Directory;

const MAX_FILE: usize = 1024 * 1024;
const MAX_TREE: usize = 128 * 1024 * 1024;
const MAX_TASKS: usize = 100_000;

/// Caller-selected identity; the reader never substitutes the latest operation.
#[derive(Clone, Debug)]
pub struct PreparedSelection {
    pub operation: uuid::Uuid,
    pub destination: PathBuf,
    pub project: ProjectId,
    pub profile: ExportProfile,
}

/// Local preparation provenance, not Git publication or disclosure approval.
#[derive(Clone, Debug)]
pub struct PreparedProvenance {
    pub sequence: u64,
    pub operation: uuid::Uuid,
    pub predecessor: Option<uuid::Uuid>,
    pub store: PathBuf,
    pub destination: PathBuf,
    pub project: ProjectId,
    pub profile: ExportProfile,
    pub producer: ObjectId,
    pub cut: ExportCut,
    pub omissions: NoteOmissions,
    pub activity_omissions: NoteOmissions,
}

/// Raw bytes are authoritative. `tree` is structural inspection, not a new export.
#[derive(Clone, Debug)]
pub struct PreparedCandidate {
    pub documents: BTreeMap<String, Vec<u8>>,
    pub tree: TaskTree,
    pub provenance: PreparedProvenance,
}

/// Read an explicitly selected completed generation and its byte-identical output.
/// No locks, directories or markers are created, and no leftovers are removed.
/// Retained notes/activity are validated structurally; a publishing caller must
/// separately apply its disclosure policy. This is not an atomic filesystem snapshot.
/// On Windows, retained directory handles temporarily deny deletion or renaming;
/// a concurrent preparation that prunes those directories may therefore refuse.
///
/// # Errors
/// Refuses pending/unknown entries, unsafe paths, malformed or divergent bytes,
/// missing generations and mismatched operation/project/profile/destination.
pub fn read_prepared(progress: &Path, selection: &PreparedSelection) -> Result<PreparedCandidate> {
    read_selected(progress, selection, false)
}

pub(super) fn read_selected(
    progress: &Path,
    selection: &PreparedSelection,
    pending: bool,
) -> Result<PreparedCandidate> {
    selection.profile.validate()?;
    let progress = Directory::open(progress)?;
    let names = progress.names(3)?;
    let mut generations = Vec::new();
    for name in &names {
        if name == "prepare.lock" {
            progress.check_file(Path::new(name), 64)?;
            continue;
        }
        let name_text = name.to_str().context("non-UTF8 prepared journal entry")?;
        let tail = name_text
            .strip_prefix("generation-")
            .context("pending or unknown prepared journal entry; preserve it")?;
        let (sequence, operation) = tail
            .split_once('-')
            .context("invalid prepared generation name")?;
        let sequence = sequence.parse::<u64>()?;
        let operation = uuid::Uuid::parse_str(operation)?;
        ensure!(
            sequence > 0 && name_text == format!("generation-{sequence:020}-{operation}"),
            "noncanonical prepared generation name"
        );
        let directory = progress.child(Path::new(name))?;
        let entries = directory.names(if pending { 16 } else { 3 })?;
        let declared: Vec<_> = entries
            .iter()
            .filter(|name| {
                !pending
                    || !name
                        .to_str()
                        .is_some_and(|name| name.starts_with(".complete-"))
            })
            .cloned()
            .collect();
        let completed =
            declared == ["candidate", "complete", "state.json"].map(std::ffi::OsString::from);
        ensure!(
            completed
                || (pending
                    && declared == ["candidate", "state.json"].map(std::ffi::OsString::from)),
            "incomplete or unknown prepared generation entries; preserve them"
        );
        let state = directory.read(Path::new("state.json"), journal::STATE_LIMIT)?;
        let metadata: journal::Metadata = crate::task_sync::parse(&state)?;
        ensure!(
            metadata.sequence == sequence
                && metadata.operation == operation
                && metadata.store.is_absolute()
                && metadata.destination.is_absolute()
                && metadata.cut.position >= 0
                && (metadata.cut.position == 0) == metadata.cut.anchor.is_none(),
            "invalid prepared journal binding"
        );
        metadata.profile.validate()?;
        if pending {
            for name in entries.iter().filter(|name| {
                name.to_str()
                    .is_some_and(|name| name.starts_with(".complete-"))
            }) {
                let name_text = name
                    .to_str()
                    .context("non-UTF8 completion temporary name")?;
                let suffix = name_text
                    .strip_prefix(".complete-")
                    .expect("filtered completion prefix");
                ensure!(
                    !suffix.is_empty()
                        && suffix.len() <= 64
                        && suffix
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
                    "unknown completion temporary entry; preserve it"
                );
                let bytes = directory.read(Path::new(name), 64)?;
                ensure!(
                    metadata
                        .operation
                        .to_string()
                        .as_bytes()
                        .starts_with(&bytes),
                    "completion temporary differs from admitted operation; preserve it"
                );
            }
        }
        let complete = if completed {
            directory.read(Path::new("complete"), 64)?
        } else {
            Vec::new()
        };
        ensure!(
            !completed || complete == operation.to_string().as_bytes(),
            "invalid prepared completion marker"
        );
        generations.push((metadata, directory, entries, state, complete));
    }
    generations.sort_by_key(|generation| generation.0.sequence);
    if pending {
        ensure!(
            generations
                .last()
                .is_some_and(|generation| generation.0.operation == selection.operation
                    && generation.4.is_empty())
                && generations
                    .iter()
                    .take(generations.len().saturating_sub(1))
                    .all(|generation| !generation.4.is_empty()),
            "only the latest admitted pending preparation can recover after opt-out"
        );
    }
    ensure!(
        !generations.is_empty() && generations.len() <= 2,
        "selected preparation missing or journal exceeds retained generations"
    );
    if generations.len() == 2 {
        let earlier = &generations[0].0;
        let later = &generations[1].0;
        ensure!(
            earlier.sequence.checked_add(1) == Some(later.sequence)
                && later
                    .predecessor
                    .is_none_or(|operation| operation == earlier.operation),
            "prepared predecessor missing or divergent"
        );
    } else {
        ensure!(
            generations[0].0.predecessor.is_none(),
            "prepared predecessor missing or divergent"
        );
    }
    // Keep only the selected generation's capabilities for the document read.
    let (metadata, directory, entries, state, complete) = generations
        .into_iter()
        .find(|generation| generation.0.operation == selection.operation)
        .context("selected prepared operation is not retained")?;
    ensure!(
        metadata.project == selection.project && metadata.profile == selection.profile,
        "{}",
        if pending {
            format!(
                "prepared project or profile differs from selection; {}",
                super::PENDING_PROFILE_GUIDANCE
            )
        } else {
            "prepared project or profile differs from selection".into()
        }
    );
    let destination = if pending {
        ensure!(
            super::resolved_child(&selection.destination)? == metadata.destination,
            "pending destination differs from selection"
        );
        None
    } else {
        let destination = Directory::open(&selection.destination)?;
        let recorded_destination = Directory::open(&metadata.destination)?;
        ensure!(
            destination.same_directory(&recorded_destination)?,
            "prepared destination differs from selection"
        );
        Some(destination)
    };
    let documents = read_documents(&directory.child(Path::new("candidate"))?)?;
    let tree = TaskTree::from_stored_documents(&documents)?;
    ensure!(
        tree.project.project_id == metadata.project
            && tree.project.notes_included == metadata.profile.include_notes,
        "prepared documents differ from metadata project or profile"
    );
    ensure!(
        !tree.project.notes_included || !tree.tasks.is_empty(),
        "empty notes-included preparation"
    );
    ensure!(
        documents_fingerprint(&documents) == metadata.documents_fingerprint,
        "prepared bytes differ from recorded basis"
    );
    if let Some(destination) = &destination {
        ensure!(
            read_documents(destination)? == documents,
            "prepared destination bytes differ from retained generation"
        );
    }
    ensure!(
        progress.names(3)? == names
            && directory.names(if pending { 16 } else { 3 })? == entries
            && directory.read(Path::new("state.json"), journal::STATE_LIMIT)? == state
            && (if complete.is_empty() {
                directory.names(if pending { 16 } else { 3 })? == entries
            } else {
                directory.read(Path::new("complete"), 64)? == complete
            }),
        "prepared journal changed during read"
    );
    Ok(PreparedCandidate {
        documents,
        tree,
        provenance: PreparedProvenance {
            sequence: metadata.sequence,
            operation: metadata.operation,
            predecessor: metadata.predecessor,
            store: metadata.store.clone(),
            destination: metadata.destination.clone(),
            project: metadata.project.clone(),
            profile: metadata.profile.clone(),
            producer: metadata.producer.clone(),
            cut: metadata.cut.clone(),
            omissions: metadata.omissions.clone(),
            activity_omissions: metadata.activity_omissions.clone(),
        },
    })
}

fn read_documents(directory: &Directory) -> Result<BTreeMap<String, Vec<u8>>> {
    let names = directory.names(2)?;
    ensure!(
        names == ["project.json", "tasks"].map(std::ffi::OsString::from),
        "incomplete or unknown prepared candidate entry"
    );
    let tasks = directory.child(Path::new("tasks"))?;
    let task_names = tasks.names(MAX_TASKS)?;
    let mut documents = BTreeMap::new();
    let project = directory.read(Path::new("project.json"), MAX_FILE)?;
    let mut total = project.len();
    documents.insert("project.json".into(), project);
    for name in &task_names {
        let name_text = name.to_str().context("non-UTF8 prepared task filename")?;
        let id = name_text
            .strip_suffix(".json")
            .context("invalid prepared task filename")?;
        let id = uuid::Uuid::parse_str(id)?;
        ensure!(
            name_text == format!("{id}.json"),
            "noncanonical prepared task filename"
        );
        let bytes = tasks.read(Path::new(name), MAX_FILE)?;
        total = total
            .checked_add(bytes.len())
            .context("prepared candidate byte bound overflow")?;
        ensure!(total <= MAX_TREE, "prepared candidate byte bound exceeded");
        documents.insert(format!("tasks/{name_text}"), bytes);
    }
    ensure!(
        directory.names(2)? == names && tasks.names(MAX_TASKS)? == task_names,
        "prepared candidate inventory changed during read"
    );
    Ok(documents)
}
