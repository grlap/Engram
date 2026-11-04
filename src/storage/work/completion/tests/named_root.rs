use super::*;
mod git_tree;
use crate::domain::{
    AcceptanceBinding, ExecutionSourceBasis, NamedRootBindingKind, SourceRootState,
    VerificationKind, VerificationRequirement, VerificationResult,
};
use crate::storage::test_support::open_work_host;

mod assessment;
mod binding_lifecycle;
mod disposition;
mod fixtures;
mod freshness;
mod integrity;
mod operator_guidance;
mod stock_reminder;

use fixtures::{
    Pick, Unbind, active_generation, basis, begin, end_root, forge_resolution, name_root,
    name_root_in, named_work, named_work_in, obligation, operator_obligation, pinned_rule,
    pinned_store, refused_on, release, select_rules, stock_completion_action,
};
