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
        if let Some(delta) = &self.git_delta {
            ensure!(
                self.changes.is_empty()
                    && self.submodules.is_empty()
                    && delta.sha256.len() == 64
                    && delta
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    && (delta.dirty || delta.gitlinks.is_empty()),
                "invalid source Git delta"
            );
            for link in &delta.gitlinks {
                ensure!(
                    path(&link.path)
                        && (link.head_pin.is_some() || link.index_pin.is_some())
                        && (link.index_pin.is_none() || link.checkout_head.is_some())
                        && [&link.head_pin, &link.index_pin, &link.checkout_head]
                            .into_iter()
                            .flatten()
                            .all(|commit| oid(commit)),
                    "invalid source gitlink"
                );
            }
            ensure!(
                delta
                    .gitlinks
                    .windows(2)
                    .all(|pair| pair[0].path < pair[1].path),
                "source gitlinks must be unique and UTF-8 byte ordered"
            );
            return Ok(());
        }
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
    use crate::source::{GitDelta, Gitlink, Mode};

    #[test]
    fn git_delta_requires_an_ordered_link_set_and_consistent_pins() {
        let commit = "1".repeat(40);
        let link = Gitlink {
            path: "child".to_owned(),
            head_pin: Some(commit.clone()),
            index_pin: Some(commit.clone()),
            checkout_head: Some("2".repeat(40)),
        };
        let delta = GitDelta {
            sha256: "a".repeat(64),
            dirty: true,
            gitlinks: vec![link.clone()],
        };
        let mut source = SourceId {
            commit,
            tree: "3".repeat(40),
            git_delta: Some(delta.clone()),
            submodules: Vec::new(),
            changes: Vec::new(),
        };
        assert!(source.validate().is_ok());
        let mut hybrid = serde_json::to_value(&source).unwrap();
        hybrid["submodules"] = serde_json::json!([]);
        hybrid["changes"] = serde_json::json!([]);
        assert!(serde_json::from_value::<SourceId>(hybrid).is_err());

        source.git_delta.as_mut().unwrap().dirty = false;
        assert!(source.validate().is_err());
        for gitlinks in [
            vec![link.clone(), link.clone()],
            vec![
                Gitlink {
                    path: "z".to_owned(),
                    ..link.clone()
                },
                link.clone(),
            ],
            vec![Gitlink {
                checkout_head: None,
                ..link.clone()
            }],
            vec![Gitlink {
                head_pin: None,
                index_pin: None,
                ..link
            }],
        ] {
            source.git_delta = Some(GitDelta {
                gitlinks,
                ..delta.clone()
            });
            assert!(source.validate().is_err());
        }
    }

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
            git_delta: None,
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
