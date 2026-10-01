//! A host's read of what satisfied each bound criterion of an item on its
//! active run: the obligation completion would select, its recorded
//! resolution and the original verification with its producer, paged at a
//! pinned run-feed cut, with typed refusals and no writes.

use std::fmt::Write as _;

use super::*;
use crate::domain::{
    AcceptanceBinding, AcceptanceBindingPage, AcceptanceBindingReadRefusal as Refusal,
    AcceptanceBindingResolutionKind as Kind, AcceptanceBindingRow, VerificationRequirement,
    WaiveWorkObligationRequest, WorkObligationState,
};
use crate::storage::BindingReadRequest;

fn read_at(
    store: &SqliteStore,
    host: &HostSession,
    work: &WorkItem,
    run_id: WorkRunId,
    after: Option<&str>,
) -> Result<AcceptanceBindingPage, StoreError> {
    store.read_acceptance_bindings(
        &host.project_id,
        &host.session_id,
        &host.connection_token,
        &host.routing_token,
        &BindingReadRequest {
            work_id: work.work_id,
            expected_work_revision: work.revision,
            run_id,
            after,
        },
    )
}

fn read(
    store: &SqliteStore,
    host: &HostSession,
    work: &WorkItem,
    after: Option<&str>,
) -> Result<AcceptanceBindingPage, StoreError> {
    read_at(
        store,
        host,
        work,
        work.active_run_id.expect("active run"),
        after,
    )
}

fn page(store: &SqliteStore, host: &HostSession, work: &WorkItem) -> AcceptanceBindingPage {
    read(store, host, work, None).expect("binding read")
}

fn refused(result: Result<AcceptanceBindingPage, StoreError>) -> Refusal {
    match result {
        Err(StoreError::AcceptanceBindingReadRefused { refusal, .. }) => refusal,
        other => panic!("expected a binding read refusal, got {other:?}"),
    }
}

fn bound(criterion: usize, kind: VerificationKind) -> AcceptanceBinding {
    AcceptanceBinding {
        criterion,
        requirement: VerificationRequirement {
            check_kind: kind,
            check_fingerprint: None,
        },
    }
}

fn criteria(count: usize) -> Vec<String> {
    (1..=count)
        .map(|n| format!("criterion {n} holds"))
        .collect()
}

fn bind(
    fixture: &mut Fixture,
    work: &WorkItem,
    acceptance: Vec<String>,
    bindings: Vec<AcceptanceBinding>,
    key: &str,
    second: i64,
) -> WorkItem {
    let claim = fixture.claim.clone();
    revise(
        &mut fixture.store,
        work,
        &claim,
        WorkRevisionPatch {
            acceptance: Some(acceptance),
            acceptance_bindings: Some(bindings),
            ..empty_patch()
        },
        key,
        second,
    )
    .expect("revise acceptance")
}

fn check(
    fixture: &mut Fixture,
    work: &WorkItem,
    key: &str,
    result: VerificationResult,
    second: i64,
) -> ObjectId {
    let claim = fixture.claim.clone();
    host_verification(
        &mut fixture.store,
        work,
        &claim,
        "runner",
        key,
        VerificationKind::Test,
        result,
        second,
    )
}

fn row(page: &AcceptanceBindingPage, criterion: usize) -> &AcceptanceBindingRow {
    page.rows
        .iter()
        .find(|row| row.criterion == criterion)
        .expect("criterion row")
}

/// Every row of every table, so a read can be shown to write nothing.
fn database_rows(store: &SqliteStore) -> Vec<String> {
    let tables: Vec<String> = store
        .connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .expect("list tables")
        .query_map([], |row| row.get(0))
        .expect("query tables")
        .collect::<Result<_, _>>()
        .expect("table names");
    let mut all = Vec::new();
    for table in tables {
        let mut statement = store
            .connection
            .prepare(&format!("SELECT * FROM \"{table}\""))
            .expect("select table");
        let columns = statement.column_count();
        let mut rows: Vec<String> = statement
            .query_map([], |row| {
                let mut line = table.clone();
                for index in 0..columns {
                    let value: rusqlite::types::Value = row.get(index)?;
                    write!(line, "|{value:?}").expect("write row");
                }
                Ok(line)
            })
            .expect("query rows")
            .collect::<Result<_, _>>()
            .expect("rows");
        rows.sort();
        all.extend(rows);
    }
    all
}

