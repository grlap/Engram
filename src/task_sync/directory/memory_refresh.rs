//! Combined publication over a committed task component. The existing pending
//! envelope fences mutable task writes until the memory component also validates.

use super::{
    BTreeMap, FolderExchange, FolderProjectDocument, LIMITS, MAX_FILE_BYTES, MAX_TASKS,
    MemoryRecovery, Path, Result, TaskSyncRefusal, TaskTree, prepare_folder_documents, pretty,
};
use crate::task_sync::{
    data_worktree::{DataWorktree, QUERY_BOUND},
    git,
    memories::{KeyHistory, directory as memory},
    progress::ExportProfile,
    setup::PrivateDirectory,
};
use anyhow::{Context, ensure};
use std::io::Write;

/// Transient inputs captured together by the ordinary export service.
pub struct MemoryTaskExport<'a> {
    pub tree: &'a TaskTree,
    pub profile: &'a ExportProfile,
    pub basis: &'a BTreeMap<String, KeyHistory>,
    pub selected: &'a BTreeMap<String, KeyHistory>,
    pub recovery_basis: Option<&'a Path>,
}

fn refusal(code: TaskSyncRefusal, check: &str, detail: impl std::fmt::Display) -> anyhow::Error {
    code.invalid(format!(
        "check={check}; {detail}; preserve the folder and index; import or reconcile edits before refresh; interrupted publication requires --memory-recovery-basis RETAINED/.engram"
    )).into()
}

fn query(data: &DataWorktree, arguments: &[&str]) -> Result<Vec<u8>> {
    git::query(&data.checkout, arguments, false, None, QUERY_BOUND)
}

fn revalidate(data: &DataWorktree) -> Result<()> {
    let observed = git::query(
        &data.checkout,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        true,
        None,
        QUERY_BOUND,
    )?;
    let observed = std::str::from_utf8(&observed)?.trim();
    ensure!(data.head.as_deref().unwrap_or("") == observed,
        TaskSyncRefusal::MemoryExportBasisChanged.invalid(format!(
            "check=data_head; captured={:?}; observed={observed:?}; preserve output and restart admission against the intended imported commit", data.head)));
    data.revalidate()
}

fn require_index(data: &DataWorktree) -> Result<()> {
    if let Some(head) = &data.head {
        let changes = query(
            data,
            &[
                "diff-index",
                "--cached",
                "--raw",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                head,
                "--",
            ],
        )?;
        ensure!(
            changes.is_empty(),
            refusal(
                TaskSyncRefusal::MemoryTaskExportIndexChanged,
                "index_matches_head",
                format_args!("head={head}; observed_diff_bytes={}", changes.len())
            )
        );
    }
    Ok(())
}

fn auxiliary_files(root: &PrivateDirectory, prefix: &str, result: &mut Vec<String>) -> Result<()> {
    for name in root.names(MAX_TASKS)? {
        let name = name.to_str().context("non-UTF8 auxiliary path")?;
        let relative = format!("{prefix}{name}");
        ensure!(result.len() < MAX_TASKS, "auxiliary path bound exceeded");
        let metadata = root.dir().symlink_metadata(name)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            auxiliary_files(
                &root.child(Path::new(name))?,
                &format!("{relative}/"),
                result,
            )?;
        } else {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "auxiliary input must be an ordinary file"
            );
            result.push(relative);
        }
    }
    Ok(())
}

fn require_auxiliary(
    data: &DataWorktree,
    root: &PrivateDirectory,
    expected: &BTreeMap<String, String>,
) -> Result<()> {
    let mut present = Vec::new();
    for name in root.names(MAX_TASKS)? {
        if name == ".gitignore" || name == ".gitattributes" {
            present.push(name.to_str().context("non-UTF8 policy path")?.to_owned());
        } else if name == "evidence" {
            super::evidence::validate(&data.checkout.join(".engram/evidence"))?;
            auxiliary_files(
                &root.child(Path::new("evidence"))?,
                "evidence/",
                &mut present,
            )?;
        }
    }
    ensure!(
        present.len() == expected.len() && present.iter().all(|path| expected.contains_key(path)),
        refusal(
            TaskSyncRefusal::MemoryTaskExportComponentChanged,
            "auxiliary_membership",
            format_args!(
                "expected_files={}; observed_files={}",
                expected.len(),
                present.len()
            )
        )
    );
    require_file_identities(
        data,
        &present,
        expected,
        TaskSyncRefusal::MemoryTaskExportComponentChanged,
    )
}

