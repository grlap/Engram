//! One synchronous note word's window, shared across its store lock guards.

use std::cell::RefCell;

use crate::storage::WriterAdmissionWindow;

use super::LocalWorkService;

thread_local! {
    static NOTE_WORD: RefCell<Option<NoteWord>> = const { RefCell::new(None) };
}

#[derive(Clone)]
struct NoteWord {
    owner: usize,
    window: WriterAdmissionWindow,
}

struct Restore(Option<NoteWord>);

impl Drop for Restore {
    fn drop(&mut self) {
        NOTE_WORD.with(|scope| *scope.borrow_mut() = self.0.take());
    }
}

impl LocalWorkService {
    /// The closure runs synchronously; it must not await or move threads.
    /// Nested note scopes share the outer window, which starts only at BEGIN.
    pub(crate) fn note_writer_admission_word<T>(&self, word: impl FnOnce() -> T) -> T {
        let owner = std::ptr::from_ref(self) as usize;
        let previous = NOTE_WORD.with(|scope| {
            let mut scope = scope.borrow_mut();
            let window = scope
                .as_ref()
                .map_or_else(WriterAdmissionWindow::note, |outer| outer.window.clone());
            scope.replace(NoteWord { owner, window })
        });
        let _restore = Restore(previous);
        word()
    }

    pub(super) fn note_word_window(&self) -> Option<WriterAdmissionWindow> {
        let owner = std::ptr::from_ref(self) as usize;
        NOTE_WORD.with(|scope| {
            scope
                .borrow()
                .as_ref()
                .filter(|scope| scope.owner == owner)
                .map(|scope| scope.window.clone())
        })
    }
}
