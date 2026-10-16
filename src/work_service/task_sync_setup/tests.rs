use super::*;
use crate::{
    ProjectId, SessionId,
    task_sync::{
        ProjectDocument, TaskTree,
        notes::NoteOmissions,
        progress::{ExportCapture, ExportCut, ExportProfile},
    },
};
use std::fs;

fn service(database: &Path) -> LocalWorkService {
    LocalWorkService::new(
        database.into(),
        ProjectId("service/opt-in".into()),
        "fixture-operator".into(),
        SessionId("fixture-service".into()),
        None,
    )
}

fn empty_tree() -> TaskTree {
    TaskTree {
        project: ProjectDocument {
            format: task_sync::FORMAT.into(),
            project_id: ProjectId("service/opt-in".into()),
            notes_included: false,
        },
        tasks: Vec::new(),
    }
}

fn entries(path: &Path) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
    let mut result = Vec::new();
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.push((path.clone(), None));
            result.extend(entries(&path));
        } else {
            let bytes = fs::read(&path).unwrap();
            result.push((path, Some(bytes)));
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

#[test]
fn setup_actor_context_service_clone_refuses_raw_controls_before_repository_state() {
    let owned = crate::test_support::temp_home().unwrap();
    let project = ProjectId("service/opt-in".into());
    let database = crate::project_database_path(&owned.path().join("home"), &project);
    let service = LocalWorkService::new_with_attribution(
        database.clone(),
        project,
        "fixture-operator".into(),
        SessionId("fixture-service".into()),
        None,
        Some("model\tsecret-context".into()),
        crate::WorkAttributionDefaults::default(),
    );
    service
        .task_sync_settings(
            &task_sync::settings::Request {
                action: task_sync::settings::Action::Enable,
                user_instruction: Some("Enable Git task sync for this test fixture.".into()),
            },
            chrono::Utc::now(),
        )
        .unwrap();
    let before = entries(owned.path());
    let selection = setup::MaterializeSelection {
        repository: owned.path().join("missing-repository"),
        setup_operation: uuid::Uuid::now_v7(),
        prepared_operation: uuid::Uuid::now_v7(),
        progress_directory: owned.path().join("progress"),
        candidate_directory: owned.path().join("candidate"),
    };
    for service in [&service, &service.clone()] {
        let prepare = service
            .prepare_task_sync_setup(
                &owned.path().join("marker"),
                &selection.repository,
                "ordinary",
                setup::Assertions {
                    visibility: "private".into(),
                    readership: "fixture maintainers".into(),
                    profile: task_sync::FORMAT.into(),
                    include_notes: false,
                    origin_machine: None,
                },
            )
            .err()
            .unwrap();
        let materialize = service
            .materialize_task_sync_setup(&owned.path().join("marker"), &selection)
            .unwrap_err();
        for error in [prepare, materialize] {
            assert!(
                error.to_string().contains(
                    "actor context must be nonblank bounded text without control characters"
                ),
                "{error}"
            );
        }
        assert_eq!(entries(owned.path()), before);
        assert!(!database.exists());
    }
}

#[test]
fn task_sync_all_service_exchange_routes_refuse_without_opt_in_before_opening_sqlite() {
    for state in ["absent", "disabled", "malformed", "oversized"] {
        let owned = crate::test_support::temp_home().unwrap();
        let database = crate::project_database_path(
            &owned.path().join("home"),
            &ProjectId("service/opt-in".into()),
        );
        let service = service(&database);
        if state != "absent" {
            fs::create_dir_all(database.parent().unwrap()).unwrap();
            let path = database.parent().unwrap().join("task-sync-setting.json");
            match state {
                "disabled" => {
                    service
                        .task_sync_settings(
                            &task_sync::settings::Request {
                                action: task_sync::settings::Action::Disable,
                                user_instruction: None,
                            },
                            chrono::Utc::now(),
                        )
                        .unwrap();
                }
                "malformed" => fs::write(path, b"not JSON").unwrap(),
                "oversized" => fs::write(path, vec![b' '; 128 * 1024]).unwrap(),
                _ => unreachable!(),
            }
        }
        let before = entries(owned.path());
        let profile = ExportProfile {
            include_notes: false,
            origin_machine: None,
        };
        assert!(service.export_task_tree(chrono::Utc::now()).is_err());
        assert!(
            service
                .export_task_tree_with_notes("fixture-machine", chrono::Utc::now())
                .is_err()
        );
        assert!(
            service
                .export_task_tree_with_options(
                    &task_sync::TaskSyncExportOptions::default(),
                    chrono::Utc::now()
                )
                .is_err()
        );
        for dry in [false, true] {
            assert!(
                service
                    .import_task_tree(&empty_tree(), dry, chrono::Utc::now())
                    .is_err()
            );
        }
        assert!(
            service
                .prepare_task_tree(
                    &owned.path().join("candidate"),
                    &owned.path().join("progress"),
                    &profile,
                    false,
                    chrono::Utc::now()
                )
                .is_err()
        );
        assert!(
            service
                .prepare_task_sync_setup(
                    &owned.path().join("marker"),
                    owned.path(),
                    "ordinary",
                    setup::Assertions {
                        visibility: "private".into(),
                        readership: "fixture maintainers".into(),
                        profile: task_sync::FORMAT.into(),
                        include_notes: false,
                        origin_machine: None,
                    }
                )
                .is_err()
        );
        assert!(!database.exists());
        assert_eq!(entries(owned.path()), before, "{state}");
    }
}

#[test]
fn task_sync_pending_preparation_finishes_after_opt_out_without_capture() {
    let owned = crate::test_support::temp_home().unwrap();
    let project = ProjectId("service/opt-in".into());
    let database = crate::project_database_path(&owned.path().join("home"), &project);
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    fs::write(&database, b"not a SQLite store; do not open").unwrap();
    let service = service(&database);
    let destination = owned.path().join("candidate");
    let progress = owned.path().join("progress");
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let receipt = task_sync::progress::prepare(
        &destination,
        &progress,
        &database,
        &project,
        &profile,
        false,
        |_| {
            Ok(ExportCapture {
                tree: empty_tree(),
                omissions: NoteOmissions::default(),
                activity_omissions: NoteOmissions::default(),
                cut: ExportCut {
                    position: 0,
                    anchor: None,
                },
                full_reason: None,
            })
        },
    )
    .unwrap();
    let generation = fs::read_dir(&progress)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("generation-")
        })
        .unwrap();
    fs::rename(
        generation.join("complete"),
        owned.path().join("original-complete"),
    )
    .unwrap();
    fs::write(
        generation.join(".complete-partial"),
        &receipt.operation.to_string().as_bytes()[..7],
    )
    .unwrap();
    let recovered = service
        .prepare_task_tree(&destination, &progress, &profile, false, chrono::Utc::now())
        .unwrap();
    assert_eq!(recovered.operation, receipt.operation);
    assert!(recovered.recovered);
    assert!(!generation.join(".complete-partial").exists());
    assert_eq!(
        fs::read(&database).unwrap(),
        b"not a SQLite store; do not open"
    );
    let before = entries(owned.path());
    assert!(
        service
            .prepare_task_tree(&destination, &progress, &profile, false, chrono::Utc::now())
            .is_err()
    );
    assert_eq!(entries(owned.path()), before);
}

