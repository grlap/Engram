//! Bounded memory files in the existing admitted task-data checkout.

use super::{FORMAT, Head, KeyHistory, ObjectId, Revision, validate_key};
use crate::task_sync::{data_worktree::DataWorktree, setup::PrivateDirectory};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Write, path::Path};

const FILE_LIMIT: usize = 1024 * 1024;
const TREE_LIMIT: usize = 128 * 1024 * 1024;
const ENTRY_LIMIT: usize = 100_000;
mod recovery;

#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_MEMORY_WRITE: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HeadDocument {
    format: String,
    project_id: crate::ProjectId,
    key: String,
    head: Head,
}

/// Find interrupted memory publication before task export can confirm or write.
pub(crate) fn require_no_pending_publication(data: &DataWorktree) -> Result<()> {
    let folder = data.checkout.join(".engram");
    if !folder.try_exists()? {
        return Ok(());
    }
    let root = PrivateDirectory::open(&folder)?;
    let Some(memories) = optional_child(&root, Path::new("memories"))? else {
        return Ok(());
    };
    for key in memories.names(ENTRY_LIMIT)? {
        let directory = memories.child(Path::new(&key))?;
        if directory.names(ENTRY_LIMIT)?.iter().any(|name| {
            name.to_str().is_some_and(|name| {
                name.ends_with(".pending")
                    && (name.starts_with("head-") || name.starts_with("revision-"))
            })
        }) {
            return Err(crate::task_sync::TaskSyncRefusal::MemoryExportComponentChanged
                .invalid("memory publication staging is pending; finish with `engram task-sync export --rebuild --include-memories --memory-recovery-basis OWN_EXPORT`")
                .into());
        }
    }
    Ok(())
}

/// Reads every selected key without following links. Import refuses unresolved
/// revision files even when Git merged their names without a text conflict.
///
/// # Errors
/// Refuses malformed, oversized, linked or structurally divergent selections.
pub fn read(data: &DataWorktree) -> Result<BTreeMap<String, KeyHistory>> {
    let held = publication_lock(data)?;
    read_locked(data, &held)
}

/// Reads under the fence held by an encompassing folder import.
///
/// # Errors
/// Refuses a different checkout fence or malformed memory selection.
pub fn read_locked(
    data: &DataWorktree,
    held: &PublicationLock,
) -> Result<BTreeMap<String, KeyHistory>> {
    held.validate(data)?;
    let selected = data.select_folder(&data.checkout.join(".engram"))?;
    let root = PrivateDirectory::open(&selected)?;
    read_root(&root)
}

/// [`read_locked`], with the bytes the memory files hold on disk as read, for
/// an import that counts them against the folder's size limit.
///
/// # Errors
/// As [`read_locked`].
pub fn read_locked_with_bytes(
    data: &DataWorktree,
    held: &PublicationLock,
) -> Result<(BTreeMap<String, KeyHistory>, u64)> {
    held.validate(data)?;
    let selected = data.select_folder(&data.checkout.join(".engram"))?;
    let root = PrivateDirectory::open(&selected)?;
    let (result, bytes) = read_root_with_bytes(&root)?;
    Ok((result, u64::try_from(bytes)?))
}

/// Captures the data worktree HEAD component, or an unborn bootstrap basis.
///
/// # Errors
/// Refuses a changed worktree, malformed committed memories or missing history.
pub fn committed(
    data: &DataWorktree,
    project: &crate::ProjectId,
) -> Result<BTreeMap<String, KeyHistory>> {
    data.revalidate()?;
    Ok(match &data.head {
        Some(head) => crate::task_sync::git::read_memory_snapshot(&data.checkout, project, head)?,
        None => BTreeMap::new(),
    })
}

