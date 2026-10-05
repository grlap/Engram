//! The assessment summary's counts, over rows of every status and reason.

use super::{AssessmentView, assessment_page};
use crate::control::{ObligationAssessment, ObligationSkip};
use crate::domain::{
    BuiltinObligationRuleRef, ProjectId, VerificationEvidenceMismatch, VerificationKind, WorkId,
    WorkObligationId, WorkRunId,
};
use crate::storage::{
    RecordedObligationEnd, VerificationAssessment, VerificationObligationAssessment,
};
use std::str::FromStr;

fn row(
    trigger_position: i64,
    assessment: ObligationAssessment,
    recorded: RecordedObligationEnd,
) -> VerificationObligationAssessment {
    VerificationObligationAssessment {
        obligation_id: WorkObligationId::new(),
        rule: BuiltinObligationRuleRef {
            rule_id: "source_change_requires_test".into(),
            rule_version: 1,
        },
        check_kind: VerificationKind::Test,
        pinned: false,
        criterion: None,
        trigger_position,
        assessment,
        stale_source: None,
        recorded,
    }
}

// Every non-zero (status, reason) pair is named with its exact count, every
// left-out reason among them, foreign or displaced and another run
// included: no reason is folded into another or into a catch-all, and the
// counts cover every row. Another run cannot reach a record's assessment
// through the store, which selects candidates by the record's own run, so
// the rows are built here.
#[test]
fn summary_counts_name_every_status_and_reason_with_no_other_bucket() {
    use ObligationAssessment::{Matches, Mismatch, Skipped};
    use RecordedObligationEnd::{Displaced, Open, SatisfiedByAnotherRecord, Waived};
    let shape: &[(ObligationAssessment, RecordedObligationEnd, usize)] = &[
        (Skipped(ObligationSkip::ForeignOrDisplaced), Displaced, 3),
        (Skipped(ObligationSkip::ForeignOrDisplaced), Open, 1),
        (Skipped(ObligationSkip::OtherRun), Waived, 2),
        (Skipped(ObligationSkip::NoSourceContext), Waived, 1),
        (
            Skipped(ObligationSkip::NotYetDefined),
            SatisfiedByAnotherRecord,
            4,
        ),
        (
            Skipped(ObligationSkip::AlreadyClosed),
            SatisfiedByAnotherRecord,
            5,
        ),
        (
            Mismatch(VerificationEvidenceMismatch::StaleSourceRevision),
            Open,
            2,
        ),
        (Matches, Open, 1),
    ];
    let mut rows = Vec::new();
    for (assessment, recorded, count) in shape {
        for _ in 0..*count {
            let position = i64::try_from(rows.len()).expect("small") + 1;
            rows.push(row(position, *assessment, *recorded));
        }
    }
    let total = rows.len();
    let page = assessment_page(
        &ProjectId("record-windows".into()),
        WorkId::new(),
        &crate::ObjectId::from_str(&"a".repeat(32)).expect("record id"),
        AssessmentView::Summary,
        VerificationAssessment {
            run_id: WorkRunId::new(),
            record_position: 40,
            cut_position: 41,
            head_position: 41,
            check_kind: VerificationKind::Test,
            total,
            earlier: 0,
            boundary_found: true,
            rows,
            bound: None,
        },
        None,
    )
    .expect("summary page");

    let counts: Vec<(String, Option<String>, usize)> = page
        .counts
        .iter()
        .map(|count| (count.status.clone(), count.reason.clone(), count.count))
        .collect();
    let named = |status: &str, reason: Option<&str>, count: usize| {
        (status.to_owned(), reason.map(str::to_owned), count)
    };
    assert_eq!(
        counts,
        vec![
            named("left_out", Some("already_closed"), 5),
            named("left_out", Some("foreign_or_displaced"), 4),
            named("left_out", Some("no_source_context"), 1),
            named("left_out", Some("not_yet_defined"), 4),
            named("left_out", Some("other_run"), 2),
            named("matches", None, 1),
            named("mismatch", Some("stale_source_revision"), 2),
        ],
        "one entry per (status, reason) pair, in key order"
    );
    assert_eq!(
        counts.iter().map(|(_, _, count)| count).sum::<usize>(),
        total,
        "the counts cover every row"
    );
    // The open foreign row, the mismatches and the match are shown; every
    // other row is counted only.
    assert_eq!(page.view_total, 4);
    assert_eq!(page.rows.len(), 4);
}
