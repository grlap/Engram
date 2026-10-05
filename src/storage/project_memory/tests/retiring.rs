//! Retiring targets: the explicit clear marker, the disclosure of a target a
//! revision dropped without one, and the target's part in replay equality.

use super::*;
use crate::domain::{
    ProjectMemoryRetiringTarget, ProjectMemoryRetiringTargetChange as Change,
    ProjectMemoryRetiringTargetInput,
};

fn set(target: ProjectMemoryRetiringTargetInput) -> Change {
    Change::Set { target }
}

const PROJECT: &str = "project-memory-retiring";
const SESSION: &str = "retiring-session";

fn external(reference: &str) -> ProjectMemoryRetiringTargetInput {
    ProjectMemoryRetiringTargetInput::External {
        project: "other-tracker".into(),
        reference: reference.into(),
    }
}

fn stored_external(reference: &str) -> ProjectMemoryRetiringTarget {
    ProjectMemoryRetiringTarget::External {
        project: "other-tracker".into(),
        reference: reference.into(),
    }
}

fn request(
    key: &str,
    body: &str,
    at_ms: i64,
    revise: bool,
    expected_revision: Option<u64>,
    retiring_target: Change,
) -> RememberProjectMemoryRequest {
    let mut request = project_memory_request(PROJECT, SESSION, Some(key), body, at_ms);
    request.revise = revise;
    request.expected_revision = expected_revision;
    request.retiring_target = retiring_target;
    request
}

fn remember(
    store: &mut SqliteStore,
    request: &RememberProjectMemoryRequest,
) -> Result<ProjectMemoryMutationReceipt, StoreError> {
    store.remember_project_memory_with_admission(
        request,
        &DevelopmentNoopRedactor,
        admit_project_memory_full,
    )
}

fn full(store: &SqliteStore, key: &str, revision: Option<u64>) -> ProjectMemoryFull {
    store
        .project_memory_full(
            &ProjectId(PROJECT.into()),
            &SessionId(SESSION.into()),
            &actor(SESSION),
            key,
            revision,
        )
        .expect("full read")
}

fn listed(store: &SqliteStore, key: &str) -> ProjectMemoryListRow {
    store
        .project_memories(
            &ProjectId(PROJECT.into()),
            &SessionId(SESSION.into()),
            &actor(SESSION),
            None,
            None,
        )
        .expect("list")
        .memories
        .into_iter()
        .find(|row| row.key == key)
        .expect("listed key")
}

fn history(store: &SqliteStore, key: &str) -> Vec<StoredProjectMemory> {
    lookup_project_memory_history_on(&store.connection, &ProjectId(PROJECT.into()), key)
        .expect("history")
}

/// Appends a revision the way a build without retiring targets writes one:
/// the body changes, and neither the target nor a clear is carried.
fn append_revision_without_target(store: &mut SqliteStore, key: &str, body: &str, at_ms: i64) {
    insert_raw_version(store, PROJECT, key, body, None, at_ms);
}

