//! Directory candidates are explicit, immutable inputs, never Git or store backups.

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::Path,
};

use crate::task_sync::{
    ProjectDocument, TaskSyncRefusal, TaskTree, pretty,
    progress::{ExportProfile, documents_fingerprint},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TREE_BYTES: usize = 128 * 1024 * 1024;
const MAX_TASKS: usize = 100_000;

mod envelope;
mod evidence;
pub(crate) use envelope::prepare_folder_documents;

#[derive(Clone, Copy)]
struct Limits {
    file_bytes: u64,
    tree_bytes: usize,
    tasks: usize,
}

const LIMITS: Limits = Limits {
    file_bytes: MAX_FILE_BYTES,
    tree_bytes: MAX_TREE_BYTES,
    tasks: MAX_TASKS,
};

type Checkpoint<'a> = dyn FnMut(&str, &Path) -> Result<()> + 'a;

/// A complete folder candidate together with the inventory observed when its
/// destination host exported it. The basis is a caller-selected content
/// comparison, not authentication; import callers must select their own copy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderExchange {
    pub tree: TaskTree,
    pub export_basis: String,
    pub profile: ExportProfile,
}

/// The destination basis a caller selected from this host's own pre-merge
/// folder export: its project document, kept before the Git merge. It is a
/// content comparison, not an authenticated host identity, so selecting this
/// host's own copy is the caller's obligation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderBasis {
    pub project_id: crate::ProjectId,
    pub profile: ExportProfile,
    pub export_basis: String,
}

