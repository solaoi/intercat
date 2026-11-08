//! Shared turn admission. Requests survive in the journal; tickets only arbitrate
//! their current eligible execution attempts across worker actors.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Priority {
    Continuation,
    User,
    Plan,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Waiting {
    pub id: String,
    pub priority: Priority,
    pub position: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub limit: usize,
    pub active: usize,
    pub waiting: Vec<Waiting>,
}

type Key = (Priority, String, u64);
struct State {
    limit: usize,
    revision: u64,
    active: usize,
    sequence: u64,
    waiting: BTreeMap<Key, ()>,
    turns: BTreeMap<(String, uuid::Uuid), u64>,
}

pub struct Capacity {
    state: Mutex<State>,
    changes: watch::Sender<u64>,
}

impl Capacity {
    pub fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(State {
                limit,
                revision: 0,
                active: 0,
                sequence: 0,
                waiting: BTreeMap::new(),
                turns: BTreeMap::new(),
            }),
            changes: watch::channel(0).0,
        }
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
    fn changed(&self) {
        self.changes.send_modify(|revision| *revision += 1);
    }
    pub fn set_limit(&self, revision: u64, limit: usize) {
        let mut state = self.state.lock().unwrap();
        if revision > state.revision {
            state.revision = revision;
            state.limit = limit;
            self.changed();
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().unwrap();
        Snapshot {
            limit: state.limit,
            active: state.active,
            waiting: state
                .waiting
                .keys()
                .enumerate()
                .map(|(index, (priority, id, _))| Waiting {
                    id: id.clone(),
                    priority: *priority,
                    position: index + 1,
                })
                .collect(),
        }
    }
    pub fn available(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.limit.saturating_sub(state.active)
    }
    /// IDs are the journal's time-ordered operation/input IDs. Re-registering
    /// after an actor restart therefore preserves the request's original order.
    pub fn request(self: &Arc<Self>, id: String, priority: Priority) -> Ticket {
        let mut state = self.state.lock().unwrap();
        let key = (priority, id, state.sequence);
        state.sequence += 1;
        state.waiting.insert(key.clone(), ());
        self.changed();
        Ticket {
            capacity: self.clone(),
            key: Some(key),
        }
    }
}

pub struct Ticket {
    capacity: Arc<Capacity>,
    key: Option<Key>,
}
impl Ticket {
    pub fn id(&self) -> &str {
        &self.key.as_ref().expect("Pending admission").1
    }
    pub fn priority(&self) -> Priority {
        self.key.as_ref().expect("Pending admission").0
    }
    pub fn try_acquire(&mut self) -> Option<Permit> {
        let key = self.key.as_ref()?;
        let mut state = self.capacity.state.lock().unwrap();
        if state.active >= state.limit
            || state.waiting.first_key_value().map(|(first, _)| first) != Some(key)
        {
            return None;
        }
        state.waiting.remove(key);
        state.active += 1;
        let id = key.2;
        self.key = None;
        self.capacity.changed();
        Some(Permit {
            capacity: self.capacity.clone(),
            id,
            turn: None,
            counted: true,
        })
    }
    pub async fn acquire(mut self) -> Permit {
        let mut changes = self.capacity.subscribe();
        loop {
            if let Some(permit) = self.try_acquire() {
                return permit;
            }
            changes
                .changed()
                .await
                .expect("Ticket retains its capacity");
        }
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(key) = &self.key {
            self.capacity.state.lock().unwrap().waiting.remove(key);
            self.capacity.changed();
        }
    }
}

