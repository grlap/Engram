//! Exchange baselines as a chain of changes, and the per-task view of the
//! latest one.
//!
//! Each `task_sync_import_planning` record holds only what changed against
//! the record it names as `previous`: the task documents that joined or
//! changed, and the members that left. Reading the baseline replays nothing.
//! `work_task_sync_baseline` holds each task's latest baseline document, or
//! its departure, with the record and project-feed position that set it.
//! `work_task_sync_baseline_heads` holds each project's latest record and the
//! content fingerprint of its whole membership. Every planning append applies
//! its record here in the same transaction, refusing a record whose
//! predecessor is no longer the head. Repair rebuilds both tables by
//! replaying the records in feed order, and the doctor compares that replay
//! with the stored rows.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::folder::IMPORT_PLANNING_KIND;
use crate::{
    CanonicalObject, ObjectId, ProjectId, WorkId,
    storage::StoreError,
    task_sync::{FORMAT, ProjectDocument, TaskDocument, TaskSyncRefusal, TaskTree, pretty},
};

/// One exchange baseline, as the changes since its predecessor. A `None`
/// change records a member that left the exchange; its id stays retired
/// until a later baseline brings it back.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::storage::work) struct TaskSyncImportPlanning {
    pub project_id: ProjectId,
    pub previous: Option<ObjectId>,
    pub changes: BTreeMap<WorkId, Option<TaskDocument>>,
    /// The membership fingerprint after these changes. A content
    /// fingerprint for comparison, never an identity.
    pub inventory_root: String,
    pub export_basis: String,
    pub profile: crate::task_sync::progress::ExportProfile,
    pub created_at: DateTime<Utc>,
}

impl TaskSyncImportPlanning {
    /// Checks what one record can show on its own: its project, its
    /// envelope, and that each change names the task it carries.
    pub(in crate::storage::work) fn validate(&self, project: &ProjectId) -> Result<(), StoreError> {
        let consistent = self.project_id == *project
            && !self.export_basis.trim().is_empty()
            && self.profile.options().validate().is_ok()
            && Root::parse(&self.inventory_root).is_some()
            && self.changes.iter().all(|(id, change)| {
                change.as_ref().is_none_or(|document| {
                    document.id == *id && (self.profile.include_notes || document.notes.is_empty())
                })
            });
        if consistent {
            Ok(())
        } else {
            Err(invalid("task folder baseline record is inconsistent"))
        }
    }
}

/// A project's latest exchange baseline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::storage::work) struct BaselineHead {
    pub record: ObjectId,
    pub position: i64,
    pub inventory_root: String,
    pub export_basis: String,
    pub include_notes: bool,
}

/// The latest complete baseline of a project, read from the per-task view.
pub(in crate::storage::work) struct Baseline {
    pub head: BaselineHead,
    pub tree: TaskTree,
}

pub(in crate::storage::work) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_baseline (
             project_id TEXT NOT NULL,
             work_id TEXT NOT NULL,
             record_id TEXT NOT NULL,
             position INTEGER NOT NULL CHECK(position > 0),
             document BLOB,
             leaf BLOB CHECK(leaf IS NULL OR length(leaf) = 32),
             PRIMARY KEY(project_id, work_id),
             CHECK((document IS NULL) = (leaf IS NULL))
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_task_sync_baseline_retired
             ON work_task_sync_baseline(project_id, work_id)
             WHERE document IS NULL;
         CREATE TABLE IF NOT EXISTS work_task_sync_baseline_heads (
             project_id TEXT PRIMARY KEY,
             record_id TEXT NOT NULL,
             position INTEGER NOT NULL CHECK(position > 0),
             inventory_root TEXT NOT NULL,
             export_basis TEXT NOT NULL,
             include_notes INTEGER NOT NULL CHECK(include_notes IN (0, 1))
         ) STRICT;",
    )?;
    Ok(())
}

/// Applies one planning append to the view, in the caller's transaction.
pub(in crate::storage::work) fn record_on(
    connection: &Connection,
    project: &str,
    position: i64,
    object: &CanonicalObject,
) -> Result<(), StoreError> {
    let planning = object.decode::<TaskSyncImportPlanning>()?;
    apply(
        &mut SqlView(connection),
        project,
        position,
        object.key(),
        &planning,
    )
}

/// The latest baseline head of `project`, if it was ever enrolled.
pub(in crate::storage::work) fn head(
    connection: &Connection,
    project: &ProjectId,
) -> Result<Option<BaselineHead>, StoreError> {
    SqlView(connection).head(&project.0)
}

