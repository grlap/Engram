//! The check of a store copy that `backup restore` is about to install: the
//! full check a backup gets, and what the copy says about its project and the
//! live authority it carries, read from the same immutable bytes.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{BackupManifest, Path, SqliteStore, StoreError, store_sidecars};

/// What a checked store copy holds for a restore.
#[derive(Clone, Debug)]
pub struct RestoreCopyReport {
    /// The full check's result, as [`SqliteStore::verify_backup`] gives it.
    pub manifest: BackupManifest,
    /// Every project id a row of the copy names, in order; empty for a store
    /// that holds no project rows.
    pub project_ids: Vec<String>,
    /// The live authority in the copy, as of one clock reading.
    pub authority: LiveAuthority,
}

/// The claims, grants and begun turns a store copy holds, read with one clock
/// reading over the whole store.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct LiveAuthority {
    /// The clock reading every expiry was compared with.
    pub as_of: DateTime<Utc>,
    /// Active claims that have not yet expired at `as_of`.
    pub unexpired_claims: u64,
    /// When the last of them expires.
    pub claims_expire_by: Option<DateTime<Utc>>,
    /// Issued turn grants that have not yet expired at `as_of`.
    pub unexpired_grants: u64,
    /// When the last of them expires.
    pub grants_expire_by: Option<DateTime<Utc>>,
    /// Turns begun and not yet completed, whatever their grants' expiry.
    pub begun_turns: u64,
}

impl SqliteStore {
    /// Checks the store copy at `path` as [`Self::verify_backup`] does, then
    /// reads from exactly those bytes the project ids its rows name and its
    /// live authority as of `now`. Nothing is written: the copy is opened
    /// immutable, and no log or shared-memory file is created beside it.
    ///
    /// # Errors
    ///
    /// Returns what [`Self::verify_backup`] returns for a copy that is not a
    /// healthy store this build accepts, and a SQLite error from the reads.
    pub fn verify_restore_copy(
        path: &Path,
        now: DateTime<Utc>,
    ) -> Result<RestoreCopyReport, StoreError> {
        let (manifest, store) = Self::verify_copy_file(path, None)?;
        let project_ids = store.distinct_project_ids()?;
        let authority = store.live_authority(now)?;
        Ok(RestoreCopyReport {
            manifest,
            project_ids,
            authority,
        })
    }

    /// Checks a store a restore has moved into place at `path`, as
    /// [`Self::verify_restore_copy`] does, but accepts the files a read leaves
    /// beside it: an empty write-ahead log, which holds no frames, so the main
    /// file is the whole store, and its shared-memory index, which holds no
    /// rows. A rollback journal or a log with any bytes is refused.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidWork`] for such a journal or log, and what
    /// [`Self::verify_restore_copy`] returns otherwise.
    pub fn verify_installed_restore(
        path: &Path,
        now: DateTime<Utc>,
    ) -> Result<RestoreCopyReport, StoreError> {
        if let Some(problem) = installed_sidecar_problem(path) {
            return Err(StoreError::InvalidWork(problem));
        }
        if !path.is_file() {
            return Err(StoreError::InvalidWork(format!(
                "{} is not an existing file",
                path.display()
            )));
        }
        let (manifest, store) = Self::verify_copy_bytes(path, None)?;
        let project_ids = store.distinct_project_ids()?;
        let authority = store.live_authority(now)?;
        Ok(RestoreCopyReport {
            manifest,
            project_ids,
            authority,
        })
    }

    /// Every distinct value of every `project_id` column in the store.
    fn distinct_project_ids(&self) -> Result<Vec<String>, StoreError> {
        let tables = {
            let mut statement = self.connection.prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                   AND EXISTS (
                       SELECT 1 FROM pragma_table_info(sqlite_master.name)
                       WHERE name = 'project_id'
                   )
                 ORDER BY name",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut ids = std::collections::BTreeSet::new();
        for table in tables {
            let quoted = table.replace('"', "\"\"");
            let mut statement = self.connection.prepare(&format!(
                "SELECT DISTINCT CAST(project_id AS TEXT) FROM \"{quoted}\"
                 WHERE project_id IS NOT NULL"
            ))?;
            for id in statement.query_map([], |row| row.get::<_, String>(0))? {
                ids.insert(id?);
            }
        }
        Ok(ids.into_iter().collect())
    }

    /// The live authority in the store as of `now`.
    fn live_authority(&self, now: DateTime<Utc>) -> Result<LiveAuthority, StoreError> {
        let now_ms = now.timestamp_millis();
        let (unexpired_claims, claims_last) = self.connection.query_row(
            "SELECT COUNT(*), MAX(expires_at_ms) FROM work_claims
             WHERE state = 'active' AND expires_at_ms > ?1",
            [now_ms],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
        )?;
        let (unexpired_grants, grants_last) = self.connection.query_row(
            "SELECT COUNT(*), MAX(expires_at_ms) FROM control_turn_grants
             WHERE state = 'issued' AND expires_at_ms > ?1",
            [now_ms],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
        )?;
        let begun_turns = self.connection.query_row(
            "SELECT COUNT(*) FROM control_turn_grants WHERE state = 'begun'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(LiveAuthority {
            as_of: now,
            unexpired_claims: count(unexpired_claims),
            claims_expire_by: claims_last.and_then(DateTime::from_timestamp_millis),
            unexpired_grants: count(unexpired_grants),
            grants_expire_by: grants_last.and_then(DateTime::from_timestamp_millis),
            begun_turns: count(begun_turns),
        })
    }
}

/// Why the files beside an installed store keep it from being checked as the
/// restore's own output, if they do: a rollback journal, or a write-ahead log
/// with any bytes. An empty log and the shared-memory index are what a read
/// leaves, and are accepted.
#[must_use]
pub fn installed_sidecar_problem(path: &Path) -> Option<String> {
    for sidecar in store_sidecars(path) {
        let name = sidecar.to_string_lossy();
        let metadata = match std::fs::symlink_metadata(&sidecar) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Some(format!("{} cannot be examined: {error}", sidecar.display()));
            }
        };
        if name.ends_with("-shm") && metadata.is_file() {
            continue;
        }
        if name.ends_with("-wal") && metadata.is_file() && metadata.len() == 0 {
            continue;
        }
        return Some(format!(
            "{} stands beside {}; only an empty write-ahead log and its index may",
            sidecar.display(),
            path.display()
        ));
    }
    None
}

/// A SQLite count, which is never negative.
fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

#[cfg(test)]
mod tests;
