//! `work ls --page-bytes`: larger JSON listing pages for a program reading
//! the CLI, apart from the agent budget every other surface keeps.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use engram::{AgentVerbs, LsInput, Receipt, VerbError};

/// From the agent protocol budget through the JSON page ceiling. Clap needs
/// the bounds as `i64`; the assertion keeps them equal to the library's
/// constants.
pub(crate) const PAGE_BYTES_RANGE: std::ops::RangeInclusive<i64> = 12_288..=1_048_576;
const _: () = assert!(
    engram::work_service::MAX_AGENT_WORK_RESPONSE_BYTES == 12_288
        && engram::work_service::MAX_LS_PAGE_BYTES == 1_048_576
);

/// `ls` as before without `--page-bytes`; with it, the larger JSON page.
/// The option is refused without `--json`, before any store is read.
pub(crate) fn ls(
    verbs: &AgentVerbs,
    input: &LsInput,
    now: DateTime<Utc>,
    json: bool,
    page_bytes: Option<u32>,
) -> Result<Result<Receipt, VerbError>> {
    match page_bytes {
        None => Ok(verbs.ls(input, now)),
        Some(_) if !json => bail!(
            "--page-bytes applies only to JSON output: add --json, or omit --page-bytes for the default page budget"
        ),
        Some(page_bytes) => Ok(verbs.ls_with_page_bytes(
            input,
            now,
            usize::try_from(page_bytes).context("--page-bytes exceeds this platform")?,
        )),
    }
}
