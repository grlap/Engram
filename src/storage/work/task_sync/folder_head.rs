//! The shared task folder's recorded membership.
//!
//! The folder on disk is one membership that both an export and a complete
//! import produce, so one append-only chain per project and export profile
//! records it. Each record names its predecessor and is appended only while
//! that predecessor is still the head, in the writer's transaction, so two
//! racing writers cannot both advance. The heads table is a rebuildable
//! projection of the chain; the doctor compares the two.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{
    CanonicalObject, ObjectId, ProjectId, WorkId,
    storage::{SqliteStore, StoreError},
    task_sync::{
        TaskSyncRefusal,
        inventory::InventoryRoot,
        progress::{ExportCut, ExportProfile},
    },
};

pub(crate) const FOLDER_HEAD_KIND: &str = "task_sync_folder_head";

/// Why a folder membership was recorded.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderHeadReason {
    /// The first export of a profile: a complete export that seeds the chain.
    Bootstrap,
    /// An export that wrote only the tasks changed since the previous head.
    Export,
    /// A complete or partial import of a committed folder.
    Import,
    /// An explicit, named rebuild.
    Rebuild,
}

/// One recorded folder membership. The root is the folder as it is on disk,
/// including the other host's bytes of every pending task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FolderHead {
    pub project_id: ProjectId,
    pub profile: ExportProfile,
    pub previous: Option<ObjectId>,
    pub reason: FolderHeadReason,
    /// The local project-feed cut this membership reflects.
    pub cut: ExportCut,
    /// The folder on disk.
    pub inventory_root: InventoryRoot,
    pub task_count: u64,
    /// The last data commit an import applied completely, the next import's
    /// diff base. A partial import keeps the earlier one, so its held tasks
    /// return; an export keeps it, since its own commit is made by the caller.
    pub source: Option<FolderSource>,
    /// The data commit whose content the folder holds, when an import left
    /// it so; an export's own files are not yet committed, so it records none
    /// and its envelope identifies the folder instead.
    pub folder_commit: Option<String>,
    /// Held tasks whose folder bytes are another host's, never claimed as ours.
    pub pending: BTreeSet<WorkId>,
}

/// A data commit and the membership root verified for it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FolderSource {
    pub commit: String,
    pub inventory_root: InventoryRoot,
    pub task_count: u64,
}

/// One planned folder export, read on one snapshot.
#[derive(Clone, Debug)]
pub struct FolderExportPlan {
    /// The head this export was planned against; none for the bootstrap.
    pub head: Option<(ObjectId, FolderHead)>,
    /// The project-feed cut the documents were captured at.
    pub cut: ExportCut,
    pub project: crate::task_sync::ProjectDocument,
    /// For the bootstrap every exported task; otherwise only the tasks that
    /// changed since the head, `None` removing one. Pending tasks never appear.
    pub changes: BTreeMap<WorkId, Option<crate::task_sync::TaskDocument>>,
    pub omissions: crate::task_sync::notes::NoteOmissions,
    pub activity_omissions: crate::task_sync::notes::NoteOmissions,
    /// Whether `changes` holds every exported task: the bootstrap or a rebuild.
    pub complete: bool,
}

pub(in crate::storage::work) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_folder_heads (
             project_id TEXT NOT NULL,
             profile_key TEXT NOT NULL,
             record_id TEXT NOT NULL REFERENCES objects(object_id) ON DELETE CASCADE,
             PRIMARY KEY(project_id, profile_key)
         ) STRICT, WITHOUT ROWID;",
    )?;
    Ok(())
}

/// One chain per project and note selection. The origin alias is a host's own
/// and a merged folder can carry another host's, so it is recorded in the
/// head, never used to find it; a different alias on export is a rebuild.
fn profile_key(profile: &ExportProfile) -> &'static str {
    if profile.include_notes {
        "notes"
    } else {
        "plain"
    }
}

/// The current head of `project` and `profile`, if any.
///
/// # Errors
/// Refuses a projected head whose record is missing or of another project or
/// profile; repair is explicit.
pub(in crate::storage::work) fn head_on(
    connection: &Connection,
    project: &ProjectId,
    profile: &ExportProfile,
) -> Result<Option<(ObjectId, FolderHead)>, StoreError> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT record_id FROM work_task_sync_folder_heads
             WHERE project_id = ?1 AND profile_key = ?2",
            params![project.0, profile_key(profile)],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let id = ObjectId::from_stored(stored).ok_or_else(|| invalid("folder head id is invalid"))?;
    let head: FolderHead = SqliteStore::get_typed_object_on(connection, &id, FOLDER_HEAD_KIND)?
        .ok_or_else(|| invalid("folder head record is missing"))?;
    if head.project_id != *project || head.profile.include_notes != profile.include_notes {
        return Err(invalid(
            "folder head belongs to another project or note selection",
        ));
    }
    Ok(Some((id, head)))
}

