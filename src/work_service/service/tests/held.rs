//! `work core held`: every claim a session holds, with its bindable tuple.

use super::super::super::test_support::*;
use super::super::super::*;
use super::{inspection_sensitive_state, propose_root_for_inspect};

fn service(database: &std::path::Path, session: &str) -> LocalWorkService {
    LocalWorkService::new(
        database.to_path_buf(),
        ProjectId("held-project".into()),
        "agent".into(),
        SessionId(session.into()),
        Some("held-test".into()),
    )
}

/// Focuses and claims one item for 600 seconds, returning the claim
/// receipt's binding.
fn claim(
    service: &LocalWorkService,
    item: &WorkItemSummary,
    key: &str,
    second: i64,
) -> ControlWorkBinding {
    claim_for(service, item, key, second, 600, None)
}

fn claim_for(
    service: &LocalWorkService,
    item: &WorkItemSummary,
    key: &str,
    second: i64,
    ttl_seconds: i64,
    recovery_reason: Option<&str>,
) -> ControlWorkBinding {
    service
        .work_focus(&item.short_ref, at(second))
        .expect("focus the item to claim");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(ttl_seconds),
                recovery_reason: recovery_reason.map(Into::into),
                idempotency_key: key.into(),
            },
            at(second),
        )
        .expect("claim")
        .receipt
        .control_binding
        .expect("claim receipt binding")
}

#[test]
fn a_retaken_or_recovered_claim_is_acquired_again_and_held_stays_in_its_project() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let holder = service(&database, "holder");
    let rescuer = service(&database, "rescuer");
    let retaken = propose_root_for_inspect(&holder, "retaken", at(0));
    let recovered = propose_root_for_inspect(&holder, "recovered", at(0));
    let first_retaken = claim_for(&holder, &retaken, "claim-retaken", 1, 60, None);
    let first_recovered = claim_for(&holder, &recovered, "claim-recovered", 1, 60, None);
    // Claims acquired in the same second come in ascending work id order.
    let mut same_second = [retaken.work_id, recovered.work_id];
    same_second.sort_by_key(|work_id| work_id.0);
    assert_eq!(
        holder
            .work_held(at(2))
            .expect("claims acquired together")
            .items
            .iter()
            .map(|row| row.work_id)
            .collect::<Vec<_>>(),
        same_second
    );

    // After its own claim lapsed, the holder takes the item again: a new
    // acquisition under the same claim id and a higher fence.
    let second_retaken = claim_for(&holder, &retaken, "retake", 100, 600, None);
    assert_eq!(second_retaken.claim_id, first_retaken.claim_id);
    assert!(second_retaken.claim_fence > first_retaken.claim_fence);
    let row = row_after(&holder, &retaken, 101);
    assert_eq!(row.claimed_at, at(100));
    assert_eq!(row.claim_fence, second_retaken.claim_fence);

    // Another session recovers the lapsed claim: its claim time is the
    // recovery, and the old holder no longer lists the item.
    let recovery_binding = claim_for(
        &rescuer,
        &recovered,
        "recover",
        102,
        600,
        Some("the holder's claim lapsed"),
    );
    assert_eq!(recovery_binding.claim_id, first_recovered.claim_id);
    assert!(recovery_binding.claim_fence > first_recovered.claim_fence);
    let row = row_after(&rescuer, &recovered, 103);
    assert_eq!(
        (row.claimed_at, row.claim_fence),
        (at(102), recovery_binding.claim_fence)
    );
    let held = holder.work_held(at(103)).expect("the old holder's claims");
    assert_eq!(
        held.items.iter().map(|row| row.work_id).collect::<Vec<_>>(),
        [retaken.work_id]
    );

    // The same session id in another project holds nothing there.
    let elsewhere = LocalWorkService::new(
        database.clone(),
        ProjectId("other-project".into()),
        "agent".into(),
        SessionId("holder".into()),
        Some("held-test".into()),
    );
    let nothing = elsewhere
        .work_held(at(103))
        .expect("another project's claims");
    assert_eq!((nothing.items.len(), nothing.total), (0, 0));

    // A live claim with no event that gave it to its holder is refused as
    // an invalid projection, never given a wrong time.
    let connection = rusqlite::Connection::open(&database).expect("fixture connection");
    let removed = connection
        .execute(
            "DELETE FROM work_feed_entries
             WHERE feed_kind = 'project' AND work_id = ?1
               AND object_id IN (
                   SELECT object_id FROM objects
                   WHERE json_extract(canonical_json, '$.transition.kind') = 'claimed'
                     AND json_extract(canonical_json, '$.claim.fence') = ?2)",
            rusqlite::params![retaken.work_id.0.to_string(), second_retaken.claim_fence],
        )
        .expect("remove the acquisition from the item's feed");
    assert_eq!(removed, 1);
    assert!(matches!(
        holder.work_held(at(104)),
        Err(StoreError::InvalidWorkProjection(detail))
            if detail.contains("has no event that gave it to its holder")
    ));
}

