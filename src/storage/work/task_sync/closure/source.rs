//! Canonical bootstrap membership. A destination baseline is never a source.

use std::collections::BTreeMap;

use rusqlite::{Connection, params};

use super::super::ledger;
pub(super) mod delta;
use crate::{
    CanonicalObject, ObjectId, WorkId,
    storage::StoreError,
    task_sync::{
        self, ProjectDocument, TaskDocument, TaskTree,
        delta::{
            BootstrapMember, BootstrapMembers, CheckpointSeed, ManagedCustodyCheckpoint,
            OutputMembership,
        },
        inventory::InventoryRoot,
        ledger::{BINDING_KIND, Binding, EVENT_KIND, Event, Transition, require},
    },
};

#[derive(Clone)]
pub(in crate::storage::work) struct ResolvedBootstrap {
    pub project: crate::ProjectId,
    pub role: &'static str,
    pub output: OutputMembership,
    pub members: BTreeMap<WorkId, (ObjectId, TaskDocument)>,
}

pub(in crate::storage::work) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_source_heads (
        binding_id TEXT NOT NULL REFERENCES objects(object_id),
        role TEXT NOT NULL CHECK(role IN ('source','export')),
        checkpoint_id TEXT NOT NULL REFERENCES objects(object_id),
        inventory_root TEXT NOT NULL,
        profile_json BLOB NOT NULL,
        PRIMARY KEY(binding_id,role)
    ) STRICT;",
    )?;
    super::membership::head::create_schema(connection)
}

type OwnerCache = BTreeMap<ObjectId, BTreeMap<WorkId, TaskDocument>>;

pub(in crate::storage::work::task_sync) fn validate_delta_event_on(
    connection: &Connection,
    id: &ObjectId,
    event: &Event,
    binding: &Binding,
) -> Result<(), StoreError> {
    delta::resolve_on(connection, id, event, binding, true).map(|_| ())
}
type SourceHeads = BTreeMap<(String, String), (ObjectId, ResolvedBootstrap)>;
type Members = BTreeMap<WorkId, (ObjectId, TaskDocument)>;

fn initial_members(
    connection: &Connection,
    binding_id: &ObjectId,
    binding: &Binding,
    outcome: &ObjectId,
) -> Result<(task_sync::ledger::RetainedCommit, Members), StoreError> {
    let mut budget = ledger::view::read::ReadBudget::default();
    let event: Event = ledger::view::read::object(connection, outcome, EVENT_KIND, &mut budget)?;
    require(
        event.binding == *binding_id,
        "bootstrap import binding differs",
    )?;
    let Transition::ImportApplied { prepared, receipt } = event.transition else {
        return Err(task_sync::TaskSyncRefusal::IncrementalBasisUnavailable.invalid(
            "bootstrap source requires a complete initial ImportApplied outcome; reconciled source is unavailable"));
    };
    let prepared: Event =
        ledger::view::read::object(connection, &prepared, EVENT_KIND, &mut budget)?;
    require(
        prepared.binding == *binding_id,
        "bootstrap preparation binding differs",
    )?;
    let Transition::Prepared { intent } = prepared.transition else {
        return Err(super::super::invalid(
            "bootstrap import preparation is missing",
        ));
    };
    intent.validate()?;
    require(
        intent.direction == task_sync::ledger::Direction::Incoming
            && receipt.source_commit == intent.commit.oid
            && receipt.source_tree == intent.commit.tree
            && receipt.notes_included == binding.profile.include_notes,
        "bootstrap source snapshot or disclosure profile differs from initial import",
    )?;
    ledger::validate_receipt(connection, binding, &receipt)?;
    let mut owners = OwnerCache::new();
    let mut members = Members::new();
    for task in receipt.tasks {
        let owner = task.local_record_ids[0].clone();
        let document = member_on(
            connection,
            &owner,
            task.task,
            binding,
            &mut budget,
            &mut owners,
        )?;
        require(
            members.insert(task.task, (owner, document)).is_none(),
            "duplicate bootstrap import member",
        )?;
    }
    Ok((intent.commit, members))
}

/// Capture the original complete admitted import, independently of any newer
/// local baseline. This canonical path does not need Git to be present.
pub(in crate::storage::work) fn initial_seed_on(
    connection: &Connection,
    binding_id: &ObjectId,
    binding: &Binding,
    outcome: &ObjectId,
) -> Result<CheckpointSeed, StoreError> {
    let (commit, members) = initial_members(connection, binding_id, binding, outcome)?;
    let mut root = InventoryRoot::ZERO;
    for (_, document) in members.values() {
        root = root.including(document)?;
    }
    Ok(CheckpointSeed::Incoming {
        snapshot: Box::new(task_sync::delta::MembershipSnapshot {
            commit,
            membership: OutputMembership {
                inventory_root: root,
                profile: binding.profile.clone(),
            },
        }),
        members: BootstrapMembers::InitialImport {
            outcome: outcome.clone(),
        },
    })
}

