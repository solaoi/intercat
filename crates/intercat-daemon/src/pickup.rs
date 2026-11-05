//! Durable file recovery. Old requests stay held; the handoff is a new input.
use crate::{Result, checkpoint::Checkpoint, preparation::Workspace, runtime::WorkerState};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub track_id: String,
    pub session_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelRequest {
    pub op_id: Uuid,
    pub recovery_id: Uuid,
    pub track_id: String,
    pub session_id: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Stopping,
    Capturing,
    Publishing,
    Summarizing,
    Starting,
    AwaitingReceipt,
    Ready,
    Cancelling,
    Cancelled,
}
impl Phase {
    pub fn active(self) -> bool {
        !matches!(self, Self::Ready | Self::Cancelled)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recovery {
    pub request: Request,
    pub phase: Phase,
    pub previous: WorkerState,
    pub workspace: Workspace,
    pub snapshot: Option<Checkpoint>,
    pub session_id: String,
    pub start_command: Uuid,
    pub stop_command: Uuid,
    pub context: Value,
}
impl Recovery {
    pub fn status(&self) -> Value {
        json!({"opId":self.request.op_id,"trackId":self.request.track_id,"sessionId":self.request.session_id,"phase":self.phase,"snapshotSha":self.snapshot.as_ref().map(|s|&s.sha)})
    }
}
pub enum Change {
    Prepare(Request),
    Stopped(Uuid),
    Captured(Uuid, Checkpoint),
    Published(Uuid),
    Summarized(Uuid, crate::handoff::Document),
    Received(Uuid),
    Activate(Uuid),
    Cancelled(Uuid),
}
pub fn read(db: &Connection, id: Uuid) -> Result<Option<Recovery>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(kind, payload)| {
        if kind != "worker.pickup" {
            return Err("Operation ID reused.".into());
        }
        Ok(serde_json::from_str(&payload)?)
    })
    .transpose()
}
pub fn snapshot(db: &Connection) -> Result<Vec<Recovery>> {
    let mut query =
        db.prepare("SELECT payload FROM operations WHERE kind='worker.pickup' ORDER BY rowid")?;
    query
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect()
}
fn save(tx: &Transaction<'_>, recovery: &Recovery) -> Result<()> {
    let payload = serde_json::to_string(recovery)?;
    tx.execute("INSERT INTO operations VALUES(?1,'worker.pickup',?2,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,state=excluded.state", params![recovery.request.op_id.to_string(),payload,if recovery.phase.active() {"prepared"}else{"complete"}])?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'worker.pickup',?2)",
        params![crate::journal::timestamp(), payload],
    )?;
    Ok(())
}
fn local_context(
    tx: &Transaction<'_>,
    request: &Request,
    previous: &WorkerState,
    key: &crate::secrets::Key,
) -> Result<Value> {
    let unfinished = previous.active_turn.map(|t| t.to_string());
    let item_bytes:i64=tx.query_row("SELECT coalesce(sum(length(i.record)),0) FROM timeline_items i JOIN timeline_clips c ON c.id=i.clip WHERE i.session=?1 AND (json_extract(i.record,'$.kind')='userMessage' OR (json_extract(i.record,'$.kind')='toolCall' AND json_extract(i.record,'$.status')<>'inProgress' AND json_extract(c.record,'$.turnId')=?2))",params![request.session_id,unfinished],|r|r.get(0))?;
    let clip_bytes:i64=tx.query_row("SELECT coalesce(sum(length(i.summary)),0) FROM timeline_items i WHERE i.clip IN (SELECT id FROM timeline_clips WHERE track=?1 AND json_extract(record,'$.sessionId')=?2 AND json_extract(record,'$.repairRunId') IS NULL ORDER BY ordinal DESC LIMIT 3)",params![request.track_id,request.session_id],|r|r.get(0))?;
    let question_bytes:i64=tx.query_row("SELECT coalesce(sum(length(record)),0) FROM report_questions WHERE track_id=?1 AND json_extract(record,'$.sessionId')=?2 AND state IN ('open','queued','held')",params![request.track_id,request.session_id],|r|r.get(0))?;
    if item_bytes
        + clip_bytes
        + question_bytes
        + previous.output.as_ref().map_or(0, |t| t.len() as i64)
        > crate::handoff::CONTEXT_BYTES as i64
    {
        return Err(
            "The local history exceeds the protected summary input limit. It remains held.".into(),
        );
    }
    let mut clips = Vec::new();
    let mut query = tx.prepare("SELECT id,record FROM timeline_clips WHERE track=?1 AND json_extract(record,'$.sessionId')=?2 AND json_extract(record,'$.repairRunId') IS NULL ORDER BY ordinal DESC LIMIT 3")?;
    for row in query.query_map(params![request.track_id, request.session_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (id, record) = row?;
        let mut items =
            tx.prepare("SELECT summary FROM timeline_items WHERE clip=?1 ORDER BY rowid")?;
        let items = items
            .query_map([&id], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str::<Value>(&r?)?))
            .collect::<Result<Vec<_>>>()?;
        clips.push(
            json!({"address":id,"clip":serde_json::from_str::<Value>(&record)?,"items":items}),
        );
    }
    clips.reverse();
    let mut query=tx.prepare("SELECT i.record,c.id,c.record FROM timeline_items i JOIN timeline_clips c ON c.id=i.clip WHERE i.session=?1 AND (json_extract(i.record,'$.kind')='userMessage' OR (json_extract(i.record,'$.kind')='toolCall' AND json_extract(i.record,'$.status')<>'inProgress' AND json_extract(c.record,'$.turnId')=?2)) ORDER BY i.rowid")?;
    let mut instructions = Vec::new();
    let mut completed_tools = Vec::new();
    for row in query.query_map(params![request.session_id, unfinished], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (item, address, clip) = row?;
        let item: Value = serde_json::from_str(&item)?;
        if item["kind"] == "userMessage" {
            instructions.push(json!({"address":address,"message":item}));
        } else if item["kind"] == "toolCall"
            && item["status"] != "inProgress"
            && previous.active_turn.is_some_and(|turn| {
                serde_json::from_str::<Value>(&clip)
                    .is_ok_and(|clip| clip["turnId"] == turn.to_string())
            })
        {
            completed_tools.push(json!({"address":address,"tool":item}));
        }
    }
    let title: String = tx.query_row(
        "SELECT title FROM tracks WHERE id=?1",
        [&request.track_id],
        |r| r.get(0),
    )?;
    let mut questions=tx.prepare("SELECT record FROM report_questions WHERE track_id=?1 AND json_extract(record,'$.sessionId')=?2 AND state IN ('open','queued','held') ORDER BY rowid")?;
    let questions = questions
        .query_map(params![request.track_id, request.session_id], |r| {
            r.get::<_, String>(0)
        })?
        .map(|r| Ok(serde_json::from_str::<Value>(&r?)?))
        .collect::<Result<Vec<_>>>()?;
    let mut value = json!({"purpose":title,"instructions":instructions,"lastClips":clips,"unfinishedTurn":previous.active_turn,"completedTools":completed_tools,"lastReport":previous.output,"unresolvedQuestions":questions,"unverified":"The previous turn did not finish. Inspect the current files before continuing. Unanswered questions must remain unresolved until the user answers them."});
    key.mask(crate::secret_store::read(
        tx,
        &request.track_id,
        &request.session_id,
    )?)?
    .redact_value(&mut value)?;
    if value.get("_intercatTruncated").is_some() {
        return Err(
            "A protected history field is too large to summarize without losing evidence.".into(),
        );
    }
    if serde_json::to_vec(&value)?.len() > crate::handoff::CONTEXT_BYTES {
        return Err(
            "The local history exceeds the protected summary input limit. It remains held.".into(),
        );
    }
    Ok(value)
}
fn prepare(tx: &Transaction<'_>, request: Request, key: &crate::secrets::Key) -> Result<Recovery> {
    if request.op_id.get_version_num() != 7 {
        return Err("Recovery needs a new operation ID.".into());
    }
    if let Some(old) = read(tx, request.op_id)? {
        if old.request != request {
            return Err("Recovery operation ID reused.".into());
        }
        return Ok(old);
    }
    if snapshot(tx)?
        .iter()
        .any(|r| r.request.track_id == request.track_id && r.phase.active())
    {
        return Err("This track already has a pending file recovery.".into());
    }
    let catalog = crate::secret_store::catalog(tx)?;
    if catalog.current.revision > 0 && catalog.sealed.as_ref() != Some(&catalog.current) {
        return Err("Confirm the saved fingerprints before file recovery.".into());
    }
    let (previous, workspace) = interrupted(tx, &request)?;
    let context = local_context(tx, &request, &previous, key)?;
    let recovery = reserve(tx, request, previous, workspace, context, Phase::Stopping)?;
    Ok(recovery)
}
fn interrupted(tx: &Transaction<'_>, request: &Request) -> Result<(WorkerState, Workspace)> {
    let mut previous: WorkerState = serde_json::from_str(&tx.query_row(
        "SELECT record FROM workers WHERE track_id=?1",
        [&request.track_id],
        |r| r.get::<_, String>(0),
    )?)?;
    if previous.session_id.as_deref() != Some(&request.session_id)
        || !["lost", "failed", "needs-recovery", "after-connection-loss"]
            .contains(&previous.phase.as_str())
    {
        return Err("This recovery no longer matches the interrupted worker.".into());
    }
    if previous.active_turn.is_none() && previous.phase == "after-connection-loss" {
        let turn:Option<String>=tx.query_row("SELECT json_extract(payload,'$.turnId') FROM operations WHERE kind='worker.stopLost' AND json_extract(payload,'$.trackId')=?1 AND json_extract(payload,'$.sessionId')=?2 AND json_extract(payload,'$.turnId') IS NOT NULL ORDER BY rowid DESC LIMIT 1",params![request.track_id,request.session_id],|r|r.get(0)).optional()?;
        previous.active_turn = turn.map(|t| Uuid::parse_str(&t)).transpose()?;
    }
    let workspace: Workspace = serde_json::from_str(&tx.query_row(
        "SELECT record FROM workspaces WHERE track_id=?1",
        [&request.track_id],
        |r| r.get::<_, String>(0),
    )?)?;
    if workspace.state != "ready" {
        return Err("The workspace is not available for file recovery.".into());
    }
    Ok((previous, workspace))
}
fn reserve(
    tx: &Transaction<'_>,
    request: Request,
    mut previous: WorkerState,
    mut workspace: Workspace,
    context: Value,
    phase: Phase,
) -> Result<Recovery> {
    previous.output = None;
    previous.detail = None;
    previous.approvals.clear();
    previous.prompts.clear();
    workspace.state = "recovering".into();
    tx.execute(
        "UPDATE workspaces SET record=?2 WHERE track_id=?1",
        params![request.track_id, serde_json::to_string(&workspace)?],
    )?;
    let recovery = Recovery {
        request,
        phase,
        previous,
        workspace,
        snapshot: None,
        session_id: Uuid::now_v7().to_string(),
        start_command: Uuid::now_v7(),
        stop_command: Uuid::now_v7(),
        context,
    };
    save(tx, &recovery)?;
    Ok(recovery)
}

