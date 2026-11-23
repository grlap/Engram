/// How a completion-time source identity enters a freshness check.
#[derive(Clone, Copy, Debug)]
pub(super) enum SourceCheck<'a> {
    /// A read: nothing has been measured for a completion, so a recorded
    /// basis is pending rather than stale.
    Unmeasured,
    /// A completion attempt presenting this identity, if any.
    AtCompletion(Option<&'a crate::source::SourceId>),
}

impl SourceCheck<'_> {
    pub(super) fn identity(self) -> Option<crate::domain::VerificationSourceIdentity> {
        match self {
            #[cfg(test)]
            Self::AtCompletion(None) if historical_completion::presented().is_some() => {
                historical_completion::presented().map(|source_revision| {
                    crate::domain::VerificationSourceIdentity::Legacy {
                        source_revision,
                        git_tree: None,
                    }
                })
            }
            Self::Unmeasured | Self::AtCompletion(None) => None,
            Self::AtCompletion(Some(source_id)) => {
                Some(crate::domain::VerificationSourceIdentity::SourceId {
                    source_id: source_id.clone(),
                })
            }
        }
    }
}

/// Lets a test seal a completion the way a build before the fingerprint's
/// retirement could, so that tests of stored historical seals have one to
/// read. Nothing outside tests can present a legacy revision at completion.
/// This affects the freshness comparison only. Exact historical request replay
/// is tested separately using the stored fingerprint field.
#[cfg(test)]
pub(crate) mod historical_completion {
    use std::cell::RefCell;

    thread_local! {
        static PRESENTED: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// While held, completions on this thread present `revision` as their
    /// legacy source declaration.
    pub(crate) struct Presenting(());

    impl Drop for Presenting {
        fn drop(&mut self) {
            PRESENTED.with(|presented| presented.borrow_mut().take());
        }
    }

    pub(crate) fn present(revision: &str) -> Presenting {
        PRESENTED.with(|presented| *presented.borrow_mut() = Some(revision.to_owned()));
        Presenting(())
    }

    pub(super) fn presented() -> Option<String> {
        PRESENTED.with(|presented| presented.borrow().clone())
    }
}
