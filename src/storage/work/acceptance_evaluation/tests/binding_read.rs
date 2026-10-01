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

/// A host's read, per acceptance criterion of an item on its active run, of
/// every host verification of the criterion's bound kind up to a pinned
/// cut: complete recorded facts in run-feed order, paged with exact counts
/// and a validated continuation, typed refusals, damage reported as damage,
/// and no writes. It shares this file's fixtures with the binding read.
mod candidates {
    use super::*;
    use crate::domain::{
        AcceptanceBinding, AcceptanceVerificationPage,
        AcceptanceVerificationReadRefusal as Refusal, VerificationRequirement,
    };
    use crate::storage::VerificationReadRequest;

    fn read_with(
        store: &SqliteStore,
        host: &HostSession,
        work: &WorkItem,
        run_id: WorkRunId,
        run_cut: i64,
        criterion: usize,
        after: Option<&str>,
    ) -> Result<AcceptanceVerificationPage, StoreError> {
        store.read_acceptance_verifications(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            &VerificationReadRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                run_id,
                run_cut,
                criterion,
                after,
            },
        )
    }

    /// Reads `criterion` at `run_cut` on the item's active run.
    fn read_at(
        store: &SqliteStore,
        host: &HostSession,
        work: &WorkItem,
        run_cut: i64,
        criterion: usize,
        after: Option<&str>,
    ) -> Result<AcceptanceVerificationPage, StoreError> {
        let run = work.active_run_id.expect("active run");
        read_with(store, host, work, run, run_cut, criterion, after)
    }

    /// The first page of `criterion` at the run's current head.
    fn first(
        store: &SqliteStore,
        host: &HostSession,
        work: &WorkItem,
        criterion: usize,
    ) -> AcceptanceVerificationPage {
        read_at(store, host, work, cut(store, work), criterion, None).expect("first page")
    }

    /// Every candidate of `criterion`, following continuations at one cut.
    fn all_pages(
        store: &SqliteStore,
        host: &HostSession,
        work: &WorkItem,
        criterion: usize,
    ) -> Vec<AcceptanceVerificationPage> {
        let head = cut(store, work);
        let mut pages = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = read_at(store, host, work, head, criterion, after.as_deref()).expect("page");
            after.clone_from(&page.continuation);
            pages.push(page);
            if after.is_none() {
                return pages;
            }
        }
    }

    fn refused(result: Result<AcceptanceVerificationPage, StoreError>) -> Refusal {
        match result {
            Err(StoreError::AcceptanceVerificationReadRefused { refusal, .. }) => refusal,
            other => panic!("expected a verification read refusal, got {other:?}"),
        }
    }

    fn pinned(
        criterion: usize,
        kind: VerificationKind,
        fingerprint: ObjectId,
    ) -> AcceptanceBinding {
        AcceptanceBinding {
            criterion,
            requirement: VerificationRequirement {
                check_kind: kind,
                check_fingerprint: Some(fingerprint),
            },
        }
    }

    /// One host check on the fixture's run, with a producer outcome that agrees
    /// with its result, as the checkpoint protocol records them.
    fn check(
        fixture: &mut Fixture,
        work: &WorkItem,
        key: &str,
        kind: VerificationKind,
        result: VerificationResult,
        second: i64,
        workspace: Option<&str>,
    ) -> ObjectId {
        let claim = fixture.claim.clone();
        let outcome = match result {
            VerificationResult::Passed => ExecutionOutcome::Succeeded,
            VerificationResult::Failed => ExecutionOutcome::Failed,
            VerificationResult::Indeterminate => ExecutionOutcome::Unknown,
        };
        host_verification_with_outcome(
            &mut fixture.store,
            work,
            &claim,
            "runner",
            HostCheck {
                key,
                kind,
                outcome,
                result,
                summary: &format!("host observed {key}"),
            },
            second,
            ExecutionSourceBasis {
                workspace_id: workspace.map_or_else(|| format!("workspace-{key}"), str::to_owned),
                source_revision: format!("revision-{key}"),
                source_root_generation: None,
                source_root_state: None,
            },
        )
    }

    fn stored(store: &SqliteStore, id: &ObjectId) -> VerificationEvidence {
        crate::storage::work::load_typed_work_object(&store.connection, id, "verification_evidence")
            .expect("stored verification")
    }

    /// Where `record` sits on `run`'s feed, read straight from the feed table.
    fn feed_position(store: &SqliteStore, run: WorkRunId, record: &ObjectId) -> i64 {
        store
            .connection
            .query_row(
                "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2",
                rusqlite::params![run.0.to_string(), record.as_str()],
                |row| row.get(0),
            )
            .expect("feed position")
    }

    /// A continuation token carrying `cursor`, as the read encodes one.
    fn forged(prefix: &str, cursor: &serde_json::Value) -> String {
        let mut token = String::from(prefix);
        for byte in serde_json::to_vec(cursor).expect("encode cursor") {
            write!(token, "{byte:02x}").expect("write cursor");
        }
        token
    }

    /// The JSON a continuation carries, decoded from its token.
    fn cursor_of(token: &str) -> serde_json::Value {
        let hex = token.strip_prefix("avr1-").expect("verification cursor");
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex"))
            .collect();
        serde_json::from_slice(&bytes).expect("cursor json")
    }

    // Every check of the bound kind on the run, whatever its result, its
    // fingerprint, its source or when it was recorded, reads in run-feed order
    // with the complete facts the record holds. Other kinds are left out, two
    // criteria of one kind read the same candidates, a pinned fingerprint does
    // not filter, and an unbound criterion reads an empty page that is not a
    // pass.
    #[test]
    fn every_check_of_the_bound_kind_reads_in_feed_order_with_its_recorded_facts() {
        let mut fixture = fixture("project-verification-read");
        let claim = fixture.claim.clone();
        let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
        let work = fixture.work.clone();
        // Checks recorded before any criterion was bound, at an older revision.
        let early = check(
            &mut fixture,
            &work,
            "early",
            VerificationKind::Test,
            VerificationResult::Passed,
            6,
            None,
        );
        let unit = check(
            &mut fixture,
            &work,
            "unit",
            VerificationKind::Test,
            VerificationResult::Passed,
            7,
            None,
        );
        let unit_fingerprint = stored(&fixture.store, &unit).check_fingerprint;
        let work = bind(
            &mut fixture,
            &work,
            criteria(5),
            vec![
                bound(1, VerificationKind::Test),
                bound(2, VerificationKind::Lint),
                bound(3, VerificationKind::Test),
                pinned(5, VerificationKind::Test, check_fingerprint("never-run")),
            ],
            "bind-candidates",
            10,
        );
        let failed = check(
            &mut fixture,
            &work,
            "flaky",
            VerificationKind::Test,
            VerificationResult::Failed,
            11,
            None,
        );
        let lint = check(
            &mut fixture,
            &work,
            "style",
            VerificationKind::Lint,
            VerificationResult::Passed,
            12,
            None,
        );
        let unknown = check(
            &mut fixture,
            &work,
            "unknown",
            VerificationKind::Test,
            VerificationResult::Indeterminate,
            13,
            None,
        );
        // The same command again: an equal fingerprint, a distinct record.
        let again = check(
            &mut fixture,
            &work,
            "unit",
            VerificationKind::Test,
            VerificationResult::Passed,
            14,
            None,
        );
        assert_ne!(again, unit);
        assert_eq!(
            stored(&fixture.store, &again).check_fingerprint,
            unit_fingerprint
        );
        let unicode = check(
            &mut fixture,
            &work,
            "unicode",
            VerificationKind::Test,
            VerificationResult::Passed,
            15,
            Some("przestrzeń-✓-工作区"),
        );

        let head = cut(&fixture.store, &work);
        let tests = first(&fixture.store, &host, &work, 1);
        assert_eq!(tests.basis.project_id, work.project_id);
        assert_eq!(tests.basis.work_id, work.work_id);
        assert_eq!(tests.basis.work_revision, work.revision);
        assert_eq!(Some(tests.basis.run_id), work.active_run_id);
        assert_eq!(tests.basis.run_cut, head);
        assert_eq!(tests.criterion, 1);
        assert_eq!(
            tests
                .requirement
                .as_ref()
                .map(|requirement| requirement.check_kind),
            Some(VerificationKind::Test)
        );
        assert_eq!(
            (tests.total, tests.earlier, tests.shown, tests.omitted),
            (6, 0, 6, 0)
        );
        assert_eq!(tests.continuation, None);
        let records: Vec<&ObjectId> = tests.rows.iter().map(|row| &row.record).collect();
        assert_eq!(
            records,
            vec![&early, &unit, &failed, &unknown, &again, &unicode]
        );
        let positions: Vec<i64> = tests.rows.iter().map(|row| row.position).collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{positions:?}"
        );
        for row in &tests.rows {
            let record = stored(&fixture.store, &row.record);
            assert_eq!(row.check_kind, record.check_kind);
            assert_eq!(row.check_fingerprint, record.check_fingerprint);
            assert_eq!(row.result, record.result);
            assert_eq!(row.source_basis, record.source_basis);
            assert_eq!(row.producer.record, record.producer_observation);
            assert!(row.producer.position < row.position);
            assert!(row.position <= head);
            assert_eq!(
                row.position,
                feed_position(&fixture.store, claim.run_id, &row.record)
            );
            assert_eq!(
                row.producer.position,
                feed_position(&fixture.store, claim.run_id, &row.producer.record)
            );
        }
        let results: Vec<VerificationResult> = tests.rows.iter().map(|row| row.result).collect();
        assert_eq!(
            results,
            vec![
                VerificationResult::Passed,
                VerificationResult::Passed,
                VerificationResult::Failed,
                VerificationResult::Indeterminate,
                VerificationResult::Passed,
                VerificationResult::Passed,
            ]
        );
        assert_eq!(tests.rows[2].producer.outcome, ExecutionOutcome::Failed);
        assert_eq!(tests.rows[3].producer.outcome, ExecutionOutcome::Unknown);
        assert_eq!(
            tests.rows[5].source_basis.workspace_id,
            "przestrzeń-✓-工作区"
        );

        // Another kind is never a candidate.
        let lints = first(&fixture.store, &host, &work, 2);
        assert_eq!(
            lints.rows.iter().map(|row| &row.record).collect::<Vec<_>>(),
            vec![&lint]
        );
        assert_eq!((lints.total, lints.shown), (1, 1));
        // Two criteria of one kind read the same candidates.
        assert_eq!(first(&fixture.store, &host, &work, 3).rows, tests.rows);
        // An unbound criterion reads an empty page, which is not a pass.
        let unbound = first(&fixture.store, &host, &work, 4);
        assert_eq!(unbound.requirement, None);
        assert_eq!(
            (
                unbound.total,
                unbound.earlier,
                unbound.shown,
                unbound.omitted
            ),
            (0, 0, 0, 0)
        );
        assert!(unbound.rows.is_empty(), "{:?}", unbound.rows);
        assert_eq!(unbound.continuation, None);
        // A pinned fingerprint no check ran under filters nothing: the consumer
        // judges which candidate applies.
        let pinned = first(&fixture.store, &host, &work, 5);
        assert_eq!(
            pinned
                .requirement
                .as_ref()
                .and_then(|requirement| requirement.check_fingerprint.clone()),
            Some(check_fingerprint("never-run"))
        );
        assert_eq!(pinned.rows, tests.rows);
    }

    // More candidates than a page holds read once each, in order, across pages
    // with exact counts. Byte fitting and the refusal of a row too large for a
    // page are unit-tested beside the read: valid records are bounded well below
    // a page, and the read refuses a record outside those bounds as damage.
    #[test]
    fn candidates_page_in_order_with_exact_counts() {
        let mut fixture = fixture("project-verification-read-pages");
        let claim = fixture.claim.clone();
        let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
        let work = fixture.work.clone();
        let work = bind(
            &mut fixture,
            &work,
            criteria(2),
            vec![
                bound(1, VerificationKind::Test),
                bound(2, VerificationKind::Test),
            ],
            "bind-pages",
            10,
        );
        let mut recorded = Vec::new();
        for n in 0..19 {
            recorded.push(check(
                &mut fixture,
                &work,
                &format!("check-{n}"),
                VerificationKind::Test,
                VerificationResult::Passed,
                11 + n,
                None,
            ));
        }
        let pages = all_pages(&fixture.store, &host, &work, 1);
        let mut seen = Vec::new();
        for page in &pages {
            assert_eq!(page.total, 19);
            assert_eq!(page.earlier, seen.len());
            assert_eq!(page.shown, page.rows.len());
            assert_eq!(page.omitted, 19 - page.earlier - page.shown);
            assert_eq!(page.continuation.is_some(), page.omitted > 0);
            assert!(serde_json::to_vec(page).expect("page bytes").len() <= 16 * 1_024);
            assert_eq!(page.basis, pages[0].basis);
            seen.extend(page.rows.iter().map(|row| row.record.clone()));
        }
        assert_eq!(seen, recorded);
        assert_eq!(
            pages.iter().map(|page| page.shown).collect::<Vec<_>>(),
            vec![8, 8, 3]
        );
        // Each criterion has its own continuation, even over the same candidates.
        let next = pages[0].continuation.clone().expect("a second page");
        assert_eq!(cursor_of(&next)["criterion"], 1);
        assert_eq!(
            cursor_of(&next)["last_record"],
            serde_json::json!(recorded[7])
        );
        assert_eq!(
            refused(read_at(
                &fixture.store,
                &host,
                &work,
                pages[0].basis.run_cut,
                2,
                Some(&next)
            )),
            Refusal::CursorBasisMismatch
        );
    }

    // Each way the item, run, cut, criterion or continuation can fail to hold
    // refuses with its own typed reason. A check appended after the cut refuses
    // even a first page at that cut, an append on another item does not move
    // this basis, and no read writes anything.
    #[test]
    fn a_read_refuses_what_does_not_hold_and_writes_nothing() {
        let mut fixture = fixture("project-verification-read-refusals");
        let claim = fixture.claim.clone();
        let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
        let work = fixture.work.clone();
        let work = bind(
            &mut fixture,
            &work,
            criteria(3),
            vec![
                bound(1, VerificationKind::Test),
                bound(2, VerificationKind::Lint),
            ],
            "bind-refusals",
            10,
        );
        let mut recorded = Vec::new();
        for n in 0..10 {
            recorded.push(check(
                &mut fixture,
                &work,
                &format!("refusal-{n}"),
                VerificationKind::Test,
                VerificationResult::Passed,
                11 + n,
                None,
            ));
        }
        let neighbour = fixture
            .store
            .create_work(
                &root_request("project-verification-read-refusals", "create-neighbour", 30),
                &DevelopmentNoopRedactor,
            )
            .expect("neighbour");
        let neighbour_claim = super::claim(
            &mut fixture.store,
            &neighbour,
            "neighbour",
            "claim-neighbour",
            31,
            3_600,
        );
        let foreign = fixture
            .store
            .create_work(
                &root_request("project-verification-read-elsewhere", "create-foreign", 32),
                &DevelopmentNoopRedactor,
            )
            .expect("foreign");
        let foreign = super::claim(
            &mut fixture.store,
            &foreign,
            "foreigner",
            "claim-foreign",
            33,
            3_600,
        );
        let foreign_work = fixture
            .store
            .get_work_item(foreign.work_id)
            .expect("foreign item");

        let before = database_rows(&fixture.store);
        let head = cut(&fixture.store, &work);
        let page = first(&fixture.store, &host, &work, 1);
        let next = page.continuation.clone().expect("a second page");
        let second = read_at(&fixture.store, &host, &work, head, 1, Some(&next)).expect("page two");
        assert_eq!(second.earlier, 8);
        assert_eq!(second.rows.len(), 2);

        let mut stale = work.clone();
        stale.revision -= 1;
        assert_eq!(
            refused(read_at(&fixture.store, &host, &stale, head, 1, None)),
            Refusal::WrongRevision
        );
        let mut unknown = work.clone();
        unknown.work_id = WorkId::new();
        assert_eq!(
            refused(read_at(&fixture.store, &host, &unknown, head, 1, None)),
            Refusal::UnknownWork
        );
        assert_eq!(
            refused(read_with(
                &fixture.store,
                &host,
                &foreign_work,
                foreign.run_id,
                head,
                1,
                None
            )),
            Refusal::WrongProject
        );
        for run in [WorkRunId::new(), neighbour_claim.run_id] {
            assert_eq!(
                refused(read_with(&fixture.store, &host, &work, run, head, 1, None)),
                Refusal::WrongRun
            );
        }
        for criterion in [0, 4] {
            assert_eq!(
                refused(read_at(&fixture.store, &host, &work, head, criterion, None)),
                Refusal::InvalidCriterion,
                "criterion {criterion}"
            );
        }
        for wrong_cut in [head - 1, head + 5] {
            assert_eq!(
                refused(read_at(&fixture.store, &host, &work, wrong_cut, 1, None)),
                Refusal::StaleCut,
                "cut {wrong_cut}"
            );
        }

        // Continuations: garbage, a binding read's token and every forged
        // boundary are refused as invalid; one made for another basis,
        // criterion or requirement is a mismatch.
        let pinned = cursor_of(&next);
        let binding_token = forged(
            "abr1-",
            &serde_json::json!({
                "project_id": work.project_id,
                "work_id": work.work_id,
                "work_revision": work.revision,
                "run_id": claim.run_id,
                "run_cut": head,
                "total": 3,
                "through": 1,
            }),
        );
        for garbage in [
            String::new(),
            "avr1-".into(),
            "avr1-zz".into(),
            "avr1-0".into(),
            next.to_uppercase(),
            binding_token,
        ] {
            assert_eq!(
                refused(read_at(
                    &fixture.store,
                    &host,
                    &work,
                    head,
                    1,
                    Some(&garbage)
                )),
                Refusal::InvalidCursor,
                "{garbage:?}"
            );
        }
        let altered = |key: &str, value: serde_json::Value| {
            let mut cursor = pinned.clone();
            cursor[key] = value;
            forged("avr1-", &cursor)
        };
        assert_eq!(
            read_at(
                &fixture.store,
                &host,
                &work,
                head,
                1,
                Some(&forged("avr1-", &pinned))
            )
            .expect("a reconstructed boundary resumes")
            .rows,
            second.rows
        );
        for (key, value) in [
            ("last_record", serde_json::json!(recorded[6])),
            (
                "last_position",
                serde_json::json!(pinned["last_position"].as_i64().expect("position") - 1),
            ),
            ("through", serde_json::json!(7)),
            ("total", serde_json::json!(11)),
            ("fresh", serde_json::json!(true)),
        ] {
            assert_eq!(
                refused(read_at(
                    &fixture.store,
                    &host,
                    &work,
                    head,
                    1,
                    Some(&altered(key, value))
                )),
                Refusal::InvalidCursor,
                "{key}"
            );
        }
        for (key, value) in [
            ("criterion", serde_json::json!(2)),
            ("run_cut", serde_json::json!(head - 1)),
            ("project_id", serde_json::json!("project-elsewhere")),
            ("requirement", serde_json::json!({ "check_kind": "lint" })),
        ] {
            assert_eq!(
                refused(read_at(
                    &fixture.store,
                    &host,
                    &work,
                    head,
                    1,
                    Some(&altered(key, value))
                )),
                Refusal::CursorBasisMismatch,
                "{key}"
            );
        }
        assert_eq!(
            refused(read_at(&fixture.store, &host, &stale, head, 1, Some(&next))),
            Refusal::CursorBasisMismatch
        );
        let mut wrong_routing = host.routing_token.clone();
        wrong_routing.push('x');
        let credential = fixture.store.read_acceptance_verifications(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &wrong_routing,
            &VerificationReadRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                run_id: claim.run_id,
                run_cut: head,
                criterion: 1,
                after: None,
            },
        );
        assert!(
            matches!(
                credential,
                Err(ref error)
                    if !matches!(error, StoreError::AcceptanceVerificationReadRefused { .. })
            ),
            "{credential:?}"
        );
        assert_eq!(database_rows(&fixture.store), before, "a read wrote");

        // Another item's append leaves this basis whole.
        evidence(
            &mut fixture.store,
            &neighbour,
            &neighbour_claim,
            "neighbour",
            "neighbour-note",
            40,
        );
        read_at(&fixture.store, &host, &work, head, 1, Some(&next))
            .expect("an unrelated append keeps the cut");
        // A check after the cut refuses the first page at that cut as well as the
        // continuation; reading again at the new head counts it.
        let late = check(
            &mut fixture,
            &work,
            "after-the-cut",
            VerificationKind::Test,
            VerificationResult::Passed,
            41,
            None,
        );
        assert_eq!(
            refused(read_at(&fixture.store, &host, &work, head, 1, None)),
            Refusal::StaleCut
        );
        assert_eq!(
            refused(read_at(&fixture.store, &host, &work, head, 1, Some(&next))),
            Refusal::StaleCut
        );
        let fresh = all_pages(&fixture.store, &host, &work, 1);
        assert!(fresh[0].basis.run_cut > head);
        assert_eq!(fresh[0].total, 11);
        assert_eq!(
            fresh
                .last()
                .and_then(|page| page.rows.last())
                .map(|row| &row.record),
            Some(&late)
        );
    }

    /// The records one damage case may break: the candidate, its producer and
    /// environment, and the root execution they name.
    struct Damaged {
        verification: ObjectId,
        producer: ObjectId,
        environment: ObjectId,
        root: String,
    }

    /// One damage case: the SQL that breaks a record of `Damaged`.
    type Damage = dyn Fn(&Damaged) -> String;

    /// A store where the SQL `damage` returns broke one record the read
    /// relies on, read for the bound criterion at the cut before the damage.
    fn read_damaged(
        project: &str,
        damage: impl Fn(&Damaged) -> String,
    ) -> Result<AcceptanceVerificationPage, StoreError> {
        let mut fixture = fixture(project);
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
        let verification = check(
            &mut fixture,
            &work,
            "damaged",
            VerificationKind::Test,
            VerificationResult::Passed,
            11,
            None,
        );
        let record = stored(&fixture.store, &verification);
        let damaged = Damaged {
            verification,
            producer: record.producer_observation,
            environment: record.environment.expect("the check's environment"),
            root: serde_json::to_value(record.binding.root_execution_id)
                .expect("root id")
                .as_str()
                .expect("root id text")
                .to_owned(),
        };
        let head = cut(&fixture.store, &work);
        read_at(&fixture.store, &host, &work, head, 1, None).expect("the undamaged read");
        fixture
            .store
            .connection
            .execute_batch(&format!("PRAGMA foreign_keys = OFF; {}", damage(&damaged)))
            .expect("damage the store");
        read_at(&fixture.store, &host, &work, head, 1, None)
    }

    // A candidate the run's records do not fully vouch for is a damaged store,
    // never a refusal or a shorter list: a verification without its feed
    // entry or projection row or disagreeing with its projection; a producer
    // without its feed entry, recorded as another kind, at the feed's start
    // or after its verification; and a verification and producer that agree
    // with each other and their environment but name another root execution.
    #[test]
    fn a_damaged_candidate_is_an_error() {
        let cases: [(&str, &Damage); 8] = [
            ("unfed", &|d| {
                format!(
                    "DELETE FROM work_feed_entries WHERE object_id = '{}'",
                    d.verification
                )
            }),
            ("unprojected", &|d| {
                format!(
                    "DELETE FROM work_run_evidence WHERE evidence_id = '{}'",
                    d.verification
                )
            }),
            ("projection", &|d| {
                format!(
                    "UPDATE work_run_evidence SET verification_result = 'failed' WHERE evidence_id = '{}'",
                    d.verification
                )
            }),
            ("producer-unfed", &|d| {
                format!(
                    "DELETE FROM work_feed_entries WHERE object_id = '{}'",
                    d.producer
                )
            }),
            ("producer-kind", &|d| {
                format!(
                    "UPDATE work_feed_entries SET object_kind = 'work_evidence' WHERE object_id = '{}'",
                    d.producer
                )
            }),
            ("producer-start", &|d| {
                format!(
                    "UPDATE work_feed_entries SET position = 0 WHERE object_id = '{}'",
                    d.producer
                )
            }),
            ("producer-after", &|d| {
                format!(
                    "UPDATE work_feed_entries SET position = position + 1000 WHERE object_id = '{}'",
                    d.producer
                )
            }),
            ("root", &|d| {
                format!(
                    "UPDATE objects
                     SET canonical_json = CAST(replace(CAST(canonical_json AS TEXT), '{root}', '{other}') AS BLOB)
                     WHERE object_id IN ('{}', '{}', '{}')",
                    d.verification,
                    d.producer,
                    d.environment,
                    root = d.root,
                    other = uuid::Uuid::new_v4(),
                )
            }),
        ];
        for (case, damage) in cases {
            let result = read_damaged(&format!("project-verification-read-{case}"), damage);
            assert!(
                matches!(result, Err(StoreError::InvalidWorkProjection(_))),
                "{case}: {result:?}"
            );
        }
    }

    // Criteria keep the order their author typed, so a list that is not in
    // alphabetical order, reordered, moves its bound criterion: both host
    // reads find it at its new position with its requirement and the
    // obligation the reorder opened, the old position reads unbound, and the
    // revision before the reorder is refused by both.
    #[test]
    fn both_reads_follow_a_bound_criterion_across_a_reorder() {
        let mut fixture = fixture("project-reorder-reads");
        let claim = fixture.claim.clone();
        let host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
        let work = fixture.work.clone();
        let typed = vec![
            "zeta: the tests pass".to_owned(),
            "alpha: the docs are written".to_owned(),
            "mid: the changelog names it".to_owned(),
        ];
        let work = bind(
            &mut fixture,
            &work,
            typed.clone(),
            vec![bound(1, VerificationKind::Test)],
            "bind-before-reorder",
            10,
        );
        assert_eq!(work.acceptance, typed);
        let passed = check(
            &mut fixture,
            &work,
            "before-reorder",
            VerificationKind::Test,
            VerificationResult::Passed,
            11,
            None,
        );
        let before = page(&fixture.store, &host, &work);
        let original = row(&before, 1)
            .binding
            .as_ref()
            .and_then(|binding| binding.obligation.as_ref())
            .expect("the original obligation")
            .clone();
        assert_eq!(original.state, WorkObligationState::Satisfied);
        let candidates = first(&fixture.store, &host, &work, 1);
        assert_eq!(candidates.criterion, 1);
        assert_eq!(
            candidates.requirement,
            Some(bound(1, VerificationKind::Test).requirement)
        );

        let reordered_list = vec![
            "mid: the changelog names it".to_owned(),
            "zeta: the tests pass".to_owned(),
            "alpha: the docs are written".to_owned(),
        ];
        let reordered = bind(
            &mut fixture,
            &work,
            reordered_list.clone(),
            vec![bound(2, VerificationKind::Test)],
            "reorder",
            12,
        );
        assert_eq!(reordered.acceptance, reordered_list);
        assert_eq!(reordered.revision, work.revision + 1);

        let after = page(&fixture.store, &host, &reordered);
        assert_eq!(after.basis.work_revision, reordered.revision);
        assert_eq!(row(&after, 1).binding, None);
        assert_eq!(row(&after, 3).binding, None);
        let moved = row(&after, 2).binding.as_ref().expect("the moved binding");
        assert_eq!(
            moved.requirement,
            bound(2, VerificationKind::Test).requirement
        );
        let reopened = moved.obligation.as_ref().expect("the reopened obligation");
        assert_ne!(reopened.obligation_id, original.obligation_id);
        assert_eq!(reopened.state, WorkObligationState::Open);
        assert_eq!(reopened.work_revision, reordered.revision);
        assert_eq!(reopened.rule, crate::control::acceptance_binding_rule(2));
        assert_eq!(reopened.resolution, None);

        let moved_candidates = first(&fixture.store, &host, &reordered, 2);
        assert_eq!(moved_candidates.basis.work_revision, reordered.revision);
        assert_eq!(moved_candidates.criterion, 2);
        assert_eq!(
            moved_candidates.requirement,
            Some(bound(2, VerificationKind::Test).requirement)
        );
        let recorded: Vec<&ObjectId> = moved_candidates
            .rows
            .iter()
            .map(|row| &row.record)
            .collect();
        assert_eq!(recorded, vec![&passed]);
        let vacated = first(&fixture.store, &host, &reordered, 1);
        assert_eq!(vacated.requirement, None);
        assert!(vacated.rows.is_empty(), "{vacated:?}");

        assert_eq!(
            super::refused(read(&fixture.store, &host, &work, None)),
            crate::domain::AcceptanceBindingReadRefusal::WrongRevision
        );
        let head = cut(&fixture.store, &reordered);
        assert_eq!(
            refused(read_at(&fixture.store, &host, &work, head, 2, None)),
            Refusal::WrongRevision
        );
    }
}