fn ensure_memory_policy(
    data: &DataWorktree,
    root: Option<&PrivateDirectory>,
    selected: &BTreeMap<String, KeyHistory>,
) -> Result<()> {
    use crate::task_sync::{data_worktree::QUERY_BOUND, git};
    if let Some(head) = &data.head {
        let entry = git::query(
            &data.checkout,
            &["ls-tree", "-z", head, "--", ".engram/.gitignore"],
            false,
            None,
            QUERY_BOUND,
        )?;
        let committed = if entry.is_empty() {
            None
        } else {
            ensure!(
                entry.starts_with(b"100644 blob "),
                crate::task_sync::TaskSyncRefusal::MemoryIgnorePolicyUncommitted.invalid("memory_ignore_policy_uncommitted: ignore policy must be an ordinary committed file")
            );
            Some(git::query(
                &data.checkout,
                &["show", &format!("{head}:.engram/.gitignore")],
                false,
                None,
                QUERY_BOUND,
            )?)
        };
        ensure!(
            root.map(|root| root.read_bounded(Path::new(".gitignore"), FILE_LIMIT))
                .transpose()?
                .flatten()
                == committed,
            crate::task_sync::TaskSyncRefusal::MemoryIgnorePolicyUncommitted.invalid("memory_ignore_policy_uncommitted: commit the explicit memory policy block before export")
        );
    }
    let mut input = Vec::new();
    for (key, history) in selected {
        for name in std::iter::once("head.json".to_owned())
            .chain(history.revisions.keys().map(|id| format!("{id}.json")))
        {
            input.extend_from_slice(format!(".engram/memories/{key}/{name}").as_bytes());
            input.push(0);
        }
    }
    if !input.is_empty() {
        let ignored = git::query(
            &data.checkout,
            &["check-ignore", "--no-index", "-z", "--stdin"],
            true,
            Some(&input),
            QUERY_BOUND,
        )?;
        ensure!(
            ignored.is_empty(),
            crate::task_sync::TaskSyncRefusal::MemoryIgnorePolicyExcludesTree.invalid("memory_ignore_policy_excludes_tree: explicitly update and commit the .engram/.gitignore memory allowlist; see the memory profile documentation")
        );
    }
    Ok(())
}

/// Performs all memory publication admission before any shared-file write.
///
/// # Errors
/// Refuses a changed basis, unsafe policy/content, invalid recovery evidence,
/// immutable rewrites and combined task/memory size or project mismatches.
pub fn preflight(
    data: &DataWorktree,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
    recovering: bool,
    tasks: &crate::task_sync::TaskTree,
    profile: &crate::task_sync::progress::ExportProfile,
) -> Result<()> {
    let task_bytes = crate::task_sync::directory::memory_export_task_bytes(tasks, profile)?;
    preflight_component(
        data,
        basis,
        selected,
        recovering,
        &tasks.project.project_id,
        Some(task_bytes),
    )
    .map(drop)
}