/// Creates one work item in `project`.
fn create_item(store: &mut SqliteStore, project: &str, title: &str) -> crate::WorkItem {
    store
        .create_work(
            &CreateWorkRequest {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                project_id: ProjectId(project.into()),
                parent_id: None,
                child_requirement: ChildRequirement::Required,
                title: title.into(),
                outcome: "the workaround can go".into(),
                acceptance: vec!["the fix lands".into()],
                kind: WorkItemKind::Task,
                priority: 1,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                origin: WorkOrigin::Local,
                source_snapshot_id: None,
                actor: actor(SESSION),
                idempotency_key: format!("{project}-{title}"),
                created_at: Utc.timestamp_millis_opt(1).unwrap(),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("work item")
}

/// Writes one version of `key` in `project` straight to the store, as an
/// import file or an older build could, with whatever target it is given and
/// no resolution or admission.
pub(in crate::storage::project_memory) fn insert_raw_version(
    store: &mut SqliteStore,
    project: &str,
    key: &str,
    body: &str,
    target: Option<ProjectMemoryRetiringTarget>,
    at_ms: i64,
) {
    let mut request = project_memory_request(project, SESSION, Some(key), body, at_ms);
    let previous = lookup_project_memory_history_on(&store.connection, &request.project_id, key)
        .expect("history")
        .pop();
    request.revise = previous.is_some();
    let prepared = prepare_project_memory(&request, key, previous.as_ref(), target, false)
        .expect("prepare raw version");
    let transaction = store.connection.transaction().expect("raw version tx");
    project_memory_state_on(&transaction, &request.project_id).expect("state");
    SqliteStore::insert_project_memory_version_object(
        &transaction,
        &prepared.version_object,
        &request.project_id,
        key,
    )
    .expect("insert version");
    SqliteStore::insert_object(
        &transaction,
        "memory_assertion_event",
        &prepared.assertion_object,
    )
    .expect("insert assertion");
    SqliteStore::apply_memory_projection(
        &transaction,
        prepared.version_object.key(),
        prepared.assertion_object.key(),
        &prepared.version,
        &prepared.assertion,
        MemoryProjectionMode::Live,
    )
    .expect("project raw version");
    advance_project_memory_state_on(
        &transaction,
        &request.project_id,
        i64::from(previous.is_none()),
    )
    .expect("advance");
    transaction.commit().expect("commit raw version");
}

#[test]
fn an_explicit_clear_is_recorded_and_needs_a_revise_and_a_current_target() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("store.db")).unwrap();
    let key = "cleared-note";

    let refused = remember(
        &mut store,
        &request(key, "first", 1, false, None, Change::Clear),
    )
    .expect_err("a clear without --revise");
    assert!(
        refused.to_string().contains("requires --revise"),
        "{refused}"
    );

    remember(
        &mut store,
        &request(key, "first", 1, false, None, set(external("issue-1"))),
    )
    .expect("targeted memory");
    let refused = remember(
        &mut store,
        &request(key, "second", 2, false, None, Change::Clear),
    )
    .expect_err("a clear of an existing key without --revise");
    assert!(
        refused.to_string().contains("requires --revise"),
        "{refused}"
    );

    remember(
        &mut store,
        &request(key, "second", 2, true, None, Change::Keep),
    )
    .expect("body revise");
    assert_eq!(
        full(&store, key, None).retiring_target,
        Some(stored_external("issue-1")),
        "an ordinary revise keeps the current target"
    );

    let clear = request(key, "third", 3, true, None, Change::Clear);
    let cleared = remember(&mut store, &clear).expect("explicit clear");
    assert_eq!(cleared.revision, 3);
    let versions = history(&store, key);
    assert!(versions[2].version.retiring_target_cleared);
    assert!(versions[2].version.retiring_target.is_none());
    assert!(!versions[1].version.retiring_target_cleared);
    let current = full(&store, key, None);
    assert!(current.retiring_target.is_none());
    assert!(current.retiring_target_dropped.is_none());
    assert!(listed(&store, key).retiring_target_dropped.is_none());

    let replay = remember(&mut store, &clear).expect("exact replay of the clear");
    assert!(replay.duplicate);
    assert_eq!(replay.revision, 3);

    let refused = remember(
        &mut store,
        &request(key, "fourth", 4, true, None, Change::Clear),
    )
    .expect_err("nothing left to clear");
    assert!(
        refused
            .to_string()
            .contains("no retirement target to clear"),
        "{refused}"
    );
    assert_eq!(history(&store, key).len(), 3);
}

