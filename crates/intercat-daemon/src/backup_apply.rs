//! An authenticated, restartable handoff between reviewed files and a new receipt.
use crate::{
    Result, backup_choices::Choice, backup_review::Plan, checkpoint::Checkpoint,
    fingerprint_receipt::Receipt, secrets::Key,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Capturing,
    AwaitingReceipt,
    Complete,
    Cancelled,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct State {
    pub op_id: Uuid,
    pub source: Receipt,
    pub target: Option<Receipt>,
    pub phase: Phase,
    pub acknowledge_unverified: bool,
    pub(crate) plan: Plan,
    pub reviews: BTreeMap<String, Uuid>,
    pub heads: BTreeMap<String, String>,
    pub checkpoint_refs: BTreeMap<String, Option<String>>,
    pub directories: BTreeMap<String, (u64, u64)>,
    pub snapshots: BTreeMap<String, Checkpoint>,
    seal: String,
}
impl State {
    fn tag(&self, key: &Key) -> Result<String> {
        let mut value = serde_json::to_value(self)?;
        value.as_object_mut().unwrap().remove("seal");
        let mut bytes = b"intercat.backup-apply.v1\0".to_vec();
        bytes.extend(serde_json::to_vec(&value)?);
        Ok(key.tag(&bytes).iter().map(|b| format!("{b:02x}")).collect())
    }
    pub fn accepts(&self, receipt: &Receipt) -> bool {
        receipt == &self.source || self.target.as_ref() == Some(receipt)
    }
    pub fn status(&self) -> Value {
        json!({"opId":self.op_id,"phase":self.phase,"acknowledgeUnverified":self.acknowledge_unverified})
    }
    pub fn view(&self) -> Value {
        json!({"historyPaused":self.phase!=Phase::Complete,"apply":self.status(),"publication":self.target.as_ref().map(|target|json!({"source":self.source,"target":target})),"sessions":[],"workspaces":[],"hosts":[],"missingProcessRecords":[],"pendingOperations":usize::from(!matches!(self.phase,Phase::Complete|Phase::Cancelled))})
    }
    pub fn recovery(&self) -> Value {
        json!({"sessions":[],"projects":[],"historyReviewRequired":true,"pendingApply":self.status()})
    }
}
fn eligible(plan: &Plan, source: &Receipt) -> Result<()> {
    if plan.fingerprints["projects"]
        .as_array()
        .is_some_and(|p| !p.is_empty())
    {
        return Err("Rebuild the saved project fingerprints before applying recovery.".into());
    }
    let binding = plan.binding(source)?;
    for session in plan.fingerprints["sessions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let choice = plan
            .choices
            .iter()
            .find(|c| c.request.session_id == session["sessionId"].as_str().unwrap())
            .ok_or("Choose recovery for every affected session first.")?;
        if choice.binding != binding {
            return Err("A recovery choice is stale. Review it again.".into());
        }
    }
    Ok(())
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE backup_applies(id TEXT PRIMARY KEY,active INTEGER NOT NULL,record TEXT NOT NULL); CREATE UNIQUE INDEX active_backup_apply ON backup_applies(active) WHERE active=1;")?;
    Ok(())
}
pub(crate) fn read(db: &Connection, id: Option<Uuid>, key: &Key) -> Result<Option<State>> {
    use subtle::ConstantTimeEq;
    let row: Option<(String, String)> = if let Some(id) = id {
        db.query_row(
            "SELECT id,record FROM backup_applies WHERE id=?1",
            [id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    } else {
        db.query_row(
            "SELECT id,record FROM backup_applies WHERE active=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
    };
    row.map(|(id, text)| {
        if text.len() > 8 * 1024 * 1024 {
            return Err("Recovery record exceeds its limit.".into());
        }
        let state: State = serde_json::from_str(&text)?;
        if state.op_id.to_string() != id
            || !bool::from(state.seal.as_bytes().ct_eq(state.tag(key)?.as_bytes()))
        {
            return Err("The saved backup recovery changed.".into());
        }
        Ok(state)
    })
    .transpose()
}
fn save(db: &Connection, mut state: State, key: &Key) -> Result<State> {
    state.seal = state.tag(key)?;
    let encoded = serde_json::to_string(&state)?;
    if encoded.len() > 8 * 1024 * 1024 {
        return Err("Recovery record exceeds its limit.".into());
    }
    db.execute("INSERT INTO backup_applies VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET active=excluded.active,record=excluded.record",params![state.op_id.to_string(),!matches!(state.phase,Phase::Complete|Phase::Cancelled),encoded])?;
    let phase = match state.phase {
        Phase::Capturing => "prepared",
        Phase::AwaitingReceipt => "dispatched",
        Phase::Complete | Phase::Cancelled => "complete",
    };
    db.execute("INSERT INTO operations VALUES(?1,'backup.apply',?2,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,state=excluded.state",params![state.op_id.to_string(),serde_json::to_string(&state.status())?,phase])?;
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'backup.apply',?2)",
        params![
            crate::journal::timestamp(),
            serde_json::to_string(&state.status())?
        ],
    )?;
    Ok(state)
}
pub(crate) enum Change {
    Begin {
        id: Uuid,
        source: Receipt,
        plan: Box<Plan>,
        reviews: BTreeMap<String, Uuid>,
        heads: BTreeMap<String, String>,
        checkpoint_refs: BTreeMap<String, Option<String>>,
        directories: BTreeMap<String, (u64, u64)>,
        acknowledge_unverified: bool,
    },
    Snapshot {
        id: Uuid,
        track: String,
        checkpoint: Checkpoint,
    },
    Commit(Uuid),
    Cancel(Uuid),
    Confirm {
        id: Uuid,
        receipt: Receipt,
    },
}
pub(crate) fn change(db: &mut Connection, change: Change, key: &Key) -> Result<State> {
    let tx = db.transaction()?;
    let result = match change {
        Change::Begin {
            id,
            source,
            plan,
            reviews,
            heads,
            checkpoint_refs,
            directories,
            acknowledge_unverified,
        } => {
            if id.get_version_num() != 7 {
                return Err("Invalid backup recovery operation.".into());
            }
            if let Some(state) = read(&tx, Some(id), key)? {
                if !state.accepts(&source) || state.acknowledge_unverified != acknowledge_unverified
                {
                    return Err("Backup recovery operation reused.".into());
                }
                return Ok(state);
            }
            if read(&tx, None, key)?.is_some() {
                return Err("Another backup recovery must finish first.".into());
            }
            if tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
                [id.to_string()],
                |r| r.get::<_, bool>(0),
            )? {
                return Err("Backup recovery operation reused.".into());
            }
            let current = crate::backup_review::plan(&tx, &source, key)?;
            if current.binding(&source)? != plan.binding(&source)?
                || current.choices != plan.choices
            {
                return Err("Recovery choices changed. Review again.".into());
            }
            eligible(&plan, &source)?;
            save(
                &tx,
                State {
                    op_id: id,
                    source,
                    target: None,
                    phase: Phase::Capturing,
                    acknowledge_unverified,
                    plan: *plan,
                    reviews,
                    heads,
                    checkpoint_refs,
                    directories,
                    snapshots: BTreeMap::new(),
                    seal: String::new(),
                },
                key,
            )?
        }
        Change::Snapshot {
            id,
            track,
            checkpoint,
        } => {
            let mut state = read(&tx, Some(id), key)?.ok_or("Backup recovery is unavailable.")?;
            if state.phase != Phase::Capturing {
                return Err("Backup recovery has already advanced.".into());
            }
            if !state
                .plan
                .choices
                .iter()
                .any(|c| c.track_id == track && c.request.choice == Choice::PickUp)
            {
                return Err("This track has no file pickup decision.".into());
            }
            if let Some(old) = state.snapshots.get(&track) {
                if serde_json::to_value(old)? != serde_json::to_value(&checkpoint)? {
                    return Err("The recovery snapshot changed.".into());
                }
                return Ok(state);
            }
            state.snapshots.insert(track, checkpoint);
            save(&tx, state, key)?
        }
        Change::Commit(id) => {
            let mut state = read(&tx, Some(id), key)?.ok_or("Backup recovery is unavailable.")?;
            if state.phase != Phase::Capturing {
                return Ok(state);
            }
            let current = crate::backup_review::plan(&tx, &state.source, key)?;
            if current.binding(&state.source)? != state.plan.binding(&state.source)?
                || current.choices != state.plan.choices
            {
                return Err("The recovery plan changed before applying it.".into());
            }
            eligible(&state.plan, &state.source)?;
            let before = crate::secret_store::catalog(&tx)?;
            for session in state.plan.fingerprints["sessions"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let old = session["sessionId"].as_str().unwrap();
                let track = session["trackId"].as_str().unwrap();
                let choice = state
                    .plan
                    .choices
                    .iter()
                    .find(|c| c.request.session_id == old)
                    .ok_or("Every affected session needs a recovery decision.")?;
                let worker:Option<String>=tx.query_row("SELECT record FROM workers WHERE track_id=?1 AND json_extract(record,'$.sessionId')=?2",params![track,old],|r|r.get(0)).optional()?;
                let mut next = worker
                    .map(|w| serde_json::from_str::<crate::runtime::WorkerState>(&w))
                    .transpose()?;
                let pickup = choice.request.choice == Choice::PickUp;
                if pickup {
                    let checkpoint = state
                        .snapshots
                        .get(track)
                        .ok_or("The file recovery snapshot is unavailable.")?
                        .clone();
                    let resume = crate::session_retirement::Resume {
                        track_id: track.into(),
                        session_id: Uuid::now_v7().to_string(),
                        start_command: Uuid::now_v7(),
                        checkpoint: checkpoint.clone(),
                    };
                    let owner = Uuid::new_v5(&id, format!("attachments/{old}").as_bytes());
                    crate::attachment_history::freeze(&tx, owner, old, None)?;
                    crate::attachment_history::inherit(&tx, owner, &resume.session_id)?;
                    tx.execute("INSERT INTO secret_fingerprints SELECT ?1,track,name,byte_len,tag FROM secret_fingerprints WHERE session=?2",params![resume.session_id,old])?;
                    tx.execute("INSERT INTO backup_resumes VALUES(?1,?2,0) ON CONFLICT(track) DO UPDATE SET record=excluded.record,opened=0",params![track,serde_json::to_string(&resume)?])?;
                    next.as_mut()
                        .ok_or("The selected worker is unavailable.")?
                        .checkpoint = Some(checkpoint);
                }
                crate::session_retirement::retire(&tx, old, track, id)?;
                if let Some(mut worker) = next {
                    worker.phase = "stopped".into();
                    worker.session_id = None;
                    worker.identity = None;
                    worker.active_turn = None;
                    worker.output = None;
                    worker.approvals.clear();
                    worker.prompts.clear();
                    worker.secret_revision = None;
                    worker.checkpoint_pending = false;
                    if !pickup {
                        worker.checkpoint = None;
                        tx.execute("DELETE FROM backup_resumes WHERE track=?1", [track])?;
                    }
                    worker.detail =
                        Some("Previous history was retired after backup recovery.".into());
                    tx.execute(
                        "UPDATE workers SET record=?2 WHERE track_id=?1",
                        params![track, serde_json::to_string(&worker)?],
                    )?;
                    tx.execute("UPDATE workspaces SET record=json_set(record,'$.state','ready','$.error',NULL) WHERE track_id=?1",[track])?;
                    tx.execute("UPDATE utterances SET state='held',revision=revision+1,reason='Review this request after backup recovery.' WHERE (provisional=?1 OR json_extract(decision,'$.trackId')=?1) AND state IN ('staged','reading','placed','starting','sent') AND coalesce(json_extract(decision,'$.kind'),'')<>'park'",[track])?;
                    if pickup {
                        let input =
                            Uuid::new_v5(&id, format!("handoff/{track}").as_bytes()).to_string();
                        let text = "This is a new session after restoring a backup. Continue from the reviewed current files and attached references. Previous session history, instructions and results are unavailable. Inspect the files and report what remains uncertain. Ask for a new instruction before making changes.";
                        let decision = crate::decision::Decision {
                            kind: crate::decision::Kind::Extend,
                            track_id: Some(track.into()),
                            boundary: Some("now".into()),
                            command: None,
                            confidence: 1.,
                            inference_ms: 0.,
                            timed_out: false,
                            scores: json!({}),
                        };
                        tx.execute("INSERT INTO utterances(id,text,created_at,target,selected,state,revision,decision,provisional) VALUES(?1,?2,?3,?4,?5,'held',1,?6,?5)",params![input,text,crate::journal::timestamp(),serde_json::to_string(&crate::decision::Target::Lock(track.into()))?,track,serde_json::to_string(&decision)?])?;
                    }
                }
            }
            state.target = Some({
                let catalog = crate::secret_store::advance_recovery(
                    &tx,
                    &before,
                    state.source.catalog.revision,
                )?;
                Receipt {
                    catalog: catalog.current,
                    proof: catalog.proof,
                }
            });
            state.phase = Phase::AwaitingReceipt;
            save(&tx, state, key)?
        }
        Change::Cancel(id) => {
            let mut state = read(&tx, Some(id), key)?.ok_or("Backup recovery is unavailable.")?;
            if state.phase == Phase::Cancelled {
                return Ok(state);
            }
            if state.phase != Phase::Capturing {
                return Err("Recovery can only be cancelled before history is retired.".into());
            }
            state.phase = Phase::Cancelled;
            save(&tx, state, key)?
        }
        Change::Confirm { id, receipt } => {
            let mut state = read(&tx, Some(id), key)?.ok_or("Backup recovery is unavailable.")?;
            if state.phase == Phase::Capturing || state.target.as_ref() != Some(&receipt) {
                return Err("Publish the prepared recovery receipt first.".into());
            }
            let catalog = crate::secret_store::catalog(&tx)?;
            if catalog.current != receipt.catalog || catalog.proof != receipt.proof {
                return Err("The recovered fingerprint catalog changed.".into());
            }
            if state.phase == Phase::Complete {
                return Ok(state);
            }
            tx.execute("UPDATE secret_catalog SET sealed_revision=revision,sealed_digest=digest WHERE singleton=1",[])?;
            tx.execute("DELETE FROM settings WHERE key='fingerprint-recovery'", [])?;
            for choice in &state.plan.choices {
                if choice.request.choice == Choice::PickUp {
                    let input =
                        Uuid::new_v5(&id, format!("handoff/{}", choice.track_id).as_bytes())
                            .to_string();
                    tx.execute(
                        "UPDATE utterances SET state='placed' WHERE id=?1 AND state='held'",
                        [input],
                    )?;
                }
            }
            state.phase = Phase::Complete;
            save(&tx, state, key)?
        }
    };
    tx.commit()?;
    Ok(result)
}

async fn verify_files(
    journal: &crate::journal::Journal,
    data: &std::path::Path,
    state: &State,
) -> Result<()> {
    for workspace in &state.plan.workspaces {
        let id = *state
            .reviews
            .get(&workspace.track_id)
            .ok_or("Compare every saved workspace before applying recovery.")?;
        let review = crate::backup_review::review_files(
            journal,
            data,
            &state.source,
            state.plan.clone(),
            id,
            &workspace.track_id,
        )
        .await?;
        if review.stale {
            return Err("Files or originals changed after review. Restore or review the changed files before continuing recovery.".into());
        }
        let choice = state.plan.choices.iter().find(|c| {
            c.track_id == workspace.track_id
                && Some(&c.request.session_id) == workspace.session_id.as_ref()
        });
        if choice.is_some_and(|c| c.request.choice == Choice::PickUp)
            && (!review.complete || !review.attachments.complete)
        {
            return Err("Pick up requires verified files and attachment originals.".into());
        }
        if (!review.complete || !review.attachments.complete)
            && !state.acknowledge_unverified
            && !choice.is_some_and(|c| c.request.acknowledge_unread)
        {
            return Err("Acknowledge unverified files before applying recovery.".into());
        }
    }
    verify_finish(journal, data, state).await
}
async fn verify_finish(
    journal: &crate::journal::Journal,
    data: &std::path::Path,
    state: &State,
) -> Result<()> {
    let plan = state.plan.clone();
    let path = data.to_path_buf();
    let heads = state.heads.clone();
    let directories = state.directories.clone();
    let checkpoint_refs = state.checkpoint_refs.clone();
    let preparing = state.phase == Phase::Capturing;
    let store = journal.attachment_store();
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        crate::backup_review::ensure_stopped(&path, &plan)?;
        for workspace in &plan.workspaces {
            let meta = std::fs::symlink_metadata(&workspace.path)?;
            if !meta.is_dir()
                || directories.get(&workspace.track_id) != Some(&(meta.dev(), meta.ino()))
                || workspace.path.canonicalize()? != workspace.path
            {
                return Err("The reviewed workspace directory changed.".into());
            }
            if heads.get(&workspace.track_id).map(String::as_str)
                != Some(
                    intercat_git::Repository::open(&workspace.path)?
                        .head_commit()?
                        .to_string()
                        .as_str(),
                )
            {
                return Err("The workspace HEAD changed during backup recovery.".into());
            }
            if preparing {
                let actual = intercat_git::Repository::open(&workspace.path)?
                    .reference_target(&format!("{}-checkpoint", workspace.reference))?
                    .map(|id| id.to_string());
                if checkpoint_refs.get(&workspace.track_id) != Some(&actual) {
                    return Err(
                        "The checkpoint reference changed during backup preparation.".into(),
                    );
                }
            }
            if plan
                .choices
                .iter()
                .any(|c| c.track_id == workspace.track_id && c.request.choice == Choice::PickUp)
            {
                for id in &workspace.attachment_ids {
                    let expected = plan
                        .originals
                        .iter()
                        .find(|s| s.attachment.id == *id)
                        .ok_or("The reference original is unavailable.")?;
                    if store.load(*id)?.0 != *expected {
                        return Err("The reference original changed during recovery.".into());
                    }
                }
            }
        }
        Ok(())
    })
    .await?
}
async fn verify_snapshots(state: &State) -> Result<()> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        for (track, snapshot) in &state.snapshots {
            let workspace = state
                .plan
                .workspaces
                .iter()
                .find(|w| &w.track_id == track)
                .ok_or("Recovery workspace is missing.")?;
            let repo = intercat_git::Repository::open(&workspace.path)?;
            if repo.reference_target(&snapshot.reference)? != Some(snapshot.sha.parse()?)
                || repo.tree_id(snapshot.sha.parse()?).is_err()
            {
                return Err("The saved backup recovery snapshot changed.".into());
            }
        }
        Ok(())
    })
    .await?
}
/// The DB transaction retires history first. While history is still paused,
/// publish the new checkpoint baseline using the expected refs saved before it.
async fn publish_checkpoints(state: &State) -> Result<()> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        for workspace in &state.plan.workspaces {
            let Some(choice) = state.plan.choices.iter().find(|choice| {
                choice.track_id == workspace.track_id
                    && workspace.session_id.as_deref() == Some(&choice.request.session_id)
                    && state
                        .plan
                        .owns_affected_session(&choice.request.session_id, &choice.track_id)
            }) else {
                continue;
            };
            let next = if choice.request.choice == Choice::PickUp {
                Some(
                    state
                        .snapshots
                        .get(&workspace.track_id)
                        .ok_or("Missing recovery snapshot.")?
                        .sha
                        .parse()?,
                )
            } else {
                None
            };
            let expected = state
                .checkpoint_refs
                .get(&workspace.track_id)
                .ok_or("Missing checkpoint reference receipt.")?
                .as_ref()
                .map(|s| s.parse())
                .transpose()?;
            let repo = intercat_git::Repository::open(&workspace.path)?;
            let reference = format!("{}-checkpoint", workspace.reference);
            let actual = repo.reference_target(&reference)?;
            if actual != next && actual != expected {
                return Err("The checkpoint reference changed during backup recovery.".into());
            }
            match next {
                Some(next) => {
                    repo.compare_and_swap(&reference, expected, next)?;
                    repo.sync_ref(&reference)?;
                }
                None => {
                    if let Some(expected) = expected {
                        repo.delete_ref(&reference, expected)?;
                    }
                }
            }
        }
        Ok(())
    })
    .await?
}
pub(crate) async fn apply(
    journal: &crate::journal::Journal,
    data: &std::path::Path,
    receipt: Receipt,
    id: Uuid,
    acknowledge_unverified: bool,
) -> Result<Value> {
    let mut state = if let Some(state) = journal.backup_apply(Some(id)).await? {
        if !state.accepts(&receipt) || state.acknowledge_unverified != acknowledge_unverified {
            return Err("Backup recovery operation reused.".into());
        }
        state
    } else {
        let plan = journal.backup_review(receipt.clone()).await?;
        eligible(&plan, &receipt)?;
        let mut reviews = BTreeMap::new();
        for workspace in &plan.workspaces {
            let choice = plan.choices.iter().find(|c| {
                c.track_id == workspace.track_id
                    && Some(&c.request.session_id) == workspace.session_id.as_ref()
            });
            let review = choice
                .and_then(|c| c.request.review_id)
                .or_else(|| plan.file_reviews.get(&workspace.track_id).copied())
                .ok_or("Compare every saved workspace before applying recovery.")?;
            reviews.insert(workspace.track_id.clone(), review);
        }
        let workspaces = plan.workspaces.clone();
        let (heads, directories, checkpoint_refs) =
            tokio::task::spawn_blocking(move || -> Result<_> {
                use std::os::unix::fs::MetadataExt;
                let mut heads = BTreeMap::new();
                let mut directories = BTreeMap::new();
                let mut checkpoint_refs = BTreeMap::new();
                for w in workspaces {
                    let meta = std::fs::symlink_metadata(&w.path)?;
                    directories.insert(w.track_id.clone(), (meta.dev(), meta.ino()));
                    let repo = intercat_git::Repository::open(w.path)?;
                    checkpoint_refs.insert(
                        w.track_id.clone(),
                        repo.reference_target(&format!("{}-checkpoint", w.reference))?
                            .map(|id| id.to_string()),
                    );
                    heads.insert(w.track_id, repo.head_commit()?.to_string());
                }
                Ok((heads, directories, checkpoint_refs))
            })
            .await??;
        let proposed = State {
            op_id: id,
            source: receipt.clone(),
            target: None,
            phase: Phase::Capturing,
            acknowledge_unverified,
            plan: plan.clone(),
            reviews: reviews.clone(),
            heads: heads.clone(),
            checkpoint_refs: checkpoint_refs.clone(),
            directories: directories.clone(),
            snapshots: BTreeMap::new(),
            seal: String::new(),
        };
        verify_files(journal, data, &proposed).await?;
        journal
            .change_backup_apply(Change::Begin {
                id,
                source: receipt,
                plan: Box::new(plan),
                reviews,
                heads,
                checkpoint_refs,
                directories,
                acknowledge_unverified,
            })
            .await?
    };
    if state.phase == Phase::Complete {
        return Ok(state.view());
    }
    if state.phase == Phase::Cancelled {
        return Err("This recovery preparation was cancelled. Choose a new operation.".into());
    }
    if state.phase == Phase::Capturing {
        verify_files(journal, data, &state).await?;
        let choices = state.plan.choices.clone();
        for choice in choices {
            if choice.request.choice != Choice::PickUp
                || !state
                    .plan
                    .owns_affected_session(&choice.request.session_id, &choice.track_id)
            {
                continue;
            }
            let workspace = state
                .plan
                .workspaces
                .iter()
                .find(|w| {
                    w.track_id == choice.track_id
                        && w.session_id.as_deref() == Some(&choice.request.session_id)
                })
                .ok_or("The pickup workspace is unavailable.")?
                .clone();
            if !state.snapshots.contains_key(&choice.track_id) {
                let source = crate::preparation::Workspace {
                    track_id: workspace.track_id.clone(),
                    path: workspace.path.clone(),
                    reference: workspace.reference.clone(),
                    base: workspace.base.clone(),
                    state: "ready".into(),
                    error: None,
                    attachment_roots: workspace.attachment_roots.clone(),
                };
                let capture = crate::checkpoint::capture(
                    &source,
                    &state.heads[&choice.track_id],
                    "Intercat backup recovery",
                )
                .await?;
                let snapshot = Checkpoint {
                    turn_id: Uuid::new_v5(&id, format!("snapshot/{}", choice.track_id).as_bytes()),
                    sha: capture.sha,
                    reference: format!("{}-backup/{id}", workspace.reference),
                    unmerged_paths: capture.unmerged_paths,
                    live_processes: false,
                };
                state = journal
                    .change_backup_apply(Change::Snapshot {
                        id,
                        track: choice.track_id.clone(),
                        checkpoint: snapshot,
                    })
                    .await?;
            }
            let snapshot = state.snapshots[&choice.track_id].clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                let repo = intercat_git::Repository::open(workspace.path)?;
                let id = snapshot.sha.parse()?;
                match repo.reference_target(&snapshot.reference)? {
                    None => repo.compare_and_swap(&snapshot.reference, None, id)?,
                    Some(old) if old == id => {}
                    Some(_) => return Err("The backup snapshot reference changed.".into()),
                };
                repo.sync_ref(&snapshot.reference)?;
                Ok(())
            })
            .await??;
        }
        verify_files(journal, data, &state).await?;
        verify_snapshots(&state).await?;
        state = journal.change_backup_apply(Change::Commit(id)).await?;
    }
    verify_finish(journal, data, &state).await?;
    verify_snapshots(&state).await?;
    publish_checkpoints(&state).await?;
    Ok(state.view())
}
pub(crate) async fn confirm(
    journal: &crate::journal::Journal,
    data: &std::path::Path,
    receipt: Receipt,
    id: Uuid,
) -> Result<Value> {
    let state = journal
        .backup_apply(Some(id))
        .await?
        .ok_or("The prepared recovery is unavailable.")?;
    if state.target.as_ref() != Some(&receipt) {
        return Err("The external recovery confirmation does not match.".into());
    }
    if state.phase != Phase::Complete {
        verify_finish(journal, data, &state).await?;
        verify_snapshots(&state).await?;
        publish_checkpoints(&state).await?;
    }
    Ok(journal
        .change_backup_apply(Change::Confirm { id, receipt })
        .await?
        .view())
}

pub(crate) async fn cancel(
    journal: &crate::journal::Journal,
    receipt: &Receipt,
    id: Uuid,
) -> Result<Value> {
    let state = journal
        .backup_apply(Some(id))
        .await?
        .ok_or("Backup recovery is unavailable.")?;
    if !state.accepts(receipt) {
        return Err("The recovery confirmation changed.".into());
    }
    Ok(journal
        .change_backup_apply(Change::Cancel(id))
        .await?
        .view())
}
