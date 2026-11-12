//! Embedded documents and identity witnesses with their immutable member paths.
//! Ordinary native events do not enter this projection.

use std::collections::BTreeSet;

use rusqlite::{Connection, params};

use super::super::{
    ImportOrigin, ORIGIN_KIND, baseline::TaskSyncImportPlanning, folder::IMPORT_PLANNING_KIND,
};
use crate::{
    CanonicalObject, ObjectId, ProjectId, WorkId,
    storage::StoreError,
    task_sync::{
        TaskDocument,
        delta::{BootstrapMember, BootstrapMembers, CheckpointSeed},
        ledger::{EVENT_KIND, Event, Transition},
        reconciliation::{SHARED_VERSION_KIND, SharedImportVersion},
    },
};

#[cfg(test)]
thread_local! {
    pub(in crate::storage::work) static PROJECTOR_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Member {
    record: String,
    work: String,
    project: String,
    path: String,
    document: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Identity {
    record: String,
    work: String,
    family: String,
    machine: String,
    value: String,
    path: String,
}

#[derive(Default, Eq, PartialEq)]
struct Rows {
    members: BTreeSet<Member>,
    identities: BTreeSet<Identity>,
}

pub(in crate::storage::work) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_document_members (
             record_id TEXT NOT NULL REFERENCES objects(object_id) ON DELETE CASCADE,
             work_id TEXT NOT NULL,
             project_id TEXT NOT NULL,
             member_path TEXT NOT NULL,
             document_json BLOB,
             PRIMARY KEY(record_id, work_id)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_task_sync_shared_identity_members (
             record_id TEXT NOT NULL,
             work_id TEXT NOT NULL,
             family TEXT NOT NULL CHECK(family IN ('note','activity')),
             machine TEXT NOT NULL,
             value TEXT NOT NULL,
             member_path TEXT NOT NULL,
             PRIMARY KEY(record_id, work_id, family, machine, value),
             FOREIGN KEY(record_id, work_id)
                 REFERENCES work_task_sync_document_members(record_id, work_id) ON DELETE CASCADE
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_task_sync_shared_identity_lookup
             ON work_task_sync_shared_identity_members
                 (family, machine, value, record_id, work_id);",
    )?;
    Ok(())
}

fn pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

pub(super) struct DocumentIdentity {
    pub family: &'static str,
    pub machine: String,
    pub value: String,
    pub path: String,
}

pub(super) fn document_identities(document: &TaskDocument) -> Vec<DocumentIdentity> {
    document
        .notes
        .iter()
        .enumerate()
        .map(|(index, note)| DocumentIdentity {
            family: "note",
            machine: String::new(),
            value: note.id.to_string(),
            path: format!("/notes/{index}"),
        })
        .chain(document.activity.iter().flat_map(|(machine, notices)| {
            notices.keys().map(move |id| DocumentIdentity {
                family: "activity",
                machine: machine.clone(),
                value: id.to_string(),
                path: format!("/activity/{}/{}", pointer(machine), pointer(id.as_str())),
            })
        }))
        .collect()
}

impl Rows {
    fn member(
        &mut self,
        record: &ObjectId,
        project: &ProjectId,
        work: WorkId,
        path: &str,
        document: Option<&TaskDocument>,
    ) -> Result<(), StoreError> {
        self.members.insert(Member {
            record: record.as_str().into(),
            work: work.0.to_string(),
            project: project.0.clone(),
            path: path.to_owned(),
            document: document.map(serde_json::to_vec).transpose()?,
        });
        let Some(document) = document else {
            return Ok(());
        };
        for identity in document_identities(document) {
            self.identity(
                record,
                work,
                identity.family,
                &identity.machine,
                &identity.value,
                format!("{path}{}", identity.path),
            );
        }
        Ok(())
    }

    fn identity(
        &mut self,
        record: &ObjectId,
        work: WorkId,
        family: &str,
        machine: &str,
        value: &str,
        path: String,
    ) {
        self.identities.insert(Identity {
            record: record.as_str().into(),
            work: work.0.to_string(),
            family: family.into(),
            machine: machine.into(),
            value: value.into(),
            path,
        });
    }

    fn write(&self, connection: &Connection) -> Result<(), StoreError> {
        for row in &self.members {
            connection.execute(
                "INSERT INTO work_task_sync_document_members
                (record_id,work_id,project_id,member_path,document_json) VALUES (?1,?2,?3,?4,?5)",
                params![row.record, row.work, row.project, row.path, row.document],
            )?;
        }
        for row in &self.identities {
            connection.execute(
                "INSERT INTO work_task_sync_shared_identity_members
                (record_id,work_id,family,machine,value,member_path) VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    row.record,
                    row.work,
                    row.family,
                    row.machine,
                    row.value,
                    row.path
                ],
            )?;
        }
        Ok(())
    }
}

fn derive(kind: &str, object: &CanonicalObject) -> Result<Rows, StoreError> {
    let mut rows = Rows::default();
    match kind {
        ORIGIN_KIND => {
            let origin: ImportOrigin = object.decode()?;
            rows.member(
                object.key(),
                &origin.project_id,
                origin.task.id,
                "/task",
                Some(&origin.task),
            )?;
        }
        SHARED_VERSION_KIND => {
            let version: SharedImportVersion = object.decode()?;
            rows.member(
                object.key(),
                &version.project_id,
                version.task.id,
                "/task",
                Some(&version.task),
            )?;
        }
        IMPORT_PLANNING_KIND => {
            let planning: TaskSyncImportPlanning = object.decode()?;
            planning.validate(&planning.project_id)?;
            for (work, document) in &planning.changes {
                rows.member(
                    object.key(),
                    &planning.project_id,
                    *work,
                    &format!("/changes/{}", work.0),
                    document.as_ref(),
                )?;
            }
        }
        EVENT_KIND => {
            let event: Event = object.decode()?;
            let delta = match &event.transition {
                Transition::DeltaPrepared { intent, payload } => {
                    Some((intent.as_ref(), payload.as_ref()))
                }
                Transition::PlanningSuccessorPrepared { recipe, payload } => {
                    Some((&recipe.request.intent, payload.as_ref()))
                }
                _ => None,
            };
            if let Some((intent, payload)) = delta {
                payload.validate_historical_declared(intent)?;
                for (work, document) in &payload.documents {
                    rows.member(
                        object.key(),
                        &payload.project_id,
                        *work,
                        &format!("/transition/payload/documents/{}", work.0),
                        document.as_ref(),
                    )?;
                }
            }
            if let Transition::CustodyCheckpoint { checkpoint } = &event.transition
                && let Some(seed) = &checkpoint.seed
            {
                let crate::FeedId::Project(project) = &checkpoint.local_cut.position.feed else {
                    return Err(StoreError::InvalidWorkProjection(
                        "checkpoint requires a project feed".into(),
                    ));
                };
                let members = match seed {
                    CheckpointSeed::Incoming { members, .. }
                    | CheckpointSeed::PreparedExport { members, .. } => members,
                };
                if let BootstrapMembers::Captured { members } = members {
                    for (work, member) in members {
                        if let BootstrapMember::Document { task } = member {
                            rows.member(
                                object.key(),
                                project,
                                *work,
                                &format!(
                                    "/transition/checkpoint/seed/members/members/{}/task",
                                    work.0
                                ),
                                Some(task),
                            )?;
                        }
                    }
                }
            }
        }
        _ => {
            return Err(StoreError::InvalidWorkProjection(
                "unsupported embedded document record kind".into(),
            ));
        }
    }
    Ok(rows)
}

/// Resolve one immutable owner's member without relying on disposable rows.
/// Repair and cold bootstrap share the same member-path derivation as writers.
pub(in crate::storage::work::task_sync) fn canonical_members(
    kind: &str,
    object: &CanonicalObject,
    project: &ProjectId,
) -> Result<std::collections::BTreeMap<WorkId, TaskDocument>, StoreError> {
    let rows = derive(kind, object)?;
    let mut members = std::collections::BTreeMap::new();
    for member in rows.members {
        if member.project != project.0 {
            return Err(StoreError::InvalidWorkProjection(
                "canonical source member belongs to another project".into(),
            ));
        }
        if let Some(bytes) = member.document {
            let task: TaskDocument = serde_json::from_slice(&bytes)?;
            members.insert(task.id, task);
        }
    }
    Ok(members)
}

/// The caller admits the canonical record and owns its transaction. This
/// transformation reads no task, origin, prior version or activity history.
pub(in crate::storage::work) fn record_on(
    connection: &Connection,
    kind: &str,
    object: &CanonicalObject,
) -> Result<(), StoreError> {
    #[cfg(test)]
    PROJECTOR_CALLS.with(|count| count.set(count.get() + 1));
    derive(kind, object)?.write(connection)?;
    super::graph::record_on(connection, kind, object)
}

fn expected(connection: &Connection) -> Result<(Rows, Vec<String>), StoreError> {
    let mut rows = Rows::default();
    let mut findings = Vec::new();
    let mut statement = connection.prepare(
        "SELECT object_id,object_kind,canonical_json FROM objects
        WHERE object_kind IN (?1,?2,?3)
           OR (object_kind=?4 AND json_valid(canonical_json)
               AND (json_type(canonical_json,'$.transition.checkpoint')='object'
                    OR json_extract(canonical_json,'$.transition.transition') IN ('delta_prepared','planning_successor_prepared')))
        ORDER BY object_id",
    )?;
    let mut records = statement.query(params![
        ORIGIN_KIND,
        SHARED_VERSION_KIND,
        IMPORT_PLANNING_KIND,
        EVENT_KIND
    ])?;
    while let Some(record) = records.next()? {
        let id: String = record.get(0)?;
        let kind: String = record.get(1)?;
        let bytes: Vec<u8> = record.get(2)?;
        let derived = ObjectId::from_stored(id.clone())
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection("invalid embedded document record id".into())
            })
            .and_then(|id| CanonicalObject::stored(&id, bytes))
            .and_then(|object| derive(&kind, &object));
        match derived {
            Ok(derived) => {
                rows.members.extend(derived.members);
                rows.identities.extend(derived.identities);
            }
            Err(StoreError::Sqlite(error)) => return Err(error.into()),
            Err(error) => findings.push(crate::storage::decode_failure_label(
                format!("work_task_sync_document_members:underivable:{id}"),
                &error,
            )),
        }
    }
    Ok((rows, findings))
}