#[test]
fn a_target_dropped_without_a_clear_is_disclosed_until_restored() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("store.db")).unwrap();
    let key = "dropped-note";
    remember(
        &mut store,
        &request(key, "first", 1, false, None, set(external("issue-7"))),
    )
    .expect("targeted memory");
    append_revision_without_target(&mut store, key, "second, by an older build", 2);
    append_revision_without_target(&mut store, key, "third, by an older build", 3);

    let expected = Some(crate::domain::ProjectMemoryRetiringTargetDropped {
        revision: 2,
        target: stored_external("issue-7"),
    });
    let current = full(&store, key, None);
    assert!(current.retiring_target.is_none());
    assert_eq!(current.retiring_target_dropped, expected);
    assert_eq!(listed(&store, key).retiring_target_dropped, expected);
    assert_eq!(full(&store, key, Some(2)).retiring_target_dropped, expected);
    assert!(full(&store, key, Some(1)).retiring_target_dropped.is_none());

    let response = crate::work_service::project_memory_full_response(
        current,
        crate::argument_names::ArgumentNames::Cli,
    )
    .unwrap();
    assert!(
        response.reminders.iter().any(|reminder| {
            reminder.contains("revision 2 dropped the retirement target without a clear")
                && reminder.contains("--retires-with external:other-tracker#issue-7")
        }),
        "{:?}",
        response.reminders
    );
    assert!(
        response
            .terminal_lines()
            .iter()
            .any(|line| line.contains("dropped by revision 2 without a clear")),
        "{:?}",
        response.terminal_lines()
    );

    remember(
        &mut store,
        &request(key, "restored", 4, true, None, set(external("issue-7"))),
    )
    .expect("restore the target");
    let restored = full(&store, key, None);
    assert_eq!(restored.retiring_target, Some(stored_external("issue-7")));
    assert!(restored.retiring_target_dropped.is_none());
    assert_eq!(
        full(&store, key, Some(3)).retiring_target_dropped,
        expected,
        "history keeps its own disclosure"
    );
}

/// A clear may acknowledge a target that revisions dropped: the drop is then
/// no longer disclosed, the clear is recorded, and a second clear has
/// nothing left to remove.
#[test]
fn a_clear_acknowledges_a_dropped_target() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("store.db")).unwrap();
    let key = "acknowledged-note";
    remember(
        &mut store,
        &request(key, "first", 1, false, None, set(external("issue-7"))),
    )
    .expect("targeted memory");
    append_revision_without_target(&mut store, key, "second, by an older build", 2);
    append_revision_without_target(&mut store, key, "third, by an older build", 3);

    let acknowledge = request(
        key,
        "fourth, the drop was intended",
        4,
        true,
        None,
        Change::Clear,
    );
    assert_eq!(remember(&mut store, &acknowledge).unwrap().revision, 4);
    assert!(remember(&mut store, &acknowledge).unwrap().duplicate);
    let versions = history(&store, key);
    assert!(versions[3].version.retiring_target_cleared);
    let current = full(&store, key, None);
    assert!(current.retiring_target.is_none());
    assert!(current.retiring_target_dropped.is_none());
    assert!(listed(&store, key).retiring_target_dropped.is_none());
    assert_eq!(
        full(&store, key, Some(3))
            .retiring_target_dropped
            .map(|dropped| dropped.revision),
        Some(2),
        "history keeps its own disclosure"
    );

    let refused = remember(
        &mut store,
        &request(key, "fifth", 5, true, None, Change::Clear),
    )
    .expect_err("nothing left to clear");
    assert!(
        refused
            .to_string()
            .contains("no retirement target to clear")
    );

    let never = "never-targeted";
    remember(
        &mut store,
        &request(never, "plain", 1, false, None, Change::Keep),
    )
    .unwrap();
    let refused = remember(
        &mut store,
        &request(never, "still plain", 2, true, None, Change::Clear),
    )
    .expect_err("a memory that never had a target has nothing to clear");
    assert!(
        refused
            .to_string()
            .contains("no retirement target to clear")
    );
}