/// [`preflight`] for an export whose task files are written and sized by
/// their own incremental route: every memory admission check, and the
/// combined size only when `task_bytes` is given. Returns the bytes of the
/// selected memory component, for the task route's size limit.
///
/// # Errors
/// As [`preflight`].
pub fn preflight_component(
    data: &DataWorktree,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
    recovering: bool,
    project: &crate::ProjectId,
    task_bytes: Option<usize>,
) -> Result<u64> {
    data.revalidate()?;
    let mut total = 0_usize;
    for (key, history) in selected {
        history.validate()?;
        ensure!(
            key == &history.key && history.project_id == *project,
            "memory publication project/key mismatch"
        );
        let head = serde_json::to_vec_pretty(&HeadDocument {
            format: FORMAT.into(),
            project_id: history.project_id.clone(),
            key: key.clone(),
            head: history.head.clone(),
        })?;
        ensure!(head.len() <= FILE_LIMIT, "memory head exceeds 1 MiB");
        total = total
            .checked_add(head.len())
            .context("memory publication byte overflow")?;
        for revision in history.revisions.values() {
            let bytes = serde_json::to_vec_pretty(revision)?;
            ensure!(bytes.len() <= FILE_LIMIT, "memory revision exceeds 1 MiB");
            total = total
                .checked_add(bytes.len())
                .context("memory publication byte overflow")?;
        }
    }
    if let Some(task_bytes) = task_bytes {
        ensure!(
            total
                .checked_add(task_bytes)
                .is_some_and(|combined| combined <= TREE_LIMIT),
            "combined task and memory tree exceeds 128 MiB"
        );
    }
    for (key, old) in basis {
        old.validate()?;
        let new = selected
            .get(key)
            .context("memory export cannot omit a selected key")?;
        ensure!(old.project_id == *project, "memory basis project mismatch");
        if matches!(old.head, Head::Tombstone { .. }) {
            ensure!(old == new, "permanent tombstone cannot be replaced");
        }
        for (id, revision) in &old.revisions {
            if let Some(value) = new.revisions.get(id) {
                ensure!(value == revision, "immutable shared revision bytes changed");
            }
        }
    }
    let path = data.select_folder(&data.checkout.join(".engram"))?;
    let root = if path.exists() {
        Some(PrivateDirectory::open(&path)?)
    } else {
        None
    };
    ensure_memory_policy(data, root.as_ref(), selected)?;
    match root {
        Some(root) if recovering => {
            recovery::validate(&root, basis, selected)?;
        }
        Some(root) => {
            ensure!(
                read_root_for_capability(&root, true)?.0 == *basis,
                crate::task_sync::TaskSyncRefusal::MemoryExportComponentChanged.invalid(
                    "memory_export_component_changed: on-disk memory tree differs from the committed basis. Preserve any loose memory edits, then restore the committed memory component and retry export."
                )
            );
            ensure_revision_bytes(&root, selected)?;
        }
        None => ensure!(
            basis.is_empty(),
            crate::task_sync::TaskSyncRefusal::MemoryExportComponentMissing.invalid("memory_export_component_missing: restore the committed memory component before export")
        ),
    }
    Ok(u64::try_from(total)?)
}

/// The exact memory paths and postimages the admitted publication will
/// change, for confirming a combined task and memory commit later.
///
/// # Errors
/// Refuses invalid selections or unsafe or unreadable memory paths.
pub fn publication_changes(
    data: &DataWorktree,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
    recovering: bool,
) -> Result<BTreeMap<String, Option<String>>> {
    let folder = data.select_admitted_folder(&data.checkout.join(".engram"))?;
    let memories = if folder.exists() {
        optional_child(&PrivateDirectory::open(&folder)?, Path::new("memories"))?
    } else {
        None
    };
    let mut changes = BTreeMap::new();
    for (key, history) in selected {
        validate_key(key)?;
        let previous = basis.get(key);
        let head = serde_json::to_string_pretty(&HeadDocument {
            format: FORMAT.into(),
            project_id: history.project_id.clone(),
            key: key.clone(),
            head: history.head.clone(),
        })?;
        let path = format!("memories/{key}/head.json");
        let directory = match &memories {
            Some(memories) => optional_child(memories, Path::new(key))?,
            None => None,
        };
        let existing = match &directory {
            Some(directory) => directory.read_bounded(Path::new("head.json"), FILE_LIMIT)?,
            None => None,
        };
        if previous.is_none_or(|previous| previous.head != history.head)
            || existing.as_deref() != Some(head.as_bytes())
        {
            changes.insert(path, Some(head));
        }
        for (id, revision) in &history.revisions {
            let is_new = previous.is_none_or(|previous| !previous.revisions.contains_key(id));
            if !is_new && !recovering {
                continue;
            }
            let name = format!("{id}.json");
            // Recovery preflight already admitted this retained history.
            // Its writer will restore a missing revision to these exact bytes.
            let restores_missing = if !is_new && let Some(directory) = &directory {
                match directory.dir().symlink_metadata(&name) {
                    Ok(_) => false,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                    Err(error) => return Err(error.into()),
                }
            } else {
                false
            };
            if is_new || restores_missing {
                changes.insert(
                    format!("memories/{key}/{name}"),
                    Some(serde_json::to_string_pretty(revision)?),
                );
            }
        }
        if let Some(previous) = previous {
            for id in previous.revisions.keys() {
                if !history.revisions.contains_key(id) {
                    changes.insert(format!("memories/{key}/{id}.json"), None);
                }
            }
        }
    }
    Ok(changes)
}