pub(in crate::storage::work) fn rebuild(
    connection: &Connection,
) -> Result<Vec<String>, StoreError> {
    let (rows, findings) = expected(connection)?;
    connection.execute("DELETE FROM work_task_sync_shared_identity_members", [])?;
    connection.execute("DELETE FROM work_task_sync_document_members", [])?;
    rows.write(connection)?;
    Ok(findings)
}

pub(in crate::storage::work) fn verify(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let (expected, findings) = expected(connection)?;
    invalid.extend(findings);
    let members = connection
        .prepare(
            "SELECT record_id,work_id,project_id,member_path,document_json
        FROM work_task_sync_document_members",
        )?
        .query_map([], |row| {
            Ok(Member {
                record: row.get(0)?,
                work: row.get(1)?,
                project: row.get(2)?,
                path: row.get(3)?,
                document: row.get(4)?,
            })
        })?
        .collect::<Result<BTreeSet<_>, _>>()?;
    let identities = connection
        .prepare(
            "SELECT record_id,work_id,family,machine,value,member_path
        FROM work_task_sync_shared_identity_members",
        )?
        .query_map([], |row| {
            Ok(Identity {
                record: row.get(0)?,
                work: row.get(1)?,
                family: row.get(2)?,
                machine: row.get(3)?,
                value: row.get(4)?,
                path: row.get(5)?,
            })
        })?
        .collect::<Result<BTreeSet<_>, _>>()?;
    *checked +=
        expected.members.len() + members.len() + expected.identities.len() + identities.len();
    if expected.members != members {
        invalid.push("work_task_sync_document_members:content".into());
    }
    if expected.identities != identities {
        invalid.push("work_task_sync_shared_identity_members:content".into());
    }
    Ok(())
}
