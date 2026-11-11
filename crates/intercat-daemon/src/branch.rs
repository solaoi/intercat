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
    pub point: crate::branch_start::Selection,
    pub instructions: InstructionMode,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum InstructionMode {
    Current,
    Checkpoint,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstructionSnapshot {
    pub reference: String,
    pub sha: String,
    pub changed: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Branch {
    pub request: Request,
    pub track_id: String,
    pub parent_session_id: String,
    pub session_id: Option<String>,
    pub workspace: Workspace,
    pub point: crate::branch_start::Point,
    pub phase: Phase,
    pub instructions: Option<InstructionSnapshot>,
    pub origin: crate::branch_points::Origin,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Preparing,
    AwaitingReceipt,
    Ready,
    Cancelled,
}
pub enum Change {
    Prepare(Request),
    Instructions {
        operation: Uuid,
        snapshot: InstructionSnapshot,
    },
    WorkspaceReady {
        operation: Uuid,
        roots: Vec<String>,
    },
    Received {
        operation: Uuid,
        session: String,
    },
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
    let mut query = db.prepare("SELECT record FROM branches WHERE json_extract(record,'$.phase')!='cancelled' ORDER BY rowid")?;
    query
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|s| serde_json::from_str(&s?).map_err(Into::into))
        .collect()
}

pub(crate) fn initial_instructions(
    db: &Connection,
    track: &str,
) -> Result<Option<InstructionSnapshot>> {
    let raw = db.query_row(
        "SELECT record FROM branches WHERE json_extract(record,'$.trackId')=?1 AND json_extract(record,'$.phase')='ready' LIMIT 1",
        [track], |row| row.get::<_, String>(0)
    ).optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let branch: Branch = serde_json::from_str(&raw)?;
    let raw: String = db.query_row(
        "SELECT record FROM workers WHERE track_id=?1",
        [track],
        |row| row.get(0),
    )?;
    let worker: WorkerState = serde_json::from_str(&raw)?;
    let first = match (&branch.point, &worker.checkpoint) {
        (crate::branch_start::Point::Turn { checkpoint: base }, Some(current)) => {
            base.turn_id == current.turn_id
        }
        (crate::branch_start::Point::Initial { .. }, None) => true,
        _ => false,
    };
    if !first {
        return Ok(None);
    }
    Ok(Some(
        branch
            .instructions
            .ok_or("Branch instructions were not frozen.")?,
    ))
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
        Change::Instructions {
            operation,
            snapshot,
        } => {
            let mut branch = read(&tx, operation)?.ok_or("Unknown branch.")?;
            if branch.phase != Phase::Preparing
                || snapshot.reference != format!("{}-instructions", branch.workspace.reference)
            {
                return Err("Invalid branch instruction snapshot.".into());
            }
            intercat_git::ObjectId::from_hex(snapshot.sha.as_bytes())?;
            if branch
                .instructions
                .as_ref()
                .is_some_and(|old| old != &snapshot)
            {
                return Err("The branch instructions were already frozen.".into());
            }
            branch.instructions = Some(snapshot);
            save(&tx, &branch)?;
            branch
        }
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
pub(crate) struct Source {
    pub session_id: String,
    pub checkpoint: Checkpoint,
    pub workspace: Workspace,
}

pub(crate) fn source(db: &Connection, track: &str, turn: Uuid) -> Result<Source> {
    // A file recovery replaces the active session, not the saved branch point.
    // Resolve the requested turn's recorded owner instead of the active worker.
    let mut query=db.prepare("SELECT session FROM timeline_turns WHERE turn=?1 AND json_extract(record,'$.track')=?2 LIMIT 2")?;
    let sessions = query
        .query_map(params![turn.to_string(), track], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let [parent_session_id] = sessions.as_slice() else {
        return Err("This turn has no unambiguous recorded parent session.".into());
    };
    let parent_session_id = parent_session_id.clone();
    let checkpoint_id = Uuid::new_v5(&turn, b"checkpoint");
    let raw: String = db.query_row("SELECT payload FROM operations WHERE id=?1 AND kind='checkpoint.save' AND state='complete'",[checkpoint_id.to_string()],|r|r.get(0))?;
    let payload: Value = serde_json::from_str(&raw)?;
    let checkpoint: Checkpoint = serde_json::from_value(payload["checkpoint"].clone())?;
    if payload["trackId"] != track
        || checkpoint.turn_id != turn
        || !checkpoint.unmerged_paths.is_empty()
    {
        return Err("The branch needs this parent's saved, resolved checkpoint.".into());
    }
    let raw: String = db.query_row(
        "SELECT record FROM workspaces WHERE track_id=?1",
        [track],
        |r| r.get(0),
    )?;
    let parent: Workspace = serde_json::from_str(&raw)?;
    if parent.state != "ready" {
        return Err("The parent workspace is unavailable.".into());
    }
    let expected = format!("{}-checkpoints/{}", parent.reference, turn);
    if checkpoint.reference != expected {
        return Err("The checkpoint reference does not belong to this workspace.".into());
    }
    Ok(Source {
        session_id: parent_session_id,
        checkpoint,
        workspace: parent,
    })
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
    let (parent_session_id, point, parent) = crate::branch_start::resolve(tx, &request)?;
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
        denied_roots: Vec::new(),
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
        base: point.sha().to_owned(),
        state: "branching".into(),
        error: None,
    };
    tx.execute(
        "INSERT INTO tracks VALUES(?1,?2,?3,'waiting')",
        params![track_id, label, format!("Branch · {title}")],
    )?;
    if crate::folders::project(tx)?.is_some() {
        let folder = crate::folders::track(tx, &request.parent_track_id)?;
        crate::folders::bind(tx, &track_id, &folder.id())?;
    }
    tx.execute(
        "INSERT INTO workspaces VALUES(?1,?2)",
        params![track_id, serde_json::to_string(&workspace)?],
    )?;
    let ancestry = crate::lineage_store::point(tx, &request.parent_track_id, point.sha())?;
    let mut basis = ancestry.basis();
    basis.parents =
        std::collections::BTreeMap::from([(ancestry.track.clone(), ancestry.inherited())]);
    crate::lineage::include(&mut basis.inherits, &ancestry.track, &ancestry.inherited())?;
    crate::lineage_store::initialize(tx, &track_id, &basis)?;
    crate::attachment_history::freeze(
        tx,
        request.op_id,
        &parent_session_id,
        point
            .checkpoint()
            .map(|checkpoint| checkpoint.turn_id.to_string())
            .as_deref(),
    )?;
    let origin = if let Some(checkpoint) = point.checkpoint() {
        crate::branch_points::origin(
            tx,
            &request.parent_track_id,
            checkpoint.turn_id,
            Some(&parent_session_id),
        )?
    } else {
        crate::branch_points::Origin {
            ended_at: None,
            timeline_at: None,
            turn_number: 0,
        }
    };
    let branch = Branch {
        request,
        track_id,
        parent_session_id,
        session_id: None,
        workspace,
        point,
        phase: Phase::Preparing,
        instructions: None,
        origin,
    };
    save(tx, &branch)?;
    Ok(branch)
}
fn received(tx: &Transaction<'_>, operation: Uuid, session: &str) -> Result<Branch> {
    let mut branch = read(tx, operation)?.ok_or("Unknown branch.")?;
    if branch.phase == Phase::Cancelled {
        return Err("This branch preparation was cancelled.".into());
    }
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
    if branch.phase == Phase::Cancelled {
        return Err("This branch preparation was cancelled.".into());
    }
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
        checkpoint: branch.point.checkpoint().cloned(),
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

/// Retain the cancelled lineage record; no new writer or generic recovery is offered.
pub(crate) fn cancel(tx: &Transaction<'_>, operation: Uuid) -> Result<()> {
    let Some(mut branch) = read(tx, operation)? else {
        return Ok(());
    };
    if branch.phase == Phase::Cancelled {
        return Ok(());
    }
    if branch.session_id.is_some() {
        let catalog = crate::secret_store::catalog(tx)?;
        if catalog.sealed.as_ref() != Some(&catalog.current) {
            return Err("Confirm the saved fingerprints before completing cancellation.".into());
        }
    }
    if let Some(raw) = tx
        .query_row(
            "SELECT record FROM workers WHERE track_id=?1",
            [&branch.track_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        let mut worker: WorkerState = serde_json::from_str(&raw)?;
        if worker.active_turn.is_some()
            || worker.checkpoint_pending
            || worker
                .checkpoint
                .as_ref()
                .is_some_and(|point| point.sha != branch.point.sha())
        {
            return Err(
                "The paired branch has work of its own. Review it before cancelling preparation."
                    .into(),
            );
        }
        worker.phase = "stopped".into();
        tx.execute(
            "UPDATE workers SET record=?2 WHERE track_id=?1",
            params![branch.track_id, serde_json::to_string(&worker)?],
        )?;
    }
    orphan_cancelled_inputs(tx, &branch.track_id)?;
    branch.workspace.state = "cancelled".into();
    branch.phase = Phase::Cancelled;
    tx.execute(
        "UPDATE workspaces SET record=?2 WHERE track_id=?1",
        params![branch.track_id, serde_json::to_string(&branch.workspace)?],
    )?;
    tx.execute(
        "UPDATE tracks SET status='stopped' WHERE id=?1",
        [&branch.track_id],
    )?;
    save(tx, &branch)
}
pub(crate) fn cancelled_track(db: &Connection, track: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM branches WHERE json_extract(record,'$.trackId')=?1 AND json_extract(record,'$.phase')='cancelled')",[track],|row|row.get(0))?)
}

pub(crate) const CANCELLED_DESTINATION: &str =
    "The paired branch preparation was cancelled. Choose another destination.";

/// Preserve words that were still being read, including a stale locked draft.
pub(crate) fn orphan_cancelled_inputs(db: &Connection, track: &str) -> Result<()> {
    db.execute("UPDATE utterances SET state='orphaned',revision=revision+1,reason=?2 WHERE state IN ('staged','reading','placed','held','asked') AND (json_extract(decision,'$.trackId')=?1 OR (decision IS NULL AND (provisional=?1 OR json_extract(target,'$.trackId')=?1)))",params![track,CANCELLED_DESTINATION])?;
    Ok(())
}
