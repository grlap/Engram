//! Incremental canonical root state and its exact current projection.
//!
//! Heads are real objects. Full-state checksums bind every projected member,
//! including absence; they are never passed to the object resolver as addresses.
use std::collections::{BTreeMap, HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::domain::{
    RootExecutionDelta, RootExecutionHeader, RootExecutionMember, RootExecutionRef,
};
use crate::{CanonicalObject, ObjectHash, RootExecution, RootExecutionId, SqliteStore, StoreError};

use super::feeds::load_typed_work_object;
use crate::domain::{
    CompletionWaiver, RequiredChildWaiver, RootContribution, SessionId, WorkRunId,
};
use std::cmp::Ordering;

pub(in crate::storage) const KIND: &str = "work_root_delta";

#[cfg(test)]
mod tests;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
struct Cost {
    member_hashes: usize,
    member_bytes: usize,
    checksums: usize,
    checksum_bytes: usize,
    assemblies: usize,
    resolves: usize,
    head_loads: usize,
}

#[cfg(test)]
thread_local! {
    static COST: std::cell::RefCell<Cost> = const { std::cell::RefCell::new(Cost {
        member_hashes: 0, member_bytes: 0, checksums: 0, checksum_bytes: 0,
        assemblies: 0, resolves: 0, head_loads: 0,
    }) };
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(format!("root state: {message}"))
}

fn header(value: &RootExecution) -> RootExecutionHeader {
    RootExecutionHeader {
        schema_version: value.schema_version,
        root_execution_id: value.root_execution_id,
        project_id: value.project_id.clone(),
        root_id: value.root_id,
        generation: value.generation,
        state: value.state,
        revision: value.revision,
        created_at: value.created_at,
        updated_at: value.updated_at,
    }
}

fn empty(value: &RootExecutionHeader) -> RootExecution {
    RootExecution {
        schema_version: value.schema_version,
        root_execution_id: value.root_execution_id,
        project_id: value.project_id.clone(),
        root_id: value.root_id,
        generation: value.generation,
        state: value.state,
        revision: value.revision,
        created_at: value.created_at,
        updated_at: value.updated_at,
        run_ids: Vec::new(),
        required_child_seals: Vec::new(),
        required_child_waivers: Vec::new(),
        expected_contributors: Vec::new(),
        contributions: Vec::new(),
        waivers: Vec::new(),
    }
}

fn member_hash(value: &RootExecutionMember) -> Result<ObjectHash, StoreError> {
    let object = CanonicalObject::freeze(value)?;
    #[cfg(test)]
    COST.with_borrow_mut(|cost| {
        cost.member_hashes += 1;
        cost.member_bytes += object.bytes().len();
    });
    Ok(object.hash().clone())
}

fn checksum(value: &RootExecution) -> Result<ObjectHash, StoreError> {
    let object = CanonicalObject::freeze(value)?;
    #[cfg(test)]
    COST.with_borrow_mut(|cost| {
        cost.checksums += 1;
        cost.checksum_bytes += object.bytes().len();
    });
    Ok(object.hash().clone())
}

fn members(value: &RootExecution) -> Result<BTreeMap<ObjectHash, RootExecutionMember>, StoreError> {
    let values = value
        .run_ids
        .iter()
        .copied()
        .map(RootExecutionMember::Run)
        .chain(
            value
                .required_child_seals
                .iter()
                .cloned()
                .map(RootExecutionMember::ChildSeal),
        )
        .chain(
            value
                .required_child_waivers
                .iter()
                .cloned()
                .map(RootExecutionMember::ChildWaiver),
        )
        .chain(
            value
                .expected_contributors
                .iter()
                .cloned()
                .map(RootExecutionMember::Contributor),
        )
        .chain(
            value
                .contributions
                .iter()
                .cloned()
                .map(RootExecutionMember::Contribution),
        )
        .chain(
            value
                .waivers
                .iter()
                .cloned()
                .map(RootExecutionMember::Waiver),
        );
    let mut result = BTreeMap::new();
    for member in values {
        if result.insert(member_hash(&member)?, member).is_some() {
            return Err(invalid("duplicate member"));
        }
    }
    Ok(result)
}

pub(super) fn compare_runs(a: &WorkRunId, b: &WorkRunId) -> Ordering {
    a.0.cmp(&b.0)
}
pub(super) fn compare_seals(a: &ObjectHash, b: &ObjectHash) -> Ordering {
    a.cmp(b)
}
pub(super) fn compare_child_waivers(a: &RequiredChildWaiver, b: &RequiredChildWaiver) -> Ordering {
    a.work_id.0.cmp(&b.work_id.0)
}
pub(super) fn compare_contributors(a: &SessionId, b: &SessionId) -> Ordering {
    a.0.cmp(&b.0)
}
pub(super) fn compare_contributions(a: &RootContribution, b: &RootContribution) -> Ordering {
    a.participant
        .0
        .cmp(&b.participant.0)
        .then_with(|| a.object.cmp(&b.object))
}
pub(super) fn compare_waivers(a: &CompletionWaiver, b: &CompletionWaiver) -> Ordering {
    a.participant.0.cmp(&b.participant.0)
}

fn require_order(value: &RootExecution) -> Result<(), StoreError> {
    let checks = [
        (
            "run_ids",
            value
                .run_ids
                .is_sorted_by(|a, b| compare_runs(a, b).is_le()),
        ),
        (
            "required_child_seals",
            value
                .required_child_seals
                .is_sorted_by(|a, b| compare_seals(a, b).is_le()),
        ),
        (
            "required_child_waivers",
            value
                .required_child_waivers
                .is_sorted_by(|a, b| compare_child_waivers(a, b).is_le()),
        ),
        (
            "expected_contributors",
            value
                .expected_contributors
                .is_sorted_by(|a, b| compare_contributors(a, b).is_le()),
        ),
        (
            "contributions",
            value
                .contributions
                .is_sorted_by(|a, b| compare_contributions(a, b).is_le()),
        ),
        (
            "waivers",
            value
                .waivers
                .is_sorted_by(|a, b| compare_waivers(a, b).is_le()),
        ),
    ];
    for (collection, ordered) in checks {
        if !ordered {
            return Err(invalid(&format!(
                "noncanonical collection order: {collection}"
            )));
        }
    }
    if value
        .required_child_waivers
        .windows(2)
        .any(|pair| pair[0].work_id == pair[1].work_id)
        || value
            .waivers
            .windows(2)
            .any(|pair| pair[0].participant == pair[1].participant)
    {
        return Err(invalid("conflicting member identities"));
    }
    Ok(())
}

fn assemble(
    metadata: &RootExecutionHeader,
    values: &BTreeMap<ObjectHash, RootExecutionMember>,
) -> Result<RootExecution, StoreError> {
    #[cfg(test)]
    COST.with_borrow_mut(|cost| cost.assemblies += 1);
    let mut result = empty(metadata);
    for member in values.values() {
        match member {
            RootExecutionMember::Run(value) => result.run_ids.push(*value),
            RootExecutionMember::ChildSeal(value) => {
                result.required_child_seals.push(value.clone());
            }
            RootExecutionMember::ChildWaiver(value) => {
                result.required_child_waivers.push(value.clone());
            }
            RootExecutionMember::Contributor(value) => {
                result.expected_contributors.push(value.clone());
            }
            RootExecutionMember::Contribution(value) => result.contributions.push(value.clone()),
            RootExecutionMember::Waiver(value) => result.waivers.push(value.clone()),
        }
    }
    result.run_ids.sort_by(compare_runs);
    result.required_child_seals.sort_by(compare_seals);
    result.required_child_waivers.sort_by(compare_child_waivers);
    result.expected_contributors.sort_by(compare_contributors);
    result.contributions.sort_by(compare_contributions);
    result.waivers.sort_by(compare_waivers);
    require_order(&result)?;
    Ok(result)
}

fn reference(value: &RootExecutionHeader, hash: ObjectHash) -> RootExecutionRef {
    RootExecutionRef {
        root_execution_id: value.root_execution_id,
        project_id: value.project_id.clone(),
        root_id: value.root_id,
        generation: value.generation,
        head: hash,
    }
}

fn load_head(
    connection: &Connection,
    value: &RootExecutionRef,
) -> Result<RootExecutionDelta, StoreError> {
    #[cfg(test)]
    COST.with_borrow_mut(|cost| cost.head_loads += 1);
    let json: serde_json::Value = load_typed_work_object(connection, &value.head, KIND)?;
    let head: RootExecutionDelta = serde_json::from_value(json.clone())?;
    if serde_json::to_value(&head)? != json {
        return Err(invalid("unexpected canonical head fields"));
    }
    if head.header.schema_version != crate::domain::SCHEMA_VERSION
        || head.header.root_execution_id != value.root_execution_id
        || head.header.project_id != value.project_id
        || head.header.root_id != value.root_id
        || head.header.generation != value.generation
        || head.header.generation < 1
        || head.header.revision < 1
    {
        return Err(invalid("head identity or generation mismatch"));
    }
    Ok(head)
}

/// Current rows are verified against the entire state, not a count or the last
/// changed member. This read deliberately remains linear in current state size.
pub(super) fn projected(
    connection: &Connection,
    id: RootExecutionId,
) -> Result<(RootExecution, RootExecutionRef), StoreError> {
    let loaded = projected_with_head(connection, id)?;
    Ok((loaded.value, loaded.address))
}

struct LoadedRoot {
    value: RootExecution,
    address: RootExecutionRef,
    head: RootExecutionDelta,
    members: BTreeMap<ObjectHash, RootExecutionMember>,
}

/// A validated, persisted head borrowed from its writer connection.
/// Event append checks the same connection and event state, then queries the
/// stored head again to reject a stale proof. Connection pointer equality does
/// not establish transaction identity. Fields are private; this is not a cache.
pub(super) struct WrittenRoot<'a> {
    connection: &'a Connection,
    value: RootExecution,
    address: RootExecutionRef,
}

