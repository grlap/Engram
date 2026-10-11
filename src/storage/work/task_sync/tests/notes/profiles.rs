use super::*;

fn profile_documents(
    tree: &TaskTree,
    profile: Option<bool>,
    orphan_notes: &[SharedNote],
) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut project = serde_json::to_value(&tree.project).unwrap();
    let fields = project.as_object_mut().unwrap();
    match profile {
        Some(included) => {
            fields.insert("notes_included".into(), included.into());
        }
        None => {
            fields.remove("notes_included");
        }
    }
    if !orphan_notes.is_empty() {
        // With no owning task, notes have no place in the shared format.
        fields.insert("notes".into(), serde_json::to_value(orphan_notes).unwrap());
    }
    let mut documents = std::collections::BTreeMap::new();
    documents.insert("project.json".into(), serde_json::to_vec(&project).unwrap());
    for task in &tree.tasks {
        documents.insert(
            format!("tasks/{}.json", task.id.0),
            serde_json::to_vec(task).unwrap(),
        );
    }
    documents
}

#[test]
fn task_sync_notes_profile_matrix_admits_only_retained_repeatable_profiles() {
    for profile in [None, Some(false), Some(true)] {
        for task_count in [0, 1, 3] {
            for note_count in [0, 1, 3] {
                let mut tree = candidate();
                let mut task = tree.tasks[0].clone();
                task.external_ref = None;
                task.parent_id = None;
                task.prerequisites.clear();
                task.blockers.clear();
                task.lifecycle = WorkLifecycle::Open;
                task.completion = None;
                task.superseded_by = None;
                task.disposal_reason = None;
                tree.tasks = (0..task_count)
                    .map(|_| {
                        let mut task = task.clone();
                        task.id = WorkId(uuid::Uuid::new_v4());
                        task
                    })
                    .collect();
                tree.tasks.sort_by_key(|task| task.id.0);
                let mut notes = (0..note_count)
                    .map(|_| shared_note("Shared progress"))
                    .collect::<Vec<_>>();
                notes.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
                let orphan_notes = if let Some(task) = tree.tasks.first_mut() {
                    task.notes = notes;
                    Vec::new()
                } else {
                    notes
                };
                tree.project.notes_included = profile.unwrap_or(false);
                let documents = profile_documents(&tree, profile, &orphan_notes);
                let expected = if task_count == 0 && note_count > 0 {
                    Some("unknown field `notes`")
                } else if task_count == 0 && profile == Some(true) {
                    Some("notes_included requires at least one task")
                } else if note_count > 0 && profile != Some(true) {
                    Some("shared notes require explicit notes_included")
                } else {
                    None
                };
                let mut store = SqliteStore::open_in_memory().unwrap();
                let before = state(&store);
                for dry in [true, false] {
                    for _ in 0..2 {
                        let result = TaskTree::from_documents(&documents).and_then(|decoded| {
                            store.import_task_tree(
                                &tree.project.project_id,
                                &actor("matrix"),
                                &decoded,
                                dry,
                                at(30),
                            )
                        });
                        if let Some(reason) = expected {
                            assert!(result.unwrap_err().to_string().contains(reason));
                            assert_eq!(state(&store), before);
                        } else {
                            let result = result.unwrap();
                            assert_eq!(result.dry_run, dry);
                            if dry || task_count == 0 {
                                assert_eq!(state(&store), before);
                            }
                        }
                    }
                }
                if expected.is_none() {
                    let retained = state(&store);
                    for dry in [true, false] {
                        let decoded = TaskTree::from_documents(&documents).unwrap();
                        let repeated = store
                            .import_task_tree(
                                &tree.project.project_id,
                                &actor("repeat"),
                                &decoded,
                                dry,
                                at(40),
                            )
                            .unwrap();
                        assert!(!repeated.changed);
                        assert_eq!(state(&store), retained);
                        if profile != Some(true) {
                            // Equivalent spelling row: absent and false are one profile.
                            let equivalent = if profile.is_none() { Some(false) } else { None };
                            let equivalent = TaskTree::from_documents(&profile_documents(
                                &decoded,
                                equivalent,
                                &[],
                            ))
                            .unwrap();
                            let result = store
                                .import_task_tree(
                                    &tree.project.project_id,
                                    &actor("equivalent"),
                                    &equivalent,
                                    dry,
                                    at(40),
                                )
                                .unwrap();
                            assert!(!result.changed);
                            assert_eq!(state(&store), retained);
                        }
                        let mut switched = decoded;
                        switched.project.notes_included = !switched.project.notes_included;
                        let error = store
                            .import_task_tree(
                                &tree.project.project_id,
                                &actor("switch"),
                                &switched,
                                dry,
                                at(40),
                            )
                            .unwrap_err()
                            .to_string();
                        assert!(
                            error.contains("notes_included requires at least one task")
                                || error.contains("shared notes require explicit notes_included")
                                || error.contains("divergent or removed planning state")
                        );
                        assert_eq!(state(&store), retained);
                    }
                }
                assert!(store.verify_all().unwrap().is_healthy());
            }
        }
    }
}
