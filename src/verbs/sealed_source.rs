//! Shared CLI/MCP wording. A reported endpoint is neither the source at done
//! nor evidence that no intermediate write occurred.

use super::{Value, json};
use crate::storage::SealedSourceDiagnostic;
use crate::work_service::SealedSourceView;

#[derive(Clone, Debug)]
pub(super) struct SourceDiagnostic {
    pub(super) summary: String,
    pub(super) detail: Option<String>,
    pub(super) lines: Vec<String>,
    pub(super) value: Value,
}

impl SourceDiagnostic {
    /// Ordinary show and compact next expose status and opaque navigation.
    /// Raw identities and reported source text belong to the explicit window.
    pub(super) fn terse(view: &SealedSourceView, work_ref: &str) -> Self {
        let mut rendered = Self::new(view, work_ref);
        let Some(facts) = view
            .diagnostic
            .as_ref()
            .filter(|facts| facts.evaluation.is_some())
        else {
            if let Some(value) = rendered.value.as_object_mut() {
                value.remove("seal");
            }
            return rendered;
        };
        rendered.lines = vec![
            rendered.summary.clone(),
            format!(
                "  later observations: {} differing, {} agreeing, {} incomparable; {} total. Completion remains final; recording order does not establish a write after sealing or the source at done.",
                facts.differences, facts.agreements, facts.incomparable, facts.total,
            ),
        ];
        if let Some(command) = &rendered.detail {
            rendered
                .lines
                .push(format!("  retained observation details: {command}"));
        }
        rendered.value = json!({
            "status": rendered.value["status"],
            "evaluated_cut": facts.evaluated_cut, "completion_cut": facts.completion_cut,
            "horizon": facts.horizon, "total": facts.total,
            "differences": facts.differences, "agreements": facts.agreements,
            "incomparable": facts.incomparable,
            "observations_detail": rendered.detail,
            "limits": "observation interval only; no source-at-done, intermediate-write or exact done-turn claim; completion unchanged",
        });
        if view.details_omitted {
            rendered.omit_details();
        }
        rendered
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one bounded projection keeps shared text and JSON wording together"
    )]
    pub(super) fn new(view: &SealedSourceView, work_ref: &str) -> Self {
        let Some(diagnostic) = &view.diagnostic else {
            let line = format!(
                "sealed source diagnostic unavailable ({})",
                view.unavailable.unwrap_or("unavailable")
            );
            return Self {
                summary: line.clone(),
                detail: None,
                lines: vec![line.clone()],
                value: json!({"status": "unavailable", "seal": view.seal, "error_class": view.unavailable, "summary": line}),
            };
        };
        if diagnostic.evaluation.is_none() {
            let summary = "sealed source: no bound evaluation".to_owned();
            return Self {
                summary: summary.clone(),
                detail: None,
                lines: vec![summary],
                value: json!({"status": "no bound evaluation", "seal": diagnostic.seal}),
            };
        }
        let status = if diagnostic.judged.is_none() {
            SealedSourceDiagnostic::JUDGED_BASIS_UNAVAILABLE
        } else if diagnostic.differences > 0 {
            "later source sighting differs from sealed evaluated source"
        } else if diagnostic.agreements > 0 {
            "reported endpoint agreement"
        } else {
            "no comparable later closing sighting"
        };
        let navigation = view.continuation.as_ref().map(|token| {
            format!(
                "engram work show {} --observations --after {token}",
                super::record_windows::safe_reference_argument(work_ref),
            )
        });
        let mut lines = vec![format!(
            "sealed source: {status}; seal {}; evaluation {}; run {}; completion cut {}; sighting horizon {}",
            diagnostic.seal,
            diagnostic
                .evaluation
                .as_ref()
                .map_or("not recorded", |id| id.as_str()),
            diagnostic.run.0,
            diagnostic.completion_cut,
            diagnostic.horizon,
        )];
        let evaluation_detail = diagnostic.evaluation.as_ref().map(|id| {
            format!(
                "engram work show {} --evaluation {id}",
                super::record_windows::safe_reference_argument(work_ref)
            )
        });
        let basis = diagnostic.judged.as_ref().map(|basis| json!({
            "workspace": bounded(&basis.workspace_id), "revision": bounded(&basis.source_revision),
            "root_generation": basis.source_root_generation, "root_state": basis.source_root_state,
            "workspace_bytes": basis.workspace_id.len(), "revision_bytes": basis.source_revision.len(),
            "truncated": basis.workspace_id.len() > 256 || basis.source_revision.len() > 256,
        }));
        if let Some(basis) = &diagnostic.judged {
            lines.push(format!(
                "  judged source: workspace {}; generation {}; revision {}{}",
                super::terminal_safe_line(&bounded(&basis.workspace_id)),
                basis.source_root_generation.map_or_else(
                    || "not recorded".into(),
                    |generation| generation.to_string(),
                ),
                super::terminal_safe_line(&bounded(&basis.source_revision)),
                if basis.workspace_id.len() > 256 || basis.source_revision.len() > 256 {
                    format!(
                        " (previews truncated; workspace {} bytes, revision {} bytes)",
                        basis.workspace_id.len(),
                        basis.source_revision.len(),
                    )
                } else {
                    String::new()
                },
            ));
        } else if let Some(reason) = diagnostic
            .unavailable
            .as_deref()
            .filter(|reason| *reason != status)
        {
            lines.push(format!(
                "  judged basis unavailable: {}",
                super::terminal_safe_line(reason)
            ));
        }
        if let Some(fact) = &diagnostic.first_difference {
            lines.push(format!(
                "  earlier differing observation {} at run position {} (recorded after seal cut)",
                fact.observation, fact.position
            ));
            if let Some(basis) = &fact.source_basis {
                lines.push(format!(
                    "  reported closing revision {}{}",
                    super::terminal_safe_line(&bounded(&basis.source_revision)),
                    if basis.source_revision.len() > 256 {
                        " (truncated; see that observation's row)"
                    } else {
                        ""
                    }
                ));
            }
        }
        if let Some(fact) = &diagnostic.latest_endpoint {
            lines.push(format!(
                "  latest comparable endpoint {} at run position {}: {}",
                fact.observation, fact.position, fact.comparison
            ));
        }
        lines.push(format!("  later observations: {} differing, {} agreeing, {} incomparable; {} total. Completion remains final; recording order does not establish a write after sealing or the source at done.", diagnostic.differences, diagnostic.agreements, diagnostic.incomparable, diagnostic.total));
        if let Some(command) = &navigation {
            lines.push(format!("  retained observation details: {command}"));
        }
        if let Some(command) = &evaluation_detail {
            lines.push(format!("  sealed evaluation detail: {command}"));
        }
        let mut rendered = Self {
            summary: format!("sealed source: {status}"),
            detail: navigation.clone(),
            lines,
            value: json!({
                "status": status, "seal": diagnostic.seal, "evaluation": diagnostic.evaluation,
                "run": diagnostic.run, "root_execution": diagnostic.root_execution,
                "original_claim": diagnostic.claim, "original_claim_fence": diagnostic.claim_fence,
                "named_root_binding": diagnostic.named_root_binding,
                "evaluated_cut": diagnostic.evaluated_cut, "completion_cut": diagnostic.completion_cut,
                "horizon": diagnostic.horizon, "judged_source": basis,
                "unavailable": diagnostic.unavailable, "first_difference": diagnostic.first_difference.as_ref().map(fact_value),
                "latest_endpoint": diagnostic.latest_endpoint.as_ref().map(fact_value),
                "total": diagnostic.total, "differences": diagnostic.differences,
                "agreements": diagnostic.agreements, "incomparable": diagnostic.incomparable,
                "observations_detail": navigation,
                "evaluation_detail": evaluation_detail,
                "limits": "observation interval only; no source-at-done, intermediate-write or exact done-turn claim; completion unchanged",
            }),
        };
        if view.details_omitted {
            rendered.omit_details();
        }
        rendered
    }

    /// Shed recoverable detail before the item's outcome and acceptance.
    pub(super) fn omit_details(&mut self) -> bool {
        if (self.value["evaluation"].is_null() && self.value["total"].is_null())
            || self.value["details_omitted"] == true
        {
            return false;
        }
        self.lines = vec![self.summary.clone()];
        if let Some(command) = &self.detail {
            self.lines.push(format!(
                "  details omitted; completion remains final; {command}"
            ));
        }
        self.value = json!({
            "status": self.value["status"],
            "details_omitted": true, "observations_detail": self.detail,
            "limits": "observation interval only; completion unchanged",
        });
        true
    }
}

fn fact_value(fact: &crate::storage::SealedSourceFact) -> Value {
    json!({ "observation": fact.observation, "run_position": fact.position,
        "comparison": fact.comparison,
        "source_basis": fact.source_basis.as_ref().map(|basis| json!({
            "workspace": bounded(&basis.workspace_id), "revision": bounded(&basis.source_revision),
            "root_generation": basis.source_root_generation, "root_state": basis.source_root_state,
            "truncated": basis.workspace_id.len() > 256 || basis.source_revision.len() > 256,
        })),
    })
}

fn bounded(text: &str) -> String {
    if text.len() <= 256 {
        return text.to_owned();
    }
    let mut end = 256;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
