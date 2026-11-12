use super::*;
use crate::task_sync::retention::successor::{self, RetainedSuccessor};

fn selected(f: &mut Fixture) -> (DeltaIntent, PlanningSuccessorRecipe) {
    let original = f.intent.clone();
    f.intent.operation = uuid::Uuid::new_v4();
    f.intent.key = "fresh successor".into();
    f.intent.candidate.operation = f.intent.operation;
    f.payload.operation = f.intent.operation;
    // A completed local run or another preserved local edit can make the
    // resolved local membership differ from the original source snapshot.
    f.intent.candidate.output.inventory_root = f
        .intent
        .candidate
        .output
        .inventory_root
        .including(&document("preserved local member"))
        .unwrap();
    f.payload.output = f.intent.candidate.output.clone();
    refresh(f);
    let request = PlanningSuccessorRequest {
        deferred: ObjectId::mint(),
        hold: ObjectId::mint(),
        original_prepared: ObjectId::mint(),
        original_candidate: original.candidate.content_fingerprint.clone(),
        intent: f.intent.clone(),
    };
    let mut recipe = PlanningSuccessorRecipe::new(request, Vec::new(), &[]).unwrap();
    recipe
        .bind_chain(ObjectId::mint(), 8, chrono::Utc::now())
        .unwrap();
    (original, recipe)
}

#[test]
fn successor_keeps_original_source_and_distinct_resolved_local_membership() {
    let mut f = fixture();
    let (original, recipe) = selected(&mut f);
    original.validate(&f.binding_id, &f.binding).unwrap();
    recipe
        .request
        .validate(&f.binding_id, &f.binding, &original)
        .unwrap();
    f.payload
        .validate_successor(&f.binding_id, &f.binding, &f.intent)
        .unwrap();
    assert!(
        f.payload
            .validate(&f.binding_id, &f.binding, &f.intent)
            .is_err()
    );
    let event = recipe
        .prepared_event(&f.binding_id, &f.binding, f.payload.clone())
        .unwrap();
    assert!(matches!(
        event.transition,
        crate::task_sync::ledger::Transition::PlanningSuccessorPrepared { .. }
    ));
    let mut changed = recipe.request.clone();
    let DeltaBasis::Incoming { source_after, .. } = &mut changed.intent.basis else {
        unreachable!()
    };
    source_after.membership.inventory_root = changed.intent.candidate.output.inventory_root;
    assert!(
        changed
            .validate(&f.binding_id, &f.binding, &original)
            .is_err()
    );
}

#[test]
fn historical_successor_checks_exact_envelope_without_readmitting_payload() {
    let mut f = fixture();
    let (_, recipe) = selected(&mut f);
    let event = recipe
        .prepared_event(&f.binding_id, &f.binding, f.payload.clone())
        .unwrap();
    let mut historical = event.clone();
    let crate::task_sync::ledger::Transition::PlanningSuccessorPrepared { payload, .. } =
        &mut historical.transition
    else {
        unreachable!()
    };
    payload
        .documents
        .values_mut()
        .flatten()
        .next()
        .unwrap()
        .title
        .clear();
    assert!(
        recipe
            .prepared_event(&f.binding_id, &f.binding, (**payload).clone())
            .is_err()
    );
    recipe
        .validate_historical_prepared(&historical, &f.binding)
        .unwrap();
    for fault in [
        "schema",
        "binding",
        "sequence",
        "predecessor",
        "actor",
        "time",
        "recipe",
    ] {
        let mut changed = event.clone();
        match fault {
            "schema" => changed.schema_version += 1,
            "binding" => changed.binding = ObjectId::mint(),
            "sequence" => changed.sequence += 1,
            "predecessor" => changed.predecessor = Some(ObjectId::mint()),
            "actor" => changed.actor.session_id = Some(SessionId("other-session".into())),
            "time" => changed.recorded_at += chrono::Duration::seconds(1),
            "recipe" => {
                let crate::task_sync::ledger::Transition::PlanningSuccessorPrepared {
                    recipe, ..
                } = &mut changed.transition
                else {
                    unreachable!()
                };
                recipe.outcome = ObjectId::mint();
            }
            _ => unreachable!(),
        }
        assert!(
            recipe
                .validate_historical_prepared(&changed, &f.binding)
                .is_err(),
            "{fault}"
        );
    }
}

#[test]
fn successor_retention_reuses_exact_ids_and_refuses_partial_or_changed_construction() {
    let home = crate::test_support::temp_home().unwrap();
    let mut f = fixture();
    f.binding.repository.git_directory = home.path().join(".git");
    std::fs::create_dir_all(&f.binding.repository.git_directory).unwrap();
    let (_, recipe) = selected(&mut f);
    let retained = RetainedSuccessor {
        binding: f.binding_id.clone(),
        recipe,
        payload: f.payload.clone(),
    };
    assert_eq!(
        successor::read(&f.binding_id, &f.binding, &retained.recipe.request).unwrap(),
        None
    );
    successor::retain(&f.binding, &retained).unwrap();
    let file = f
        .binding
        .repository
        .git_directory
        .join("engram-task-sync-planning-successors")
        .join(format!("{}.json", f.intent.operation));
    let bytes = std::fs::read(&file).unwrap();
    assert_eq!(
        successor::read(&f.binding_id, &f.binding, &retained.recipe.request).unwrap(),
        Some(retained.clone())
    );
    successor::retain(&f.binding, &retained).unwrap();
    assert_eq!(
        crate::task_sync::retention::inventory(&f.binding.repository.git_directory)
            .unwrap()
            .planning_successors[&f.intent.operation],
        retained
    );
    let mut changed = retained.clone();
    changed.recipe.outcome = ObjectId::mint();
    assert!(successor::retain(&f.binding, &changed).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    std::fs::write(&file, b"partial successor").unwrap();
    assert!(successor::read(&f.binding_id, &f.binding, &retained.recipe.request).is_err());
    assert!(successor::retain(&f.binding, &retained).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"partial successor");
}
