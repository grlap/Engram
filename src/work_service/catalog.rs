use super::*;
use crate::domain::WorkCatalogReadCut;
use crate::storage::ListingExpectation;

const LISTING_CURSOR_PREFIX: &str = "c1-";
const MAX_LISTING_CURSOR_BYTES: usize = 8192;
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Self-describing navigation: encoded filters/identity are not confidential.
/// No authorization, signature, or server-side state. Listing-owned sparse DTO;
/// record windows keep the shared hex `continuation` helper.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ListingCursor {
    project: ProjectId,
    #[serde(default, skip_serializing_if = "listing_filters_are_empty")]
    filters: ListingFilters,
    after: WorkId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after_priority: Option<i32>,
    basis: ListingBasis,
}

/// What a continuation is checked against. A token without one is refused.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ListingBasis {
    /// `ls`: when the page was read and the fingerprint of the listing's
    /// complete selected sequence then; a continuation is refused when the
    /// sequence differs or the clock went back.
    Membership {
        observed_at: DateTime<Utc>,
        fingerprint: String,
    },
    /// Compact `next`'s ready navigation, which reads no complete sequence:
    /// its project cut, refused by any project write or the next
    /// project-wide time transition.
    ProjectCut { cut: WorkCatalogReadCut },
}

/// Normalized listing filters without the wire-nulls and zeroed seek fields
/// that `WorkCatalogQuery` still emits for host-core identity.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ListingFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    search: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lifecycles: Vec<WorkLifecycle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    availabilities: Vec<WorkAvailability>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    blocked_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assigned_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    held_by: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_id: Option<WorkId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child_requirement: Option<ChildRequirement>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    ready_priority_order: bool,
}

impl ListingBasis {
    fn expectation(&self) -> ListingExpectation<'_> {
        match self {
            Self::Membership {
                observed_at,
                fingerprint,
            } => ListingExpectation::Membership {
                observed_at: *observed_at,
                fingerprint,
            },
            Self::ProjectCut { cut } => ListingExpectation::ProjectCut(cut),
        }
    }
}

fn listing_filters_are_empty(filters: &ListingFilters) -> bool {
    filters == &ListingFilters::default()
}

impl ListingFilters {
    fn from_query(query: &WorkCatalogQuery) -> Self {
        Self {
            search: query.search.clone(),
            lifecycles: query.lifecycles.clone(),
            availabilities: query.availabilities.clone(),
            blocked_only: query.blocked_only,
            assigned_to: query.assigned_to.clone(),
            held_by: query.held_by.clone(),
            label: query.label.clone(),
            parent_id: query.parent_id,
            child_requirement: query.child_requirement,
            ready_priority_order: query.ready_priority_order,
        }
    }

    fn to_query(&self) -> WorkCatalogQuery {
        WorkCatalogQuery {
            search: self.search.clone(),
            lifecycles: self.lifecycles.clone(),
            availabilities: self.availabilities.clone(),
            blocked_only: self.blocked_only,
            assigned_to: self.assigned_to.clone(),
            held_by: self.held_by.clone(),
            label: self.label.clone(),
            parent_id: self.parent_id,
            child_requirement: self.child_requirement,
            after: None,
            after_priority: None,
            ready_priority_order: self.ready_priority_order,
            limit: 0,
        }
    }
}

pub(crate) struct WorkListingPage {
    pub items: Vec<ReadyWorkSummary>,
    pub total: usize,
    pub preceding: usize,
    pub claims: Vec<WorkClaim>,
    project: ProjectId,
    filters: WorkCatalogQuery,
    observed_at: DateTime<Utc>,
    membership: String,
}

impl WorkListingPage {
    /// The renderer supplies the final emitted key, never the fetched sentinel.
    pub(crate) fn continuation(
        &self,
        after: WorkId,
        after_priority: i32,
    ) -> Result<String, StoreError> {
        listing_continuation(
            &self.project,
            &self.filters,
            after,
            after_priority,
            ListingBasis::Membership {
                observed_at: self.observed_at,
                fingerprint: self.membership.clone(),
            },
        )
    }
}

