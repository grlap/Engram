use chrono::{TimeDelta, TimeZone};

use super::*;
use crate::storage::test_support::*;
use crate::*;

use crate::domain::{EffectClass, ProjectId, TurnIntent, TurnPurpose};

fn at(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).unwrap()
}

const T0: i64 = 1_700_000_000_000;

const NO_WARNINGS: [MediationWarning; 0] = [];

fn activity() -> ControlActivity {
    ControlActivity {
        last_session_activity: None,
        last_turn_begun: None,
        last_turn_settled: None,
        last_turn_evaluation: None,
        oldest_open_turn_begun: None,
        newest_open_turn_begun: None,
        latest_work_recorded: None,
    }
}

fn store_with(directory: &std::path::Path, assurance: ControlAssurance) -> SqliteStore {
    open_with_assurance(&directory.join("engram.db"), assurance).expect("store")
}

/// Issues and begins a turn in the binding's session, leaving it unsettled.
fn begin_open_turn(store: &mut SqliteStore, binding: &TestControlBinding, now: DateTime<Utc>) {
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "turn-open".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"turn-open"),
                purpose: Some(TurnPurpose::Ordinary),
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now,
        )
        .expect("issue the open turn");
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("the open turn must grant");
    };
    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                &[],
                "begin-open",
                now,
            )
            .expect("begin the open turn"),
        ControlTurnBeginDecision::Begin { .. }
    ));
}

/// A record kind the project feed carries without a work item.
const PROJECT_RECORD_KIND: &str = crate::storage::work::task_sync::folder::IMPORT_PLANNING_KIND;

/// Appends one record to the project's work feed, stored at `created_at`,
/// or at SQLite's own insertion time when `created_at` is `None`.
fn record_project_work(store: &SqliteStore, object_id: &str, created_at: Option<&str>) {
    let connection = &store.connection;
    match created_at {
        Some(created_at) => connection.execute(
            "INSERT INTO objects (object_id, object_kind, canonical_json, created_at)
             VALUES (?1, ?2, CAST('{}' AS BLOB), ?3)",
            rusqlite::params![object_id, PROJECT_RECORD_KIND, created_at],
        ),
        None => connection.execute(
            "INSERT INTO objects (object_id, object_kind, canonical_json)
             VALUES (?1, ?2, CAST('{}' AS BLOB))",
            rusqlite::params![object_id, PROJECT_RECORD_KIND],
        ),
    }
    .expect("insert work record");
    connection
        .execute(
            "INSERT INTO work_feed_heads (feed_kind, feed_id, position)
             VALUES ('project', 'project-a', 1)
             ON CONFLICT(feed_kind, feed_id) DO UPDATE SET position = position + 1",
            [],
        )
        .expect("advance project feed head");
    connection
        .execute(
            "INSERT INTO work_feed_entries (feed_kind, feed_id, position, object_kind, object_id)
             SELECT 'project', 'project-a', position, ?2, ?1
             FROM work_feed_heads WHERE feed_kind = 'project' AND feed_id = 'project-a'",
            rusqlite::params![object_id, PROJECT_RECORD_KIND],
        )
        .expect("append project feed entry");
}

#[test]
fn a_store_without_control_rows_reports_never_and_no_warning() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
    let store = store_with(directory.path(), ControlAssurance::TurnGated);
    let report = store.mediation_activity_at(at(T0)).expect("activity");
    assert_eq!(report.required_assurance, ControlAssurance::TurnGated);
    assert_eq!(report.activity, activity());
    assert_eq!(report.warnings, NO_WARNINGS);
}

