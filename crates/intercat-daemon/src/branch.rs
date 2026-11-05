//! A branch is held until its inherited secret fingerprints have an external receipt.
use crate::{Result, checkpoint::Checkpoint, preparation::Workspace, runtime::WorkerState};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub parent_track_id: String,
    pub turn_id: Uuid,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Branch {
    pub request: Request,
    pub track_id: String,
    pub parent_session_id: String,
    pub session_id: Option<String>,
    pub workspace: Workspace,
    pub checkpoint: Checkpoint,
    pub phase: Phase,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Preparing,
    AwaitingReceipt,
    Ready,
}
pub enum Change {
    Prepare(Request),
    WorkspaceReady { operation: Uuid, roots: Vec<String> },
    Received { operation: Uuid, session: String },
    Activate(Uuid),
}

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE branches(id TEXT PRIMARY KEY,record TEXT NOT NULL);")?;
    Ok(())
}
fn read(db: &Connection, id: Uuid) -> Result<Option<Branch>> {
    db.query_row(
        "SELECT record FROM branches WHERE id=?1",
        [id.to_string()],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|s| serde_json::from_str(&s).map_err(Into::into))
    .transpose()
}
pub fn snapshot(db: &Connection) -> Result<Vec<Branch>> {
    let mut query = db.prepare("SELECT record FROM branches ORDER BY rowid")?;
    query
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|s| serde_json::from_str(&s?).map_err(Into::into))
        .collect()
}
fn save(tx: &Transaction<'_>, branch: &Branch) -> Result<()> {
    let value = serde_json::to_string(branch)?;
    tx.execute(
        "INSERT INTO branches VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
        params![branch.request.op_id.to_string(), value],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'branch.state',?2)",
        params![crate::journal::timestamp(), value],
    )?;
    Ok(())
}
pub fn change(db: &mut Connection, change: Change) -> Result<Branch> {
    let tx = db.transaction()?;
    let branch = match change {
        Change::Prepare(request) => prepare(&tx, request)?,
        Change::WorkspaceReady { operation, roots } => workspace_ready(&tx, operation, roots)?,
        Change::Received { operation, session } => received(&tx, operation, &session)?,
        Change::Activate(operation) => activate(&tx, operation)?,
    };
    tx.commit()?;
    Ok(branch)
}
fn workspace_ready(tx: &Transaction<'_>, operation: Uuid, roots: Vec<String>) -> Result<Branch> {
    let mut branch = read(tx, operation)?.ok_or("Unknown branch.")?;
    if roots.is_empty() || branch.phase != Phase::Preparing {
        return Err("The branch workspace cannot acquire attachment directories.".into());
    }
    if !branch.workspace.attachment_roots.is_empty() && branch.workspace.attachment_roots != roots {
        return Err("The branch attachment directories changed.".into());
    }
    branch.workspace.attachment_roots = roots;
    tx.execute(
        "UPDATE workspaces SET record=?1 WHERE track_id=?2",
        params![serde_json::to_string(&branch.workspace)?, branch.track_id],
    )?;
    save(tx, &branch)?;
    Ok(branch)
}
fn prepare(tx: &Transaction<'_>, request: Request) -> Result<Branch> {
    if request.op_id.get_version_num() != 7 {
        return Err("A branch needs a new operation ID.".into());
    }
    if let Some(branch) = read(tx, request.op_id)? {
        if branch.request != request {
            return Err("Branch operation ID was reused.".into());
        }
        return Ok(branch);
    }
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
        [request.op_id.to_string()],
        |r| r.get::<_, bool>(0),
    )? {
        return Err("This operation ID already belongs to another action.".into());
    }
    let before = crate::secret_store::catalog(tx)?;
    if before.current.revision > 0 && before.sealed.as_ref() != Some(&before.current) {
        return Err("Confirm the saved fingerprints before branching.".into());
    }
    // A file recovery replaces the active session, not the saved branch point.
    // Resolve the requested turn's recorded owner instead of the active worker.
    let mut query=tx.prepare("SELECT session FROM timeline_turns WHERE turn=?1 AND json_extract(record,'$.track')=?2 LIMIT 2")?;
    let sessions = query
        .query_map(
            params![request.turn_id.to_string(), request.parent_track_id],
            |r| r.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let [parent_session_id] = sessions.as_slice() else {
        return Err("This turn has no unambiguous recorded parent session.".into());
    };
    let parent_session_id = parent_session_id.clone();
    let checkpoint_id = Uuid::new_v5(&request.turn_id, b"checkpoint");
    let raw: String = tx.query_row("SELECT payload FROM operations WHERE id=?1 AND kind='checkpoint.save' AND state='complete'",[checkpoint_id.to_string()],|r|r.get(0))?;
    let payload: Value = serde_json::from_str(&raw)?;
    let checkpoint: Checkpoint = serde_json::from_value(payload["checkpoint"].clone())?;
    if payload["trackId"] != request.parent_track_id
        || checkpoint.turn_id != request.turn_id
        || !checkpoint.unmerged_paths.is_empty()
    {
        return Err("The branch needs this parent's saved, resolved checkpoint.".into());
    }
    let raw: String = tx.query_row(
        "SELECT record FROM workspaces WHERE track_id=?1",
        [&request.parent_track_id],
        |r| r.get(0),
    )?;
    let parent: Workspace = serde_json::from_str(&raw)?;
    if parent.state != "ready" {
        return Err("The parent workspace is unavailable.".into());
    }
    let expected = format!("{}-checkpoints/{}", parent.reference, request.turn_id);
    if checkpoint.reference != expected {
        return Err("The checkpoint reference does not belong to this workspace.".into());
    }
    let parent_label: String = tx.query_row(
        "SELECT label FROM tracks WHERE id=?1",
        [&request.parent_track_id],
        |r| r.get(0),
    )?;
    let family = parent_label.trim_end_matches(|c: char| c.is_ascii_digit());
    if family.is_empty() || !family.bytes().all(|c| c.is_ascii_uppercase()) {
        return Err("Invalid parent track name.".into());
    }
    let mut number = 2;
    let label = loop {
        let candidate = format!("{family}{number}");
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tracks WHERE label=?1)",
            [&candidate],
            |r| r.get(0),
        )?;
        if !exists {
            break candidate;
        }
        number += 1;
    };
    let title: String = tx.query_row(
        "SELECT title FROM tracks WHERE id=?1",
        [&request.parent_track_id],
        |r| r.get(0),
    )?;
    let track_id = Uuid::new_v5(&request.op_id, b"branch-track").to_string();
    let workspace = Workspace {
        attachment_roots: Vec::new(),
        track_id: track_id.clone(),
        path: parent
            .path
            .parent()
            .ok_or("Invalid parent workspace path.")?
            .join(&track_id),
        reference: format!(
            "{}/{}",
            parent
                .reference
                .rsplit_once('/')
                .ok_or("Invalid parent reference.")?
                .0,
            label
        ),
        base: checkpoint.sha.clone(),
        state: "branching".into(),
        error: None,
    };
    tx.execute(
        "INSERT INTO tracks VALUES(?1,?2,?3,'waiting')",
        params![track_id, label, format!("Branch · {title}")],
    )?;
    tx.execute(
        "INSERT INTO workspaces VALUES(?1,?2)",
        params![track_id, serde_json::to_string(&workspace)?],
    )?;
    crate::attachment_history::freeze(
        tx,
        request.op_id,
        &parent_session_id,
        Some(&request.turn_id.to_string()),
    )?;
    let branch = Branch {
        request,
        track_id,
        parent_session_id,
        session_id: None,
        workspace,
        checkpoint,
        phase: Phase::Preparing,
    };
    save(tx, &branch)?;
    Ok(branch)
}
fn received(tx: &Transaction<'_>, operation: Uuid, session: &str) -> Result<Branch> {
    let mut branch = read(tx, operation)?.ok_or("Unknown branch.")?;
    Uuid::parse_str(session)?;
    if let Some(saved) = &branch.session_id {
        if saved != session {
            return Err("The fork returned a different child session.".into());
        }
        return Ok(branch);
    }
    if session == branch.parent_session_id {
        return Err("The fork reused its parent session.".into());
    }
    let occupied: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM msp_positions WHERE session=?1 UNION ALL SELECT 1 FROM workers WHERE json_extract(record,'$.sessionId')=?1 UNION ALL SELECT 1 FROM repair_runs WHERE json_extract(record,'$.session')=?1 UNION ALL SELECT 1 FROM branches WHERE json_extract(record,'$.sessionId')=?1 UNION ALL SELECT 1 FROM secret_fingerprints WHERE session=?1)",[session],|r|r.get(0))?;
    if occupied {
        return Err("The child session is already owned.".into());
    }
    let before = crate::secret_store::catalog(tx)?;
    if before.current.revision > 0 && before.sealed.as_ref() != Some(&before.current) {
        return Err("Confirm the preceding fingerprints first.".into());
    }
    crate::attachment_history::inherit(tx, operation, session)?;
    tx.execute("INSERT INTO secret_fingerprints SELECT ?1,?2,name,byte_len,tag FROM secret_fingerprints WHERE session=?3 AND track=?4",params![session,branch.track_id,branch.parent_session_id,branch.request.parent_track_id])?;
    crate::secret_store::advance(tx, &before)?;
    branch.session_id = Some(session.into());
    branch.phase = Phase::AwaitingReceipt;
    save(tx, &branch)?;
    Ok(branch)
}
fn activate(tx: &Transaction<'_>, operation: Uuid) -> Result<Branch> {
    let mut branch = read(tx, operation)?.ok_or("Unknown branch.")?;
    if branch.phase == Phase::Ready {
        return Ok(branch);
    }
    let session = branch
        .session_id
        .clone()
        .ok_or("The child session has not been created.")?;
    let catalog = crate::secret_store::catalog(tx)?;
    if catalog.sealed.as_ref() != Some(&catalog.current) {
        return Err(
            "Confirm the child's inherited fingerprints before reading its history.".into(),
        );
    }
    let worker = WorkerState {
        track_id: branch.track_id.clone(),
        phase: "stopped".into(),
        session_id: Some(session),
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: None,
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: Some(branch.checkpoint.clone()),
        prompts: vec![],
    };
    branch.workspace.state = "ready".into();
    branch.phase = Phase::Ready;
    tx.execute(
        "INSERT INTO workers VALUES(?1,?2)",
        params![branch.track_id, serde_json::to_string(&worker)?],
    )?;
    tx.execute(
        "UPDATE workspaces SET record=?2 WHERE track_id=?1",
        params![branch.track_id, serde_json::to_string(&branch.workspace)?],
    )?;
    save(tx, &branch)?;
    Ok(branch)
}
