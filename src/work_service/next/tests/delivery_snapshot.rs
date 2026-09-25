//! `next` under another process that uses the same session. The staged page
//! `next` replays is read with its session row, and checked against the
//! feed, from one commit: when the other process confirms or re-stages that
//! page between the row and payload reads, the read returns the page as it
//! was staged, not a broken projection. And the page the previous call
//! returned is confirmed without depending on what was read as pending, so
//! the other process confirming past it cannot make the confirmation fail.

use super::*;
use crate::storage::concurrent_commit::{
    act_across_a_concurrent_commit, read_across_a_concurrent_commit,
};
use crate::test_support::TempHome;

/// The statement that loads a staged page's payload after the session row.
const STAGED_PAYLOAD: &[&str] = &["SELECT tentative_delivery_payload"];

const PROJECT: &str = "staged-snapshot";
const SESSION: &str = "shared";

/// A one-change page staged for the shared session, with at least two more
/// changes after it on the feed. The directory is the last field, so it is dropped
/// after the service closes its connection.
struct Staged {
    database: std::path::PathBuf,
    service: LocalWorkService,
    through: i64,
    token: Option<String>,
    _directory: TempHome,
}

fn shared_session(database: &std::path::Path) -> LocalWorkService {
    LocalWorkService::new(
        database.to_path_buf(),
        ProjectId(PROJECT.into()),
        "agent".into(),
        SessionId(SESSION.into()),
        None,
    )
}

fn changes_only() -> WorkNextQuery {
    WorkNextQuery {
        sections: vec![WorkNextSection::Changes],
        ..WorkNextQuery::default()
    }
}

fn staged() -> Staged {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("engram.sqlite3");
    let service = shared_session(&database);
    service
        .work_propose(root_input("First", "first"), at(0))
        .expect("first root");
    service
        .work_propose(root_input("Second", "second"), at(1))
        .expect("second root");
    service
        .work_propose(root_input("Third", "third"), at(1))
        .expect("third root");
    let view = service
        .work_next(1, changes_only(), at(2))
        .expect("stage a page");
    let through = view.delivered_through.expect("a staged page");
    Staged {
        database,
        service,
        through,
        token: view.delivery_token,
        _directory: directory,
    }
}

/// The feed positions a staged page carries.
fn positions(page: &StagedWorkChangePage) -> Vec<i64> {
    page.changes
        .iter()
        .map(|change| change.entry.position.position)
        .collect()
}

#[test]
fn a_page_confirmed_partway_through_the_read_is_returned_as_staged() {
    let staged = staged();
    let reader = SqliteStore::open(&staged.database).expect("reader store");
    let before = staged
        .service
        .delivery_state(&reader, at(3))
        .expect("before");
    let mut writer = SqliteStore::open(&staged.database).expect("writer store");
    let (through, token) = (staged.through, staged.token.clone());
    let (session, page) = read_across_a_concurrent_commit(
        &reader,
        |reader| staged.service.delivery_state(reader, at(3)),
        STAGED_PAYLOAD,
        move || {
            writer
                .acknowledge_work_session_delivery(
                    &ProjectId(PROJECT.into()),
                    &SessionId(SESSION.into()),
                    through,
                    token.as_deref(),
                    at(3),
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    )
    .expect("the page as staged, not a broken projection");
    assert_eq!(session, before.0);
    let (page_through, page) = page.expect("the staged page");
    assert_eq!(page_through, staged.through);
    assert_eq!(
        positions(&page),
        positions(&before.1.expect("staged before").1)
    );
    let (after, pending) = staged
        .service
        .delivery_state(&reader, at(4))
        .expect("after");
    assert_eq!(after.project_cursor, staged.through);
    assert!(pending.is_none(), "the other process confirmed the page");
}

#[test]
fn a_page_restaged_partway_through_the_read_is_returned_as_staged() {
    let staged = staged();
    let reader = SqliteStore::open(&staged.database).expect("reader store");
    let before = staged
        .service
        .delivery_state(&reader, at(3))
        .expect("before");
    let other = shared_session(&staged.database);
    let (session, page) = read_across_a_concurrent_commit(
        &reader,
        |reader| staged.service.delivery_state(reader, at(3)),
        STAGED_PAYLOAD,
        move || {
            other
                .work_next(1, changes_only(), at(3))
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    )
    .expect("the page as staged, not a broken projection");
    assert_eq!(session, before.0);
    let (page_through, page) = page.expect("the staged page");
    assert_eq!(page_through, staged.through);
    assert_eq!(
        positions(&page),
        positions(&before.1.expect("staged before").1)
    );
    let (after, pending) = staged
        .service
        .delivery_state(&reader, at(4))
        .expect("after");
    assert_eq!(after.project_cursor, staged.through);
    let (restaged_through, _) = pending.expect("the other process staged the next page");
    assert!(restaged_through > staged.through);
}

/// Before its changes, `next` confirms the page its session's previous call
/// returned. The other process confirms that page, stages the next, and
/// confirms that too, all between this call's read of the pending page and
/// its confirmation. The page is delivered already, so the confirmation
/// succeeds and leaves the other process's newest page pending no longer.
#[test]
fn the_previous_page_confirmed_past_by_another_process_counts_as_delivered() {
    let staged = staged();
    let mut store = SqliteStore::open(&staged.database).expect("store");
    let project = ProjectId(PROJECT.into());
    let session = SessionId(SESSION.into());
    assert_eq!(
        store
            .work_session_state(&project, &session, at(4))
            .expect("before")
            .tentative_project_cursor,
        Some(staged.through),
        "the previous call's page is pending"
    );
    let other = shared_session(&staged.database);
    let last_staged = std::rc::Rc::new(std::cell::Cell::new(None));
    let seen = last_staged.clone();
    act_across_a_concurrent_commit(
        &mut store,
        |store| staged.service.confirm_previous_page(store, at(5)),
        &["BEGIN IMMEDIATE"],
        move || {
            for _ in 0..2 {
                let view = other
                    .work_next(1, changes_only(), at(4))
                    .map_err(|error| error.to_string())?;
                seen.set(view.delivered_through);
            }
            Ok(())
        },
    )
    .expect("the page is delivered already, not a mismatch");
    let last = last_staged.get().expect("the other process staged a page");
    assert!(last > staged.through);
    let session = store
        .work_session_state(&project, &session, at(6))
        .expect("after");
    assert_eq!(session.project_cursor, last);
    assert_eq!(session.tentative_project_cursor, None);
}
