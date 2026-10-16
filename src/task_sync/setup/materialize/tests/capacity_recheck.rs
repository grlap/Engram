use super::super::super::files::{PhysicalId, file_identity};
use super::*;

const SIBLING: &str = "unrelated-private";
const SIBLING_BYTES: &[u8] = b"retain this unrelated sibling\n";

#[test]
fn task_materialize_rechecks_existing_shared_capacity_after_sibling_growth() {
    assert_refusal(
        true,
        3,
        "setup-checked",
        "materialization directory has no room for declared publication within entry bound",
    );
    assert_permitted(true, 4, "setup-checked");
}

#[test]
fn task_materialize_rechecks_owned_shared_capacity_after_sibling_growth() {
    assert_refusal(
        false,
        2,
        "engram-renamed",
        "materialization directory entry bound exceeded",
    );
    assert_permitted(false, 3, "engram-renamed");
}

fn assert_refusal(existing: bool, shared: usize, boundary: &str, reason: &str) {
    let fixture = Fixture::new(existing, 1);
    let lock = fixture.admin.join("engram-task-sync-setup.lock");
    let lock_bytes = fs::read(&lock).unwrap();
    let store = fs::read(&fixture.store).unwrap();
    let mut checkpoints = Vec::new();
    let mut injected = 0;
    let mut before = None;
    let result = super::super::run_with_capacity(
        &fixture.marker,
        &ProjectId("materialize/fixture".into()),
        &fixture.selection,
        true,
        attribution(),
        DirectoryCapacity {
            root: 100_000,
            shared,
        },
        &mut |point| {
            checkpoints.push(point.to_owned());
            if point == boundary {
                injected += 1;
                insert_sibling(&fixture);
                let public = Directory::open(&fixture.root.join(".engram")).unwrap();
                let expected: &[&str] = if existing {
                    &["private.db", SIBLING]
                } else {
                    &["project.json", "tasks", SIBLING]
                };
                let mut expected: Vec<_> = expected
                    .iter()
                    .map(|name| std::ffi::OsString::from(*name))
                    .collect();
                expected.sort();
                assert_eq!(public.names(100_000).unwrap(), expected);
                // Earlier setup edits and, for the fresh branch, the public
                // rename already happened. Preserve state from this boundary.
                before = Some((
                    snapshot_with_lock_metadata(fixture.owned.path(), Some(&lock)),
                    public_identities(&fixture),
                ));
            }
            Ok(())
        },
    );
    assert_eq!(injected, 1);
    assert_eq!(result.unwrap_err().to_string(), reason);
    assert_eq!(checkpoints.last().map(String::as_str), Some(boundary));
    if !existing {
        assert_eq!(
            &checkpoints[checkpoints.len() - 3..],
            &["setup-checked", "before-engram-rename", "engram-renamed"]
        );
    }
    assert_eq!(
        (
            snapshot_with_lock_metadata(fixture.owned.path(), Some(&lock)),
            public_identities(&fixture),
        ),
        before.unwrap()
    );
    assert_eq!(fs::read(&lock).unwrap(), lock_bytes);
    assert_eq!(fs::read(&fixture.store).unwrap(), store);
    assert!(!fixture.private().join("complete").exists());
    for name in ["project.json", "tasks"] {
        assert_eq!(fixture.root.join(".engram").join(name).exists(), !existing);
    }
}

fn assert_permitted(existing: bool, shared: usize, boundary: &str) {
    let fixture = Fixture::new(existing, 1);
    let lock = fixture.admin.join("engram-task-sync-setup.lock");
    let lock_bytes = fs::read(&lock).unwrap();
    let store = fs::read(&fixture.store).unwrap();
    let mut injected = 0;
    let mut sibling_id = None;
    let mut checkpoints = Vec::new();
    let capacity = DirectoryCapacity {
        root: 100_000,
        shared,
    };
    let first = super::super::run_with_capacity(
        &fixture.marker,
        &ProjectId("materialize/fixture".into()),
        &fixture.selection,
        true,
        attribution(),
        capacity,
        &mut |point| {
            checkpoints.push(point.to_owned());
            if point == boundary {
                injected += 1;
                insert_sibling(&fixture);
                sibling_id = Some(
                    file_identity(
                        &fs::File::open(fixture.root.join(".engram").join(SIBLING)).unwrap(),
                    )
                    .unwrap(),
                );
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(injected, 1);
    assert_eq!(
        checkpoints.last().map(String::as_str),
        Some("complete-published")
    );
    assert!(!first.repeated);
    fixture.assert_output();
    assert_eq!(fs::read(&lock).unwrap(), lock_bytes);
    assert_eq!(fs::read(&fixture.store).unwrap(), store);
    let sibling = fixture.root.join(".engram").join(SIBLING);
    assert_eq!(fs::read(&sibling).unwrap(), SIBLING_BYTES);
    assert_eq!(
        file_identity(&fs::File::open(&sibling).unwrap()).unwrap(),
        sibling_id.unwrap()
    );
    let before = (snapshot(fixture.owned.path()), public_identities(&fixture));
    let mut repeated_checkpoints = Vec::new();
    let repeated = super::super::run_with_capacity(
        &fixture.marker,
        &ProjectId("materialize/fixture".into()),
        &fixture.selection,
        true,
        attribution(),
        capacity,
        &mut |point| {
            repeated_checkpoints.push(point.to_owned());
            Ok(())
        },
    )
    .unwrap();
    assert!(repeated.repeated);
    assert_eq!(repeated.operation, first.operation);
    assert_eq!(
        repeated_checkpoints.last().map(String::as_str),
        Some("public-checked")
    );
    assert_eq!(
        (snapshot(fixture.owned.path()), public_identities(&fixture)),
        before
    );
}

fn insert_sibling(fixture: &Fixture) {
    let path = fixture.root.join(".engram").join(SIBLING);
    assert!(!path.exists());
    fs::write(path, SIBLING_BYTES).unwrap();
}

fn public_identities(fixture: &Fixture) -> BTreeMap<&'static str, PhysicalId> {
    [
        ".engram",
        ".engram/private.db",
        ".engram/unrelated-private",
        ".engram/tasks",
        ".engram/project.json",
        ".gitignore",
        ".gitattributes",
    ]
    .into_iter()
    .filter_map(|relative| {
        let path = fixture.root.join(relative);
        if !path.exists() {
            return None;
        }
        let id = if path.is_dir() {
            Directory::open(&path).unwrap().identity().unwrap()
        } else {
            file_identity(&fs::File::open(path).unwrap()).unwrap()
        };
        Some((relative, id))
    })
    .collect()
}
