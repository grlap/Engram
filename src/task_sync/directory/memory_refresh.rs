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
use std::io::{Read, Seek, SeekFrom, Write};

pub(super) const RESTORE: &str = "preserve the interrupted folder first; restore tracked .engram/tasks, .engram/project.json and .engram/memories from HEAD (git restore --source=HEAD --worktree -- .engram); inspect and remove only the new task/memory files and project-*.pending fences from that interrupted export, then refresh again; preserve all unrecognized files separately";

/// Transient inputs captured together by the ordinary export service.
pub struct MemoryTaskExport<'a> {
    pub tree: &'a TaskTree,
    pub profile: &'a ExportProfile,
    pub basis: &'a BTreeMap<String, KeyHistory>,
    pub selected: &'a BTreeMap<String, KeyHistory>,
    pub recovery_basis: Option<&'a Path>,
}

fn refusal(code: TaskSyncRefusal, check: &str, detail: impl std::fmt::Display) -> anyhow::Error {
    let remedy = if code == TaskSyncRefusal::MemoryTaskExportIndexChanged {
        "commit the intended staged data or unstage it to match HEAD, then restart refresh"
    } else if matches!(
        code,
        TaskSyncRefusal::MemoryTaskExportRecoveryRequired
            | TaskSyncRefusal::MemoryTaskExportRecoveryMismatch
    ) {
        RESTORE
    } else {
        "preserve the folder and index; import or reconcile edits before refresh; interrupted publication requires --memory-recovery-basis RETAINED/.engram; verify a committed eol=lf policy for JSON when checkout line endings differ"
    };
    code.invalid(format!("check={check}; {detail}; {remedy}"))
        .into()
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
            "check=data_head; captured={}; observed={observed}; preserve output and restart admission against the intended imported commit", data.head.as_deref().unwrap_or("unborn"))));
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
        ensure!(
            result.len() < MAX_TASKS,
            refusal(
                TaskSyncRefusal::MemoryTaskExportComponentChanged,
                "auxiliary_entry_limit",
                format_args!("observed={}; limit={MAX_TASKS}", result.len())
            )
        );
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
                refusal(
                    TaskSyncRefusal::MemoryTaskExportComponentChanged,
                    "auxiliary_file_type",
                    format_args!("path={relative:?}; ordinary_file=false")
                )
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
    for batch in present.chunks(1024) {
        require_identity_batch(data, batch, expected, code)?;
    }
    Ok(())
}

fn require_identity_batch(
    data: &DataWorktree,
    present: &[String],
    expected: &BTreeMap<String, String>,
    code: TaskSyncRefusal,
) -> Result<()> {
    let mut input = Vec::new();
    for path in present {
        ensure!(
            !path.contains(['\n', '\r', '"', '\\']),
            refusal(
                code,
                "component_path",
                format_args!("path={path:?}; stdin_path_safe=false")
            )
        );
        input.extend_from_slice(format!(".engram/{path}\n").as_bytes());
    }
    // --no-filters neither executes configured clean filters nor writes objects.
    let output = git::query(
        &data.checkout,
        &["hash-object", "--no-filters", "--stdin-paths"],
        false,
        Some(&input),
        QUERY_BOUND,
    )?;
    let identities: Vec<_> = std::str::from_utf8(&output)?.lines().collect();
    ensure!(
        identities.len() == present.len(),
        refusal(
            code,
            "component_identity_count",
            format_args!("expected={}; observed={}", present.len(), identities.len())
        )
    );
    for (path, observed) in present.iter().zip(identities) {
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                expected.is_empty(),
                refusal(
                    TaskSyncRefusal::MemoryExportComponentMissing,
                    "memory_directory",
                    format_args!("present=false; committed_files={}", expected.len())
                )
            );
        }
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
    super::admit_limits_with(&documents, final_limits())?;
    Ok((
        documents,
        FolderExchange {
            tree: request.tree.clone(),
            profile: request.profile.clone(),
            export_basis,
        },
    ))
}

fn memory_validation(error: anyhow::Error) -> anyhow::Error {
    if error
        .downcast_ref::<crate::storage::StoreError>()
        .is_some_and(|error| !matches!(error, crate::storage::StoreError::InvalidWork(_)))
        || error.downcast_ref::<std::io::Error>().is_some()
    {
        return error;
    }
    refusal(
        TaskSyncRefusal::MemoryTaskExportRecoveryMismatch,
        "memory_publication_validation",
        format_args!("accepted=false; reason={error}"),
    )
}

fn raw_tasks(path: &Path, intended: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    raw_tasks_with_limits(path, intended, union_limits())
}

fn final_limits() -> super::Limits {
    #[cfg(test)]
    if let Some(limits) = TEST_LIMITS.with(std::cell::Cell::get) {
        return limits;
    }
    LIMITS
}

