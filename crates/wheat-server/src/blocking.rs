//! Store work off the async threads.
//!
//! The stores read and write the disk synchronously, even behind `async` methods, so
//! a session that called them on a Tokio worker would stall every other task on that
//! worker for as long as the disk takes. Sessions hand their store work here instead,
//! a batch at a time (a want, a batch of the walk, a content request), so a handoff
//! costs once per batch rather than once per read.

use std::future::Future;

use tokio::runtime::Handle;
use tokio::task::{JoinError, spawn_blocking};

/// Runs `work` on a thread for blocking work, driving the future it returns there.
pub(crate) async fn drive<F, Fut>(work: F) -> Result<Fut::Output, JoinError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future,
    Fut::Output: Send + 'static,
{
    let handle = Handle::current();
    spawn_blocking(move || handle.block_on(work())).await
}