#[test]
fn task_sync_read_only_mutation_refusal_precedes_opt_in_and_repository_reads() {
    let owned = crate::test_support::temp_home().unwrap();
    let database = crate::project_database_path(
        &owned.path().join("home"),
        &ProjectId("service/opt-in".into()),
    );
    let service = service(&database).into_read_only();
    let before = entries(owned.path());
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let error = service
        .prepare_task_tree(
            &owned.path().join("candidate"),
            &owned.path().join("progress"),
            &profile,
            false,
            chrono::Utc::now(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("read-only"));
    let selection = setup::MaterializeSelection {
        repository: owned.path().join("missing-repository"),
        setup_operation: uuid::Uuid::now_v7(),
        prepared_operation: uuid::Uuid::now_v7(),
        progress_directory: owned.path().join("progress"),
        candidate_directory: owned.path().join("candidate"),
    };
    let error = service
        .materialize_task_sync_setup(&owned.path().join("marker"), &selection)
        .unwrap_err();
    assert!(error.to_string().contains("read-only"));
    let error = service
        .import_task_tree(&empty_tree(), false, chrono::Utc::now())
        .unwrap_err();
    assert!(error.to_string().contains("read-only"));
    assert_eq!(entries(owned.path()), before);
}

#[test]
fn task_sync_pending_preparation_refuses_drift_without_cleaning_or_creating_state() {
    for change in ["unknown", "temporary", "metadata", "destination"] {
        let owned = crate::test_support::temp_home().unwrap();
        let project = ProjectId("service/opt-in".into());
        let database = crate::project_database_path(&owned.path().join("home"), &project);
        fs::create_dir_all(database.parent().unwrap()).unwrap();
        fs::write(&database, b"not SQLite").unwrap();
        let service = service(&database);
        let destination = owned.path().join("candidate");
        let progress = owned.path().join("progress");
        let profile = ExportProfile {
            include_notes: false,
            origin_machine: None,
        };
        let receipt = task_sync::progress::prepare(
            &destination,
            &progress,
            &database,
            &project,
            &profile,
            false,
            |_| {
                Ok(ExportCapture {
                    tree: empty_tree(),
                    omissions: NoteOmissions::default(),
                    activity_omissions: NoteOmissions::default(),
                    cut: ExportCut {
                        position: 0,
                        anchor: None,
                    },
                    full_reason: None,
                })
            },
        )
        .unwrap();
        let generation = fs::read_dir(&progress)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap();
        fs::rename(
            generation.join("complete"),
            owned.path().join("original-complete"),
        )
        .unwrap();
        fs::write(
            generation.join(".complete-partial"),
            &receipt.operation.to_string().as_bytes()[..7],
        )
        .unwrap();
        let selected_destination = if change == "destination" {
            owned.path().join("unadmitted-output")
        } else {
            destination
        };
        match change {
            "unknown" => fs::write(progress.join("unknown"), b"preserve").unwrap(),
            "temporary" => {
                fs::write(generation.join(".complete-partial"), b"another operation").unwrap();
            }
            "metadata" => fs::write(generation.join("state.json"), b"malformed").unwrap(),
            "destination" => (),
            _ => unreachable!(),
        }
        let before = entries(owned.path());
        assert!(
            service
                .prepare_task_tree(
                    &selected_destination,
                    &progress,
                    &profile,
                    false,
                    chrono::Utc::now()
                )
                .is_err(),
            "{change}"
        );
        assert_eq!(entries(owned.path()), before, "{change}");
    }
}