/// Note disclosure stays bound to the admitted exchange baseline. Machine
/// aliases remain local to each host and are not part of this comparison.
pub(super) fn validate_note_selection(
    connection: &Connection,
    project: &ProjectId,
    include_notes: bool,
) -> Result<(), StoreError> {
    if head(connection, project)?.is_some_and(|head| head.include_notes != include_notes) {
        return Err(TaskSyncRefusal::DivergentPlanning
            .invalid("task folder export note selection differs from its complete baseline"));
    }
    Ok(())
}

/// One task's latest baseline entry: the record and project-feed position
/// that set it, and its document, or `None` once the task left the exchange.
/// A task with no entry was never shared.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::storage::work) struct BaselineEntry {
    pub record: ObjectId,
    pub position: i64,
    pub document: Option<TaskDocument>,
}

/// The baseline entries of exactly `ids`: one keyed row read and one keyed
/// feed check each, whatever the size of the membership. An entry whose
/// record is not the planning record at its project-feed position, or whose
/// document does not match its task and leaf, refuses; nothing falls back
/// to replaying the records.
pub(in crate::storage::work) fn entries(
    connection: &Connection,
    project: &ProjectId,
    ids: impl IntoIterator<Item = WorkId>,
) -> Result<BTreeMap<WorkId, BaselineEntry>, StoreError> {
    let mut view = SqlView(connection);
    let mut found = BTreeMap::new();
    for id in ids {
        let Some(row) = view.row(&project.0, id)? else {
            continue;
        };
        let fed = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_feed_entries
                 WHERE feed_kind = 'project' AND feed_id = ?1 AND position = ?2
                   AND object_id = ?3 AND object_kind = 'task_sync_import_planning'
             )",
            params![project.0, row.position, row.record.as_str()],
            |exists| exists.get::<_, bool>(0),
        )?;
        if !fed {
            return Err(invalid(format!(
                "task folder baseline entry for {} names a record the project feed does not hold at its position",
                id.0
            )));
        }
        let document = row
            .document
            .as_deref()
            .map(serde_json::from_slice::<TaskDocument>)
            .transpose()
            .map_err(|error| {
                invalid(format!(
                    "task folder baseline entry for {} does not decode: {error}",
                    id.0
                ))
            })?;
        let consistent = match &document {
            Some(document) => document.id == id && row.leaf == Some(leaf(document)?),
            None => row.leaf.is_none(),
        };
        if !consistent {
            return Err(invalid(format!(
                "task folder baseline entry for {} does not match its task",
                id.0
            )));
        }
        found.insert(
            id,
            BaselineEntry {
                record: row.record,
                position: row.position,
                document,
            },
        );
    }
    Ok(found)
}

/// The tasks that left the exchange of `project`.
pub(in crate::storage::work) fn retired(
    connection: &Connection,
    project: &ProjectId,
) -> Result<BTreeSet<WorkId>, StoreError> {
    // The partial index serves this read, so it costs the retired rows only.
    connection
        .prepare(
            "SELECT work_id FROM work_task_sync_baseline
             INDEXED BY work_task_sync_baseline_retired
             WHERE project_id = ?1 AND document IS NULL",
        )?
        .query_map([&project.0], |row| row.get::<_, String>(0))?
        .map(|row| parse_work_id(&row?))
        .collect()
}

/// The complete latest baseline of `project`. Only a complete-folder
/// exchange, which compares the whole folder, reads every member. Each
/// member's document must match its key and leaf, and the members' leaves
/// must sum to the head's fingerprint, or the read refuses. That is a
/// consistency guard between two parts of one rebuildable projection, so a
/// view out of step with itself never decides an exchange; it is not a
/// corruption check. The doctor's comparison with a replay of the records
/// stays the check that the view matches them.
pub(in crate::storage::work) fn load(
    connection: &Connection,
    project: &ProjectId,
) -> Result<Option<Baseline>, StoreError> {
    let Some(head) = head(connection, project)? else {
        return Ok(None);
    };
    let rows = connection
        .prepare(
            "SELECT work_id, document, leaf FROM work_task_sync_baseline
             WHERE project_id = ?1 AND document IS NOT NULL
             ORDER BY work_id",
        )?
        .query_map([&project.0], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut root = Root::ZERO;
    let mut tasks = Vec::with_capacity(rows.len());
    for (work_id, document, stored_leaf) in rows {
        let id = parse_work_id(&work_id)?;
        let document = serde_json::from_slice::<TaskDocument>(&document)?;
        let computed = leaf(&document)?;
        if document.id != id || stored_leaf.as_slice() != computed.as_slice() {
            return Err(invalid(format!(
                "task folder baseline entry for {} does not match its task",
                id.0
            )));
        }
        root = root.add(&computed);
        tasks.push(document);
    }
    if root.hex() != head.inventory_root {
        return Err(invalid(
            "task folder baseline members do not sum to the head fingerprint",
        ));
    }
    let mut tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project.clone(),
            notes_included: head.include_notes,
        },
        tasks,
    };
    tree.tasks.sort_by_key(|task| task.id.0);
    Ok(Some(Baseline { head, tree }))
}

