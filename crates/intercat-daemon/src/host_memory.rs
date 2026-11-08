//! Host memory admission, independent of the number of running model turns.
//! Reclamation is a request: the worker rechecks its idle boundary before closing.
use crate::{Result, process::Identity};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::watch;

const INITIAL_HOST_BYTES: u64 = 260 * 1024 * 1024;
const IDLE_LIFETIME: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub free_bytes: u64,
    pub pressured: bool,
}

/// These APIs report system pressure and free pages, not this process's limit.
/// os_proc_available_memory is unavailable on macOS.
#[allow(deprecated)] // libc retains the ABI bindings used by the current macOS SDK.
pub fn sample() -> Result<Sample> {
    let mut pressure = 0u32;
    let mut size = std::mem::size_of_val(&pressure);
    // SAFETY: the fixed C name and output buffer live through this read-only call.
    if unsafe {
        libc::sysctlbyname(
            c"kern.memorystatus_vm_pressure_level".as_ptr(),
            (&mut pressure as *mut u32).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if size != std::mem::size_of_val(&pressure) || !matches!(pressure, 1 | 2 | 4) {
        return Err("System memory pressure is unavailable.".into());
    }
    // SAFETY: zero is valid initialization; the count describes this buffer.
    let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let port = unsafe { libc::mach_host_self() };
    let result = unsafe {
        libc::host_statistics64(
            port,
            libc::HOST_VM_INFO64,
            (&mut stats as *mut libc::vm_statistics64).cast(),
            &mut count,
        )
    };
    unsafe extern "C" {
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }
    // SAFETY: mach_host_self returned this owned send right; release it once.
    unsafe {
        mach_port_deallocate(libc::mach_task_self(), port);
    }
    if result != 0 || count < 4 {
        return Err("Free system memory is unavailable.".into());
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err("The system page size is unavailable.".into());
    }
    Ok(Sample {
        free_bytes: u64::from(stats.free_count).saturating_mul(page as u64),
        pressured: pressure != 1,
    })
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Waiting {
    pub owner: String,
    pub position: usize,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub sample: Option<Sample>,
    pub error: Option<String>,
    pub estimated_host_bytes: u64,
    pub hosts: usize,
    pub waiting: Vec<Waiting>,
}
struct Entry {
    identity: Option<Identity>,
    attached_at: Option<Instant>,
    idle: Option<Instant>,
    reclaim: watch::Sender<bool>,
}
struct State {
    sample: Option<Sample>,
    sampled_at: Option<Instant>,
    error: Option<String>,
    estimate: u64,
    next: u64,
    waiting: BTreeMap<u64, String>,
    hosts: BTreeMap<u64, Entry>,
}
pub struct Pool {
    state: Mutex<State>,
    changes: watch::Sender<u64>,
    #[cfg(test)]
    fixed: bool,
}
impl Pool {
    pub fn new() -> Arc<Self> {
        Self::start(None)
    }
    fn start(initial: Option<Sample>) -> Arc<Self> {
        let pool = Arc::new(Self {
            state: Mutex::new(State {
                sample: initial,
                sampled_at: initial.map(|_| Instant::now()),
                error: None,
                estimate: INITIAL_HOST_BYTES,
                next: 0,
                waiting: BTreeMap::new(),
                hosts: BTreeMap::new(),
            }),
            changes: watch::channel(0).0,
            #[cfg(test)]
            fixed: initial.is_some(),
        });
        let weak = Arc::downgrade(&pool);
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(1));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                let Some(pool) = weak.upgrade() else { break };
                #[cfg(test)]
                if pool.fixed {
                    continue;
                }
                let identities = pool
                    .state
                    .lock()
                    .unwrap()
                    .hosts
                    .values()
                    .filter_map(|host| host.identity)
                    .collect::<Vec<_>>();
                let values = tokio::task::spawn_blocking(move || {
                    let sampled_at = Instant::now();
                    let current = sample();
                    let largest = identities
                        .into_iter()
                        .filter(|id| crate::process::is_alive(*id))
                        .filter_map(|id| {
                            libproc::libproc::pid_rusage::pidrusage::<
                                libproc::libproc::pid_rusage::RUsageInfoV0,
                            >(id.pid as i32)
                            .ok()
                        })
                        .map(|value| value.ri_resident_size)
                        .max();
                    (current, largest, sampled_at)
                })
                .await;
                if let Ok((sample, largest, sampled_at)) = values {
                    let mut state = pool.state.lock().unwrap();
                    if let Some(largest) = largest {
                        state.estimate = state.estimate.max(largest);
                    }
                    match sample {
                        Ok(sample) => {
                            state.sample = Some(sample);
                            state.sampled_at = Some(sampled_at);
                            state.error = None;
                        }
                        Err(error) => {
                            state.sample = None;
                            state.error = Some(error.to_string());
                        }
                    }
                    Self::maintain(&mut state);
                    drop(state);
                    pool.changed();
                }
            }
        });
        pool
    }
    fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
    pub fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().unwrap();
        Snapshot {
            sample: state.sample,
            error: state.error.clone(),
            estimated_host_bytes: state.estimate,
            hosts: state.hosts.len(),
            waiting: state
                .waiting
                .values()
                .enumerate()
                .map(|(i, owner)| Waiting {
                    owner: owner.clone(),
                    position: i + 1,
                })
                .collect(),
        }
    }
    pub fn request(self: &Arc<Self>, owner: String) -> Ticket {
        let mut state = self.state.lock().unwrap();
        let id = state.next;
        state.next += 1;
        state.waiting.insert(id, owner);
        drop(state);
        self.changed();
        Ticket {
            pool: self.clone(),
            id: Some(id),
        }
    }
    fn reclaim(state: &mut State) {
        if state.hosts.values().any(|entry| *entry.reclaim.borrow()) {
            return;
        }
        if let Some((_, entry)) = state
            .hosts
            .iter()
            .filter_map(|(id, entry)| entry.idle.map(|idle| ((idle, *id), entry)))
            .min_by_key(|(key, _)| *key)
        {
            entry.reclaim.send_replace(true);
        }
    }
    fn maintain(state: &mut State) {
        if state.sample.is_some_and(|sample| sample.pressured)
            || state.hosts.values().any(|host| {
                host.idle
                    .is_some_and(|idle| idle.elapsed() >= IDLE_LIFETIME)
            })
        {
            Self::reclaim(state);
        }
    }
    #[cfg(test)]
    pub(crate) fn fixed(sample: Sample) -> Arc<Self> {
        Self::start(Some(sample))
    }
    #[cfg(test)]
    pub(crate) fn set_sample(&self, sample: Sample) {
        let mut state = self.state.lock().unwrap();
        state.sample = Some(sample);
        state.sampled_at = Some(Instant::now());
        Self::maintain(&mut state);
        drop(state);
        self.changed();
    }
}
pub struct Ticket {
    pool: Arc<Pool>,
    id: Option<u64>,
}
impl Ticket {
    pub fn try_acquire(&mut self) -> Option<Lease> {
        let id = self.id?;
        let mut state = self.pool.state.lock().unwrap();
        if state.waiting.first_key_value().map(|(id, _)| *id) != Some(id) {
            return None;
        }
        let reserved = state
            .hosts
            .values()
            .filter(|entry| {
                entry
                    .attached_at
                    .is_none_or(|at| state.sampled_at.is_none_or(|sampled| at >= sampled))
            })
            .count() as u64
            * state.estimate;
        let available = state
            .sample
            .filter(|sample| !sample.pressured)
            .map(|sample| sample.free_bytes / 5 * 3)
            .unwrap_or(0);
        if available.saturating_sub(reserved) < state.estimate {
            Pool::reclaim(&mut state);
            return None;
        }
        state.waiting.remove(&id);
        let (reclaim, _) = watch::channel(false);
        state.hosts.insert(
            id,
            Entry {
                identity: None,
                attached_at: None,
                idle: None,
                reclaim,
            },
        );
        self.id = None;
        drop(state);
        self.pool.changed();
        Some(Lease {
            pool: self.pool.clone(),
            id,
        })
    }
    pub async fn acquire(mut self, guard: crate::msp::StartGuard) -> Result<Lease> {
        let mut changes = self.pool.subscribe();
        loop {
            if !guard.current() {
                return Err(crate::msp::StartCancelled.into());
            }
            if let Some(lease) = self.try_acquire() {
                return Ok(lease);
            }
            tokio::select! {_=changes.changed()=>{},_=tokio::time::sleep(Duration::from_millis(100))=>{}}
        }
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.pool.state.lock().unwrap().waiting.remove(&id);
            self.pool.changed();
        }
    }
}
pub struct Lease {
    pool: Arc<Pool>,
    id: u64,
}
impl Lease {
    pub fn attach(&self, identity: Identity) {
        let mut state = self.pool.state.lock().unwrap();
        let entry = state.hosts.get_mut(&self.id).unwrap();
        entry.identity = Some(identity);
        entry.attached_at = Some(Instant::now());
        drop(state);
        self.pool.changed();
    }
    pub fn reclaim_requested(&self) -> bool {
        *self.pool.state.lock().unwrap().hosts[&self.id]
            .reclaim
            .borrow()
    }
    pub fn set_idle(&self) {
        let mut state = self.pool.state.lock().unwrap();
        let entry = state.hosts.get_mut(&self.id).unwrap();
        entry.idle.get_or_insert_with(Instant::now);
    }
    pub fn claim_work(&self) -> bool {
        let mut state = self.pool.state.lock().unwrap();
        let entry = state.hosts.get_mut(&self.id).unwrap();
        if *entry.reclaim.borrow() {
            false
        } else {
            entry.idle = None;
            true
        }
    }
    pub fn defer_reclaim(&self) {
        let mut state = self.pool.state.lock().unwrap();
        let entry = state.hosts.get_mut(&self.id).unwrap();
        entry.idle = None;
        entry.reclaim.send_replace(false);
        drop(state);
        self.pool.changed();
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.pool.state.lock().unwrap().hosts.remove(&self.id);
        self.pool.changed();
    }
}