fn member_on(
    connection: &Connection,
    record: &ObjectId,
    work: WorkId,
    binding: &Binding,
    budget: &mut ledger::view::read::ReadBudget,
    owners: &mut OwnerCache,
) -> Result<TaskDocument, StoreError> {
    if let std::collections::btree_map::Entry::Vacant(entry) = owners.entry(record.clone()) {
        let kind: String = connection.query_row(
            "SELECT object_kind FROM objects WHERE object_id=?1",
            [record.as_str()],
            |row| row.get(0),
        )?;
        let value: serde_json::Value =
            ledger::view::read::object(connection, record, &kind, budget)?;
        let object = CanonicalObject::identified(record, &value)?;
        entry.insert(super::documents::canonical_members(
            &kind,
            &object,
            &binding.project,
        )?);
    }
    owners
        .get(record)
        .and_then(|members| members.get(&work))
        .cloned()
        .ok_or_else(|| {
            super::super::invalid(
                "canonical source member is missing, retired or has a different task key",
            )
        })
}

/// One derivation serves cold admission, doctor and repair. It never opens Git
/// or a retained filesystem artifact, and resolves each distinct owner once.
pub(in crate::storage::work) fn resolve_seed_on(
    connection: &Connection,
    checkpoint_id: &ObjectId,
    binding: &Binding,
    checkpoint: &ManagedCustodyCheckpoint,
) -> Result<Option<ResolvedBootstrap>, StoreError> {
    let Some(seed) = &checkpoint.seed else {
        return Ok(None);
    };
    let (role, output, members) = match seed {
        CheckpointSeed::Incoming { snapshot, members } => ("source", &snapshot.membership, members),
        CheckpointSeed::PreparedExport {
            membership,
            members,
        } => ("export", membership, members),
    };
    let mut budget = ledger::view::read::ReadBudget::default();
    let mut owners = OwnerCache::new();
    let mut resolved = BTreeMap::new();
    match members {
        BootstrapMembers::Captured { members } => {
            for (work, member) in members {
                let (owner, task) = match member {
                    BootstrapMember::Document { task } => {
                        (checkpoint_id.clone(), task.as_ref().clone())
                    }
                    BootstrapMember::Canonical { member } => (
                        member.record_id.clone(),
                        member_on(
                            connection,
                            &member.record_id,
                            *work,
                            binding,
                            &mut budget,
                            &mut owners,
                        )?,
                    ),
                };
                require(task.id == *work, "bootstrap source task key differs")?;
                resolved.insert(*work, (owner, task));
            }
        }
        BootstrapMembers::InitialImport { outcome } => {
            let CheckpointSeed::Incoming { snapshot, .. } = seed else {
                return Err(super::super::invalid(
                    "export seed cannot use an import outcome",
                ));
            };
            let (commit, members) =
                initial_members(connection, &checkpoint.local_cut.binding, binding, outcome)?;
            require(
                commit == snapshot.commit
                    && output.profile.include_notes == binding.profile.include_notes,
                "bootstrap source snapshot or disclosure profile differs from initial import",
            )?;
            resolved = members;
        }
    }
    let tree = TaskTree {
        project: ProjectDocument {
            format: task_sync::FORMAT.into(),
            project_id: binding.project.clone(),
            notes_included: output.profile.include_notes,
        },
        tasks: resolved.values().map(|(_, task)| task.clone()).collect(),
    };
    tree.validate_stored()?;
    let mut root = InventoryRoot::ZERO;
    for (_, document) in resolved.values() {
        root = root.including(document)?;
    }
    require(
        root == output.inventory_root,
        "bootstrap source membership root differs",
    )?;
    Ok(Some(ResolvedBootstrap {
        project: binding.project.clone(),
        role,
        output: output.clone(),
        members: resolved,
    }))
}

pub(in crate::storage::work) fn record_on(
    connection: &Connection,
    checkpoint_id: &ObjectId,
    checkpoint: &ManagedCustodyCheckpoint,
    resolved: &ResolvedBootstrap,
) -> Result<(), StoreError> {
    record_head_on(
        connection,
        checkpoint_id,
        &checkpoint.local_cut.binding,
        resolved,
    )
}

pub(super) fn record_head_on(
    connection: &Connection,
    head_id: &ObjectId,
    binding_id: &ObjectId,
    resolved: &ResolvedBootstrap,
) -> Result<(), StoreError> {
    connection.execute(
        "INSERT INTO work_task_sync_source_heads
        (binding_id,role,checkpoint_id,inventory_root,profile_json) VALUES (?1,?2,?3,?4,?5)
        ON CONFLICT(binding_id,role) DO UPDATE SET checkpoint_id=excluded.checkpoint_id,
            inventory_root=excluded.inventory_root,profile_json=excluded.profile_json",
        params![
            binding_id.as_str(),
            resolved.role,
            head_id.as_str(),
            resolved.output.inventory_root.hex(),
            serde_json::to_vec(&resolved.output.profile)?
        ],
    )?;
    Ok(())
}

