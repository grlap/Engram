//! A characterization of how a named root's source is assessed, at
//! admission and at consumption, over every report sequence the design
//! names. Each cell states the full outcome as it stands, so moving the
//! assessment into one owner must leave every cell unchanged.

use super::*;
use std::fmt::Write as _;

const ROOT: &str = "workspace-B";
const GENERATION: i64 = 9;
const SIGHTED: &str = "R1";
const OTHER: &str = "R2";

/// Where a report after the cut sights the source.
#[derive(Clone, Copy, Debug)]
enum At {
    /// The named root, with its generation and named state.
    Root,
    /// Another workspace the claim never named.
    Foreign,
    /// The root's workspace with no generation or state stated.
    Unstated,
    /// No source basis at all: a change the host could not place.
    Unknown,
}

#[derive(Clone, Copy, Debug)]
struct Report {
    at: At,
    revision: &'static str,
    flagged: bool,
}

const fn quiet(at: At, revision: &'static str) -> Report {
    Report {
        at,
        revision,
        flagged: false,
    }
}

const fn flagged(at: At, revision: &'static str) -> Report {
    Report {
        at,
        revision,
        flagged: true,
    }
}

impl Report {
    fn input(self, host: &HostSession, index: usize, second: i64) -> ExecutionObservationInput {
        let basis = match self.at {
            At::Root => Some(workspace(ROOT, self.revision, Some(GENERATION))),
            At::Foreign => Some(workspace("workspace-A", self.revision, None)),
            At::Unstated => Some(workspace(ROOT, self.revision, None)),
            At::Unknown => None,
        };
        ExecutionObservationInput {
            observation_id: host.key(&format!("report-{index}")),
            action_fingerprint: ObjectId::from_canonical_bytes(
                host.key(&format!("report action {index}")).as_bytes(),
            ),
            effect: EffectClass::MutateLocal,
            outcome: ExecutionOutcome::Succeeded,
            source_changed: self.flagged,
            reported_source_change: None,
            observed_at: basis.as_ref().map(|_| at(second + 1)),
            source_basis: basis,
        }
    }
}

/// One host turn reporting `reports` in order, with the checkpoint's own
/// answer: `Err` names a refused checkpoint.
fn report_turn(
    host: &mut HostSession,
    store: &mut SqliteStore,
    reports: &[Report],
    second: i64,
) -> Result<(), String> {
    let grant = host.grant(store, &[EffectClass::MutateLocal], true, second);
    host.begin(store, &grant, second + 1);
    let observations: Vec<_> = reports
        .iter()
        .enumerate()
        .map(|(index, report)| report.input(host, index, second))
        .collect();
    match store.checkpoint_control_turn_with_evidence(
        &host.project_id,
        &host.session_id,
        &host.connection_token,
        &host.routing_token,
        &grant.grant_id,
        TurnNextIntent::Continue,
        &observations,
        &[],
        &[],
        &host.key("checkpoint"),
        at(second + 2),
    ) {
        Ok(ControlTurnCheckpointDecision::Checkpointed { .. }) => Ok(()),
        Ok(other) => Err(format!("checkpoint {other:?}")),
        Err(error) => Err(format!("checkpoint refused: {error}")),
    }
}

/// One host act after the cut, each in its own turn.
#[derive(Clone, Copy, Debug)]
enum Act {
    Report(Report),
    /// A turn that reports a check of `revision` at `at`, with a change to it
    /// when `changed`.
    Check {
        at: At,
        revision: &'static str,
        changed: bool,
        outcome: ExecutionOutcome,
    },
    /// The host names the root again, at the next generation.
    Rename,
    /// The host ends the named root's bound generation.
    End,
}

/// What an evaluation declares it judged.
#[derive(Clone, Copy, Debug)]
struct Declared {
    revision: Option<&'static str>,
    workspace: Option<&'static str>,
}

/// Whether the root was sighted through the cut.
#[derive(Clone, Copy, Debug)]
enum Start {
    Sighted,
    Unsighted,
}

/// How the reports are delivered: all in one host turn, or one turn each.
#[derive(Clone, Copy, Debug)]
enum Turns {
    One,
    Each,
}

/// The run-feed positions each report's turn added, to name a deciding
/// observation by the report that made it.
struct Delivered {
    spans: Vec<(i64, i64)>,
    refused: Option<String>,
}

impl Delivered {
    fn name(&self, position: i64) -> String {
        self.spans
            .iter()
            .position(|(from, to)| position > *from && position <= *to)
            .map_or_else(
                || format!("position {position}"),
                |turn| format!("turn {turn}"),
            )
    }
}

fn deliver(
    host: &mut HostSession,
    store: &mut SqliteStore,
    work: &WorkItem,
    reports: &[Report],
    turns: Turns,
    second: i64,
) -> Delivered {
    let mut delivered = Delivered {
        spans: Vec::new(),
        refused: None,
    };
    let batches: Vec<&[Report]> = match turns {
        Turns::One if reports.is_empty() => Vec::new(),
        Turns::One => vec![reports],
        Turns::Each => reports.chunks(1).collect(),
    };
    for (index, batch) in batches.into_iter().enumerate() {
        let before = cut(store, work);
        if let Err(refused) = report_turn(
            host,
            store,
            batch,
            second + 10 * i64::try_from(index).expect("few turns"),
        ) {
            delivered.refused = Some(refused);
            return delivered;
        }
        delivered.spans.push((before, cut(store, work)));
    }
    delivered
}