/// Appends one baseline record of `changes` against `previous`, which must
/// be the current head, and applies it to the view. Reads and writes the
/// changed tasks' entries alone, whatever the size of the membership.
pub(in crate::storage::work) fn append(
    transaction: &rusqlite::Transaction<'_>,
    project: &ProjectId,
    previous: Option<ObjectId>,
    changes: BTreeMap<WorkId, Option<TaskDocument>>,
    export_basis: &str,
    profile: &crate::task_sync::progress::ExportProfile,
    now: DateTime<Utc>,
) -> Result<ObjectId, StoreError> {
    let planning = TaskSyncImportPlanning {
        project_id: project.clone(),
        previous,
        inventory_root: next_root(transaction, project, &changes)?,
        changes,
        export_basis: export_basis.into(),
        profile: profile.clone(),
        created_at: now,
    };
    let object = CanonicalObject::mint(&planning)?;
    crate::storage::SqliteStore::insert_object(transaction, IMPORT_PLANNING_KIND, &object)?;
    super::super::feeds::append_to_project_feed(
        transaction,
        project,
        IMPORT_PLANNING_KIND,
        &object,
    )?;
    Ok(object.key().clone())
}

/// The membership fingerprint after `changes`, from the current head and the
/// changed tasks' entries alone.
pub(in crate::storage::work) fn next_root(
    connection: &Connection,
    project: &ProjectId,
    changes: &BTreeMap<WorkId, Option<TaskDocument>>,
) -> Result<String, StoreError> {
    let mut view = SqlView(connection);
    let mut root = match view.head(&project.0)? {
        Some(head) => Root::parse(&head.inventory_root)
            .ok_or_else(|| invalid("task folder baseline head has an invalid root"))?,
        None => Root::ZERO,
    };
    for (id, change) in changes {
        if let Some(leaf) = view.row(&project.0, *id)?.and_then(|stored| stored.leaf) {
            root = root.sub(&leaf);
        }
        if let Some(document) = change {
            root = root.add(&leaf(document)?);
        }
    }
    Ok(root.hex())
}

/// Rebuilds both tables by replaying every planning record in feed order,
/// naming each record it cannot apply so that the caller refuses the whole
/// repair with every such record listed.
pub(in crate::storage::work) fn rebuild(
    connection: &Connection,
) -> Result<Vec<String>, StoreError> {
    let (derived, findings) = derive(connection)?;
    connection.execute("DELETE FROM work_task_sync_baseline", [])?;
    connection.execute("DELETE FROM work_task_sync_baseline_heads", [])?;
    let mut view = SqlView(connection);
    for ((project, id), row) in &derived.rows {
        view.put_row(project, *id, row)?;
    }
    for (project, head) in &derived.heads {
        view.put_head(project, head)?;
    }
    Ok(findings)
}

/// Compares the stored view with a replay of the records.
pub(in crate::storage::work) fn verify(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let (expected, findings) = derive(connection)?;
    invalid.extend(findings);
    let actual = match stored(connection) {
        Ok(view) => view,
        Err(StoreError::Sqlite(error)) => return Err(error.into()),
        Err(error) => {
            invalid.push(crate::storage::decode_failure_label(
                "work_task_sync_baseline:unreadable".into(),
                &error,
            ));
            return Ok(());
        }
    };
    let keys = expected
        .rows
        .keys()
        .chain(actual.rows.keys())
        .collect::<BTreeSet<_>>();
    // One check per row and per head either side holds.
    for key in keys {
        *checked += 1;
        if expected.rows.get(key) != actual.rows.get(key) {
            invalid.push(format!("work_task_sync_baseline:{}:{}", key.0, key.1.0));
        }
    }
    let projects = expected
        .heads
        .keys()
        .chain(actual.heads.keys())
        .collect::<BTreeSet<_>>();
    for project in projects {
        *checked += 1;
        if expected.heads.get(project) != actual.heads.get(project) {
            invalid.push(format!("work_task_sync_baseline_heads:{project}"));
        }
    }
    Ok(())
}