#[test]
fn the_retiring_target_takes_part_in_replay_equality() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("store.db")).unwrap();
    let key = "replayed-note";
    remember(
        &mut store,
        &request(key, "first", 1, false, None, set(external("issue-1"))),
    )
    .expect("targeted memory");
    let retarget = request(key, "second", 2, true, Some(1), set(external("issue-2")));
    assert_eq!(remember(&mut store, &retarget).unwrap().revision, 2);
    assert!(remember(&mut store, &retarget).unwrap().duplicate);

    let other_target = request(key, "second", 2, true, Some(1), set(external("issue-3")));
    assert!(matches!(
        remember(&mut store, &other_target),
        Err(StoreError::ProjectMemoryRevisionConflict {
            expected: 1,
            current: 2,
            ..
        })
    ));
    let inherit = request(key, "second", 2, true, Some(1), Change::Keep);
    assert!(
        matches!(
            remember(&mut store, &inherit),
            Err(StoreError::ProjectMemoryRevisionConflict { .. })
        ),
        "inheriting from revision 1 is not the retarget recorded as revision 2"
    );
    assert_eq!(history(&store, key).len(), 2);
}

/// A local target's item state is read when the memory is read, so admission
/// judges every envelope with that state in its largest form: a completed
/// item, whose read adds the forget-candidate reminder and command.
#[test]
fn admission_judges_a_local_target_in_its_completed_read_form() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("store.db")).unwrap();
    let work = create_item(&mut store, PROJECT, "Retiring fix");
    let seen = std::cell::RefCell::new(Vec::new());
    let key = "admitted-note";
    let local = ProjectMemoryRetiringTargetInput::Local {
        work_ref: work.short_ref.clone(),
    };
    for (body, revise) in [("first", false), ("second", true)] {
        store
            .remember_project_memory_with_admission(
                &request(key, body, 2, revise, None, set(local.clone())),
                &DevelopmentNoopRedactor,
                |full: &ProjectMemoryFull, admission| {
                    seen.borrow_mut().push(full.clone());
                    admit_project_memory_full(full, admission)
                },
            )
            .expect("remember");
    }
    let seen = seen.into_inner();
    assert_eq!(seen.len(), 3, "the first version, then both on the revise");
    for full in &seen {
        let state = full
            .retiring_state
            .as_ref()
            .expect("read-time state reserved");
        assert_eq!(state.lifecycle, crate::domain::WorkLifecycle::Completed);
        assert_eq!(
            state
                .updated_at
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
            "9999-12-31T23:59:59.999999999Z"
        );
    }
    let stored = full(&store, key, None);
    assert_eq!(
        stored.retiring_state.map(|state| state.lifecycle),
        Some(crate::domain::WorkLifecycle::Open),
        "the stored read shows the item's real state"
    );
}

#[test]
fn a_clear_marker_must_follow_a_targeted_version() {
    let request = request("marker-shape", "body", 1, false, None, Change::Keep);
    let prepared =
        prepare_project_memory(&request, "marker-shape", None, None, true).expect("prepare");
    assert!(
        validate_keyed_project_memory_shape(&prepared.version, &prepared.assertion).is_err(),
        "a first version cannot carry a clear"
    );
    let mut targeted = prepared.version.clone();
    targeted.retiring_target = Some(stored_external("issue-1"));
    targeted.parents = vec![prepared.version_object.key().clone()];
    assert!(
        validate_keyed_project_memory_shape(&targeted, &prepared.assertion).is_err(),
        "a clear carries no target"
    );
    for (target, cleared) in [(Some(stored_external("issue-1")), false), (None, true)] {
        let mut unkeyed = prepared.version.clone();
        unkeyed.project_key = None;
        unkeyed.parents = Vec::new();
        unkeyed.retiring_target = target;
        unkeyed.retiring_target_cleared = cleared;
        assert!(
            validate_keyed_project_memory_shape(&unkeyed, &prepared.assertion).is_err(),
            "only keyed project memories carry a target or a clear"
        );
    }
}