fn deliver_acts(
    host: &mut HostSession,
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    acts: &[Act],
    second: i64,
) -> Delivered {
    let mut delivered = Delivered {
        spans: Vec::new(),
        refused: None,
    };
    for (index, act) in acts.iter().enumerate() {
        let second = second + 10 * i64::try_from(index).expect("few turns");
        let before = cut(store, work);
        let outcome = match *act {
            Act::Report(report) => report_turn(host, store, &[report], second),
            Act::Check {
                at,
                revision,
                changed,
                outcome,
            } => {
                host.basis = match at {
                    At::Root => workspace(ROOT, revision, Some(GENERATION)),
                    At::Foreign => workspace("workspace-A", revision, None),
                    At::Unstated => workspace(ROOT, revision, None),
                    At::Unknown => panic!("a check has a source"),
                };
                host.checkpoint_checks(
                    store,
                    changed,
                    &[(VerificationKind::Test, outcome)],
                    second,
                );
                Ok(())
            }
            Act::Rename | Act::End => {
                let key = host.key("rebind");
                host_binds(
                    store,
                    host,
                    claim,
                    ROOT,
                    // A rename names the next generation; an end ends the
                    // one bound.
                    if matches!(act, Act::Rename) {
                        GENERATION + 1
                    } else {
                        GENERATION
                    },
                    if matches!(act, Act::Rename) {
                        NamedRootBindingKind::Bound
                    } else {
                        NamedRootBindingKind::Ended
                    },
                    // An end repeats the bound generation's named time,
                    // the second the fixture named it.
                    if matches!(act, Act::Rename) {
                        second
                    } else {
                        20
                    },
                    &key,
                    second,
                )
                .map(|_| ())
                .map_err(|error| format!("binding refused: {error}"))
            }
        };
        if let Err(refused) = outcome {
            delivered.refused = Some(refused);
            return delivered;
        }
        delivered.spans.push((before, cut(store, work)));
    }
    delivered
}

fn evaluation(
    work: &WorkItem,
    note: &ObjectId,
    through: i64,
    declared: Declared,
    key: &str,
    second: i64,
) -> RecordAcceptanceEvaluationRequest {
    let mut input = declared_pass(
        work,
        note,
        through,
        declared.revision.unwrap_or(SIGHTED),
        key,
        second,
    );
    input.source_basis = declared.revision.map(|revision| AcceptanceSourceBasis {
        workspace_id: declared.workspace.map(Into::into),
        fingerprint: revision.into(),
    });
    input
}

/// The named root at `workspace-B`, generation 9, not yet sighted.
fn unsighted_root(name: &str) -> (Fixture, WorkItem, WorkClaim, HostSession) {
    let mut fixture = fixture(name);
    let (work, claim) = (fixture.work.clone(), fixture.claim.clone());
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    let host = HostSession::bind(store, &work, &claim, 6);
    host_binds(
        store,
        &host,
        &claim,
        ROOT,
        GENERATION,
        NamedRootBindingKind::Bound,
        20,
        "name-B",
        20,
    )
    .expect("host names B");
    (fixture, work, claim, host)
}

/// The stale status as completion reads it, in words.
fn status(store: &SqliteStore, work: &WorkItem, delivered: &Delivered) -> String {
    let status = store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read")
        .expect("an evaluation");
    let mut words = match status.stale {
        None => "fresh".to_owned(),
        Some(reason) => format!("stale {}", reason.word()),
    };
    if let Some(observation) = &status.stale_observation {
        let _ = write!(
            words,
            " by {} {} at {}",
            delivered.name(observation.position),
            if observation.source_changed {
                "change"
            } else {
                "sighting"
            },
            observation.revision.as_deref().unwrap_or("-"),
        );
    }
    if let Some(source) = &status.source_recovery {
        let _ = write!(
            words,
            " ({:?}, reported {})",
            source.mismatch,
            source.reported_revision.as_deref().unwrap_or("-")
        );
    }
    words
}

/// A submission's outcome, in words.
fn admission(
    result: Result<AcceptanceEvaluationReceipt, StoreError>,
    delivered: &Delivered,
) -> Result<(), String> {
    match result {
        Ok(_) => Ok(()),
        Err(StoreError::AcceptanceEvaluationBasisMoved {
            moved, observation, ..
        }) => Err(format!(
            "moved {moved:?}{}",
            observation.map_or_else(String::new, |observation| format!(
                " by {} at {}",
                delivered.name(observation.position),
                observation.revision.as_deref().unwrap_or("-")
            ))
        )),
        Err(StoreError::AcceptanceEvaluationAdmissionRefused { cause, .. }) => match *cause {
            crate::domain::AcceptanceEvaluationAdmissionCause::SourceRoot(root) => Err(format!(
                "root {:?}, reported {}",
                root.mismatch,
                root.reported_revision.as_deref().unwrap_or("-")
            )),
            other => Err(format!("refused {other:?}")),
        },
        Err(other) => Err(format!("error {other}")),
    }
}

/// The two ways a cell is observed: an evaluation recorded at the cut before
/// the reports, read by completion after them; and one submitted on that
/// same cut after the reports. Plus, for a declaration, what the owner being
/// replaced answered, beside the movement scan.
#[derive(Debug, PartialEq)]
struct Outcome {
    early: String,
    late: String,
    probe: Option<(bool, bool)>,
}

