//! Selected-project diagnostic facts, separate from absence evidence.

use chrono::{DateTime, Datelike, Utc};
use rusqlite::{Connection, OptionalExtension, types::Value};
use serde::Serialize;

use crate::{ProjectId, SessionPhase, TurnGrantState};

use super::StoreError;

const VM_STEPS: usize = 100_000;

/// Bounded, versioned stored facts; never authorization or replay evidence.
#[derive(Debug, Serialize)]
pub struct ControlInspectionDetails {
    schema_version: u16,
    session: SessionDetails,
    session_begun_grant_present: Option<bool>,
    retained_grant: GrantDetails,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum RowStatus {
    Found,
    NotFound,
    Withheld,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ScalarStatus {
    Recognized,
    Unrecognized,
}

#[derive(Debug, Serialize)]
struct SessionDetails {
    status: RowStatus,
    project_match: Option<bool>,
    phase: Option<SessionPhase>,
    phase_status: Option<ScalarStatus>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum SessionRelation {
    Target,
    Other,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum TimestampStatus {
    Valid,
    Invalid,
    Inconsistent,
}

#[derive(Debug, Serialize)]
struct GrantDetails {
    status: RowStatus,
    project_match: Option<bool>,
    state: Option<TurnGrantState>,
    state_status: Option<ScalarStatus>,
    session_id: Option<String>,
    session_relation: Option<SessionRelation>,
    owner_session_present: Option<bool>,
    matches_current_task: Option<bool>,
    issued_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    begun_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    timestamps_status: Option<TimestampStatus>,
}

impl GrantDetails {
    fn undisclosed(status: RowStatus, project_match: Option<bool>) -> Self {
        Self {
            status,
            project_match,
            state: None,
            state_status: None,
            session_id: None,
            session_relation: None,
            owner_session_present: None,
            matches_current_task: None,
            issued_at: None,
            expires_at: None,
            begun_at: None,
            completed_at: None,
            timestamps_status: None,
        }
    }
}

/// This is installed only on the inspector's private, read-only connection.
/// Always remove it, including after an interrupted query or unwind.
struct VmBudget<'a>(&'a Connection);

impl<'a> VmBudget<'a> {
    fn install(connection: &'a Connection, steps: usize) -> Result<Self, StoreError> {
        let stride = steps.clamp(1, 1_000);
        let mut remaining = steps;
        connection.progress_handler(
            i32::try_from(stride).unwrap_or(1_000),
            Some(move || {
                remaining = remaining.saturating_sub(stride);
                remaining == 0
            }),
        )?;
        Ok(Self(connection))
    }
}

impl Drop for VmBudget<'_> {
    fn drop(&mut self) {
        let _ = self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

pub(super) fn read(
    connection: &Connection,
    project: &ProjectId,
    session: &str,
    grant: &str,
) -> Result<ControlInspectionDetails, StoreError> {
    read_with_budget(connection, project, session, grant, VM_STEPS)
}

fn read_with_budget(
    connection: &Connection,
    project: &ProjectId,
    session: &str,
    grant: &str,
    steps: usize,
) -> Result<ControlInspectionDetails, StoreError> {
    let _budget = VmBudget::install(connection, steps)?;
    let row = connection
        .query_row(
            "SELECT project_id = ?2,
                    CASE WHEN phase IN ('ready','sync_required','turn_open','exited')
                         THEN phase END
             FROM control_sessions WHERE session_id = ?1",
            [session, project.0.as_str()],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let session_details = match row {
        None => SessionDetails {
            status: RowStatus::NotFound,
            project_match: None,
            phase: None,
            phase_status: None,
        },
        Some((false, _)) => SessionDetails {
            status: RowStatus::Withheld,
            project_match: Some(false),
            phase: None,
            phase_status: None,
        },
        Some((true, phase)) => SessionDetails {
            status: RowStatus::Found,
            project_match: Some(true),
            phase: phase.as_deref().and_then(phase_value),
            phase_status: Some(scalar_status(phase.is_some())),
        },
    };
    let session_begun_grant_present = if session_details.project_match == Some(true) {
        Some(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM control_turn_grants
                           WHERE session_id = ?1 AND state = 'begun')",
            [session],
            |row| row.get(0),
        )?)
    } else {
        None
    };
    let retained_grant = read_grant(connection, project, session, grant)?;
    Ok(ControlInspectionDetails {
        schema_version: 1,
        session: session_details,
        session_begun_grant_present,
        retained_grant,
    })
}

fn read_grant(
    connection: &Connection,
    project: &ProjectId,
    session: &str,
    grant: &str,
) -> Result<GrantDetails, StoreError> {
    let mut statement = connection.prepare(
        "SELECT CASE WHEN a.task_id IS NOT NULL THEN a.project_id = ?3 END,
                CASE WHEN length(CAST(g.session_id AS BLOB)) BETWEEN 1 AND ?4
                     THEN g.session_id END,
                g.session_id = ?2,
                EXISTS(SELECT 1 FROM control_sessions s WHERE s.session_id = g.session_id),
                g.session_id = ?2 AND g.task_id =
                    (SELECT task_id FROM control_sessions WHERE session_id = ?2),
                CASE WHEN g.state IN ('issued','begun','completed','expired','superseded')
                     THEN g.state END,
                g.issued_at_ms,g.expires_at_ms,g.begun_at_ms,g.completed_at_ms
         FROM control_turn_grants g LEFT JOIN control_anchors a ON a.task_id = g.task_id
         WHERE g.grant_id = ?1",
    )?;
    let max_session_id_bytes = i64::try_from(crate::MAX_SESSION_ID_BYTES)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let mut rows = statement.query(rusqlite::params![
        grant,
        session,
        project.0,
        max_session_id_bytes
    ])?;
    let Some(row) = rows.next()? else {
        return Ok(GrantDetails::undisclosed(RowStatus::NotFound, None));
    };
    let project_match: Option<bool> = row.get(0)?;
    if project_match != Some(true) {
        return Ok(GrantDetails::undisclosed(
            RowStatus::Withheld,
            project_match,
        ));
    }
    let owner: Option<String> = row.get(1)?;
    let session_id = owner.filter(|id| !id.trim().is_empty() && !id.chars().any(char::is_control));
    let raw_state: Option<String> = row.get(5)?;
    let state = raw_state.as_deref().and_then(state_value);
    let (issued_at, issued_valid) = timestamp(&row.get::<_, Value>(6)?, false);
    let (expires_at, expires_valid) = timestamp(&row.get::<_, Value>(7)?, false);
    let (begun_at, begun_valid) = timestamp(&row.get::<_, Value>(8)?, true);
    let (completed_at, completed_valid) = timestamp(&row.get::<_, Value>(9)?, true);
    let timestamps_status = if !(issued_valid && expires_valid && begun_valid && completed_valid) {
        TimestampStatus::Invalid
    } else if state.is_some_and(|state| match state {
        TurnGrantState::Issued | TurnGrantState::Expired | TurnGrantState::Superseded => {
            begun_at.is_some() || completed_at.is_some()
        }
        TurnGrantState::Begun => begun_at.is_none() || completed_at.is_some(),
        TurnGrantState::Completed => begun_at.is_none() || completed_at.is_none(),
    }) {
        TimestampStatus::Inconsistent
    } else {
        TimestampStatus::Valid
    };
    Ok(GrantDetails {
        status: RowStatus::Found,
        project_match,
        state,
        state_status: Some(scalar_status(raw_state.is_some())),
        session_id,
        session_relation: Some(if row.get::<_, bool>(2)? {
            SessionRelation::Target
        } else {
            SessionRelation::Other
        }),
        owner_session_present: Some(row.get(3)?),
        matches_current_task: row.get(4)?,
        issued_at,
        expires_at,
        begun_at,
        completed_at,
        timestamps_status: Some(timestamps_status),
    })
}

fn scalar_status(recognized: bool) -> ScalarStatus {
    if recognized {
        ScalarStatus::Recognized
    } else {
        ScalarStatus::Unrecognized
    }
}

fn phase_value(value: &str) -> Option<SessionPhase> {
    match value {
        "ready" => Some(SessionPhase::Ready),
        "sync_required" => Some(SessionPhase::SyncRequired),
        "turn_open" => Some(SessionPhase::TurnOpen),
        "exited" => Some(SessionPhase::Exited),
        _ => None,
    }
}

fn state_value(value: &str) -> Option<TurnGrantState> {
    match value {
        "issued" => Some(TurnGrantState::Issued),
        "begun" => Some(TurnGrantState::Begun),
        "completed" => Some(TurnGrantState::Completed),
        "expired" => Some(TurnGrantState::Expired),
        "superseded" => Some(TurnGrantState::Superseded),
        _ => None,
    }
}

fn timestamp(value: &Value, nullable: bool) -> (Option<DateTime<Utc>>, bool) {
    match value {
        Value::Null => (None, nullable),
        Value::Integer(ms) => {
            let time = DateTime::from_timestamp_millis(*ms)
                .filter(|time| (0..=9999).contains(&time.year()));
            let valid = time.is_some();
            (time, valid)
        }
        _ => (None, false),
    }
}

#[cfg(test)]
mod tests;