pub(in crate::storage::work) fn expected(
    connection: &Connection,
) -> Result<SourceHeads, StoreError> {
    let mut heads = BTreeMap::new();
    let mut statement = connection.prepare("SELECT object_id FROM objects
        INDEXED BY objects_task_sync_exchange_sequence
        WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
          AND (json_type(canonical_json,'$.transition.checkpoint')='object'
            OR json_extract(canonical_json,'$.transition.transition') IN ('delta_prepared','delta_import_applied','planning_successor_applied'))
        ORDER BY json_extract(canonical_json,'$.binding'),json_extract(canonical_json,'$.sequence')")?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut budget = ledger::view::read::ReadBudget::default();
    // Each distinct binding is read and charged once, so the derivation's reads
    // stay within the ledger's admitted unique records and bytes.
    let mut bindings = BTreeMap::<ObjectId, Binding>::new();
    for id in ids {
        let id = ObjectId::from_stored(id)
            .ok_or_else(|| super::super::invalid("invalid bootstrap checkpoint id"))?;
        let event: Event = ledger::view::read::object(connection, &id, EVENT_KIND, &mut budget)?;
        if !bindings.contains_key(&event.binding) {
            let binding =
                ledger::view::read::object(connection, &event.binding, BINDING_KIND, &mut budget)?;
            bindings.insert(event.binding.clone(), binding);
        }
        let binding = &bindings[&event.binding];
        match &event.transition {
            Transition::CustodyCheckpoint { checkpoint } => {
                checkpoint.validate(&event.binding, binding)?;
                if let Some(resolved) = resolve_seed_on(connection, &id, binding, checkpoint)? {
                    heads.insert(
                        (event.binding.to_string(), resolved.role.into()),
                        (id, resolved),
                    );
                }
            }
            _ => {
                if let Some(change) = delta::resolve_on(connection, &id, &event, binding, true)? {
                    let key = (event.binding.to_string(), change.role.into());
                    let (previous, membership) = heads.get_mut(&key).ok_or_else(|| {
                        super::super::invalid("delta membership has no admitted bootstrap")
                    })?;
                    change.require_before(previous, &membership.output)?;
                    change.apply(&mut membership.members, &membership.output)?;
                    membership.output = change.output;
                    TaskTree {
                        project: ProjectDocument {
                            format: task_sync::FORMAT.into(),
                            project_id: membership.project.clone(),
                            notes_included: membership.output.profile.include_notes,
                        },
                        tasks: membership
                            .members
                            .values()
                            .map(|(_, task)| task.clone())
                            .collect(),
                    }
                    .validate_stored()?;
                    *previous = id;
                }
            }
        }
    }
    Ok(heads)
}

pub(in crate::storage::work) fn rebuild(
    connection: &Connection,
) -> Result<Vec<String>, StoreError> {
    let heads = match expected(connection) {
        Ok(heads) => heads,
        Err(StoreError::Sqlite(error)) => return Err(error.into()),
        Err(error) => {
            return Ok(vec![crate::storage::decode_failure_label(
                "work_task_sync_source_heads:underivable".into(),
                &error,
            )]);
        }
    };
    connection.execute("DELETE FROM work_task_sync_source_heads", [])?;
    for ((binding, role), (checkpoint, resolved)) in heads {
        connection.execute(
            "INSERT INTO work_task_sync_source_heads
            (binding_id,role,checkpoint_id,inventory_root,profile_json) VALUES (?1,?2,?3,?4,?5)",
            params![
                binding,
                role,
                checkpoint.as_str(),
                resolved.output.inventory_root.hex(),
                serde_json::to_vec(&resolved.output.profile)?
            ],
        )?;
    }
    Ok(Vec::new())
}

pub(in crate::storage::work) fn verify(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let expected = match expected(connection) {
        Ok(expected) => expected,
        Err(StoreError::Sqlite(error)) => return Err(error.into()),
        Err(error) => {
            invalid.push(crate::storage::decode_failure_label(
                "work_task_sync_source_heads:underivable".into(),
                &error,
            ));
            return Ok(());
        }
    };
    let expected = expected
        .into_iter()
        .map(|((binding, role), (id, resolved))| {
            Ok((
                binding,
                role,
                id.to_string(),
                resolved.output.inventory_root.hex(),
                serde_json::to_vec(&resolved.output.profile)?,
            ))
        })
        .collect::<Result<std::collections::BTreeSet<_>, StoreError>>()?;
    let actual = connection
        .prepare(
            "SELECT binding_id,role,checkpoint_id,inventory_root,profile_json
        FROM work_task_sync_source_heads",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })?
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    *checked += expected.len() + actual.len();
    if expected != actual {
        invalid.push("work_task_sync_source_heads:content".into());
    }
    Ok(())
}