fn row(held: &WorkHeldView, item: &WorkItemSummary) -> WorkHeldClaim {
    held.items
        .iter()
        .find(|row| row.work_id == item.work_id)
        .cloned()
        .expect("a row for the held item")
}

#[test]
fn held_lists_every_claim_newest_first_with_its_binding_and_the_focus() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let holder = service(&database, "holder");
    let other = service(&database, "other");
    let first = propose_root_for_inspect(&holder, "first", at(0));
    let second = propose_root_for_inspect(&holder, "second", at(0));
    let third = propose_root_for_inspect(&holder, "third", at(0));
    let elsewhere = propose_root_for_inspect(&holder, "elsewhere", at(0));
    let first_binding = claim(&holder, &first, "claim-first", 2);
    let second_binding = claim(&holder, &second, "claim-second", 4);
    let third_binding = claim(&holder, &third, "claim-third", 6);
    claim(&other, &elsewhere, "claim-elsewhere", 7);
    holder
        .work_focus(&second.short_ref, at(8))
        .expect("focus moves back to the second item");

    let held = holder.work_held(at(9)).expect("held claims");
    assert_eq!(
        held.items.iter().map(|row| row.work_id).collect::<Vec<_>>(),
        [third.work_id, second.work_id, first.work_id],
        "newest claim first; another session's claim is not listed"
    );
    assert_eq!((held.total, held.omitted), (3, 0));
    assert_eq!(held.focused_work_id, Some(second.work_id));
    for (item, binding, second_claimed) in [
        (&first, &first_binding, 2),
        (&second, &second_binding, 4),
        (&third, &third_binding, 6),
    ] {
        let row = row(&held, item);
        assert_eq!(row.short_ref, item.short_ref);
        assert_eq!(row.claim_id, binding.claim_id);
        assert_eq!(row.claim_fence, binding.claim_fence);
        assert_eq!(row.claimed_at, at(second_claimed));
        assert_eq!(row.expires_at, at(second_claimed + 600));
        assert_eq!(row.focused, item.work_id == second.work_id);
        assert_eq!(row.control_binding.as_ref(), Some(binding));
    }

    // Every key is present on the wire: a binding, and the focus.
    let json = serde_json::to_value(&held).expect("held JSON");
    assert!(json["items"].as_array().expect("items").iter().all(|row| {
        row.get("control_binding")
            .is_some_and(serde_json::Value::is_object)
    }));

    // The other session sees only its own claim, and no focus of the holder.
    let seen = other.work_held(at(9)).expect("other session's claims");
    assert_eq!(
        seen.items.iter().map(|row| row.work_id).collect::<Vec<_>>(),
        [elsewhere.work_id]
    );
    assert_eq!(seen.focused_work_id, Some(elsewhere.work_id));
}

#[test]
fn a_held_claim_bind_would_refuse_is_listed_with_null_and_a_handoff_moves_the_claim_time() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let holder = service(&database, "holder");
    let planner = service(&database, "planner");
    let recipient = service(&database, "recipient");
    let revised = propose_root_for_inspect(&holder, "revised", at(0));
    let offered = propose_root_for_inspect(&holder, "offered", at(0));
    let revised_binding = claim(&holder, &revised, "claim-revised", 1);
    let offered_binding = claim(&holder, &offered, "claim-offered", 2);
    let revise = |service: &LocalWorkService, key: &str| {
        service
            .work_focus(&revised.short_ref, at(3))
            .expect("focus the claimed item");
        service.work_update(
            WorkUpdateInput::Revise {
                patch: WorkRevisionPatch {
                    title: Some(format!("Revised after the claim by {key}")),
                    ..WorkRevisionPatch::default()
                },
                idempotency_key: key.into(),
            },
            at(3),
        )
    };

    // Only the holder may revise a claimed item. Its revision re-accepts the
    // claim at the new revision, so the earlier binding is stale and the row
    // shows a fresh one.
    revise(&planner, "planner-revise")
        .expect_err("another session cannot revise work held by a live claim");
    revise(&holder, "holder-revise").expect("the holder revises its claimed item");
    let after_revision = row_after(&holder, &revised, 3);
    // The revision re-accepts the claim but is not an acquisition.
    assert_eq!(after_revision.claimed_at, at(1));
    assert_eq!(after_revision.claim_fence, revised_binding.claim_fence);
    let refreshed = after_revision
        .control_binding
        .expect("the holder's revision keeps a binding");
    assert_eq!(refreshed.claim_id, revised_binding.claim_id);
    assert!(refreshed.work_revision > revised_binding.work_revision);
    // A pending handoff offer leaves the claim held but not bindable.
    holder
        .work_focus(&offered.short_ref, at(4))
        .expect("focus the item to offer");
    holder
        .work_handoff(
            WorkHandoffInput::Offer {
                to: "recipient".into(),
                ttl_seconds: Some(300),
                checkpoint_summary: "offer the held item".into(),
                idempotency_key: "offer".into(),
            },
            at(4),
        )
        .expect("offer");

    let held = holder.work_held(at(5)).expect("held claims");
    assert_eq!((held.total, held.omitted), (2, 0));
    let pending = row(&held, &offered);
    assert!(pending.control_binding.is_none(), "{pending:?}");
    assert_eq!(pending.claim_id, offered_binding.claim_id);
    assert_eq!(row(&held, &revised).control_binding, Some(refreshed));
    let json = serde_json::to_value(&held).expect("held JSON");
    let nulls = json["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|row| row.get("control_binding") == Some(&serde_json::Value::Null))
        .count();
    assert_eq!(nulls, 1, "the claim bind would refuse is an explicit null");

    // Accepting the offer moves the claim: the holder no longer lists it, and
    // the recipient's claim time is the acceptance, under a new fence.
    recipient
        .work_focus(&offered.short_ref, at(6))
        .expect("recipient focuses the offered item");
    recipient
        .work_handoff(
            WorkHandoffInput::Accept {
                idempotency_key: "accept".into(),
            },
            at(6),
        )
        .expect("accept");
    let held = holder.work_held(at(7)).expect("holder after the handoff");
    assert!(
        held.items.iter().all(|row| row.work_id != offered.work_id),
        "the offering session no longer holds the claim"
    );
    assert_eq!(held.total, 1);
    let received = recipient.work_held(at(7)).expect("recipient's claims");
    let row = row(&received, &offered);
    assert_eq!(row.claimed_at, at(6));
    assert_eq!(row.claim_id, offered_binding.claim_id);
    assert_eq!(row.claim_fence, offered_binding.claim_fence + 1);
    let binding = row.control_binding.expect("the recipient can bind");
    assert_eq!(binding.claim_fence, row.claim_fence);

    // A renewal keeps the time the claim was acquired.
    recipient
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(86_000),
                recovery_reason: None,
                idempotency_key: "renew".into(),
            },
            at(8),
        )
        .expect("renew the received claim");
    let renewed = row_after(&recipient, &offered, 9);
    assert_eq!(renewed.claimed_at, at(6));
    assert!(renewed.expires_at > row.expires_at);
}

