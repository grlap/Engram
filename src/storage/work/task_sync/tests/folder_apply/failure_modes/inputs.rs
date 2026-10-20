use super::*;
use crate::task_sync::directory;
use std::{fs, path::Path};

fn folder_bytes(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            for task in fs::read_dir(entry.path()).unwrap() {
                let task = task.unwrap();
                files.insert(
                    format!("tasks/{}", task.file_name().to_str().unwrap()),
                    fs::read(task.path()).unwrap(),
                );
            }
        } else {
            files.insert(
                entry.file_name().to_str().unwrap().into(),
                fs::read(entry.path()).unwrap(),
            );
        }
    }
    files
}

#[test]
fn folder_resolved_input_refusals_preserve_candidate_and_destination() {
    let home = crate::test_support::temp_home().unwrap();
    let (mut store, fixture) = enrolled();
    for (case, reason) in [
        ("truncated", "EOF while parsing an object"),
        ("duplicate", "duplicate JSON member"),
        ("nested-unknown", "unknown field `unknown_policy`"),
        ("conflict", "expected value"),
        ("wrong-name", "task filename/id mismatch"),
        ("dangling", "dangling parent"),
        ("incomplete", "task folder exchange must be complete"),
        ("missing-project", "project.json missing"),
    ] {
        let path = home.path().join(case);
        directory::write_folder(&path, &fixture.tree, &profile()).unwrap();
        let task_path = path.join(format!("tasks/{}.json", fixture.first.0));
        let original = fs::read_to_string(&task_path).unwrap();
        match case {
            "truncated" => fs::write(&task_path, b"{").unwrap(),
            "duplicate" => fs::write(
                &task_path,
                format!("{{\"title\":\"duplicate\",{}", &original[1..]),
            )
            .unwrap(),
            "nested-unknown" => {
                let mut value: serde_json::Value = serde_json::from_str(&original).unwrap();
                value["acceptance_bindings"] =
                    serde_json::to_value(vec![crate::domain::AcceptanceBinding {
                        criterion: 1,
                        requirement: crate::domain::VerificationRequirement {
                            check_kind: crate::domain::VerificationKind::Test,
                            check_fingerprint: None,
                        },
                    }])
                    .unwrap();
                value["acceptance_bindings"][0]["requirement"]["unknown_policy"] = true.into();
                fs::write(&task_path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "conflict" => fs::write(&task_path, b"<<<<<<< unresolved Git conflict").unwrap(),
            "wrong-name" => fs::rename(
                &task_path,
                path.join(format!("tasks/{}.json", uuid::Uuid::new_v4())),
            )
            .unwrap(),
            "dangling" => {
                fs::remove_file(path.join(format!("tasks/{}.json", fixture.parent.0))).unwrap();
            }
            "incomplete" => {
                let project_path = path.join("project.json");
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&project_path).unwrap()).unwrap();
                value["folder_exchange"]["complete"] = false.into();
                fs::write(project_path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "missing-project" => fs::remove_file(path.join("project.json")).unwrap(),
            _ => unreachable!(),
        }
        let bytes = folder_bytes(&path);
        let before = state(&store);
        let retained_basis = basis(&store);
        // The CLI uses this same resolved reader before the storage apply.
        let attempt = (|| -> anyhow::Result<TaskSyncImportResult> {
            let resolved = directory::read_resolved_folder(&path)?;
            Ok(apply_basis(
                &mut store,
                &resolved.tree,
                &retained_basis,
                case,
                6,
            )?)
        })();
        let error = attempt.unwrap_err();
        let error = error.downcast_ref::<StoreError>().unwrap();
        assert_eq!(
            refusal_code(error),
            "task_sync_invalid_graph",
            "{case}: {error:#}"
        );
        assert!(format!("{error:#}").contains(reason), "{case}: {error:#}");
        assert_eq!(state(&store), before, "{case}");
        assert_eq!(folder_bytes(&path), bytes, "{case}");
    }
    // A valid merged edit has a stale embedded fingerprint; the caller's
    // retained destination basis, not that fingerprint, controls admission.
    let path = home.path().join("valid-merged");
    directory::write_folder(&path, &fixture.tree, &profile()).unwrap();
    let selected = edited(&fixture.tree, fixture.first, |task| task.priority = 4);
    let task = selected
        .tasks
        .iter()
        .find(|task| task.id == fixture.first)
        .unwrap();
    fs::write(
        path.join(format!("tasks/{}.json", task.id.0)),
        serde_json::to_vec(task).unwrap(),
    )
    .unwrap();
    let error = directory::read_folder(&path).unwrap_err();
    assert!(
        format!("{error:#}").contains("inventory fingerprint is inconsistent"),
        "{error:#}"
    );
    let parsed = directory::read_resolved_folder(&path).unwrap();
    assert_eq!(parsed.tree, selected);
    exchange(&mut store, &parsed.tree, "valid-merge", 6).unwrap();
    assert_eq!(exported(&store), selected);
    assert!(store.verify_all().unwrap().is_healthy());
}
