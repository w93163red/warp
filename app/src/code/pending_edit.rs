//! Tracks files opened in the built-in editor on behalf of a blocked
//! `warp edit` process.
//!
//! When a tool such as `kubectl edit` spawns `warp edit` as its `$EDITOR`, that
//! process sits blocked on a marker file until we tell it the user is done.
//! "Done" here means the editor tab for the file has been closed, which is the
//! closest analogue to a terminal editor exiting — the calling tool then reads
//! the file back and applies whatever was saved.
//!
//! Completion is signalled by [`PendingEditSession`]'s [`Drop`] rather than
//! from the various close paths, because there are many ways for a tab to go
//! away — closing the tab, closing the pane, closing the window, closing the
//! tab group — and missing one of them would leave the caller hung forever.
//! Tying it to the lifetime of the value stored in the tab covers all of them
//! at once, including the tab never being opened in the first place.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use warpui::{Entity, SingletonEntity};

/// Exit code reported to the caller when the edit completed normally.
const EXIT_CODE_COMPLETED: i32 = 0;

/// A `warp edit` invocation waiting on a file to be closed in the editor.
///
/// Dropping this writes the done marker, which unblocks the caller. It is held
/// behind an [`Arc`] by the editor tab it belongs to, so a tab being cloned
/// (while being dragged between panes, say) does not complete the edit early.
pub struct PendingEditSession {
    path: PathBuf,
    done_path: PathBuf,
}

impl PendingEditSession {
    fn new(path: PathBuf, done_path: PathBuf) -> Self {
        Self { path, done_path }
    }
}

impl Drop for PendingEditSession {
    fn drop(&mut self) {
        log::info!(
            "Completing pending external edit of {}",
            self.path.display()
        );
        complete_edit(&self.done_path);
    }
}

/// Unblocks the `warp edit` process waiting on `done_path`.
///
/// Use this for requests that never became an editor tab; a tab that was opened
/// completes through [`PendingEditSession`] instead.
pub fn complete_edit(done_path: &Path) {
    // A failure here strands the caller, so it is worth a loud log. There is
    // nothing to fall back to: the marker path was chosen by the caller.
    if let Err(err) = std::fs::write(done_path, EXIT_CODE_COMPLETED.to_string()) {
        log::error!(
            "Failed to write edit completion marker {}: {err:#}",
            done_path.display()
        );
    }
}

/// Sessions that have been requested but not yet claimed by an editor tab.
///
/// Registration and pickup are deliberately decoupled: the request arrives on
/// the terminal's event path, while the tab that ends up owning it is built
/// several layers down inside the code editor.
#[derive(Default)]
pub struct PendingEditsModel {
    by_path: HashMap<PathBuf, Arc<PendingEditSession>>,
}

impl Entity for PendingEditsModel {
    type Event = ();
}

impl SingletonEntity for PendingEditsModel {}

impl PendingEditsModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a request to edit `path`, returning the session to keep alive
    /// until an editor tab claims it.
    ///
    /// The caller must hold the returned [`Arc`] across the call that opens the
    /// file and then drop it: if no tab claimed the session in the meantime,
    /// that drop completes the edit immediately rather than leaving the calling
    /// tool blocked on an editor that never appeared.
    pub fn register(&mut self, path: PathBuf, done_path: PathBuf) -> Arc<PendingEditSession> {
        let session = Arc::new(PendingEditSession::new(path.clone(), done_path));

        // A second request for a path that is already pending replaces the
        // first. Dropping the old session completes that earlier edit, which is
        // the right outcome: its editor tab is about to be reused.
        self.by_path.insert(path, Arc::clone(&session));

        session
    }

    /// Claims the session for `path`, if an edit of it is pending.
    pub fn claim(&mut self, path: &Path) -> Option<Arc<PendingEditSession>> {
        self.by_path.remove(path)
    }

    /// Drops the registry's reference to the session for `path` without
    /// completing it, once ownership has moved elsewhere.
    ///
    /// Called after the file has been opened so that a session nobody claimed
    /// does not sit in the map forever.
    pub fn forget(&mut self, path: &Path) {
        self.by_path.remove(path);
    }
}

#[cfg(test)]
#[path = "pending_edit_tests.rs"]
mod tests;