#[derive(Debug)]
pub(crate) struct Reclaimed;
impl std::fmt::Display for Reclaimed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Idle host closed to make memory available.")
    }
}
impl std::error::Error for Reclaimed {}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reservations_serialize_new_hosts_and_cancellation_removes_the_waiter() {
        let pool = Pool::fixed(Sample {
            free_bytes: INITIAL_HOST_BYTES * 2,
            pressured: false,
        });
        let mut first = pool.request("A".into());
        let active = first.try_acquire().unwrap();
        let mut second = pool.request("B".into());
        assert!(second.try_acquire().is_none());
        drop(second);
        assert!(pool.snapshot().waiting.is_empty());
        drop(active);
        assert_eq!(pool.snapshot().hosts, 0);
    }
    #[tokio::test]
    async fn only_the_oldest_idle_host_receives_a_reclaim_request() {
        let pool = Pool::fixed(Sample {
            free_bytes: INITIAL_HOST_BYTES * 10,
            pressured: false,
        });
        let active = pool.request("active".into()).try_acquire().unwrap();
        let first = pool.request("first".into()).try_acquire().unwrap();
        first.set_idle();
        let second = pool.request("second".into()).try_acquire().unwrap();
        second.set_idle();
        pool.set_sample(Sample {
            free_bytes: 0,
            pressured: true,
        });
        assert!(first.reclaim_requested());
        assert!(!second.reclaim_requested());
        assert!(!active.reclaim_requested());
        assert!(!first.claim_work());
        drop(first);
        pool.set_sample(Sample {
            free_bytes: 0,
            pressured: true,
        });
        assert!(second.reclaim_requested());
    }
    #[tokio::test]
    async fn changed_work_defers_reclamation_and_waiting_resumes_after_memory_returns() {
        let pool = Pool::fixed(Sample {
            free_bytes: INITIAL_HOST_BYTES * 2,
            pressured: false,
        });
        let host = pool.request("A".into()).try_acquire().unwrap();
        host.set_idle();
        pool.set_sample(Sample {
            free_bytes: 0,
            pressured: true,
        });
        assert!(host.reclaim_requested());
        host.defer_reclaim();
        assert!(host.claim_work());
        let mut waiting = pool.request("B".into());
        assert!(waiting.try_acquire().is_none());
        pool.set_sample(Sample {
            free_bytes: INITIAL_HOST_BYTES * 10,
            pressured: false,
        });
        assert!(waiting.try_acquire().is_some());
    }
    #[tokio::test]
    async fn five_minute_expiry_closes_only_the_expired_idle_host() {
        let sample = Sample {
            free_bytes: INITIAL_HOST_BYTES * 10,
            pressured: false,
        };
        let pool = Pool::fixed(sample);
        let active = pool.request("active".into()).try_acquire().unwrap();
        let old = pool.request("old".into()).try_acquire().unwrap();
        let recent = pool.request("recent".into()).try_acquire().unwrap();
        old.set_idle();
        recent.set_idle();
        pool.state
            .lock()
            .unwrap()
            .hosts
            .get_mut(&old.id)
            .unwrap()
            .idle = Some(Instant::now() - IDLE_LIFETIME);
        pool.set_sample(sample);
        assert!(old.reclaim_requested());
        assert!(!active.reclaim_requested());
        assert!(!recent.reclaim_requested());
        drop(old);
        pool.set_sample(sample);
        assert!(!recent.reclaim_requested());
    }
    #[tokio::test]
    async fn attached_hosts_keep_their_reservation_until_a_new_sample() {
        let pool = Pool::fixed(Sample {
            free_bytes: INITIAL_HOST_BYTES * 2,
            pressured: false,
        });
        let host = pool.request("A".into()).try_acquire().unwrap();
        host.attach(Identity {
            pid: 1,
            started_seconds: 0,
            started_micros: 0,
        });
        let mut next = pool.request("B".into());
        assert!(next.try_acquire().is_none());
        pool.set_sample(Sample {
            free_bytes: INITIAL_HOST_BYTES * 2,
            pressured: false,
        });
        assert!(next.try_acquire().is_some());
    }
    #[test]
    #[ignore = "read-only native macOS memory API probe"]
    fn native_memory_sample() {
        let value = sample().unwrap();
        assert!(value.free_bytes > 0);
        eprintln!("{}", serde_json::to_string(&value).unwrap());
    }
}
