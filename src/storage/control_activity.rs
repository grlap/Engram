//! When the control plane last recorded activity, read from rows the store
//! already keeps, and the warnings a mediating policy draws from them. Only
//! doctor reads this; it is not part of the shared control diagnostics.
//!
//! Every read here is bounded: aggregates over the control session and
//! turn grant tables, an endpoint read by append sequence, and a join over
//! the few project feed heads. Nothing scans work history. All of them run
//! in one read snapshot, together with the required assurance they are
//! judged against, so a concurrent turn cannot be half seen.

use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension};

use super::{ControlAssurance, SqliteStore, StoreError, parse_enum};

/// How long recorded work may follow the last turn activity before doctor
/// says no turn mediated it. Kept beside [`OPEN_TURN_LIMIT`] so the pair is
/// changed in one place.
pub const MEDIATION_GRACE: TimeDelta = TimeDelta::minutes(10);

/// How long a begun turn may stay unsettled before doctor reports it as an
/// open turn without completion. The store records no liveness while a turn
/// runs, so a healthy long turn and a wedged one look alike until this age.
pub const OPEN_TURN_LIMIT: TimeDelta = TimeDelta::minutes(60);

/// When the store last recorded each kind of control activity, or `None`
/// when it has recorded none.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlActivity {
    /// The largest update time of any control session: a bind or any later
    /// change to that session. The store keeps no separate bind time.
    pub last_session_activity: Option<DateTime<Utc>>,
    /// The largest recorded turn begin time.
    pub last_turn_begun: Option<DateTime<Utc>>,
    /// The largest recorded turn settlement time.
    pub last_turn_settled: Option<DateTime<Utc>>,
    /// The creation time of the most recently appended turn evaluation.
    pub last_turn_evaluation: Option<DateTime<Utc>>,
    /// The begin time of the oldest turn that has begun and not settled.
    pub oldest_open_turn_begun: Option<DateTime<Utc>>,
    /// The begin time of the newest turn that has begun and not settled.
    pub newest_open_turn_begun: Option<DateTime<Utc>>,
    /// When the most recently appended record of the project's work feed
    /// was stored, to the second.
    pub latest_work_recorded: Option<DateTime<Utc>>,
}

impl ControlActivity {
    /// The latest turn activity: a begin, a settlement or an evaluation.
    #[must_use]
    pub fn last_turn_activity(&self) -> Option<DateTime<Utc>> {
        [
            self.last_turn_begun,
            self.last_turn_settled,
            self.last_turn_evaluation,
        ]
        .into_iter()
        .flatten()
        .max()
    }
}

/// What the recorded activity says about a policy that requires mediation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediationWarning {
    /// Work was recorded more than [`MEDIATION_GRACE`] after the last turn
    /// activity, or with no turn activity ever, and no recent open turn.
    WorkWithoutControlActivity {
        work_recorded: DateTime<Utc>,
        last_turn_activity: Option<DateTime<Utc>>,
    },
    /// A turn has stayed begun and unsettled for at least
    /// [`OPEN_TURN_LIMIT`].
    OpenTurnWithoutCompletion { begun: DateTime<Utc> },
}

/// The activity times read in one snapshot, with the required assurance of
/// that snapshot and the warnings drawn from both at the diagnostic instant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediationReport {
    pub required_assurance: ControlAssurance,
    pub activity: ControlActivity,
    pub warnings: Vec<MediationWarning>,
}

impl SqliteStore {
    /// Reads when the control plane last recorded activity and what that
    /// says, at `now`, about a policy that requires mediation. All reads
    /// share one snapshot: the caller's transaction when one is open,
    /// otherwise a read transaction of its own.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a recorded time cannot be read.
    pub fn mediation_activity_at(&self, now: DateTime<Utc>) -> Result<MediationReport, StoreError> {
        let (required_assurance, activity) = if self.connection.is_autocommit() {
            let snapshot = self.connection.unchecked_transaction()?;
            let read = read_mediation_facts(&snapshot)?;
            snapshot.commit()?;
            read
        } else {
            read_mediation_facts(&self.connection)?
        };
        let warnings = mediation_warnings(required_assurance, &activity, now);
        Ok(MediationReport {
            required_assurance,
            activity,
            warnings,
        })
    }
}