/// The destination basis selected for one folder import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DestinationBasis {
    /// This host's own pre-merge folder export.
    Recorded(FolderBasis),
    /// A first import into a destination with no shared planning. The
    /// applying transaction checks that the destination is in fact empty.
    Empty,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FolderProjectDocument {
    project: ProjectDocument,
    profile: ExportProfile,
    folder_exchange: FolderExchangeDocument,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_profile: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FolderExchangeDocument {
    complete: bool,
    export_basis: FolderExportBasis,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FolderExportBasis {
    inventory_fingerprint: String,
}

/// Read one complete directory without following links or ignoring unknown entries.
///
/// # Errors
/// Refuses partial, oversized, linked or malformed candidates.
pub fn read(path: &Path) -> Result<TaskTree> {
    let tree = read_stored(path)?;
    tree.validate()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    Ok(tree)
}

/// Reads the Git-folder envelope.  Its embedded basis proves only that the
/// folder is self-consistent; the destination's caller supplies the retained
/// local basis when applying a merged folder.
///
/// # Errors
///
/// Refuses malformed, incomplete, or self-inconsistent folder data.
pub fn read_folder(path: &Path) -> Result<FolderExchange> {
    let exchange = read_folder_stored(path, true)?;
    exchange
        .tree
        .validate_folder()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    Ok(exchange)
}

/// Reads a human-resolved Git folder for import. A merge that edits a task
/// leaves the embedded inventory fingerprint stale, and Git cannot regenerate
/// it, so the inventory is computed from the files instead. Every other check
/// stays: the envelope, `complete`, the profile, the strict graph and the
/// limits. What the destination compares is its own selected basis, never
/// this folder's.
///
/// # Errors
///
/// Refuses malformed, incomplete or invalid folder data.
pub fn read_resolved_folder(path: &Path) -> Result<FolderExchange> {
    let exchange = read_folder_stored(path, false)?;
    exchange
        .tree
        .validate_folder()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    Ok(exchange)
}

/// Reads a folder envelope and verifies its structural tree, and with
/// `verify_inventory` its embedded inventory fingerprint, without applying
/// current note-disclosure admission.
fn read_folder_stored(path: &Path, verify_inventory: bool) -> Result<FolderExchange> {
    folder_from_documents(
        read_raw_documents_with_tasks(path, false)?,
        verify_inventory,
    )
}

/// Read resolved task text from an admitted data worktree, accepting its two
/// ordinary policy files without treating them as task documents. Unknown
/// entries still refuse. Private raw candidates retain their strict layout.
///
/// # Errors
/// Refuses changed worktree selection, unsafe files or invalid task graphs.
pub fn read_resolved_data_folder(
    data: &super::data_worktree::DataWorktree,
) -> Result<FolderExchange> {
    read_resolved_data_folder_profile(data, false)
}

/// Reads the task component of an explicitly selected memory-capable profile.
/// The caller separately consumes its validated memory component after tasks.
///
/// # Errors
/// Refuses unsafe worktrees or malformed task/memory data.
pub fn read_resolved_data_folder_with_memories(
    data: &super::data_worktree::DataWorktree,
) -> Result<FolderExchange> {
    read_resolved_data_folder_profile(data, true)
}

fn read_resolved_data_folder_profile(
    data: &super::data_worktree::DataWorktree,
    memories: bool,
) -> Result<FolderExchange> {
    data.revalidate()?;
    let documents = read_documents_with_policy(
        &data.checkout.join(".engram"),
        LIMITS,
        false,
        true,
        &mut |_, _| Ok(()),
    )?;
    let exchange = folder_from_documents_profile(documents, false, memories)?;
    exchange
        .tree
        .validate_folder()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    data.revalidate()?;
    Ok(exchange)
}

pub(crate) fn folder_from_documents(
    documents: BTreeMap<String, Vec<u8>>,
    verify_inventory: bool,
) -> Result<FolderExchange> {
    folder_from_documents_profile(documents, verify_inventory, false)
}

pub(in crate::task_sync) fn folder_from_documents_profile(
    mut documents: BTreeMap<String, Vec<u8>>,
    verify_inventory: bool,
    memories: bool,
) -> Result<FolderExchange> {
    let project = documents
        .remove("project.json")
        .ok_or_else(|| TaskSyncRefusal::InvalidGraph.invalid("project.json missing"))?;
    let envelope: FolderProjectDocument = crate::task_sync::parse(&project)
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    if let Some(profile) = &envelope.memory_profile {
        anyhow::ensure!(
            profile == crate::task_sync::memories::FORMAT,
            "unsupported memory capability"
        );
        anyhow::ensure!(
            memories,
            "memory-capable profile requires explicit --include-memories"
        );
    }
    if !envelope.folder_exchange.complete {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task folder exchange must be complete")
            .into());
    }
    envelope.profile.options().validate()?;
    if envelope.project.notes_included != envelope.profile.include_notes {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task folder profile and tree disclosure differ")
            .into());
    }
    documents.insert(
        "project.json".into(),
        pretty(&envelope.project).map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?,
    );
    let tree = TaskTree::from_stored_documents(&documents)
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    let fingerprint = documents_fingerprint(&tree.folder_documents()?);
    if verify_inventory
        && envelope.folder_exchange.export_basis.inventory_fingerprint != fingerprint
    {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task folder exchange inventory fingerprint is inconsistent")
            .into());
    }
    Ok(FolderExchange {
        tree,
        export_basis: fingerprint,
        profile: envelope.profile,
    })
}

/// Reads one folder project document as a destination basis, with the same
/// file bounds as a folder read.
///
/// # Errors
///
/// Refuses a linked, irregular, oversized, malformed or incomplete document.
pub fn read_folder_basis(path: &Path) -> Result<FolderBasis> {
    let bytes = read_file_with(path, LIMITS, &mut |_, _| Ok(()))?;
    folder_basis_from_bytes(&bytes)
}

pub(crate) fn folder_basis_from_bytes(bytes: &[u8]) -> Result<FolderBasis> {
    let envelope: FolderProjectDocument = crate::task_sync::parse(bytes)
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    if let Some(profile) = &envelope.memory_profile {
        anyhow::ensure!(
            profile == crate::task_sync::memories::FORMAT,
            "unsupported memory capability"
        );
    }
    if envelope.project.format != crate::task_sync::FORMAT {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("unsupported task format")
            .into());
    }
    if !envelope.folder_exchange.complete {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task folder basis must be complete")
            .into());
    }
    envelope.profile.options().validate()?;
    if envelope.project.notes_included != envelope.profile.include_notes {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task folder basis profile and disclosure differ")
            .into());
    }
    let export_basis = envelope.folder_exchange.export_basis.inventory_fingerprint;
    if export_basis.len() != 64
        || !export_basis
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task folder basis fingerprint is malformed")
            .into());
    }
    Ok(FolderBasis {
        project_id: envelope.project.project_id,
        profile: envelope.profile,
        export_basis,
    })
}

