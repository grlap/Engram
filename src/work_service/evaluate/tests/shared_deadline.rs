//! An evaluation's open and write share one absolute deadline. Another opener
//! holds the store's open lock and then a writer holds its write lock; each
//! hold alone is shorter than the deadline. An evaluation submitted meanwhile
//! records once when the two holds end within it; with only a separate wait
//! per acquisition it records only after both, well past one deadline; and
//! with the shared deadline it is refused at the deadline, records nothing,
//! and an identical resend after the holds end records exactly once. The
//! record, the session's registration and a holder's focus commit together,
//! so nothing waits between a recorded evaluation and its answer.
//!
//! The assertions compare when each wait began and ended with when each hold
//! was released, so a slow host shifts the timings without changing what is
//! asserted.

use std::rc::Rc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::budget::{claimed_root, service};
use super::*;
use crate::storage::OperationDeadlineScope;

/// When the blocker released each of its holds.
struct Released {
    open_lock: Instant,
    write_lock: Instant,
}

/// Holds the store's write lock from the start until `open_lock +
/// write_lock` have passed, and its open lock for the first `open_lock`;
/// returns once both are held.
fn hold(database: &Path, open_lock: Duration, write_lock: Duration) -> JoinHandle<Released> {
    let sidecar = std::fs::canonicalize(database)
        .expect("store path")
        .with_added_extension("open-lock");
    let database = database.to_path_buf();
    let (held, holding) = mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let writer = rusqlite::Connection::open(&database).expect("blocking connection");
        writer
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold the write lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&sidecar)
            .expect("open lock sidecar");
        lock.lock().expect("hold the open lock");
        held.send(()).expect("report the holds");
        std::thread::sleep(open_lock);
        // Read before the release, so no wait can begin after the lock is
        // free yet before this instant.
        let open_lock = Instant::now();
        lock.unlock().expect("release the open lock");
        std::thread::sleep(write_lock);
        let write_lock = Instant::now();
        writer
            .execute_batch("ROLLBACK")
            .expect("release the write lock");
        Released {
            open_lock,
            write_lock,
        }
    });
    holding.recv().expect("both locks held");
    blocker
}

struct Fixture {
    _directory: crate::test_support::TempHome,
    database: std::path::PathBuf,
    project: ProjectId,
    work_ref: String,
    input: WorkEvaluateInput,
}

fn fixture(name: &str) -> Fixture {
    fixture_with(name, &[AcceptanceEvaluationMode::SameSession])
}

fn fixture_with(name: &str, modes: &[AcceptanceEvaluationMode]) -> Fixture {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId(name.into());
    let (work_ref, basis, evidence) =
        claimed_root(&database, &project, vec!["the submission records".into()]);
    enable(&database, modes, 3);
    let input = evaluate_input(
        &work_ref,
        1,
        basis,
        vec![verdict(
            1,
            "pass",
            "judgment",
            &[evidence.as_str().to_owned()],
        )],
    );
    Fixture {
        _directory: directory,
        database,
        project,
        work_ref,
        input,
    }
}

fn recorded(fixture: &Fixture) -> bool {
    recorded_in(&fixture.database, &fixture.project, &fixture.work_ref)
}

fn recorded_in(database: &Path, project: &ProjectId, work_ref: &str) -> bool {
    let store = SqliteStore::open(database).expect("store");
    let work = store
        .resolve_work_ref(project, work_ref)
        .expect("resolve")
        .work_id;
    store
        .acceptance_evaluation_status(work)
        .expect("status")
        .is_some()
}

/// What one submission did: its result, when it began and returned, and
/// when each of its writer acquisitions began waiting and returned.
struct Submitted {
    result: Result<WorkEvaluateResult, StoreError>,
    began: Instant,
    returned: Instant,
    writer_waits: Vec<(Instant, Instant)>,
}

