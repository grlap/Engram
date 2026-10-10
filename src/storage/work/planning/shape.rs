//! Shape admission that needs neither the store nor any other item: a
//! revision patch, a new root, and a decomposition's children and the edges
//! among them, checked as storage checks them, so a malformed request is
//! refused before a word moves focus or records an attempt.

use std::collections::{HashMap, HashSet};

use super::{
    PlanningValidation, normalize_acceptance, normalize_note_text, normalize_strings,
    normalize_text, normalize_work_catalog_key, validate_child_count, validate_child_inputs,
    validate_priority,
};
use crate::storage::StoreError;

/// Pure revision shape admission, shared before ambient navigation.
pub(crate) fn validate_revision_patch(
    patch: &crate::domain::WorkRevisionPatch,
) -> Result<(Vec<String>, Vec<String>), StoreError> {
    if patch.clear_assignment && patch.assigned_to.is_some() {
        return Err(StoreError::InvalidWork(
            "assignment cannot be set and cleared in one revision".into(),
        ));
    }
    if patch.clear_evaluation_mode && patch.evaluation_mode.is_some() {
        return Err(StoreError::InvalidWork(
            "evaluation mode cannot be set and cleared in one revision".into(),
        ));
    }
    if patch.clear_deferral && patch.deferred_until.is_some() {
        return Err(StoreError::InvalidWork(
            "deferral cannot be set and cleared in one revision".into(),
        ));
    }
    if let Some(acceptance) = &patch.acceptance
        && (acceptance.is_empty() || acceptance.iter().any(|value| value.trim().is_empty()))
    {
        return Err(StoreError::InvalidWork(
            "acceptance replacement needs at least one nonblank criterion; omit acceptance to leave it unchanged".into(),
        ));
    }
    if patch.labels.is_some() && (!patch.add_labels.is_empty() || !patch.remove_labels.is_empty()) {
        return Err(StoreError::InvalidWork(
            "labels cannot be replaced and incrementally changed in one revision".into(),
        ));
    }
    let add_labels = normalize_strings(&patch.add_labels);
    let remove_labels = normalize_strings(&patch.remove_labels);
    let add_label_keys = add_labels
        .iter()
        .map(|label| normalize_work_catalog_key(label))
        .collect::<HashSet<_>>();
    let remove_label_keys = remove_labels
        .iter()
        .map(|label| normalize_work_catalog_key(label))
        .collect::<HashSet<_>>();
    if !add_label_keys.is_disjoint(&remove_label_keys) {
        return Err(StoreError::InvalidWork(
            "the same label cannot be added and removed in one revision".into(),
        ));
    }
    if patch
        .priority
        .is_some_and(|priority| !(0..=4).contains(&priority))
    {
        return Err(StoreError::InvalidWork(
            "priority must be an integer from 0 through 4".into(),
        ));
    }
    if patch.clear_external && patch.external_ref.is_some() {
        return Err(StoreError::InvalidWork(
            "cannot set and clear the external reference together".into(),
        ));
    }
    if let Some(title) = &patch.title {
        normalize_text(title, crate::storage::refusal_labels::TITLE)?;
    }
    if let Some(outcome) = &patch.outcome {
        normalize_text(outcome, crate::storage::refusal_labels::OUTCOME)?;
    }
    crate::domain::normalize_external_reference(patch.external_ref.as_deref())
        .map_err(StoreError::InvalidWork)?;
    if let Some(acceptance) = &patch.acceptance {
        normalize_acceptance(
            acceptance,
            patch.acceptance_bindings.as_deref().unwrap_or_default(),
        )?;
    } else if let Some(bindings) = &patch.acceptance_bindings {
        // Bindings alone name the stored list; whether each position exists
        // there is checked against it later, but a criterion bound twice, or
        // a position 0, is malformed whatever the list holds.
        let sorted =
            crate::domain::sorted_acceptance_bindings(bindings).map_err(StoreError::InvalidWork)?;
        if sorted.first().is_some_and(|binding| binding.criterion == 0) {
            return Err(StoreError::InvalidWork(
                "a binding names criterion 0, but criteria are numbered from 1".into(),
            ));
        }
    }
    let changed = patch.external_ref.is_some()
        || patch.clear_external
        || patch.title.is_some()
        || patch.outcome.is_some()
        || patch.acceptance.is_some()
        || patch.acceptance_bindings.is_some()
        || patch.kind.is_some()
        || patch.priority.is_some()
        || patch.labels.is_some()
        || !add_labels.is_empty()
        || !remove_labels.is_empty()
        || patch.assigned_to.is_some()
        || patch.clear_assignment
        || patch.deferred_until.is_some()
        || patch.clear_deferral
        || patch.evaluation_mode.is_some()
        || patch.clear_evaluation_mode;
    if !changed {
        return Err(StoreError::InvalidWork("revision patch is empty".into()));
    }
    Ok((add_labels, remove_labels))
}

/// The fields of a new root that need neither the store nor any other item,
/// checked as root creation checks them, so a malformed root is refused
/// before a proposal moves focus or records an attempt.
pub(crate) struct RootDraftShape<'a> {
    pub title: &'a str,
    pub outcome: &'a str,
    pub acceptance: &'a [String],
    pub acceptance_bindings: &'a [crate::domain::AcceptanceBinding],
    pub external_ref: Option<&'a str>,
    pub notes: &'a [String],
    pub priority: i32,
}

