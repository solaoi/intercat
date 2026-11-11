//! Verify each starting tree without changing its files, HEAD, or real index.
use crate::{Result, journal::Journal, two_ways::Job};
use intercat_git::Repository;
async fn verify(journal: &Journal, track: &str, point: &str) -> Result<()> {
    let workspace = journal.workspace(track.into()).await?;
    let captured = crate::checkpoint::capture(
        &workspace,
        point,
        "Intercat paired starting tree verification",
        journal,
    )
    .await?;
    let path = workspace.path;
    let expected = point.parse()?;
    let current = captured.sha.parse()?;
    let equal = tokio::task::spawn_blocking(move || -> Result<bool> {
        let repo = Repository::open(path)?;
        Ok(repo.tree_id(expected)? == repo.tree_id(current)?)
    })
    .await??;
    if !equal || !captured.unmerged_paths.is_empty() {
        return Err("The files changed since the paired starting point. Review this attempt before sending it.".into());
    }
    Ok(())
}
pub(crate) async fn before_publish(journal: &Journal, job: &Job, child: &str) -> Result<()> {
    let point = &job
        .point
        .as_ref()
        .ok_or("Missing paired starting point.")?
        .sha();
    verify(journal, &job.source_track, point).await?;
    verify(journal, child, point).await
}
pub(crate) async fn before_turn(journal: &Journal, input: &str, track: &str) -> Result<()> {
    if let Some(point) = journal.two_ways_start(input.into(), track.into()).await? {
        verify(journal, track, point.sha()).await?;
    }
    Ok(())
}