fn optional_child(root: &PrivateDirectory, name: &Path) -> Result<Option<PrivateDirectory>> {
    match root.child(name) {
        Ok(child) => Ok(Some(child)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Each selected revision already on disk holds exactly the bytes the writer
/// would publish for it. The writer refuses anything else after the task
/// files are written, so it is refused here, before any write.
fn ensure_revision_bytes(
    root: &PrivateDirectory,
    selected: &BTreeMap<String, KeyHistory>,
) -> Result<()> {
    let absent = |error: &anyhow::Error| {
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    };
    let memories = match root.child(Path::new("memories")) {
        Ok(child) => child,
        Err(error) if absent(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    for (key, history) in selected {
        let directory = match memories.child(Path::new(key)) {
            Ok(child) => child,
            Err(error) if absent(&error) => continue,
            Err(error) => return Err(error),
        };
        for (id, revision) in &history.revisions {
            let name = format!("{id}.json");
            let Some(existing) = directory.read_bounded(Path::new(&name), FILE_LIMIT)? else {
                continue;
            };
            if existing != serde_json::to_vec_pretty(revision)? {
                let eol = if existing.windows(2).any(|pair| pair == b"\r\n") {
                    " It holds CRLF line endings: keep `text eol=lf` in .engram/.gitattributes and check core.autocrlf, then restore the file from the data branch."
                } else {
                    " Restore it from the data branch."
                };
                return Err(crate::task_sync::TaskSyncRefusal::MemoryExportComponentChanged
                    .invalid(format!(
                        "memory_export_component_changed: memories/{key}/{name} is not byte for byte the shared revision it records, so nothing was written.{eol}"
                    ))
                    .into());
            }
        }
    }
    Ok(())
}

/// Reads a caller-retained pre-export folder as explicit recovery evidence.
/// No host-private mapping or publication checkpoint is created.
///
/// # Errors
/// Refuses unsafe, malformed or incomplete retained memory data.
pub fn read_basis(path: &Path) -> Result<BTreeMap<String, KeyHistory>> {
    read_root(&PrivateDirectory::open(path)?)
}

/// Reads only the unchanged task component while explicit memory recovery is
/// pending. Recovery separately validates every memory entry before changing it.
///
/// # Errors
/// Refuses unsafe files, unknown root entries and malformed task metadata.
pub fn read_tasks_for_recovery(
    data: &DataWorktree,
) -> Result<crate::task_sync::directory::FolderExchange> {
    Ok(crate::task_sync::directory::read_resolved_data_folder_for_memory_recovery(data)?.0)
}

/// Reads bounded caller-selected reconciliation evidence without following links.
///
/// # Errors
/// Refuses unsafe/oversized files or malformed per-key options.
pub fn read_options(path: &Path) -> Result<BTreeMap<String, super::ImportOptions>> {
    let absolute = std::path::absolute(path)?;
    let parent = PrivateDirectory::open(
        absolute
            .parent()
            .context("memory resolution parent missing")?,
    )?;
    let bytes = parent
        .read_bounded(
            Path::new(
                absolute
                    .file_name()
                    .context("memory resolution filename missing")?,
            ),
            FILE_LIMIT,
        )?
        .context("memory resolution file missing")?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(in crate::task_sync) fn read_root(
    root: &PrivateDirectory,
) -> Result<BTreeMap<String, KeyHistory>> {
    Ok(read_root_with_bytes(root)?.0)
}

pub(in crate::task_sync) fn read_root_with_bytes(
    root: &PrivateDirectory,
) -> Result<(BTreeMap<String, KeyHistory>, usize)> {
    read_root_for_capability(root, false)
}

fn read_root_for_capability(
    root: &PrivateDirectory,
    allow_absent: bool,
) -> Result<(BTreeMap<String, KeyHistory>, usize)> {
    match root.dir().symlink_metadata("memories") {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((BTreeMap::new(), 0)),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    let memories = root.child(Path::new("memories"))?;
    let envelope: serde_json::Value = serde_json::from_slice(
        &root
            .read_bounded(Path::new("project.json"), FILE_LIMIT)?
            .context("memory profile envelope missing")?,
    )?;
    ensure!(
        envelope
            .get("memory_profile")
            .and_then(serde_json::Value::as_str)
            == Some(FORMAT)
            || (allow_absent && envelope.get("memory_profile").is_none()),
        "memory directory requires its versioned project capability"
    );
    let project = envelope
        .get("project")
        .and_then(|p| p.get("project_id"))
        .and_then(serde_json::Value::as_str)
        .context("memory profile project missing")?;
    let mut result = BTreeMap::new();
    let mut total = 0usize;
    for name in memories.names(ENTRY_LIMIT)? {
        let key = name.to_str().context("non-UTF8 memory key")?;
        validate_key(key)?;
        let directory = memories.child(Path::new(key))?;
        let raw = directory
            .read_bounded(Path::new("head.json"), FILE_LIMIT)?
            .context("memory head missing")?;
        total = total
            .checked_add(raw.len())
            .context("memory head byte overflow")?;
        ensure!(total <= TREE_LIMIT, "memory tree exceeds 128 MiB");
        let head: HeadDocument = serde_json::from_slice(&raw)?;
        ensure!(
            head.format == FORMAT && head.key == key,
            "memory head format/key mismatch"
        );
        let mut revisions = BTreeMap::new();
        for filename in directory.names(ENTRY_LIMIT)? {
            if filename == "head.json" {
                continue;
            }
            let filename = filename.to_str().context("non-UTF8 memory filename")?;
            let id: ObjectId = filename
                .strip_suffix(".json")
                .context("unknown memory directory entry")?
                .parse()
                .map_err(anyhow::Error::msg)?;
            super::validate_id(&id)?;
            let bytes = directory
                .read_bounded(Path::new(filename), FILE_LIMIT)?
                .context("memory revision missing")?;
            total = total
                .checked_add(bytes.len())
                .context("memory byte count overflow")?;
            ensure!(total <= TREE_LIMIT, "memory tree exceeds 128 MiB");
            let revision: Revision = serde_json::from_slice(&bytes)?;
            ensure!(
                revision.provenance.revision_id == id,
                "memory filename/id mismatch"
            );
            revisions.insert(id, revision);
        }
        let history = KeyHistory {
            format: head.format,
            project_id: head.project_id,
            key: head.key,
            head: head.head,
            revisions,
        };
        history.validate()?;
        ensure!(
            history.project_id.0 == project,
            "memory key belongs to another project"
        );
        result.insert(key.into(), history);
    }
    ensure!(total <= TREE_LIMIT, "memory tree exceeds 128 MiB");
    Ok((result, total))
}

pub(in crate::task_sync) fn decode_key(
    key: &str,
    documents: BTreeMap<String, Vec<u8>>,
) -> Result<KeyHistory> {
    let (project_id, head) = decode_head(
        key,
        documents.get("head.json").context("memory head missing")?,
    )?;
    let mut revisions = BTreeMap::new();
    for (name, bytes) in documents {
        if name == "head.json" {
            continue;
        }
        let id: ObjectId = name
            .strip_suffix(".json")
            .context("unknown memory file")?
            .parse()
            .map_err(anyhow::Error::msg)?;
        let revision: Revision = serde_json::from_slice(&bytes)?;
        ensure!(
            revision.provenance.revision_id == id,
            "memory filename/id mismatch"
        );
        revisions.insert(id, revision);
    }
    let history = KeyHistory {
        format: FORMAT.to_owned(),
        project_id,
        key: key.to_owned(),
        head,
        revisions,
    };
    history.validate()?;
    Ok(history)
}

pub(in crate::task_sync) fn decode_head(
    key: &str,
    bytes: &[u8],
) -> Result<(crate::ProjectId, Head)> {
    validate_key(key)?;
    let head: HeadDocument = serde_json::from_slice(bytes)?;
    ensure!(
        head.key == key && head.format == FORMAT,
        "memory head format/key mismatch"
    );
    Ok((head.project_id, head.head))
}

/// Publishes deterministic revision bytes and updates heads last. Existing UUID
/// bytes are immutable. The caller's exact selected basis is rechecked under
/// the existing file-lock primitive before publication.
///
/// # Errors
/// Refuses stale inputs, changed immutable files and unsafe paths. Failed
/// publication retains its files for explicit recovery; it never deletes a key.
pub fn write(
    data: &DataWorktree,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
) -> Result<()> {
    write_inner(data, basis, selected, false)
}

/// Recovers only files proved to be an interrupted publication of the explicit
/// retained basis to the newly recaptured canonical selection. Unknown bytes,
/// immutable rewrites and changed selected heads refuse without mutation.
///
/// # Errors
/// Refuses unrelated files or changes, unsafe paths and invalid recovery input.
pub fn recover(
    data: &DataWorktree,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
) -> Result<()> {
    write_inner(data, basis, selected, true)
}

/// Holds the memory publication fence across admission and all component writes.
pub struct PublicationLock {
    _held: crate::task_sync::file_lock::HeldLock,
    git_directory: std::path::PathBuf,
}

impl PublicationLock {
    pub(in crate::task_sync) fn validate(&self, data: &DataWorktree) -> Result<()> {
        ensure!(
            self.git_directory == data.git_directory,
            "memory publication fence belongs to another checkout"
        );
        Ok(())
    }
}

/// Acquires the checkout's memory publication fence.
///
/// # Errors
/// Refuses a concurrent publication or an unsafe Git admin path.
pub fn publication_lock(data: &DataWorktree) -> Result<PublicationLock> {
    let admin = PrivateDirectory::open(&data.git_directory)?;
    let lock = match admin.open_file(Path::new("memory-sync.lock"), true, false) {
        Ok(file) => file,
        Err(error) => {
            if error
                .downcast_ref::<std::io::Error>()
                .is_none_or(|e| e.kind() != std::io::ErrorKind::NotFound)
            {
                return Err(error);
            }
            admin.open_file(Path::new("memory-sync.lock"), true, true)?
        }
    };
    Ok(PublicationLock {
        _held: crate::task_sync::file_lock::HeldLock::try_acquire(lock)?,
        git_directory: data.git_directory.clone(),
    })
}

fn write_inner(
    data: &DataWorktree,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
    recovering: bool,
) -> Result<()> {
    let held = publication_lock(data)?;
    write_locked(data, basis, selected, recovering, &held)
}

/// Publishes under a fence already held by the encompassing export.
///
/// # Errors
/// Refuses changed basis, invalid content or unsafe filesystem operations.
pub fn write_locked(
    data: &DataWorktree,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
    recovering: bool,
    fence: &PublicationLock,
) -> Result<()> {
    fence.validate(data)?;
    data.revalidate()?;
    let (tasks, task_bytes) =
        crate::task_sync::directory::read_resolved_data_folder_for_memory_recovery(data)?;
    write_admitted(
        data,
        basis,
        selected,
        recovering,
        fence,
        &tasks.tree.project.project_id,
        task_bytes,
    )
}

/// [`write_locked`] for an export whose task route already admitted the
/// folder: it gives the folder's project and the bytes its task files hold,
/// so no task file is read again.
///
/// # Errors
/// As [`write_locked`].
pub fn write_admitted(
    data: &DataWorktree,
    basis: &BTreeMap<String, KeyHistory>,
    selected: &BTreeMap<String, KeyHistory>,
    recovering: bool,
    fence: &PublicationLock,
    project: &crate::ProjectId,
    task_bytes: usize,
) -> Result<()> {
    fence.validate(data)?;
    let root = PrivateDirectory::open(&data.select_folder(&data.checkout.join(".engram"))?)?;
    preflight_component(data, basis, selected, recovering, project, Some(task_bytes))?;
    for (key, old) in basis {
        old.validate()?;
        let new = selected
            .get(key)
            .context("memory export cannot omit a selected key")?;
        if matches!(old.head, Head::Tombstone { .. }) {
            ensure!(old == new, "permanent tombstone cannot be replaced");
        }
    }
    let mut total = 0usize;
    for (key, history) in selected {
        history.validate()?;
        ensure!(key == &history.key, "memory map key mismatch");
        let head = serde_json::to_vec_pretty(&HeadDocument {
            format: FORMAT.into(),
            project_id: history.project_id.clone(),
            key: key.clone(),
            head: history.head.clone(),
        })?;
        ensure!(head.len() <= FILE_LIMIT, "memory head exceeds 1 MiB");
        total = total
            .checked_add(head.len())
            .context("memory head byte overflow")?;
        for revision in history.revisions.values() {
            let bytes = serde_json::to_vec_pretty(revision)?;
            ensure!(bytes.len() <= FILE_LIMIT, "memory revision exceeds 1 MiB");
            total = total
                .checked_add(bytes.len())
                .context("memory tree size overflow")?;
        }
    }
    ensure!(total <= TREE_LIMIT, "memory tree exceeds 128 MiB");
    ensure!(
        selected
            .values()
            .chain(basis.values())
            .all(|history| history.project_id == *project),
        "memory publication belongs to another project"
    );
    ensure!(
        total
            .checked_add(task_bytes)
            .is_some_and(|bytes| bytes <= TREE_LIMIT),
        "combined task and memory tree exceeds 128 MiB"
    );
    let cleanup = if recovering {
        recovery::validate(&root, basis, selected)?
    } else {
        ensure!(
            read_root(&root)? == *basis,
            crate::task_sync::TaskSyncRefusal::MemoryExportComponentChanged.invalid("memory_export_component_changed: selected memory export basis changed; an interrupted publication requires --memory-recovery-basis")
        );
        Vec::new()
    };
    let memories = optional_child(&root, Path::new("memories"))?;
    #[cfg(test)]
    if let Some(hook) = BEFORE_MEMORY_WRITE.with(std::cell::Cell::take) {
        hook();
    }
    data.check_head()?;
    recovery::remove_staging(&root, cleanup)?;
    let memories = match memories {
        Some(child) => child,
        None => root.create_child(Path::new("memories"))?,
    };
    for (key, history) in selected {
        let directory = match memories.child(Path::new(key)) {
            Ok(child) => child,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                memories.create_child(Path::new(key))?
            }
            Err(error) => return Err(error),
        };
        for (id, revision) in &history.revisions {
            let name = format!("{id}.json");
            let bytes = serde_json::to_vec_pretty(revision)?;
            ensure!(bytes.len() <= FILE_LIMIT, "memory revision exceeds 1 MiB");
            if let Some(existing) = directory.read_bounded(Path::new(&name), FILE_LIMIT)? {
                ensure!(existing == bytes, "immutable shared revision bytes changed");
            } else {
                let pending = format!("revision-{id}.pending");
                let mut file = directory.open_file(Path::new(&pending), true, true)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                drop(file);
                directory
                    .dir()
                    .hard_link(&pending, directory.dir(), &name)?;
                directory.dir().remove_file(&pending)?;
            }
        }
        let head = serde_json::to_vec_pretty(&HeadDocument {
            format: FORMAT.into(),
            project_id: history.project_id.clone(),
            key: key.clone(),
            head: history.head.clone(),
        })?;
        let temporary = format!("head-{}.pending", uuid::Uuid::new_v4());
        let mut file = directory.open_file(Path::new(&temporary), true, true)?;
        file.write_all(&head)?;
        file.sync_all()?;
        drop(file);
        directory
            .dir()
            .rename(&temporary, directory.dir(), "head.json")?;
        // Only old, validated selected revision files are removed. Their Git
        // history and original canonical local histories remain unchanged.
        if let Some(old) = basis.get(key) {
            for id in old
                .revisions
                .keys()
                .filter(|id| !history.revisions.contains_key(*id))
            {
                match directory.dir().remove_file(format!("{id}.json")) {
                    Ok(()) => {}
                    Err(error) if recovering && error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    ensure!(
        read_root(&root)? == *selected,
        "memory publication did not match selected content"
    );
    Ok(())
}
