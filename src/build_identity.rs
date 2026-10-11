//! Process-local diagnostics, never store admission or authenticated identity.
//! Storage must not call back into this module: identity initialization reads
//! the storage schema reference and must never re-enter its own once-only latch.

use std::{
    fs::File,
    io,
    sync::{Condvar, Mutex, OnceLock, PoisonError},
    time::Instant,
};

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

/// The process's identity, latched once.
static IDENTITY: Latch<BuildIdentity> = Latch::new();

/// Latches executable bytes and the in-memory schema reference once per process.
/// Long-lived MCP hosts call at startup before an install can replace their
/// executable; short-lived CLI words call only when emitting diagnostics.
/// Diagnostic unavailability must never refuse a word.
#[must_use]
pub fn current() -> &'static BuildIdentity {
    IDENTITY.get_or_init(identify)
}

/// [`current`], but waiting for its first latch only until `deadline`, which
/// `None` leaves unbounded. The first latch hashes the running executable, so
/// it runs on a thread of its own and goes on after a wait gives up; a later
/// call finds it latched. `None` means the identity was not read in time: the
/// deadline passed first, no thread could be started to read it, or the
/// reading this call started panicked. A bounded call never reads in its
/// own thread, which no deadline could bound.
#[must_use]
pub fn current_by(deadline: Option<Instant>) -> Option<&'static BuildIdentity> {
    #[cfg(test)]
    if let Some((latch, init)) = STAND_IN.with(std::cell::Cell::get) {
        return latch.get_by(deadline, init);
    }
    IDENTITY.get_by(deadline, identify)
}

fn identify() -> BuildIdentity {
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
}

#[cfg(test)]
type StandIn = Option<(&'static Latch<BuildIdentity>, fn() -> BuildIdentity)>;

#[cfg(test)]
thread_local! {
    /// A latch, with the reading that fills it, that stands in for the
    /// process's identity in [`current_by`] on this thread.
    static STAND_IN: std::cell::Cell<StandIn> = const { std::cell::Cell::new(None) };
}

/// Makes [`current_by`] on this thread wait on `latch`, filled by `init`,
/// instead of the process's identity, until the returned guard drops.
#[cfg(test)]
pub(crate) fn stand_in(
    latch: &'static Latch<BuildIdentity>,
    init: fn() -> BuildIdentity,
) -> impl Drop {
    struct Restore(StandIn);
    impl Drop for Restore {
        fn drop(&mut self) {
            STAND_IN.with(|slot| slot.set(self.0));
        }
    }
    Restore(STAND_IN.with(|slot| slot.replace(Some((latch, init)))))
}

/// A value latched once per process, which a caller may wait for with a
/// deadline. Its first bounded wait starts the reading on a thread of its own.
pub(crate) struct Latch<T: 'static> {
    value: OnceLock<T>,
    reading: Mutex<Reading>,
    /// Told when the value latches or the reading thread ends.
    changed: Condvar,
    /// How many times a bounded call has begun to wait, counted under the
    /// reading lock just before the wait releases it.
    #[cfg(test)]
    waits: std::sync::atomic::AtomicUsize,
}

/// Where the reading thread of a [`Latch`] is.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Reading {
    NotStarted,
    Running,
    /// It ended, with or without a value, or could not start.
    Ended,
}

/// Marks the reading thread ended when it returns or unwinds.
struct ReadingEnds<T: 'static>(&'static Latch<T>);

impl<T: 'static> Drop for ReadingEnds<T> {
    fn drop(&mut self) {
        *self.0.lock() = Reading::Ended;
        self.0.changed.notify_all();
    }
}

