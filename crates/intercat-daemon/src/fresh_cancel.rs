//! Finish a persisted cancellation only after owned hosts and captures end.
use crate::{
    Result,
    fresh::{Change, Job, Phase},
    journal::Journal,
};
use std::path::Path;

pub(crate) async fn complete(data: &Path, journal: &Journal, job: &Job) -> Result<Job> {
    if job.phase != Phase::Cancelling {
        return Err("This Start fresh request is not cancelling.".into());
    }
    crate::fresh_files::stopped(data, journal, job).await?;
    if let Some(summary) = job.summary_id {
        let tasks = journal.summary_tasks(summary).await?;
        let data = data.to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let workspace = data.join("summary-workspaces").join(summary.to_string());
            let mut failures = Vec::new();
            for task in tasks {
                let result = (|| -> Result<()> {
                    if let Some(mut tracker) = crate::process::Tracker::restore(
                        &data.join("summary-hosts").join(task.to_string()),
                        &workspace,
                        None,
                    )? {
                        tracker.stop_confirmed()?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    failures.push(error.to_string());
                }
            }
            if !failures.is_empty() {
                return Err(failures.join("\n").into());
            }
            Ok(())
        })
        .await??;
    }
    journal
        .fresh(Change::Cancelled {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await
}
