use super::*;

#[test]
fn historical_delta_structure_preserves_original_admission_without_accepting_wrong_links() {
    let f = fixture();
    f.payload
        .validate(&f.binding_id, &f.binding, &f.intent)
        .unwrap();
    let mut historical = f.payload.clone();
    historical
        .documents
        .values_mut()
        .flatten()
        .next()
        .unwrap()
        .title
        .clear();
    historical
        .validate_historical(&f.binding_id, &f.binding, &f.intent)
        .unwrap();
    assert!(
        historical
            .validate(&f.binding_id, &f.binding, &f.intent)
            .is_err()
    );
    for fault in ["project", "operation", "output", "keys", "task"] {
        let mut changed = f.payload.clone();
        match fault {
            "project" => changed.project_id = ProjectId("another-project".into()),
            "operation" => changed.operation = uuid::Uuid::new_v4(),
            "output" => {
                changed.output.profile.include_notes = !changed.output.profile.include_notes;
            }
            "keys" => changed.documents.clear(),
            "task" => changed.documents.values_mut().flatten().next().unwrap().id = WorkId::new(),
            _ => unreachable!(),
        }
        assert!(
            changed
                .validate_historical(&f.binding_id, &f.binding, &f.intent)
                .is_err(),
            "{fault}"
        );
        assert!(
            changed
                .validate(&f.binding_id, &f.binding, &f.intent)
                .is_err(),
            "{fault}"
        );
    }
}
