use super::*;

fn add_alias(fixture: &Fixture, name: &str) {
    let path = fixture.root.join(".engram").join(name);
    if name.eq_ignore_ascii_case("tasks") {
        fs::create_dir(&path).unwrap();
        fs::write(path.join("unowned.txt"), b"preserve unowned task bytes").unwrap();
    } else {
        fs::write(path, b"preserve unowned project bytes").unwrap();
    }
}

#[test]
fn task_materialize_refuses_shared_case_aliases_before_admission_and_recovery() {
    for admitted in [false, true] {
        for name in ["Project.json", "PROJECT.JSON", "Tasks", "TASKS"] {
            let fixture = Fixture::new(true, 1);
            if admitted {
                fixture.interrupt("journal-published");
            }
            add_alias(&fixture, name);
            let before = snapshot(fixture.owned.path());
            let error = fixture
                .service
                .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
                .unwrap_err();
            assert!(error.to_string().contains("case alias"), "{error:#}");
            assert_eq!(snapshot(fixture.owned.path()), before);
            assert_eq!(fixture.private().exists(), admitted);
        }
    }
}

#[test]
fn task_materialize_rechecks_shared_case_aliases_before_publication() {
    for name in ["Project.json", "Tasks"] {
        let fixture = Fixture::new(true, 1);
        let lock = fixture.admin.join("engram-task-sync-setup.lock");
        let lock_bytes = fs::read(&lock).unwrap();
        let mut before = None;
        let result = super::super::run(
            &fixture.marker,
            &ProjectId("materialize/fixture".into()),
            &fixture.selection,
            true,
            attribution(),
            &mut |point| {
                if point == "setup-checked" {
                    add_alias(&fixture, name);
                    before = Some(snapshot_with_lock_metadata(
                        fixture.owned.path(),
                        Some(&lock),
                    ));
                }
                Ok(())
            },
        );
        let error = result.unwrap_err();
        assert!(error.to_string().contains("case alias"), "{error:#}");
        assert_eq!(
            snapshot_with_lock_metadata(fixture.owned.path(), Some(&lock)),
            before.unwrap()
        );
        assert_eq!(fs::read(lock).unwrap(), lock_bytes);
    }
}

#[test]
fn task_materialize_repeat_refuses_case_only_renames_with_identical_objects() {
    for existing in [false, true] {
        for (canonical, alias) in [("project.json", "Project.json"), ("tasks", "Tasks")] {
            let fixture = Fixture::new(existing, 1);
            fixture.run();
            let public = fixture.root.join(".engram");
            fs::write(public.join("unrelated-private"), b"preserve sibling").unwrap();
            let original = public.join(canonical);
            let identity = physical_identity(&original);
            let intermediate = fixture.owned.path().join("case-rename-intermediate");
            fs::rename(&original, &intermediate).unwrap();
            fs::rename(&intermediate, public.join(alias)).unwrap();
            assert_eq!(physical_identity(&public.join(alias)), identity);
            let before = snapshot(fixture.owned.path());
            let error = fixture
                .service
                .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
                .unwrap_err();
            assert!(error.to_string().contains("case alias"), "{error:#}");
            assert_eq!(snapshot(fixture.owned.path()), before);
        }
    }
}

fn physical_identity(path: &Path) -> super::super::super::files::PhysicalId {
    if path.is_dir() {
        Directory::open(path).unwrap().identity().unwrap()
    } else {
        super::super::super::files::file_identity(&fs::File::open(path).unwrap()).unwrap()
    }
}
