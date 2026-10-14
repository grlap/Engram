use super::{ExportCut, ExportProfile, PrepareReceipt, directory, documents_fingerprint, invalid};
use crate::{ObjectId, ProjectId, task_sync::notes::NoteOmissions};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// The largest `state.json` a generation may hold. Every reader refuses a
/// larger one, so a writer must never publish it.
pub(super) const STATE_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Metadata {
    pub sequence: u64,
    pub operation: uuid::Uuid,
    pub predecessor: Option<uuid::Uuid>,
    pub project: ProjectId,
    pub store: PathBuf,
    pub destination: PathBuf,
    pub profile: ExportProfile,
    pub producer: ObjectId,
    pub cut: ExportCut,
    pub documents_fingerprint: String,
    pub path: String,
    pub reason: String,
    pub serialized_tasks: usize,
    pub omissions: NoteOmissions,
    pub activity_omissions: NoteOmissions,
}

pub(super) struct Generation {
    pub directory: PathBuf,
    pub meta: Metadata,
    pub documents: BTreeMap<String, Vec<u8>>,
    pub complete: bool,
}

impl Generation {
    pub(super) fn finish(&self) -> Result<()> {
        let path = self.directory.join("complete");
        if path.exists() {
            if read_bounded(&path, 64)? != self.meta.operation.to_string().as_bytes() {
                return Err(invalid("journal completion does not match pending operation").into());
            }
            return Ok(());
        }
        let mut temporary = tempfile::Builder::new()
            .prefix(".complete-")
            .tempfile_in(&self.directory)?;
        temporary.write_all(self.meta.operation.to_string().as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
        Ok(())
    }

    pub(super) fn receipt(&self, recovered: bool) -> PrepareReceipt {
        PrepareReceipt {
            directory: self.meta.destination.display().to_string(),
            operation: self.meta.operation,
            prepared: true,
            recovered,
            path: self.meta.path.clone(),
            reason: self.meta.reason.clone(),
            project_position: self.meta.cut.position,
            serialized_tasks: self.meta.serialized_tasks,
            omissions: self.meta.omissions.clone(),
            activity_omissions: self.meta.activity_omissions.clone(),
        }
    }
}

pub(super) struct Loaded {
    pub current: Option<Generation>,
    pub next_sequence: u64,
    pub reason: Option<String>,
}

fn generations(directory: &Path) -> Result<Vec<(u64, uuid::Uuid, PathBuf)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 progress entry"))?;
        if name == "prepare.lock" {
            continue;
        }
        if let Some(id) = name.strip_prefix(".staging-") {
            uuid::Uuid::parse_str(id).map_err(|_| invalid("unknown progress entry"))?;
            remove_owned(directory, &entry.path())?;
            continue;
        }
        let tail = name
            .strip_prefix("generation-")
            .ok_or_else(|| invalid("unknown progress entry"))?;
        let (number, id) = tail
            .split_once('-')
            .ok_or_else(|| invalid("invalid journal generation name"))?;
        let number = number
            .parse::<u64>()
            .map_err(|_| invalid("invalid journal sequence"))?;
        let id = uuid::Uuid::parse_str(id).map_err(|_| invalid("invalid journal operation id"))?;
        entries.push((number, id, entry.path()));
        if entries.len() > 2 {
            return Err(
                invalid("progress journal exceeds predecessor and current generations").into(),
            );
        }
    }
    entries.sort_by_key(|entry| entry.0);
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(invalid("duplicate progress sequence").into());
    }
    Ok(entries)
}

pub(super) fn load(directory: &Path) -> Result<Loaded> {
    let entries = generations(directory)?;
    let next_sequence = entries.last().map_or(Ok(1), |entry| {
        entry
            .0
            .checked_add(1)
            .ok_or_else(|| invalid("journal sequence exhausted"))
    })?;
    let Some((sequence, operation, path)) = entries.last() else {
        return Ok(Loaded {
            current: None,
            next_sequence,
            reason: None,
        });
    };
    let current = read_generation(path, *sequence, *operation);
    let current = match current {
        Ok(current) => current,
        Err(error) => {
            return Ok(Loaded {
                current: None,
                next_sequence,
                reason: Some(format!("journal unavailable: {error}")),
            });
        }
    };
    if let Some(predecessor) = current.meta.predecessor
        && entries
            .iter()
            .rev()
            .nth(1)
            .is_none_or(|entry| entry.1 != predecessor || entry.0 + 1 != *sequence)
    {
        return Ok(Loaded {
            current: None,
            next_sequence,
            reason: Some("journal predecessor missing or divergent".into()),
        });
    }
    Ok(Loaded {
        current: Some(current),
        next_sequence,
        reason: None,
    })
}

