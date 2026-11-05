//! A secret retry is a new snapshot even when it resumes the same model turn.
//! Each request owns one immutable ref; cancelled requests cannot pin later edits.
use crate::{
    Result,
    checkpoint::{self, Checkpoint},
    journal::{Admission, Journal},
    preparation::Workspace,
};
use serde_json::json;
use uuid::Uuid;

pub async fn save(
    workspace: Workspace,
    request: Uuid,
    turn: Uuid,
    parent: Option<String>,
    journal: &Journal,
) -> Result<Checkpoint> {
    let id = Uuid::new_v5(&request, b"repair checkpoint");
    let reference = format!("{}-secrets/{request}", workspace.reference);
    let (checkpoint, admission) = if let Some(operation) = journal.operation(id).await? {
        let checkpoint: Checkpoint =
            serde_json::from_value(operation["payload"]["checkpoint"].clone())?;
        if operation["kind"] != "repair.checkpoint"
            || operation["payload"]["trackId"] != workspace.track_id
            || checkpoint.turn_id != turn
            || checkpoint.reference != reference
        {
            return Err("The saved checkpoint belongs to another repair request.".into());
        }
        let state = operation["state"]
            .as_str()
            .filter(|state| matches!(*state, "prepared" | "dispatched" | "complete"))
            .ok_or("The repair checkpoint was superseded.")?;
        (checkpoint, Admission::Replay(state.into()))
    } else {
        let captured = checkpoint::capture(
            &workspace,
            parent.as_deref().unwrap_or(&workspace.base),
            &format!("Intercat repair checkpoint {request}\n"),
        )
        .await?;
        let checkpoint = Checkpoint {
            turn_id: turn,
            sha: captured.sha,
            reference,
            unmerged_paths: captured.unmerged_paths,
            live_processes: false,
        };
        let admission = journal
            .intent(
                &id.to_string(),
                "repair.checkpoint",
                json!({"trackId":workspace.track_id,"checkpoint":checkpoint}),
            )
            .await?;
        (checkpoint, admission)
    };
    let sha = checkpoint.sha.parse()?;
    let reference = checkpoint.reference.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = intercat_git::Repository::open(workspace.path)?;
        repo.compare_and_swap(&reference, None, sha)?;
        repo.sync_ref(&reference)
    })
    .await??;
    let id = id.to_string();
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
        _ => return Err("The repair checkpoint was superseded.".into()),
    }
    Ok(checkpoint)
}
