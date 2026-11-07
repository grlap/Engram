//! Representation validation for new ingress, without measuring source state.

use super::{Change, SourceId};
use anyhow::{Result, ensure};

impl SourceId {
    /// Validate the closed identity's values and ordering. This establishes no
    /// provenance and performs no Git or filesystem access.
    ///
    /// # Errors
    /// Refuses malformed object IDs, paths, change records or array ordering.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            oid(&self.commit) && oid(&self.tree),
            "invalid source commit or tree"
        );
        changes(&self.changes)?;
        for child in &self.submodules {
            ensure!(
                path(&child.path) && oid(&child.commit),
                "invalid source submodule"
            );
            changes(&child.changes)?;
        }
        ensure!(
            self.submodules
                .windows(2)
                .all(|pair| pair[0].path < pair[1].path),
            "source submodules must be unique and UTF-8 byte ordered"
        );
        Ok(())
    }
}

fn oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn path(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['\0', '\r', '\n'])
        && value
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
        && !std::path::Path::new(value).is_absolute()
        && std::path::Path::new(value)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
}

fn changes(values: &[Change]) -> Result<()> {
    for value in values {
        ensure!(
            path(&value.path)
                && value.status.len() == 2
                && value
                    .status
                    .bytes()
                    .all(|byte| b" MADRCUT?".contains(&byte))
                && value.status != "  ",
            "invalid source change path or status"
        );
        let renamed = value.status.contains(['R', 'C']);
        ensure!(
            if renamed {
                value.origin.as_deref().is_some_and(path)
            } else {
                value.origin.is_none()
            },
            "invalid source change origin"
        );
        ensure!(
            match (&value.blob, value.mode) {
                (Some(blob), Some(_)) => oid(blob),
                (None, None) => value.status.contains('D'),
                _ => false,
            },
            "invalid source change blob or mode"
        );
    }
    ensure!(
        values
            .windows(2)
            .all(|pair| change_key(&pair[0]) < change_key(&pair[1])),
        "source changes must be unique and UTF-8 byte ordered"
    );
    Ok(())
}

fn change_key(value: &Change) -> (&str, &str, Option<&str>) {
    (&value.path, &value.status, value.origin.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Mode;

    #[test]
    fn new_identity_validation_checks_values_and_preserves_distinct_same_path_records() {
        let id = crate::ObjectId::from_canonical_bytes(b"source shape reference").to_string();
        let oid = id[..40].to_owned();
        let change = Change {
            path: "file".to_owned(),
            status: "??".to_owned(),
            origin: None,
            blob: Some(oid.clone()),
            mode: Some(Mode::Regular),
        };
        let mut value = SourceId {
            commit: oid.clone(),
            tree: oid,
            submodules: Vec::new(),
            changes: vec![change.clone()],
        };
        assert!(value.validate().is_ok());
        for status in ["! ", " !", "!!"] {
            value.changes[0].status = status.to_owned();
            assert!(value.validate().is_err());
        }
        value.changes[0].status = "??".to_owned();
        value.changes.push(Change {
            status: "D ".to_owned(),
            ..change
        });
        assert!(value.validate().is_ok());
        value.changes.reverse();
        assert!(value.validate().is_err());
        value.changes.reverse();
        for invalid in ["../file", "/file", "file\nname"] {
            value.changes[0].path = invalid.to_owned();
            assert!(value.validate().is_err());
        }
        value.changes.clear();
        value.commit.make_ascii_uppercase();
        assert!(value.validate().is_err());
    }
}
