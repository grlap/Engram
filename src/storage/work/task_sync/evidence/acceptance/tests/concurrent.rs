use super::*;
use std::sync::mpsc;
use std::time::Duration;

thread_local! {
    static RELEASE_WRITER: std::cell::RefCell<Option<mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

#[test]
fn concurrent_acceptance_qualification_replays_after_the_first_writer_commits() {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    let request = prepare(&mut fixture, root.path(), false);
    let database_home = crate::test_support::temp_home().unwrap();
    let database = database_home.path().join("qualification.sqlite3");
    fixture
        .store
        .connection
        .execute("VACUUM INTO ?1", [database.to_str().unwrap()])
        .unwrap();
    let mut first = SqliteStore::open(&database).unwrap();
    let mut second = SqliteStore::open(&database).unwrap();
    let actor = fixture.local_actor();
    let (pending, inserted) = mpsc::channel();
    let (release, continue_commit) = mpsc::channel();
    first
        .connection
        .commit_hook(Some(move || {
            pending.send(()).unwrap();
            continue_commit
                .recv_timeout(Duration::from_secs(30))
                .is_err()
        }))
        .unwrap();
    let original_request = request.clone();
    let original_actor = actor.clone();
    let first = std::thread::spawn(move || {
        let id = first
            .associate_imported_acceptance(
                &project(),
                &session(),
                &original_actor,
                &original_request,
                at(22),
            )
            .unwrap();
        let bytes = object_on(&first.connection, &id).unwrap().unwrap();
        (id, bytes)
    });
    inserted.recv_timeout(Duration::from_secs(30)).unwrap();
    let retry_request = request.clone();
    let retry_actor = actor.clone();
    let second = std::thread::spawn(move || {
        RELEASE_WRITER.with(|slot| *slot.borrow_mut() = Some(release));
        second
            .connection
            .busy_handler(Some(|attempt| {
                // The fast lookup has missed the uncommitted first decision. Only
                // reaching the writer lock permits that first decision to commit.
                RELEASE_WRITER.with(|slot| {
                    if let Some(release) = slot.borrow_mut().take() {
                        release.send(()).unwrap();
                    }
                });
                std::thread::sleep(Duration::from_millis(1));
                attempt < 30_000
            }))
            .unwrap();
        second.associate_imported_acceptance(
            &project(),
            &session(),
            &retry_actor,
            &retry_request,
            at(23),
        )
    });
    let (original, bytes) = first.join().unwrap();
    assert_eq!(second.join().unwrap().unwrap(), original);
    let mut store = SqliteStore::open(&database).unwrap();
    assert_eq!(
        object_on(&store.connection, &original).unwrap().unwrap(),
        bytes
    );
    let count: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM work_task_sync_acceptance_uses WHERE decision_id=?1",
            [original.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert!(
        store
            .imported_acceptance_summary(&original, at(24))
            .unwrap()
            .currently_eligible
    );
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    for fault in ["actor", "project", "session", "request", "collision"] {
        let mut changed_actor = actor.clone();
        let mut changed_request = request.clone();
        let mut changed_project = project();
        let mut changed_session = session();
        match fault {
            "actor" => changed_actor.actor_id.push_str("-other"),
            "project" => changed_project.0.push_str("-other"),
            "session" => changed_session.0.push_str("-other"),
            "request" => changed_request.origin_cuts.clear(),
            "collision" => changed_request.decision = request.origin.clone(),
            _ => unreachable!(),
        }
        assert!(
            store
                .associate_imported_acceptance(
                    &changed_project,
                    &changed_session,
                    &changed_actor,
                    &changed_request,
                    at(25)
                )
                .is_err(),
            "{fault}"
        );
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    healthy(&store);
}
