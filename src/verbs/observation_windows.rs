//! The explicit source-observation window of `show`, fitted to the agent
//! response budget like the evaluations window.

use super::{AgentVerbs, DateTime, Guidance, Receipt, StoreError, Utc, Value, VerbError, json};
use std::fmt::Write as _;

use crate::work_service::{WorkObservationRow, WorkObservationWindow};

impl AgentVerbs {
    /// The bounded source-observation window of the item's run.
    pub(super) fn show_observations(
        &self,
        work_ref: &str,
        after: Option<&str>,
        now: DateTime<Utc>,
        budget: usize,
    ) -> Result<Receipt, VerbError> {
        let command = format!(
            "engram work show {} --observations",
            super::record_windows::safe_reference_argument(work_ref)
        );
        let page = self
            .service
            .work_observation_window(work_ref, after, now)
            .map_err(|error| VerbError::for_listing(error, &command))?;
        fit_window(&page, budget)
    }
}

/// The largest number of newest rows that fits, never fewer than one when the
/// run has an observation. A row's stored text is bounded, so one row fits.
fn fit_window(page: &WorkObservationWindow, budget: usize) -> Result<Receipt, VerbError> {
    let first = usize::from(!page.rows.is_empty());
    let mut best = render_window(page, first, budget)?;
    if !super::receipts::agent_receipt_fits(&best, budget)? {
        return Err(VerbError::at(
            StoreError::InvalidWorkProjection(
                "observations window exceeds the agent response byte budget".into(),
            ),
            &page.short_ref,
        ));
    }
    let (mut lower, mut upper) = (first, page.rows.len());
    while lower < upper {
        let visible = lower + (upper - lower).div_ceil(2);
        let candidate = render_window(page, visible, budget)?;
        if super::receipts::agent_receipt_fits(&candidate, budget)? {
            best = candidate;
            lower = visible;
        } else {
            upper = visible - 1;
        }
    }
    Ok(best)
}

fn render_window(
    page: &WorkObservationWindow,
    visible: usize,
    budget: usize,
) -> Result<Receipt, VerbError> {
    let work_ref = &page.short_ref;
    let command = format!("engram work show {work_ref} --observations");
    let after = page
        .continuation(visible)
        .map_err(|error| VerbError::at(error, work_ref))?;
    let older = page.total - page.newer - visible;
    let omitted = page.total - visible;
    // Selected newest first; shown oldest to newest.
    let shown = page.rows[..visible].iter().rev().collect::<Vec<_>>();
    let mut next = Vec::new();
    if let Some(after) = &after {
        next.push(format!("{command} --after {after}"));
    }
    next.push(format!("engram work show {work_ref}"));
    if page.title_truncated {
        next.push(format!("engram work show {work_ref} --full"));
    }
    let cut = page.read_cut();
    let mut lines = vec![
        format!("{work_ref} \"{}\"", super::short(&page.title)),
        format!(
            "observations: window {visible} of {}; {omitted} omitted ({older} older, {} newer); oldest to newest within window",
            page.total, page.newer
        ),
        format!(
            "  byte budget: {budget}; read cut: project position {}, observed at {}, valid until ms {}",
            cut.project_position,
            cut.observed_at.to_rfc3339(),
            cut.valid_until_ms
                .map_or_else(|| "none".into(), |until| until.to_string())
        ),
    ];
    if page.total == 0 {
        lines.push("  no source observation is recorded on this item's run".into());
    }
    for row in &shown {
        if let Some(comparison) = &row.sealed_comparison {
            let root_state = match row.root_state {
                Some(crate::domain::SourceRootState::Named) => "named",
                Some(crate::domain::SourceRootState::Ended) => "ended",
                None => "not recorded",
            };
            lines.push(format!("  sealed comparison: {}; recorded after seal cut; original observation binding run {}, claim {}, fence {}; root state {}", comparison, row.binding.run_id.0, row.binding.claim_id.0, row.binding.claim_fence, root_state));
        }
        lines.push(row_line(row));
    }
    // This page is the observation detail that show and next point into, so
    // its diagnostic names no observation-detail command; it pages through
    // the window's own continuation and next commands.
    let sealed_source = page.sealed_diagnostic().map(|diagnostic| {
        let mut rendered = super::sealed_source::SourceDiagnostic::new(
            &crate::work_service::SealedSourceView {
                seal: diagnostic.seal.clone(),
                diagnostic: Some(diagnostic.clone()),
                unavailable: None,
                continuation: None,
                details_omitted: false,
            },
            work_ref,
        );
        if let Some(object) = rendered.value.as_object_mut() {
            object.remove("observations_detail");
        }
        rendered
    });
    if let Some(diagnostic) = &sealed_source {
        lines.extend(diagnostic.lines.iter().cloned());
    }
    let mut work = json!({ "short_ref": work_ref, "title": page.title });
    if page.title_truncated {
        work["title_truncated"] = json!(true);
        work["title_bytes"] = json!(page.title_bytes);
    }
    let value = json!({
        "work": work,
        "sealed_source": sealed_source.map(|diagnostic| diagnostic.value),
        "observations": shown.iter().map(|row| row_value(row)).collect::<Vec<_>>(),
        "observations_window": {
            "selection": "newest_first",
            "order": "oldest_first",
            "total": page.total,
            "shown": visible,
            "omitted": omitted,
            "older": older,
            "newer": page.newer,
            "after": after,
            "byte_budget": budget,
            "read_cut": cut,
        },
        "next": next,
    });
    Ok(Receipt::assemble(
        lines,
        Guidance {
            reminders: Vec::new(),
            next,
        },
        value,
        false,
    ))
}