/// Submits `fixture`'s evaluation through `service`, timed, with `before`
/// run before each writer acquisition. With a `deadline` it runs inside that
/// one; without, inside the ordinary one.
fn submit_with(
    fixture: &Fixture,
    service: &LocalWorkService,
    deadline: Option<Duration>,
    before: impl FnMut() + 'static,
) -> Submitted {
    crate::storage::with_writer_admission_test_policy(None, before, || {
        let began = Instant::now();
        let _deadline = deadline.map(OperationDeadlineScope::lasting);
        let result = service.work_evaluate_on(&fixture.input, at(4));
        Submitted {
            result,
            began,
            returned: Instant::now(),
            writer_waits: crate::storage::writer_admission_acquisitions(),
        }
    })
}

/// Submits through a service that has not opened the store yet, as each CLI
/// submission is.
fn submit(fixture: &Fixture, deadline: Option<Duration>) -> Submitted {
    let service = service(&fixture.database, &fixture.project);
    submit_with(fixture, &service, deadline, || {})
}

/// Each hold is shorter than this; the two together outlast it.
const DEADLINE: Duration = Duration::from_secs(6);
const OPEN_HOLD: Duration = Duration::from_millis(3_500);
const WRITE_HOLD: Duration = Duration::from_millis(4_500);

#[test]
fn holds_ending_within_the_shared_deadline_let_the_evaluation_record_once() {
    let fixture = fixture("evaluate-shared-deadline-within");
    let blocker = hold(
        &fixture.database,
        Duration::from_millis(800),
        Duration::from_millis(1_000),
    );
    let submitted = submit(&fixture, Some(DEADLINE));
    let released = blocker.join().expect("blocker");
    let result = submitted
        .result
        .expect("the evaluation records once both holds end");
    assert!(!result.replayed);
    // The open waited for the open lock, then the writer for the write lock.
    let (writer_began, writer_returned) = submitted.writer_waits[0];
    assert!(writer_began >= released.open_lock);
    assert!(writer_returned >= released.write_lock);
    assert!(recorded(&fixture));
}

// The same two holds against waits that each have only their own limit, as
// every wait had before an evaluation's waits shared one deadline: each hold
// fits its own wait, so the evaluation records, but only after the two waits
// together have run well past one deadline. The next test ends the same
// submission at that one deadline.
#[test]
fn separate_waits_each_within_the_limit_add_up_past_it() {
    let fixture = fixture("evaluate-shared-deadline-separate");
    let blocker = hold(&fixture.database, OPEN_HOLD, WRITE_HOLD);
    let submitted = crate::storage::operation_deadline::without_operation_deadlines(|| {
        crate::storage::with_store_lock_wait(DEADLINE, || submit(&fixture, None))
    });
    let released = blocker.join().expect("blocker");
    let result = submitted.result.expect("each hold fits its own wait");
    assert!(!result.replayed);
    let (writer_began, writer_returned) = submitted.writer_waits[0];
    assert!(writer_began >= released.open_lock);
    assert!(writer_returned >= released.write_lock);
    let waited = submitted.returned - submitted.began;
    assert!(waited > DEADLINE, "{waited:?}");
    assert!(recorded(&fixture));
}

#[test]
fn holds_outlasting_the_shared_deadline_refuse_at_it_and_record_nothing() {
    let fixture = fixture("evaluate-shared-deadline-past");
    let blocker = hold(&fixture.database, OPEN_HOLD, WRITE_HOLD);
    let submitted = submit(&fixture, Some(DEADLINE));
    let released = blocker.join().expect("blocker");
    let refusal = submitted
        .result
        .expect_err("the evaluation is refused at the shared deadline");
    assert!(
        matches!(
            &refusal,
            StoreError::WorkWriterAdmissionRefused {
                reason: crate::storage::WorkWriterAdmissionReason::BusyBudgetExhausted,
                ..
            }
        ),
        "{refusal:?}"
    );
    // The writer began after the open waited out the open lock, and gave up
    // at the deadline while the write lock was still held: the two waits
    // shared one deadline.
    let (writer_began, writer_returned) = submitted.writer_waits[0];
    assert!(writer_began >= released.open_lock);
    assert!(writer_returned < released.write_lock);
    let waited = submitted.returned - submitted.began;
    assert!(
        waited >= DEADLINE.saturating_sub(Duration::from_millis(50)),
        "{waited:?}"
    );
    // Nothing half-recorded: no evaluation exists after the refusal.
    assert!(!recorded(&fixture));
    // An identical resend records exactly once, and its repeat replays it.
    let first = submit(&fixture, Some(DEADLINE))
        .result
        .expect("the resend records");
    assert!(!first.replayed);
    let again = submit(&fixture, Some(DEADLINE))
        .result
        .expect("the repeat replays");
    assert!(again.replayed);
    assert_eq!(again.evaluation, first.evaluation);
}