/// Append `head` if its predecessor is still the current head, in the
/// caller's transaction.
///
/// # Errors
/// Refuses with [`TaskSyncRefusal::BaselineHeadMoved`] when another writer
/// advanced the chain first, and refuses an invalid record.
pub(in crate::storage::work) fn append_on(
    connection: &Connection,
    head: &FolderHead,
) -> Result<ObjectId, StoreError> {
    if head.project_id.0.trim().is_empty() {
        return Err(invalid("folder head project is blank"));
    }
    head.profile.options().validate()?;
    if head.reason == FolderHeadReason::Bootstrap && head.previous.is_some() {
        return Err(invalid("a folder bootstrap head cannot have a predecessor"));
    }
    if head
        .source
        .as_ref()
        .is_some_and(|source| crate::task_sync::ledger::oid(&source.commit).is_err())
    {
        return Err(invalid("folder head commit is not an exact object id"));
    }
    let current = head_on(connection, &head.project_id, &head.profile)?.map(|(id, _)| id);
    if current != head.previous {
        return Err(TaskSyncRefusal::BaselineHeadMoved.invalid(
            "another export or import recorded the shared folder first; export or import again",
        ));
    }
    let object = CanonicalObject::mint(head)?;
    SqliteStore::insert_object(connection, FOLDER_HEAD_KIND, &object)?;
    connection.execute(
        "INSERT INTO work_task_sync_folder_heads (project_id, profile_key, record_id)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(project_id, profile_key) DO UPDATE SET record_id = excluded.record_id",
        params![
            head.project_id.0,
            profile_key(&head.profile),
            object.key().as_str()
        ],
    )?;
    Ok(object.key().clone())
}

/// Rebuild every head from the recorded chains, naming each chain that has
/// no single tip.
pub(in crate::storage::work) fn rebuild(
    connection: &Connection,
) -> Result<Vec<String>, StoreError> {
    connection.execute("DELETE FROM work_task_sync_folder_heads", [])?;
    let (heads, findings) = derive(connection)?;
    for ((project, profile), record) in heads {
        connection.execute(
            "INSERT INTO work_task_sync_folder_heads (project_id, profile_key, record_id)
             VALUES (?1, ?2, ?3)",
            params![project, profile, record],
        )?;
    }
    Ok(findings)
}

/// Compare the projected heads with the chains.
pub(in crate::storage::work) fn verify(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let (expected, findings) = derive(connection)?;
    invalid.extend(findings);
    let actual = connection
        .prepare("SELECT project_id, profile_key, record_id FROM work_task_sync_folder_heads")?
        .query_map([], |row| Ok(((row.get(0)?, row.get(1)?), row.get(2)?)))?
        .collect::<Result<BTreeMap<(String, String), String>, _>>()?;
    let keys = expected
        .keys()
        .chain(actual.keys())
        .collect::<BTreeSet<_>>();
    *checked += keys.len();
    for key in keys {
        if expected.get(key) != actual.get(key) {
            invalid.push(format!("work_task_sync_folder_heads:{}:{}", key.0, key.1));
        }
    }
    Ok(())
}

type Heads = BTreeMap<(String, String), String>;

/// Each chain's tip: the one record no other record of its project and
/// profile names as its predecessor, reached from a bootstrap.
fn derive(connection: &Connection) -> Result<(Heads, Vec<String>), StoreError> {
    let mut statement = connection
        .prepare("SELECT object_id, canonical_json FROM objects WHERE object_kind = ?1")?;
    let mut rows = statement.query([FOLDER_HEAD_KIND])?;
    let mut chains: BTreeMap<(String, String), BTreeMap<String, Option<String>>> = BTreeMap::new();
    let mut findings = Vec::new();
    while let Some(row) = rows.next()? {
        let id = row.get::<_, String>(0)?;
        let bytes = row.get::<_, Vec<u8>>(1)?;
        match CanonicalObject::decode_bytes::<FolderHead>(&bytes)
            .map(|head| (profile_key(&head.profile).to_owned(), head))
        {
            Ok((profile, head)) => {
                chains
                    .entry((head.project_id.0.clone(), profile))
                    .or_default()
                    .insert(
                        id,
                        head.previous.map(|previous| previous.as_str().to_owned()),
                    );
            }
            Err(StoreError::Sqlite(error)) => return Err(error.into()),
            Err(error) => findings.push(crate::storage::decode_failure_label(
                format!("work_task_sync_folder_heads:{id}"),
                &error,
            )),
        }
    }
    let mut heads = BTreeMap::new();
    for (key, records) in chains {
        let named = records.values().flatten().collect::<BTreeSet<_>>();
        let tips = records
            .keys()
            .filter(|id| !named.contains(id))
            .collect::<Vec<_>>();
        // Walk back from the single tip; every record must be on that path,
        // so a fork or a dangling predecessor is a finding, not a guess.
        let walked = match tips.as_slice() {
            [tip] => {
                let mut seen = BTreeSet::new();
                let mut cursor = Some((*tip).clone());
                let mut sound = true;
                while let Some(id) = cursor.take() {
                    // A predecessor outside the chain or a repeated record
                    // leaves the chain unsound.
                    let Some(previous) = records.get(&id) else {
                        sound = false;
                        break;
                    };
                    if !seen.insert(id) {
                        sound = false;
                        break;
                    }
                    cursor.clone_from(previous);
                }
                (sound && seen.len() == records.len()).then(|| (*tip).clone())
            }
            _ => None,
        };
        match walked {
            Some(tip) => {
                heads.insert(key, tip);
            }
            None => findings.push(format!(
                "work_task_sync_folder_heads:{}:{}:chain",
                key.0, key.1
            )),
        }
    }
    Ok((heads, findings))
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(format!(
        "task folder head {message}; repair projections explicitly"
    ))
}
