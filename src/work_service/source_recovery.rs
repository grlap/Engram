//! One source remedy for service recovery, words and show.

use crate::domain::{AcceptanceSourceRecoveryCause, AcceptanceSourceRemedy};

/// The one source remedy that names a word's argument.
pub(crate) const MEASURE_SOURCE_REMEDY: crate::argument_names::Twin = crate::argument_names::Twin {
    cli: "obtain a fresh source measurement from the host and retry done with it (--source-fingerprint F); copying the evaluated fingerprint is not a measurement",
    mcp: "obtain a fresh source measurement from the host and retry done with it (source_fingerprint); copying the evaluated fingerprint is not a measurement",
};

pub(crate) fn source_recovery_remedy(cause: &AcceptanceSourceRecoveryCause) -> &'static str {
    match cause.remedy {
        AcceptanceSourceRemedy::EndTurnReadAndRetry => {
            "this snapshot does not confirm the declared revision; end the turn, read the same run again, then retry done; matching host confirmation can preserve this judgment, but no future report is promised"
        }
        AcceptanceSourceRemedy::ReadSourceAndEvaluate => {
            "read the named root's current source and request a new acceptance evaluation of it, then retry done"
        }
        AcceptanceSourceRemedy::MeasureSourceAndRetry => MEASURE_SOURCE_REMEDY.cli,
        AcceptanceSourceRemedy::EvaluateCurrentSource => {
            "request a new acceptance evaluation of the current source with its host-measured source basis, then retry done; copying an earlier fingerprint is insufficient"
        }
    }
}

/// Bound host-recorded strings in agent receipts; raw storage details stay whole.
pub(crate) fn shown_source_recovery(
    cause: &AcceptanceSourceRecoveryCause,
) -> AcceptanceSourceRecoveryCause {
    let mut shown = cause.clone();
    for field in [
        &mut shown.workspace_id,
        &mut shown.declared_revision,
        &mut shown.reported_revision,
        &mut shown.expected_fingerprint,
        &mut shown.presented_fingerprint,
    ]
    .into_iter()
    .flatten()
    {
        *field = super::deciding::bounded(field);
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::shown_source_recovery;
    use crate::domain::{
        AcceptanceSourceMismatch, AcceptanceSourceRecoveryCause, AcceptanceSourceRemedy,
    };
    use crate::{ObjectId, WorkRunId};

    type Field = fn(&mut AcceptanceSourceRecoveryCause) -> &mut Option<String>;

    /// Each of the five host-recorded strings is bounded on its own at 128
    /// UTF-8 bytes, never inside a character, with the stored byte length
    /// after the cut; the cause it was projected from keeps the whole value.
    #[test]
    fn every_source_string_is_bounded_at_128_bytes_with_its_stored_length() {
        let fields: [(&str, Field); 5] = [
            ("workspace_id", |cause| &mut cause.workspace_id),
            ("declared_revision", |cause| &mut cause.declared_revision),
            ("reported_revision", |cause| &mut cause.reported_revision),
            ("expected_fingerprint", |cause| {
                &mut cause.expected_fingerprint
            }),
            ("presented_fingerprint", |cause| {
                &mut cause.presented_fingerprint
            }),
        ];
        // The 43rd three-byte character spans bytes 126 to 129, so the cut
        // falls back to byte 126; the four-byte one at 125 to 129 likewise.
        let cases = [
            ("a".repeat(127), "a".repeat(127)),
            ("a".repeat(128), "a".repeat(128)),
            (
                "a".repeat(129),
                format!("{}… (129 bytes stored)", "a".repeat(128)),
            ),
            (
                "源".repeat(43),
                format!("{}… (129 bytes stored)", "源".repeat(42)),
            ),
            (
                format!("{}😀", "a".repeat(125)),
                format!("{}… (129 bytes stored)", "a".repeat(125)),
            ),
            (
                format!("{}源", "a".repeat(125)),
                format!("{}源", "a".repeat(125)),
            ),
        ];
        for (name, field) in fields {
            for (stored, expected) in &cases {
                let mut raw = AcceptanceSourceRecoveryCause {
                    mismatch: AcceptanceSourceMismatch::CompletionFingerprintMismatch,
                    evaluation: ObjectId::from_canonical_bytes(b"evaluation"),
                    run_id: WorkRunId::new(),
                    evaluated_cut: 7,
                    remedy: AcceptanceSourceRemedy::EvaluateCurrentSource,
                    root_binding: None,
                    workspace_id: Some("w".into()),
                    declared_revision: Some("d".into()),
                    reported_revision: Some("r".into()),
                    expected_fingerprint: Some("e".into()),
                    presented_fingerprint: Some("p".into()),
                };
                *field(&mut raw) = Some(stored.clone());
                let mut shown = shown_source_recovery(&raw);
                assert_eq!(
                    field(&mut shown).as_deref(),
                    Some(expected.as_str()),
                    "{name}"
                );
                assert_eq!(field(&mut raw).as_deref(), Some(stored.as_str()), "{name}");
                // The other four, and every other field, are unchanged.
                *field(&mut shown) = Some(stored.clone());
                assert_eq!(shown, raw, "{name}");
            }
            // An absent string stays absent.
            let mut raw = AcceptanceSourceRecoveryCause {
                mismatch: AcceptanceSourceMismatch::EvaluationSourceBasisMissing,
                evaluation: ObjectId::from_canonical_bytes(b"evaluation"),
                run_id: WorkRunId::new(),
                evaluated_cut: 7,
                remedy: AcceptanceSourceRemedy::EvaluateCurrentSource,
                root_binding: None,
                workspace_id: None,
                declared_revision: None,
                reported_revision: None,
                expected_fingerprint: None,
                presented_fingerprint: None,
            };
            assert_eq!(shown_source_recovery(&raw), raw);
            assert!(field(&mut raw).is_none());
        }
    }
}
