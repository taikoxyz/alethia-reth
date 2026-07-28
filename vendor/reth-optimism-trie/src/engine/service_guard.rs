//! Generic guard that joins a service thread on drop.

use std::{fmt, thread::JoinHandle};

/// Joins the wrapped thread when dropped. `None` is allowed for test/mock construction.
pub(super) struct ServiceGuard(Option<JoinHandle<()>>);

impl ServiceGuard {
    /// Wraps a running service thread so it is joined when the final owner drops it.
    pub(super) const fn new(handle: JoinHandle<()>) -> Self {
        Self(Some(handle))
    }
}

impl fmt::Debug for ServiceGuard {
    /// Formats whether a service thread is present without exposing platform-specific join-handle
    /// details.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ServiceGuard").field(&self.0.as_ref().map(|_| "...")).finish()
    }
}

impl Drop for ServiceGuard {
    /// Takes and joins the service thread, blocking until it exits and ignoring a thread panic
    /// during cleanup.
    fn drop(&mut self) {
        if let Some(join_handle) = self.0.take() {
            let _ = join_handle.join();
        }
    }
}
