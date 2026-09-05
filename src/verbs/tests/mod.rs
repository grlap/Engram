use std::str::FromStr;

use chrono::{Duration, TimeZone};
use tempfile::tempdir;

use super::{
    handlers::{
        completion_recovery_reminder, next_commands, normalize_gate_input, obligation_reminders,
        reminder_for_reason,
    },
    receipts::{
        CompactNextReceipt, CompactSectionOmission, CompactWorkRow, append_changes_lines,
        compact_next_lines, compact_next_value, compact_omitted, compact_omitted_for_reason,
        compact_row_line, fit_compact_next, fit_compact_next_to, record_compact_omission,
        shed_compact_labels,
    },
    show::show_lines,
    *,
};
use crate::{
    BuiltinObligationRuleRef, ObjectHash, SqliteStore, VerificationRequirement,
    WorkObligationGuidance,
    domain::{
        MAX_GATE_FAILURE_BYTES, MAX_GATE_FAILURE_INPUTS, MAX_GATE_FAILURE_TOTAL_BYTES,
        MAX_GATE_FAILURES, MAX_GATE_NAME_BYTES, MAX_GATE_REF_BYTES,
    },
};

fn at(second: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 1, 18, 0, 0)
        .single()
        .expect("fixed timestamp")
        + Duration::seconds(second)
}

fn root_input(title: &str, key: &str) -> WorkProposeInput {
    WorkProposeInput::Root {
        notes: Vec::new(),
        title: title.into(),
        outcome: format!("{title} outcome"),
        acceptance: vec![format!("{title} accepted")],
        work_kind: None,
        priority: None,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        idempotency_key: key.into(),
    }
}

fn hash(fill: char) -> ObjectHash {
    ObjectHash::from_str(&fill.to_string().repeat(64)).expect("hash")
}

fn compact_test_row(index: usize) -> CompactWorkRow {
    let title = format!("Readable compact title {index} {}", "x".repeat(100));
    CompactWorkRow {
        work_ref: format!("w-{index:012x}"),
        title: short_with_limit(&title, MAX_COMPACT_TITLE_BYTES),
        state: "blocked".into(),
        holder: Some("session-with-a-readable-name".into()),
        held_until: Some("01:45".into()),
        priority: 1,
        kind: WorkItemKind::Bug,
        labels: vec!["first-label".into(), "second-label".into()],
        labels_omitted: None,
        parent_ref: Some("w-000000000000".into()),
    }
}

fn page(kind: VerificationKind, state: WorkObligationState) -> WorkObligationPage {
    let requirement = VerificationRequirement {
        check_kind: kind,
        check_fingerprint: None,
        required_environment: None,
    };
    WorkObligationPage {
        items: vec![crate::WorkObligationSummary {
            obligation_id: crate::WorkObligationId(uuid::Uuid::nil()),
            definition: hash('a'),
            rule_set: hash('c'),
            state,
            rule: BuiltinObligationRuleRef {
                rule_id: "source_mutation_requires_test".into(),
                rule_version: 1,
            },
            requirement: requirement.clone(),
            triggering_observation: hash('b'),
            resolution: None,
            evidence: None,
            waived_by: None,
            guidance: WorkObligationGuidance::RecordVerificationThenCheckpoint {
                requirement,
                host_waiver_requestable: true,
            },
        }],
        omitted_count: 0,
    }
}

fn assert_ordinary_claim_guidance(receipt: &Receipt, work_ref: &str) {
    assert_eq!(
        receipt.reminders,
        vec!["unclaimed: claim it before execution"]
    );
    assert_eq!(
        receipt.next,
        vec![
            format!("engram work claim {work_ref}"),
            format!("engram work note {work_ref} \"…\"")
        ]
    );
    let actions = receipt.value["allowed_next"]
        .as_array()
        .expect("ordinary allowed_next")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(actions.contains(&WORK_UPDATE_CLAIM_ACTION));
    assert!(!actions.contains(&WORK_UPDATE_CLAIM_RECOVERY_ACTION));
}

fn assert_recovery_claim_guidance(receipt: &Receipt, work_ref: &str) {
    assert_eq!(
        receipt.reminders,
        vec![
            "a previous holder's claim lapsed; claiming needs a recovery reason",
            "unclaimed: claim it before execution",
        ]
    );
    assert_eq!(
        receipt.next,
        vec![
            format!("engram work claim {work_ref} --recover \"…\""),
            format!("engram work note {work_ref} \"…\"")
        ]
    );
    let actions = receipt.value["allowed_next"]
        .as_array()
        .expect("recovery allowed_next")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(!actions.contains(&WORK_UPDATE_CLAIM_ACTION));
    assert!(actions.contains(&WORK_UPDATE_CLAIM_RECOVERY_ACTION));
}

mod customer_workflow;
mod handlers;
mod planning;
mod receipts;
mod shared;
mod show;
