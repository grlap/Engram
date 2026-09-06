//! Bounded child-obligation groups shared by receipt renderers.

use super::{
    Guidance, Receipt, Serialize, StoreError, Value, VerbError, json, short_with_limit,
    terminal_safe_line,
};
use crate::work_service::{WorkChildFollowupPage, WorkChildObligations, WorkChildSummaryPage};

pub(super) use crate::work_service::MAX_CHILD_OBLIGATION_REFS;

/// Safe view of derived successor accounting. Proof hashes stay in the seal.
#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowChildSuccessor {
    #[serde(rename = "ref")]
    pub(super) work_ref: String,
    pub(super) lifecycle: super::WorkLifecycle,
    pub(super) disposition: &'static str,
    pub(super) reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) remedy: Option<String>,
}

impl ShowChildSuccessor {
    pub(super) fn for_work(work: &super::WorkItemSummary) -> Option<Self> {
        let state = work.required_child_successor.as_ref()?;
        let resolved = state.resolution.is_some();
        Some(Self {
            work_ref: super::short_ref_for_work_id(state.successor),
            lifecycle: state.lifecycle,
            disposition: if state.waived {
                "waived"
            } else if resolved {
                "resolved_by_successor"
            } else {
                "owed"
            },
            reason: state.reason,
            remedy: (!resolved && state.can_waive)
                .then(|| {
                    work.parent_id.map(|parent| {
                        format!(
                            "engram work update {} --waive {} --reason \"…\"",
                            super::short_ref_for_work_id(parent),
                            work.short_ref,
                        )
                    })
                })
                .flatten(),
        })
    }

    pub(super) fn line(&self) -> String {
        if self.disposition == "resolved_by_successor" {
            format!(
                "resolved by successor {} ({})",
                self.work_ref,
                super::lifecycle_word(self.lifecycle)
            )
        } else {
            format!(
                "successor {} ({}): {}",
                self.work_ref,
                super::lifecycle_word(self.lifecycle),
                self.reason
            )
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ChildObligationRow {
    #[serde(rename = "ref")]
    pub(super) work_ref: String,
    pub(super) title: String,
    pub(super) remedy: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) resolve_first: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) child_resolution: Option<ShowChildSuccessor>,
}

/// Navigation is a command, not a fixed verb, so scoped listing can replace
/// today's parent inspection without inventing a different group shape.
#[derive(Clone, Debug, Serialize)]
pub(super) struct ChildObligationGroup {
    pub(super) count: usize,
    pub(super) items: Vec<ChildObligationRow>,
    pub(super) omitted: usize,
    pub(super) navigation: String,
}

impl ChildObligationGroup {
    fn for_show(
        page: &WorkChildSummaryPage,
        parent: &super::WorkItemSummary,
        requirement: &str,
    ) -> Self {
        let parent_ref = &parent.short_ref;
        let items = page
            .items
            .iter()
            .map(|work| {
                let disposed = matches!(
                    work.lifecycle,
                    super::WorkLifecycle::Cancelled | super::WorkLifecycle::Superseded
                );
                let can_waive = disposed && parent.lifecycle == super::WorkLifecycle::Open;
                let child_resolution = ShowChildSuccessor::for_work(work);
                ChildObligationRow {
                    work_ref: work.short_ref.clone(),
                    title: short_with_limit(&work.title, super::MAX_COMPACT_TITLE_BYTES),
                    remedy: if can_waive {
                        format!(
                            "engram work update {parent_ref} --waive {} --reason \"…\"",
                            work.short_ref
                        )
                    } else {
                        format!("engram work show {}", work.short_ref)
                    },
                    resolve_first: disposed.then(|| {
                        if can_waive {
                            child_resolution.as_ref().map_or_else(
                                || "disposed required child still needs an explicit waiver".into(),
                                ShowChildSuccessor::line,
                            )
                        } else {
                            "parent is terminal; inspect retained child context".into()
                        }
                    }),
                    child_resolution,
                }
            })
            .collect::<Vec<_>>();
        Self {
            count: page.total,
            omitted: page.total.saturating_sub(items.len()),
            items,
            navigation: format!(
                "engram work ls --under {parent_ref} --{requirement}{}",
                if page.includes_disposed { " --all" } else { "" }
            ),
        }
    }