fn owner(tx: &Transaction<'_>, recovery: &Recovery) -> Result<WorkerState> {
    let worker: WorkerState = serde_json::from_str(&tx.query_row(
        "SELECT record FROM workers WHERE track_id=?1",
        [&recovery.request.track_id],
        |row| row.get::<_, String>(0),
    )?)?;
    let workspace: Workspace = serde_json::from_str(&tx.query_row(
        "SELECT record FROM workspaces WHERE track_id=?1",
        [&recovery.request.track_id],
        |row| row.get::<_, String>(0),
    )?)?;
    if worker.session_id.as_deref() != Some(&recovery.request.session_id)
        || workspace.state != "recovering"
        || workspace.path != recovery.workspace.path
        || workspace.reference != recovery.workspace.reference
        || workspace.base != recovery.workspace.base
    {
        return Err("The interrupted worker or workspace changed during file recovery.".into());
    }
    Ok(worker)
}

/// Record cancellation before waiting for any host or orchestration lock.
pub fn request_cancel(db: &mut Connection, request: CancelRequest) -> Result<Recovery> {
    if request.op_id.get_version_num() != 7
        || request.recovery_id.get_version_num() != 7
        || request.op_id == request.recovery_id
    {
        return Err("Cancellation needs its own operation ID.".into());
    }
    let tx = db.transaction()?;
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, payload)) = old
        && (kind != "worker.pickup.cancel"
            || serde_json::from_str::<CancelRequest>(&payload)? != request)
    {
        return Err("Cancellation operation ID reused.".into());
    }
    let mut recovery = if let Some(recovery) = read(&tx, request.recovery_id)? {
        recovery
    } else {
        // A fast Cancel can reach the journal before Prepare. Reserve a
        // tombstone so that this recovery ID can never launch a host later.
        if snapshot(&tx)?
            .iter()
            .any(|r| r.request.track_id == request.track_id && r.phase.active())
        {
            return Err("This track already has a pending file recovery.".into());
        }
        let original = Request {
            op_id: request.recovery_id,
            track_id: request.track_id.clone(),
            session_id: request.session_id.clone(),
        };
        let (previous, workspace) = interrupted(&tx, &original)?;
        reserve(
            &tx,
            original,
            previous,
            workspace,
            Value::Null,
            Phase::Cancelling,
        )?
    };
    if recovery.request.track_id != request.track_id
        || recovery.request.session_id != request.session_id
    {
        return Err("Cancellation no longer matches the interrupted worker.".into());
    }
    if recovery.phase == Phase::Ready {
        return Err(
            "Recovery already continued in a new session. Use Stop here to stop that worker."
                .into(),
        );
    }
    if recovery.phase != Phase::Cancelled {
        owner(&tx, &recovery)?;
        recovery.phase = Phase::Cancelling;
        save(&tx, &recovery)?;
    }
    tx.execute(
        "INSERT INTO operations VALUES(?1,'worker.pickup.cancel',?2,?3) ON CONFLICT(id) DO NOTHING",
        params![
            request.op_id.to_string(),
            serde_json::to_string(&request)?,
            if recovery.phase == Phase::Cancelled {
                "complete"
            } else {
                "prepared"
            }
        ],
    )?;
    tx.commit()?;
    Ok(recovery)
}
pub fn change(db: &mut Connection, change: Change, key: &crate::secrets::Key) -> Result<Recovery> {
    let tx = db.transaction()?;
    if let Change::Prepare(request) = change {
        let recovery = prepare(&tx, request, key)?;
        tx.commit()?;
        return Ok(recovery);
    }
    let id = match &change {
        Change::Stopped(id)
        | Change::Captured(id, _)
        | Change::Published(id)
        | Change::Summarized(id, _)
        | Change::Received(id)
        | Change::Activate(id)
        | Change::Cancelled(id) => *id,
        Change::Prepare(_) => unreachable!(),
    };
    let mut recovery = read(&tx, id)?.ok_or("Unknown file recovery.")?;
    match change {
        Change::Cancelled(_) if recovery.phase == Phase::Cancelled => return Ok(recovery),
        Change::Cancelled(_) if recovery.phase == Phase::Cancelling => {
            let mut current = owner(&tx, &recovery)?;
            if current.phase != "after-connection-loss" {
                return Err("Confirm the old worker stopped before finishing cancellation.".into());
            }
            current.detail = Some(
                "File recovery cancelled. Current files and saved snapshots were kept.".into(),
            );
            tx.execute(
                "UPDATE workers SET record=?2 WHERE track_id=?1",
                params![recovery.request.track_id, serde_json::to_string(&current)?],
            )?;
            // Nothing may dispatch automatically after cancellation.
            tx.execute("UPDATE utterances SET state='held',revision=revision+1,reason='File recovery was cancelled. This request remains held.' WHERE (json_extract(decision,'$.trackId')=?1 OR provisional=?1) AND state IN ('staged','reading','placed','starting','sent')",[&recovery.request.track_id])?;
            recovery.workspace.state = "ready".into();
            tx.execute(
                "UPDATE workspaces SET record=?2 WHERE track_id=?1",
                params![
                    recovery.request.track_id,
                    serde_json::to_string(&recovery.workspace)?
                ],
            )?;
            tx.execute("UPDATE operations SET state='complete' WHERE kind='worker.pickup.cancel' AND json_extract(payload,'$.recoveryId')=?1",[id.to_string()])?;
            recovery.phase = Phase::Cancelled;
        }
        Change::Stopped(_) if recovery.phase == Phase::Stopping => {
            let current: WorkerState = serde_json::from_str(&tx.query_row(
                "SELECT record FROM workers WHERE track_id=?1",
                [&recovery.request.track_id],
                |r| r.get::<_, String>(0),
            )?)?;
            if current.phase != "after-connection-loss"
                || current.session_id.as_deref() != Some(&recovery.request.session_id)
            {
                return Err("Confirm the old worker stopped before capturing files.".into());
            }
            let mut context_state = recovery.previous.clone();
            context_state.output = current.output;
            recovery.context = local_context(&tx, &recovery.request, &context_state, key)?;
            recovery.previous.checkpoint = current.checkpoint;
            crate::attachment_history::freeze(&tx, id, &recovery.request.session_id, None)?;
            recovery.phase = Phase::Capturing;
        }
        Change::Captured(_, checkpoint) if recovery.phase == Phase::Capturing => {
            if checkpoint.reference != format!("{}-recovery/{}", recovery.workspace.reference, id)
                || checkpoint.live_processes
                || checkpoint.turn_id != Uuid::new_v5(&id, b"recovery-snapshot")
            {
                return Err("Invalid recovery snapshot.".into());
            }
            intercat_git::ObjectId::from_hex(checkpoint.sha.as_bytes())?;
            recovery.snapshot = Some(checkpoint);
            recovery.phase = Phase::Publishing;
        }
        Change::Published(_) if recovery.phase == Phase::Publishing => {
            recovery.phase = Phase::Summarizing
        }
        Change::Summarized(_, document) if recovery.phase == Phase::Summarizing => {
            recovery.context = crate::handoff::compact(&recovery.context, document)?;
            recovery.phase = Phase::Starting;
        }
        Change::Received(_) if recovery.phase == Phase::Starting => {
            let before = crate::secret_store::catalog(&tx)?;
            if before.current.revision > 0 && before.sealed.as_ref() != Some(&before.current) {
                return Err("Confirm the preceding fingerprints first.".into());
            }
            let occupied:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM msp_positions WHERE session=?1 UNION ALL SELECT 1 FROM workers WHERE json_extract(record,'$.sessionId')=?1 UNION ALL SELECT 1 FROM secret_fingerprints WHERE session=?1 UNION ALL SELECT 1 FROM repair_runs WHERE json_extract(record,'$.session')=?1 UNION ALL SELECT 1 FROM branches WHERE json_extract(record,'$.sessionId')=?1)",[&recovery.session_id],|r|r.get(0))?;
            if occupied {
                return Err("The recovery session is already owned.".into());
            }
            crate::attachment_history::inherit(&tx, id, &recovery.session_id)?;
            tx.execute("INSERT INTO secret_fingerprints SELECT ?1,track,name,byte_len,tag FROM secret_fingerprints WHERE session=?2 AND track=?3",params![recovery.session_id,recovery.request.session_id,recovery.request.track_id])?;
            crate::secret_store::advance(&tx, &before)?;
            recovery.phase = Phase::AwaitingReceipt;
        }
        Change::Activate(_) if recovery.phase == Phase::AwaitingReceipt => {
            let current: WorkerState = serde_json::from_str(&tx.query_row(
                "SELECT record FROM workers WHERE track_id=?1",
                [&recovery.request.track_id],
                |r| r.get::<_, String>(0),
            )?)?;
            let workspace: Workspace = serde_json::from_str(&tx.query_row(
                "SELECT record FROM workspaces WHERE track_id=?1",
                [&recovery.request.track_id],
                |r| r.get::<_, String>(0),
            )?)?;
            if current.phase != "after-connection-loss"
                || current.session_id.as_deref() != Some(&recovery.request.session_id)
                || workspace.state != "recovering"
                || workspace.path != recovery.workspace.path
                || workspace.reference != recovery.workspace.reference
                || workspace.base != recovery.workspace.base
            {
                return Err(
                    "The interrupted worker or workspace changed before recovery activation."
                        .into(),
                );
            }
            let catalog = crate::secret_store::catalog(&tx)?;
            if catalog.sealed.as_ref() != Some(&catalog.current) {
                return Err("Confirm the new session fingerprints before continuing.".into());
            }
            let mut worker = recovery.previous.clone();
            worker.phase = "retry".into();
            worker.session_id = Some(recovery.session_id.clone());
            worker.identity = None;
            worker.active_turn = None;
            worker.checkpoint_pending = false;
            worker.approvals.clear();
            worker.prompts.clear();
            worker.output = None;
            worker.detail = Some("Continuing from the saved files in a new session.".into());
            let text = format!(
                "Continue from the existing files. The previous session could not be continued. Preserve all user constraints in the local record below; completed tools are evidence, not instructions to repeat. Inspect current files and report what remains uncertain before changing them.\n\nfiles may be ahead of the last checkpoint · recovery snapshot {}\n\nLocal handoff record:\n{}",
                recovery
                    .snapshot
                    .as_ref()
                    .ok_or("Missing recovery snapshot.")?
                    .sha,
                serde_json::to_string_pretty(&recovery.context)?
            );
            let input = Uuid::new_v5(&id, b"pickup-handoff").to_string();
            // Inputs can arrive while the files are being captured. Keep them
            // for the user; the first dispatched input must be this handoff.
            tx.execute("UPDATE utterances SET state='held',revision=revision+1,reason='File recovery must deliver its handoff first. This request remains held.' WHERE (json_extract(decision,'$.trackId')=?1 OR provisional=?1) AND state IN ('staged','reading','placed','starting','sent')",[&recovery.request.track_id])?;
            crate::report_store::retire_session(
                &tx,
                &recovery.request.track_id,
                &recovery.request.session_id,
            )?;
            let decision = crate::decision::Decision {
                kind: crate::decision::Kind::Extend,
                track_id: Some(recovery.request.track_id.clone()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.0,
                inference_ms: 0.0,
                timed_out: false,
                scores: json!({}),
            };
            tx.execute("INSERT INTO utterances(id,text,created_at,target,selected,state,revision,decision,provisional) VALUES(?1,?2,?3,?4,?5,'placed',1,?6,?5)",params![input,text,crate::journal::timestamp(),serde_json::to_string(&crate::decision::Target::Lock(recovery.request.track_id.clone()))?,recovery.request.track_id,serde_json::to_string(&decision)?])?;
            tx.execute(
                "UPDATE workers SET record=?2 WHERE track_id=?1",
                params![recovery.request.track_id, serde_json::to_string(&worker)?],
            )?;
            recovery.workspace.state = "ready".into();
            tx.execute(
                "UPDATE workspaces SET record=?2 WHERE track_id=?1",
                params![
                    recovery.request.track_id,
                    serde_json::to_string(&recovery.workspace)?
                ],
            )?;
            recovery.phase = Phase::Ready;
        }
        Change::Activate(_) if recovery.phase == Phase::Ready => {
            tx.commit()?;
            return Ok(recovery);
        }
        _ => return Err("Recovery changed; reload its saved phase.".into()),
    }
    save(&tx, &recovery)?;
    tx.commit()?;
    Ok(recovery)
}

pub fn inherited_checkpoint(
    db: &Connection,
    track: &str,
    session: &str,
    checkpoint: Uuid,
) -> Result<bool> {
    if crate::session_retirement::resume(db, track)?.is_some_and(|(resume, _)| {
        resume.session_id == session && resume.checkpoint.turn_id == checkpoint
    }) {
        return Ok(true);
    }
    Ok(snapshot(db)?.iter().any(|r| {
        r.phase == Phase::Ready
            && r.request.track_id == track
            && r.session_id == session
            && r.previous
                .checkpoint
                .as_ref()
                .is_some_and(|c| c.turn_id == checkpoint)
    }))
}