fn row_after(service: &LocalWorkService, item: &WorkItemSummary, second: i64) -> WorkHeldClaim {
    row(&service.work_held(at(second)).expect("held claims"), item)
}

#[test]
fn held_is_bounded_with_an_exact_omitted_count_and_changes_nothing() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let holder = service(&database, "holder");
    let items = (0..=MAX_HELD_CLAIMS)
        .map(|index| propose_root_for_inspect(&holder, &format!("bounded-{index}"), at(0)))
        .collect::<Vec<_>>();
    for (index, item) in items.iter().enumerate() {
        let second = 1 + i64::try_from(index).expect("index");
        claim(&holder, item, &format!("claim-{index}"), second);
    }
    // A staged, unacknowledged page is the state a host must not disturb.
    let staged = holder
        .work_next(20, WorkNextQuery::default(), at(40))
        .expect("stage a delivery page");
    assert!(staged.session.pending_delivery);
    let before = inspection_sensitive_state(&database);

    let held = holder.work_held(at(41)).expect("held claims");
    assert_eq!(held.items.len(), MAX_HELD_CLAIMS);
    assert_eq!((held.total, held.omitted), (MAX_HELD_CLAIMS + 1, 1));
    assert!(
        held.items.iter().all(|row| row.work_id != items[0].work_id),
        "the oldest claim is the one left out"
    );
    assert!(serde_json::to_vec(&held).expect("held JSON").len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
    assert_eq!(inspection_sensitive_state(&database), before);

    // A missing or empty store is refused and left as it was.
    let missing = directory.path().join("missing.sqlite3");
    assert!(matches!(
        service(&missing, "holder").work_held(at(41)),
        Err(StoreError::StoreNotInitialized)
    ));
    assert!(!missing.exists(), "held must not create a store");
    let empty = directory.path().join("empty.sqlite3");
    std::fs::write(&empty, b"").expect("empty file");
    assert!(matches!(
        service(&empty, "holder").work_held(at(41)),
        Err(StoreError::StoreNotInitialized)
    ));
    assert_eq!(std::fs::metadata(&empty).expect("empty file").len(), 0);

    // A process-default session reads without being registered.
    let fresh = process_default_session_at(20, at(41));
    let reader = LocalWorkService::new_with_attribution(
        database.clone(),
        ProjectId("held-project".into()),
        "agent".into(),
        fresh.clone(),
        None,
        None,
        WorkAttributionDefaults {
            actor: None,
            session: true,
        },
    );
    let nothing = reader.work_held(at(41)).expect("process-default read");
    assert_eq!(
        (nothing.items.len(), nothing.total, nothing.omitted),
        (0, 0, 0)
    );
    assert_eq!(nothing.focused_work_id, None);
    let registered: i64 = rusqlite::Connection::open(&database)
        .expect("state reader")
        .query_row(
            "SELECT COUNT(*) FROM work_session_state WHERE session_id = ?1",
            [&fresh.0],
            |row| row.get(0),
        )
        .expect("session rows");
    assert_eq!(registered, 0, "held must not register the session");
    assert_eq!(inspection_sensitive_state(&database), before);
}