pub struct Permit {
    capacity: Arc<Capacity>,
    id: u64,
    turn: Option<(String, uuid::Uuid)>,
    counted: bool,
}
impl Permit {
    /// A start reservation becomes the one counted owner of an actual turn.
    /// A steered input can name a turn that already has a counted owner.
    pub fn bind_turn(&mut self, run: &str, turn: uuid::Uuid) -> crate::Result<()> {
        let key = (run.to_owned(), turn);
        if let Some(previous) = &self.turn {
            return if *previous == key {
                Ok(())
            } else {
                Err("A running slot cannot be rebound to another turn.".into())
            };
        }
        let mut state = self.capacity.state.lock().unwrap();
        if state.turns.get(&key).is_some_and(|id| *id != self.id) {
            state.active -= 1;
            self.counted = false;
        } else {
            state.turns.insert(key.clone(), self.id);
        }
        self.turn = Some(key);
        drop(state);
        self.capacity.changed();
        Ok(())
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if !self.counted {
            return;
        }
        let mut state = self.capacity.state.lock().unwrap();
        if let Some(turn) = &self.turn
            && state.turns.get(turn) == Some(&self.id)
        {
            state.turns.remove(turn);
        }
        state.active -= 1;
        drop(state);
        self.capacity.changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_steered_reservation_does_not_count_or_release_the_existing_turn_twice() {
        let capacity = Arc::new(Capacity::new(2));
        let turn = uuid::Uuid::now_v7();
        let mut running = capacity
            .request("first".into(), Priority::User)
            .try_acquire()
            .unwrap();
        running.bind_turn("A", turn).unwrap();
        let mut extra = capacity
            .request("steered".into(), Priority::User)
            .try_acquire()
            .unwrap();
        assert_eq!(capacity.snapshot().active, 2);
        extra.bind_turn("A", turn).unwrap();
        extra.bind_turn("A", turn).unwrap();
        assert_eq!(capacity.snapshot().active, 1);
        drop(extra);
        assert_eq!(capacity.snapshot().active, 1);
        drop(running);
        assert_eq!(capacity.snapshot().active, 0);
    }
    #[test]
    fn separate_runs_do_not_share_a_turn_slot_and_a_bound_slot_cannot_change_targets() {
        let capacity = Arc::new(Capacity::new(2));
        let turn = uuid::Uuid::now_v7();
        let mut a = capacity
            .request("a".into(), Priority::User)
            .try_acquire()
            .unwrap();
        let mut b = capacity
            .request("b".into(), Priority::User)
            .try_acquire()
            .unwrap();
        a.bind_turn("A", turn).unwrap();
        b.bind_turn("B", turn).unwrap();
        assert_eq!(capacity.snapshot().active, 2);
        assert!(a.bind_turn("B", turn).is_err());
        assert_eq!(capacity.snapshot().active, 2);
        drop(a);
        assert_eq!(capacity.snapshot().active, 1);
        drop(b);
        assert_eq!(capacity.snapshot().active, 0);
    }
    #[tokio::test]
    async fn shrinking_waits_for_active_turns_and_increasing_wakes_the_queue() {
        let capacity = Arc::new(Capacity::new(2));
        let first = capacity.request("1".into(), Priority::User).acquire().await;
        let second = capacity.request("2".into(), Priority::User).acquire().await;
        capacity.set_limit(1, 1);
        let mut next = capacity.request("3".into(), Priority::User);
        assert!(next.try_acquire().is_none());
        drop(first);
        assert!(next.try_acquire().is_none());
        let waiting = tokio::spawn(next.acquire());
        capacity.set_limit(2, 2);
        let third = tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(capacity.snapshot().active, 2);
        drop((second, third));
        assert_eq!(capacity.available(), 2);
    }
    #[test]
    fn cancellations_and_reclaimed_starts_never_leak_capacity() {
        let capacity = Arc::new(Capacity::new(1));
        for _ in 0..10 {
            let mut ticket = capacity.request("1".into(), Priority::User);
            let permit = ticket.try_acquire().unwrap();
            let cancelled = capacity.request("2".into(), Priority::User);
            drop(cancelled);
            assert!(capacity.snapshot().waiting.is_empty());
            drop(permit);
            assert_eq!(capacity.available(), 1);
        }
    }
    #[test]
    fn continuations_and_user_requests_precede_plans_and_keep_original_order() {
        let capacity = Arc::new(Capacity::new(0));
        let mut plan = capacity.request("1".into(), Priority::Plan);
        let mut later = capacity.request("3".into(), Priority::User);
        let mut earlier = capacity.request("2".into(), Priority::User);
        let mut reply = capacity.request("4".into(), Priority::Continuation);
        capacity.set_limit(1, 1);
        assert!(plan.try_acquire().is_none());
        assert!(earlier.try_acquire().is_none());
        drop(reply.try_acquire().unwrap());
        assert!(later.try_acquire().is_none());
        drop(earlier.try_acquire().unwrap());
        drop(later.try_acquire().unwrap());
        drop(plan.try_acquire().unwrap());
    }
}