/// The acts after the cut: reports grouped into turns, or acts one turn each.
#[derive(Clone, Copy, Debug)]
enum Input<'a> {
    Reports(&'a [Report], Turns),
    Acts(&'a [Act]),
}

fn apply(
    host: &mut HostSession,
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    input: Input<'_>,
    second: i64,
) -> Delivered {
    match input {
        Input::Reports(reports, turns) => deliver(host, store, work, reports, turns, second),
        Input::Acts(acts) => deliver_acts(host, store, work, claim, acts, second),
    }
}

fn start(start: Start, name: &str) -> (Fixture, WorkItem, WorkClaim, HostSession) {
    match start {
        Start::Sighted => sighted_root(name),
        Start::Unsighted => unsighted_root(name),
    }
}

fn observe(name: &str, begin: Start, declared: Declared, input: Input<'_>) -> Outcome {
    let nothing = Delivered {
        spans: Vec::new(),
        refused: None,
    };
    // Recorded first, read after the acts.
    let (mut fixture, work, claim, mut host) = start(begin, &format!("early-{name}"));
    let note = fixture.evidence.clone();
    let store = &mut fixture.store;
    let started_at = cut(store, &work);
    let recorded = admission(
        record(
            store,
            &evaluation(&work, &note, started_at, declared, "early", 40),
        ),
        &nothing,
    );
    let early = if let Err(refused) = recorded {
        format!("not recorded: {refused}")
    } else {
        let delivered = apply(&mut host, store, &work, &claim, input, 100);
        match &delivered.refused {
            Some(refused) => refused.clone(),
            None => status(store, &work, &delivered),
        }
    };

    // Submitted after the acts, on the earlier cut.
    let (mut fixture, work, claim, mut host) = start(begin, &format!("late-{name}"));
    let note = fixture.evidence.clone();
    let store = &mut fixture.store;
    let started_at = cut(store, &work);
    let delivered = apply(&mut host, store, &work, &claim, input, 100);
    let late = match &delivered.refused {
        Some(refused) => refused.clone(),
        None => match admission(
            record(
                store,
                &evaluation(&work, &note, started_at, declared, "late", 400),
            ),
            &delivered,
        ) {
            Ok(()) => format!("recorded, {}", status(store, &work, &delivered)),
            Err(refused) => refused,
        },
    };
    let connection = &store.connection;
    let root = named_root_at_on(connection, claim.run_id, started_at).expect("root read");
    let probe = match (declared.revision, &delivered.refused, root, begin) {
        (Some(revision), None, Some(root), Start::Sighted) => {
            let open =
                declared_not_contradicted(connection, claim.run_id, started_at, &root, revision)
                    .expect("declaration read");
            let basis = AcceptanceSourceBasis {
                workspace_id: declared.workspace.map(Into::into),
                fingerprint: revision.into(),
            };
            let moved = basis_moved_after(
                connection,
                claim.run_id,
                started_at,
                Some(&basis),
                Some(&root),
            )
            .expect("movement read")
            .is_some();
            Some((open, moved))
        }
        _ => None,
    };
    Outcome { early, late, probe }
}

/// The removal proof the design asks for: no cell in which admission would
/// refuse only because the declaration looked contradicted, while the
/// movement scan found nothing and the root was sighted through the cut.
fn assert_declaration_owner_is_covered(name: &str, outcome: &Outcome) {
    if let Some((open, moved)) = outcome.probe {
        assert!(
            open || moved,
            "{name}: the declaration reads contradicted with no movement; the late outcome was {}",
            outcome.late
        );
    }
}

use At::{Foreign, Root, Unknown, Unstated};

const A: &str = SIGHTED;
const B: &str = OTHER;

const fn declared(revision: &'static str) -> Declared {
    Declared {
        revision: Some(revision),
        workspace: None,
    }
}

const UNDECLARED: Declared = Declared {
    revision: None,
    workspace: None,
};

const DECLARATIONS: [(&str, Declared); 3] =
    [("dA", declared(A)), ("uA", UNDECLARED), ("dB", declared(B))];

/// One observed cell: its name, where it starts, what it declares and what
/// happens after its cut.
type CellSpec<'a> = (String, Start, Declared, Input<'a>);