fn row_line(row: &WorkObservationRow) -> String {
    if let Some(detail) = &row.unadmitted {
        return unadmitted_row_line(row, detail);
    }
    let field = |value: Option<&String>| {
        value.map_or_else(
            || "not recorded".to_owned(),
            |value| super::terminal_safe_line(value),
        )
    };
    format!(
        "  run position {}: {} {}; workspace {}; revision {}{}; reported by {}; observed {}; recorded {}",
        row.position,
        row.observation,
        if row.source_changed == Some(true) {
            "change"
        } else {
            "sighting"
        },
        field(row.workspace.as_ref()),
        field(row.revision.as_ref()),
        row.root_generation
            .map_or_else(String::new, |generation| format!(
                "; root generation {generation}"
            )),
        super::terminal_safe_line(&row.reporting_session),
        row.observed_at
            .map_or_else(|| "at a time not recorded".to_owned(), |at| at.to_rfc3339()),
        row.recorded_at.to_rfc3339(),
    )
}

/// An unadmitted observation's line: what was seen and when, that it was not
/// admitted, its cause as known, and every check as uncredited.
fn unadmitted_row_line(
    row: &WorkObservationRow,
    detail: &crate::work_service::UnadmittedObservationDetail,
) -> String {
    let safe = super::terminal_safe_line;
    let mut line = format!(
        "  run position {}: {} unadmitted observation: {}; revision {}{}; {}; {}; reported by {}; observed {} to {}; recorded {}",
        row.position,
        row.observation,
        safe(&detail.occurrence),
        row.revision
            .as_deref()
            .map_or_else(|| "not recorded".to_owned(), safe),
        row.root_generation
            .map_or_else(String::new, |generation| format!(
                "; root generation {generation}"
            )),
        safe(&detail.cause),
        detail.accounting,
        safe(&row.reporting_session),
        detail.observed_from.to_rfc3339(),
        detail.observed_through.to_rfc3339(),
        row.recorded_at.to_rfc3339(),
    );
    for check in &detail.checks {
        let _ = write!(
            line,
            "\n    observed check, uncredited: {} {} {}; {}; source {}{}",
            safe(&check.host_check_id),
            check.kind,
            check.result,
            check.finished_at.map_or_else(
                || "not finished".to_owned(),
                |at| format!("finished {}", at.to_rfc3339())
            ),
            check.source_revision.as_deref().map_or_else(
                || "unknown".to_owned(),
                |revision| format!("revision {}", safe(revision))
            ),
            check
                .evidence_ref
                .as_deref()
                .map_or_else(String::new, |reference| format!(
                    "; host ref {}",
                    safe(reference)
                )),
        );
    }
    line
}