fn read_stored(path: &Path) -> Result<TaskTree> {
    Ok(stored_tree(&read_documents(path)?)?)
}

fn stored_tree(
    documents: &BTreeMap<String, Vec<u8>>,
) -> Result<TaskTree, crate::storage::StoreError> {
    TaskTree::from_stored_documents(documents)
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))
}

/// Read retained bytes without applying today's note disclosure admission.
pub(crate) fn read_documents(path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let documents = read_raw_documents(path)?;
    stored_tree(&documents)?;
    Ok(documents)
}

fn read_raw_documents(path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    read_raw_documents_with_tasks(path, true)
}

fn read_raw_documents_with_tasks(
    path: &Path,
    require_tasks: bool,
) -> Result<BTreeMap<String, Vec<u8>>> {
    read_documents_with(path, LIMITS, require_tasks, &mut |_, _| Ok(()))
}

fn read_documents_with(
    path: &Path,
    limits: Limits,
    require_tasks: bool,
    checkpoint: &mut Checkpoint<'_>,
) -> Result<BTreeMap<String, Vec<u8>>> {
    read_documents_with_policy(path, limits, require_tasks, false, checkpoint)
}

fn read_documents_with_policy(
    path: &Path,
    limits: Limits,
    require_tasks: bool,
    allow_policy: bool,
    checkpoint: &mut Checkpoint<'_>,
) -> Result<BTreeMap<String, Vec<u8>>> {
    Ok(read_documents_with_memory_mode(
        path,
        limits,
        require_tasks,
        allow_policy,
        checkpoint,
        false,
    )?
    .0)
}

fn read_documents_with_memory_mode(
    path: &Path,
    limits: Limits,
    require_tasks: bool,
    allow_policy: bool,
    checkpoint: &mut Checkpoint<'_>,
    recovering_memories: bool,
) -> Result<(BTreeMap<String, Vec<u8>>, usize)> {
    regular_directory(path)?;
    let mut documents = BTreeMap::new();
    let mut total_bytes = 0;
    let mut task_count = 0;
    let mut has_tasks = false;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "project.json" {
            add_document(
                &mut documents,
                &mut total_bytes,
                "project.json".into(),
                &entry.path(),
                limits,
                checkpoint,
            )?;
        } else if name == "tasks" {
            has_tasks = true;
            regular_directory(&entry.path())?;
            for task in fs::read_dir(entry.path())? {
                let task = task?;
                let name = task.file_name().into_string().map_err(|_| {
                    TaskSyncRefusal::DestinationNonUtf8Name.invalid("non-UTF8 task filename")
                })?;
                task_count += 1;
                if task_count > limits.tasks {
                    return Err(TaskSyncRefusal::DestinationTaskLimit
                        .invalid("task candidate count limit exceeded")
                        .into());
                }
                add_document(
                    &mut documents,
                    &mut total_bytes,
                    format!("tasks/{name}"),
                    &task.path(),
                    limits,
                    checkpoint,
                )?;
            }
        } else if allow_policy && name == "evidence" {
            evidence::validate(&entry.path())?;
        } else if allow_policy && name == "memories" {
            if !recovering_memories {
                let (_, bytes) = crate::task_sync::memories::directory::read_root_with_bytes(
                    &crate::task_sync::setup::PrivateDirectory::open(path)?,
                )?;
                total_bytes = total_bytes
                    .checked_add(bytes)
                    .context("combined tree byte overflow")?;
                anyhow::ensure!(
                    total_bytes <= limits.tree_bytes,
                    "combined task and memory tree exceeds byte bound"
                );
            }
        } else if allow_policy && (name == ".gitignore" || name == ".gitattributes") {
            let bytes = read_file_with(&entry.path(), limits, checkpoint)?;
            total_bytes += bytes.len();
            if total_bytes > limits.tree_bytes {
                return Err(TaskSyncRefusal::DestinationTreeLimit
                    .invalid("task data and policy files exceed byte bound")
                    .into());
            }
        } else {
            return Err(TaskSyncRefusal::DestinationUnknownEntry
                .invalid("unknown task directory entry")
                .into());
        }
    }
    if require_tasks && !has_tasks {
        return Err(TaskSyncRefusal::DestinationMissingTasks
            .invalid("tasks directory missing")
            .into());
    }
    anyhow::ensure!(
        total_bytes <= limits.tree_bytes,
        "combined task and memory tree exceeds byte bound"
    );
    Ok((documents, total_bytes))
}

