//! Named points at which a test stops a push running in a process of its
//! own. The test then kills that process, which leaves on disk exactly what a
//! kill at that point leaves, and checks what the next push makes of it.

use std::{path::PathBuf, sync::OnceLock, time::Duration};

/// A point in a push at which a test may stop it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Stage {
    /// The target answered for a pending attempt; what it answered is not
    /// recorded yet.
    Resolving,
    /// After any pending attempt was resolved, before the capture.
    BeforeCapture,
    /// The live store was copied into the stage; its check has not begun.
    CopyStaged,
    /// The compressed file was prepared in the stage; no attempt is
    /// recorded yet.
    Prepared,
    /// The attempt was recorded as pending; nothing was sent yet.
    PendingRecorded,
    /// The copy's bytes were written to a temporary file at the target,
    /// which was not renamed yet.
    TemporaryWritten,
    /// The copy's data file is in place at the target; its manifest is not.
    DataPublished,
    /// The put returned its receipt, which is not recorded yet.
    PutReturned,
    /// A copy beyond the retention count was removed at the target; the
    /// ledger does not record that yet.
    CopyRemoved,
}

impl Stage {
    pub(crate) const ALL: [Self; 9] = [
        Self::Resolving,
        Self::BeforeCapture,
        Self::CopyStaged,
        Self::Prepared,
        Self::PendingRecorded,
        Self::TemporaryWritten,
        Self::DataPublished,
        Self::PutReturned,
        Self::CopyRemoved,
    ];

    pub(crate) fn name(self) -> String {
        format!("{self:?}")
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|stage| stage.name() == name)
    }
}

static ARMED: OnceLock<(Stage, PathBuf)> = OnceLock::new();

/// Makes this process stop at `stage`: once it gets there it writes `ready`
/// and waits until it is killed. Arming twice is refused.
pub(crate) fn arm(stage: Stage, ready: PathBuf) {
    assert!(
        ARMED.set((stage, ready)).is_ok(),
        "a push is armed to stop once"
    );
}

/// Stops here when this process was armed for `stage`, until it is killed.
/// A process whose test was itself killed and never kills it ends after two
/// minutes, at once and without any cleanup, as a kill would end it.
pub(crate) fn at(stage: Stage) {
    if let Some((armed, ready)) = ARMED.get()
        && *armed == stage
    {
        std::fs::write(ready, b"stopped").expect("write the ready file");
        std::thread::sleep(Duration::from_secs(120));
        std::process::exit(3);
    }
}