fn read_mediation_facts(
    connection: &Connection,
) -> Result<(ControlAssurance, ControlActivity), StoreError> {
    let required_assurance: String = connection.query_row(
        "SELECT required_assurance FROM control_policy_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let required_assurance: ControlAssurance = parse_enum(&required_assurance)?;
    let last_session_ms = connection.query_row(
        "SELECT MAX(updated_at_ms) FROM control_sessions",
        [],
        |row| row.get::<_, Option<i64>>(0),
    )?;
    let (begun_ms, settled_ms, oldest_open_ms, newest_open_ms) = connection.query_row(
        "SELECT MAX(begun_at_ms),
                MAX(completed_at_ms),
                MIN(CASE WHEN state = 'begun' THEN begun_at_ms END),
                MAX(CASE WHEN state = 'begun' THEN begun_at_ms END)
         FROM control_turn_grants",
        [],
        |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        },
    )?;
    let evaluation_ms = connection
        .query_row(
            "SELECT created_at_ms FROM control_turn_results
             ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let work_recorded = connection.query_row(
        "SELECT MAX(object.created_at)
         FROM work_feed_heads AS head
         JOIN work_feed_entries AS entry
           ON entry.feed_kind = head.feed_kind
          AND entry.feed_id = head.feed_id
          AND entry.position = head.position
         JOIN objects AS object ON object.object_id = entry.object_id
         WHERE head.feed_kind = 'project'",
        [],
        |row| row.get::<_, Option<String>>(0),
    )?;
    let activity = ControlActivity {
        last_session_activity: millis(last_session_ms, "session update")?,
        last_turn_begun: millis(begun_ms, "turn begin")?,
        last_turn_settled: millis(settled_ms, "turn settlement")?,
        last_turn_evaluation: millis(evaluation_ms, "turn evaluation")?,
        oldest_open_turn_begun: millis(oldest_open_ms, "open turn begin")?,
        newest_open_turn_begun: millis(newest_open_ms, "open turn begin")?,
        latest_work_recorded: work_recorded
            .as_deref()
            .map(stored_object_time)
            .transpose()?,
    };
    Ok((required_assurance, activity))
}

/// Draws the warnings a policy requiring at least turn-gated mediation
/// gets from the recorded activity at `now`; a weaker policy gets none.
#[must_use]
pub fn mediation_warnings(
    required: ControlAssurance,
    activity: &ControlActivity,
    now: DateTime<Utc>,
) -> Vec<MediationWarning> {
    if !required.covers(ControlAssurance::TurnGated) {
        return Vec::new();
    }
    let mut warnings = Vec::new();
    let recent_open_turn = activity
        .newest_open_turn_begun
        .is_some_and(|begun| now - begun < OPEN_TURN_LIMIT);
    if let Some(work_recorded) = activity.latest_work_recorded {
        let last_turn_activity = activity.last_turn_activity();
        let unmediated =
            last_turn_activity.is_none_or(|activity| work_recorded - activity > MEDIATION_GRACE);
        if unmediated && !recent_open_turn {
            warnings.push(MediationWarning::WorkWithoutControlActivity {
                work_recorded,
                last_turn_activity,
            });
        }
    }
    if let Some(begun) = activity.oldest_open_turn_begun
        && now - begun >= OPEN_TURN_LIMIT
    {
        warnings.push(MediationWarning::OpenTurnWithoutCompletion { begun });
    }
    warnings
}

fn millis(value: Option<i64>, what: &str) -> Result<Option<DateTime<Utc>>, StoreError> {
    value
        .map(|ms| {
            DateTime::from_timestamp_millis(ms).ok_or_else(|| {
                StoreError::InvalidControlProjection(format!(
                    "the recorded {what} time is out of range"
                ))
            })
        })
        .transpose()
}

/// Reads an `objects.created_at` value, which SQLite writes as
/// `YYYY-MM-DD HH:MM:SS` in UTC.
fn stored_object_time(text: &str) -> Result<DateTime<Utc>, StoreError> {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
        .map(|time| time.and_utc())
        .map_err(|_| {
            StoreError::InvalidWorkProjection(format!(
                "a stored work record time is not a timestamp: {text:?}"
            ))
        })
}

#[cfg(test)]
mod tests;