#[test]
fn activity_follows_the_recorded_turns_and_work() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
    let mut store = store_with(directory.path(), ControlAssurance::TurnGated);
    let binding = bind_control(&mut store, at(T0));
    let settled_turn = at(T0 + 1_000);
    complete_control_turn(
        &mut store,
        &binding,
        "settled",
        vec![EffectClass::Observe],
        Vec::new(),
        settled_turn,
    );
    let open_turn = at(T0 + 60_000);
    begin_open_turn(&mut store, &binding, open_turn);
    // Work recorded twenty minutes after the bind.
    record_project_work(
        &store,
        "aa".repeat(16).as_str(),
        Some("2023-11-14 22:33:20"),
    );
    let work_recorded = at(T0 + 1_200_000);
    assert_eq!(
        work_recorded,
        Utc.with_ymd_and_hms(2023, 11, 14, 22, 33, 20).unwrap(),
        "the fixture text is twenty minutes after the bind"
    );

    let young = store
        .mediation_activity_at(open_turn + TimeDelta::minutes(30))
        .expect("activity while the open turn is young");
    assert_eq!(young.activity.last_turn_evaluation, Some(open_turn));
    assert_eq!(young.activity.last_turn_begun, Some(open_turn));
    assert_eq!(
        young.activity.last_turn_settled,
        Some(settled_turn + TimeDelta::milliseconds(2))
    );
    assert_eq!(young.activity.oldest_open_turn_begun, Some(open_turn));
    assert_eq!(young.activity.newest_open_turn_begun, Some(open_turn));
    assert_eq!(young.activity.latest_work_recorded, Some(work_recorded));
    let session_activity = young
        .activity
        .last_session_activity
        .expect("the bound session has an update time");
    assert!(session_activity >= at(T0) && session_activity <= open_turn);
    assert_eq!(
        young.warnings, NO_WARNINGS,
        "work inside a young open turn does not warn"
    );

    let old = store
        .mediation_activity_at(open_turn + OPEN_TURN_LIMIT)
        .expect("activity once the open turn is old");
    assert_eq!(
        old.warnings,
        vec![
            MediationWarning::WorkWithoutControlActivity {
                work_recorded,
                last_turn_activity: Some(open_turn),
            },
            MediationWarning::OpenTurnWithoutCompletion { begun: open_turn },
        ],
        "an old open turn no longer masks the work and is reported itself"
    );
}

#[test]
fn open_turns_in_two_sessions_report_the_oldest_and_newest() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
    let mut store = store_with(directory.path(), ControlAssurance::TurnGated);
    let first = bind_control_for(
        &mut store,
        "control-session-first",
        "bind-first",
        &[EffectClass::Observe, EffectClass::Communicate],
        at(T0),
    );
    let second = bind_control_for(
        &mut store,
        "control-session-second",
        "bind-second",
        &[EffectClass::Observe, EffectClass::Communicate],
        at(T0),
    );
    let wedged = at(T0 + 1_000);
    let young = wedged + TimeDelta::hours(5);
    begin_open_turn(&mut store, &first, wedged);
    begin_open_turn(&mut store, &second, young);
    let report = store
        .mediation_activity_at(young + TimeDelta::minutes(5))
        .expect("activity");
    assert_eq!(report.activity.oldest_open_turn_begun, Some(wedged));
    assert_eq!(report.activity.newest_open_turn_begun, Some(young));
    assert_eq!(
        report.warnings,
        vec![MediationWarning::OpenTurnWithoutCompletion { begun: wedged }],
        "a young turn in one session does not hide a wedged one in another"
    );
}

#[test]
fn an_advisory_store_reports_activity_without_warnings() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
    let mut store = store_with(directory.path(), ControlAssurance::Advisory);
    let binding = bind_control(&mut store, at(T0));
    begin_open_turn(&mut store, &binding, at(T0));
    record_project_work(
        &store,
        "bb".repeat(16).as_str(),
        Some("2023-11-15 22:13:20"),
    );
    let report = store
        .mediation_activity_at(at(T0) + TimeDelta::days(2))
        .expect("activity");
    assert_eq!(report.required_assurance, ControlAssurance::Advisory);
    assert!(report.activity.latest_work_recorded.is_some());
    assert_eq!(report.warnings, NO_WARNINGS);
}

#[test]
fn every_read_shares_the_callers_snapshot() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
    let reader = store_with(directory.path(), ControlAssurance::TurnGated);
    let snapshot = reader
        .connection
        .unchecked_transaction()
        .expect("open the caller's snapshot");
    assert_eq!(
        reader
            .mediation_activity_at(at(T0))
            .expect("activity read starts the snapshot")
            .activity,
        activity()
    );
    let writer = SqliteStore::open(directory.path().join("engram.db")).expect("second connection");
    record_project_work(
        &writer,
        "ee".repeat(16).as_str(),
        Some("2023-11-14 22:33:20"),
    );
    assert_eq!(
        reader
            .mediation_activity_at(at(T0))
            .expect("activity inside the same snapshot")
            .activity
            .latest_work_recorded,
        None,
        "work committed after the snapshot began is not half seen"
    );
    snapshot.rollback().expect("end the caller's snapshot");
    assert!(
        reader
            .mediation_activity_at(at(T0))
            .expect("activity in a fresh snapshot")
            .activity
            .latest_work_recorded
            .is_some()
    );
}

#[test]
fn sqlite_insertion_time_is_read_as_the_work_time() {
    let store = SqliteStore::open_in_memory().expect("store");
    let before = Utc::now() - TimeDelta::seconds(1);
    record_project_work(&store, "cc".repeat(16).as_str(), None);
    let after = Utc::now() + TimeDelta::seconds(1);
    let recorded = store
        .mediation_activity_at(Utc::now())
        .expect("activity")
        .activity
        .latest_work_recorded
        .expect("the stored work time parses");
    assert!(recorded >= before && recorded <= after);
}