/// The leaf one member contributes to the membership fingerprint: the
/// fingerprint of its exported file, path included.
pub(in crate::storage::work) fn leaf(document: &TaskDocument) -> Result<[u8; 32], StoreError> {
    let path = format!("tasks/{}.json", document.id.0);
    let bytes = pretty(document)?;
    let mut hash = Sha256::new();
    hash.update((path.len() as u64).to_le_bytes());
    hash.update(path.as_bytes());
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(&bytes);
    Ok(hash.finalize().into())
}

/// The membership fingerprint of a complete tree, computed from scratch.
pub(in crate::storage::work) fn tree_root(tree: &TaskTree) -> Result<String, StoreError> {
    let mut root = Root::ZERO;
    for task in &tree.tasks {
        root = root.add(&leaf(task)?);
    }
    Ok(root.hex())
}

/// The order-independent sum of member leaves, modulo 2^256. Adding a leaf
/// and subtracting it again restore the root, so a record updates it for
/// exactly the tasks it changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Root([u8; 32]);

impl Root {
    const ZERO: Self = Self([0; 32]);

    fn parse(hex: &str) -> Option<Self> {
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        let mut bytes = [0; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
        }
        Some(Self(bytes))
    }

    fn hex(self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut hex = String::with_capacity(64);
        for byte in self.0 {
            hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
            hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        hex
    }

    /// Big-endian addition with the final carry dropped.
    fn add(self, leaf: &[u8; 32]) -> Self {
        let mut sum = [0; 32];
        let mut carry = false;
        for index in (0..32).rev() {
            let (partial, first) = self.0[index].overflowing_add(leaf[index]);
            let (total, second) = partial.overflowing_add(u8::from(carry));
            sum[index] = total;
            carry = first || second;
        }
        Self(sum)
    }

    /// Big-endian subtraction with the final borrow dropped.
    fn sub(self, leaf: &[u8; 32]) -> Self {
        let mut difference = [0; 32];
        let mut borrow = false;
        for index in (0..32).rev() {
            let (partial, first) = self.0[index].overflowing_sub(leaf[index]);
            let (total, second) = partial.overflowing_sub(u8::from(borrow));
            difference[index] = total;
            borrow = first || second;
        }
        Self(difference)
    }
}

/// One stored row: the canonical document bytes and their leaf.
#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredRow {
    record: ObjectId,
    position: i64,
    document: Option<Vec<u8>>,
    leaf: Option<[u8; 32]>,
}

/// Where the replay reads and writes: the stored tables on the hot path, or
/// memory for repair and the doctor.
trait View {
    fn head(&mut self, project: &str) -> Result<Option<BaselineHead>, StoreError>;
    fn row(&mut self, project: &str, id: WorkId) -> Result<Option<StoredRow>, StoreError>;
    fn put_row(&mut self, project: &str, id: WorkId, row: &StoredRow) -> Result<(), StoreError>;
    fn put_head(&mut self, project: &str, head: &BaselineHead) -> Result<(), StoreError>;
}