fn require_file_identities(
    data: &DataWorktree,
    present: &[String],
    expected: &BTreeMap<String, String>,
    code: TaskSyncRefusal,
) -> Result<()> {
    for path in present {
        // --no-filters neither executes configured clean filters nor writes objects.
        let relative = format!(".engram/{path}");
        let observed = query(data, &["hash-object", "--no-filters", "--", &relative])?;
        let observed = std::str::from_utf8(&observed)?.trim();
        ensure!(
            expected[path] == observed,
            refusal(
                code,
                "component_bytes",
                format_args!(
                    "path={path:?}; expected={}; observed={observed}",
                    expected[path]
                )
            )
        );
    }
    Ok(())
}

fn require_memory_basis(
    data: &DataWorktree,
    root: &PrivateDirectory,
    expected: &BTreeMap<String, String>,
) -> Result<()> {
    let mut present = Vec::new();
    match root.dir().symlink_metadata("memories") {
        Ok(_) => auxiliary_files(
            &root.child(Path::new("memories"))?,
            "memories/",
            &mut present,
        )?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let code = TaskSyncRefusal::MemoryExportComponentChanged;
    ensure!(
        present.len() == expected.len() && present.iter().all(|path| expected.contains_key(path)),
        refusal(
            code,
            "memory_membership",
            format_args!(
                "expected_files={}; observed_files={}",
                expected.len(),
                present.len()
            )
        )
    );
    require_file_identities(data, &present, expected, code)
}

fn intended_documents(
    request: &MemoryTaskExport<'_>,
) -> Result<(BTreeMap<String, Vec<u8>>, FolderExchange)> {
    let (mut documents, export_basis) =
        prepare_folder_documents(&request.tree.folder_documents()?, request.profile)?;
    let mut envelope: FolderProjectDocument = super::super::parse(&documents["project.json"])?;
    envelope.memory_profile = Some(crate::task_sync::memories::FORMAT.into());
    documents.insert("project.json".into(), pretty(&envelope)?);
    Ok((
        documents,
        FolderExchange {
            tree: request.tree.clone(),
            profile: request.profile.clone(),
            export_basis,
        },
    ))
}

fn raw_tasks(path: &Path, intended: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    Ok(super::read_documents_with_memory_mode(
        path,
        LIMITS,
        false,
        true,
        &mut |_, _| Ok(()),
        MemoryRecovery::Capability(intended),
    )?
    .0)
}

fn pending(root: &PrivateDirectory, intended: &[u8]) -> Result<Vec<(std::ffi::OsString, bool)>> {
    let mut found = Vec::new();
    for name in root.names(MAX_TASKS)? {
        if name.to_str().is_some_and(super::is_memory_profile_pending) {
            let bytes = root
                .read_bounded(Path::new(&name), usize::try_from(MAX_FILE_BYTES)?)?
                .context("pending envelope disappeared")?;
            ensure!(
                intended.starts_with(&bytes),
                refusal(
                    TaskSyncRefusal::MemoryTaskExportRecoveryMismatch,
                    "pending_envelope_prefix",
                    format_args!(
                        "path={name:?}; intended_bytes={}; observed_bytes={}",
                        intended.len(),
                        bytes.len()
                    )
                )
            );
            found.push((name, bytes == intended));
        }
    }
    Ok(found)
}

fn task_mismatch(code: TaskSyncRefusal, path: &str, observed: Option<&Vec<u8>>) -> anyhow::Error {
    refusal(
        code,
        "task_component_bytes",
        format_args!(
            "path={path:?}; observed_bytes={:?}; expected=committed_or_intended",
            observed.map(Vec::len)
        ),
    )
}

fn validate_recovery(
    old: &BTreeMap<String, Vec<u8>>,
    current: &BTreeMap<String, Vec<u8>>,
    desired: &BTreeMap<String, Vec<u8>>,
    complete_fence: bool,
) -> Result<()> {
    let code = TaskSyncRefusal::MemoryTaskExportRecoveryMismatch;
    let envelope = current
        .get("project.json")
        .ok_or_else(|| task_mismatch(code, "project.json", None))?;
    // Existing capability-only recovery may have complete intended tasks and an
    // envelope missing only the capability. Preserve that supported legacy case.
    let mut capable: FolderProjectDocument = super::super::parse(envelope)
        .map_err(|_| task_mismatch(code, "project.json", Some(envelope)))?;
    let capability_only = capable.memory_profile.is_none();
    capable.memory_profile = Some(crate::task_sync::memories::FORMAT.into());
    let mut legacy = current.clone();
    legacy.insert("project.json".into(), pretty(&capable)?);
    let legacy_complete = capability_only && legacy == *desired;
    ensure!(
        complete_fence || current == old || legacy_complete,
        refusal(
            code,
            "full_envelope_before_task_mutation",
            "full_fence=false; task_component_differs_from_basis_and_complete_legacy_output"
        )
    );
    for (path, bytes) in current {
        let before = old.get(path);
        let after = desired.get(path);
        let allowed = before == Some(bytes)
            || after == Some(bytes)
            || (complete_fence
                && path != "project.json"
                && before != after
                && after.is_some_and(|value| value.starts_with(bytes)))
            || (path == "project.json" && legacy_complete);
        ensure!(allowed, task_mismatch(code, path, Some(bytes)));
    }
    for path in old.keys().filter(|path| desired.contains_key(*path)) {
        ensure!(current.contains_key(path), task_mismatch(code, path, None));
    }
    Ok(())
}

fn write_bytes(root: &PrivateDirectory, name: &Path, bytes: &[u8]) -> Result<()> {
    let exists = root
        .read_bounded(name, usize::try_from(MAX_FILE_BYTES)?)?
        .is_some();
    let mut file = root.open_file(name, true, !exists)?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn stage_envelope(root: &PrivateDirectory, bytes: &[u8]) -> Result<std::ffi::OsString> {
    let name = std::ffi::OsString::from(format!("project-{}.pending", uuid::Uuid::new_v4()));
    let mut file = root.open_file(Path::new(&name), true, true)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(name)
}

fn write_tasks(root: &PrivateDirectory, desired: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let tasks = match root.child(Path::new("tasks")) {
        Ok(tasks) => tasks,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            root.create_child(Path::new("tasks"))?
        }
        Err(error) => return Err(error),
    };
    for (path, bytes) in desired
        .iter()
        .filter(|(path, _)| path.starts_with("tasks/"))
    {
        let name = Path::new(&path[6..]);
        if tasks
            .read_bounded(name, usize::try_from(MAX_FILE_BYTES)?)?
            .as_ref()
            != Some(bytes)
        {
            write_bytes(&tasks, name, bytes)?;
            #[cfg(test)]
            checkpoint("task_written")?;
        }
    }
    for name in tasks.names(MAX_TASKS)? {
        let relative = format!("tasks/{}", name.to_str().context("non-UTF8 task path")?);
        if !desired.contains_key(&relative) {
            tasks.dir().remove_file(&name)?;
            #[cfg(test)]
            checkpoint("task_removed")?;
        }
    }
    let temporary = stage_envelope(root, &desired["project.json"])?;
    root.dir().rename(&temporary, root.dir(), "project.json")?;
    #[cfg(test)]
    checkpoint("project_published")?;
    Ok(())
}

/// Refreshes committed tasks and memories under the caller's publication fence.
///
/// # Errors
/// Refuses dirty components/index, stale basis, invalid recovery or publication I/O.
pub fn publish_memory_export(
    data: &DataWorktree,
    request: &MemoryTaskExport<'_>,
    publication: &memory::PublicationLock,
) -> Result<FolderExchange> {
    publication.validate(data)?;
    revalidate(data)?;
    let (desired, result) = intended_documents(request)?;
    let path = data.select_folder(&data.checkout.join(".engram"))?;
    if !path.exists() {
        return publish_initial(data, request, publication);
    }
    let root = PrivateDirectory::open(&path)?;
    let has_pending = super::memory_profile_recovery_pending(data)?;
    ensure!(
        !has_pending || request.recovery_basis.is_some(),
        refusal(
            TaskSyncRefusal::MemoryTaskExportRecoveryRequired,
            "pending_envelope",
            "observed=true; explicit_recovery=false"
        )
    );
    let current = raw_tasks(&path, &desired["project.json"])?;
    let committed = if let Some(head) = &data.head {
        git::read_task_publication_basis(&data.checkout, &request.tree.project.project_id, head)?
    } else {
        None
    };
    if let Some(committed) = &committed {
        require_index(data)?;
        require_auxiliary(data, &root, &committed.auxiliary)?;
    }
    if !has_pending && current == desired && memory::publication_matches(&root, request.selected)? {
        memory::preflight(
            data,
            request.basis,
            request.selected,
            true,
            request.tree,
            request.profile,
        )?;
        revalidate(data)?;
        return Ok(result);
    }
    let Some(committed) = committed else {
        return publish_initial(data, request, publication);
    };
    admit_refresh(data, request, &root, &committed, &current, &desired)?;
    publish_refresh(data, request, publication, &root, &committed, &desired)?;
    Ok(result)
}

fn admit_refresh(
    data: &DataWorktree,
    request: &MemoryTaskExport<'_>,
    root: &PrivateDirectory,
    committed: &git::TaskPublicationBasis,
    current: &BTreeMap<String, Vec<u8>>,
    desired: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    if request.recovery_basis.is_none() {
        require_memory_basis(data, root, &committed.memory_files)?;
    }
    let old = &committed.documents;
    let old_folder = super::folder_from_documents_profile(old.clone(), false, true)?;
    ensure!(
        old_folder.profile == *request.profile,
        refusal(
            TaskSyncRefusal::MemoryTaskExportComponentChanged,
            "task_profile",
            format_args!(
                "committed={:?}; requested={:?}",
                old_folder.profile, request.profile
            )
        )
    );
    let transient = pending(root, &desired["project.json"])?;
    if let Some(retained) = request.recovery_basis {
        let retained_tasks = raw_tasks(retained, &desired["project.json"])?;
        ensure!(
            retained_tasks == *old,
            refusal(
                TaskSyncRefusal::MemoryTaskExportRecoveryMismatch,
                "retained_tasks_match_head",
                format_args!("head={:?}; matches=false", data.head)
            )
        );
        validate_recovery(
            old,
            current,
            desired,
            transient.iter().any(|(_, full)| *full),
        )?;
    } else if current != old {
        let changed = current
            .keys()
            .chain(old.keys())
            .find(|path| current.get(*path) != old.get(*path))
            .context("changed task path missing")?;
        return Err(task_mismatch(
            TaskSyncRefusal::MemoryTaskExportComponentChanged,
            changed,
            current.get(changed),
        ));
    }
    memory::preflight(
        data,
        request.basis,
        request.selected,
        request.recovery_basis.is_some(),
        request.tree,
        request.profile,
    )?;
    Ok(())
}

fn publish_refresh(
    data: &DataWorktree,
    request: &MemoryTaskExport<'_>,
    held: &memory::PublicationLock,
    root: &PrivateDirectory,
    committed: &git::TaskPublicationBasis,
    desired: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let path = data.select_folder(&data.checkout.join(".engram"))?;
    revalidate(data)?;
    stage_envelope(root, &desired["project.json"])?;
    #[cfg(test)]
    checkpoint("fence_synced")?;
    write_tasks(root, desired)?;
    memory::write_locked(
        data,
        request.basis,
        request.selected,
        request.recovery_basis.is_some(),
        held,
    )?;
    #[cfg(test)]
    checkpoint("memories_published")?;
    ensure!(
        &raw_tasks(&path, &desired["project.json"])? == desired
            && memory::publication_matches(root, request.selected)?,
        refusal(
            TaskSyncRefusal::MemoryTaskExportRecoveryMismatch,
            "final_components",
            "whole_intended_output_matches=false"
        )
    );
    require_auxiliary(data, root, &committed.auxiliary)?;
    revalidate(data)?;
    for (name, _) in pending(root, &desired["project.json"])? {
        root.dir().remove_file(name)?;
    }
    Ok(())
}

fn publish_initial(
    data: &DataWorktree,
    request: &MemoryTaskExport<'_>,
    held: &memory::PublicationLock,
) -> Result<FolderExchange> {
    memory::preflight(
        data,
        request.basis,
        request.selected,
        request.recovery_basis.is_some(),
        request.tree,
        request.profile,
    )?;
    let written = match super::matching_memory_task_export(
        data,
        request.tree,
        request.profile,
        request.recovery_basis.is_some(),
    )? {
        Some(existing) => existing,
        None => super::write_data_folder_with_memories(data, request.tree, request.profile)?,
    };
    if let Some(retained) = request.recovery_basis {
        super::recover_memory_profile_locked(data, retained, held)?;
    }
    super::enable_memory_profile(data)?;
    memory::write_locked(
        data,
        request.basis,
        request.selected,
        request.recovery_basis.is_some(),
        held,
    )?;
    Ok(written)
}

#[cfg(test)]
pub(in crate::task_sync) fn checkpoint(stage: &str) -> Result<()> {
    if FAILURE.with(|failure| failure.borrow().as_deref() == Some(stage)) {
        anyhow::bail!("injected combined publication interruption");
    }
    Ok(())
}

#[cfg(test)]
thread_local! { static FAILURE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) }; }

#[cfg(test)]
mod tests;
