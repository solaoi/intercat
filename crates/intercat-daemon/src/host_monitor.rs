//! Enforce output-stop deadlines independently of the journal and execution loop.
use crate::{
    output_flow::Flow,
    process::{Identity, Tracker},
};
use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle, time::Instant};

pub struct OutputMonitor {
    stop: watch::Sender<bool>,
    task: JoinHandle<io::Result<()>>,
}
#[derive(Clone, Copy)]
enum Stage {
    Interrupt,
    Terminate,
    Kill,
}

impl OutputMonitor {
    pub fn start(flow: Flow, tracker: Arc<Mutex<Tracker>>, root: Identity) -> Self {
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut deadline = None;
            let mut stage = Stage::Interrupt;
            let mut watch_closed = false;
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    changed=stopping.changed(),if !watch_closed=>{
                        if changed.is_err() { watch_closed=true; if !flow.needs_force() { return Ok(()); } }
                        else if *stopping.borrow() { return Ok(()); }
                    },
                    _=tick.tick()=>{},
                }
                if !flow.needs_force() && matches!(stage, Stage::Interrupt) {
                    if deadline.is_some() || flow.status().interrupted {
                        return Ok(());
                    }
                    continue;
                }
                let until =
                    *deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(5));
                if Instant::now() < until {
                    continue;
                }
                let source = tracker.clone();
                let remaining =
                    tokio::task::spawn_blocking(move || -> io::Result<Vec<Identity>> {
                        let mut tracker = source.lock().unwrap();
                        let mut live = tracker.refresh()?.processes;
                        live.sort_by_key(|process| process.identity == root);
                        let remaining = live.iter().map(|p| p.identity).collect();
                        for process in live {
                            if !matches!(stage, Stage::Kill) {
                                tracker.terminate(
                                    process.identity,
                                    matches!(stage, Stage::Terminate),
                                )?;
                            }
                        }
                        Ok(remaining)
                    })
                    .await??;
                if remaining.is_empty() {
                    return Ok(());
                }
                (stage, deadline) = match stage {
                    Stage::Interrupt => (
                        Stage::Terminate,
                        Some(Instant::now() + Duration::from_secs(5)),
                    ),
                    Stage::Terminate => {
                        (Stage::Kill, Some(Instant::now() + Duration::from_secs(1)))
                    }
                    Stage::Kill => {
                        return Err(io::Error::other(format!(
                            "Output-stopped host still has live owned processes: {remaining:?}"
                        )));
                    }
                };
            }
        });
        Self { stop, task }
    }

    pub async fn stop(self) -> io::Result<()> {
        let _ = self.stop.send(true);
        self.task.await?
    }
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}