/// Where a family hands its cells: to the comparison, or to the check that
/// every table row is used exactly once.
type Visit<'v> = &'v mut dyn FnMut(&[CellSpec<'_>]);

/// Every declaration against every report sequence, from a sighted root.
fn report_cells<'a>(sequences: &[(&'a str, &'a [Report], Turns)]) -> Vec<CellSpec<'a>> {
    sequences
        .iter()
        .flat_map(|(sequence, reports, turns)| {
            DECLARATIONS.iter().map(move |(label, declared)| {
                (
                    format!("{label}-{sequence}"),
                    Start::Sighted,
                    *declared,
                    Input::Reports(reports, *turns),
                )
            })
        })
        .collect()
}

/// Observes each cell and compares it with its one row in `expected`, a
/// list of `(cell, early, late)`. A missing or differing cell fails,
/// printing the measured row to paste.
fn compare(cells: &[CellSpec<'_>], expected: &[(&str, &str, &str)]) {
    let mut differing = Vec::new();
    for (name, begin, declared, input) in cells {
        let outcome = observe(name, *begin, *declared, *input);
        assert_declaration_owner_is_covered(name, &outcome);
        let row = expected.iter().find(|(cell, _, _)| cell == name);
        if row.is_none_or(|(_, early, late)| *early != outcome.early || *late != outcome.late) {
            differing.push(format!(
                "        (\"{name}\", \"{}\", \"{}\"),",
                outcome.early, outcome.late
            ));
        }
    }
    assert!(
        differing.is_empty(),
        "{} cells differ from the table; measured:\n{}",
        differing.len(),
        differing.join("\n")
    );
}

fn one<'a>(name: &'a str, reports: &'a [Report]) -> (&'a str, &'a [Report], Turns) {
    (name, reports, Turns::One)
}

fn each<'a>(name: &'a str, reports: &'a [Report]) -> (&'a str, &'a [Report], Turns) {
    (name, reports, Turns::Each)
}

// B: reports inside the root after the cut, singly and in pairs.
fn inside_the_root(visit: Visit<'_>) {
    visit(&report_cells(&[
        each("none", &[]),
        each("qA", &[quiet(Root, A)]),
        each("qB", &[quiet(Root, B)]),
        each("fA", &[flagged(Root, A)]),
        each("fB", &[flagged(Root, B)]),
        each("qB.qA", &[quiet(Root, B), quiet(Root, A)]),
        one("qB+qA", &[quiet(Root, B), quiet(Root, A)]),
        each("qA.qB", &[quiet(Root, A), quiet(Root, B)]),
        one("qA+qB", &[quiet(Root, A), quiet(Root, B)]),
        each("fB.fA", &[flagged(Root, B), flagged(Root, A)]),
        one("fB+fA", &[flagged(Root, B), flagged(Root, A)]),
        each("fA.fB", &[flagged(Root, A), flagged(Root, B)]),
        one("fA+fB", &[flagged(Root, A), flagged(Root, B)]),
        each("fB.qA", &[flagged(Root, B), quiet(Root, A)]),
        one("fB+qA", &[flagged(Root, B), quiet(Root, A)]),
        each("qB.fA", &[quiet(Root, B), flagged(Root, A)]),
        one("qB+fA", &[quiet(Root, B), flagged(Root, A)]),
    ]));
}

#[test]
fn reports_inside_the_root_after_the_cut() {
    inside_the_root(&mut |cells| compare(cells, MEASURED));
}

// B: A to B to A and B to A to B, with mixed quiet and flagged legs.
fn round_trips(visit: Visit<'_>) {
    visit(&report_cells(&[
        each(
            "fA.fB.fA",
            &[flagged(Root, A), flagged(Root, B), flagged(Root, A)],
        ),
        one(
            "fA+fB+fA",
            &[flagged(Root, A), flagged(Root, B), flagged(Root, A)],
        ),
        each(
            "qA.fB.qA",
            &[quiet(Root, A), flagged(Root, B), quiet(Root, A)],
        ),
        one(
            "qA+fB+qA",
            &[quiet(Root, A), flagged(Root, B), quiet(Root, A)],
        ),
        each(
            "fA.qB.fA",
            &[flagged(Root, A), quiet(Root, B), flagged(Root, A)],
        ),
        one(
            "fA+qB+fA",
            &[flagged(Root, A), quiet(Root, B), flagged(Root, A)],
        ),
        each(
            "fB.fA.fB",
            &[flagged(Root, B), flagged(Root, A), flagged(Root, B)],
        ),
        one(
            "fB+fA+fB",
            &[flagged(Root, B), flagged(Root, A), flagged(Root, B)],
        ),
        each(
            "qB.qA.qB",
            &[quiet(Root, B), quiet(Root, A), quiet(Root, B)],
        ),
        one(
            "qB+qA+qB",
            &[quiet(Root, B), quiet(Root, A), quiet(Root, B)],
        ),
        each(
            "fB.qA.fB",
            &[flagged(Root, B), quiet(Root, A), flagged(Root, B)],
        ),
        one(
            "fB+qA+fB",
            &[flagged(Root, B), quiet(Root, A), flagged(Root, B)],
        ),
    ]));
}

#[test]
fn round_trips_inside_the_root_after_the_cut() {
    round_trips(&mut |cells| compare(cells, MEASURED));
}

// C: reports outside the root, or that cannot be placed, alone and
// interleaved with the root's own reports.
fn outside_the_root(visit: Visit<'_>) {
    visit(&report_cells(&[
        each("Fq.A", &[quiet(Foreign, A)]),
        each("Fq.B", &[quiet(Foreign, B)]),
        each("Ff.A", &[flagged(Foreign, A)]),
        each("Ff.B", &[flagged(Foreign, B)]),
        each("Sq.A", &[quiet(Unstated, A)]),
        each("Sq.B", &[quiet(Unstated, B)]),
        each("Sf.A", &[flagged(Unstated, A)]),
        each("Sf.B", &[flagged(Unstated, B)]),
        each("Uq", &[quiet(Unknown, A)]),
        each("Uf", &[flagged(Unknown, A)]),
        each(
            "FqB.fB.FqA",
            &[quiet(Foreign, B), flagged(Root, B), quiet(Foreign, A)],
        ),
        one(
            "FqB+fB+FqA",
            &[quiet(Foreign, B), flagged(Root, B), quiet(Foreign, A)],
        ),
        each(
            "fB.FfA.fA",
            &[flagged(Root, B), flagged(Foreign, A), flagged(Root, A)],
        ),
        one(
            "fB+FfA+fA",
            &[flagged(Root, B), flagged(Foreign, A), flagged(Root, A)],
        ),
        each("FfB.qA", &[flagged(Foreign, B), quiet(Root, A)]),
        each("Uf.qA", &[flagged(Unknown, A), quiet(Root, A)]),
    ]));
}

#[test]
fn reports_outside_the_root_after_the_cut() {
    outside_the_root(&mut |cells| compare(cells, MEASURED));
}

/// Every declaration in `declarations` against every act sequence, from
/// `begin`.
fn acts_cells<'a>(
    family: &str,
    begin: Start,
    declarations: &[(&str, Declared)],
    sequences: &[(&str, &'a [Act])],
) -> Vec<CellSpec<'a>> {
    sequences
        .iter()
        .flat_map(|(sequence, acts)| {
            declarations.iter().map(move |(label, declared)| {
                (
                    format!("{family}:{label}-{sequence}"),
                    begin,
                    *declared,
                    Input::Acts(acts),
                )
            })
        })
        .collect()
}

const fn report(report: Report) -> Act {
    Act::Report(report)
}

const fn check(at: At, revision: &'static str, changed: bool, outcome: ExecutionOutcome) -> Act {
    Act::Check {
        at,
        revision,
        changed,
        outcome,
    }
}

const fn declared_in(revision: &'static str, workspace: &'static str) -> Declared {
    Declared {
        revision: Some(revision),
        workspace: Some(workspace),
    }
}

// A: the root was not sighted through the cut. No declaration stands in for
// that first sighting, even when a matching report arrives later.
fn unsighted_cells(visit: Visit<'_>) {
    visit(&acts_cells(
        "A",
        Start::Unsighted,
        &[
            ("dA", declared(A)),
            ("uA", UNDECLARED),
            ("dB", declared(B)),
            ("dBwB", declared_in(B, ROOT)),
        ],
        &[
            ("none", &[]),
            ("fB", &[report(flagged(Root, B))]),
            ("qB", &[report(quiet(Root, B))]),
        ],
    ));
}

#[test]
fn a_root_not_sighted_through_the_cut() {
    unsighted_cells(&mut |cells| compare(cells, MEASURED));
}

// D: checks after the cut, passed, failed or indeterminate, inside the root,
// outside it, or with a later sighting that contradicts the declaration.
fn checks(visit: Visit<'_>) {
    use ExecutionOutcome::{Failed, Succeeded, Unknown as Indeterminate};
    visit(&acts_cells(
        "D",
        Start::Sighted,
        &DECLARATIONS,
        &[
            ("cB+pass", &[check(Root, B, true, Succeeded)]),
            ("cB+fail", &[check(Root, B, true, Failed)]),
            ("cB+unknown", &[check(Root, B, true, Indeterminate)]),
            ("cA.pass", &[check(Root, A, false, Succeeded)]),
            ("cA.fail", &[check(Root, A, false, Failed)]),
            ("FcB+pass", &[check(Foreign, B, true, Succeeded)]),
            ("FcA.pass", &[check(Foreign, A, false, Succeeded)]),
            ("ScA.pass", &[check(Unstated, A, false, Succeeded)]),
            (
                "cB+pass.qA",
                &[check(Root, B, true, Succeeded), report(quiet(Root, A))],
            ),
            (
                "fB.cA.pass",
                &[report(flagged(Root, B)), check(Root, A, false, Succeeded)],
            ),
        ],
    ));
}

#[test]
fn checks_after_the_cut() {
    checks(&mut |cells| compare(cells, MEASURED));
}

// E: the declared workspace, and the root's binding renamed or ended after
// the cut, alone and after a report inside the root.
fn bindings(visit: Visit<'_>) {
    visit(&acts_cells(
        "E",
        Start::Sighted,
        &[
            ("dA", declared(A)),
            ("uA", UNDECLARED),
            ("dB", declared(B)),
            ("dAwB", declared_in(A, ROOT)),
            ("dBwB", declared_in(B, ROOT)),
            ("dAwA", declared_in(A, "workspace-A")),
            ("dBwA", declared_in(B, "workspace-A")),
        ],
        &[
            ("none", &[]),
            ("fB", &[report(flagged(Root, B))]),
            ("rename", &[Act::Rename]),
            ("end", &[Act::End]),
            ("fB.rename", &[report(flagged(Root, B)), Act::Rename]),
        ],
    ));
}

#[test]
fn bindings_and_declared_workspaces() {
    bindings(&mut |cells| compare(cells, MEASURED));
}

// An older passing evaluation never stands in for a newer declaration the
// root has not confirmed: the newest record reads stale, and done waits.
#[test]
fn a_newer_unconfirmed_declaration_hides_an_older_pass() {
    let (mut fixture, work, claim, _host) = sighted_root("project-root-no-fallback");
    let note = fixture.evidence.clone();
    let store = &mut fixture.store;
    let started_at = cut(store, &work);
    let older = record(
        store,
        &evaluation(&work, &note, started_at, declared(A), "older", 40),
    )
    .expect("the evaluation of the sighted revision records");
    assert_eq!(stale_reason(store, &work), None);
    let newer = record(
        store,
        &evaluation(&work, &note, started_at, declared(B), "newer", 50),
    )
    .expect("a declaration the root has not reported records");
    assert_ne!(newer.evaluation, older.evaluation);
    let status = store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read")
        .expect("an evaluation");
    assert_eq!(status.evaluation, newer.evaluation);
    assert_eq!(status.stale, Some(AcceptanceStaleReason::Source));
    assert!(matches!(
        recovery_cause(complete_evaluated(
            store, &work, &claim, "runner", &note, None, "blocked", 60
        )),
        WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
            reason: AcceptanceStaleReason::Source
        }
    ));
}