impl WrittenRoot<'_> {
    pub(super) fn value(&self) -> &RootExecution {
        &self.value
    }

    pub(super) fn event_ref(
        &self,
        connection: &Connection,
        value: Option<&RootExecution>,
    ) -> Result<RootExecutionRef, StoreError> {
        if !std::ptr::eq(self.connection, connection) || value != Some(&self.value) {
            return Err(invalid(
                "written head belongs to another transaction or event state",
            ));
        }
        let current: Option<String> = connection
            .query_row(
                "SELECT head_hash FROM work_root_executions WHERE root_execution_id = ?1",
                [self.address.root_execution_id.0.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if current.as_deref() != Some(self.address.head.as_str()) {
            return Err(invalid("written head is no longer current"));
        }
        Ok(self.address.clone())
    }
}

fn projected_with_head(
    connection: &Connection,
    id: RootExecutionId,
) -> Result<LoadedRoot, StoreError> {
    let (bytes, stored_hash, scalars): (Vec<u8>, String, bool) = connection
        .query_row(
            "SELECT header_json, head_hash,
             root_execution_id = json_extract(header_json, '$.root_execution_id') AND
             project_id = json_extract(header_json, '$.project_id') AND
             root_id = json_extract(header_json, '$.root_id') AND
             generation = json_extract(header_json, '$.generation') AND
             state = json_extract(header_json, '$.state') AND
             revision = json_extract(header_json, '$.revision')
         FROM work_root_executions WHERE root_execution_id = ?1",
            [id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .ok_or_else(|| invalid("current projection is missing"))?;
    let metadata: RootExecutionHeader = serde_json::from_slice(&bytes)?;
    if serde_json::to_value(&metadata)? != serde_json::from_slice::<serde_json::Value>(&bytes)? {
        return Err(invalid("unexpected header fields"));
    }
    let hash = ObjectHash::from_stored(stored_hash.clone())
        .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
    let address = reference(&metadata, hash);
    let head = load_head(connection, &address)?;
    if !scalars || metadata.root_execution_id != id || head.header != metadata {
        return Err(invalid("header differs from canonical head"));
    }
    let mut statement = connection.prepare(
        "SELECT member_hash, member_json FROM work_root_members WHERE root_execution_id = ?1 ORDER BY member_hash",
    )?;
    let mut values = BTreeMap::new();
    for row in statement.query_map([id.0.to_string()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })? {
        let (stored_hash, bytes) = row?;
        let value: RootExecutionMember = serde_json::from_slice(&bytes)?;
        if serde_json::to_value(&value)? != serde_json::from_slice::<serde_json::Value>(&bytes)? {
            return Err(invalid("unexpected member fields"));
        }
        let hash = member_hash(&value)?;
        if hash.as_str() != stored_hash || values.insert(hash, value).is_some() {
            return Err(invalid("member key mismatch"));
        }
    }
    let result = assemble(&metadata, &values)?;
    if checksum(&result)? != head.state_checksum {
        return Err(invalid("current members differ from full-state checksum"));
    }
    Ok(LoadedRoot {
        value: result,
        address,
        head,
        members: values,
    })
}

pub(super) fn current_ref(
    connection: &Connection,
    value: &RootExecution,
) -> Result<RootExecutionRef, StoreError> {
    let (stored, address) = projected(connection, value.root_execution_id)?;
    if stored != *value {
        return Err(invalid("event state differs from persisted head"));
    }
    Ok(address)
}

/// Live completion proves only the requested waiver facts, not historical
/// full-state checksums. Start at a fully verified current projection, undo
/// each canonical delta to the empty origin, and require each cited addition
/// on that exact path with no subsequent removal (even followed by re-add).
/// Doctor/export retain `resolve` and its exhaustive checksum verification.
///
/// One head load per delta, one current-state checksum, and member hashing
/// proportional to current members, requested facts, and actual delta payloads.
/// No collection of historical full states or per-witness chain replay.
pub(super) fn verify_waiver_witnesses(
    connection: &Connection,
    execution: &RootExecution,
    witnesses: &[(RootExecutionRef, RequiredChildWaiver)],
) -> Result<(), StoreError> {
    if witnesses.is_empty() {
        return Ok(());
    }
    let LoadedRoot {
        value,
        mut address,
        mut head,
        mut members,
    } = projected_with_head(connection, execution.root_execution_id)?;
    if value != *execution {
        return Err(invalid("waiver proof differs from current root state"));
    }
    super::query::verify_root_execution_reference_on(connection, &address)?;
    let mut pending = HashMap::<_, Vec<_>>::new();
    let mut pending_members = HashSet::new();
    for (target, waiver) in witnesses {
        if reference(&head.header, target.head.clone()) != *target {
            return Err(invalid(
                "waiver witness belongs to another root or generation",
            ));
        }
        let member = RootExecutionMember::ChildWaiver(waiver.clone());
        let hash = member_hash(&member)?;
        if members.get(&hash) != Some(&member) || !pending_members.insert(hash.clone()) {
            return Err(invalid(
                "waiver witness is absent or duplicated in current state",
            ));
        }
        pending
            .entry(target.head.clone())
            .or_default()
            .push((hash, member));
    }
    loop {
        let mut added = HashMap::new();
        // Reverse the forward application order: undo additions, then removals.
        for member in &head.added {
            let hash = member_hash(member)?;
            if members.remove(&hash).as_ref() != Some(member)
                || added.insert(hash, member).is_some()
            {
                return Err(invalid(
                    "waiver chain adds an absent or duplicate result member",
                ));
            }
        }
        for member in &head.removed {
            let hash = member_hash(member)?;
            if pending_members.contains(&hash) {
                return Err(invalid("waiver witness was removed on the current chain"));
            }
            if members.insert(hash, member.clone()).is_some() {
                return Err(invalid(
                    "waiver chain removes a member still present in the result",
                ));
            }
        }
        if let Some(requests) = pending.remove(&address.head) {
            for (hash, member) in requests {
                if added.get(&hash).copied() != Some(&member) {
                    return Err(invalid("waiver witness head does not add the exact fact"));
                }
                pending_members.remove(&hash);
            }
        }
        let Some(predecessor) = head.predecessor.clone() else {
            if head.sequence != 0
                || head.previous_revision.is_some()
                || !head.added.is_empty()
                || !head.removed.is_empty()
                || !members.is_empty()
            {
                return Err(invalid("waiver chain does not end at an empty origin"));
            }
            break;
        };
        address.head = predecessor;
        let prior = load_head(connection, &address)?;
        if prior.sequence.checked_add(1) != Some(head.sequence)
            || head.previous_revision != Some(prior.header.revision)
            || head.header.revision < prior.header.revision
            || head.header.created_at != prior.header.created_at
        {
            return Err(invalid(
                "waiver chain sequence, revision or origin discontinuity",
            ));
        }
        head = prior;
    }
    if !pending.is_empty() {
        return Err(invalid(
            "waiver witness is not an ancestor of the current head",
        ));
    }
    Ok(())
}

/// Replay only the addressed generation, never a project-feed prefix or a newer
/// generation. Every predecessor is a real object and every intermediate result
/// is checked. Full-state hashing at each step is quadratic when history and
/// membership grow together. This is the exhaustive audit/read contract, not
/// the narrower live completion waiver-fact proof above.
pub(super) fn resolve(
    connection: &Connection,
    address: &RootExecutionRef,
) -> Result<RootExecution, StoreError> {
    #[cfg(test)]
    COST.with_borrow_mut(|cost| cost.resolves += 1);
    let mut chain = Vec::new();
    let mut cursor = address.clone();
    let mut expected_sequence = None;
    loop {
        let head = load_head(connection, &cursor)?;
        if expected_sequence.is_some_and(|expected| expected != head.sequence) {
            return Err(invalid("noncontiguous delta sequence"));
        }
        let predecessor = head.predecessor.clone();
        let sequence = head.sequence;
        // Keep addresses, not decoded member payloads, during the backward
        // walk. The forward walk verifies each object again before applying it.
        chain.push(cursor.head.clone());
        match predecessor {
            Some(hash) => {
                expected_sequence = Some(
                    sequence
                        .checked_sub(1)
                        .ok_or_else(|| invalid("origin has a predecessor"))?,
                );
                cursor.head = hash;
            }
            None if sequence == 0 => break,
            None => return Err(invalid("missing generation origin")),
        }
    }
    let mut values = BTreeMap::new();
    let mut previous: Option<RootExecutionHeader> = None;
    let mut result = None;
    for hash in chain.into_iter().rev() {
        let mut step = address.clone();
        step.head = hash;
        let head = load_head(connection, &step)?;
        if let Some(prior) = &previous {
            if head.previous_revision != Some(prior.revision)
                || head.header.revision < prior.revision
                || head.header.created_at != prior.created_at
            {
                return Err(invalid("delta revision or origin discontinuity"));
            }
        } else if head.previous_revision.is_some()
            || !head.added.is_empty()
            || !head.removed.is_empty()
        {
            return Err(invalid("origin is not empty"));
        }
        for member in &head.removed {
            if values.remove(&member_hash(member)?).as_ref() != Some(member) {
                return Err(invalid("delta removes an absent member"));
            }
        }
        for member in &head.added {
            if values
                .insert(member_hash(member)?, member.clone())
                .is_some()
            {
                return Err(invalid("delta adds an existing member"));
            }
        }
        let state = assemble(&head.header, &values)?;
        if checksum(&state)? != head.state_checksum {
            return Err(invalid("replayed state checksum mismatch"));
        }
        previous = Some(head.header);
        result = Some(state);
    }
    result.ok_or_else(|| invalid("empty history"))
}

pub(super) fn initialize(
    transaction: &Transaction<'_>,
    value: &RootExecution,
) -> Result<(), StoreError> {
    let metadata = header(value);
    let origin = RootExecutionDelta {
        state_checksum: checksum(&empty(&metadata))?,
        header: metadata.clone(),
        sequence: 0,
        predecessor: None,
        previous_revision: None,
        removed: Vec::new(),
        added: Vec::new(),
    };
    let object = CanonicalObject::freeze(&origin)?;
    SqliteStore::insert_object(transaction, KIND, &object)?;
    transaction.execute(
        "INSERT INTO work_root_executions (
             root_execution_id, project_id, root_id, generation, state, revision,
             created_at_ms, updated_at_ms, header_json, head_hash
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            value.root_execution_id.0.to_string(),
            value.project_id.0,
            value.root_id.0.to_string(),
            value.generation,
            super::planning::encode_state(value.state)?,
            value.revision,
            value.created_at.timestamp_millis(),
            value.updated_at.timestamp_millis(),
            serde_json::to_vec(&metadata)?,
            object.hash().as_str()
        ],
    )?;
    persist(transaction, value)
}

pub(super) fn persist(
    transaction: &Transaction<'_>,
    value: &RootExecution,
) -> Result<(), StoreError> {
    let prior = projected_with_head(transaction, value.root_execution_id)?;
    persist_loaded(transaction, value, prior).map(|_| ())
}

/// The completion head must directly extend the exact state named by its
/// seal. Reuse the persistence read; do not add another full-state checksum.
pub(super) fn persist_completion(
    transaction: &Transaction<'_>,
    value: &RootExecution,
    pre_seal: &RootExecutionRef,
) -> Result<(), StoreError> {
    let prior = projected_with_head(transaction, value.root_execution_id)?;
    if prior.address != *pre_seal || prior.value.revision.checked_add(1) != Some(value.revision) {
        return Err(invalid(
            "completion does not extend its exact pre-seal state",
        ));
    }
    persist_loaded(transaction, value, prior).map(|_| ())
}

/// Check the canonical pair without replaying historical full states on a
/// live child-seal read. Exhaustive audit still checks every state checksum.
pub(super) fn verify_completion_predecessor(
    connection: &Connection,
    pre_seal: &RootExecutionRef,
    completed: &RootExecutionRef,
) -> Result<(), StoreError> {
    let before = load_head(connection, pre_seal)?;
    let after = load_head(connection, completed)?;
    let mut same_generation = pre_seal.clone();
    same_generation.head = completed.head.clone();
    if same_generation != *completed
        || after.predecessor.as_ref() != Some(&pre_seal.head)
        || before.sequence.checked_add(1) != Some(after.sequence)
        || after.previous_revision != Some(before.header.revision)
        || before.header.revision.checked_add(1) != Some(after.header.revision)
        || before.header.created_at != after.header.created_at
    {
        return Err(invalid(
            "completion does not bind its exact pre-seal predecessor",
        ));
    }
    Ok(())
}

/// Read and validate once, change only the in-memory value, then persist in
/// this same writer transaction. The returned proof is borrowed from that
/// transaction and event append refuses it if the stored head has advanced.
pub(super) fn update<'a>(
    transaction: &'a Transaction<'_>,
    id: RootExecutionId,
    change: impl FnOnce(&mut RootExecution),
) -> Result<WrittenRoot<'a>, StoreError> {
    let prior = projected_with_head(transaction, id)?;
    super::query::verify_root_execution_reference_on(transaction, &prior.address)?;
    let mut value = prior.value.clone();
    change(&mut value);
    let address = persist_loaded(transaction, &value, prior)?;
    Ok(WrittenRoot {
        connection: transaction,
        value,
        address,
    })
}