/// Target text is stored and shown like the body, so the redactor inspects
/// it before the target is resolved or anything is written.
#[test]
fn the_redactor_inspects_retiring_target_text() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("store.db")).unwrap();
    let redactor = MatchingRedactor("secret-shaped-text");
    for target in [
        ProjectMemoryRetiringTargetInput::External {
            project: "other-tracker".into(),
            reference: "secret-shaped-text".into(),
        },
        ProjectMemoryRetiringTargetInput::External {
            project: "secret-shaped-text".into(),
            reference: "issue-1".into(),
        },
        ProjectMemoryRetiringTargetInput::Local {
            work_ref: "secret-shaped-text".into(),
        },
    ] {
        let refused = store
            .remember_project_memory_with_admission(
                &request("redacted-note", "plain body", 1, false, None, set(target)),
                &redactor,
                admit_project_memory_full,
            )
            .expect_err("target text is refused by the redactor");
        assert!(
            matches!(refused, StoreError::RedactionRefused(_)),
            "{refused}"
        );
    }
    assert!(history(&store, "redacted-note").is_empty());
}

/// Doctor checks every stored local target against the items of its memory's
/// own project, so a bad row from an import file is reported rather than
/// failing later reads.
#[test]
fn doctor_reports_a_local_target_that_names_no_item_of_its_project() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("store.db")).unwrap();
    let item = create_item(&mut store, PROJECT, "Retiring fix");
    let foreign = create_item(&mut store, "another-project", "Foreign fix");
    let local = |work_id, work_ref: &str| {
        Some(ProjectMemoryRetiringTarget::Local {
            work_id,
            work_ref: work_ref.into(),
        })
    };
    insert_raw_version(
        &mut store,
        PROJECT,
        "valid-target",
        "body",
        local(item.work_id, &item.short_ref),
        2,
    );
    assert!(
        store
            .verify_all()
            .unwrap()
            .invalid_objects
            .iter()
            .all(|entry| !entry.ends_with(":retiring_target")),
        "a target naming an item of its project passes"
    );
    let missing = crate::domain::WorkId(uuid::Uuid::from_u128(1));
    for (key, target) in [
        ("missing-item", local(missing, "w-000000000001")),
        ("wrong-short-ref", local(item.work_id, "w-ffffffffffff")),
        (
            "other-project-item",
            local(foreign.work_id, &foreign.short_ref),
        ),
    ] {
        insert_raw_version(&mut store, PROJECT, key, "body", target, 3);
    }
    let flagged = store
        .verify_all()
        .unwrap()
        .invalid_objects
        .into_iter()
        .filter(|entry| entry.ends_with(":retiring_target"))
        .count();
    assert_eq!(flagged, 3);
}

/// The candidate query reads only current active heads of the project: a
/// retargeted, cleared or forgotten memory and another project's memory are
/// never candidates, and more than the listed bound keeps its exact total.
#[test]
fn candidates_are_the_current_active_heads_of_this_project_only() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("store.db")).unwrap();
    let item = create_item(&mut store, PROJECT, "Retiring fix");
    let other = create_item(&mut store, PROJECT, "Other fix");
    let local = |work: &crate::WorkItem| {
        set(ProjectMemoryRetiringTargetInput::Local {
            work_ref: work.short_ref.clone(),
        })
    };
    for index in 0..18 {
        remember(
            &mut store,
            &request(
                &format!("current-{index:02}"),
                "body",
                2,
                false,
                None,
                local(&item),
            ),
        )
        .unwrap();
    }
    for (key, later) in [("retargeted", local(&other)), ("cleared", Change::Clear)] {
        remember(
            &mut store,
            &request(key, "first", 3, false, None, local(&item)),
        )
        .unwrap();
        remember(&mut store, &request(key, "second", 4, true, None, later)).unwrap();
    }
    remember(
        &mut store,
        &request("forgotten", "first", 3, false, None, local(&item)),
    )
    .unwrap();
    store
        .forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: ProjectId(PROJECT.into()),
                session_id: SessionId(SESSION.into()),
                key: "forgotten".into(),
                actor: actor(SESSION),
                created_at: Utc.timestamp_millis_opt(5).unwrap(),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    insert_raw_version(
        &mut store,
        "another-project",
        "foreign",
        "body",
        Some(ProjectMemoryRetiringTarget::Local {
            work_id: item.work_id,
            work_ref: item.short_ref.clone(),
        }),
        3,
    );
    let candidates = store
        .project_memory_retirement_candidates(
            &ProjectId(PROJECT.into()),
            &SessionId(SESSION.into()),
            &actor(SESSION),
            item.work_id,
        )
        .unwrap();
    assert_eq!(candidates.total, 18);
    assert_eq!(candidates.omitted, 2);
    assert_eq!(
        candidates.keys,
        (0..16)
            .map(|index| format!("current-{index:02}"))
            .collect::<Vec<_>>()
    );
}