/// The item's id and the evaluating session's current focus.
fn focus(fixture: &Fixture) -> (String, Option<String>) {
    let store = SqliteStore::open(&fixture.database).expect("store");
    let work = store
        .resolve_work_ref(&fixture.project, &fixture.work_ref)
        .expect("resolve")
        .work_id
        .0
        .to_string();
    let focused = rusqlite::Connection::open(&fixture.database)
        .expect("reader")
        .query_row(
            "SELECT focused_work_id FROM work_session_state WHERE session_id = ?1",
            [super::budget::SESSION],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    (work, focused)
}

/// Another writer, connected ahead, that takes the store's write lock when
/// told to, reports that it holds it, and keeps it for `hold`; joining it
/// gives when it released the lock.
struct Contender {
    go: mpsc::Sender<()>,
    holding: mpsc::Receiver<()>,
    thread: JoinHandle<Instant>,
}

impl Contender {
    fn connect(database: &Path, hold_for: Duration) -> Self {
        let writer = rusqlite::Connection::open(database).expect("contending connection");
        let (go, told) = mpsc::channel();
        let (reported, holding) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            told.recv().expect("told to take the lock");
            writer
                .execute_batch("BEGIN IMMEDIATE")
                .expect("take the write lock");
            reported.send(()).expect("report the hold");
            std::thread::sleep(hold_for);
            let released = Instant::now();
            writer.execute_batch("ROLLBACK").expect("release");
            released
        });
        Self {
            go,
            holding,
            thread,
        }
    }

    /// Takes the lock now, returning once it is held.
    fn take(&self) {
        self.go.send(()).expect("tell the contender");
        self.holding.recv().expect("the contender holds the lock");
    }
}

// A submission whose deadline has already passed when it reaches the writer
// is refused there even with the writer free: no write begins after the
// deadline, so nothing is recorded, and a resend records once.
#[test]
fn no_write_begins_after_the_deadline_even_with_the_writer_free() {
    let fixture = fixture("evaluate-shared-deadline-expired");
    let submitted = submit(&fixture, Some(Duration::ZERO));
    let refusal = submitted
        .result
        .expect_err("a write is not begun after the deadline");
    assert!(
        matches!(
            &refusal,
            StoreError::WorkWriterAdmissionRefused {
                reason: crate::storage::WorkWriterAdmissionReason::BusyBudgetExhausted,
                ..
            }
        ),
        "{refusal:?}"
    );
    assert!(!recorded(&fixture));
    let resent = submit(&fixture, Some(DEADLINE))
        .result
        .expect("the resend records");
    assert!(!resent.replayed);
    assert!(recorded(&fixture));
}

// A submission admitted to the writer but whose deadline passes before its
// COMMIT is given back whole: nothing commits after the deadline, nothing is
// recorded, and a resend records once.
#[test]
fn nothing_commits_after_the_deadline() {
    let fixture = fixture("evaluate-shared-deadline-commit");
    let deadline = Duration::from_secs(1);
    let mut settled = false;
    let service = service(&fixture.database, &fixture.project);
    let result = crate::storage::with_writer_settle_hook(
        move || {
            // The record's writes are made; let its deadline pass before it
            // settles.
            if !std::mem::replace(&mut settled, true) {
                std::thread::sleep(deadline + Duration::from_millis(200));
            }
        },
        || {
            let _deadline = OperationDeadlineScope::lasting(deadline);
            service.work_evaluate_on(&fixture.input, at(4))
        },
    );
    let refusal = result.expect_err("nothing commits after the deadline");
    assert!(
        matches!(&refusal, StoreError::Sqlite(error)
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)),
        "{refusal:?}"
    );
    assert!(!recorded(&fixture));
    let resent = submit(&fixture, Some(DEADLINE))
        .result
        .expect("the resend records");
    assert!(!resent.replayed);
    assert!(recorded(&fixture));
}