    pub(super) fn lines(&self, label: &str) -> Vec<String> {
        let mut lines = vec![format!(
            "{label} ({} of {} shown):",
            self.items.len(),
            self.count
        )];
        for row in &self.items {
            lines.push(format!(
                "  {} \"{}\"",
                row.work_ref,
                terminal_safe_line(&row.title)
            ));
            if let Some(reason) = &row.resolve_first {
                lines.push(format!("    resolve first: {}", terminal_safe_line(reason)));
            }
            lines.push(format!("    {}", row.remedy));
        }
        if self.omitted > 0 {
            lines.push(format!("  ({} more {label} not shown)", self.omitted));
        }
        lines.push(format!("  inspect: {}", self.navigation));
        lines
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ShowChildObligations {
    required_owed: ChildObligationGroup,
    open_optional: ChildObligationGroup,
}

impl ShowChildObligations {
    pub(super) fn new(groups: &WorkChildObligations, parent: &super::WorkItemSummary) -> Self {
        Self {
            required_owed: ChildObligationGroup::for_show(
                &groups.required_owed,
                parent,
                "required",
            ),
            open_optional: ChildObligationGroup::for_show(
                &groups.open_optional,
                parent,
                "optional",
            ),
        }
    }

    pub(super) fn lines(&self) -> Vec<String> {
        let mut lines = self.required_owed.lines("required children still owed");
        lines.extend(self.open_optional.lines("open optional follow-ups"));
        lines.push("optional children do not block completion".into());
        lines
    }
}

pub(super) fn done_with_child_obligations(
    lines: Vec<String>,
    guidance: Guidance,
    value: Value,
    children: Result<WorkChildFollowupPage, StoreError>,
    parent_ref: &str,
    budget: usize,
) -> Result<Receipt, VerbError> {
    let navigation = format!("engram work show {parent_ref}");
    let page = match children {
        Ok(page) if page.total == 0 => return Ok(Receipt::assemble(lines, guidance, value, false)),
        Ok(page) => page,
        Err(error) => {
            // Completion already committed. A diagnostic failure must never
            // relabel success as refusal or claim there are no remaining rows.
            let class = advisory_error_class(&error);
            let mut lines = lines;
            lines.push(format!(
                "remaining optional children unavailable ({class}); {navigation}"
            ));
            let mut value = value;
            value["child_obligations_unavailable"] = json!(true);
            value["child_obligations_error_class"] = json!(class);
            return Ok(Receipt::assemble(lines, guidance, value, false));
        }
    };
    let items = page
        .items
        .into_iter()
        .map(|child| {
            let (resolve_first, remedy) = match child.refusal {
                Some((reason, remedy)) => (Some(reason), remedy),
                None => (None, super::handlers::detach_command(&child.work.short_ref)),
            };
            ChildObligationRow {
                work_ref: child.work.short_ref,
                title: short_with_limit(&child.work.title, super::MAX_COMPACT_TITLE_BYTES),
                remedy,
                resolve_first,
                child_resolution: None,
            }
        })
        .collect::<Vec<_>>();
    let mut group = ChildObligationGroup {
        count: page.total,
        omitted: page.total.saturating_sub(items.len()),
        items,
        navigation,
    };
    loop {
        let mut rendered_lines = lines.clone();
        rendered_lines.extend(group.lines("open optional children"));
        rendered_lines.push("  broader blocked-work list: engram work ls --blocked".into());
        rendered_lines.push("  optional children do not block this completion".into());
        let mut rendered_value = value.clone();
        rendered_value["child_obligations"] = json!({"open_optional": group});
        let receipt = Receipt::assemble(rendered_lines, guidance.clone(), rendered_value, false);
        if receipt.text().len() <= budget
            && serde_json::to_vec_pretty(&receipt.value)?.len() <= budget
        {
            return Ok(receipt);
        }
        if group.items.pop().is_some() {
            group.omitted += 1;
        } else {
            // Never drop completion evidence or relabel a committed success
            // to satisfy a budget smaller than the original receipt plus the
            // fixed count/navigation metadata. Only advisory rows are shed.
            return Ok(receipt);
        }
    }
}

/// A fixed diagnostic class, never an error body, path, hash, or actor text.
fn advisory_error_class(error: &StoreError) -> &'static str {
    match error {
        StoreError::InvalidWorkProjection(_) => "work_projection_invalid",
        StoreError::Sqlite(_) => "sqlite_error",
        StoreError::Json(_) => "stored_json_invalid",
        StoreError::HashMismatch { .. }
        | StoreError::NonCanonicalObject(_)
        | StoreError::ImmutableCollision(_)
        | StoreError::ObjectKindMismatch { .. }
        | StoreError::InvalidStoredHash(_) => "canonical_object_invalid",
        // Unclassified failures stay unavailable, not a corruption claim.
        _ => "store_error",
    }
}
