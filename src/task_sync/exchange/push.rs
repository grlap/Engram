//! Interpret one completed single-ref Git porcelain refusal. It says nothing
//! about an earlier attempt, and never establishes a publication receipt.

use super::RunError;

pub(super) fn classify_failure(
    error: RunError,
    exit_code: Option<i32>,
    stdout: &[u8],
    commit: &str,
    reference: &str,
    leased: bool,
) -> RunError {
    if error != RunError::Git || exit_code != Some(1) {
        return error;
    }
    match selected_status(stdout, commit, reference) {
        Some("[rejected] (non-fast-forward)" | "[rejected] (fetch first)") => {
            RunError::NonFastForward
        }
        Some("[remote rejected] (failed to update ref)") => RunError::RefUpdateRejected,
        Some("[rejected] (stale info)") if leased => RunError::RefUpdateRejected,
        _ => error,
    }
}

fn selected_status<'a>(stdout: &'a [u8], commit: &str, reference: &str) -> Option<&'a str> {
    if !stdout.ends_with(b"\n") {
        return None;
    }
    let stdout = std::str::from_utf8(stdout).ok()?;
    let mut records = stdout.lines().filter(|line| line.contains('\t'));
    let record = records.next()?;
    if records.next().is_some() {
        return None;
    }
    let mut fields = record.split('\t');
    let status = fields.next()?;
    let refspec = fields.next()?;
    let reason = fields.next()?;
    (status == "!" && refspec == format!("{commit}:{reference}") && fields.next().is_none())
        .then_some(reason)
}

#[cfg(test)]
mod tests;