fn persist_loaded(
    transaction: &Transaction<'_>,
    value: &RootExecution,
    LoadedRoot {
        value: prior,
        address,
        head: prior_head,
        members: old,
    }: LoadedRoot,
) -> Result<RootExecutionRef, StoreError> {
    if prior == *value {
        return Ok(address);
    }
    let metadata = header(value);
    if reference(&metadata, address.head.clone()) != address
        || metadata.schema_version != prior.schema_version
    {
        return Err(invalid("root identity, schema or generation changed"));
    }
    if metadata.created_at != prior.created_at {
        return Err(invalid("root origin created_at changed"));
    }
    if metadata.revision < prior.revision {
        return Err(invalid("root revision moved backwards"));
    }
    require_order(value)?;
    let new = members(value)?;
    let removed = old
        .iter()
        .filter(|(hash, _)| !new.contains_key(*hash))
        .map(|(hash, member)| (hash.clone(), member.clone()))
        .collect::<Vec<_>>();
    let added = new
        .iter()
        .filter(|(hash, _)| !old.contains_key(*hash))
        .map(|(hash, member)| (hash.clone(), member.clone()))
        .collect::<Vec<_>>();
    let head = RootExecutionDelta {
        header: metadata.clone(),
        sequence: prior_head
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("state sequence overflow"))?,
        predecessor: Some(address.head),
        previous_revision: Some(prior.revision),
        removed: removed.iter().map(|(_, member)| member.clone()).collect(),
        added: added.iter().map(|(_, member)| member.clone()).collect(),
        state_checksum: checksum(value)?,
    };
    let object = CanonicalObject::freeze(&head)?;
    SqliteStore::insert_object(transaction, KIND, &object)?;
    for (hash, _) in &removed {
        let changed = transaction.execute(
            "DELETE FROM work_root_members WHERE root_execution_id = ?1 AND member_hash = ?2",
            params![value.root_execution_id.0.to_string(), hash.as_str()],
        )?;
        if changed != 1 {
            return Err(invalid("member removal lost its row"));
        }
    }
    for (hash, member) in &added {
        transaction.execute("INSERT INTO work_root_members (root_execution_id, member_hash, member_json) VALUES (?1, ?2, ?3)",
            params![value.root_execution_id.0.to_string(), hash.as_str(), serde_json::to_vec(member)?])?;
    }
    let changed = transaction.execute(
        "UPDATE work_root_executions SET state = ?2, revision = ?3, updated_at_ms = ?4, header_json = ?5, head_hash = ?6 WHERE root_execution_id = ?1",
        params![value.root_execution_id.0.to_string(), super::planning::encode_state(value.state)?, value.revision,
            value.updated_at.timestamp_millis(), serde_json::to_vec(&metadata)?, object.hash().as_str()],
    )?;
    if changed != 1 {
        return Err(invalid("head update lost its row"));
    }
    Ok(reference(&metadata, object.hash().clone()))
}