/// Filters must already be normalized by the query's owner. `basis` is the
/// listing's sequence fingerprint, or compact `next`'s project cut.
pub(super) fn listing_continuation(
    project: &ProjectId,
    filters: &WorkCatalogQuery,
    after: WorkId,
    after_priority: i32,
    basis: ListingBasis,
) -> Result<String, StoreError> {
    let cursor = ListingCursor {
        project: project.clone(),
        filters: ListingFilters::from_query(filters),
        after,
        after_priority: filters.ready_priority_order.then_some(after_priority),
        basis,
    };
    encode_listing_bytes(&serde_json::to_vec(&cursor)?).ok_or_else(|| {
        invalid("listing continuation metadata is too large; shorten search, label or parent scope")
    })
}

impl LocalWorkService {
    pub(crate) fn work_catalog_page(
        &self,
        query: &WorkCatalogQuery,
        under: Option<&str>,
        after: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkListingPage, StoreError> {
        let cursor = after.map(decode_cursor).transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.project != self.project_id)
        {
            return Err(invalid(
                "continuation belongs to different filters or project",
            ));
        }
        let store = self.read_store_at(now)?;
        store.work_read_snapshot(|store| {
            let mut query = query.clone();
            if let Some(parent) = under {
                query.parent_id = Some(store.resolve_work_ref(&self.project_id, parent)?.work_id);
            }
            let mut filters = query.clone();
            SqliteStore::normalize_catalog_filters(&mut filters);
            if let Some(cursor) = &cursor {
                if cursor.filters.to_query() != filters {
                    return Err(invalid(
                        "continuation belongs to different filters or project",
                    ));
                }
                if cursor.filters.ready_priority_order != cursor.after_priority.is_some() {
                    return Err(invalid("continuation item no longer matches this listing"));
                }
                query.after = Some(cursor.after);
                query.after_priority = cursor.after_priority;
            }
            let (page, total, preceding, claims, membership) = store
                .query_work_catalog_continuation(
                    &self.project_id,
                    now,
                    &query,
                    cursor.as_ref().map(|cursor| cursor.basis.expectation()),
                )?;
            Ok(WorkListingPage {
                items: page
                    .items
                    .into_iter()
                    .map(|item| {
                        let successor = store.required_child_successor(&item.work)?;
                        let mut summary = ready_work_summary(item);
                        summary.work.required_child_successor = successor;
                        Ok(summary)
                    })
                    .collect::<Result<_, StoreError>>()?,
                total,
                preceding,
                claims,
                project: self.project_id.clone(),
                filters,
                observed_at: now,
                membership,
            })
        })
    }
}

fn decode_cursor(token: &str) -> Result<ListingCursor, StoreError> {
    let bytes = decode_listing_bytes(token)
        .ok_or_else(|| invalid("invalid listing cursor; use the fresh listing command"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| invalid("invalid listing cursor; use the fresh listing command"))
}

fn encode_listing_bytes(bytes: &[u8]) -> Option<String> {
    let mut token = String::from(LISTING_CURSOR_PREFIX);
    token.push_str(&encode_base64url(bytes));
    (token.len() <= MAX_LISTING_CURSOR_BYTES).then_some(token)
}

fn decode_listing_bytes(token: &str) -> Option<Vec<u8>> {
    if token.len() > MAX_LISTING_CURSOR_BYTES {
        return None;
    }
    decode_base64url(token.strip_prefix(LISTING_CURSOR_PREFIX)?)
}

fn encode_base64url(bytes: &[u8]) -> String {
    let (chunks, remainder) = bytes.as_chunks::<3>();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        push_base64url_bits(&mut out, n, 4);
    }
    match remainder {
        [b0] => push_base64url_bits(&mut out, u32::from(*b0) << 16, 2),
        [b0, b1] => {
            push_base64url_bits(&mut out, (u32::from(*b0) << 16) | (u32::from(*b1) << 8), 3);
        }
        _ => {}
    }
    out
}

