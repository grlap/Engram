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
    /// The bytes its task files hold as stored, not as re-encoded, kept so an
    /// incremental export checks the folder's size limit without reading
    /// every task.
    pub task_bytes: u64,
    /// The data commit the last import read. Own-export confirmation leaves
    /// this provenance intact and records the new comparison commit in `disk`.
    pub source: Option<FolderSource>,
    /// How the folder on disk relates to the data branch, so the next export
    /// can prove the disk is this head's before it writes over it.
    pub disk: FolderDisk,
    /// Held tasks whose folder bytes are another host's, never claimed as
    /// ours: each with the content fingerprint its local planning document
    /// had when it was held, `None` when this host had none. A claim and its
    /// release leave that document unchanged, so a held task still at its
    /// fingerprint changed nothing here and the next import may apply it.
    pub pending: BTreeMap<WorkId, Option<String>>,
    /// Local changes an import passed over: the next export writes them,
    /// although the cut has moved past them.
    pub unexported: BTreeSet<WorkId>,
    /// The tasks incremental exports wrote or removed after the comparison
    /// basis, each with the fingerprint of the written file or `None` for a
    /// removal. They stay owed to import until that import adjudicates them
    /// or an exact own commit becomes the comparison basis.
    pub exported: BTreeMap<WorkId, Option<String>>,
    /// Whether a complete export rewrote the folder since the last import.
    /// It publishes every task without tracking them in `exported`, so no
    /// diff from `source` can show another host returning one of them to its
    /// bytes there: the next import reads the complete folder against this
    /// head instead. Later incremental exports keep it; an import clears it.
    pub rewritten: bool,
}

impl FolderHead {
    /// A rebuild without a matched parent must replace the full membership.
    pub(crate) fn rebuild_needs_complete_import(&self) -> bool {
        self.rewritten
            && self.reason == FolderHeadReason::Rebuild
            && matches!(self.disk, FolderDisk::Unbound)
    }

    /// The committed folder represented by the keyed planning baseline.
    /// A pending write has no admitted commit to import from.
    #[must_use]
    pub fn import_basis(&self) -> Option<FolderSource> {
        let FolderDisk::Committed { commit } = &self.disk else {
            return None;
        };
        Some(FolderSource {
            commit: commit.clone(),
            inventory_root: self.inventory_root,
            task_count: self.task_count,
            task_bytes: self.task_bytes,
        })
    }
}

/// The retained write and the exact commit that was proved to hold it.
pub(crate) struct FolderExportConfirmation {
    pub(crate) expected: ObjectId,
    pub(crate) head: FolderHead,
    pub(crate) commit: String,
    pub(crate) changes: BTreeMap<WorkId, Option<crate::task_sync::TaskDocument>>,
}

impl FolderExportConfirmation {
    /// The fields an import needs to plan Git reads. The storage transaction
    /// will apply the confirmation before it uses the keyed baseline.
    pub(crate) fn comparison_head(&self) -> FolderHead {
        let mut head = self.head.clone();
        head.disk = FolderDisk::Committed {
            commit: self.commit.clone(),
        };
        head.exported.clear();
        head.rewritten = false;
        head
    }
}

/// The folder on disk, as the last export or import left it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", deny_unknown_fields)]
pub enum FolderDisk {
    /// Exactly this data commit, as an import read it or an export published it.
    Committed { commit: String },
    /// An incremental export wrote `files` onto the clean commit `from`: each
    /// task's content fingerprint, `None` for a removed file.
    Written {
        from: FolderSource,
        files: BTreeMap<WorkId, Option<String>>,
    },
    /// A complete export, or an import of loose files, with no commit to
    /// compare by: the next export reads the whole folder once to prove it.
    Unbound,
}

/// A data commit and the membership root verified for it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FolderSource {
    pub commit: String,
    pub inventory_root: InventoryRoot,
    pub task_count: u64,
    pub task_bytes: u64,
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
    /// On the complete route, the held tasks whose local planning changed
    /// since they were held: a rebuild writes this host's version over the
    /// held bytes. Empty on the incremental route, which refuses them.
    pub conflicted: BTreeSet<WorkId>,
    /// The held tasks that changed here only in what never conflicts, such
    /// as a note of this host's own: no export rewrites their held bytes, so
    /// they stay owed to the export after the hold resolves.
    pub held_owed: BTreeSet<WorkId>,
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

/// The import comparison base when a newer write is awaiting its commit.
/// Same-parent writes can carry `Written.from` from the keyed baseline; older
/// written heads without that proof continue from their applied import.
pub(super) fn import_basis_on(
    connection: &Connection,
    head: &FolderHead,
) -> Result<Option<FolderSource>, StoreError> {
    if let Some(basis) = head.import_basis() {
        return Ok(Some(basis));
    }
    if let FolderDisk::Written { from, .. } = &head.disk {
        let mut previous_id = head.previous.clone();
        while let Some(id) = previous_id {
            let previous: FolderHead =
                SqliteStore::get_typed_object_on(connection, &id, FOLDER_HEAD_KIND)?
                    .ok_or_else(|| invalid("folder head predecessor is missing"))?;
            if previous.project_id != head.project_id
                || previous.profile.include_notes != head.profile.include_notes
            {
                return Err(invalid(
                    "folder head predecessor belongs to another project or note selection",
                ));
            }
            if previous.import_basis().as_ref() == Some(from) {
                return Ok(Some(from.clone()));
            }
            if !matches!(&previous.disk, FolderDisk::Written { from: prior, .. } if prior == from) {
                break;
            }
            previous_id = previous.previous;
        }
    }
    Ok(head.source.clone())
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
    // A head records the alias this host last exported under, if it has
    // exported yet: one an import records first may have none, even with
    // notes shared.
    if let Some(alias) = &head.profile.origin_machine {
        crate::task_sync::notes::validate_machine_alias(alias)?;
    }
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
/// profile names as its predecessor. A chain starts at a bootstrap export or
/// at a complete import, either with no predecessor.
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
