//! The jobs this process is running, with their cancellation tokens.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::executor::{CancelCell, CancelReason};
use crate::store::finish::Fence;

/// One running attempt: what the heartbeat needs to cancel it and to flush its progress.
pub struct RunningEntry {
    pub attempt: i32,
    pub token: CancellationToken,
    pub cancel: Arc<CancelCell>,
    pub progress: Arc<Mutex<Option<Value>>>,
}

/// Shared set of running jobs, keyed by job id.
#[derive(Clone, Default)]
pub struct Running(Arc<Mutex<HashMap<i64, RunningEntry>>>);

impl Running {
    fn map(&self) -> MutexGuard<'_, HashMap<i64, RunningEntry>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn insert(&self, id: i64, entry: RunningEntry) {
        self.map().insert(id, entry);
    }

    pub fn remove(&self, id: i64) {
        self.map().remove(&id);
    }

    pub fn ids(&self) -> Vec<i64> {
        self.map().keys().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.map().len()
    }

    pub fn is_empty(&self) -> bool {
        self.map().is_empty()
    }

    pub fn cancel(&self, id: i64, reason: CancelReason) {
        if let Some(e) = self.map().get(&id) {
            e.cancel.set(reason);
            e.token.cancel();
        }
    }

    pub fn cancel_all(&self, reason: CancelReason) {
        for e in self.map().values() {
            e.cancel.set(reason);
            e.token.cancel();
        }
    }

    /// Cancel every entry that has no cancel reason yet; returns their ids.
    /// Already-cancelled entries keep their reason and are not reported.
    pub fn cancel_pending(&self, reason: CancelReason) -> Vec<i64> {
        let mut cancelled = Vec::new();
        for (id, e) in self.map().iter() {
            if e.cancel.get().is_none() {
                e.cancel.set(reason);
                e.token.cancel();
                cancelled.push(*id);
            }
        }
        cancelled
    }

    /// Put back a value whose write failed, unless the handler reported a newer
    /// one meanwhile or the entry is gone or belongs to another attempt.
    pub fn restore_progress(&self, job_id: i64, attempt: i32, value: Value) {
        if let Some(e) = self.map().get(&job_id).filter(|e| e.attempt == attempt) {
            let mut slot = e.progress.lock().unwrap_or_else(PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(value);
            }
        }
    }

    /// Pending progress values, removed from their slots, with the fence to write them under.
    pub fn take_progress(&self, process_id: i64) -> Vec<(Fence, Value)> {
        self.map()
            .iter()
            .filter_map(|(id, e)| {
                let value = e
                    .progress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()?;
                Some((
                    Fence {
                        job_id: *id,
                        process_id,
                        attempt: e.attempt,
                    },
                    value,
                ))
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entry(attempt: i32) -> RunningEntry {
        RunningEntry {
            attempt,
            token: CancellationToken::new(),
            cancel: Arc::new(CancelCell::default()),
            progress: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn cancel_sets_the_reason_once_and_fires_the_token() {
        let r = Running::default();
        let e = entry(1);
        let (token, cell) = (e.token.clone(), e.cancel.clone());
        r.insert(5, e);
        r.cancel(5, CancelReason::Requested);
        r.cancel(5, CancelReason::Shutdown);
        assert!(token.is_cancelled());
        assert_eq!(cell.get(), Some(CancelReason::Requested));
        r.cancel(99, CancelReason::Requested); // unknown id: no-op
    }

    #[test]
    fn cancel_pending_skips_entries_that_already_have_a_reason() {
        let r = Running::default();
        let (a, b) = (entry(1), entry(1));
        let (ca, cb) = (a.cancel.clone(), b.cancel.clone());
        r.insert(1, a);
        r.insert(2, b);
        r.cancel(1, CancelReason::Requested);
        assert_eq!(r.cancel_pending(CancelReason::LeaseLost), vec![2]);
        assert!(r.cancel_pending(CancelReason::LeaseLost).is_empty());
        assert_eq!(ca.get(), Some(CancelReason::Requested));
        assert_eq!(cb.get(), Some(CancelReason::LeaseLost));
    }

    #[test]
    fn restore_progress_never_overwrites_a_newer_value() {
        let r = Running::default();
        let e = entry(2);
        let slot = e.progress.clone();
        r.insert(1, e);
        r.restore_progress(1, 2, serde_json::json!("old"));
        assert_eq!(*slot.lock().unwrap(), Some(serde_json::json!("old")));
        *slot.lock().unwrap() = Some(serde_json::json!("new"));
        r.restore_progress(1, 2, serde_json::json!("older"));
        assert_eq!(*slot.lock().unwrap(), Some(serde_json::json!("new")));
        slot.lock().unwrap().take();
        r.restore_progress(1, 1, serde_json::json!("other attempt"));
        r.restore_progress(9, 2, serde_json::json!("unknown job"));
        assert_eq!(*slot.lock().unwrap(), None);
    }

    #[test]
    fn take_progress_drains_pending_values_with_their_fence() {
        let r = Running::default();
        let e = entry(3);
        *e.progress.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(serde_json::json!({"done": 1}));
        r.insert(7, e);
        r.insert(8, entry(1));
        let taken = r.take_progress(42);
        assert_eq!(taken.len(), 1);
        assert_eq!(
            taken[0].0,
            Fence {
                job_id: 7,
                process_id: 42,
                attempt: 3
            }
        );
        assert!(r.take_progress(42).is_empty());
        let mut ids = r.ids();
        ids.sort();
        assert_eq!(ids, vec![7, 8]);
        r.cancel_all(CancelReason::Shutdown);
        r.remove(7);
        assert_eq!(r.len(), 1);
    }
}