/// A full-store import runs the doctor, so a memory whose local target names
/// no item of its project refuses the import and publishes nothing, while a
/// store whose targets all resolve imports and reads as before.
#[test]
fn a_full_store_import_refuses_a_local_target_that_names_no_item() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    {
        let mut store = SqliteStore::open(&source).unwrap();
        let item = create_item(&mut store, PROJECT, "Retiring fix");
        remember(
            &mut store,
            &request(
                "valid-note",
                "body",
                2,
                false,
                None,
                set(ProjectMemoryRetiringTargetInput::Local {
                    work_ref: item.short_ref.clone(),
                }),
            ),
        )
        .unwrap();
    }
    let good_file = directory.path().join("good.jsonl");
    crate::storage::migration::export_json(&source, &good_file).expect("export");
    let good_target = directory.path().join("good.db");
    crate::storage::migration::import_json(&good_file, &good_target).expect("valid targets import");

    {
        let mut store = SqliteStore::open(&source).unwrap();
        insert_raw_version(
            &mut store,
            PROJECT,
            "dangling-note",
            "body",
            Some(ProjectMemoryRetiringTarget::Local {
                work_id: crate::domain::WorkId(uuid::Uuid::from_u128(1)),
                work_ref: "w-000000000001".into(),
            }),
            3,
        );
    }
    let bad_file = directory.path().join("bad.jsonl");
    crate::storage::migration::export_json(&source, &bad_file).expect("export");
    let bad_target = directory.path().join("bad.db");
    let error = crate::storage::migration::import_json(&bad_file, &bad_target)
        .expect_err("a dangling local target refuses the import");
    assert!(error.to_string().contains("retiring_target"), "{error}");
    assert!(!bad_target.exists(), "nothing is published");
}

/// External target text is echoed back inside suggested commands, so it is
/// held to characters no common shell splits or interprets: whitespace,
/// quotes and shell syntax are refused on write, and a stored row holding them
/// fails the shape check that doctor and snapshot load apply.
#[test]
fn external_target_text_is_limited_to_shell_safe_characters() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("store.db")).unwrap();
    for (project, reference) in [
        ("Other Tracker", "bug-12"),
        ("other-tracker", "bug 12"),
        ("other-tracker", "12; rm -rf x"),
        ("other-tracker", "$(id)"),
        ("tracker'quoted", "12"),
        ("other-tracker", "a|b"),
        ("other-tracker", "a`b`"),
    ] {
        let refused = remember(
            &mut store,
            &request(
                "shell-note",
                "body",
                1,
                false,
                None,
                set(ProjectMemoryRetiringTargetInput::External {
                    project: project.into(),
                    reference: reference.into(),
                }),
            ),
        )
        .expect_err("shell syntax in external target text is refused");
        assert!(
            refused.to_string().contains(". _ - / : @ +"),
            "{project}#{reference}: {refused}"
        );
    }
    assert!(history(&store, "shell-note").is_empty());
    remember(
        &mut store,
        &request(
            "shell-note",
            "body",
            1,
            false,
            None,
            set(ProjectMemoryRetiringTargetInput::External {
                project: "github.com/owner/repo".into(),
                reference: "PROJ-123:user@host+v2".into(),
            }),
        ),
    )
    .expect("the full safe set is admitted");
    assert!(
        validate_retiring_target_shape(&ProjectMemoryRetiringTarget::External {
            project: "other tracker".into(),
            reference: "12".into(),
        })
        .is_err(),
        "a stored row with whitespace fails the shape check"
    );
}