fn push_base64url_bits(out: &mut String, n: u32, count: usize) {
    let shifts = [18, 12, 6, 0];
    for shift in shifts.into_iter().take(count) {
        let index = usize::from(masked_u8((n >> shift) & 63));
        out.push(BASE64URL[index] as char);
    }
}

fn masked_u8(n: u32) -> u8 {
    u8::try_from(n).unwrap_or(0)
}

fn base64url_value(byte: u8) -> Option<u32> {
    Some(match byte {
        b'A'..=b'Z' => u32::from(byte - b'A'),
        b'a'..=b'z' => u32::from(byte - b'a' + 26),
        b'0'..=b'9' => u32::from(byte - b'0' + 52),
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    })
}

fn decode_base64url(encoded: &str) -> Option<Vec<u8>> {
    if encoded.is_empty()
        || encoded.len() % 4 == 1
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return None;
    }
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(encoded.len() * 3 / 4);
    let mut index = 0;
    while index < bytes.len() {
        let remaining = bytes.len() - index;
        if remaining >= 4 {
            let n = (base64url_value(bytes[index])? << 18)
                | (base64url_value(bytes[index + 1])? << 12)
                | (base64url_value(bytes[index + 2])? << 6)
                | base64url_value(bytes[index + 3])?;
            out.push(masked_u8((n >> 16) & 0xff));
            out.push(masked_u8((n >> 8) & 0xff));
            out.push(masked_u8(n & 0xff));
            index += 4;
        } else if remaining == 2 {
            let n =
                (base64url_value(bytes[index])? << 18) | (base64url_value(bytes[index + 1])? << 12);
            out.push(masked_u8((n >> 16) & 0xff));
            index += 2;
        } else if remaining == 3 {
            let n = (base64url_value(bytes[index])? << 18)
                | (base64url_value(bytes[index + 1])? << 12)
                | (base64url_value(bytes[index + 2])? << 6);
            out.push(masked_u8((n >> 16) & 0xff));
            out.push(masked_u8((n >> 8) & 0xff));
            index += 3;
        } else {
            return None;
        }
    }
    Some(out)
}