// A same-session holder's evaluate word moves its focus in the record's own
// transaction. Another writer takes the write lock right after the commit that
// records the evaluation, before the answer is assembled, and keeps it well
// past the deadline: the word answers, with the focus moved, while that lock
// is still held, since nothing is left to write once the record commits.
#[test]
fn a_writer_taking_the_lock_once_the_record_commits_does_not_hold_up_its_answer() {
    let fixture = fixture("evaluate-shared-deadline-after-record");
    let deadline = Duration::from_secs(2);
    let contender = Rc::new(Contender::connect(
        &fixture.database,
        Duration::from_secs(4),
    ));
    let after = Rc::clone(&contender);
    let (database, project, work_ref) = (
        fixture.database.clone(),
        fixture.project.clone(),
        fixture.work_ref.clone(),
    );
    let mut taken = false;
    let agent = crate::verbs::AgentVerbs::new(
        fixture.database.clone(),
        fixture.project.clone(),
        "agent".into(),
        SessionId(super::budget::SESSION.into()),
        Some("evaluate_shared_deadline_test".into()),
    );
    let input = &fixture.input;
    let request = crate::verbs::EvaluateInput {
        work_ref: Some(fixture.work_ref.clone()),
        mode: input.mode.clone(),
        acceptance_basis: input.acceptance_basis,
        evidence_basis: input.evidence_basis,
        verdicts: input.verdicts.clone(),
        attempt: None,
        source_id: None,
        model: None,
        execution_identity: None,
        parent_session: None,
        supersedes: None,
    };
    let (answered, began, returned) = crate::storage::with_writer_commit_hook(
        || {},
        move || {
            // On the commit that leaves the evaluation recorded.
            if !taken && recorded_in(&database, &project, &work_ref) {
                taken = true;
                after.take();
            }
        },
        || {
            let began = Instant::now();
            let _deadline = OperationDeadlineScope::lasting(deadline);
            let answered = agent.evaluate(request, at(4));
            (answered, began, Instant::now())
        },
    );
    let Ok(contender) = Rc::try_unwrap(contender) else {
        panic!("the hook has finished with the contender");
    };
    let released = contender.thread.join().expect("contender");
    answered.expect("a recorded evaluation is answered, not refused");
    // The word answered while the other writer still held the lock.
    assert!(returned < released, "{:?}", returned - began);
    assert!(recorded(&fixture));
    let (item, focused) = focus(&fixture);
    assert_eq!(focused, Some(item));
}

// An exact resend that owes nothing answers from its read, without the
// writer: another writer holding the lock past the deadline does not hold it
// up, as it must not for a host resending after its own timeout. The resend
// comes from a new process reusing the evaluator's generated session, whose
// row the first submission registered, and the evaluator holds no claim.
#[test]
fn a_resend_that_owes_nothing_answers_while_another_writer_holds_the_lock() {
    let fixture = fixture_with(
        "evaluate-shared-deadline-replay",
        &[
            AcceptanceEvaluationMode::SameSession,
            AcceptanceEvaluationMode::IndependentSession,
        ],
    );
    let session = process_default_session_at(41, at(4));
    let process = || {
        LocalWorkService::new_with_attribution(
            fixture.database.clone(),
            fixture.project.clone(),
            "evaluator".into(),
            session.clone(),
            Some("evaluate_shared_deadline_test".into()),
            None,
            WorkAttributionDefaults {
                actor: None,
                session: true,
            },
        )
    };
    let mut input = fixture.input.clone();
    input.mode = "independent_session".into();
    let first = process()
        .work_evaluate_on(&input, at(4))
        .expect("the evaluation records and registers the session");
    assert!(!first.replayed);
    let contender = Contender::connect(&fixture.database, Duration::from_secs(3));
    contender.take();
    let began = Instant::now();
    let resent = {
        let _deadline = OperationDeadlineScope::lasting(Duration::from_secs(1));
        process().work_evaluate_on(&input, at(5))
    };
    let returned = Instant::now();
    let released = contender.thread.join().expect("contender");
    let resent = resent.expect("the resend answers the recorded evaluation");
    assert!(resent.replayed);
    assert_eq!(resent.evaluation, first.evaluation);
    assert!(returned < released, "{:?}", returned - began);
}

