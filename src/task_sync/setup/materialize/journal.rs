//! Complete private operation record and physically pinned publication sources.

use super::super::{
    Attribution, Basis,
    files::{Directory, MAX_PRIVATE_BYTES, PhysicalId, file_identity},
    repository,
};
use super::{
    MaterializeSelection,
    candidate::{self, Documents},
    replacement,
};
use crate::task_sync::{notes::NoteOmissions, progress::PreparedCandidate};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Write, path::Path};

const FORMAT: &str = "engram-task-sync-materialize-v1";

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Completion {
    operation: uuid::Uuid,
    public: PhysicalId,
    tasks: PhysicalId,
    project: PhysicalId,
    pub equal_unowned_empty_tasks: bool,
}

impl Completion {
    pub fn observe(
        root: &Directory,
        operation: uuid::Uuid,
        equal_unowned_empty_tasks: bool,
    ) -> Result<Self> {
        let public = root.child(Path::new(".engram"))?;
        Ok(Self {
            operation,
            public: public.identity()?,
            tasks: public.child(Path::new("tasks"))?.identity()?,
            project: file_identity(&public.open_file(Path::new("project.json"), false, false)?)?,
            equal_unowned_empty_tasks,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub format: String,
    pub operation: uuid::Uuid,
    pub selection: MaterializeSelection,
    pub basis: Basis,
    pub attribution: Attribution,
    pub index: Vec<u8>,
    pub root: PhysicalId,
    pub admin: PhysicalId,
    pub private: PhysicalId,
    pub completion_recovery: PhysicalId,
    pub completion_source: PhysicalId,
    pub existing_engram: Option<PhysicalId>,
    pub stage: PhysicalId,
    pub stage_tasks: PhysicalId,
    pub before: Vec<Option<PhysicalId>>,
    pub replacements: Vec<replacement::PlanBinding>,
    pub displaced_paths: Vec<Option<String>>,
    pub documents_fingerprint: String,
    pub omissions: NoteOmissions,
    pub activity_omissions: NoteOmissions,
    pub omitted_worktrees: usize,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn initialize(
    private: &Directory,
    root: &Directory,
    admin: &Directory,
    basis: Basis,
    selection: MaterializeSelection,
    attribution: Attribution,
    prepared: PreparedCandidate,
    index: Vec<u8>,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<Record> {
    let existing_engram = match root.dir().symlink_metadata(".engram") {
        Ok(_) => Some(root.child(Path::new(".engram"))?.identity()?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let raw = private.create_child(Path::new("raw"))?;
    candidate::write(&raw, &prepared.documents)?;
    let pins = private.create_child(Path::new("pins"))?;
    candidate::write(&pins, &prepared.documents)?;
    let stage = private.create_child(Path::new("stage"))?;
    candidate::link_tree(&pins, &stage, &prepared.documents)?;
    let after = private.create_child(Path::new("after"))?;
    let before = private.create_child(Path::new("before"))?;
    let completion_recovery = private.create_child(Path::new("completion-recovery"))?;
    let completion_source = completion_recovery.open_file(Path::new("source"), true, true)?;
    completion_source.sync_all()?;
    let mut before_ids = Vec::new();
    for edit in &basis.edits {
        ensure!(
            root.read(Path::new(&edit.path))? == edit.before,
            "setup before bytes changed during admission"
        );
        let mut file = after.open_file(Path::new(&edit.path), true, true)?;
        file.write_all(&edit.after)?;
        file.sync_all()?;
        if edit.before.is_some() {
            root.dir().hard_link(&edit.path, before.dir(), &edit.path)?;
            let file = before.open_file(Path::new(&edit.path), false, false)?;
            before_ids.push(Some(file_identity(&file)?));
            ensure!(
                before.read(Path::new(&edit.path))? == edit.before,
                "setup before bytes changed while pinning"
            );
        } else {
            before_ids.push(None);
        }
    }
    let replacements = before_ids
        .iter()
        .enumerate()
        .map(|(index, before)| replacement::prepare(private, index, *before, checkpoint))
        .collect::<Result<Vec<_>>>()?;
    let worktrees = repository::git(&basis.repository, &["worktree", "list", "--porcelain"])?;
    let omitted_worktrees = worktrees
        .split(|byte| *byte == b'\n')
        .filter(|line| line.starts_with(b"worktree "))
        .count()
        .saturating_sub(1);
    let displaced_paths = basis
        .edits
        .iter()
        .enumerate()
        .map(|(index, edit)| {
            edit.before
                .as_ref()
                .map(|_| format!("replace-{index}/displaced"))
        })
        .collect();
    let record = Record {
        format: FORMAT.into(),
        operation: uuid::Uuid::now_v7(),
        selection,
        basis,
        attribution,
        index,
        root: root.identity()?,
        admin: admin.identity()?,
        private: private.identity()?,
        completion_recovery: completion_recovery.identity()?,
        completion_source: file_identity(&completion_source)?,
        existing_engram,
        stage: stage.identity()?,
        stage_tasks: stage.child(Path::new("tasks"))?.identity()?,
        before: before_ids,
        replacements,
        displaced_paths,
        documents_fingerprint: candidate::fingerprint(&prepared.documents),
        omissions: prepared.provenance.omissions,
        activity_omissions: prepared.provenance.activity_omissions,
        omitted_worktrees,
    };
    repository::recheck(&record.basis, &record.index)?;
    let mut bytes = serde_json::to_vec(&record)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() <= MAX_PRIVATE_BYTES,
        "materialization journal exceeded byte bound"
    );
    publish_bytes(private, "journal.json", &bytes, checkpoint)?;
    Ok(record)
}

pub(super) fn read(private: &Directory) -> Result<Record> {
    check_inventory(private)?;
    let bytes = private
        .read(Path::new("journal.json"))?
        .context("incomplete private materialization attempt; preserve it")?;
    let pending = private.open_file(Path::new(".journal.json.pending"), false, false)?;
    let final_file = private.open_file(Path::new("journal.json"), false, false)?;
    ensure!(
        file_identity(&pending)? == file_identity(&final_file)?,
        "private journal publication identity changed"
    );
    let record: Record = crate::task_sync::parse(&bytes)?;
    ensure!(
        record.format == FORMAT
            && record.before.len() == 2
            && record.replacements.len() == 2
            && record.displaced_paths
                == record
                    .basis
                    .edits
                    .iter()
                    .enumerate()
                    .map(|(index, edit)| {
                        edit.before
                            .as_ref()
                            .map(|_| format!("replace-{index}/displaced"))
                    })
                    .collect::<Vec<_>>()
            && record.private == private.identity()?,
        "invalid materialization journal binding"
    );
    Ok(record)
}

pub(super) fn is_complete(
    private: &Directory,
    root: &Directory,
    operation: uuid::Uuid,
) -> Result<bool> {
    let Some(bytes) = private.read(Path::new("complete"))? else {
        return Ok(false);
    };
    let completed: Completion = crate::task_sync::parse(&bytes)?;
    ensure!(
        completed == Completion::observe(root, operation, completed.equal_unowned_empty_tasks)?,
        "completed public directory or file identity changed"
    );
    let final_file = private.open_file(Path::new("complete"), false, false)?;
    super::completion::check_published(private, &bytes, file_identity(&final_file)?)?;
    Ok(true)
}

pub(super) fn check_inventory(private: &Directory) -> Result<()> {
    let allowed = [
        "raw",
        "pins",
        "stage",
        "after",
        "before",
        "journal.json",
        ".journal.json.pending",
        "complete",
        ".complete.pending",
        "completion-recovery",
        "replace-0",
        "replace-1",
    ];
    for name in private.names(allowed.len())? {
        ensure!(
            name.to_str().is_some_and(|name| allowed.contains(&name)),
            "unknown private materialization entry; preserve it"
        );
    }
    Ok(())
}

pub(super) fn check_sources(
    private: &Directory,
    record: &Record,
    documents: &Documents,
) -> Result<()> {
    check_inventory(private)?;
    super::completion::check_source(private, record)?;
    ensure!(
        candidate::read(&private.child(Path::new("pins"))?)? == *documents,
        "publication source bytes changed; preserve public and private state"
    );
    let after = private.child(Path::new("after"))?;
    ensure!(
        after.names(2)? == [".gitattributes", ".gitignore"].map(std::ffi::OsString::from),
        "setup after-source inventory changed"
    );
    let before = private.child(Path::new("before"))?;
    let expected = record
        .basis
        .edits
        .iter()
        .filter(|edit| edit.before.is_some())
        .map(|edit| std::ffi::OsString::from(&edit.path))
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        before
            .names(2)?
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            == expected,
        "setup before-source inventory changed"
    );
    for (index, edit) in record.basis.edits.iter().enumerate() {
        replacement::check_attempt(private, record, index)?;
        ensure!(
            after.read(Path::new(&edit.path))?.as_ref() == Some(&edit.after),
            "setup after-source bytes changed"
        );
        if let Some(id) = record.before[index] {
            ensure!(
                file_identity(&before.open_file(Path::new(&edit.path), false, false)?)? == id,
                "setup before-source identity changed"
            );
        }
    }
    Ok(())
}

/// Files stay inside an exclusively created, physically bound operation directory.
/// The pending source remains as the no-replace publication identity witness.
fn publish_bytes(
    directory: &Directory,
    name: &str,
    bytes: &[u8],
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let pending_name = format!(".{name}.pending");
    match directory.dir().symlink_metadata(&pending_name) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut pending = directory.open_file(Path::new(&pending_name), true, true)?;
            checkpoint("journal-pending-created")?;
            let split = bytes.len() / 2;
            pending.write_all(&bytes[..split])?;
            checkpoint("journal-pending-prefix")?;
            pending.write_all(&bytes[split..])?;
            pending.sync_all()?;
            checkpoint("journal-pending-synced")?;
        }
        Ok(_) => ensure!(
            directory.read(Path::new(&pending_name))?.as_deref() == Some(bytes),
            "private publication pending bytes differ; preserve them"
        ),
        Err(error) => return Err(error.into()),
    }
    directory
        .dir()
        .hard_link(&pending_name, directory.dir(), name)?;
    Ok(())
}
