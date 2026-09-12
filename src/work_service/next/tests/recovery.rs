use super::*;

struct RecoveryFixture {
    reader: LocalWorkService,
    writer: LocalWorkService,
    directory: crate::test_support::TempHome,
}

#[derive(Debug, PartialEq, Eq)]
struct DeliveryState {
    confirmed: i64,
    through: Option<i64>,
    token: Option<String>,
    payload_hash: Option<String>,
    payload: Option<Vec<u8>>,
}

impl RecoveryFixture {
    fn new() -> Self {
        let directory = crate::test_support::temp_home().expect("temp home");
        let database = directory.path().join("engram.sqlite3");
        let service = |session: &str| {
            LocalWorkService::new(
                database.clone(),
                ProjectId("host-recovery".into()),
                session.into(),
                SessionId(session.into()),
                None,
            )
        };
        let reader = service("reader");
        let writer = service("writer");
        writer
            .work_propose(root_input("First page", "first"), at(0))
            .expect("first source event");
        Self {
            reader,
            writer,
            directory,
        }
    }

    fn next(&self, through: Option<i64>, token: Option<&str>) -> Result<WorkNextView, StoreError> {
        self.reader.work_next_with_delivery_token(
            20,
            through,
            token,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            at(4),
        )
    }

    fn state(&self) -> DeliveryState {
        let connection = rusqlite::Connection::open(self.directory.path().join("engram.sqlite3"))
            .expect("inspect delivery");
        connection
            .query_row(
                "SELECT project_cursor, tentative_project_cursor, tentative_delivery_token,
                    tentative_delivery_payload_hash, tentative_delivery_payload
             FROM work_session_state WHERE session_id = 'reader'",
                [],
                |row| {
                    Ok(DeliveryState {
                        confirmed: row.get(0)?,
                        through: row.get(1)?,
                        token: row.get(2)?,
                        payload_hash: row.get(3)?,
                        payload: row.get(4)?,
                    })
                },
            )
            .expect("stored delivery state")
    }

    fn append_later_data(&self) {
        self.writer
            .work_propose(root_input("Later page", "later"), at(3))
            .expect("later source data before replay");
    }

    fn assert_refused(&self, through: i64, token: Option<&str>) {
        let before = self.state();
        assert!(matches!(
            self.next(Some(through), token),
            Err(StoreError::InvalidWork(message))
                if message.starts_with("work delivery acknowledgement does not match the pending page;")
        ));
        assert_eq!(
            self.state(),
            before,
            "refusal preserves pending payload and cursors"
        );
    }
}

fn assert_same_page(actual: &WorkNextView, expected: &WorkNextView) {
    assert_eq!(actual.delivered_through, expected.delivered_through);
    assert_eq!(actual.delivery_token, expected.delivery_token);
    assert_eq!(
        serde_json::to_vec(&actual.changes).expect("changes bytes"),
        serde_json::to_vec(&expected.changes).expect("reference changes bytes")
    );
}

#[test]
fn host_ack_recovery_replays_pending_page_and_preserves_the_following_page() {
    let fixture = RecoveryFixture::new();
    let page = fixture
        .next(None, None)
        .expect("stage P; host loses response");
    let original = fixture.state();
    let through = page.delivered_through.expect("P through");
    let token = page.delivery_token.as_deref().expect("P token");
    assert!(through > original.confirmed);
    assert!(original.payload.is_some());
    fixture.append_later_data();
    assert_eq!(fixture.state(), original);
    fixture.assert_refused(through + 100, Some("wrong-token"));
    fixture.assert_refused(through, Some("wrong-token"));

    // Same service route as core next --sections focus, without ACK fields.
    let focus = fixture
        .reader
        .work_next(
            20,
            WorkNextQuery {
                sections: vec![WorkNextSection::Focus],
                ..WorkNextQuery::default()
            },
            at(4),
        )
        .expect("read confirmed cursor without staging or acknowledging");
    assert_eq!(focus.session.confirmed_project_cursor, original.confirmed);
    assert!(focus.session.pending_delivery);
    assert!(focus.changes.is_none());
    assert!(focus.delivered_through.is_none());
    assert!(focus.delivery_token.is_none());
    assert_eq!(fixture.state(), original);

    let replay = fixture
        .next(Some(focus.session.confirmed_project_cursor), None)
        .expect("ACK confirmed is a no-op; replay retained P");
    assert_same_page(&replay, &page);
    assert_eq!(
        fixture.state(),
        original,
        "including exact stored payload bytes"
    );
    let following = fixture
        .next(Some(through), Some(token))
        .expect("deliver P, then ACK P and stage P+1");
    let following_state = fixture.state();
    let following_through = following.delivered_through.expect("P+1 through");
    assert!(
        following_through > through,
        "new data existed before replay"
    );
    assert_eq!(following_state.confirmed, through);
    assert_eq!(following_state.through, Some(following_through));
    assert!(!following.changes.as_ref().expect("P+1 changes").is_empty());
    let repeated = fixture
        .next(Some(through), Some(token))
        .expect("repeat ACK P");
    assert_same_page(&repeated, &following);
    assert_eq!(
        fixture.state(),
        following_state,
        "ACK P must not confirm P+1"
    );

    fixture
        .reader
        .work_next_with_delivery_token(
            20,
            Some(following_through),
            following.delivery_token.as_deref(),
            WorkNextQuery {
                sections: vec![WorkNextSection::Focus],
                ..WorkNextQuery::default()
            },
            at(5),
        )
        .expect("ACK P+1 without staging another page");
    let finished = fixture.state();
    assert_eq!(finished.confirmed, following_through);
    assert_eq!(finished.through, None);
    assert_eq!(finished.token, None);
    assert_eq!(finished.payload, None);
    assert_eq!(finished.payload_hash, None);
}