fn row_value(row: &WorkObservationRow) -> Value {
    json!({
        "observation": row.observation,
        "run_position": row.position,
        "sealed_comparison": row.sealed_comparison,
        "observation_binding": row.binding,
        "root_state": row.root_state,
        "admission": row.admission,
        "source_changed": row.source_changed,
        "workspace": row.workspace,
        "revision": row.revision,
        "root_generation": row.root_generation,
        "reporting_session": row.reporting_session,
        "observed_at": row.observed_at,
        "recorded_at": row.recorded_at,
        "unadmitted": row.unadmitted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SessionId;
    use crate::storage::sealed_source_transport_fixture;
    use crate::work_service::{ObservedCheckRow, UnadmittedObservationDetail};

    #[test]
    fn sealed_source_text_window_keeps_maximum_unadmitted_checks_with_disclosures() {
        let fixture = sealed_source_transport_fixture("difference");
        let words = AgentVerbs::new(
            fixture.database.clone(),
            fixture.work.project_id.clone(),
            "runner".into(),
            SessionId("runner".into()),
            None,
        );
        let now = Utc::now();
        let shown = words.show(&fixture.work.short_ref, now).unwrap();
        let command = shown.value["sealed_source"]["observations_detail"]
            .as_str()
            .unwrap();
        let token = command.split(" --after ").nth(1).unwrap();
        let mut page = words
            .service
            .work_observation_window(&fixture.work.short_ref, Some(token), now)
            .unwrap();
        // Stress the renderer's transient rows at the service's display
        // bounds, preserving the real pinned cursor and historical facts.
        let displayed = crate::work_service::unadmitted::displayed;
        for row in &mut page.rows {
            row.admission = "unadmitted";
            row.revision = Some(displayed(
                &"\"".repeat(crate::domain::MAX_OBSERVED_SOURCE_BYTES),
            ));
            row.unadmitted = Some(UnadmittedObservationDetail {
                occurrence: "a turn seen without admission, not a begun turn".into(),
                observed_from: row.recorded_at,
                observed_through: row.recorded_at,
                cause: "cause unknown".into(),
                accounting: "unadmitted",
                checks: (0..crate::domain::MAX_OBSERVED_CHECKS)
                    .map(|index| ObservedCheckRow {
                        host_check_id: displayed(&format!(
                            "{index:02}{}",
                            "\u{1}".repeat(crate::domain::MAX_OBSERVATION_LABEL_BYTES - 2)
                        )),
                        kind: "test",
                        result: "passed",
                        credit: "uncredited",
                        finished_at: Some(row.recorded_at),
                        source_revision: row.revision.clone(),
                        evidence_ref: Some(displayed(&format!(
                            "r{}",
                            "\u{1}".repeat(crate::domain::MAX_OBSERVED_EVIDENCE_REF_BYTES - 1)
                        ))),
                    })
                    .collect(),
            });
        }
        let budget = crate::verbs::MAX_AGENT_WORK_RESPONSE_BYTES;
        let fitted = fit_window(&page, budget).expect("one complete row and its disclosures fit");
        let text = fitted.text();
        assert!(text.contains("judged source: workspace"));
        assert!(text.contains("earlier differing observation"));
        assert!(text.contains("latest comparable endpoint"));
        assert!(text.contains("sealed evaluation detail:"));
        let rows = fitted.value["observations"].as_array().unwrap();
        assert_ne!(rows.as_slice(), &[] as &[Value]);
        assert!(rows.len() < page.rows.len());
        for row in rows {
            let checks = row["unadmitted"]["checks"].as_array().unwrap();
            assert_eq!(checks.len(), crate::domain::MAX_OBSERVED_CHECKS);
            assert!(checks.iter().all(|check| check["credit"] == "uncredited"));
        }
        assert_eq!(
            text.matches("observed check, uncredited").count(),
            rows.len() * crate::domain::MAX_OBSERVED_CHECKS
        );
        assert!(
            fitted.value["observations_window"]["after"]
                .as_str()
                .is_some()
        );
        assert!(super::super::receipts::agent_receipt_fits(&fitted, budget).unwrap());
    }
}