/// Pure root shape admission, shared before ambient navigation.
pub(crate) fn validate_root_draft(root: &RootDraftShape<'_>) -> Result<(), StoreError> {
    crate::domain::normalize_initial_work_notes(root.notes).map_err(StoreError::InvalidWork)?;
    validate_priority(root.priority, crate::storage::refusal_labels::PRIORITY)?;
    normalize_text(root.title, crate::storage::refusal_labels::TITLE)?;
    normalize_text(root.outcome, crate::storage::refusal_labels::OUTCOME)?;
    normalize_acceptance(root.acceptance, root.acceptance_bindings)?;
    crate::domain::normalize_external_reference(root.external_ref)
        .map_err(StoreError::InvalidWork)?;
    validate_initial_note_sizes(root.notes)?;
    Ok(())
}

/// Whether the edges between proposed siblings (those whose prerequisite is a
/// sibling's key) form a cycle.
fn proposed_edges_cycle(keys: &HashSet<&str>, edges: &[DraftPrerequisiteEdge<'_>]) -> bool {
    let mut after: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in edges {
        let prerequisite = edge.prerequisite.trim();
        if keys.contains(prerequisite) {
            after
                .entry(edge.work_key.trim())
                .or_default()
                .push(prerequisite);
        }
    }
    let mut on_path = HashSet::new();
    let mut finished = HashSet::new();
    keys.iter()
        .any(|&key| reaches_own_path(key, &after, &mut on_path, &mut finished))
}

/// Depth-first search with the usual three states: unvisited, on the current
/// path, finished. Reaching a key already on the current path is a cycle.
fn reaches_own_path<'a>(
    key: &'a str,
    after: &HashMap<&'a str, Vec<&'a str>>,
    on_path: &mut HashSet<&'a str>,
    finished: &mut HashSet<&'a str>,
) -> bool {
    if finished.contains(key) {
        return false;
    }
    if !on_path.insert(key) {
        return true;
    }
    let cycle = after.get(key).is_some_and(|next| {
        next.iter()
            .any(|&next| reaches_own_path(next, after, on_path, finished))
    });
    on_path.remove(key);
    finished.insert(key);
    cycle
}

/// Each initial note within the note size bound, as the initial notes are
/// appended under: refused here before anything is recorded.
fn validate_initial_note_sizes(notes: &[String]) -> Result<(), StoreError> {
    for note in notes {
        normalize_note_text(note, crate::storage::refusal_labels::NOTE_SUMMARY)?;
    }
    Ok(())
}

/// One proposed prerequisite edge as the caller typed it: the child it gates
/// and the prerequisite, a sibling's local key or an existing item's ref.
pub(crate) struct DraftPrerequisiteEdge<'a> {
    pub work_key: &'a str,
    pub prerequisite: &'a str,
}

/// Pure decomposition shape admission, shared before ambient navigation: the
/// child count, initial notes, local keys and priorities, each child's own
/// fields, and the edges among the proposed children, checked as
/// decomposition checks them. The parent's lifecycle, authority and budget,
/// and edges onto existing items, need the store and stay there.
pub(crate) fn validate_decomposition_drafts(
    children: &[crate::domain::ChildWorkDraft],
    edges: &[DraftPrerequisiteEdge<'_>],
) -> Result<(), StoreError> {
    validate_child_count(children.len(), PlanningValidation::Immediate)?;
    let count = children.iter().fold(0_usize, |count, child| {
        count.saturating_add(child.notes.len())
    });
    crate::domain::validate_initial_work_note_count(count).map_err(StoreError::InvalidWork)?;
    for (index, child) in children.iter().enumerate() {
        crate::domain::normalize_initial_work_notes(&child.notes)
            .map_err(|reason| StoreError::InvalidWork(format!("child {}: {reason}", index + 1)))?;
    }
    validate_child_inputs(children)?;
    for draft in children {
        normalize_acceptance(&draft.acceptance, &draft.acceptance_bindings)?;
        crate::domain::normalize_external_reference(draft.external_ref.as_deref())
            .map_err(StoreError::InvalidWork)?;
        normalize_text(&draft.title, crate::storage::refusal_labels::CHILD_TITLE)?;
        normalize_text(
            &draft.outcome,
            crate::storage::refusal_labels::CHILD_OUTCOME,
        )?;
    }
    let keys = children
        .iter()
        .map(|child| child.local_key.trim())
        .collect::<HashSet<_>>();
    for edge in edges {
        let work_key = edge.work_key.trim();
        if !keys.contains(work_key) {
            return Err(StoreError::InvalidWork(
                crate::storage::refusal_labels::unknown_child_edge(work_key),
            ));
        }
        // A prerequisite naming a sibling's key is that sibling; naming the
        // gated child itself is a cycle whatever the store holds.
        if edge.prerequisite.trim() == work_key {
            return Err(StoreError::WorkDependencyCycle);
        }
    }
    // Edges among the proposed siblings alone can close a longer loop
    // (a after b, b after a), also whatever the store holds.
    if proposed_edges_cycle(&keys, edges) {
        return Err(StoreError::WorkDependencyCycle);
    }
    for child in children {
        validate_initial_note_sizes(&child.notes)?;
    }
    Ok(())
}
