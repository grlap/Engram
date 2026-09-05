use super::*;
use crate::domain::WorkCatalogReadCut;

const MAX_CURSOR_BYTES: usize = 8192;

/// Self-describing navigation: encoded filters/identity are not confidential.
/// No authorization, signature, or server-side state.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ListingCursor {
    project: ProjectId,
    filters: WorkCatalogQuery,
    cut: WorkCatalogReadCut,
    after: WorkId,
}

pub(crate) struct WorkListingPage {
    pub items: Vec<ReadyWorkSummary>,
    pub total: usize,
    pub preceding: usize,
    pub claims: Vec<WorkClaim>,
    project: ProjectId,
    filters: WorkCatalogQuery,
    cut: WorkCatalogReadCut,
}

impl WorkListingPage {
    /// The renderer supplies the final emitted key, never the fetched sentinel.
    pub(crate) fn continuation(&self, after: WorkId) -> Result<String, StoreError> {
        let cursor = ListingCursor {
            project: self.project.clone(),
            filters: self.filters.clone(),
            cut: self.cut.clone(),
            after,
        };
        let bytes = serde_json::to_vec(&cursor)?;
        let mut token = String::from("c1-");
        for byte in bytes {
            write!(token, "{byte:02x}")
                .map_err(|_| invalid("cannot encode listing continuation"))?;
        }
        if token.len() > MAX_CURSOR_BYTES {
            return Err(invalid(
                "listing continuation metadata is too large; shorten search, label or parent scope",
            ));
        }
        Ok(token)
    }
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
        let store = self.store_at(now)?;
        store.work_read_snapshot(|store| {
            let mut query = query.clone();
            if let Some(parent) = under {
                query.parent_id = Some(store.resolve_work_ref(&self.project_id, parent)?.work_id);
            }
            let mut filters = query.clone();
            SqliteStore::normalize_catalog_filters(&mut filters);
            if let Some(cursor) = &cursor {
                if cursor.filters != filters {
                    return Err(invalid(
                        "continuation belongs to different filters or project",
                    ));
                }
                query.after = Some(cursor.after);
            }
            let (page, total, preceding, claims, cut) = store.query_work_catalog_continuation(
                &self.project_id,
                now,
                &query,
                cursor.as_ref().map(|cursor| &cursor.cut),
            )?;
            Ok(WorkListingPage {
                items: page.items.into_iter().map(ready_work_summary).collect(),
                total,
                preceding,
                claims,
                project: self.project_id.clone(),
                filters,
                cut,
            })
        })
    }
}

fn decode_cursor(token: &str) -> Result<ListingCursor, StoreError> {
    let malformed = || invalid("invalid listing cursor; use the fresh listing command");
    if token.len() > MAX_CURSOR_BYTES {
        return Err(malformed());
    }
    let encoded = token.strip_prefix("c1-").ok_or_else(malformed)?;
    if encoded.len() % 2 != 0 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(malformed());
    }
    let bytes = encoded
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |byte: u8| {
                if byte.is_ascii_digit() {
                    byte - b'0'
                } else {
                    byte.to_ascii_lowercase() - b'a' + 10
                }
            };
            digit(pair[0]) * 16 + digit(pair[1])
        })
        .collect::<Vec<_>>();
    serde_json::from_slice(&bytes).map_err(|_| malformed())
}

fn invalid(reason: &str) -> StoreError {
    StoreError::WorkCatalogCursorInvalid {
        reason: reason.into(),
    }
}
