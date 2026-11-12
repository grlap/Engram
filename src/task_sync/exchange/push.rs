//! Interpret one completed single-ref Git porcelain refusal. It says nothing
//! about an earlier attempt, and never establishes a publication receipt.

use super::RunError;

pub(super) fn classify_failure(
    error: RunError,
    exit_code: Option<i32>,
    stdout: &[u8],
    commit: &str,
    reference: &str,
) -> RunError {
    if error == RunError::Git && exit_code == Some(1) && non_fast_forward(stdout, commit, reference)
    {
        RunError::NonFastForward
    } else {
        error
    }
}

fn non_fast_forward(stdout: &[u8], commit: &str, reference: &str) -> bool {
    if !stdout.ends_with(b"\n") {
        return false;
    }
    let Ok(stdout) = std::str::from_utf8(stdout) else {
        return false;
    };
    let mut records = stdout.lines().filter(|line| line.contains('\t'));
    let Some(record) = records.next() else {
        return false;
    };
    if records.next().is_some() {
        return false;
    }
    let fields = record.split('\t').collect::<Vec<_>>();
    fields.len() == 3
        && fields[0] == "!"
        && fields[1] == format!("{commit}:{reference}")
        && matches!(
            fields[2],
            "[rejected] (non-fast-forward)" | "[rejected] (fetch first)"
        )
}

#[cfg(test)]
mod tests;
