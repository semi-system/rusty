//! `MemoryBudget`: one global budget for the memory in-flight messages may hold, shared by every
//! service of the process (SMTP DATA, IMAP APPEND). Moved here from rusty-smtp unchanged so the
//! services draw on the same pool.

use std::sync::Arc;

/// A global budget in 1 KiB units. Reserving never waits: when the budget is used up the caller
/// refuses new data with a temporary status, so a flood of big transfers degrades into
/// `421`/`451` (SMTP) or `NO [UNAVAILABLE]` (IMAP) instead of exhausting the host.
#[derive(Debug, Clone)]
pub struct MemoryBudget {
    sem: Arc<tokio::sync::Semaphore>,
}

impl MemoryBudget {
    pub fn new_mib(mib: u32) -> MemoryBudget {
        MemoryBudget { sem: Arc::new(tokio::sync::Semaphore::new((mib as usize).saturating_mul(1024))) }
    }

    /// Reserve room for `bytes` (rounded up to KiB). `None`: the budget cannot cover it now.
    pub fn try_reserve(&self, bytes: u64) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let kib = u32::try_from(bytes.div_ceil(1024)).ok()?;
        self.sem.clone().try_acquire_many_owned(kib.max(1)).ok()
    }

    /// KiB still free (diagnostics and tests).
    pub fn available_kib(&self) -> usize {
        self.sem.available_permits()
    }
}