pub(in crate::task_sync) fn read_resolved_data_folder_for_memory_recovery(
    data: &super::data_worktree::DataWorktree,
) -> Result<(FolderExchange, usize)> {
    data.revalidate()?;
    let (documents, bytes) = read_documents_with_memory_mode(
        &data.checkout.join(".engram"),
        LIMITS,
        false,
        true,
        &mut |_, _| Ok(()),
        true,
    )?;
    let folder = folder_from_documents_profile(documents, false, true)?;
    folder.tree.validate_folder()?;
    data.revalidate()?;
    Ok((folder, bytes))
}

/// Explicitly advertises the memory capability in the shared project envelope.
/// Old readers reject this unknown field instead of claiming complete import.
///
/// # Errors
/// Refuses unsafe paths, unsupported envelopes or a concurrently changed file.
pub fn enable_memory_profile(data: &super::data_worktree::DataWorktree) -> Result<()> {
    data.revalidate()?;
    let root = crate::task_sync::setup::PrivateDirectory::open(
        &data.select_folder(&data.checkout.join(".engram"))?,
    )?;
    let before = root
        .read_bounded(Path::new("project.json"), MAX_FILE_BYTES as usize)?
        .context("project envelope missing")?;
    let mut envelope: FolderProjectDocument = crate::task_sync::parse(&before)?;
    if let Some(profile) = &envelope.memory_profile {
        anyhow::ensure!(
            profile == crate::task_sync::memories::FORMAT,
            "unsupported memory capability"
        );
        return Ok(());
    }
    envelope.memory_profile = Some(crate::task_sync::memories::FORMAT.into());
    let bytes = pretty(&envelope)?;
    let pending = format!("project-{}.pending", uuid::Uuid::new_v4());
    let mut file = root.open_file(Path::new(&pending), true, true)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    anyhow::ensure!(
        root.read_bounded(Path::new("project.json"), MAX_FILE_BYTES as usize)?
            .as_ref()
            == Some(&before),
        "project envelope changed before capability publication"
    );
    root.dir().rename(&pending, root.dir(), "project.json")?;
    data.revalidate()?;
    Ok(())
}

/// Preflight existing output without recording a pending operation.
pub(crate) fn matching_destination(
    path: &Path,
    documents: &BTreeMap<String, Vec<u8>>,
) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) if read_documents(path)? == *documents => Ok(true),
        Ok(_) => Err(occupied().into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Refuse documents that `write_documents` could never publish. It reads
/// nothing from disk, so a caller can check it before changing anything.
pub(crate) fn admit_documents(documents: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let tree = stored_tree(documents)?;
    if tree.project.notes_included && tree.tasks.is_empty() {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("notes_included requires at least one task")
            .into());
    }
    admit_limits(documents)
}

/// Refuse documents over a candidate limit, naming the first limit exceeded.
fn admit_limits(documents: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    admit_limits_with(documents, LIMITS)
}

