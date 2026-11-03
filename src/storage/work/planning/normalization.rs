use super::{
    Connection, CreateWorkRequest, DraftPrerequisiteEdge, HashMap, HashSet, PlanningValidation,
    SqliteStore, StoreError, WorkId, WorkOrigin, validate_prerequisite_in_degree,
};

pub(super) fn validate_root_input(request: &CreateWorkRequest) -> Result<(), StoreError> {
    if request.parent_id.is_some() {
        return Err(StoreError::InvalidWork(
            "direct child creation is not allowed; use decompose_work with the parent revision"
                .into(),
        ));
    }
    validate_priority(request.priority, crate::storage::refusal_labels::PRIORITY)?;
    match (request.origin, request.source_snapshot_id.as_ref()) {
        (WorkOrigin::Local, None) | (WorkOrigin::Imported, Some(_)) => Ok(()),
        (WorkOrigin::Local, Some(_)) => Err(StoreError::InvalidWork(
            "local work cannot carry an imported source snapshot".into(),
        )),
        (WorkOrigin::Imported, None) => Err(StoreError::InvalidWork(
            "imported work requires a source snapshot".into(),
        )),
    }
}

pub(super) fn validate_priority(priority: i32, label: &str) -> Result<(), StoreError> {
    if !(0..=4).contains(&priority) {
        return Err(StoreError::InvalidWork(format!(
            "{label} must be an integer from 0 through 4"
        )));
    }
    Ok(())
}

pub(super) fn validate_child_count(
    count: usize,
    validation: PlanningValidation,
) -> Result<(), StoreError> {
    let limit = validation.child_budget();
    if count == 0 || count > limit {
        return Err(StoreError::InvalidWork(format!(
            "decomposition must contain from 1 through {limit} children"
        )));
    }
    Ok(())
}

pub(super) fn validate_child_inputs(
    children: &[crate::domain::ChildWorkDraft],
) -> Result<(), StoreError> {
    let mut keys = HashSet::new();
    for child in children {
        let key = normalize_text(&child.local_key, "child local key")?;
        if !keys.insert(key) {
            return Err(StoreError::InvalidWork(
                "child local keys must be unique".into(),
            ));
        }
        validate_priority(child.priority, "child priority")?;
    }
    Ok(())
}

pub(crate) fn normalize_text(value: &str, label: &str) -> Result<String, StoreError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(StoreError::InvalidWork(format!(
            "{label} must not be empty"
        )));
    }
    Ok(trimmed.to_owned())
}

/// Write-boundary limit only. Retained canonical notes remain readable at any size.
pub(crate) fn normalize_note_text(value: &str, label: &str) -> Result<String, StoreError> {
    let text = normalize_text(value, label)?;
    if text.len() > crate::domain::MAX_WORK_NOTE_BYTES {
        return Err(StoreError::WorkNoteTooLarge {
            bytes: text.len(),
            limit: crate::domain::MAX_WORK_NOTE_BYTES,
        });
    }
    Ok(text)
}

pub(super) fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

pub(in crate::storage::work) fn normalize_strings(values: &[String]) -> Vec<String> {
    let mut normalized = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

/// An acceptance list in the order its author typed it: each criterion
/// trimmed, blanks dropped, and a criterion that repeats an earlier one after
/// trimming dropped in favour of the first. Stored position N is the Nth
/// criterion kept, which is the order `show` numbers.
pub(in crate::storage) fn normalize_acceptance_criteria(values: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(values.len());
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty() && seen.insert(*value))
        .map(str::to_owned)
        .collect()
}