#[cfg(test)]
pub(crate) fn listing_cursor_json(token: &str) -> Result<serde_json::Value, StoreError> {
    let bytes = decode_listing_bytes(token)
        .ok_or_else(|| invalid("invalid listing cursor; use the fresh listing command"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| invalid("invalid listing cursor; use the fresh listing command"))
}

#[cfg(test)]
pub(crate) fn encode_listing_cursor_json(value: &serde_json::Value) -> Result<String, StoreError> {
    encode_listing_bytes(&serde_json::to_vec(value)?).ok_or_else(|| {
        invalid("listing continuation metadata is too large; shorten search, label or parent scope")
    })
}

fn invalid(reason: &str) -> StoreError {
    StoreError::WorkCatalogCursorInvalid {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod listing_cursor_tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn sample_cut() -> WorkCatalogReadCut {
        WorkCatalogReadCut {
            project_position: 12,
            observed_at: Utc.with_ymd_and_hms(2026, 9, 1, 18, 0, 0).single().unwrap(),
            valid_until_ms: Some(30_000),
        }
    }

    fn sample_membership() -> ListingBasis {
        ListingBasis::Membership {
            observed_at: sample_cut().observed_at,
            fingerprint: "sample-membership".into(),
        }
    }

    // A membership token carries only its observed time beside the
    // fingerprint; the project position and expiry stay in the project cut.
    #[test]
    fn membership_tokens_carry_no_project_cut() {
        let token = listing_continuation(
            &ProjectId("customer-workflow".into()),
            &default_ready_filters(),
            sample_after(),
            3,
            sample_membership(),
        )
        .expect("membership token");
        let basis = listing_cursor_json(&token).expect("json")["basis"].clone();
        let mut keys: Vec<_> = basis.as_object().expect("basis").keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["fingerprint", "kind", "observed_at"]);
        let token = listing_continuation(
            &ProjectId("customer-workflow".into()),
            &default_ready_filters(),
            sample_after(),
            3,
            ListingBasis::ProjectCut { cut: sample_cut() },
        )
        .expect("project-cut token");
        let cursor = listing_cursor_json(&token).expect("json");
        assert_eq!(
            cursor["basis"]["cut"],
            serde_json::to_value(sample_cut()).expect("cut")
        );
        assert!(cursor.get("cut").is_none());
    }

    fn sample_after() -> WorkId {
        WorkId(uuid::Uuid::from_u128(1))
    }

    fn sample_filters() -> WorkCatalogQuery {
        let mut filters = WorkCatalogQuery {
            search: Some("find matching work".into()),
            lifecycles: vec![WorkLifecycle::Open],
            label: Some("gate".into()),
            parent_id: Some(sample_after()),
            ready_priority_order: true,
            limit: 20,
            ..WorkCatalogQuery::default()
        };
        SqliteStore::normalize_catalog_filters(&mut filters);
        filters
    }

    fn default_ready_filters() -> WorkCatalogQuery {
        let mut filters = WorkCatalogQuery {
            lifecycles: vec![WorkLifecycle::Open],
            availabilities: vec![WorkAvailability::Ready],
            ready_priority_order: true,
            limit: 20,
            ..WorkCatalogQuery::default()
        };
        SqliteStore::normalize_catalog_filters(&mut filters);
        filters
    }

    fn assert_roundtrip(filters: &WorkCatalogQuery, after_priority: i32) {
        let token = listing_continuation(
            &ProjectId("customer-workflow".into()),
            filters,
            sample_after(),
            after_priority,
            sample_membership(),
        )
        .expect("dense token");
        let decoded = decode_cursor(&token).expect("roundtrip");
        assert_eq!(decoded.filters.to_query(), *filters);
        assert_eq!(decoded.after, sample_after());
        assert_eq!(
            decoded.after_priority,
            filters.ready_priority_order.then_some(after_priority)
        );
        assert!(matches!(
            decoded.basis,
            ListingBasis::Membership { fingerprint, observed_at }
                if fingerprint == "sample-membership" && observed_at == sample_cut().observed_at
        ));
    }

    // A token without its basis, as an earlier build minted, is refused
    // rather than read on another basis.
    #[test]
    fn listing_cursor_without_a_basis_is_refused() {
        let token = listing_continuation(
            &ProjectId("customer-workflow".into()),
            &sample_filters(),
            sample_after(),
            3,
            sample_membership(),
        )
        .expect("dense token");
        let mut cursor: serde_json::Value =
            serde_json::from_slice(&decode_listing_bytes(&token).expect("bytes")).expect("json");
        cursor.as_object_mut().expect("object").remove("basis");
        let legacy =
            encode_listing_bytes(&serde_json::to_vec(&cursor).expect("json")).expect("token");
        assert!(matches!(
            decode_cursor(&legacy),
            Err(StoreError::WorkCatalogCursorInvalid { .. })
        ));
    }

    #[test]
    fn listing_cursor_roundtrips_filters_and_position() {
        assert_roundtrip(&sample_filters(), 3);
        assert_roundtrip(&default_ready_filters(), 3);
    }

    #[test]
    fn listing_base64url_roundtrips_every_remainder_and_refuses_invalid() {
        for len in 1..=8 {
            let bytes: Vec<u8> = (0..len).map(|index| index * 17).collect();
            let encoded = encode_base64url(&bytes);
            assert_eq!(
                decode_base64url(&encoded).as_deref(),
                Some(bytes.as_slice())
            );
        }
        assert!(decode_base64url("A").is_none());
        assert!(decode_base64url("+++").is_none());
        assert!(decode_base64url("====").is_none());
    }
}
