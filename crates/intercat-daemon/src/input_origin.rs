//! A sent request keeps the identity of the turn that received it.
use crate::{Result, checkpoint::Checkpoint, journal::Journal, runtime::WorkerState};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub utterance_id: Uuid,
    pub session_id: String,
    pub turn_id: Uuid,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Running,
    Stopping,
    Saving,
    Finished,
    MovedOn,
    Unavailable,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Origin {
    pub identity: Identity,
    pub track_id: String,
    pub label: String,
    pub phase: Phase,
    pub checkpoint: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Changes {
    pub identity: Identity,
    pub label: String,
    pub checkpoint: String,
    pub files: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiffRequest {
    pub identity: Identity,
    pub checkpoint: String,
    pub path: String,
}

pub(crate) async fn current(journal: &Journal, identity: &Identity) -> Result<Origin> {
    journal
        .read_input_origin(identity.utterance_id)
        .await?
        .filter(|origin| origin.identity == *identity)
        .ok_or_else(|| "The original request turn is unavailable.".into())
}

async fn saved_changes(
    journal: &Journal,
    identity: &Identity,
) -> Result<(Origin, std::path::PathBuf, Vec<crate::integration::Change>)> {
    let origin = current(journal, identity).await?;
    let checkpoint = origin
        .checkpoint
        .clone()
        .ok_or("The original turn's changes are still being saved.")?;
    let root = journal.workspace(origin.track_id.clone()).await?.path;
    let read_root = root.clone();
    let changes = tokio::task::spawn_blocking(move || -> Result<_> {
        let repo = intercat_git::Repository::open(read_root)?;
        let saved = repo.commit_details(checkpoint.parse()?)?;
        if saved.parents.len() != 1 {
            return Err("The saved turn has no single parent.".into());
        }
        crate::integration::changes(&repo, repo.tree_id(saved.parents[0])?, saved.tree)
    })
    .await??;
    Ok((origin, root, changes))
}

pub async fn changes(journal: &Journal, identity: Identity) -> Result<Changes> {
    let (origin, _, changes) = saved_changes(journal, &identity).await?;
    Ok(Changes {
        identity,
        label: origin.label,
        checkpoint: origin.checkpoint.unwrap(),
        files: changes.into_iter().map(|change| change.path).collect(),
    })
}

pub async fn diff(journal: &Journal, request: DiffRequest) -> Result<crate::integration::Diff> {
    let (origin, root, changes) = saved_changes(journal, &request.identity).await?;
    if origin.checkpoint.as_deref() != Some(&request.checkpoint) {
        return Err("The saved turn's changes changed. Reopen them.".into());
    }
    let change = changes
        .into_iter()
        .find(|change| change.path == request.path)
        .ok_or("The file is not in the original turn's changes.")?;
    let mut diff =
        tokio::task::spawn_blocking(move || crate::integration::read_diff(root, change)).await??;
    let safe = journal
        .screen_start(serde_json::json!({"text":diff.before,"arguments":diff.after}))
        .await?;
    let before = safe.params["text"]
        .as_str()
        .ok_or("Missing screened file.")?;
    let after = safe.params["arguments"]
        .as_str()
        .ok_or("Missing screened file.")?;
    diff.redacted = before != diff.before || after != diff.after;
    diff.before = before.into();
    diff.after = after.into();
    let latest = current(journal, &request.identity).await?;
    if latest.checkpoint != origin.checkpoint {
        return Err("The saved turn's changes changed. Reopen them.".into());
    }
    Ok(diff)
}

pub(crate) fn read(db: &Connection, input: Uuid) -> Result<Option<Origin>> {
    crate::session_retirement::ensure_input(db, &input.to_string())?;
    let row: Option<(String,String,String,String,i64)> = db.query_row(
        "SELECT json_extract(u.decision,'$.trackId'),t.label,a.session,a.turn,a.ordinal
         FROM utterances u JOIN operations o ON o.id=u.dispatch_command AND o.kind='turn.start'
         JOIN attachment_turns a ON a.command=u.dispatch_command AND a.session=json_extract(o.payload,'$.sessionId') AND a.inherited=0
         JOIN tracks t ON t.id=json_extract(u.decision,'$.trackId')
         WHERE u.id=?1 AND u.state='landed' AND a.turn IS NOT NULL",
        [input.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    ).optional()?;
    let Some((track, label, session, turn, ordinal)) = row else {
        return Ok(None);
    };
    let turn: Uuid = turn.parse()?;
    let terminal: bool=db.query_row("SELECT coalesce(json_extract(record,'$.terminal'),0) FROM timeline_turns WHERE session=?1 AND turn=?2",
        params![session,turn.to_string()],|row|row.get(0)).optional()?.unwrap_or(false);
    let checkpoint: Option<String>=db.query_row("SELECT json_extract(payload,'$.checkpoint') FROM operations
        WHERE id=?1 AND kind='checkpoint.save' AND state='complete' AND json_extract(payload,'$.trackId')=?2",
        params![Uuid::new_v5(&turn,b"checkpoint").to_string(),track],|row|row.get(0)).optional()?;
    let checkpoint = checkpoint
        .map(|raw| serde_json::from_str::<Checkpoint>(&raw))
        .transpose()?;
    if checkpoint
        .as_ref()
        .is_some_and(|checkpoint| checkpoint.turn_id != turn)
    {
        return Err("The saved changes belong to another turn.".into());
    }
    let worker: Option<String> = db
        .query_row(
            "SELECT record FROM workers WHERE track_id=?1",
            [&track],
            |row| row.get(0),
        )
        .optional()?;
    let worker = worker
        .map(|raw| serde_json::from_str::<WorkerState>(&raw))
        .transpose()?;
    let later: bool=db.query_row("SELECT EXISTS(SELECT 1 FROM attachment_turns WHERE session=?1 AND ordinal>?2 AND turn IS NOT NULL AND inherited=0)",
        params![session,ordinal],|row|row.get(0))?;
    let moved_on = later
        || worker.as_ref().is_some_and(|worker| {
            (worker.active_turn.is_some()
                && (worker.session_id.as_deref() != Some(&session)
                    || worker.active_turn != Some(turn)))
                || worker
                    .checkpoint
                    .as_ref()
                    .is_some_and(|checkpoint| checkpoint.turn_id != turn)
        });
    let running = !terminal
        && checkpoint.is_none()
        && worker.as_ref().is_some_and(|worker| {
            worker.session_id.as_deref() == Some(&session) && worker.active_turn == Some(turn)
        });
    let phase = if moved_on {
        Phase::MovedOn
    } else if running
        && worker
            .as_ref()
            .is_some_and(|worker| worker.phase == "stopping")
    {
        Phase::Stopping
    } else if running {
        Phase::Running
    } else if checkpoint.is_some() {
        Phase::Finished
    } else if terminal {
        Phase::Saving
    } else {
        Phase::Unavailable
    };
    Ok(Some(Origin {
        identity: Identity {
            utterance_id: input,
            session_id: session,
            turn_id: turn,
        },
        track_id: track,
        label,
        phase,
        checkpoint: checkpoint.map(|checkpoint| checkpoint.sha),
    }))
}