/// Applies one record: its predecessor must be the head, a departure must
/// name a current member, and the record's fingerprint must equal the head's
/// updated for exactly its changes. Every check runs before the first write,
/// so a refused record leaves the view as it was.
fn apply(
    view: &mut impl View,
    project: &str,
    position: i64,
    record: &ObjectId,
    planning: &TaskSyncImportPlanning,
) -> Result<(), StoreError> {
    planning.validate(&ProjectId(project.into()))?;
    let head = view.head(project)?;
    if planning.previous.as_ref() != head.as_ref().map(|head| &head.record) {
        return Err(TaskSyncRefusal::BaselineHeadMoved.invalid(format!(
            "task folder baseline names {} as its predecessor, but the latest baseline is {}",
            planning
                .previous
                .as_ref()
                .map_or("none".into(), ToString::to_string),
            head.as_ref()
                .map_or("none".into(), |head| head.record.to_string()),
        )));
    }
    if head
        .as_ref()
        .is_some_and(|head| head.include_notes != planning.profile.include_notes)
    {
        return Err(TaskSyncRefusal::DivergentPlanning
            .invalid("task folder baseline note selection differs from its predecessor"));
    }
    let mut root = match &head {
        Some(head) => Root::parse(&head.inventory_root)
            .ok_or_else(|| invalid("task folder baseline head has an invalid root"))?,
        None => Root::ZERO,
    };
    let mut staged = Vec::with_capacity(planning.changes.len());
    for (id, change) in &planning.changes {
        let previous = view.row(project, *id)?;
        if let Some(leaf) = previous.as_ref().and_then(|row| row.leaf) {
            root = root.sub(&leaf);
        }
        let (document, leaf) = match change {
            Some(document) => (
                Some(serde_json::to_vec(document)?),
                Some(self::leaf(document)?),
            ),
            None if previous.as_ref().is_some_and(|row| row.leaf.is_some()) => (None, None),
            None => {
                return Err(invalid(format!(
                    "task folder baseline retires {}, which is not a member",
                    id.0
                )));
            }
        };
        if let Some(leaf) = &leaf {
            root = root.add(leaf);
        }
        staged.push((
            *id,
            StoredRow {
                record: record.clone(),
                position,
                document,
                leaf,
            },
        ));
    }
    if root.hex() != planning.inventory_root {
        return Err(invalid(
            "task folder baseline fingerprint does not match its changes",
        ));
    }
    for (id, row) in &staged {
        view.put_row(project, *id, row)?;
    }
    view.put_head(
        project,
        &BaselineHead {
            record: record.clone(),
            position,
            inventory_root: planning.inventory_root.clone(),
            export_basis: planning.export_basis.clone(),
            include_notes: planning.profile.include_notes,
        },
    )
}

struct SqlView<'a>(&'a Connection);

