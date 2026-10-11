//! A fresh process-default session registers on its first admitted write
//! only: a request refused for its own input, through the core service or a
//! word, leaves the store as it was, with no session row and nothing
//! reclaimed. The first valid word then registers once, at its own time,
//! and reclaims; a later word does not register again.

use super::*;
use crate::verbs::{
    AddInput, AgentVerbs, ClaimInput, DoneInput, ForgetInput, NoteInput, RememberInput,
    UpdateAction, UpdateInput,
};
use rusqlite::OptionalExtension;

const DAY: i64 = 24 * 60 * 60;

/// A process-default service whose session was minted at `second`.
fn process_default(
    database: &std::path::Path,
    project: &ProjectId,
    pid: u32,
    second: i64,
) -> LocalWorkService {
    LocalWorkService::new_with_attribution(
        database.to_path_buf(),
        project.clone(),
        "agent".into(),
        process_default_session_at(pid, at(second)),
        None,
        None,
        WorkAttributionDefaults {
            actor: None,
            session: true,
        },
    )
}

/// The same, as the agent words see it.
fn process_default_words(
    database: &std::path::Path,
    project: &ProjectId,
    pid: u32,
    second: i64,
) -> AgentVerbs {
    AgentVerbs::new_with_attribution(
        database.to_path_buf(),
        project.clone(),
        "agent".into(),
        process_default_session_at(pid, at(second)),
        None,
        None,
        WorkAttributionDefaults {
            actor: None,
            session: true,
        },
    )
}

/// A session that noted `item` at `second` and has been idle since: its row
/// and its finished attempt are what a later registration may reclaim.
fn idle_session(
    database: &std::path::Path,
    project: &ProjectId,
    pid: u32,
    item: &str,
    second: i64,
) -> SessionId {
    let service = process_default(database, project, pid, second);
    service
        .work_note_on(Some(item), "an old observation", &[], at(second))
        .expect("idle session notes");
    service.session_id.clone()
}

fn session_row(database: &std::path::Path, session: &SessionId) -> Option<i64> {
    rusqlite::Connection::open(database)
        .expect("reader")
        .query_row(
            "SELECT updated_at_ms FROM work_session_state WHERE session_id = ?1",
            [&session.0],
            |row| row.get(0),
        )
        .optional()
        .expect("session row")
}

fn attempts_of(database: &std::path::Path, session: &SessionId) -> i64 {
    rusqlite::Connection::open(database)
        .expect("reader")
        .query_row(
            "SELECT COUNT(*) FROM work_protocol_attempts WHERE session_id = ?1",
            [&session.0],
            |row| row.get(0),
        )
        .expect("attempts")
}

fn update(input: serde_json::Value) -> WorkUpdateInput {
    serde_json::from_value(input).expect("update input")
}

fn propose(input: serde_json::Value) -> WorkProposeInput {
    serde_json::from_value(input).expect("proposal input")
}

/// One refusal: what it is, a part of the refusal it must give, and the
/// call, made through a fresh process-default service and the words of
/// another, returning its refusal.
type Refusal<'a> = (
    &'static str,
    &'static str,
    Box<dyn Fn(&LocalWorkService, &AgentVerbs) -> Option<String> + 'a>,
);