fn union_limits() -> super::Limits {
    let limits = final_limits();
    super::Limits {
        tasks: limits.tasks * 2,
        tree_bytes: limits.tree_bytes * 2,
        ..limits
    }
}

fn raw_tasks_with_limits(
    path: &Path,
    intended: &[u8],
    limits: super::Limits,
) -> Result<BTreeMap<String, Vec<u8>>> {
    Ok(super::read_documents_with_memory_mode(
        path,
        limits,
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
            "path={path:?}; present={}; observed_bytes={}; expected=admitted_component; checkout_line_endings={}",
            observed.is_some(),
            observed.map_or(0, Vec::len),
            if observed.is_some_and(|bytes| bytes.windows(2).any(|pair| pair == b"\r\n")) {
                "CRLF; commit an eol=lf policy for JSON and restore the checkout from HEAD before retrying"
            } else {
                "no_CRLF"
            }
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

fn write_bytes(
    data: &DataWorktree,
    root: &PrivateDirectory,
    name: &Path,
    expected: Option<&Vec<u8>>,
    bytes: &[u8],
) -> Result<()> {
    require_index(data)?;
    let code = TaskSyncRefusal::MemoryTaskExportComponentChanged;
    let mut file = root
        .open_file(name, true, expected.is_none())
        .map_err(|error| {
            if error.downcast_ref::<std::io::Error>().is_some_and(|e| {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::AlreadyExists
                )
            }) {
                refusal(
                    code,
                    "task_mutation_open",
                    format_args!(
                        "path={name:?}; expected_present={}; opened=false",
                        expected.is_some()
                    ),
                )
            } else {
                error
            }
        })?;
    let mut observed = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut observed)?;
    ensure!(
        expected.is_none_or(|bytes| bytes == &observed)
            && (expected.is_some() || observed.is_empty()),
        task_mismatch(code, &name.to_string_lossy(), Some(&observed))
    );
    require_index(data)?;
    file.seek(SeekFrom::Start(0))?;
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
    drop(file);
    root.sync()?;
    #[cfg(test)]
    checkpoint("fence_directory_synced")?;
    Ok(name)
}

fn recheck_inventory(
    data: &DataWorktree,
    expected: &BTreeMap<String, Vec<u8>>,
    desired: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    revalidate(data)?;
    require_index(data)?;
    let current = raw_tasks(&data.checkout.join(".engram"), &desired["project.json"])?;
    if current != *expected {
        let path = current
            .keys()
            .chain(expected.keys())
            .find(|path| current.get(*path) != expected.get(*path))
            .context("changed inventory path missing")?;
        return Err(task_mismatch(
            TaskSyncRefusal::MemoryTaskExportComponentChanged,
            path,
            current.get(path),
        ));
    }
    Ok(())
}

fn write_tasks(
    data: &DataWorktree,
    root: &PrivateDirectory,
    admitted: &BTreeMap<String, Vec<u8>>,
    desired: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let mut expected = admitted.clone();
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
    #[cfg(test)]
    checkpoint("before_task_mutation")?;
    recheck_inventory(data, &expected, desired)?;
    write_desired_tasks(data, &tasks, &mut expected, desired)?;
    #[cfg(test)]
    checkpoint("before_task_removal")?;
    // Bound enumeration by old + intended, and never delete an unadmitted name.
    recheck_inventory(data, &expected, desired)?;
    let removals: Vec<_> = expected
        .keys()
        .filter(|path| path.starts_with("tasks/") && !desired.contains_key(*path))
        .cloned()
        .collect();
    for path in removals {
        let name = Path::new(&path[6..]);
        require_index(data)?;
        let observed = tasks.read_bounded(name, usize::try_from(MAX_FILE_BYTES)?)?;
        ensure!(
            observed.as_ref() == expected.get(&path),
            task_mismatch(
                TaskSyncRefusal::MemoryTaskExportComponentChanged,
                &path,
                observed.as_ref()
            )
        );
        tasks.dir().remove_file(name)?;
        expected.remove(&path);
        #[cfg(test)]
        checkpoint("task_removed")?;
    }
    tasks.sync()?;
    #[cfg(test)]
    checkpoint("task_entries_synced")?;
    let temporary = stage_envelope(root, &desired["project.json"])?;
    #[cfg(test)]
    checkpoint("before_project_rename")?;
    recheck_inventory(data, &expected, desired)?;
    root.dir().rename(&temporary, root.dir(), "project.json")?;
    root.sync()?;
    #[cfg(test)]
    checkpoint("project_entry_synced")?;
    #[cfg(test)]
    checkpoint("project_published")?;
    Ok(())
}

fn write_desired_tasks(
    data: &DataWorktree,
    tasks: &PrivateDirectory,
    expected: &mut BTreeMap<String, Vec<u8>>,
    desired: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for (path, bytes) in desired
        .iter()
        .filter(|(path, _)| path.starts_with("tasks/"))
    {
        let name = Path::new(&path[6..]);
        let observed = tasks.read_bounded(name, usize::try_from(MAX_FILE_BYTES)?)?;
        ensure!(
            observed.as_ref() == expected.get(path),
            task_mismatch(
                TaskSyncRefusal::MemoryTaskExportComponentChanged,
                path,
                observed.as_ref()
            )
        );
        if observed.as_ref() != Some(bytes) {
            write_bytes(data, tasks, name, expected.get(path), bytes)?;
            expected.insert(path.clone(), bytes.clone());
            #[cfg(test)]
            checkpoint("task_written")?;
        }
    }
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
        )
        .map_err(memory_validation)?;
        revalidate(data)?;
        require_index(data)?;
        return Ok(result);
    }
    let Some(committed) = committed else {
        return publish_initial(data, request, publication);
    };
    admit_refresh(data, request, &root, &committed, &current, &desired)?;
    publish_refresh(
        data,
        request,
        publication,
        &root,
        &committed,
        &current,
        &desired,
    )?;
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
        old_folder.profile.include_notes == request.profile.include_notes,
        refusal(
            TaskSyncRefusal::MemoryTaskExportComponentChanged,
            "task_profile",
            format_args!(
                "committed_include_notes={}; requested_include_notes={}",
                old_folder.profile.include_notes, request.profile.include_notes
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
                format_args!(
                    "head={}; matches=false",
                    data.head.as_deref().unwrap_or("unborn")
                )
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
    )
    .map_err(memory_validation)?;
    Ok(())
}