impl<T: 'static> Latch<T> {
    pub(crate) const fn new() -> Self {
        Self {
            value: OnceLock::new(),
            reading: Mutex::new(Reading::NotStarted),
            changed: Condvar::new(),
            #[cfg(test)]
            waits: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The reading state; no holder of this lock can panic, so a poisoned
    /// lock still holds a consistent state.
    fn lock(&self) -> std::sync::MutexGuard<'_, Reading> {
        self.reading.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<T: Send + Sync + 'static> Latch<T> {
    fn get_or_init(&'static self, init: fn() -> T) -> &'static T {
        if let Some(value) = self.value.get() {
            return value;
        }
        let value = self.value.get_or_init(init);
        // Wakes a bounded wait, whichever caller filled the latch. Taking the
        // lock orders this after a waiter's check that found it empty.
        drop(self.lock());
        self.changed.notify_all();
        value
    }

    pub(crate) fn get_by(
        &'static self,
        deadline: Option<Instant>,
        init: fn() -> T,
    ) -> Option<&'static T> {
        if let Some(value) = self.value.get() {
            return Some(value);
        }
        let Some(deadline) = deadline else {
            return Some(self.get_or_init(init));
        };
        let mut reading = self.lock();
        // This call starts at most one reading: the first, or a new one after
        // an earlier reading ended without a value.
        let mut started_here = false;
        loop {
            if let Some(value) = self.value.get() {
                return Some(value);
            }
            if *reading != Reading::Running {
                if started_here {
                    // The reading this call started ended without a value:
                    // it panicked. Never read here, which no deadline bounds.
                    return None;
                }
                started_here = true;
                *reading = Reading::Running;
                let spawned = std::thread::Builder::new()
                    .name("engram-latch".into())
                    .spawn(move || {
                        let _ends = ReadingEnds(self);
                        self.get_or_init(init);
                    });
                if spawned.is_err() {
                    *reading = Reading::Ended;
                    return None;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            #[cfg(test)]
            self.waits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            reading = self
                .changed
                .wait_timeout(reading, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
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

    /// Blocks until `open` is set through [`release`].
    fn wait_open(open: &Mutex<bool>, opened: &Condvar) {
        let mut guard = open.lock().unwrap();
        while !*guard {
            guard = opened.wait(guard).unwrap();
        }
    }

    fn release(open: &Mutex<bool>, opened: &Condvar) {
        *open.lock().unwrap() = true;
        opened.notify_all();
    }

    #[test]
    fn a_waiting_bounded_call_is_woken_by_an_unbounded_callers_fill() {
        use std::{
            sync::atomic::{AtomicBool, AtomicUsize, Ordering},
            time::Duration,
        };

        static OPEN: Mutex<bool> = Mutex::new(false);
        static OPENED: Condvar = Condvar::new();
        static STARTED: AtomicBool = AtomicBool::new(false);
        static READINGS: AtomicUsize = AtomicUsize::new(0);
        static LATCH: Latch<usize> = Latch::new();
        /// A reading that takes until the test lets it finish.
        fn read_once_opened() -> usize {
            STARTED.store(true, Ordering::SeqCst);
            wait_open(&OPEN, &OPENED);
            READINGS.fetch_add(1, Ordering::SeqCst) + 1
        }

        // An unbounded caller runs the reading in its own thread.
        let filler = std::thread::spawn(|| *LATCH.get_by(None, read_once_opened).unwrap());
        while !STARTED.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let waiter = std::thread::spawn(|| {
            let began = Instant::now();
            let value = LATCH.get_by(Some(began + Duration::from_secs(60)), read_once_opened);
            (value.copied(), began.elapsed())
        });
        // Release the reading only once the bounded call is waiting, so the
        // fill must wake it rather than find the value already latched.
        while LATCH.waits.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        release(&OPEN, &OPENED);
        let (value, took) = waiter.join().unwrap();
        assert_eq!(value, Some(1));
        // The fill woke it, not its deadline.
        assert!(took < Duration::from_secs(30), "{took:?}");
        assert_eq!(filler.join().unwrap(), 1);
        assert_eq!(READINGS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_bounded_call_never_reads_itself_after_its_reading_panicked() {
        use std::{
            sync::atomic::{AtomicUsize, Ordering},
            time::Duration,
        };

        static OPEN: Mutex<bool> = Mutex::new(false);
        static OPENED: Condvar = Condvar::new();
        static READINGS: AtomicUsize = AtomicUsize::new(0);
        static LATCH: Latch<usize> = Latch::new();
        /// The first reading panics; a later one takes until the test lets
        /// it finish.
        fn panic_first() -> usize {
            let reading = READINGS.fetch_add(1, Ordering::SeqCst) + 1;
            assert!(reading > 1, "the first reading fails");
            wait_open(&OPEN, &OPENED);
            reading
        }

        // The reading this call started panicked: it gives up at once rather
        // than read in its own thread, which no deadline bounds.
        let began = Instant::now();
        let value = LATCH.get_by(Some(began + Duration::from_secs(60)), panic_first);
        let took = began.elapsed();
        assert_eq!(value, None);
        assert!(took < Duration::from_secs(30), "{took:?}");
        assert_eq!(READINGS.load(Ordering::SeqCst), 1);

        // A later call starts a new reading and waits for it only until its
        // own deadline.
        let began = Instant::now();
        let value = LATCH.get_by(Some(began + Duration::from_millis(300)), panic_first);
        let took = began.elapsed();
        assert_eq!(value, None);
        assert!(took >= Duration::from_millis(300), "{took:?}");
        assert!(took < Duration::from_secs(30), "{took:?}");
        // A deadline already passed gives up at once and starts no other.
        assert_eq!(LATCH.get_by(Some(Instant::now()), panic_first), None);
        assert_eq!(READINGS.load(Ordering::SeqCst), 2);

        // Released, that reading latches for every caller.
        release(&OPEN, &OPENED);
        let value = LATCH.get_by(Some(Instant::now() + Duration::from_secs(60)), panic_first);
        assert_eq!(value, Some(&2));
        assert_eq!(READINGS.load(Ordering::SeqCst), 2);
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
