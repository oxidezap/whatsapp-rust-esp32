//! Shared admission for a demo response and a streaming HTTP connection.

use async_lock::{Mutex, MutexGuardArc};
use std::sync::Arc;

/// An opt-in, non-reentrant gate for operations sharing scarce transient RAM.
/// Clone the same gate across client restarts so cancelled I/O still owns its
/// reservation until the blocking operation really releases its connection.
#[derive(Clone, Default)]
pub struct MemoryGate(Arc<Mutex<()>>);

pub struct MemoryPermit {
    _guard: MutexGuardArc<()>,
}

impl MemoryGate {
    pub async fn acquire(&self) -> MemoryPermit {
        MemoryPermit {
            _guard: self.0.lock_arc().await,
        }
    }
}
