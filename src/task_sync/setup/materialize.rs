//! Explicit publication of one prepared candidate; no SQLite or Git mutation.

mod admission;
mod candidate;
mod completion;
mod journal;
mod publication;
mod replacement;

#[cfg(test)]
mod tests;

use super::{Attribution, files::Directory, intent, repository};
use crate::{
    ProjectId,
    task_sync::{notes::NoteOmissions, progress},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Both private operations and the candidate paths are explicitly selected.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializeSelection {
    pub repository: PathBuf,
    pub setup_operation: uuid::Uuid,
    pub prepared_operation: uuid::Uuid,
    pub progress_directory: PathBuf,
    pub candidate_directory: PathBuf,
}

/// Publication accounting; assertions are not authenticated human consent.
#[derive(Debug, Serialize)]
pub struct MaterializeReceipt {
    pub operation: uuid::Uuid,
    pub setup_operation: uuid::Uuid,
    pub prepared_operation: uuid::Uuid,
    pub repeated: bool,
    pub recovered: bool,
    pub equal_unowned_empty_tasks: bool,
    pub attribution: Attribution,
    pub omissions: NoteOmissions,
    pub activity_omissions: NoteOmissions,
    pub omitted_linked_worktrees: usize,
    pub redactor: &'static str,
    pub assurance: &'static str,
}

pub(crate) fn materialize(
    marker: &Path,
    project: &ProjectId,
    selection: &MaterializeSelection,
    enabled: bool,
    attribution: Attribution,
) -> Result<MaterializeReceipt> {
    run(
        marker,
        project,
        selection,
        enabled,
        attribution,
        &mut |_| Ok(()),
    )
}

/// Checkpoints belong to the adapter's test harness, not an operator fault flag.
fn run(
    marker: &Path,
    project: &ProjectId,
    selection: &MaterializeSelection,
    enabled: bool,
    attribution: Attribution,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<MaterializeReceipt> {
    run_with_capacity(
        marker,
        project,
        selection,
        enabled,
        attribution,
        DirectoryCapacity {
            root: 100_000,
            shared: 100_000,
        },
        checkpoint,
    )
}

/// Private per-invocation bounds let tests exercise the production admission path.
#[derive(Clone, Copy)]
struct DirectoryCapacity {
    root: usize,
    shared: usize,
}

fn run_with_capacity(
    marker: &Path,
    project: &ProjectId,
    selection: &MaterializeSelection,
    enabled: bool,
    attribution: Attribution,
    capacity: DirectoryCapacity,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<MaterializeReceipt> {
    attribution.validate()?;
    let admin_path = repository::administration(&selection.repository)?;
    let admin = Directory::open(&admin_path)?;
    // No new state or cleanup before immediate contention refusal.
    let _lock = intent::acquire_existing_lock(&admin)?;
    let setup = intent::read(&admin, selection.setup_operation)?;
    ensure!(
        setup.basis.project_id == *project
            && same_file::is_same_file(&setup.basis.project_file, marker)?
            && same_file::is_same_file(&setup.basis.repository, &selection.repository)?
            && setup.basis.git_directory == admin_path,
        "selected setup project, marker or repository differs"
    );
    let root = Directory::open(&setup.basis.repository)?;
    check_disk_names(&root, &setup.basis.project_file, capacity)?;
    ensure!(
        root.identity()?.same_device(admin.identity()?),
        "materialization requires repository and Git administration on the same filesystem"
    );
    let (private, record, recovered) = admission::select(
        &root,
        &admin,
        setup,
        selection,
        enabled,
        attribution,
        checkpoint,
    )?;
    ensure!(
        root.identity()? == record.root
            && admin.identity()? == record.admin
            && private.identity()? == record.private,
        "recorded materialization directory identity changed"
    );
    repository::recheck(&record.basis, &record.index)?;
    let documents = candidate::read(&private.child(Path::new("raw"))?)?;
    ensure!(
        candidate::fingerprint(&documents) == record.documents_fingerprint,
        "retained raw candidate differs from admitted bytes"
    );
    candidate::disclose(&documents)?;
    journal::check_sources(&private, &record, &documents)?;
    let complete = journal::is_complete(&private, &root, record.operation)?;
    for (index, edit) in record.basis.edits.iter().enumerate() {
        replacement::apply(&root, &private, &record, index, edit, complete, checkpoint)?;
    }
    super::attributes::materialized(&record.basis, &documents)?;
    checkpoint("setup-checked")?;
    let equal_empty = publication::apply(
        &root,
        &private,
        &record,
        &documents,
        complete,
        capacity.shared,
        checkpoint,
    )?;
    repository::recheck(&record.basis, &record.index)?;
    super::attributes::materialized(&record.basis, &documents)?;
    checkpoint("public-checked")?;
    if !complete {
        let completion = journal::Completion::observe(&root, record.operation, equal_empty)?;
        let mut bytes = serde_json::to_vec(&completion)?;
        bytes.push(b'\n');
        completion::publish(&private, &record, &bytes, checkpoint)?;
        checkpoint("complete-published")?;
    }
    Ok(receipt(record, complete, recovered, equal_empty))
}

fn receipt(
    record: journal::Record,
    complete: bool,
    recovered: bool,
    equal_empty: bool,
) -> MaterializeReceipt {
    MaterializeReceipt {
        operation: record.operation,
        setup_operation: record.selection.setup_operation,
        prepared_operation: record.selection.prepared_operation,
        repeated: complete,
        recovered,
        equal_unowned_empty_tasks: equal_empty,
        attribution: record.attribution,
        omissions: record.omissions,
        activity_omissions: record.activity_omissions,
        omitted_linked_worktrees: record.omitted_worktrees,
        redactor: "development no-op; no DLP or authenticated disclosure approval",
        assurance: "operator assertions only; no worktree enrollment, commit or network operation",
    }
}

fn check_disk_names(root: &Directory, marker: &Path, capacity: DirectoryCapacity) -> Result<()> {
    let marker = marker
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("selected marker has no UTF-8 name"))?;
    let selected = [".engram", ".gitignore", ".gitattributes", marker];
    for name in root.names_with_room(capacity.root, &selected[..3])? {
        if let Some(name) = name.to_str()
            && let Some(expected) = selected
                .iter()
                .find(|expected| name.eq_ignore_ascii_case(expected))
        {
            ensure!(
                name == *expected,
                "case alias of materialization path; preserve it"
            );
        }
    }
    match root.dir().symlink_metadata(".engram") {
        Ok(_) => {
            publication::check_reserved_names(&root.child(Path::new(".engram"))?, capacity.shared)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