fn read_generation(path: &Path, sequence: u64, operation: uuid::Uuid) -> Result<Generation> {
    let directory_meta = fs::symlink_metadata(path)?;
    if !directory_meta.is_dir() || directory_meta.file_type().is_symlink() {
        return Err(invalid("journal generation must be an ordinary directory").into());
    }
    let meta: Metadata =
        serde_json::from_slice(&read_bounded(&path.join("state.json"), STATE_LIMIT as u64)?)?;
    if meta.sequence != sequence
        || meta.operation != operation
        || !meta.store.is_absolute()
        || !meta.destination.is_absolute()
        || meta.cut.position < 0
        || (meta.cut.position == 0) != meta.cut.anchor.is_none()
    {
        return Err(invalid("invalid journal binding").into());
    }
    meta.profile.validate()?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".complete-"))
        {
            read_bounded(&entry.path(), 64)?;
            fs::remove_file(entry.path())?;
            continue;
        }
        if !["candidate", "state.json", "complete"]
            .iter()
            .any(|name| entry.file_name() == *name)
        {
            return Err(invalid("unknown journal generation entry").into());
        }
    }
    let documents = directory::read_documents(&path.join("candidate"))?;
    if documents_fingerprint(&documents) != meta.documents_fingerprint {
        return Err(invalid("retained bytes differ from recorded candidate basis").into());
    }
    let complete = match fs::symlink_metadata(path.join("complete")) {
        Ok(_) => {
            if read_bounded(&path.join("complete"), 64)? != operation.to_string().as_bytes() {
                return Err(invalid("invalid completion marker").into());
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    Ok(Generation {
        directory: path.into(),
        meta,
        documents,
        complete,
    })
}

pub(super) fn publish(
    directory: &Path,
    meta: Metadata,
    documents: BTreeMap<String, Vec<u8>>,
) -> Result<Generation> {
    // A generation no reader admits must never be published: check its state
    // before pruning, staging or writing anything. Preparation builds this
    // metadata itself, so an oversized state is an internal defect, not
    // invalid caller input.
    let state_bytes = serde_json::to_vec(&meta)?;
    if state_bytes.len() > STATE_LIMIT {
        anyhow::bail!(
            "task-sync preparation metadata is {} bytes, over Engram's internal {}-byte bound; nothing was published, and the existing progress generations and output are unchanged. This is an Engram defect: report it with this byte count.",
            state_bytes.len(),
            STATE_LIMIT
        );
    }
    let entries = generations(directory)?;
    // Keep the current generation as predecessor before publishing its successor.
    for (_, _, path) in entries.iter().take(entries.len().saturating_sub(1)) {
        remove_owned(directory, path)?;
    }
    let staging = directory.join(format!(".staging-{}", meta.operation));
    fs::create_dir(&staging)?;
    let candidate = staging.join("candidate");
    directory::write_documents(&candidate, &documents)?;
    let mut state = fs::File::create(staging.join("state.json"))?;
    state.write_all(&state_bytes)?;
    state.sync_all()?;
    drop(state);
    let path = directory.join(format!(
        "generation-{:020}-{}",
        meta.sequence, meta.operation
    ));
    let mut pending = tempfile::TempPath::try_from_path(&staging)?;
    pending.disable_cleanup(true);
    pending
        .persist_noclobber(&path)
        .map_err(|error| error.error)?;
    Ok(Generation {
        directory: path,
        meta,
        documents,
        complete: false,
    })
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > limit {
        return Err(invalid("journal requires ordinary bounded files").into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("journal file grew past limit").into());
    }
    Ok(bytes)
}

fn remove_owned(root: &Path, path: &Path) -> Result<()> {
    fn no_links(path: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(invalid("journal cleanup refuses links").into());
        }
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                no_links(&entry?.path())?;
            }
        } else if !meta.is_file() {
            return Err(invalid("journal cleanup refuses special files").into());
        }
        Ok(())
    }
    let absolute = fs::canonicalize(path)?;
    if absolute.parent() != Some(root) {
        return Err(invalid("journal cleanup escaped its directory").into());
    }
    no_links(path).with_context(|| format!("checking cleanup {}", path.display()))?;
    fs::remove_dir_all(path)?;
    Ok(())
}
