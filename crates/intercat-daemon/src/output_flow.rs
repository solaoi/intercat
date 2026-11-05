//! One host's receive-to-journal budget. A lease survives queue hand-offs.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const LIMIT: usize = 8 * 1024 * 1024;
pub(crate) const HISTORY_LIMIT: usize = 64 * 1024 * 1024;
const HIGH: usize = LIMIT / 2;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub queued_bytes: usize,
    pub oldest_queued_ms: u64,
    pub dropped_deltas: u64,
    pub interrupted: bool,
    pub lagging: bool,
}
#[derive(Clone)]
pub struct Flow(Arc<Inner>);
struct Inner {
    budget: Arc<Semaphore>,
    history: Arc<Semaphore>,
    pending: Mutex<BTreeMap<u64, (Instant, usize)>>,
    next: AtomicU64,
    dropped: AtomicU64,
    interrupted: AtomicBool,
    settled: AtomicBool,
}
#[derive(Clone)]
pub struct Lease {
    _reservation: Arc<Reservation>,
}
struct Reservation {
    flow: Flow,
    id: u64,
    _permit: OwnedSemaphorePermit,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.flow.0.pending.lock().unwrap().remove(&self.id);
    }
}
impl Default for Flow {
    fn default() -> Self {
        Self(Arc::new(Inner {
            budget: Arc::new(Semaphore::new(LIMIT)),
            history: Arc::new(Semaphore::new(1)),
            pending: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            interrupted: AtomicBool::new(false),
            settled: AtomicBool::new(false),
        }))
    }
}
impl Flow {
    pub fn status(&self) -> Status {
        let pending = self.0.pending.lock().unwrap();
        let queued_bytes = pending.values().map(|(_, bytes)| bytes).sum();
        let oldest_queued_ms = pending.first_key_value().map_or(0, |(_, (time, _))| {
            time.elapsed().as_millis().min(u64::MAX as u128) as u64
        });
        Status {
            queued_bytes,
            oldest_queued_ms,
            dropped_deltas: self.0.dropped.load(Ordering::Relaxed),
            interrupted: self.0.interrupted.load(Ordering::Acquire),
            lagging: queued_bytes >= HIGH || oldest_queued_ms >= 2000,
        }
    }
    pub fn drop_deltas(&self, count: u64) {
        self.0.dropped.fetch_add(count, Ordering::Relaxed);
    }
    pub fn interrupt(&self) -> bool {
        !self.0.interrupted.swap(true, Ordering::AcqRel)
    }
    pub fn settle_interrupt(&self) {
        self.0.settled.store(true, Ordering::Release);
    }
    pub fn needs_force(&self) -> bool {
        self.0.interrupted.load(Ordering::Acquire) && !self.0.settled.load(Ordering::Acquire)
    }
    fn lease(&self, permit: OwnedSemaphorePermit, bytes: usize) -> Lease {
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        self.0
            .pending
            .lock()
            .unwrap()
            .insert(id, (Instant::now(), bytes));
        Lease {
            _reservation: Arc::new(Reservation {
                flow: self.clone(),
                id,
                _permit: permit,
            }),
        }
    }
    pub fn reserve(&self, bytes: usize) -> Option<Lease> {
        if bytes > LIMIT {
            return None;
        }
        self.0
            .budget
            .clone()
            .try_acquire_many_owned(bytes.max(256) as u32)
            .ok()
            .map(|p| self.lease(p, bytes.max(256)))
    }
    pub async fn wait(&self, bytes: usize) -> Option<Lease> {
        if bytes > LIMIT {
            return None;
        }
        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            self.0
                .budget
                .clone()
                .acquire_many_owned(bytes.max(256) as u32),
        )
        .await
        .ok()?
        .ok()
        .map(|p| self.lease(p, bytes.max(256)))
    }

    /// One solicited history response may occupy the wire frame limit. Its
    /// lease lasts through persistence; live notifications keep their own cap.
    pub(crate) fn reserve_history(&self, bytes: usize) -> Option<Lease> {
        if bytes <= LIMIT {
            return self.reserve(bytes);
        }
        if bytes > HISTORY_LIMIT {
            return None;
        }
        self.0
            .history
            .clone()
            .try_acquire_owned()
            .ok()
            .map(|p| self.lease(p, bytes))
    }

    pub(crate) async fn wait_history(&self, bytes: usize) -> Option<Lease> {
        if bytes <= LIMIT {
            return self.wait(bytes).await;
        }
        if bytes > HISTORY_LIMIT {
            return None;
        }
        tokio::time::timeout(
            std::time::Duration::from_millis(250),
            self.0.history.clone().acquire_owned(),
        )
        .await
        .ok()?
        .ok()
        .map(|p| self.lease(p, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_history_lease_is_bounded_and_does_not_consume_the_notification_budget() {
        let flow = Flow::default();
        let history = flow.reserve_history(LIMIT + 1024).unwrap();
        let held = history.clone();
        assert!(flow.reserve_history(LIMIT + 1).is_none());
        assert!(flow.reserve_history(HISTORY_LIMIT + 1).is_none());
        let live = flow.reserve(LIMIT).unwrap();
        assert_eq!(flow.status().queued_bytes, 2 * LIMIT + 1024);
        assert!(flow.reserve(1).is_none());
        drop(history);
        assert!(flow.reserve_history(LIMIT + 1).is_none());
        drop(held);
        assert!(flow.reserve_history(LIMIT + 1).is_some());
        drop(live);
        assert_eq!(flow.status().queued_bytes, 0);
    }
}