/// Normalizes an acceptance list as [`normalize_acceptance_criteria`] does
/// and carries each binding from the position it was authored at, in the
/// list as typed with blanks counted, to the position its criterion holds
/// once stored; a binding on a repeated criterion lands on its first
/// occurrence.
///
/// # Errors
///
/// Returns [`StoreError::InvalidWork`] when a binding names no criterion of
/// the list as typed, names a blank one, or binds a criterion twice.
pub(crate) fn normalize_acceptance(
    values: &[String],
    bindings: &[crate::domain::AcceptanceBinding],
) -> Result<(Vec<String>, Vec<crate::domain::AcceptanceBinding>), StoreError> {
    let stored = normalize_acceptance_criteria(values);
    let mut carried = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let typed = binding
            .criterion
            .checked_sub(1)
            .and_then(|index| values.get(index))
            .ok_or_else(|| {
                StoreError::InvalidWork(format!(
                    "a binding names criterion {}, but the acceptance list has {} criteria numbered from 1",
                    binding.criterion,
                    values.len()
                ))
            })?;
        let text = typed.trim();
        if text.is_empty() {
            return Err(StoreError::InvalidWork(format!(
                "a binding names criterion {}, which is blank",
                binding.criterion
            )));
        }
        let position = stored
            .iter()
            .position(|criterion| criterion == text)
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection("a normalized criterion lost its text".into())
            })?;
        carried.push(crate::domain::AcceptanceBinding {
            criterion: position + 1,
            requirement: binding.requirement.clone(),
        });
    }
    let bindings = crate::domain::normalize_acceptance_bindings(stored.len(), &carried)
        .map_err(StoreError::InvalidWork)?;
    Ok((stored, bindings))
}

impl SqliteStore {
    /// Read-only admission of a decomposition's prerequisite edges, refused as
    /// decomposition refuses them but before a proposal binds or records an
    /// attempt: each edge onto an existing item resolves, none is the parent
    /// itself, and no child's distinct prerequisites (sibling keys and
    /// resolved items) exceed the per-item bound. Lifecycle, project and
    /// ancestry of the resolved items stay with decomposition.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for an unresolvable ref, an edge onto the
    /// parent, an over-bound child, or a read failure.
    pub(crate) fn refuse_draft_edges(
        &self,
        project: &crate::domain::ProjectId,
        child_keys: &[&str],
        edges: &[DraftPrerequisiteEdge<'_>],
        parent: Option<WorkId>,
    ) -> Result<(), StoreError> {
        let keys = child_keys
            .iter()
            .map(|key| key.trim())
            .collect::<HashSet<_>>();
        let mut per_child: HashMap<&str, (HashSet<&str>, HashSet<WorkId>)> = HashMap::new();
        for edge in edges {
            let prerequisite = edge.prerequisite.trim();
            let entry = per_child.entry(edge.work_key.trim()).or_default();
            if keys.contains(prerequisite) {
                entry.0.insert(prerequisite);
                continue;
            }
            let existing = self.resolve_work_ref(project, edge.prerequisite)?;
            if parent == Some(existing.work_id) {
                return Err(StoreError::WorkDependencyCycle);
            }
            entry.1.insert(existing.work_id);
        }
        for (siblings, existing) in per_child.values() {
            validate_prerequisite_in_degree(siblings.len() + existing.len())?;
        }
        Ok(())
    }

    /// Refuses a binding whose pinned check fingerprint names a stored record,
    /// by a read alone, so a caller can refuse it before navigation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidWork`] for such a pin, or a read failure.
    pub(crate) fn refuse_record_id_pins(
        &self,
        bindings: &[crate::domain::AcceptanceBinding],
    ) -> Result<(), StoreError> {
        refuse_record_id_pins_on(&self.connection, bindings)
    }
}

/// A pinned check is the fingerprint of a check's command, which the host
/// records as `check_fingerprint` on its verification evidence. A value that
/// names a stored record (a minted id, or the fingerprint a frozen record is
/// stored under) is a different kind of value that no evidence can match, so a
/// binding pinned to one could never be satisfied: it is refused where it is
/// authored, whatever its shape. This is a guard at authoring time against
/// the likely mistake, not a proof: a value copied from another store is not
/// known here and is admitted.
pub(in crate::storage::work) fn refuse_record_id_pins_on(
    connection: &Connection,
    bindings: &[crate::domain::AcceptanceBinding],
) -> Result<(), StoreError> {
    for binding in bindings {
        let Some(pinned) = binding.requirement.check_fingerprint.as_ref() else {
            continue;
        };
        let is_record: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id = ?1)",
            [pinned.as_str()],
            |row| row.get(0),
        )?;
        if is_record {
            return Err(StoreError::InvalidWork(format!(
                "criterion {} pins {pinned}, which names a stored record rather than a check's command; pin the check_fingerprint the host recorded on its verification evidence",
                binding.criterion
            )));
        }
    }
    Ok(())
}

pub(in crate::storage) fn short_ref(work_id: WorkId) -> String {
    let simple = work_id.0.simple().to_string();
    format!("w-{}", simple.get(20..).unwrap_or(&simple))
}
