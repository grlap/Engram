//! Process-local diagnostics, never store admission or authenticated identity.
//! Storage must not call back into this module: identity initialization reads
//! the storage schema reference and must never re-enter its own once-only latch.

use std::{fs::File, io, sync::OnceLock};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{CanonicalObject, ObjectId, StoreError};

/// Inputs visible to operators when comparing two running processes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BuildComponents {
    pub package_version: String,
    pub executable_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<&'static str>,
    pub schema_reference: Option<ObjectId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<&'static str>,
    /// The commit the executable was built from, `+dirty` when tracked files
    /// differed from it, or `unavailable`. Recovery information, not proof
    /// that the revision rebuilds to the same executable.
    pub source_revision: String,
}

/// A comparable diagnostic token with its independently inspectable inputs.
#[derive(Clone, Debug, Serialize)]
pub struct BuildIdentity {
    pub build: BuildComponents,
    pub build_fingerprint: Option<ObjectId>,
}

/// Hashes precisely the visible component object, without persisting it.
///
/// # Errors
/// Returns an error if the diagnostic object cannot be canonicalized.
pub fn fingerprint(build: &BuildComponents) -> Result<ObjectId, StoreError> {
    Ok(CanonicalObject::freeze(build)?.key().clone())
}

/// Latches executable bytes and the in-memory schema reference once per process.
/// Long-lived MCP hosts call at startup before an install can replace their
/// executable; short-lived CLI words call only when emitting diagnostics.
/// Diagnostic unavailability must never refuse a word.
#[must_use]
pub fn current() -> &'static BuildIdentity {
    static IDENTITY: OnceLock<BuildIdentity> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        let executable_sha256 = executable_digest().ok();
        let schema_reference = crate::storage::running_schema_reference().ok();
        let build = BuildComponents {
            package_version: env!("CARGO_PKG_VERSION").into(),
            executable: executable_sha256.is_none().then_some("unavailable"),
            executable_sha256,
            schema: schema_reference.is_none().then_some("unavailable"),
            schema_reference,
            source_revision: source_revision().into(),
        };
        BuildIdentity {
            build_fingerprint: fingerprint(&build).ok(),
            build,
        }
    })
}

/// The source revision recorded when this executable was built: a commit id,
/// that id followed by `+dirty`, or `unavailable`.
#[must_use]
pub const fn source_revision() -> &'static str {
    env!("ENGRAM_SOURCE_REVISION")
}

/// A source revision shortened for display, keeping its `+dirty` marker.
#[must_use]
pub fn short_revision(revision: &str) -> String {
    if revision == "unavailable" {
        return revision.to_owned();
    }
    match revision.split_once('+') {
        Some((commit, marker)) => format!("{}+{marker}", short_hash(Some(commit))),
        None => short_hash(Some(revision)).to_owned(),
    }
}

fn executable_digest() -> io::Result<String> {
    let mut executable = File::open(std::env::current_exe()?)?;
    let mut digest = Sha256::new();
    io::copy(&mut executable, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

/// Package version plus compact process and schema diagnostics for clap.
#[must_use]
pub fn version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let identity = current();
        format!(
            "{} build {} (exe {}, schema {}, rev {})",
            identity.build.package_version,
            short_hash(identity.build_fingerprint.as_ref().map(ObjectId::as_str)),
            short_hash(identity.build.executable_sha256.as_deref()),
            short_hash(
                identity
                    .build
                    .schema_reference
                    .as_ref()
                    .map(ObjectId::as_str)
            ),
            short_revision(&identity.build.source_revision),
        )
    })
}

pub(crate) fn short_hash(hash: Option<&str>) -> &str {
    hash.and_then(|hash| hash.get(..12))
        .unwrap_or("unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_binds_the_visible_runtime_derived_components() {
        let identity = current();
        let build = &identity.build;
        assert_eq!(build.executable_sha256, Some(executable_digest().unwrap()));
        assert_eq!(
            build.schema_reference,
            Some(crate::storage::running_schema_reference().unwrap())
        );
        let expected = CanonicalObject::freeze(build).unwrap();
        assert_eq!(identity.build_fingerprint.as_ref(), Some(expected.key()));
        assert_eq!(
            fingerprint(build).unwrap(),
            fingerprint(&build.clone()).unwrap()
        );
        for component in 0..4 {
            let mut changed = build.clone();
            match component {
                0 => changed.package_version.push_str("-different"),
                1 => {
                    changed.executable_sha256 =
                        Some(format!("{:x}", Sha256::digest(b"different executable")));
                }
                2 => changed.source_revision.push_str("+different"),
                _ => {
                    changed.schema_reference = Some(
                        CanonicalObject::freeze(&"different schema")
                            .unwrap()
                            .key()
                            .clone(),
                    );
                }
            }
            assert_ne!(fingerprint(&changed).unwrap(), *expected.key());
        }
        assert!(std::ptr::eq(current(), current()));
    }

    #[test]
    fn unavailable_components_are_explicit_and_still_comparable() {
        let build = BuildComponents {
            package_version: env!("CARGO_PKG_VERSION").into(),
            executable_sha256: None,
            executable: Some("unavailable"),
            schema_reference: None,
            schema: Some("unavailable"),
            source_revision: "unavailable".into(),
        };
        let value = serde_json::to_value(&build).unwrap();
        assert!(value["executable_sha256"].is_null());
        assert_eq!(value["executable"], "unavailable");
        assert_eq!(
            fingerprint(&build).unwrap(),
            *CanonicalObject::freeze(&value).unwrap().key()
        );
        assert_eq!(short_hash(None), "unavailable");
    }
}
