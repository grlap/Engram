use super::*;
use rusqlite::StatementStatus;

fn note_query_cost(f: &Fixture, work: WorkId) -> (Vec<String>, i32, i32) {
    let plan = f
        .store
        .connection
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            super::super::super::notes::NOTE_OBJECTS
        ))
        .unwrap()
        .query_map([work.0.to_string()], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    println!("item note lookup plan: {plan:?}");
    let mut statement = f
        .store
        .connection
        .prepare(super::super::super::notes::NOTE_OBJECTS)
        .unwrap();
    let mut ids = statement
        .query_map([work.0.to_string()], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    ids.sort();
    (
        ids,
        statement.get_status(StatementStatus::VmStep),
        statement.get_status(StatementStatus::FullscanStep),
    )
}

#[test]
fn remote_refused_history_lookup_ignores_unrelated_root_growth() {
    let mut f = fixture();
    let work = f.request.association.binding.work_id;
    refuse_destination_for_cost(&mut f);
    let receipt = f.record().unwrap();
    let before = note_query_cost(&f, work);
    assert!(before.0.contains(&receipt.operation.as_str().to_owned()));
    let mut draft = super::super::super::test_support::child(
        "unrelated",
        ChildRequirement::Optional,
        "Other item in this root",
    );
    draft.notes = (0..16)
        .map(|i| format!("Other item observation {i}"))
        .collect();
    let parent = f.store.get_work_item(work).unwrap();
    let child = f
        .store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: work,
                expected_parent_revision: parent.revision,
                children: vec![draft],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("peer"),
                idempotency_key: "unrelated-child".into(),
                created_at: at(11),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
        .children
        .remove(0);
    f.request.association.binding.work_id = child.work_id;
    f.request.association.binding.work_revision = child.revision;
    // A never-run item still retains refused deliveries without a run feed.
    f.request.association.binding.run_id = WorkRunId::new();
    let mut remote_ids = Vec::new();
    for i in 0..128 {
        f.request.operation_key = format!("remote:other-item:{i}");
        f.request.completion.run_id = format!("other-terminal-{i}");
        let other = f.record_at(12).unwrap();
        assert!(!other.qualification.credited);
        remote_ids.push(other.operation);
    }
    let after = note_query_cost(&f, work);
    println!(
        "remote item lookup VM steps: before={} after={} fullscan_steps={}",
        before.1, after.1, after.2
    );
    assert_eq!(after.0, before.0);
    assert_eq!(after.2, 0, "no full scan: {after:?}");
    assert!(
        after.1 <= before.1 + 64,
        "unrelated history must not grow item lookup: {before:?} -> {after:?}"
    );
    let child_rows = f
        .store
        .work_record_index(
            &f.project,
            child.work_id,
            crate::storage::WorkRecordKind::NotesWithGates,
        )
        .unwrap();
    assert_eq!(
        child_rows
            .iter()
            .filter(|r| remote_ids.contains(&r.address.hash))
            .count(),
        128
    );
    f.assert_healthy();
}

fn refuse_destination_for_cost(f: &mut Fixture) {
    f.request.completion.schema_version = REMOTE_COMPLETION_SCHEMA_VERSION + 1;
}