// A holder's exact resend through the evaluate word, once the first
// submission has recorded the evaluation and moved the holder's focus to the
// item, owes nothing: it answers from its read while another writer holds the
// lock past the deadline.
#[test]
fn an_already_focused_holder_resend_answers_while_another_writer_holds_the_lock() {
    let fixture = fixture("evaluate-shared-deadline-holder-replay");
    let request = || {
        let input = &fixture.input;
        crate::verbs::EvaluateInput {
            work_ref: Some(fixture.work_ref.clone()),
            mode: input.mode.clone(),
            acceptance_basis: input.acceptance_basis,
            evidence_basis: input.evidence_basis,
            verdicts: input.verdicts.clone(),
            attempt: None,
            source_id: None,
            model: None,
            execution_identity: None,
            parent_session: None,
            supersedes: None,
        }
    };
    let agent = || {
        crate::verbs::AgentVerbs::new(
            fixture.database.clone(),
            fixture.project.clone(),
            "agent".into(),
            SessionId(super::budget::SESSION.into()),
            Some("evaluate_shared_deadline_test".into()),
        )
    };
    agent()
        .evaluate(request(), at(4))
        .expect("the evaluation records and moves the holder's focus");
    let (item, focused) = focus(&fixture);
    assert_eq!(focused, Some(item));
    let contender = Contender::connect(&fixture.database, Duration::from_secs(3));
    contender.take();
    let began = Instant::now();
    let resent = {
        let _deadline = OperationDeadlineScope::lasting(Duration::from_secs(1));
        agent().evaluate(request(), at(5))
    };
    let returned = Instant::now();
    let released = contender.thread.join().expect("contender");
    resent.expect("the resend answers the recorded evaluation");
    assert!(returned < released, "{:?}", returned - began);
}

// A concurrent call on the same service, as MCP words share one connection,
// holds that connection past the deadline: the evaluation stops waiting for
// it at the deadline, records nothing, and records once it is submitted again.
#[test]
fn the_shared_connection_held_past_the_deadline_refuses_at_it() {
    let fixture = fixture("evaluate-shared-deadline-connection");
    let service = service(&fixture.database, &fixture.project);
    let deadline = Duration::from_secs(1);
    let hold_for = Duration::from_millis(2_500);
    let (held, holding) = mpsc::channel();
    let submitted = std::thread::scope(|scope| {
        let holder = scope.spawn(|| {
            let guard = service
                .lock_store_at(at(4))
                .expect("the service's connection");
            held.send(()).expect("report the hold");
            std::thread::sleep(hold_for);
            let released = Instant::now();
            drop(guard);
            released
        });
        holding.recv().expect("the connection is held");
        let submitted = submit_with(&fixture, &service, Some(deadline), || {});
        (submitted, holder.join().expect("holder"))
    });
    let (submitted, released) = submitted;
    let refusal = submitted
        .result
        .expect_err("the evaluation stops waiting at the deadline");
    assert!(
        matches!(&refusal, StoreError::Sqlite(error)
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)),
        "{refusal:?}"
    );
    assert!(submitted.returned < released);
    assert_eq!(submitted.writer_waits.len(), 0);
    assert!(!recorded(&fixture));
    let resent = submit_with(&fixture, &service, Some(deadline), || {})
        .result
        .expect("the resend records");
    assert!(!resent.replayed);
    assert!(recorded(&fixture));
}