// Every row of the table is one cell of one family, and every cell has one
// row: a duplicate or an unused row would hide a change.
#[test]
fn every_measured_row_is_one_cell() {
    let mut names = Vec::new();
    let mut collect = |cells: &[CellSpec<'_>]| {
        names.extend(cells.iter().map(|(name, ..)| name.clone()));
    };
    for family in [
        inside_the_root,
        round_trips,
        outside_the_root,
        unsighted_cells,
        checks,
        bindings,
    ] {
        family(&mut collect);
    }
    let mut cells = names.clone();
    cells.sort();
    cells.dedup();
    assert_eq!(cells.len(), names.len(), "a cell name repeats");
    let mut rows: Vec<String> = MEASURED
        .iter()
        .map(|(name, ..)| (*name).to_owned())
        .collect();
    rows.sort();
    let unique = {
        let mut unique = rows.clone();
        unique.dedup();
        unique
    };
    assert_eq!(unique.len(), rows.len(), "a table row repeats");
    assert_eq!(rows, cells, "the table and the cells differ");
}

/// Every cell as the code before the move answered it.
const MEASURED: &[(&str, &str, &str)] = &[
    (
        "dA-fA.fB.fA",
        "stale mutation by turn 1 change at R2",
        "moved SourceChanged by turn 1 at R2",
    ),
    (
        "uA-fA.fB.fA",
        "stale mutation by turn 1 change at R2",
        "moved SourceChanged by turn 1 at R2",
    ),
    (
        "dB-fA.fB.fA",
        "stale mutation by turn 2 change at R1",
        "moved SourceChanged by turn 2 at R1",
    ),
    (
        "dA-fA+fB+fA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fA+fB+fA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "dB-fA+fB+fA",
        "stale mutation by turn 0 change at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    (
        "dA-qA.fB.qA",
        "stale mutation by turn 1 change at R2",
        "moved SourceChanged by turn 1 at R2",
    ),
    (
        "uA-qA.fB.qA",
        "stale mutation by turn 1 change at R2",
        "moved SourceChanged by turn 1 at R2",
    ),
    (
        "dB-qA.fB.qA",
        "stale mutation by turn 2 sighting at R1",
        "moved SourceChanged by turn 2 at R1",
    ),
    (
        "dA-qA+fB+qA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-qA+fB+qA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "dB-qA+fB+qA",
        "stale mutation by turn 0 sighting at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    ("dA-fA.qB.fA", "fresh", "recorded, fresh"),
    (
        "uA-fA.qB.fA",
        "stale mutation by turn 2 change at R1",
        "moved SourceChanged by turn 2 at R1",
    ),
    (
        "dB-fA.qB.fA",
        "stale mutation by turn 2 change at R1",
        "moved SourceChanged by turn 2 at R1",
    ),
    ("dA-fA+qB+fA", "fresh", "recorded, fresh"),
    (
        "uA-fA+qB+fA",
        "stale mutation by turn 0 change at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    (
        "dB-fA+qB+fA",
        "stale mutation by turn 0 change at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    (
        "dA-fB.fA.fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fB.fA.fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "dB-fB.fA.fB",
        "stale mutation by turn 1 change at R1",
        "moved SourceChanged by turn 1 at R1",
    ),
    (
        "dA-fB+fA+fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fB+fA+fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "dB-fB+fA+fB",
        "stale mutation by turn 0 change at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    (
        "dA-qB.qA.qB",
        "stale mutation by turn 2 sighting at R2",
        "moved SourceChanged by turn 2 at R2",
    ),
    (
        "uA-qB.qA.qB",
        "stale mutation by turn 2 sighting at R2",
        "moved SourceChanged by turn 2 at R2",
    ),
    ("dB-qB.qA.qB", "fresh", "recorded, fresh"),
    (
        "dA-qB+qA+qB",
        "stale mutation by turn 0 sighting at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-qB+qA+qB",
        "stale mutation by turn 0 sighting at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("dB-qB+qA+qB", "fresh", "recorded, fresh"),
    (
        "dA-fB.qA.fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fB.qA.fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("dB-fB.qA.fB", "fresh", "recorded, fresh"),
    (
        "dA-fB+qA+fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fB+qA+fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("dB-fB+qA+fB", "fresh", "recorded, fresh"),
    ("dA-Fq.A", "fresh", "recorded, fresh"),
    ("uA-Fq.A", "fresh", "recorded, fresh"),
    (
        "dB-Fq.A",
        "stale source (UnconfirmedDeclaration, reported R1)",
        "recorded, stale source (UnconfirmedDeclaration, reported R1)",
    ),
    ("dA-Fq.B", "fresh", "recorded, fresh"),
    ("uA-Fq.B", "fresh", "recorded, fresh"),
    (
        "dB-Fq.B",
        "stale source (UnconfirmedDeclaration, reported R1)",
        "recorded, stale source (UnconfirmedDeclaration, reported R1)",
    ),
    ("dA-Ff.A", "stale mutation", "moved CheckRecorded"),
    ("uA-Ff.A", "stale mutation", "moved CheckRecorded"),
    ("dB-Ff.A", "stale mutation", "moved CheckRecorded"),
    ("dA-Ff.B", "stale mutation", "moved CheckRecorded"),
    ("uA-Ff.B", "stale mutation", "moved CheckRecorded"),
    ("dB-Ff.B", "stale mutation", "moved CheckRecorded"),
    ("dA-Sq.A", "fresh", "recorded, fresh"),
    ("uA-Sq.A", "fresh", "recorded, fresh"),
    (
        "dB-Sq.A",
        "stale source (UnconfirmedDeclaration, reported R1)",
        "recorded, stale source (UnconfirmedDeclaration, reported R1)",
    ),
    ("dA-Sq.B", "fresh", "recorded, fresh"),
    ("uA-Sq.B", "fresh", "recorded, fresh"),
    (
        "dB-Sq.B",
        "stale source (UnconfirmedDeclaration, reported R1)",
        "recorded, stale source (UnconfirmedDeclaration, reported R1)",
    ),
    ("dA-Sf.A", "fresh", "recorded, fresh"),
    ("uA-Sf.A", "fresh", "recorded, fresh"),
    (
        "dB-Sf.A",
        "stale source (UnconfirmedDeclaration, reported R1)",
        "recorded, stale source (UnconfirmedDeclaration, reported R1)",
    ),
    ("dA-Sf.B", "stale mutation", "moved CheckRecorded"),
    ("uA-Sf.B", "stale mutation", "moved CheckRecorded"),
    ("dB-Sf.B", "stale mutation", "moved CheckRecorded"),
    ("dA-Uq", "fresh", "recorded, fresh"),
    ("uA-Uq", "fresh", "recorded, fresh"),
    (
        "dB-Uq",
        "stale source (UnconfirmedDeclaration, reported R1)",
        "recorded, stale source (UnconfirmedDeclaration, reported R1)",
    ),
    (
        "dA-Uf",
        "stale mutation by turn 0 change at -",
        "moved SourceChanged by turn 0 at -",
    ),
    (
        "uA-Uf",
        "stale mutation by turn 0 change at -",
        "moved SourceChanged by turn 0 at -",
    ),
    (
        "dB-Uf",
        "stale mutation by turn 0 change at -",
        "moved SourceChanged by turn 0 at -",
    ),
    (
        "dA-FqB.fB.FqA",
        "stale mutation by turn 1 change at R2",
        "moved SourceChanged by turn 1 at R2",
    ),
    (
        "uA-FqB.fB.FqA",
        "stale mutation by turn 1 change at R2",
        "moved SourceChanged by turn 1 at R2",
    ),
    ("dB-FqB.fB.FqA", "fresh", "recorded, fresh"),
    (
        "dA-FqB+fB+FqA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-FqB+fB+FqA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("dB-FqB+fB+FqA", "fresh", "recorded, fresh"),
    (
        "dA-fB.FfA.fA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fB.FfA.fA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "dB-fB.FfA.fA",
        "stale mutation by turn 2 change at R1",
        "moved SourceChanged by turn 2 at R1",
    ),
    (
        "dA-fB+FfA+fA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fB+FfA+fA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "dB-fB+FfA+fA",
        "stale mutation by turn 0 change at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    ("dA-FfB.qA", "stale mutation", "moved CheckRecorded"),
    ("uA-FfB.qA", "stale mutation", "moved CheckRecorded"),
    (
        "dB-FfB.qA",
        "stale mutation by turn 1 sighting at R1",
        "moved SourceChanged by turn 1 at R1",
    ),
    (
        "dA-Uf.qA",
        "stale mutation by turn 0 change at -",
        "moved SourceChanged by turn 0 at -",
    ),
    (
        "uA-Uf.qA",
        "stale mutation by turn 0 change at -",
        "moved SourceChanged by turn 0 at -",
    ),
    (
        "dB-Uf.qA",
        "stale mutation by turn 0 change at -",
        "moved SourceChanged by turn 0 at -",
    ),
    ("dA-none", "fresh", "recorded, fresh"),
    ("uA-none", "fresh", "recorded, fresh"),
    (
        "dB-none",
        "stale source (UnconfirmedDeclaration, reported R1)",
        "recorded, stale source (UnconfirmedDeclaration, reported R1)",
    ),
    ("dA-qA", "fresh", "recorded, fresh"),
    ("uA-qA", "fresh", "recorded, fresh"),
    (
        "dB-qA",
        "stale mutation by turn 0 sighting at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    (
        "dA-qB",
        "stale mutation by turn 0 sighting at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-qB",
        "stale mutation by turn 0 sighting at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("dB-qB", "fresh", "recorded, fresh"),
    ("dA-fA", "fresh", "recorded, fresh"),
    ("uA-fA", "fresh", "recorded, fresh"),
    (
        "dB-fA",
        "stale mutation by turn 0 sighting at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    (
        "dA-fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("dB-fB", "fresh", "recorded, fresh"),
    ("dA-qB.qA", "fresh", "recorded, fresh"),
    ("uA-qB.qA", "fresh", "recorded, fresh"),
    (
        "dB-qB.qA",
        "stale mutation by turn 1 sighting at R1",
        "moved SourceChanged by turn 1 at R1",
    ),
    ("dA-qB+qA", "fresh", "recorded, fresh"),
    ("uA-qB+qA", "fresh", "recorded, fresh"),
    (
        "dB-qB+qA",
        "stale mutation by turn 0 sighting at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    (
        "dA-qA.qB",
        "stale mutation by turn 1 sighting at R2",
        "moved SourceChanged by turn 1 at R2",
    ),
    (
        "uA-qA.qB",
        "stale mutation by turn 1 sighting at R2",
        "moved SourceChanged by turn 1 at R2",
    ),
    ("dB-qA.qB", "fresh", "recorded, fresh"),
    (
        "dA-qA+qB",
        "stale mutation by turn 0 sighting at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-qA+qB",
        "stale mutation by turn 0 sighting at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("dB-qA+qB", "fresh", "recorded, fresh"),
    (
        "dA-fB.fA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fB.fA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "dB-fB.fA",
        "stale mutation by turn 1 change at R1",
        "moved SourceChanged by turn 1 at R1",
    ),
    (
        "dA-fB+fA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fB+fA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "dB-fB+fA",
        "stale mutation by turn 0 change at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    (
        "dA-fA.fB",
        "stale mutation by turn 1 change at R2",
        "moved SourceChanged by turn 1 at R2",
    ),
    (
        "uA-fA.fB",
        "stale mutation by turn 1 change at R2",
        "moved SourceChanged by turn 1 at R2",
    ),
    ("dB-fA.fB", "fresh", "recorded, fresh"),
    (
        "dA-fA+fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fA+fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("dB-fA+fB", "fresh", "recorded, fresh"),
    (
        "dA-fB.qA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fB.qA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "dB-fB.qA",
        "stale mutation by turn 1 sighting at R1",
        "moved SourceChanged by turn 1 at R1",
    ),
    (
        "dA-fB+qA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "uA-fB+qA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "dB-fB+qA",
        "stale mutation by turn 0 sighting at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    ("dA-qB.fA", "fresh", "recorded, fresh"),
    (
        "uA-qB.fA",
        "stale mutation by turn 1 change at R1",
        "moved SourceChanged by turn 1 at R1",
    ),
    (
        "dB-qB.fA",
        "stale mutation by turn 1 change at R1",
        "moved SourceChanged by turn 1 at R1",
    ),
    ("dA-qB+fA", "fresh", "recorded, fresh"),
    (
        "uA-qB+fA",
        "stale mutation by turn 0 change at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    (
        "dB-qB+fA",
        "stale mutation by turn 0 change at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    (
        "A:dA-none",
        "not recorded: root NoInitialSighting, reported -",
        "root NoInitialSighting, reported -",
    ),
    (
        "A:uA-none",
        "not recorded: root NoInitialSighting, reported -",
        "root NoInitialSighting, reported -",
    ),
    (
        "A:dB-none",
        "not recorded: root NoInitialSighting, reported -",
        "root NoInitialSighting, reported -",
    ),
    (
        "A:dBwB-none",
        "not recorded: root NoInitialSighting, reported -",
        "root NoInitialSighting, reported -",
    ),
    (
        "A:dA-fB",
        "not recorded: root NoInitialSighting, reported -",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "A:uA-fB",
        "not recorded: root NoInitialSighting, reported -",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "A:dB-fB",
        "not recorded: root NoInitialSighting, reported -",
        "root NoInitialSighting, reported -",
    ),
    (
        "A:dBwB-fB",
        "not recorded: root NoInitialSighting, reported -",
        "root NoInitialSighting, reported -",
    ),
    (
        "A:dA-qB",
        "not recorded: root NoInitialSighting, reported -",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "A:uA-qB",
        "not recorded: root NoInitialSighting, reported -",
        "root NoInitialSighting, reported -",
    ),
    (
        "A:dB-qB",
        "not recorded: root NoInitialSighting, reported -",
        "root NoInitialSighting, reported -",
    ),
    (
        "A:dBwB-qB",
        "not recorded: root NoInitialSighting, reported -",
        "root NoInitialSighting, reported -",
    ),
    (
        "D:dA-cB+pass",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "D:uA-cB+pass",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("D:dB-cB+pass", "fresh", "recorded, fresh"),
    (
        "D:dA-cB+fail",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "D:uA-cB+fail",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("D:dB-cB+fail", "stale mutation", "moved CheckRecorded"),
    (
        "D:dA-cB+unknown",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "D:uA-cB+unknown",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("D:dB-cB+unknown", "stale mutation", "moved CheckRecorded"),
    ("D:dA-cA.pass", "fresh", "recorded, fresh"),
    ("D:uA-cA.pass", "stale mutation", "moved CheckRecorded"),
    (
        "D:dB-cA.pass",
        "stale mutation by turn 0 sighting at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    ("D:dA-cA.fail", "stale mutation", "moved CheckRecorded"),
    ("D:uA-cA.fail", "stale mutation", "moved CheckRecorded"),
    (
        "D:dB-cA.fail",
        "stale mutation by turn 0 sighting at R1",
        "moved SourceChanged by turn 0 at R1",
    ),
    ("D:dA-FcB+pass", "stale mutation", "moved CheckRecorded"),
    ("D:uA-FcB+pass", "stale mutation", "moved CheckRecorded"),
    ("D:dB-FcB+pass", "stale mutation", "moved CheckRecorded"),
    ("D:dA-FcA.pass", "stale mutation", "moved CheckRecorded"),
    ("D:uA-FcA.pass", "stale mutation", "moved CheckRecorded"),
    ("D:dB-FcA.pass", "stale mutation", "moved CheckRecorded"),
    ("D:dA-ScA.pass", "stale mutation", "moved CheckRecorded"),
    ("D:uA-ScA.pass", "stale mutation", "moved CheckRecorded"),
    ("D:dB-ScA.pass", "stale mutation", "moved CheckRecorded"),
    (
        "D:dA-cB+pass.qA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "D:uA-cB+pass.qA",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "D:dB-cB+pass.qA",
        "stale mutation by turn 1 sighting at R1",
        "moved SourceChanged by turn 1 at R1",
    ),
    (
        "D:dA-fB.cA.pass",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "D:uA-fB.cA.pass",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "D:dB-fB.cA.pass",
        "stale mutation by turn 1 sighting at R1",
        "moved SourceChanged by turn 1 at R1",
    ),
    ("E:dA-none", "fresh", "recorded, fresh"),
    ("E:uA-none", "fresh", "recorded, fresh"),
    (
        "E:dB-none",
        "stale source (UnconfirmedDeclaration, reported R1)",
        "recorded, stale source (UnconfirmedDeclaration, reported R1)",
    ),
    ("E:dAwB-none", "fresh", "recorded, fresh"),
    (
        "E:dBwB-none",
        "stale source (UnconfirmedDeclaration, reported R1)",
        "recorded, stale source (UnconfirmedDeclaration, reported R1)",
    ),
    (
        "E:dAwA-none",
        "not recorded: root DeclaredWorkspaceMismatch, reported -",
        "root DeclaredWorkspaceMismatch, reported -",
    ),
    (
        "E:dBwA-none",
        "not recorded: root DeclaredWorkspaceMismatch, reported -",
        "root DeclaredWorkspaceMismatch, reported -",
    ),
    (
        "E:dA-fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    (
        "E:uA-fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("E:dB-fB", "fresh", "recorded, fresh"),
    (
        "E:dAwB-fB",
        "stale mutation by turn 0 change at R2",
        "moved SourceChanged by turn 0 at R2",
    ),
    ("E:dBwB-fB", "fresh", "recorded, fresh"),
    (
        "E:dAwA-fB",
        "not recorded: root DeclaredWorkspaceMismatch, reported -",
        "root DeclaredWorkspaceMismatch, reported -",
    ),
    (
        "E:dBwA-fB",
        "not recorded: root DeclaredWorkspaceMismatch, reported -",
        "root DeclaredWorkspaceMismatch, reported -",
    ),
    ("E:dA-rename", "stale mutation", "moved SourceChanged"),
    ("E:uA-rename", "stale mutation", "moved SourceChanged"),
    ("E:dB-rename", "stale mutation", "moved SourceChanged"),
    ("E:dAwB-rename", "stale mutation", "moved SourceChanged"),
    ("E:dBwB-rename", "stale mutation", "moved SourceChanged"),
    (
        "E:dAwA-rename",
        "not recorded: root DeclaredWorkspaceMismatch, reported -",
        "root DeclaredWorkspaceMismatch, reported -",
    ),
    (
        "E:dBwA-rename",
        "not recorded: root DeclaredWorkspaceMismatch, reported -",
        "root DeclaredWorkspaceMismatch, reported -",
    ),
    ("E:dA-end", "stale mutation", "moved SourceChanged"),
    ("E:uA-end", "stale mutation", "moved SourceChanged"),
    ("E:dB-end", "stale mutation", "moved SourceChanged"),
    ("E:dAwB-end", "stale mutation", "moved SourceChanged"),
    ("E:dBwB-end", "stale mutation", "moved SourceChanged"),
    (
        "E:dAwA-end",
        "not recorded: root DeclaredWorkspaceMismatch, reported -",
        "root DeclaredWorkspaceMismatch, reported -",
    ),
    (
        "E:dBwA-end",
        "not recorded: root DeclaredWorkspaceMismatch, reported -",
        "root DeclaredWorkspaceMismatch, reported -",
    ),
    ("E:dA-fB.rename", "stale mutation", "moved SourceChanged"),
    ("E:uA-fB.rename", "stale mutation", "moved SourceChanged"),
    ("E:dB-fB.rename", "stale mutation", "moved SourceChanged"),
    ("E:dAwB-fB.rename", "stale mutation", "moved SourceChanged"),
    ("E:dBwB-fB.rename", "stale mutation", "moved SourceChanged"),
    (
        "E:dAwA-fB.rename",
        "not recorded: root DeclaredWorkspaceMismatch, reported -",
        "root DeclaredWorkspaceMismatch, reported -",
    ),
    (
        "E:dBwA-fB.rename",
        "not recorded: root DeclaredWorkspaceMismatch, reported -",
        "root DeclaredWorkspaceMismatch, reported -",
    ),
];