fn admit_limits_with(documents: &BTreeMap<String, Vec<u8>>, limits: Limits) -> Result<()> {
    let refusal = if documents.len().saturating_sub(1) > limits.tasks {
        TaskSyncRefusal::DestinationTaskLimit
    } else if documents
        .values()
        .any(|bytes| bytes.len() as u64 > limits.file_bytes)
    {
        TaskSyncRefusal::DestinationFileLimit
    } else if documents.values().map(Vec::len).sum::<usize>() > limits.tree_bytes {
        TaskSyncRefusal::DestinationTreeLimit
    } else {
        return Ok(());
    };
    Err(refusal
        .invalid("export exceeds the supported task candidate limits")
        .into())
}

fn occupied() -> crate::storage::StoreError {
    TaskSyncRefusal::DestinationOccupied
        .invalid("export destination already contains a different candidate")
}

fn destination_parent(path: &Path) -> Result<&Path> {
    path.parent().ok_or_else(|| {
        TaskSyncRefusal::DestinationNoParent
            .invalid("export directory has no parent")
            .into()
    })
}

/// Publish already selected documents, preserving exact bytes on retry.
pub(crate) fn write_documents(path: &Path, documents: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    admit_documents(documents)?;
    let absolute = std::path::absolute(path)?;
    let path = absolute.as_path();
    if matching_destination(path, documents)? {
        return Ok(());
    }
    let parent = destination_parent(path)?;
    regular_directory(parent)?;
    let staging = parent.join(format!(".engram-task-candidate-{}", uuid::Uuid::now_v7()));
    fs::create_dir(&staging)?;
    let result = (|| {
        fs::create_dir(staging.join("tasks"))?;
        for (relative, bytes) in documents {
            use std::io::Write;
            let mut file = fs::File::create(staging.join(relative))?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        super::publication::publish(&staging, path)?;
        Ok(())
    })();
    if result.is_err() {
        // The UUID staging path was created by this call under the checked parent.
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn add_document(
    documents: &mut BTreeMap<String, Vec<u8>>,
    total: &mut usize,
    name: String,
    path: &Path,
    limits: Limits,
    checkpoint: &mut Checkpoint<'_>,
) -> Result<()> {
    let bytes = read_file_with(path, limits, checkpoint)
        .with_context(|| format!("reading {}", path.display()))?;
    *total += bytes.len();
    if *total > limits.tree_bytes {
        return Err(TaskSyncRefusal::DestinationTreeLimit
            .invalid("task candidate byte limit exceeded")
            .into());
    }
    documents.insert(name, bytes);
    Ok(())
}

/// Publish a validated directory once. Existing divergent candidates are preserved.
///
/// # Errors
/// Refuses overwrite, links, invalid data, or an I/O failure.
pub fn write(path: &Path, tree: &TaskTree) -> Result<()> {
    write_with(path, tree, LIMITS, &mut |_, _| Ok(()))
}

/// Publishes a complete Git folder with its deterministic exchange envelope.
/// The destination must be absent, or must already contain the exact same
/// bytes from an earlier attempt.
///
/// # Errors
///
/// Refuses an invalid profile, mismatched disclosure, unsafe destination, or
/// I/O failure.
pub fn write_folder(
    path: &Path,
    tree: &TaskTree,
    profile: &ExportProfile,
) -> Result<FolderExchange> {
    write_folder_at(path, tree, profile, false)
}

/// Publish a folder only inside the admitted data checkout, preserving its
/// ordinary policy files and evidence packs on an exact retry. Evidence carrier
/// names and file types are checked without reading their payloads.
/// No branch or index is changed.
///
/// # Errors
/// Refuses worktree drift, unsafe files or an occupied divergent destination.
pub fn write_data_folder(
    data: &super::data_worktree::DataWorktree,
    tree: &TaskTree,
    profile: &ExportProfile,
) -> Result<FolderExchange> {
    let path = data.select_folder(&data.checkout.join(".engram"))?;
    let written = write_folder_at(&path, tree, profile, true)?;
    data.revalidate()?;
    Ok(written)
}

fn write_folder_at(
    path: &Path,
    tree: &TaskTree,
    profile: &ExportProfile,
    allow_policy: bool,
) -> Result<FolderExchange> {
    let documents = tree
        .folder_documents()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    let (documents, export_basis) = prepare_folder_documents(&documents, profile)?;
    write_folder_documents(path, &documents, allow_policy)?;
    Ok(FolderExchange {
        tree: tree.clone(),
        export_basis,
        profile: profile.clone(),
    })
}

fn write_folder_documents(
    path: &Path,
    documents: &BTreeMap<String, Vec<u8>>,
    allow_policy: bool,
) -> Result<()> {
    admit_limits(documents)?;
    let absolute = std::path::absolute(path)?;
    let path = absolute.as_path();
    match fs::symlink_metadata(path) {
        Ok(_)
            if read_documents_with_policy(path, LIMITS, false, allow_policy, &mut |_, _| {
                Ok(())
            })? == *documents =>
        {
            return Ok(());
        }
        Ok(_) => return Err(occupied().into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = destination_parent(path)?;
    regular_directory(parent)?;
    let pending = parent.join(format!(".engram-task-candidate-{}", uuid::Uuid::now_v7()));
    fs::create_dir(&pending)?;
    let result = (|| {
        fs::create_dir(pending.join("tasks"))?;
        for (relative, bytes) in documents {
            let mut file = fs::File::create(pending.join(relative))?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        // Re-parse before publication with structural validation and the
        // envelope fingerprint. Retained historical notes need not satisfy
        // today's public disclosure admission until a consumer reads them.
        read_folder_stored(&pending, true)?;
        super::publication::publish(&pending, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&pending);
    }
    result
}

fn write_with(
    path: &Path,
    tree: &TaskTree,
    limits: Limits,
    checkpoint: &mut Checkpoint<'_>,
) -> Result<()> {
    let absolute = std::path::absolute(path)?;
    let path = absolute.as_path();
    let documents = tree
        .documents()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    admit_limits_with(&documents, limits)?;
    if path.try_exists()? {
        let existing = read_stored(path)?
            .documents()
            .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
        if existing == documents {
            return Ok(());
        }
        return Err(occupied().into());
    }
    let parent = destination_parent(path)?;
    regular_directory(parent)?;
    let staging = parent.join(format!(".engram-task-candidate-{}", uuid::Uuid::now_v7()));
    fs::create_dir(&staging)?;
    let result = (|| {
        fs::create_dir(staging.join("tasks"))?;
        for (relative, bytes) in documents {
            fs::write(staging.join(relative), bytes)?;
        }
        // Never replace an existing directory, even one created after the first check.
        checkpoint("before-publish", path)?;
        if path.try_exists()? {
            return Err(TaskSyncRefusal::DestinationAppeared
                .invalid("export destination appeared during preparation")
                .into());
        }
        super::publication::publish(&staging, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn regular_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let refusal = if meta.file_type().is_symlink() {
        TaskSyncRefusal::DestinationLink
    } else if !meta.is_dir() {
        TaskSyncRefusal::DestinationNotDirectory
    } else {
        return Ok(());
    };
    Err(refusal
        .invalid("task candidate directory must be an ordinary directory")
        .into())
}

fn read_file_with(path: &Path, limits: Limits, checkpoint: &mut Checkpoint<'_>) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    let refusal = if meta.file_type().is_symlink() {
        Some(TaskSyncRefusal::DestinationLink)
    } else if !meta.is_file() {
        Some(TaskSyncRefusal::DestinationNotFile)
    } else if meta.len() > limits.file_bytes {
        Some(TaskSyncRefusal::DestinationFileLimit)
    } else {
        None
    };
    if let Some(refusal) = refusal {
        return Err(refusal
            .invalid("task candidate file must be an ordinary bounded file")
            .into());
    }
    let mut bytes = Vec::new();
    checkpoint("before-file-read", path)?;
    fs::File::open(path)?
        .take(limits.file_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limits.file_bytes {
        return Err(TaskSyncRefusal::DestinationFileGrew
            .invalid("task candidate file grew beyond byte limit")
            .into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