fn publish_refresh(
    data: &DataWorktree,
    request: &MemoryTaskExport<'_>,
    held: &memory::PublicationLock,
    root: &PrivateDirectory,
    committed: &git::TaskPublicationBasis,
    admitted: &BTreeMap<String, Vec<u8>>,
    desired: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let path = data.select_folder(&data.checkout.join(".engram"))?;
    revalidate(data)?;
    recheck_inventory(data, admitted, desired)?;
    if !pending(root, &desired["project.json"])?
        .iter()
        .any(|(_, full)| *full)
    {
        stage_envelope(root, &desired["project.json"])?;
    }
    #[cfg(test)]
    checkpoint("fence_synced")?;
    write_tasks(data, root, admitted, desired)?;
    memory::write_locked(
        data,
        request.basis,
        request.selected,
        request.recovery_basis.is_some(),
        held,
    )
    .map_err(memory_validation)?;
    #[cfg(test)]
    checkpoint("memories_published")?;
    ensure!(
        &raw_tasks_with_limits(&path, &desired["project.json"], final_limits())? == desired
            && memory::publication_matches(root, request.selected)?,
        refusal(
            TaskSyncRefusal::MemoryTaskExportRecoveryMismatch,
            "final_components",
            "whole_intended_output_matches=false"
        )
    );
    require_auxiliary(data, root, &committed.auxiliary)?;
    revalidate(data)?;
    require_index(data)?;
    root.child(Path::new("tasks"))?.sync()?;
    sync_memories(root)?;
    root.sync()?;
    #[cfg(test)]
    checkpoint("components_synced")?;
    for (name, _) in pending(root, &desired["project.json"])? {
        root.dir().remove_file(name)?;
    }
    root.sync()?;
    #[cfg(test)]
    checkpoint("fence_removal_synced")?;
    Ok(())
}

fn sync_memories(root: &PrivateDirectory) -> Result<()> {
    match root.child(Path::new("memories")) {
        Ok(memories) => {
            for name in memories.names(MAX_TASKS)? {
                memories.child(Path::new(&name))?.sync()?;
            }
            memories.sync()?;
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            ()
        }
        Err(error) => return Err(error),
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
    )
    .map_err(memory_validation)?;
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
    )
    .map_err(memory_validation)?;
    Ok(written)
}

#[cfg(test)]
pub(in crate::task_sync) fn checkpoint(stage: &str) -> Result<()> {
    EVENTS.with(|events| {
        if let Some(events) = events.borrow_mut().as_mut() {
            events.push(stage.to_owned());
        }
    });
    let action = INTERVENTION.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|(name, _)| name == stage) {
            slot.take().map(|(_, action)| action)
        } else {
            None
        }
    });
    if let Some(action) = action {
        action();
    }
    if FAILURE.with(|failure| failure.borrow().as_deref() == Some(stage)) {
        anyhow::bail!("injected combined publication interruption");
    }
    Ok(())
}

#[cfg(test)]
thread_local! { static FAILURE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) }; }

#[cfg(test)]
type Intervention = (String, Box<dyn FnOnce()>);
#[cfg(test)]
thread_local! {
    static TEST_LIMITS: std::cell::Cell<Option<super::Limits>> = const { std::cell::Cell::new(None) };
    static INTERVENTION: std::cell::RefCell<Option<Intervention>> = const { std::cell::RefCell::new(None) };
    static EVENTS: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests;