#[test]
fn an_unreadable_work_time_is_refused_not_hidden() {
    let store = SqliteStore::open_in_memory().expect("store");
    record_project_work(
        &store,
        "dd".repeat(16).as_str(),
        Some("2023-11-14T22:33:20Z"),
    );
    assert!(matches!(
        store.mediation_activity_at(at(T0)),
        Err(StoreError::InvalidWorkProjection(reason)) if reason.contains("2023-11-14T22:33:20Z")
    ));
}

#[test]
fn an_out_of_range_turn_time_is_refused_not_hidden() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
    let mut store = store_with(directory.path(), ControlAssurance::TurnGated);
    let binding = bind_control(&mut store, at(T0));
    begin_open_turn(&mut store, &binding, at(T0));
    store
        .connection
        .execute(
            "UPDATE control_turn_grants SET begun_at_ms = ?1 WHERE state = 'begun'",
            [i64::MAX],
        )
        .expect("corrupt the begin time");
    assert!(matches!(
        store.mediation_activity_at(at(T0)),
        Err(StoreError::InvalidControlProjection(reason)) if reason.contains("turn begin")
    ));
}

#[test]
fn work_warns_only_beyond_the_grace_window() {
    let last = at(T0);
    let mut recorded = activity();
    recorded.last_turn_settled = Some(last);
    recorded.latest_work_recorded = Some(last + MEDIATION_GRACE);
    let now = last + TimeDelta::days(1);
    assert_eq!(
        mediation_warnings(ControlAssurance::TurnGated, &recorded, now),
        NO_WARNINGS
    );

    recorded.latest_work_recorded = Some(last + MEDIATION_GRACE + TimeDelta::seconds(1));
    assert_eq!(
        mediation_warnings(ControlAssurance::TurnGated, &recorded, now),
        vec![MediationWarning::WorkWithoutControlActivity {
            work_recorded: last + MEDIATION_GRACE + TimeDelta::seconds(1),
            last_turn_activity: Some(last),
        }]
    );
    assert_eq!(
        mediation_warnings(ControlAssurance::ActionGated, &recorded, now).len(),
        1,
        "a stronger policy also requires mediation"
    );
    assert_eq!(
        mediation_warnings(ControlAssurance::Advisory, &recorded, now),
        NO_WARNINGS
    );
}

#[test]
fn work_with_no_turn_ever_warns_with_no_last_activity() {
    let mut recorded = activity();
    recorded.last_session_activity = Some(at(T0));
    recorded.latest_work_recorded = Some(at(T0));
    assert_eq!(
        mediation_warnings(ControlAssurance::TurnGated, &recorded, at(T0)),
        vec![MediationWarning::WorkWithoutControlActivity {
            work_recorded: at(T0),
            last_turn_activity: None,
        }],
        "session activity alone is not a mediated turn"
    );
}

#[test]
fn the_latest_turn_activity_of_any_kind_counts() {
    let mut recorded = activity();
    recorded.last_turn_begun = Some(at(T0));
    recorded.last_turn_settled = Some(at(T0 - 1_000));
    recorded.last_turn_evaluation = Some(at(T0 + 5_000));
    recorded.latest_work_recorded = Some(at(T0 + 5_000) + MEDIATION_GRACE);
    assert_eq!(recorded.last_turn_activity(), Some(at(T0 + 5_000)));
    assert_eq!(
        mediation_warnings(
            ControlAssurance::TurnGated,
            &recorded,
            at(T0) + TimeDelta::days(1)
        ),
        NO_WARNINGS
    );
}

#[test]
fn a_young_open_turn_suppresses_and_an_old_one_is_reported() {
    let begun = at(T0);
    let mut recorded = activity();
    recorded.last_turn_begun = Some(begun);
    recorded.oldest_open_turn_begun = Some(begun);
    recorded.newest_open_turn_begun = Some(begun);
    recorded.latest_work_recorded = Some(begun + TimeDelta::minutes(30));
    let young = begun + OPEN_TURN_LIMIT - TimeDelta::milliseconds(1);
    assert_eq!(
        mediation_warnings(ControlAssurance::TurnGated, &recorded, young),
        NO_WARNINGS
    );
    let old = begun + OPEN_TURN_LIMIT;
    assert_eq!(
        mediation_warnings(ControlAssurance::TurnGated, &recorded, old),
        vec![
            MediationWarning::WorkWithoutControlActivity {
                work_recorded: begun + TimeDelta::minutes(30),
                last_turn_activity: Some(begun),
            },
            MediationWarning::OpenTurnWithoutCompletion { begun },
        ]
    );
}
