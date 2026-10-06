//! Executing-thread attribution for test fixtures; never release timing data.
use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::domain::RootExecutionMember as Member;
use rusqlite::{Statement, StatementStatus};

#[derive(Clone, Default, serde::Serialize)]
struct SqlCost {
    calls: usize,
    vm_steps: u64,
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct Snapshot {
    root: super::root_state::Cost,
    canonical_decodes: usize,
    work_event_decodes: usize,
    work_item_decodes: usize,
    sql: BTreeMap<&'static str, SqlCost>,
}

#[derive(Default)]
struct Capture {
    active: bool,
    sql: BTreeMap<&'static str, SqlCost>,
    phases: Vec<(&'static str, Snapshot)>,
}

thread_local! {
    static CAPTURE: RefCell<Capture> = RefCell::new(Capture::default());
}

pub(crate) fn start() {
    super::root_state::reset_cost();
    crate::canonical::reset_canonical_decode_count();
    super::reset_work_event_decode_count();
    super::reset_work_item_projection_decode_count();
    CAPTURE.with_borrow_mut(|capture| {
        *capture = Capture {
            active: true,
            ..Capture::default()
        }
    });
}

pub(crate) fn snapshot() -> Snapshot {
    Snapshot {
        root: super::root_state::cost_snapshot(),
        canonical_decodes: crate::canonical::canonical_decode_count(),
        work_event_decodes: super::work_event_decode_count(),
        work_item_decodes: super::work_item_projection_decode_count(),
        sql: CAPTURE.with_borrow(|capture| capture.sql.clone()),
    }
}

pub(crate) fn phase(label: &'static str) {
    if CAPTURE.with_borrow(|capture| capture.active) {
        let snapshot = snapshot();
        CAPTURE.with_borrow_mut(|capture| capture.phases.push((label, snapshot)));
    }
}

pub(crate) fn finish() -> serde_json::Value {
    let total = snapshot();
    CAPTURE.with_borrow_mut(|capture| {
        capture.active = false;
        serde_json::json!({ "total": total, "phases": capture.phases })
    })
}

pub(super) fn sql(label: &'static str, statement: &Statement<'_>) {
    CAPTURE.with_borrow_mut(|capture| {
        if capture.active {
            let cost = capture.sql.entry(label).or_default();
            cost.calls += 1;
            cost.vm_steps += u64::try_from(statement.get_status(StatementStatus::VmStep))
                .expect("nonnegative SQLite VM steps");
        }
    });
}

impl crate::SqliteStore {
    /// Describe the independent axes before resetting the operation counters.
    pub(crate) fn root_cost_fixture_description(&self, work: crate::WorkId) -> serde_json::Value {
        let item = self.get_work_item(work).unwrap();
        let run = self.get_work_run(item.active_run_id.unwrap()).unwrap();
        let state = super::root_state::projected(&self.connection, run.root_execution_id)
            .unwrap()
            .0;
        let kinds = [
            (
                "Run",
                state
                    .run_ids
                    .iter()
                    .copied()
                    .map(Member::Run)
                    .collect::<Vec<_>>(),
            ),
            (
                "ChildSeal",
                state
                    .required_child_seals
                    .iter()
                    .cloned()
                    .map(Member::ChildSeal)
                    .collect(),
            ),
            (
                "ChildWaiver",
                state
                    .required_child_waivers
                    .iter()
                    .cloned()
                    .map(Member::ChildWaiver)
                    .collect(),
            ),
            (
                "Contributor",
                state
                    .expected_contributors
                    .iter()
                    .cloned()
                    .map(Member::Contributor)
                    .collect(),
            ),
            (
                "Contribution",
                state
                    .contributions
                    .iter()
                    .cloned()
                    .map(Member::Contribution)
                    .collect(),
            ),
            (
                "Waiver",
                state.waivers.iter().cloned().map(Member::Waiver).collect(),
            ),
        ];
        let mut members = serde_json::Map::new();
        for (kind, values) in kinds {
            let bytes = values
                .iter()
                .map(|value| crate::CanonicalObject::freeze(value).unwrap().bytes().len())
                .sum::<usize>();
            members.insert(
                kind.into(),
                serde_json::json!({"count":values.len(), "bytes":bytes}),
            );
        }
        let scalar = |sql: &str| {
            self.connection
                .query_row(sql, [run.root_execution_id.0.to_string()], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        };
        serde_json::json!({
            "members":members,
            "state_bytes":crate::CanonicalObject::freeze(&state).unwrap().bytes().len(),
            "history_heads":scalar("SELECT COUNT(*) FROM objects WHERE object_kind = 'work_root_delta' AND json_extract(canonical_json, '$.header.root_execution_id') = ?1"),
            "events":self.connection.query_row("SELECT COUNT(*) FROM work_feed_entries WHERE feed_kind = 'root_work' AND feed_id = ?1 AND object_kind = 'work_event'", [item.root_id.0.to_string()], |row| row.get::<_, i64>(0)).unwrap(),
            "run_evidence_count":self.work_run_evidence(run.run_id).unwrap().len(),
            "run_evidence_bytes":self.work_run_evidence(run.run_id).unwrap().iter().map(|id| usize::try_from(self.connection.query_row("SELECT length(canonical_json) FROM objects WHERE object_id = ?1", [id.as_str()], |row| row.get::<_, i64>(0)).unwrap()).expect("nonnegative evidence length fits usize")).sum::<usize>(),
        })
    }
}
