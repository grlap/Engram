//! Bounded current recipient duties, separate from historical participation.

use super::{
    DISCOVERY_ROWS, DateTime, ProjectId, SessionId, SqliteStore, StoreError, Utc, WorkItem,
    invalid, load_work_item, params, parse_work_id,
};
use crate::domain::{WorkHandoffOffer, WorkHandoffState, WorkLifecycle};

type PendingHandoffs = (Vec<(WorkItem, WorkHandoffOffer)>, usize);

impl SqliteStore {
    pub(crate) fn work_incoming_handoffs(
        &self,
        project: &ProjectId,
        session: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<PendingHandoffs, StoreError> {
        self.work_read_snapshot(|store| {
            let mut statement = store.connection.prepare(
                "SELECT offer.work_id, offer.offer_object_id, offer.offer_json,
                        offer.expires_at_ms, COUNT(*) OVER()
                 FROM work_handoff_offers AS offer INDEXED BY work_handoff_offer_to_live
                 JOIN work_items AS item ON item.work_id = offer.work_id
                 WHERE json_extract(offer.offer_json, '$.to') = ?2
                   AND offer.state = 'offered' AND offer.expires_at_ms >= ?3
                   AND item.project_id = ?1 AND item.lifecycle = 'open'
                   AND item.active_run_id = offer.run_id
                 ORDER BY offer.expires_at_ms, offer.offer_id",
            )?;
            let mut rows =
                statement.query(params![project.0, session.0, now.timestamp_millis()])?;
            let mut items = Vec::new();
            let mut total = 0_usize;
            let mut expired = 0_usize;
            while let Some(row) = rows.next()? {
                total = usize::try_from(row.get::<_, i64>(4)?)
                    .map_err(|_| invalid("incoming handoff count overflow"))?;
                let expires_at_ms = row.get::<_, i64>(3)?;
                // Only boundary candidates need the exact-time count adjustment.
                // Later milliseconds are live; retain the display's bounded decode.
                if expires_at_ms > now.timestamp_millis() && items.len() == DISCOVERY_ROWS {
                    break;
                }
                let work_id = parse_work_id(&row.get::<_, String>(0)?)?;
                let work = load_work_item(&store.connection, work_id)?;
                let offer = super::super::feeds::load_handoff_offer_projection(
                    &store.connection,
                    (row.get(1)?, row.get(2)?),
                )?;
                if work.project_id != *project
                    || work.lifecycle != WorkLifecycle::Open
                    || work.active_run_id != Some(offer.run_id)
                    || offer.work_id != work_id
                    || offer.to != *session
                    || offer.state != WorkHandoffState::Offered
                    || offer.expires_at.timestamp_millis() != expires_at_ms
                {
                    return Err(invalid(
                        "incoming handoff differs from its current recipient binding",
                    ));
                }
                if offer.expires_at <= now {
                    expired += 1;
                    continue;
                }
                if items.len() < DISCOVERY_ROWS {
                    items.push((work, offer));
                }
            }
            let omitted = total.saturating_sub(expired).saturating_sub(items.len());
            Ok((items, omitted))
        })
    }
}
