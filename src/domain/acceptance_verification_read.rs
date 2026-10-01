//! A host's read, for one acceptance criterion of an item on its active run,
//! of every host verification of the criterion's bound kind that the run
//! recorded up to a pinned run-feed cut, in feed order. It reads records as
//! stored and computes nothing about them: no freshness, applicability,
//! source currency or satisfaction. Which candidate answers for the criterion
//! is the consumer's judgment.

use serde::{Deserialize, Serialize};

use super::{AcceptanceBindingReadBasis, AcceptanceBindingVerification, VerificationRequirement};

/// The most verification rows one page holds.
pub const ACCEPTANCE_VERIFICATION_READ_PAGE_ROWS: usize = 8;

/// The most bytes one page's serialized result holds.
pub const ACCEPTANCE_VERIFICATION_READ_PAGE_BYTES: usize = 16 * 1_024;

/// One page of the read. Rows are complete and in ascending run-feed
/// position; `continuation` is `None` exactly when no row remains.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceVerificationPage {
    /// The item, revision, run and cut every page of the read is pinned to.
    pub basis: AcceptanceBindingReadBasis,
    /// The one-based authored criterion read.
    pub criterion: usize,
    /// The criterion's bound requirement, or `None` for an unbound one,
    /// which has no candidates. An empty page is never a pass.
    pub requirement: Option<VerificationRequirement>,
    /// Every verification of the bound kind on the run at the cut.
    pub total: usize,
    /// Candidates on earlier pages.
    pub earlier: usize,
    /// Candidates on this page.
    pub shown: usize,
    /// Candidates on later pages.
    pub omitted: usize,
    /// Each candidate as recorded: every result, fingerprint and source
    /// revision, including checks recorded before the criterion's latest
    /// obligation or at an older item revision on this run.
    pub rows: Vec<AcceptanceBindingVerification>,
    pub continuation: Option<String>,
}

/// Why a read was refused. A refusal is never an empty result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceVerificationReadRefusal {
    /// The store holds no item with this id.
    UnknownWork,
    /// The item belongs to another project.
    WrongProject,
    /// The item's revision is not the expected one.
    WrongRevision,
    /// The run is not the item's active run.
    WrongRun,
    /// The run's feed is not at the requested cut, on a first page or a
    /// continuation.
    StaleCut,
    /// The criterion is not one of the item's authored criteria.
    InvalidCriterion,
    /// The continuation is not a position this read can resume at.
    InvalidCursor,
    /// The continuation was made for another item, revision, run, cut,
    /// criterion or requirement.
    CursorBasisMismatch,
    /// One complete row does not fit a page.
    PageTooLarge,
}

impl AcceptanceVerificationReadRefusal {
    /// The host error code this refusal answers with.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnknownWork => "acceptance_verification_read_unknown_work",
            Self::WrongProject => "acceptance_verification_read_wrong_project",
            Self::WrongRevision => "acceptance_verification_read_wrong_revision",
            Self::WrongRun => "acceptance_verification_read_wrong_run",
            Self::StaleCut => "acceptance_verification_read_stale_cut",
            Self::InvalidCriterion => "acceptance_verification_read_invalid_criterion",
            Self::InvalidCursor => "acceptance_verification_read_invalid_cursor",
            Self::CursorBasisMismatch => "acceptance_verification_read_cursor_basis_mismatch",
            Self::PageTooLarge => "acceptance_verification_read_page_too_large",
        }
    }
}