#[test]
fn host_ack_recovery_stale_confirmed_cursor_refuses_after_another_advance() {
    let fixture = RecoveryFixture::new();
    let page = fixture.next(None, None).expect("stage P");
    let confirmed = page.session.confirmed_project_cursor;
    fixture.append_later_data();
    // A second advancing call for this session violates recovery serialization.
    let following = fixture.next(None, None).expect("implicit ACK P, stage P+1");
    assert_eq!(
        following.session.confirmed_project_cursor,
        page.delivered_through.unwrap()
    );
    assert!(following.delivered_through > page.delivered_through);
    fixture.assert_refused(confirmed, None);
    let current = fixture.state();
    assert_eq!(current.through, following.delivered_through);
    assert_eq!(current.token, following.delivery_token);
}

#[test]
fn agent_ack_recovery_after_focus_discard_has_plain_retry_guidance() {
    let fixture = RecoveryFixture::new();
    fixture.next(None, None).expect("stage original page");
    let mut store =
        SqliteStore::open(fixture.directory.path().join("engram.sqlite3")).expect("store");
    // Reproduce the storage-call interleaving in implicit ACK deterministically:
    // read previous, change focus, then acknowledge the previously read pair.
    let previous = store
        .work_session_state(
            &fixture.reader.project_id,
            &fixture.reader.session_id,
            at(3),
        )
        .expect("read previous pending page");
    let target = proposed_root(
        fixture
            .writer
            .work_propose(root_input("New focus", "new-focus"), at(3))
            .expect("new focus target"),
    );
    fixture
        .reader
        .work_focus(&target.short_ref, at(4))
        .expect("concurrent focus discards pending page");
    let discarded = fixture.state();
    assert_eq!(discarded.confirmed, previous.project_cursor);
    assert_eq!(discarded.through, None);
    assert_eq!(discarded.token, None);
    assert_eq!(discarded.payload_hash, None);
    assert_eq!(discarded.payload, None);
    let error = store
        .acknowledge_work_session_delivery(
            &fixture.reader.project_id,
            &fixture.reader.session_id,
            previous.tentative_project_cursor.expect("previous through"),
            previous.tentative_delivery_token.as_deref(),
            at(5),
        )
        .expect_err("previous page no longer exists");
    assert!(matches!(&error, StoreError::InvalidWork(_)));
    assert_eq!(fixture.state(), discarded, "refusal does not acknowledge");
    let message = error.to_string();
    assert!(
        message.starts_with(
            "local work input is invalid: work delivery acknowledgement does not match the pending page; for ordinary agent next, retry the next tool or engram work next; explicit-ACK hosts:"
        ),
        "the shared refusal must distinguish the agent retry before host-only commands: {message}"
    );
    let guidance = crate::verbs::VerbError::from(error).guidance();
    assert_eq!(guidance.reminders, vec![message]);
    assert!(
        guidance.next.is_empty(),
        "no host-only next command for the agent"
    );

    let retry = fixture
        .reader
        .work_next_for_agent(
            20,
            20,
            true,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            at(6),
        )
        .expect("ordinary agent next retry stages under the new focus");
    assert_eq!(retry.session.confirmed_project_cursor, discarded.confirmed);
    assert!(retry.delivered_through.expect("new page") > discarded.confirmed);
    assert!(retry.delivery_token.is_some());
    assert!(retry.session.pending_delivery);
    assert!(!retry.changes.expect("recomputed changes").is_empty());
}