/// A continuation token carrying `cursor`, as the read encodes one.
fn forged(cursor: &serde_json::Value) -> String {
    let mut token = String::from("abr1-");
    for byte in serde_json::to_vec(cursor).expect("encode cursor") {
        write!(token, "{byte:02x}").expect("write cursor");
    }
    token
}

// A satisfied, an open, a waived and an unbound criterion, each read with
// the facts the record holds: the satisfying verification by its full id,
// kind, fingerprint, result and source, and its producer's outcome, all at
// positions on the run's feed no later than the cut.
#[test]
fn each_criterion_reads_the_obligation_and_evidence_that_answer_for_it() {
    let mut fixture = fixture("project-binding-read");
    let claim = fixture.claim.clone();
    let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
    let work = fixture.work.clone();
    let work = bind(
        &mut fixture,
        &work,
        criteria(4),
        vec![
            bound(1, VerificationKind::Test),
            bound(3, VerificationKind::Lint),
            bound(4, VerificationKind::Build),
        ],
        "bind-read",
        10,
    );
    let passed = check(
        &mut fixture,
        &work,
        "unit-tests",
        VerificationResult::Passed,
        11,
    );

    let first = page(&fixture.store, &host, &work);
    let lint = row(&first, 3)
        .binding
        .as_ref()
        .and_then(|binding| binding.obligation.as_ref())
        .expect("lint obligation")
        .clone();
    fixture
        .store
        .waive_work_obligation(
            &WaiveWorkObligationRequest {
                obligation_id: lint.obligation_id,
                expected_definition: lint.definition.clone(),
                waived_by: "operator".into(),
                reason: "lint runs elsewhere".into(),
                actor: actor("operator"),
                idempotency_key: "waive-lint".into(),
                waived_at: at(12),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("waive the lint obligation");

    let read = page(&fixture.store, &host, &work);
    let head = cut(&fixture.store, &work);
    assert_eq!(read.basis.project_id, work.project_id);
    assert_eq!(read.basis.work_id, work.work_id);
    assert_eq!(read.basis.work_revision, work.revision);
    assert_eq!(Some(read.basis.run_id), work.active_run_id);
    assert_eq!(read.basis.run_cut, head);
    assert_eq!(
        (read.total, read.earlier, read.shown, read.omitted),
        (4, 0, 4, 0)
    );
    assert_eq!(read.continuation, None);
    let order: Vec<usize> = read.rows.iter().map(|row| row.criterion).collect();
    assert_eq!(order, vec![1, 2, 3, 4]);

    let satisfied = row(&read, 1).binding.as_ref().expect("bound");
    assert_eq!(satisfied.requirement.check_kind, VerificationKind::Test);
    let obligation = satisfied.obligation.as_ref().expect("obligation");
    assert_eq!(obligation.state, WorkObligationState::Satisfied);
    assert_eq!(obligation.work_revision, work.revision);
    assert!(obligation.trigger_position <= obligation.definition_position);
    let resolution = obligation.resolution.as_ref().expect("resolution");
    assert_eq!(resolution.kind, Kind::Satisfied);
    assert!(resolution.position <= head);
    let satisfaction = resolution.satisfaction.as_ref().expect("satisfaction");
    let verification = &satisfaction.verification;
    assert_eq!(verification.record, passed);
    let stored: VerificationEvidence = crate::storage::work::load_typed_work_object(
        &fixture.store.connection,
        &passed,
        "verification_evidence",
    )
    .expect("stored verification");
    assert_eq!(verification.check_kind, stored.check_kind);
    assert_eq!(verification.check_fingerprint, stored.check_fingerprint);
    assert_eq!(verification.result, VerificationResult::Passed);
    assert_eq!(verification.source_basis, stored.source_basis);
    assert_eq!(verification.producer.record, stored.producer_observation);
    assert_eq!(verification.producer.outcome, ExecutionOutcome::Succeeded);
    assert!(verification.producer.position < verification.position);
    assert!(verification.position <= resolution.position);
    assert!(satisfaction.evaluated_cut <= head);

    assert_eq!(row(&read, 2).binding, None);

    let waived = row(&read, 3).binding.as_ref().expect("bound");
    let waived = waived.obligation.as_ref().expect("obligation");
    assert_eq!(waived.obligation_id, lint.obligation_id);
    assert_eq!(waived.state, WorkObligationState::Waived);
    let lint_resolution = waived.resolution.as_ref().expect("waiver");
    assert_eq!(lint_resolution.kind, Kind::Waived);
    assert_eq!(lint_resolution.satisfaction, None);

    let open = row(&read, 4).binding.as_ref().expect("bound");
    let open = open.obligation.as_ref().expect("obligation");
    assert_eq!(open.state, WorkObligationState::Open);
    assert_eq!(open.resolution, None);
    assert_eq!(open.rule, crate::control::acceptance_binding_rule(4));
}

/// Drops the obligation projection rows of `obligations` on the run, keeping
/// their canonical records and feed entries: a damaged projection.
fn drop_obligation_rows(store: &SqliteStore, obligations: &[crate::domain::WorkObligationId]) {
    for obligation in obligations {
        store
            .connection
            .execute(
                "DELETE FROM work_run_obligations WHERE obligation_id = ?1",
                [obligation.0.to_string()],
            )
            .expect("drop an obligation row");
    }
}

fn damaged(result: &Result<AcceptanceBindingPage, StoreError>) {
    assert!(
        matches!(result, Err(StoreError::InvalidWorkProjection(_))),
        "{result:?}"
    );
}

// A bound criterion's obligation row lost from the projection is a damaged
// store, never a criterion with no obligation; and a lost newest row never
// lets the older, satisfied obligation answer for a rewritten criterion.
#[test]
fn a_lost_obligation_row_is_a_damaged_store() {
    let mut fixture = fixture("project-binding-read-lost-row");
    let claim = fixture.claim.clone();
    let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
    let work = fixture.work.clone();
    let work = bind(
        &mut fixture,
        &work,
        criteria(1),
        vec![bound(1, VerificationKind::Test)],
        "bind-lost-row",
        10,
    );
    check(
        &mut fixture,
        &work,
        "old-proof",
        VerificationResult::Passed,
        11,
    );
    let satisfied = page(&fixture.store, &host, &work);
    let old = row(&satisfied, 1)
        .binding
        .as_ref()
        .and_then(|binding| binding.obligation.as_ref())
        .expect("satisfied obligation")
        .clone();
    assert_eq!(old.state, WorkObligationState::Satisfied);
    let rewritten = bind(
        &mut fixture,
        &work,
        vec!["criterion 1 holds, reworded".into()],
        vec![bound(1, VerificationKind::Test)],
        "reword-lost-row",
        12,
    );
    let renewed = page(&fixture.store, &host, &rewritten);
    let new = row(&renewed, 1)
        .binding
        .as_ref()
        .and_then(|binding| binding.obligation.as_ref())
        .expect("new obligation")
        .clone();
    assert_ne!(new.obligation_id, old.obligation_id);
    assert_eq!(new.state, WorkObligationState::Open);

    drop_obligation_rows(&fixture.store, &[new.obligation_id]);
    damaged(&read(&fixture.store, &host, &rewritten, None));
    drop_obligation_rows(&fixture.store, &[old.obligation_id]);
    damaged(&read(&fixture.store, &host, &rewritten, None));
}

// The obligation completion would select answers for the binding: a revision
// that changes nothing about the criterion keeps the original pass; a
// rewritten sentence, a pinned check, a dropped and re-added binding and a
// binding moved to another criterion each read their own new obligation.
#[test]
fn a_revision_selects_the_obligation_completion_would() {
    let mut fixture = fixture("project-binding-read-revisions");
    let claim = fixture.claim.clone();
    let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
    let work = fixture.work.clone();
    let work = bind(
        &mut fixture,
        &work,
        criteria(2),
        vec![bound(1, VerificationKind::Test)],
        "bind-original",
        10,
    );
    let original_revision = work.revision;
    let passed = check(
        &mut fixture,
        &work,
        "original",
        VerificationResult::Passed,
        11,
    );
    let titled = revise(
        &mut fixture.store,
        &work,
        &claim,
        WorkRevisionPatch {
            title: Some("A new title".into()),
            ..empty_patch()
        },
        "retitle",
        12,
    )
    .expect("retitle");
    assert!(titled.revision > original_revision);
    let read = page(&fixture.store, &host, &titled);
    let kept = row(&read, 1).binding.as_ref().expect("bound");
    let kept = kept.obligation.as_ref().expect("obligation");
    assert_eq!(kept.work_revision, original_revision);
    assert_eq!(kept.state, WorkObligationState::Satisfied);
    let satisfied_by = kept
        .resolution
        .as_ref()
        .and_then(|resolution| resolution.satisfaction.as_ref())
        .map(|satisfaction| satisfaction.verification.record.clone());
    assert_eq!(satisfied_by, Some(passed));

    let rewritten = bind(
        &mut fixture,
        &titled,
        vec![
            "criterion 1 holds, reworded".into(),
            "criterion 2 holds".into(),
        ],
        vec![bound(1, VerificationKind::Test)],
        "reword",
        13,
    );
    let read = page(&fixture.store, &host, &rewritten);
    let renewed = row(&read, 1).binding.as_ref().expect("bound");
    let renewed = renewed.obligation.as_ref().expect("obligation");
    assert_ne!(renewed.obligation_id, kept.obligation_id);
    assert_eq!(renewed.work_revision, rewritten.revision);
    assert_eq!(renewed.state, WorkObligationState::Open);

    let pinned_requirement = VerificationRequirement {
        check_kind: VerificationKind::Test,
        check_fingerprint: Some(check_fingerprint("the-pinned-check")),
    };
    let pinned = bind(
        &mut fixture,
        &rewritten,
        rewritten.acceptance.clone(),
        vec![AcceptanceBinding {
            criterion: 1,
            requirement: pinned_requirement.clone(),
        }],
        "pin",
        14,
    );
    let read = page(&fixture.store, &host, &pinned);
    let pin = row(&read, 1).binding.as_ref().expect("bound");
    assert_eq!(pin.requirement, pinned_requirement);
    let pin = pin.obligation.as_ref().expect("obligation");
    assert_ne!(pin.obligation_id, renewed.obligation_id);
    assert_eq!(pin.state, WorkObligationState::Open);

    let dropped = bind(
        &mut fixture,
        &pinned,
        pinned.acceptance.clone(),
        Vec::new(),
        "drop",
        15,
    );
    assert_eq!(row(&page(&fixture.store, &host, &dropped), 1).binding, None);
    let readded = bind(
        &mut fixture,
        &dropped,
        dropped.acceptance.clone(),
        vec![AcceptanceBinding {
            criterion: 1,
            requirement: pinned_requirement.clone(),
        }],
        "readd",
        16,
    );
    let read = page(&fixture.store, &host, &readded);
    let again = row(&read, 1).binding.as_ref().expect("bound");
    let again = again.obligation.as_ref().expect("obligation");
    assert_ne!(again.obligation_id, pin.obligation_id);
    assert_eq!(again.state, WorkObligationState::Open);
    assert_eq!(again.work_revision, readded.revision);

    let moved = bind(
        &mut fixture,
        &readded,
        readded.acceptance.clone(),
        vec![bound(2, VerificationKind::Test)],
        "move-binding",
        17,
    );
    let read = page(&fixture.store, &host, &moved);
    assert_eq!(row(&read, 1).binding, None);
    let second = row(&read, 2).binding.as_ref().expect("bound");
    let second = second.obligation.as_ref().expect("obligation");
    assert_eq!(second.rule, crate::control::acceptance_binding_rule(2));
    assert_eq!(second.work_revision, moved.revision);
}

// The read names the record that closed the obligation even after a newer
// check failed and another passed on other source: it reports what was
// recorded, not whether it is still fresh.
#[test]
fn the_original_closure_is_read_after_newer_checks() {
    let mut fixture = fixture("project-binding-read-original");
    let claim = fixture.claim.clone();
    let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
    let work = fixture.work.clone();
    let work = bind(
        &mut fixture,
        &work,
        criteria(1),
        vec![bound(1, VerificationKind::Test)],
        "bind-original-closure",
        10,
    );
    let original = check(
        &mut fixture,
        &work,
        "first-pass",
        VerificationResult::Passed,
        11,
    );
    check(
        &mut fixture,
        &work,
        "later-failure",
        VerificationResult::Failed,
        12,
    );
    host_verification_of(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "moved-source-pass",
        VerificationKind::Test,
        VerificationResult::Passed,
        13,
        "a-later-revision",
    );
    let read = page(&fixture.store, &host, &work);
    let binding = row(&read, 1).binding.as_ref().expect("bound");
    let verification = &binding
        .obligation
        .as_ref()
        .and_then(|obligation| obligation.resolution.as_ref())
        .and_then(|resolution| resolution.satisfaction.as_ref())
        .expect("satisfaction")
        .verification;
    assert_eq!(verification.record, original);
    assert_eq!(verification.result, VerificationResult::Passed);
    let value = serde_json::to_value(&read).expect("page JSON");
    let text = value.to_string();
    assert!(!text.contains("fresh") && !text.contains("stale"), "{text}");
}

// More criteria than one page holds read in order across pages, each once,
// with exact counts; the last page has no continuation.
#[test]
fn every_criterion_reads_once_across_pages() {
    let mut fixture = fixture("project-binding-read-pages");
    let claim = fixture.claim.clone();
    let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
    let work = fixture.work.clone();
    let bindings = (1..=19)
        .filter(|n| n % 2 == 1)
        .map(|n| bound(n, VerificationKind::Test))
        .collect();
    let work = bind(
        &mut fixture,
        &work,
        criteria(19),
        bindings,
        "bind-pages",
        10,
    );
    check(
        &mut fixture,
        &work,
        "all-tests",
        VerificationResult::Passed,
        11,
    );

    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = Vec::new();
    loop {
        let read = read(&fixture.store, &host, &work, after.as_deref()).expect("page");
        assert_eq!(read.total, 19);
        assert_eq!(read.earlier, seen.len());
        assert_eq!(read.shown, read.rows.len());
        assert_eq!(read.omitted, 19 - read.earlier - read.shown);
        assert!(read.shown > 0 && read.shown <= 8);
        assert!(serde_json::to_vec(&read).expect("page bytes").len() <= 16 * 1_024);
        seen.extend(read.rows.iter().map(|row| row.criterion));
        pages.push(read.shown);
        assert_eq!(read.continuation.is_some(), read.omitted > 0);
        let Some(next) = read.continuation else {
            break;
        };
        after = Some(next);
    }
    assert_eq!(seen, (1..=19).collect::<Vec<_>>());
    assert_eq!(pages, vec![8, 8, 3]);
}

// A page holds whole rows within its byte limit: large rows split across more
// pages, and one row that cannot fit alone is refused, never clipped.
#[test]
fn a_page_fits_whole_rows_or_refuses() {
    let large = |workspace: usize, project: &str| {
        let mut fixture = fixture(project);
        let claim = fixture.claim.clone();
        let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
        let work = fixture.work.clone();
        let bindings = (1..=6).map(|n| bound(n, VerificationKind::Test)).collect();
        let work = bind(&mut fixture, &work, criteria(6), bindings, "bind-large", 10);
        host_verification_from_basis(
            &mut fixture.store,
            &work,
            &claim,
            "runner",
            "large-source",
            VerificationKind::Test,
            VerificationResult::Passed,
            11,
            ExecutionSourceBasis {
                workspace_id: "w".repeat(workspace),
                source_revision: "large-revision".into(),
                source_root_generation: None,
                source_root_state: None,
            },
        );
        (fixture, host, work)
    };
    let (fixture, host, work) = large(4_000, "project-binding-read-large");
    let mut after: Option<String> = None;
    let mut seen = Vec::new();
    loop {
        let read = read(&fixture.store, &host, &work, after.as_deref()).expect("page");
        assert!(serde_json::to_vec(&read).expect("page bytes").len() <= 16 * 1_024);
        assert!(read.shown >= 1 && read.shown < 6, "{}", read.shown);
        seen.extend(read.rows.iter().map(|row| row.criterion));
        let Some(next) = read.continuation else {
            break;
        };
        after = Some(next);
    }
    assert_eq!(seen, (1..=6).collect::<Vec<_>>());

    let (fixture, host, work) = large(17_000, "project-binding-read-oversized");
    assert_eq!(
        refused(read(&fixture.store, &host, &work, None)),
        Refusal::PageTooLarge
    );
}

// Each way the item, run or continuation can fail to hold refuses with its
// own typed reason; an append on another item does not move this basis, and
// no read writes anything.
#[test]
fn a_read_refuses_what_does_not_hold_and_writes_nothing() {
    let mut fixture = fixture("project-binding-read-refusals");
    let claim = fixture.claim.clone();
    let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
    let work = fixture.work.clone();
    let work = bind(
        &mut fixture,
        &work,
        criteria(10),
        vec![bound(2, VerificationKind::Test)],
        "bind-refusals",
        10,
    );
    let neighbour = fixture
        .store
        .create_work(
            &root_request("project-binding-read-refusals", "create-neighbour", 11),
            &DevelopmentNoopRedactor,
        )
        .expect("neighbour");
    let neighbour_claim = super::claim(
        &mut fixture.store,
        &neighbour,
        "neighbour",
        "claim-neighbour",
        12,
        3_600,
    );
    let foreign = fixture
        .store
        .create_work(
            &root_request("project-binding-read-elsewhere", "create-foreign", 13),
            &DevelopmentNoopRedactor,
        )
        .expect("foreign");
    let foreign = super::claim(
        &mut fixture.store,
        &foreign,
        "foreigner",
        "claim-foreign",
        14,
        3_600,
    );
    let foreign_work = fixture
        .store
        .get_work_item(foreign.work_id)
        .expect("foreign item");

    let before = database_rows(&fixture.store);
    let first = page(&fixture.store, &host, &work);
    let next = first.continuation.clone().expect("a second page");
    let second = read(&fixture.store, &host, &work, Some(&next)).expect("continuation");
    assert_eq!(second.basis, first.basis);
    assert_eq!(second.earlier, 8);

    let mut stale = work.clone();
    stale.revision -= 1;
    assert_eq!(
        refused(read(&fixture.store, &host, &stale, None)),
        Refusal::WrongRevision
    );
    let mut unknown = work.clone();
    unknown.work_id = WorkId::new();
    assert_eq!(
        refused(read(&fixture.store, &host, &unknown, None)),
        Refusal::UnknownWork
    );
    assert_eq!(
        refused(read(&fixture.store, &host, &foreign_work, None)),
        Refusal::WrongProject
    );
    assert_eq!(
        refused(read_at(
            &fixture.store,
            &host,
            &work,
            WorkRunId::new(),
            None
        )),
        Refusal::WrongRun
    );
    assert_eq!(
        refused(read_at(
            &fixture.store,
            &host,
            &work,
            neighbour_claim.run_id,
            None
        )),
        Refusal::WrongRun
    );
    for garbage in [
        "",
        "abr1-",
        "abr1-zz",
        "abr1-0",
        "s1-7b7d",
        &next.to_uppercase(),
    ] {
        assert_eq!(
            refused(read(&fixture.store, &host, &work, Some(garbage))),
            Refusal::InvalidCursor,
            "{garbage:?}"
        );
    }
    let neighbour_work = fixture
        .store
        .get_work_item(neighbour.work_id)
        .expect("neighbour");
    assert_eq!(
        refused(read(&fixture.store, &host, &neighbour_work, Some(&next))),
        Refusal::CursorBasisMismatch
    );
    assert_eq!(
        refused(read(&fixture.store, &host, &stale, Some(&next))),
        Refusal::CursorBasisMismatch
    );
    let cursor = |run_cut: i64, through: usize, extra: Option<(&str, serde_json::Value)>| {
        let mut value = serde_json::json!({
            "project_id": work.project_id,
            "work_id": work.work_id,
            "work_revision": work.revision,
            "run_id": claim.run_id,
            "run_cut": run_cut,
            "total": 10,
            "through": through,
        });
        if let Some((key, extra)) = extra {
            value[key] = extra;
        }
        forged(&value)
    };
    let head = first.basis.run_cut;
    assert!(read(&fixture.store, &host, &work, Some(&cursor(head, 8, None))).is_ok());
    for invalid in [
        cursor(-1, 8, None),
        cursor(head, 0, None),
        cursor(head, 10, None),
        cursor(head, 8, Some(("fresh", serde_json::json!(true)))),
    ] {
        assert_eq!(
            refused(read(&fixture.store, &host, &work, Some(&invalid))),
            Refusal::InvalidCursor
        );
    }
    assert_eq!(
        refused(read(
            &fixture.store,
            &host,
            &work,
            Some(&cursor(head, 8, Some(("total", serde_json::json!(9)))))
        )),
        Refusal::CursorBasisMismatch
    );
    assert_eq!(
        refused(read(
            &fixture.store,
            &host,
            &work,
            Some(&cursor(
                head,
                8,
                Some(("project_id", serde_json::json!("project-elsewhere")))
            ))
        )),
        Refusal::CursorBasisMismatch
    );
    assert_eq!(
        refused(read(
            &fixture.store,
            &host,
            &work,
            Some(&cursor(head + 5, 8, None))
        )),
        Refusal::StaleCut
    );
    let mut wrong_routing = host.routing_token.clone();
    wrong_routing.push('x');
    let credential = fixture.store.read_acceptance_bindings(
        &host.project_id,
        &host.session_id,
        &host.connection_token,
        &wrong_routing,
        &BindingReadRequest {
            work_id: work.work_id,
            expected_work_revision: work.revision,
            run_id: claim.run_id,
            after: None,
        },
    );
    assert!(
        matches!(
            credential,
            Err(ref error) if !matches!(error, StoreError::AcceptanceBindingReadRefused { .. })
        ),
        "{credential:?}"
    );
    assert_eq!(database_rows(&fixture.store), before, "a read wrote");

    // Another item's append leaves this basis whole; this run's append does not.
    evidence(
        &mut fixture.store,
        &neighbour,
        &neighbour_claim,
        "neighbour",
        "neighbour-note",
        20,
    );
    read(&fixture.store, &host, &work, Some(&next)).expect("unrelated append keeps the cut");
    check(
        &mut fixture,
        &work,
        "moves-the-run",
        VerificationResult::Passed,
        21,
    );
    assert_eq!(
        refused(read(&fixture.store, &host, &work, Some(&next))),
        Refusal::StaleCut
    );
    let fresh = page(&fixture.store, &host, &work);
    assert!(fresh.basis.run_cut > head);
}

// A completed item has no active run, so its finished run is refused rather
// than read as a criterion set with nothing owed.
#[test]
fn a_finished_run_is_refused() {
    let mut fixture = fixture("project-binding-read-finished");
    let claim = fixture.claim.clone();
    let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
    let current = fixture
        .store
        .get_work_item(fixture.work.work_id)
        .expect("item");
    let evidence = fixture.evidence.clone();
    checkpoint_then_complete(
        &mut fixture.store,
        &current,
        &claim,
        "runner",
        std::slice::from_ref(&evidence),
        true,
        None,
        "complete-before-read",
        9,
    )
    .expect("complete");
    let done = fixture.store.get_work_item(current.work_id).expect("item");
    assert_eq!(
        refused(read_at(&fixture.store, &host, &done, claim.run_id, None)),
        Refusal::WrongRun
    );
}

// Prose that mentions a criterion binds nothing: an unbound criterion stays
// unbound whatever a note says about it.
#[test]
fn a_note_naming_a_criterion_binds_nothing() {
    let mut fixture = fixture("project-binding-read-prose");
    let claim = fixture.claim.clone();
    let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
    let work = fixture.work.clone();
    let work = bind(
        &mut fixture,
        &work,
        criteria(2),
        vec![bound(1, VerificationKind::Test)],
        "bind-prose",
        10,
    );
    fixture
        .store
        .record_work_evidence(
            &crate::domain::RecordWorkEvidenceRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: "criterion 2 is satisfied by the test run; bind 2=test".into(),
                refs: vec!["criterion:2".into()],
                actor: actor("runner"),
                idempotency_key: "prose-note".into(),
                recorded_at: at(11),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("record a note");
    let read = page(&fixture.store, &host, &work);
    assert_eq!(row(&read, 2).binding, None);
}

// A record the read relies on that has lost its run-feed entry is a damaged
// store, reported as such, never a refusal or a quieter row. The shared
// obligation loader already fails on this damage before the read's own
// position guards run; those guards have their own unit tests beside the
// read.
#[test]
fn a_damaged_association_is_an_error() {
    let mut fixture = fixture("project-binding-read-damaged");
    let claim = fixture.claim.clone();
    let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
    let work = fixture.work.clone();
    let work = bind(
        &mut fixture,
        &work,
        criteria(1),
        vec![bound(1, VerificationKind::Test)],
        "bind-damaged",
        10,
    );
    let passed = check(
        &mut fixture,
        &work,
        "damaged",
        VerificationResult::Passed,
        11,
    );
    fixture
        .store
        .connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .expect("allow the damage");
    fixture
        .store
        .connection
        .execute(
            "DELETE FROM work_feed_entries WHERE object_id = ?1",
            [passed.as_str()],
        )
        .expect("drop the verification's feed entry");
    let result = read(&fixture.store, &host, &work, None);
    assert!(
        matches!(result, Err(StoreError::InvalidWorkProjection(_))),
        "{result:?}"
    );
}