/// The exhaustive audit uses canonical history, never the projection as its
/// expected state. A missing or extra generation is also an integrity failure.
pub(super) fn verify_projections(
    connection: &Connection,
    expected: &std::collections::HashMap<String, RootExecutionRef>,
    event_heads: &[RootExecutionRef],
    checked: &mut usize,
    failures: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut unseen = expected.clone();
    let mut reachable = std::collections::HashSet::new();
    let mut statement = connection
        .prepare("SELECT root_execution_id FROM work_root_executions ORDER BY root_execution_id")?;
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        let id = row?;
        *checked += 1;
        let valid = unseen.remove(&id).is_some_and(|address| {
            projected(connection, address.root_execution_id).is_ok_and(|(state, actual)| {
                if actual != address
                    || !resolve(connection, &address).is_ok_and(|canonical| canonical == state)
                {
                    return false;
                }
                let mut cursor = address;
                loop {
                    if !reachable.insert(cursor.head.clone()) {
                        break;
                    }
                    let Ok(head) = load_head(connection, &cursor) else {
                        return false;
                    };
                    let Some(previous) = head.predecessor else {
                        break;
                    };
                    cursor.head = previous;
                }
                true
            })
        });
        if !valid {
            failures.push(format!("work_root_execution:{id}"));
        }
    }
    for id in unseen.keys() {
        failures.push(format!("work_root_execution:{id}:missing_projection"));
    }
    let mut statement = connection.prepare(
        "SELECT member.root_execution_id, member.member_hash FROM work_root_members member
         LEFT JOIN work_root_executions root ON root.root_execution_id = member.root_execution_id
         WHERE root.root_execution_id IS NULL",
    )?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, hash) = row?;
        *checked += 1;
        failures.push(format!("work_root_member:{id}:{hash}:missing_generation"));
    }
    // Every historical event must address this generation's verified chain,
    // not merely carry a plausible root id. Project-feed order may reuse a
    // head, but cannot move backwards through root-state sequence numbers.
    let mut sequences = std::collections::HashMap::new();
    for address in event_heads {
        *checked += 1;
        let valid = reachable.contains(&address.head)
            && load_head(connection, address).is_ok_and(|head| {
                let prior = sequences.insert(address.root_execution_id, head.sequence);
                prior.is_none_or(|sequence| sequence <= head.sequence)
            });
        if !valid {
            failures.push(format!(
                "work_root_event_head:{}:invalid_history",
                address.head
            ));
        }
    }
    let mut statement = connection
        .prepare("SELECT object_hash FROM objects WHERE object_kind = 'work_root_delta'")?;
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        let stored = row?;
        *checked += 1;
        if !ObjectHash::from_stored(stored.clone()).is_some_and(|hash| reachable.contains(&hash)) {
            failures.push(format!("work_root_delta:{stored}:unbound_history"));
        }
    }
    Ok(())
}
