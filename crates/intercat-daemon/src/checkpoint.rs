//! Isolated worktree snapshots and durable hidden refs.
use crate::{
    Result,
    journal::{Admission, Journal},
    preparation::Workspace,
};
use intercat_git::Repository;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub turn_id: Uuid,
    pub sha: String,
    pub reference: String,
    pub unmerged_paths: Vec<String>,
    pub live_processes: bool,
}

#[derive(Clone, Debug)]
pub struct Capture {
    pub sha: String,
    pub unmerged_paths: Vec<String>,
    pub lfs_directory: std::path::PathBuf,
}

/// Capture and sync the tree without publishing a ref or changing the real index.
pub async fn capture(
    workspace: &Workspace,
    parent: &str,
    message: &str,
    journal: &Journal,
) -> Result<Capture> {
    let workspace = workspace.clone();
    let journal = journal.clone();
    let parent = parent.to_owned();
    let message = message.to_owned();
    // Dropping the caller must not release ownership while an owned blocking
    // snapshot is still reading files. The task finishes and drops its permit.
    tokio::spawn(async move {
        let (capture, _permit) = capture_owned(&workspace, &parent, &message, &journal).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(capture)
    })
    .await?
}
pub(crate) async fn capture_owned(
    workspace: &Workspace,
    parent: &str,
    message: &str,
    journal: &Journal,
) -> Result<(Capture, tokio::sync::OwnedRwLockReadGuard<()>)> {
    let crate::capture_scope::Scope { context, permit } =
        journal.capture_scope(workspace.clone()).await?;
    let path = workspace.path.clone();
    let context = tokio::task::spawn_blocking(move || -> Result<_> {
        context.check_directory(&path)?;
        Ok(context)
    })
    .await??;
    let snapshot = crate::snapshot::capture(
        &workspace.path,
        workspace.base.parse()?,
        crate::snapshot::Purpose::Workspace {
            denied_roots: workspace.denied_roots.clone(),
            attachments: workspace.attachment_roots.clone(),
            exclusions: context.exclusions,
        },
    )
    .await?;
    let tree = snapshot.tree;
    let temp = snapshot.root.clone();
    let configuration = snapshot.configuration.clone();
    let path = workspace.path.clone();
    let temp_path = temp.clone();
    let parent = parent.parse()?;
    let message = message.to_owned();
    let lfs_directory = configuration.lfs_directory.clone();
    let sha = tokio::task::spawn_blocking(move || -> Result<_> {
        let repo = Repository::open(path)?;
        repo.import_tree(&temp_path, tree)?;
        crate::lfs_store::publish(
            &temp_path.join("lfs"),
            &lfs_directory,
            &repo.lfs_entries(tree)?,
        )?;
        let sha = repo.checkpoint_commit(tree, parent, &message)?;
        repo.sync_objects(&[sha])?;
        Ok(sha)
    })
    .await??;
    Ok((
        Capture {
            sha: sha.to_string(),
            unmerged_paths: snapshot.plan.unmerged_paths,
            lfs_directory: configuration.lfs_directory,
        },
        permit,
    ))
}

pub async fn save(
    workspace: Workspace,
    turn: Uuid,
    previous: Option<Checkpoint>,
    live_processes: bool,
    journal: Journal,
) -> Result<Checkpoint> {
    // The capture, final intent and immutable ref publication are one owned
    // task. A cancelled caller cannot leave an unprotected writer in flight.
    tokio::spawn(save_owned(
        workspace,
        turn,
        previous,
        live_processes,
        journal,
    ))
    .await?
}
async fn save_owned(
    workspace: Workspace,
    turn: Uuid,
    previous: Option<Checkpoint>,
    live_processes: bool,
    journal: Journal,
) -> Result<Checkpoint> {
    let id = Uuid::new_v5(&turn, b"checkpoint");
    let latest = format!("{}-checkpoint", workspace.reference);
    let (checkpoint, expected, lfs_directory, admission, _permit) = if let Some(operation) =
        journal.operation(id).await?
    {
        let payload = &operation["payload"];
        let checkpoint: Checkpoint = serde_json::from_value(payload["checkpoint"].clone())?;
        if operation["kind"] != "checkpoint.save"
            || payload["trackId"] != workspace.track_id
            || payload["latestRef"] != latest
            || checkpoint.turn_id != turn
            || checkpoint.reference != format!("{}-checkpoints/{turn}", workspace.reference)
        {
            return Err("The saved checkpoint belongs to a different turn or workspace.".into());
        }
        let expected: Option<String> = serde_json::from_value(payload["expected"].clone())?;
        let lfs_directory: std::path::PathBuf =
            serde_json::from_value(payload["lfsDirectory"].clone())?;
        let state = operation["state"]
            .as_str()
            .filter(|state| matches!(*state, "prepared" | "dispatched" | "complete"))
            .ok_or("The checkpoint operation cannot be resumed.")?;
        (
            checkpoint,
            expected,
            lfs_directory,
            Admission::Replay(state.into()),
            None,
        )
    } else {
        let parent = previous
            .as_ref()
            .map(|p| p.sha.as_str())
            .unwrap_or(&workspace.base);
        let (captured, permit) = capture_owned(
            &workspace,
            parent,
            &format!("Intercat checkpoint {turn}\n"),
            &journal,
        )
        .await?;
        let checkpoint = Checkpoint {
            turn_id: turn,
            sha: captured.sha,
            reference: format!("{}-checkpoints/{turn}", workspace.reference),
            unmerged_paths: captured.unmerged_paths,
            live_processes,
        };
        let lfs_directory = captured.lfs_directory;
        let expected = previous.as_ref().map(|p| p.sha.clone());
        let admission=journal.intent(&id.to_string(),"checkpoint.save",serde_json::json!({"trackId":workspace.track_id,"checkpoint":checkpoint,"latestRef":latest,"expected":expected,"lfsDirectory":lfs_directory})).await?;
        (checkpoint, expected, lfs_directory, admission, Some(permit))
    };
    // The durable intent freezes the tree, parent and ref targets. A retry must
    // finish that exact publication, even if files or process state have changed.
    let sha: intercat_git::ObjectId = checkpoint.sha.parse()?;
    let expected: Option<intercat_git::ObjectId> = expected.map(|sha| sha.parse()).transpose()?;
    let id = id.to_string();
    let reference = checkpoint.reference.clone();
    let path = workspace.path;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(path)?;
        crate::lfs_store::sync(&lfs_directory, &repo.lfs_entries(repo.tree_id(sha)?)?)?;
        repo.compare_and_swap(&reference, None, sha)?;
        repo.sync_ref(&reference)?;
        repo.compare_and_swap(&latest, expected, sha)?;
        repo.sync_ref(&latest)?;
        Ok(())
    })
    .await??;
    match admission {
        Admission::New => {
            journal.finish(&id, "dispatched").await?;
            journal.finish(&id, "complete").await?;
        }
        Admission::Replay(state) if state == "prepared" => {
            journal.finish(&id, "dispatched").await?;
            journal.finish(&id, "complete").await?;
        }
        Admission::Replay(state) if state == "dispatched" => {
            journal.finish(&id, "complete").await?
        }
        Admission::Replay(state) if state == "complete" => {}
        _ => return Err(std::io::Error::other("Checkpoint was superseded.").into()),
    }
    journal
        .checkpoint_scope(crate::checkpoint_scope::Change::Clear {
            track: workspace.track_id,
            turn,
        })
        .await?;
    Ok(checkpoint)
}