/// The refusals, one row each, all of request-only invalid input on `item`;
/// `record` is the id of a record the store holds.
fn refusal_rows<'a>(item: &'a str, record: &'a str, now: i64) -> Vec<Refusal<'a>> {
    let link = "deadbeefdeadbeef";
    vec![
        (
            "update naming its own target as prerequisite",
            "WorkDependencyCycle",
            Box::new(move |fresh, _| {
                fresh
                    .work_update_on(
                        Some(item),
                        update(
                            serde_json::json!({"kind": "add_prerequisite", "prerequisite": item}),
                        ),
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "completion naming one criterion twice",
            "acceptance results contain a duplicate criterion",
            Box::new(move |fresh, _| {
                fresh
                    .work_complete_on(
                        Some(item),
                        serde_json::from_value(serde_json::json!({"acceptance": [
                            {"criterion": "Item accepted", "satisfied": true, "note": "met"},
                            {"criterion": "Item accepted", "satisfied": true, "note": "met"},
                        ]}))
                        .expect("completion input"),
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "evaluation of criterion 0",
            "verdict positions are one-based",
            Box::new(move |fresh, _| {
                fresh
                    .work_evaluate_on(
                        &serde_json::from_value(serde_json::json!({
                            "work_ref": item, "mode": "same_session", "acceptance_basis": 1,
                            "evidence_basis": 0, "verdicts": [{
                                "criterion": 0, "verdict": "pass", "basis": "asserted",
                                "rationale": "met", "evidence": [],
                            }],
                        }))
                        .expect("evaluation input"),
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "decomposition whose child waits on its own parent",
            "WorkDependencyCycle",
            Box::new(move |fresh, _| {
                fresh
                    .work_propose_on(
                        Some(item),
                        propose(serde_json::json!({"kind": "decompose", "children": [{
                            "key": "a", "title": "Child", "outcome": "done", "acceptance": ["done"],
                        }], "prerequisites": [{"work_key": "a", "prerequisite": item}]})),
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "plan naming an unknown item",
            "does not exist in project",
            Box::new(move |fresh, _| {
                fresh
                    .work_propose_on(
                        None,
                        propose(serde_json::json!({"kind": "plan", "plan": {
                            "tasks": [{"key": "t", "parent_key": null, "title": "Task",
                                "outcome": "done", "acceptance": ["done"]}],
                            "prerequisites": [{"work_key": "t",
                                "prerequisite": {"kind": "existing", "value": "w-000000000000"}}],
                            "idempotency_key": "plan-1",
                        }})),
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "acknowledgement token without a cursor",
            "work delivery token requires acknowledge_through",
            Box::new(move |fresh, _| {
                fresh
                    .work_next_with_delivery_token(
                        1,
                        None,
                        Some("token"),
                        WorkNextQuery::default(),
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "negative acknowledgement cursor",
            "work acknowledgement cursor must not be negative",
            Box::new(move |fresh, _| {
                fresh
                    .work_next_with_delivery_token(
                        1,
                        Some(-1),
                        Some("token"),
                        WorkNextQuery::default(),
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "import with a blank criterion",
            "import draft fields must be normalized nonblank prose",
            Box::new(move |fresh, _| {
                let input: crate::domain::WorkImportInput = serde_json::from_value(serde_json::json!({
                    "snapshot": {
                        "schema_version": crate::schema::SCHEMA_VERSION, "adapter_kind": "planner",
                        "canonical_ref": "plan/item-1",
                        "projected": {"title": "External", "body": null, "status": null, "owner": null},
                        "captured_at": at(now), "source_revision": "1", "fingerprint": "rev-1",
                        "canonical_url": null, "payload_hash": "a".repeat(64), "raw": {},
                    },
                    "draft": {"title": "Local", "outcome": "Local outcome", "acceptance": [" "]},
                }))
                .expect("import input");
                let token: ObjectId = "b".repeat(64).parse().expect("token");
                fresh
                    .apply_work_import(&input, &token, at(now))
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "claim word with a negative lifetime",
            "claim TTL must be from 1 through 86400 seconds",
            Box::new(move |_, words| {
                words
                    .claim(
                        ClaimInput {
                            work_ref: item.to_owned(),
                            ttl_seconds: Some(-5),
                            recover: None,
                        },
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "update word naming its own target as prerequisite",
            "WorkDependencyCycle",
            Box::new(move |_, words| {
                words
                    .update(
                        UpdateInput {
                            work_ref: Some(item.to_owned()),
                            action: UpdateAction::After {
                                prerequisite: item.to_owned(),
                            },
                        },
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "done word linking criterion 0",
            "criterion position is outside the acceptance list",
            Box::new(move |_, words| {
                words
                    .done(
                        DoneInput {
                            work_ref: Some(item.to_owned()),
                            links: vec![crate::work_service::WorkCriterionLinkInput {
                                criterion: 0,
                                locator: link.into(),
                            }],
                            link_basis: Some(1),
                            ..DoneInput::default()
                        },
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "note word with a summary over the note bound",
            "WorkNoteTooLarge",
            Box::new(move |_, words| {
                words
                    .note(
                        &NoteInput {
                            status: false,
                            work_ref: Some(item.to_owned()),
                            text: "x".repeat(64 * 1024 + 1),
                            refs: Vec::new(),
                        },
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "add under the item with a binding pinned to a stored record",
            "names a stored record rather than a check's command",
            Box::new(move |_, words| {
                words
                    .add(
                        AddInput {
                            title: "Child".into(),
                            under: Some(item.to_owned()),
                            bindings: vec![format!("1=build:{record}")],
                            ..AddInput::default()
                        },
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
    ]
}

/// One remember request through the core service, as a row of the table
/// below: its body, key, revise flag, basis revision, target change and edit.
struct Remember {
    body: String,
    key: Option<&'static str>,
    revise: bool,
    basis: Option<u64>,
    target: crate::domain::ProjectMemoryRetiringTargetChange,
    edit: crate::domain::ProjectMemoryEdit,
}

impl Remember {
    fn whole(body: &str, key: Option<&'static str>) -> Self {
        Self {
            body: body.into(),
            key,
            revise: false,
            basis: None,
            target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
            edit: crate::domain::ProjectMemoryEdit::Whole,
        }
    }
}

fn set_target(
    target: crate::domain::ProjectMemoryRetiringTargetInput,
) -> crate::domain::ProjectMemoryRetiringTargetChange {
    crate::domain::ProjectMemoryRetiringTargetChange::Set { target }
}

fn external(project: &str, reference: &str) -> crate::domain::ProjectMemoryRetiringTargetInput {
    crate::domain::ProjectMemoryRetiringTargetInput::External {
        project: project.into(),
        reference: reference.into(),
    }
}

/// Every refusal of a remember request that depends on the request alone,
/// one row each. The session and actor checks come from the service's
/// attribution, not the request, and the noop redactor refuses nothing.
fn remember_rows() -> Vec<(&'static str, &'static str, Remember)> {
    use crate::domain::ProjectMemoryEdit;
    vec![
        (
            "remember with an empty body",
            "memory body must not be empty",
            Remember::whole("  ", None),
        ),
        (
            "remember with a body over its bound",
            "memory body exceeds",
            Remember::whole(&"x".repeat(8 * 1024 + 1), Some("oversized")),
        ),
        (
            "remember whose body yields no key",
            "memory body cannot produce a safe key",
            Remember::whole("!!!", None),
        ),
        (
            "remember with an unsafe key",
            "memory key must be",
            Remember::whole("A body", Some("BAD")),
        ),
        (
            "remember revising without a key",
            "--revise requires --key",
            Remember {
                revise: true,
                ..Remember::whole("A body", None)
            },
        ),
        (
            "remember naming a basis without revising",
            "--expected-revision requires --revise",
            Remember {
                basis: Some(1),
                ..Remember::whole("A body", Some("basis"))
            },
        ),
        (
            "remember revising basis revision 0",
            "--expected-revision requires --revise and a positive revision",
            Remember {
                revise: true,
                basis: Some(0),
                ..Remember::whole("A body", Some("basis"))
            },
        ),
        (
            "remember appending without revising",
            "--append and --section revise a memory",
            Remember {
                edit: ProjectMemoryEdit::Append,
                ..Remember::whole("More", Some("appended"))
            },
        ),
        (
            "remember replacing a section without a basis",
            "--append and --section revise a memory",
            Remember {
                revise: true,
                edit: ProjectMemoryEdit::Section {
                    name: "part".into(),
                },
                ..Remember::whole("Part", Some("sectioned"))
            },
        ),
        (
            "remember replacing a section named out of shape",
            "a section name is 1 to",
            Remember {
                revise: true,
                basis: Some(1),
                edit: ProjectMemoryEdit::Section { name: "BAD".into() },
                ..Remember::whole("Part", Some("sectioned"))
            },
        ),
        (
            "remember replacing a section with text that carries markers",
            "replacement text for a section must not carry section markers",
            Remember {
                revise: true,
                basis: Some(1),
                edit: ProjectMemoryEdit::Section {
                    name: "part".into(),
                },
                ..Remember::whole("<!-- engram-section other -->", Some("sectioned"))
            },
        ),
        (
            "remember appending text whose markers do not pair",
            "section `open` is never closed",
            Remember {
                revise: true,
                basis: Some(1),
                edit: ProjectMemoryEdit::Append,
                ..Remember::whole("<!-- engram-section open -->", Some("appended"))
            },
        ),
        (
            "remember clearing a retirement target without revising",
            "clearing a retirement target requires --revise",
            Remember {
                target: crate::domain::ProjectMemoryRetiringTargetChange::Clear,
                ..Remember::whole("A body", Some("cleared"))
            },
        ),
        (
            "remember with a local retirement reference out of shape",
            "local retirement reference is invalid",
            Remember {
                target: set_target(crate::domain::ProjectMemoryRetiringTargetInput::Local {
                    work_ref: " w-padded".into(),
                }),
                ..Remember::whole("A body", Some("retired-locally"))
            },
        ),
        (
            "remember with an external retirement project missing",
            "external retirement project must be",
            Remember {
                target: set_target(external("", "reference")),
                ..Remember::whole("A body", Some("retired-elsewhere"))
            },
        ),
        (
            "remember with an external retirement reference out of shape",
            "external retirement reference must be",
            Remember {
                target: set_target(external("project", "has space")),
                ..Remember::whole("A body", Some("retired-elsewhere"))
            },
        ),
        (
            "remember whose full read would not fit its budget as text",
            "full memory response requires",
            Remember::whole(&"a\n".repeat(4096), Some("too-wide")),
        ),
        (
            "remember whose full read would not fit its budget as JSON",
            "serialized full memory response requires",
            Remember::whole(&"\"".repeat(8 * 1024), Some("too-quoted")),
        ),
        (
            "remember appending text no basis leaves room for",
            "serialized full memory response requires",
            Remember {
                revise: true,
                basis: Some(1),
                edit: ProjectMemoryEdit::Append,
                ..Remember::whole(&"\"".repeat(7000), Some("appended"))
            },
        ),
        (
            "remember replacing a section with text no basis leaves room for",
            "serialized full memory response requires",
            Remember {
                revise: true,
                basis: Some(1),
                edit: ProjectMemoryEdit::Section {
                    name: "part".into(),
                },
                ..Remember::whole(&"\"".repeat(7000), Some("sectioned"))
            },
        ),
    ]
}

/// A fresh process-default session's first remember or forget registers it
/// inside that write when the write succeeds, a replay included: its row
/// appears at the write's time, an eligible idle session is reclaimed, and
/// the service records the registration.
#[test]
fn a_first_memory_write_registers_inside_its_own_transaction() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("memory-registration".into());
    let now = 11 * DAY;
    let holder = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("holder".into()),
        Some("registration-test".into()),
    );
    let item = proposed_root(
        holder
            .work_propose(root_input("Item", "item"), at(0))
            .expect("item"),
    )
    .short_ref;
    let remember = |service: &LocalWorkService, body: &str, key: &str, second: i64| {
        service.revise_project_memory(
            body.into(),
            Some(key.into()),
            false,
            None,
            crate::domain::ProjectMemoryRetiringTargetChange::Keep,
            &crate::domain::ProjectMemoryEdit::Whole,
            at(second),
        )
    };

    // A new memory registers and reclaims.
    let stale = idle_session(&database, &project, 10, &item, DAY);
    let fresh = process_default(&database, &project, 30, now);
    remember(&fresh, "A note", "first", now).expect("remembered");
    assert_eq!(
        session_row(&database, &fresh.session_id),
        Some(at(now).timestamp_millis())
    );
    assert!(session_row(&database, &stale).is_none(), "reclaimed");
    assert!(fresh.process_default_session_initialized.get().is_some());

    // An exact replay registers a session whose row is gone.
    let first = process_default(&database, &project, 31, now);
    remember(&first, "Replayed", "replayed", now).expect("remembered");
    rusqlite::Connection::open(&database)
        .expect("writer")
        .execute(
            "DELETE FROM work_session_state WHERE session_id = ?1",
            [&first.session_id.0],
        )
        .expect("row removed");
    // The same process-default session, through a service that has not yet
    // registered it.
    let again = LocalWorkService::new_with_attribution(
        database.clone(),
        project.clone(),
        "agent".into(),
        first.session_id.clone(),
        None,
        None,
        WorkAttributionDefaults {
            actor: None,
            session: true,
        },
    );
    let replay = remember(&again, "Replayed", "replayed", now + 5).expect("replayed");
    assert!(replay.duplicate);
    assert_eq!(
        session_row(&database, &again.session_id),
        Some(at(now + 5).timestamp_millis())
    );
    assert!(again.process_default_session_initialized.get().is_some());

    // A forget of a key already retired replays, and registers.
    remember(&holder, "Gone soon", "gone", 1).expect("remembered");
    holder
        .forget_project_memory("gone".into(), at(2))
        .expect("retired");
    let forgetting = process_default(&database, &project, 32, now);
    let forgot = forgetting
        .forget_project_memory("gone".into(), at(now))
        .expect("replayed forget");
    assert!(forgot.duplicate);
    assert_eq!(
        session_row(&database, &forgetting.session_id),
        Some(at(now).timestamp_millis())
    );
    assert!(
        forgetting
            .process_default_session_initialized
            .get()
            .is_some()
    );
}

/// A basis whose last line opens a section with a bare carriage return
/// gains that line's break from the append separator, so appended text may
/// close the section first: the append is admitted, and its exact retry
/// replays.
#[test]
fn an_append_may_close_a_section_its_basis_opens_on_a_bare_carriage_return() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let service = LocalWorkService::new(
        database,
        ProjectId("bare-return".into()),
        "agent".into(),
        SessionId("rememberer".into()),
        Some("registration-test".into()),
    );
    let remember = |body: &str, revise: bool, basis: Option<u64>, edit, second| {
        service.revise_project_memory(
            body.into(),
            Some("bare-return".into()),
            revise,
            basis,
            crate::domain::ProjectMemoryRetiringTargetChange::Keep,
            &edit,
            at(second),
        )
    };
    remember(
        "intro\n<!-- engram-section a -->\r",
        false,
        None,
        crate::domain::ProjectMemoryEdit::Whole,
        0,
    )
    .expect("the basis");
    let close = "x\n<!-- /engram-section a -->";
    let appended = remember(
        close,
        true,
        Some(1),
        crate::domain::ProjectMemoryEdit::Append,
        1,
    )
    .expect("the append closes the section its basis opened");
    assert!(!appended.duplicate);
    let retry = remember(
        close,
        true,
        Some(1),
        crate::domain::ProjectMemoryEdit::Append,
        2,
    )
    .expect("the exact retry replays");
    assert!(retry.duplicate, "{retry:?}");
}

/// Evaluations of `item` the evaluation word refuses: one too large for any
/// record, with sixteen verdicts whose rationales fit their own bound and,
/// as a list, the evaluation cap, but whose verdicts together do not; and
/// one whose refusal comes from the store inside the evaluation's own write,
/// where this project's policy does not enable evaluation.
fn evaluation_size_rows(item: &str, now: i64) -> Vec<Refusal<'_>> {
    vec![
        (
            "evaluation the project's policy does not admit",
            "does not enable acceptance evaluation",
            Box::new(move |fresh, _| {
                fresh
                    .work_evaluate_on(
                        &serde_json::from_value(serde_json::json!({
                            "work_ref": item, "mode": "same_session", "acceptance_basis": 1,
                            "evidence_basis": 0, "verdicts": [{
                                "criterion": 1, "verdict": "fail", "basis": "asserted",
                                "rationale": "not yet", "evidence": [],
                            }],
                        }))
                        .expect("evaluation input"),
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "evaluation whose verdicts no record can hold",
            "the evaluation's verdicts alone take",
            Box::new(move |fresh, _| {
                let verdicts = (1..=16)
                    .map(|criterion| {
                        serde_json::json!({
                            "criterion": criterion, "verdict": "fail", "basis": "asserted",
                            "rationale": "r".repeat(65_530), "evidence": [],
                        })
                    })
                    .collect::<Vec<_>>();
                fresh
                    .work_evaluate_on(
                        &serde_json::from_value(serde_json::json!({
                            "work_ref": item, "mode": "same_session", "acceptance_basis": 1,
                            "evidence_basis": 0, "verdicts": verdicts,
                        }))
                        .expect("evaluation input"),
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
    ]
}

/// Refusals of the handoff and gate words' request-only input on `item`.
fn handoff_and_gate_rows(item: &str, now: i64) -> Vec<Refusal<'_>> {
    vec![
        (
            "handoff offered to its own session",
            "handoff source and destination must differ",
            Box::new(move |fresh, _| {
                fresh
                    .work_handoff_on(
                        Some(item),
                        serde_json::from_value(serde_json::json!({
                            "kind": "offer", "to": fresh.session_id.0, "ttl_seconds": null,
                            "checkpoint_summary": "ready",
                        }))
                        .expect("handoff input"),
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "gate with a blank name",
            "gate name must not be empty",
            Box::new(move |fresh, _| {
                fresh
                    .work_gate_on(Some(item), "  ", &[], None, at(now))
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
    ]
}

/// An exact retry of a memory admitted near its read budget replays,
/// whatever the retry's own attribution context and time: the bound a
/// remember meets before the store opens judges only what every outcome of
/// the request shares.
#[test]
fn an_exact_retry_near_the_read_budget_replays() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("retry-budget".into());
    let service = |context: &str| {
        LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("rememberer".into()),
            Some(context.into()),
        )
    };
    let first = service("short");
    let remember = |service: &LocalWorkService, length: usize, at| {
        service.revise_project_memory(
            "\"".repeat(length),
            Some(format!("probe-{length}")),
            false,
            None,
            crate::domain::ProjectMemoryRetiringTargetChange::Keep,
            &crate::domain::ProjectMemoryEdit::Whole,
            at,
        )
    };
    // The longest body the first remember admits, each probe under its own key.
    let (mut admitted, mut refused) = (1, 8 * 1024 + 1);
    assert!(remember(&first, admitted, at(0)).is_ok());
    while refused - admitted > 1 {
        let middle = admitted + (refused - admitted) / 2;
        if remember(&first, middle, at(0)).is_ok() {
            admitted = middle;
        } else {
            refused = middle;
        }
    }
    assert!(refused <= 8 * 1024, "the budget, not the body cap, binds");
    let retry = service("a much longer attribution context than the first one carried")
        .revise_project_memory(
            "\"".repeat(admitted),
            Some(format!("probe-{admitted}")),
            false,
            None,
            crate::domain::ProjectMemoryRetiringTargetChange::Keep,
            &crate::domain::ProjectMemoryEdit::Whole,
            at(60) + chrono::Duration::nanoseconds(123_456_789),
        )
        .expect("the exact retry replays");
    assert!(retry.duplicate, "{retry:?}");
}

/// The project-memory refusals, of the request's own input and of what the
/// store holds alike: the memory words register inside their write's own
/// transaction, so every refusal leaves the store as it was. The word's own
/// argument refusals never reach the service and are not among them.
fn memory_refusal_rows<'a>(now: i64) -> Vec<Refusal<'a>> {
    let mut rows: Vec<Refusal<'a>> = remember_rows()
        .into_iter()
        .map(|(what, expected, request)| -> Refusal<'a> {
            (
                what,
                expected,
                Box::new(move |fresh: &LocalWorkService, _: &AgentVerbs| {
                    fresh
                        .revise_project_memory(
                            request.body.clone(),
                            request.key.map(Into::into),
                            request.revise,
                            request.basis,
                            request.target.clone(),
                            &request.edit,
                            at(now),
                        )
                        .err()
                        .map(|error| format!("{error:?}"))
                }),
            )
        })
        .collect();
    rows.extend::<Vec<Refusal<'a>>>(vec![
        (
            "remember word with an external retirement target missing its project",
            "external retirement project must be",
            Box::new(move |_, words| {
                words
                    .remember(
                        RememberInput {
                            text: "A body".into(),
                            key: Some("retired-by".into()),
                            revise: false,
                            expected_revision: None,
                            retires_with: Some("external:#reference".into()),
                            clear_retires_with: false,
                            append: false,
                            section: None,
                        },
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "remember of a live key without revising",
            "ProjectMemoryExists",
            Box::new(move |fresh, _| {
                fresh
                    .revise_project_memory(
                        "A different body".into(),
                        Some("appended".into()),
                        false,
                        None,
                        crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                        &crate::domain::ProjectMemoryEdit::Whole,
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "remember revising a retired key",
            "ProjectMemoryRetired",
            Box::new(move |fresh, _| {
                fresh
                    .revise_project_memory(
                        "Back again".into(),
                        Some("retired".into()),
                        true,
                        None,
                        crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                        &crate::domain::ProjectMemoryEdit::Whole,
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "forget word of a key the store lacks",
            "ProjectMemoryNotFound",
            Box::new(move |_, words| {
                words
                    .forget(
                        ForgetInput {
                            key: "missing-memory".into(),
                        },
                        at(now),
                    )
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
        (
            "forget word with an unsafe key",
            "memory key must be",
            Box::new(move |_, words| {
                words
                    .forget(ForgetInput { key: "BAD".into() }, at(now))
                    .err()
                    .map(|error| format!("{error:?}"))
            }),
        ),
    ]);
    rows
}

#[test]
fn a_refused_first_word_registers_no_process_default_session() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("registration".into());
    let now = 11 * DAY;
    let holder = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("holder".into()),
        Some("registration-test".into()),
    );
    let item = proposed_root(
        holder
            .work_propose(root_input("Item", "item"), at(0))
            .expect("item"),
    )
    .short_ref;
    // Live memories the memory rows name, so their refusals come from the
    // write's own transaction, after the session registered in it: one
    // retired, the others live.
    for (key, body) in [
        ("appended", "A paragraph."),
        (
            "sectioned",
            "<!-- engram-section part -->\nold\n<!-- /engram-section part -->",
        ),
        ("retired", "Gone soon."),
    ] {
        holder
            .revise_project_memory(
                body.into(),
                Some(key.into()),
                false,
                None,
                crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                &crate::domain::ProjectMemoryEdit::Whole,
                at(0),
            )
            .expect("a live memory");
    }
    holder
        .forget_project_memory("retired".into(), at(1))
        .expect("a retired memory");
    // Eligible for reclamation at `now`, and one that becomes eligible only
    // a minute later.
    let stale = idle_session(&database, &project, 10, &item, DAY);
    let recent = idle_session(&database, &project, 11, &item, now - 7 * DAY + 30);
    assert_eq!(
        attempts_of(&database, &stale),
        1,
        "the stale attempt is seeded"
    );

    let record: String = rusqlite::Connection::open(&database)
        .expect("reader")
        .query_row("SELECT object_id FROM objects LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("a stored record");
    let mut refusals = refusal_rows(&item, &record, now);
    refusals.extend(handoff_and_gate_rows(&item, now));
    refusals.extend(evaluation_size_rows(&item, now));
    refusals.extend(memory_refusal_rows(now));

    let connection = rusqlite::Connection::open(&database).expect("snapshot reader");
    let mut before = crate::storage::test_database_shape_snapshot(&connection).expect("snapshot");
    let mut changed = Vec::new();
    for (index, (what, expected, refused)) in refusals.iter().enumerate() {
        // Each refusal gets sessions of its own, so one that registered
        // cannot hide another behind its spent first registration.
        let pid = 100 + 2 * u32::try_from(index).expect("few rows");
        let fresh = process_default(&database, &project, pid, now);
        let words = process_default_words(&database, &project, pid + 1, now);
        let refusal = refused(&fresh, &words).unwrap_or_else(|| panic!("{what}: refused"));
        assert!(refusal.contains(expected), "{what}: {refusal}");
        let after = crate::storage::test_database_shape_snapshot(&connection).expect("snapshot");
        // A registration always writes the session's row, so the snapshot
        // covers the word rows, whose own service is not reachable here;
        // the core rows also check their service never initialized.
        if after != before || fresh.process_default_session_initialized.get().is_some() {
            changed.push(*what);
            before = after;
        }
    }
    assert!(
        changed.is_empty(),
        "refusals that changed the store: {changed:?}"
    );
    assert!(
        session_row(&database, &stale).is_some(),
        "nothing was reclaimed"
    );
    assert_eq!(attempts_of(&database, &stale), 1);

    let fresh = process_default(&database, &project, 20, now);

    // The first valid word registers, at its own time, and reclaims.
    let ready = || WorkNextQuery {
        sections: vec![WorkNextSection::Ready],
        ..WorkNextQuery::default()
    };
    fresh
        .work_next(1, ready(), at(now))
        .expect("first valid word");
    assert_eq!(
        session_row(&database, &fresh.session_id),
        Some(at(now).timestamp_millis())
    );
    assert!(
        session_row(&database, &stale).is_none(),
        "the stale session is reclaimed"
    );
    assert_eq!(attempts_of(&database, &stale), 0);
    assert!(
        session_row(&database, &recent).is_some(),
        "still within retention"
    );

    // A later word does not register again, so it reclaims nothing, even
    // though the recent session has become eligible by then.
    fresh
        .work_next(1, ready(), at(now + 60))
        .expect("second valid word");
    assert!(
        session_row(&database, &recent).is_some(),
        "no second registration"
    );
}

/// The ambient focus a word reads before it registers is one snapshot: a
/// focus change by another connection of the same session between the two
/// reads cannot pair the old focus with a view the new focus refuses.
#[test]
fn the_ambient_focus_and_its_view_come_from_one_snapshot() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("ambient-snapshot".into());
    let session = |database: &std::path::Path| {
        LocalWorkService::new(
            database.to_path_buf(),
            project.clone(),
            "agent".into(),
            SessionId("agent".into()),
            Some("registration-test".into()),
        )
    };
    let reader = session(&database);
    let first = proposed_root(
        reader
            .work_propose(root_input("First", "first"), at(0))
            .expect("first"),
    )
    .short_ref;
    let second = proposed_root(
        reader
            .work_propose(root_input("Second", "second"), at(1))
            .expect("second"),
    )
    .short_ref;
    reader.work_focus(&first, at(2)).expect("focus first");

    let other = session(&database);
    let moved_to = second.clone();
    crate::work_service::focus::between_ambient_focus_reads(move || {
        other
            .work_focus(&moved_to, at(3))
            .expect("a concurrent focus change");
    });
    let view = reader
        .ambient_focus_view(at(3))
        .expect("the ambient focus reads")
        .expect("a focus");
    assert_eq!(view.status.work.short_ref, first);
}
