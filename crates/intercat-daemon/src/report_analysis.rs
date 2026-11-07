//! One owner batches background reports; follow-up gates keep their individual priority.
use crate::{
    Result,
    editor::{Manager, Priority},
    journal::Journal,
    reports::{self, Analysis, Report},
};
use serde_json::Value;
use std::time::Duration;
use tokio::{sync::watch, task::JoinHandle, time::Instant};

fn pending(snapshot: &Value) -> Vec<Report> {
    snapshot["reports"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| serde_json::from_value(value.clone()).ok())
        .collect()
}

fn current(snapshot: &Value, selected: &[Report], background: bool) -> bool {
    let pending = pending(snapshot);
    selected.iter().all(|source| {
        pending
            .iter()
            .any(|report| report.id == source.id && report.revision == source.revision)
    }) && (!background
        || !pending
            .iter()
            .any(|report| reports::priority(report, snapshot) == Priority::Question))
}

async fn save(journal: &Journal, results: &[(Report, Analysis)], background: bool) -> Result<()> {
    if background {
        journal.analyze_background_reports(results.to_vec()).await?;
    } else if let Some((report, analysis)) = results.first() {
        journal
            .analyze_report(report.id.clone(), report.revision, analysis.clone())
            .await?;
    }
    Ok(())
}

pub struct Service {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Service {
    pub fn start(journal: Journal, editor: Manager) -> Self {
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut changes = journal.subscribe();
            let mut status = editor.subscribe();
            let mut next_background = Instant::now() + Duration::from_secs(5);
            let mut unsaved: Option<(Vec<(Report, Analysis)>, bool)> = None;
            loop {
                if *stopping.borrow() {
                    return;
                }
                if let Some((results, background)) = &unsaved {
                    let saved = tokio::select! {
                        _ = stopping.changed() => return,
                        saved = save(&journal, results, *background) => saved,
                    };
                    if saved.is_ok() {
                        unsaved = None;
                    } else {
                        tokio::select! {
                            _ = stopping.changed() => return,
                            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
                        }
                    }
                    continue;
                }
                let snapshot = match journal.snapshot().await {
                    Ok(value) => value,
                    Err(_) => {
                        tokio::select! {
                            _ = stopping.changed() => return,
                            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
                        }
                        continue;
                    }
                };
                let reports = pending(&snapshot);
                let urgent = reports
                    .iter()
                    .find(|report| reports::priority(report, &snapshot) == Priority::Question)
                    .cloned();
                let background = urgent.is_none();
                let selected = if let Some(report) = urgent {
                    vec![report]
                } else if Instant::now() >= next_background {
                    next_background = Instant::now() + Duration::from_secs(5);
                    reports
                } else {
                    Vec::new()
                };
                if selected.is_empty() {
                    tokio::select! {
                        _ = stopping.changed() => return,
                        changed = changes.changed() => if changed.is_err() { return; },
                        changed = status.changed() => if changed.is_err() { return; },
                        _ = tokio::time::sleep_until(next_background) => {},
                    }
                    continue;
                }
                let reading = crate::input_readings::Ticket {
                    input: uuid::Uuid::now_v7().to_string(),
                    revision: 0,
                    index: 1,
                };
                let _cancel = background.then(|| crate::editor::ReadingCancellation {
                    editor: editor.clone(),
                    request: reading.clone(),
                });
                let generation = editor.status().generation;
                let work = async {
                    if background {
                        reports::analyze_batch(&selected, &editor, &journal, &reading).await
                    } else {
                        Some(vec![
                            reports::analyze_urgent(&selected[0], &editor, &journal).await,
                        ])
                    }
                };
                tokio::pin!(work);
                let result = loop {
                    tokio::select! {
                        _ = stopping.changed() => return,
                        result = &mut work => break result,
                        changed = changes.changed() => {
                            if changed.is_err() { return; }
                            if !journal.snapshot().await.is_ok_and(|snapshot| current(&snapshot, &selected, background)) { break None; }
                        },
                        changed = status.changed() => {
                            if changed.is_err() { return; }
                            if editor.status().generation != generation { break None; }
                        }
                    }
                };
                if let Some(analyses) = result {
                    unsaved = Some((selected.iter().cloned().zip(analyses).collect(), background));
                }
            }
        });
        Self { stop, task }
    }

    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}
