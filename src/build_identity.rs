//! Process-local diagnostics, never store admission or authenticated identity.
//! Storage must not call back into this module: identity initialization reads
//! the storage schema reference and must never re-enter its own once-only latch.

use std::{fs::File, io, sync::OnceLock};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{CanonicalObject, ObjectHash, StoreError};

/// Inputs visible to operators when comparing two running processes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BuildComponents {
    pub package_version: String,
    pub executable_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<&'static str>,
    pub schema_reference: Option<ObjectHash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<&'static str>,
}

/// A comparable diagnostic token with its independently inspectable inputs.
#[derive(Clone, Debug, Serialize)]
pub struct BuildIdentity {
    pub build: BuildComponents,
    pub build_fingerprint: Option<ObjectHash>,
}

/// Hashes precisely the visible component object, without persisting it.
///
/// # Errors
/// Returns an error if the diagnostic object cannot be canonicalized.
pub fn fingerprint(build: &BuildComponents) -> Result<ObjectHash, StoreError> {
    Ok(CanonicalObject::freeze(build)?.hash().clone())
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
        };
        BuildIdentity {
            build_fingerprint: fingerprint(&build).ok(),
            build,
        }
    })
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
            "{} build {} (exe {}, schema {})",
            identity.build.package_version,
            short_hash(identity.build_fingerprint.as_ref().map(ObjectHash::as_str)),
            short_hash(identity.build.executable_sha256.as_deref()),
            short_hash(
                identity
                    .build
                    .schema_reference
                    .as_ref()
                    .map(ObjectHash::as_str)
            ),
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
        assert_eq!(identity.build_fingerprint.as_ref(), Some(expected.hash()));
        assert_eq!(
            fingerprint(build).unwrap(),
            fingerprint(&build.clone()).unwrap()
        );
        for component in 0..3 {
            let mut changed = build.clone();
            match component {
                0 => changed.package_version.push_str("-different"),
                1 => {
                    changed.executable_sha256 =
                        Some(format!("{:x}", Sha256::digest(b"different executable")));
                }
                _ => {
                    changed.schema_reference = Some(
                        CanonicalObject::freeze(&"different schema")
                            .unwrap()
                            .hash()
                            .clone(),
                    );
                }
            }
            assert_ne!(fingerprint(&changed).unwrap(), *expected.hash());
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
        };
        let value = serde_json::to_value(&build).unwrap();
        assert!(value["executable_sha256"].is_null());
        assert_eq!(value["executable"], "unavailable");
        assert_eq!(
            fingerprint(&build).unwrap(),
            *CanonicalObject::freeze(&value).unwrap().hash()
        );
        assert_eq!(short_hash(None), "unavailable");
    }
}
