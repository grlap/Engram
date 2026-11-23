//! Complete private operation record and its retained publication sources.
//!
//! A journal written while physical file identities were still recorded is
//! read as before: those extra fields are ignored, never compared.

use super::super::{
    Attribution, Basis,
    files::{Directory, MAX_PRIVATE_BYTES},
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

const FORMAT: &str = "engram-task-sync-materialize-v3";

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Completion {
    pub operation: uuid::Uuid,
    pub equal_unowned_empty_tasks: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Record {
    pub format: String,
    pub operation: uuid::Uuid,
    pub selection: MaterializeSelection,
    pub basis: Basis,
    pub attribution: Attribution,
    pub index: Vec<u8>,
    /// Whether a private `.engram` directory existed at admission.
    #[serde(deserialize_with = "present")]
    pub existing_engram: bool,
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
    basis: Basis,
    selection: MaterializeSelection,
    attribution: Attribution,
    prepared: PreparedCandidate,
    publication: &Documents,
    index: Vec<u8>,
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<Record> {
    let existing_engram = match root.dir().symlink_metadata(".engram") {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let raw = private.create_child(Path::new("raw"))?;
    candidate::write(&raw, &prepared.documents)?;
    let pins = private.create_child(Path::new("pins"))?;
    candidate::write(&pins, publication)?;
    let stage = private.create_child(Path::new("stage"))?;
    candidate::link_tree(&pins, &stage, publication)?;
    let after = private.create_child(Path::new("after"))?;
    let before = private.create_child(Path::new("before"))?;
    let completion_recovery = private.create_child(Path::new("completion-recovery"))?;
    let completion_source = completion_recovery.open_file(Path::new("source"), true, true)?;
    completion_source.sync_all()?;
    drop(completion_source);
    let shared = existing_engram
        .then(|| root.child(Path::new(".engram")))
        .transpose()?;
    for edit in &basis.edits {
        ensure!(
            shared
                .as_ref()
                .map(|directory| directory.read(Path::new(&edit.path)))
                .transpose()?
                .flatten()
                == edit.before,
            "setup before bytes changed during admission"
        );
        let mut file = after.open_file(Path::new(&edit.path), true, true)?;
        file.write_all(&edit.after)?;
        file.sync_all()?;
        after.dir().hard_link(&edit.path, stage.dir(), &edit.path)?;
        if edit.before.is_some() {
            shared
                .as_ref()
                .context("setup policy parent missing")?
                .dir()
                .hard_link(&edit.path, before.dir(), &edit.path)?;
            ensure!(
                before.read(Path::new(&edit.path))? == edit.before,
                "setup before bytes changed while retaining them"
            );
        }
    }
    for index in 0..basis.edits.len() {
        replacement::prepare(private, index, checkpoint)?;
    }
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
        existing_engram,
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

pub(super) fn incomplete_attempt_guidance(private_path: &Path) -> String {
    format!(
        "incomplete private materialization attempt at {}; preserve the whole attempt, retain and reconcile unrelated public edits separately, move this attempt aside within Git administration, then rerun setup preparation and materialization; do not delete or adopt it",
        private_path.display()
    )
}

pub(super) fn read(private: &Directory, private_path: &Path) -> Result<Record> {
    check_inventory(private)?;
    let bytes = private
        .read(Path::new("journal.json"))?
        .with_context(|| incomplete_attempt_guidance(private_path))?;
    let record: Record = crate::task_sync::parse(&bytes)?;
    ensure!(
        record.format == FORMAT
            && record.basis.edits.len() == 2
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
                    .collect::<Vec<_>>(),
        "invalid materialization journal binding"
    );
    Ok(record)
}

/// Reads `existing_engram` as written now, a flag, and as an older journal
/// wrote it, the directory's identity or null.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Bool(present) => present,
        serde_json::Value::Null => false,
        _ => true,
    })
}

pub(super) fn is_complete(private: &Directory, operation: uuid::Uuid) -> Result<bool> {
    let Some(bytes) = private.read(Path::new("complete"))? else {
        return Ok(false);
    };
    let completed: Completion = crate::task_sync::parse(&bytes)?;
    ensure!(
        completed.operation == operation,
        "completion record belongs to another materialization"
    );
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
    super::completion::check_source(private)?;
    ensure!(
        candidate::read_publication(&private.child(Path::new("pins"))?)? == *documents,
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
        replacement::check_attempt(private, index)?;
        ensure!(
            after.read(Path::new(&edit.path))?.as_ref() == Some(&edit.after),
            "setup after-source bytes changed"
        );
        if edit.before.is_some() {
            ensure!(
                before.read(Path::new(&edit.path))? == edit.before,
                "setup before-source bytes changed"
            );
        }
    }
    Ok(())
}

/// Files stay inside the exclusively created operation directory. The pending
/// source is linked to its final name, so publication never replaces a file.
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