impl View for SqlView<'_> {
    fn head(&mut self, project: &str) -> Result<Option<BaselineHead>, StoreError> {
        self.0
            .query_row(
                "SELECT record_id, position, inventory_root, export_basis, include_notes
                 FROM work_task_sync_baseline_heads WHERE project_id = ?1",
                [project],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?
            .map(|(record, position, inventory_root, export_basis, notes)| {
                Ok(BaselineHead {
                    record: parse_record(record)?,
                    position,
                    inventory_root,
                    export_basis,
                    include_notes: notes == 1,
                })
            })
            .transpose()
    }

    fn row(&mut self, project: &str, id: WorkId) -> Result<Option<StoredRow>, StoreError> {
        self.0
            .query_row(
                "SELECT record_id, position, document, leaf FROM work_task_sync_baseline
                 WHERE project_id = ?1 AND work_id = ?2",
                params![project, id.0.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                },
            )
            .optional()?
            .map(|(record, position, document, leaf)| stored_row(record, position, document, leaf))
            .transpose()
    }

    fn put_row(&mut self, project: &str, id: WorkId, row: &StoredRow) -> Result<(), StoreError> {
        self.0.execute(
            "INSERT INTO work_task_sync_baseline
                 (project_id, work_id, record_id, position, document, leaf)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(project_id, work_id) DO UPDATE SET
                 record_id = excluded.record_id, position = excluded.position,
                 document = excluded.document, leaf = excluded.leaf",
            params![
                project,
                id.0.to_string(),
                row.record.as_str(),
                row.position,
                row.document,
                row.leaf.map(Vec::from),
            ],
        )?;
        Ok(())
    }

    fn put_head(&mut self, project: &str, head: &BaselineHead) -> Result<(), StoreError> {
        self.0.execute(
            "INSERT INTO work_task_sync_baseline_heads
                 (project_id, record_id, position, inventory_root, export_basis, include_notes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(project_id) DO UPDATE SET
                 record_id = excluded.record_id, position = excluded.position,
                 inventory_root = excluded.inventory_root,
                 export_basis = excluded.export_basis,
                 include_notes = excluded.include_notes",
            params![
                project,
                head.record.as_str(),
                head.position,
                head.inventory_root,
                head.export_basis,
                i64::from(head.include_notes),
            ],
        )?;
        Ok(())
    }
}

#[derive(Default)]
struct MemoryView {
    rows: BTreeMap<(String, WorkId), StoredRow>,
    heads: BTreeMap<String, BaselineHead>,
}

impl View for MemoryView {
    fn head(&mut self, project: &str) -> Result<Option<BaselineHead>, StoreError> {
        Ok(self.heads.get(project).cloned())
    }

    fn row(&mut self, project: &str, id: WorkId) -> Result<Option<StoredRow>, StoreError> {
        Ok(self.rows.get(&(project.to_owned(), id)).cloned())
    }

    fn put_row(&mut self, project: &str, id: WorkId, row: &StoredRow) -> Result<(), StoreError> {
        self.rows.insert((project.to_owned(), id), row.clone());
        Ok(())
    }

    fn put_head(&mut self, project: &str, head: &BaselineHead) -> Result<(), StoreError> {
        self.heads.insert(project.to_owned(), head.clone());
        Ok(())
    }
}

/// Replays every planning record, project by project in feed order, naming
/// each record that cannot be applied. Only a database failure is an error.
fn derive(connection: &Connection) -> Result<(MemoryView, Vec<String>), StoreError> {
    // The kind is a literal so that the partial planning index serves this
    // read instead of a scan of every project-feed entry.
    // Records are streamed, one in memory at a time.
    let mut statement = connection.prepare(
        "SELECT entry.feed_id, entry.position, entry.object_id, object.canonical_json
         FROM work_feed_entries entry
         JOIN objects object ON object.object_id = entry.object_id
         WHERE entry.feed_kind = 'project'
           AND entry.object_kind = 'task_sync_import_planning'
         ORDER BY entry.feed_id, entry.position",
    )?;
    let mut entries = statement.query([])?;
    let mut view = MemoryView::default();
    let mut findings = Vec::new();
    // A record that cannot be applied breaks its project's chain: the
    // project's later records are not replayed against a wrong head.
    let mut broken = BTreeSet::new();
    while let Some(entry) = entries.next()? {
        let project = entry.get::<_, String>(0)?;
        if broken.contains(&project) {
            continue;
        }
        let position = entry.get::<_, i64>(1)?;
        let object_id = entry.get::<_, String>(2)?;
        let bytes = entry.get::<_, Vec<u8>>(3)?;
        let applied = parse_record(object_id.clone()).and_then(|id| {
            let planning =
                CanonicalObject::stored(&id, bytes)?.decode::<TaskSyncImportPlanning>()?;
            apply(&mut view, &project, position, &id, &planning)
        });
        match applied {
            Ok(()) => {}
            Err(StoreError::Sqlite(error)) => return Err(error.into()),
            Err(error) => {
                findings.push(crate::storage::decode_failure_label(
                    format!("work_task_sync_baseline:underivable:{project}:{object_id}"),
                    &error,
                ));
                broken.insert(project);
            }
        }
    }
    Ok((view, findings))
}

/// Reads both stored tables whole, for the doctor.
fn stored(connection: &Connection) -> Result<MemoryView, StoreError> {
    let mut view = MemoryView::default();
    let rows = connection
        .prepare(
            "SELECT project_id, work_id, record_id, position, document, leaf
             FROM work_task_sync_baseline",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (project, work_id, record, position, document, leaf) in rows {
        view.rows.insert(
            (project, parse_work_id(&work_id)?),
            stored_row(record, position, document, leaf)?,
        );
    }
    let projects = connection
        .prepare("SELECT project_id FROM work_task_sync_baseline_heads")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for project in projects {
        if let Some(head) = SqlView(connection).head(&project)? {
            view.heads.insert(project, head);
        }
    }
    Ok(view)
}

fn stored_row(
    record: String,
    position: i64,
    document: Option<Vec<u8>>,
    leaf: Option<Vec<u8>>,
) -> Result<StoredRow, StoreError> {
    Ok(StoredRow {
        record: parse_record(record)?,
        position,
        document,
        leaf: leaf
            .map(|leaf| {
                <[u8; 32]>::try_from(leaf.as_slice())
                    .map_err(|_| invalid("task folder baseline leaf is not 32 bytes"))
            })
            .transpose()?,
    })
}

fn parse_record(stored: String) -> Result<ObjectId, StoreError> {
    let shown = stored.clone();
    ObjectId::from_stored(stored).ok_or_else(|| {
        invalid(format!(
            "task folder baseline names an invalid record {shown}"
        ))
    })
}

fn parse_work_id(stored: &str) -> Result<WorkId, StoreError> {
    uuid::Uuid::parse_str(stored).map(WorkId).map_err(|_| {
        invalid(format!(
            "task folder baseline names an invalid work id {stored}"
        ))
    })
}

fn invalid(reason: impl Into<String>) -> StoreError {
    StoreError::InvalidWorkProjection(reason.into())
}
