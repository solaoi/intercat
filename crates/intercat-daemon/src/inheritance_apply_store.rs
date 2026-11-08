//! Durable per-worker file applications. A pending application excludes every
//! other input until its checkpoint and actor state have been saved.
use crate::{
    Result, checkpoint::Checkpoint, inheritance::Dependencies, lineage::Basis,
    preparation::Workspace,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Application {
    pub operation: Uuid,
    pub dependencies: Dependencies,
    pub revision: i64,
    pub request_digest: String,
    pub session: String,
    pub workspace: Workspace,
    pub previous: Option<Checkpoint>,
    pub before: String,
    pub after: String,
    pub before_basis: Basis,
    pub basis: Basis,
    pub checkpoint_turn: Uuid,
    pub expected_base: Option<String>,
}
pub(crate) struct Context {
    pub source: crate::inheritance::Source,
    pub basis: Basis,
    pub saved: Option<Application>,
    pub request_digest: String,
    pub origin: Option<crate::plan_files::Origin>,
}
pub(crate) enum Change {
    Begin(Box<Application>),
    Checkpoint {
        operation: Uuid,
        checkpoint: Checkpoint,
    },
    Complete(Uuid),
    HoldSource(Uuid),
    Failure {
        input: String,
        revision: i64,
        blocker: crate::inheritance_issue::Blocker,
    },
    RecoveryFailure {
        operation: Uuid,
        blocker: crate::inheritance_issue::Blocker,
    },
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE INDEX IF NOT EXISTS pending_inherited_files ON operations(json_extract(payload,'$.dependencies.track')) WHERE kind='workspace.apply-inheritance' AND state<>'complete';")?;
    Ok(())
}
pub(crate) fn interrupt(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let count=tx.execute("UPDATE utterances SET state='held',revision=revision+1,reason='Finish the saved file update before sending this request.' WHERE state='placed' AND id IN (SELECT json_extract(payload,'$.dependencies.input') FROM operations WHERE kind='workspace.apply-inheritance' AND state<>'complete')",[])?;
    if count > 0 {
        event(
            &tx,
            "inheritance.interrupted",
            serde_json::json!({"requests":count}),
        )?;
    }
    tx.commit()?;
    Ok(())
}
fn read(db: &Connection, id: Uuid) -> Result<Option<(Application, String)>> {
    let row:Option<(String,String)>=db.query_row("SELECT payload,state FROM operations WHERE id=?1 AND kind='workspace.apply-inheritance'",[id.to_string()],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    row.map(|(raw, state)| Ok((serde_json::from_str(&raw)?, state)))
        .transpose()
}
pub(crate) fn recovery(
    db: &Connection,
    id: Uuid,
    track: &str,
    session: &str,
) -> Result<Option<Application>> {
    let (app, state) = read(db, id)?.ok_or("The file update is unavailable.")?;
    if app.dependencies.track != track || app.session != session {
        return Err("This file update belongs to another worker session.".into());
    }
    if state == "complete" {
        return Ok(None);
    }
    let worker: serde_json::Value = serde_json::from_str(&db.query_row(
        "SELECT record FROM workers WHERE track_id=?1",
        [track],
        |row| row.get::<_, String>(0),
    )?)?;
    if worker["sessionId"] != session
        || !worker["activeTurn"].is_null()
        || worker["checkpointPending"] == true
    {
        return Err("Wait for this worker's checkpoint before finishing the file update.".into());
    }
    let workspace: Workspace = serde_json::from_str(&db.query_row(
        "SELECT record FROM workspaces WHERE track_id=?1",
        [track],
        |row| row.get::<_, String>(0),
    )?)?;
    if workspace.path != app.workspace.path
        || workspace.reference != app.workspace.reference
        || workspace.state != "ready"
    {
        return Err("The worker workspace changed before recovery.".into());
    }
    Ok(Some(app))
}
pub(crate) fn snapshot(db: &Connection) -> Result<Vec<serde_json::Value>> {
    let mut query=db.prepare("SELECT id,payload,state FROM operations WHERE kind='workspace.apply-inheritance' AND state<>'complete' ORDER BY rowid")?;
    let rows = query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(|(id,raw,state)|{
        let app:Application=serde_json::from_str(&raw)?;
        let source_state:Option<String>=db.query_row("SELECT state FROM utterances WHERE id=?1",[&app.dependencies.input],|row|row.get(0)).optional()?;
        Ok(serde_json::json!({"operationId":id,"trackId":app.dependencies.track,"sourceInputId":app.dependencies.input,"sourceState":source_state,"state":state}))
    }).collect()
}
pub(crate) fn pending(db: &Connection, track: &str) -> Result<Option<Application>> {
    let raw:Option<String>=db.query_row("SELECT payload FROM operations WHERE kind='workspace.apply-inheritance' AND state<>'complete' AND json_extract(payload,'$.dependencies.track')=?1 ORDER BY rowid LIMIT 1",[track],|row|row.get(0)).optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}
pub(crate) fn available(db: &Connection, track: &str, input: &str) -> Result<bool> {
    Ok(pending(db, track)?.is_none_or(|saved| saved.dependencies.input == input))
}
pub(crate) fn context(db: &Connection, input: &str) -> Result<Option<Context>> {
    let origin = crate::plan_files::pending(db, input)?;
    let source = match crate::inheritance::source(db, input)? {
        Some(source) => Some(source),
        None => origin
            .as_ref()
            .map(|origin| {
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(crate::inheritance::Source {
                    dependencies: crate::inheritance::Dependencies {
                        input: input.into(),
                        track: origin.track.clone(),
                        points: vec![],
                    },
                    graph: crate::lineage_store::freeze(db)?,
                })
            })
            .transpose()?,
    };
    let Some(source) = source else {
        return Ok(None);
    };
    let saved = read(db, Uuid::new_v5(&input.parse()?, b"apply-inherited-files"))?;
    if saved.as_ref().is_some_and(|(_, state)| state == "complete")
        || (saved.is_none()
            && origin.is_none()
            && crate::inheritance::waiting(db, input)?.is_none())
    {
        return Ok(None);
    }
    let basis = crate::lineage_store::basis(db, &source.dependencies.track)?;
    Ok(Some(Context {
        source,
        basis,
        saved: saved.map(|(saved, _)| saved),
        request_digest: request_digest(db, input)?,
        origin,
    }))
}
fn request_digest(db: &Connection, input: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    let (text, target, track): (String, String, Option<String>) = db.query_row(
        "SELECT text,target,json_extract(decision,'$.trackId') FROM utterances WHERE id=?1",
        [input],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let mut query =
        db.prepare("SELECT attachment FROM input_attachments WHERE input=?1 ORDER BY ordinal")?;
    let attachments = query
        .query_map([input], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(
        Sha256::digest(serde_json::to_vec(&(text, target, track, attachments))?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}
fn event(db: &Connection, kind: &str, payload: serde_json::Value) -> Result<()> {
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,?2,?3)",
        params![crate::journal::timestamp(), kind, payload.to_string()],
    )?;
    Ok(())
}
pub(crate) fn change(db: &mut Connection, change: Change) -> Result<Option<Checkpoint>> {
    let tx = db.transaction()?;
    let result = match change {
        Change::RecoveryFailure { operation, blocker } => {
            let (app, state) = read(&tx, operation)?.ok_or("Missing inherited file operation.")?;
            if state != "complete" {
                crate::inheritance_issue::record(
                    &tx,
                    &app.dependencies.input,
                    &app.dependencies.track,
                    Some(operation),
                    blocker,
                )?;
            }
            None
        }
        Change::HoldSource(operation) => {
            let (app, state) = read(&tx, operation)?.ok_or("Missing inherited file operation.")?;
            if state != "complete" {
                let changed=tx.execute("UPDATE utterances SET state='held',revision=revision+1,reason='File recovery does not start this request.' WHERE id=?1 AND state IN ('staged','reading','placed')",[&app.dependencies.input])?;
                if changed > 0 {
                    event(
                        &tx,
                        "inheritance.source-held",
                        serde_json::json!({"operation":operation,"input":app.dependencies.input}),
                    )?;
                }
            }
            None
        }
        Change::Begin(app) => {
            if let Some((old, _)) = read(&tx, app.operation)? {
                if serde_json::to_value(old)? != serde_json::to_value(&app)? {
                    return Err("The inherited file operation changed.".into());
                }
            } else {
                if crate::dispatch_control::paused(&tx)?
                    || !crate::plan_execution::preparable(&tx, &app.dependencies.input)?
                    || !available(&tx, &app.dependencies.track, &app.dependencies.input)?
                {
                    return Err(crate::journal::StartSuperseded.into());
                }
                let current: (i64, String) = tx.query_row(
                    "SELECT revision,state FROM utterances WHERE id=?1",
                    [&app.dependencies.input],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if current != (app.revision, "placed".into())
                    || crate::lineage_store::basis(&tx, &app.dependencies.track)?
                        != app.before_basis
                    || request_digest(&tx, &app.dependencies.input)? != app.request_digest
                {
                    return Err(crate::journal::StartSuperseded.into());
                }
                let source = context(&tx, &app.dependencies.input)?
                    .ok_or("Missing inherited files.")?
                    .source;
                if source.dependencies != app.dependencies {
                    return Err(crate::journal::StartSuperseded.into());
                }
                let workspace: Workspace = serde_json::from_str(&tx.query_row(
                    "SELECT record FROM workspaces WHERE track_id=?1",
                    [&app.dependencies.track],
                    |row| row.get::<_, String>(0),
                )?)?;
                if workspace.path != app.workspace.path
                    || workspace.reference != app.workspace.reference
                    || workspace.state != "ready"
                {
                    return Err("The worker workspace changed before inheritance.".into());
                }
                let worker: serde_json::Value = serde_json::from_str(&tx.query_row(
                    "SELECT record FROM workers WHERE track_id=?1",
                    [&app.dependencies.track],
                    |row| row.get::<_, String>(0),
                )?)?;
                if worker["sessionId"] != app.session
                    || !worker["activeTurn"].is_null()
                    || worker["checkpointPending"] == true
                {
                    return Err(
                        "Wait for this worker's checkpoint before applying inherited files.".into(),
                    );
                }
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'workspace.apply-inheritance',?2,'prepared')",
                    params![app.operation.to_string(), serde_json::to_string(&app)?],
                )?;
                event(
                    &tx,
                    "inheritance.prepared",
                    serde_json::json!({"operation":app.operation,"trackId":app.dependencies.track,"input":app.dependencies.input}),
                )?;
            }
            None
        }
        Change::Checkpoint {
            operation,
            checkpoint,
        } => {
            let (app, state) = read(&tx, operation)?.ok_or("Missing inherited file operation.")?;
            if state != "dispatched" || checkpoint.turn_id != app.checkpoint_turn {
                return Err("The inherited checkpoint cannot be saved in this state.".into());
            }
            let id = Uuid::new_v5(&checkpoint.turn_id, b"checkpoint").to_string();
            if let Some(raw) = tx
                .query_row("SELECT payload FROM operations WHERE id=?1", [&id], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?
            {
                let value: serde_json::Value = serde_json::from_str(&raw)?;
                Some(serde_json::from_value(value["checkpoint"].clone())?)
            } else {
                if crate::lineage_store::basis(&tx, &app.dependencies.track)? != app.before_basis {
                    return Err(
                        "The worker base changed while inherited files were being applied.".into(),
                    );
                }
                crate::lineage_store::set_basis(&tx, &app.dependencies.track, &app.basis)?;
                let payload = serde_json::json!({"trackId":app.dependencies.track,"checkpoint":checkpoint,"latestRef":format!("{}-checkpoint",app.workspace.reference),"expected":app.previous.as_ref().map(|p|&p.sha)});
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'checkpoint.save',?2,'prepared')",
                    params![id, payload.to_string()],
                )?;
                crate::lineage_store::reserve(&tx, &id, &payload)?;
                event(
                    &tx,
                    "inheritance.checkpoint-prepared",
                    serde_json::json!({"operation":operation,"checkpoint":checkpoint.sha}),
                )?;
                Some(checkpoint)
            }
        }
        Change::Complete(operation) => {
            let (app, state) = read(&tx, operation)?.ok_or("Missing inherited file operation.")?;
            if state != "complete" {
                let checkpoint = Uuid::new_v5(&app.checkpoint_turn, b"checkpoint").to_string();
                let raw: String = tx.query_row(
                    "SELECT payload FROM operations WHERE id=?1 AND state='complete'",
                    [checkpoint],
                    |row| row.get(0),
                )?;
                let saved: serde_json::Value = serde_json::from_str(&raw)?;
                let worker: serde_json::Value = serde_json::from_str(&tx.query_row(
                    "SELECT record FROM workers WHERE track_id=?1",
                    [&app.dependencies.track],
                    |row| row.get::<_, String>(0),
                )?)?;
                if worker["checkpoint"]["sha"] != saved["checkpoint"]["sha"]
                    || !worker["activeTurn"].is_null()
                    || worker["sessionId"] != app.session
                {
                    return Err(
                        "Save the worker's inherited checkpoint before starting another turn."
                            .into(),
                    );
                }
                tx.execute(
                    "UPDATE operations SET state='complete' WHERE id=?1",
                    [operation.to_string()],
                )?;
                tx.execute("UPDATE operations SET state='complete' WHERE kind='inheritance.finish' AND json_extract(payload,'$.applicationId')=?1",[operation.to_string()])?;
                crate::inheritance_issue::clear(&tx, &app.dependencies.input)?;
                crate::plan_files::applied(&tx, &app)?;
                event(
                    &tx,
                    "inheritance.complete",
                    serde_json::json!({"operation":operation,"trackId":app.dependencies.track}),
                )?;
            }
            None
        }
        Change::Failure {
            input,
            revision,
            blocker,
        } => {
            if tx.execute("UPDATE utterances SET state='held',revision=revision+1,reason='Inherited files need review' WHERE id=?1 AND revision=?2 AND state='placed'",params![input,revision])?==0 {
                return Ok(None);
            }
            let reason = blocker.to_string();
            let track: String = tx.query_row(
                "SELECT json_extract(decision,'$.trackId') FROM utterances WHERE id=?1",
                [&input],
                |row| row.get(0),
            )?;
            let application = pending(&tx, &track)?.map(|app| app.operation);
            crate::inheritance_issue::record(&tx, &input, &track, application, blocker)?;
            event(
                &tx,
                "inheritance.held",
                serde_json::json!({"input":input,"reason":reason}),
            )?;
            if let Some((plan, item)) = crate::plan_execution::owner(&tx, &input)? {
                let mut plan = crate::plan_store::read(&tx, plan)?;
                if let Some(item) = plan
                    .items
                    .iter_mut()
                    .find(|entry| entry.plan_item_id == item)
                {
                    item.detail = Some(reason);
                }
                crate::plan_store::save(&tx, &plan, "plan.inheritance-held")?;
            }
            None
        }
    };
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        decision::{Decision, Kind, Outcome, Target},
        journal::Journal,
        runtime::WorkerState,
        secrets::Key,
    };
    async fn fixture() -> Result<(tempfile::TempDir, Journal, Application, WorkerState)> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("db");
        let journal = Journal::open(&path, Key::new([7; 32])).await?;
        let input = Uuid::now_v7();
        journal
            .stage_utterance(
                input,
                "Dummy inherited request".into(),
                Target::New,
                None,
                vec![],
            )
            .await?;
        let claim = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                claim.id,
                claim.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Fork,
                    track_id: None,
                    boundary: None,
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: serde_json::json!({}),
                }),
            )
            .await?;
        let word = journal.read_utterance(input.to_string()).await?;
        let track = word["decision"]["trackId"].as_str().unwrap().to_owned();
        let workspace = Workspace {
            track_id: track.clone(),
            path: root.path().join("worker"),
            reference: "refs/intercat/test/tracks/A".into(),
            base: "0".repeat(40),
            state: "ready".into(),
            error: None,
            attachment_roots: vec![],
        };
        journal.workspace_result(workspace.clone()).await?;
        let worker = WorkerState {
            track_id: track.clone(),
            phase: "starting".into(),
            session_id: Some("session".into()),
            secret_revision: None,
            identity: None,
            detail: None,
            active_turn: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        };
        journal.worker_state(worker.clone()).await?;
        let parent = crate::lineage::Point {
            track: "parent".into(),
            number: 1,
            sha: "1".repeat(40),
            base_at: "0".repeat(40),
            base_inherits_at: Default::default(),
            inherits: Default::default(),
        };
        let dependencies = Dependencies {
            input: input.to_string(),
            track: track.clone(),
            points: vec![parent.clone()],
        };
        let db = Connection::open(&path)?;
        crate::inheritance::queue(&db, &dependencies)?;
        let before_basis = Basis::root("0".repeat(40));
        let mut basis = before_basis.clone();
        basis
            .inherits
            .insert(parent.track.clone(), parent.inherited());
        let app = Application {
            operation: Uuid::new_v5(&input, b"apply-inherited-files"),
            dependencies,
            revision: word["revision"].as_i64().unwrap(),
            request_digest: request_digest(&db, &input.to_string())?,
            session: "session".into(),
            workspace,
            previous: None,
            before: "2".repeat(40),
            after: "3".repeat(40),
            before_basis,
            basis,
            checkpoint_turn: Uuid::new_v5(&input, b"inherited-checkpoint"),
            expected_base: None,
        };
        Ok((root, journal, app, worker))
    }
    #[tokio::test]
    async fn a_pending_application_excludes_other_inputs_until_the_actor_checkpoint_is_saved()
    -> Result<()> {
        let (root, journal, app, mut worker) = fixture().await?;
        journal
            .change_inheritance(Change::Begin(Box::new(app.clone())))
            .await?;
        journal
            .change_inheritance(Change::Begin(Box::new(app.clone())))
            .await?;
        let db = Connection::open(root.path().join("db"))?;
        assert!(!available(&db, &app.dependencies.track, "another-input")?);
        assert!(available(
            &db,
            &app.dependencies.track,
            &app.dependencies.input
        )?);
        journal
            .finish(&app.operation.to_string(), "dispatched")
            .await?;
        let checkpoint = Checkpoint {
            turn_id: app.checkpoint_turn,
            sha: "a".repeat(40),
            reference: format!(
                "{}-checkpoints/{}",
                app.workspace.reference, app.checkpoint_turn
            ),
            unmerged_paths: vec![],
            live_processes: false,
        };
        journal
            .change_inheritance(Change::Checkpoint {
                operation: app.operation,
                checkpoint: checkpoint.clone(),
            })
            .await?;
        let id = Uuid::new_v5(&app.checkpoint_turn, b"checkpoint");
        journal.finish(&id.to_string(), "dispatched").await?;
        journal.finish(&id.to_string(), "complete").await?;
        assert!(
            journal
                .change_inheritance(Change::Complete(app.operation))
                .await
                .is_err()
        );
        worker.checkpoint = Some(checkpoint);
        journal.worker_state(worker).await?;
        journal
            .change_inheritance(Change::Complete(app.operation))
            .await?;
        assert!(pending(&db, &app.dependencies.track)?.is_none());
        assert!(crate::inheritance::waiting(&db, &app.dependencies.input)?.is_none());
        assert_eq!(
            crate::lineage_store::basis(&db, &app.dependencies.track)?,
            app.basis
        );
        Ok(())
    }
    #[tokio::test]
    async fn stop_or_changed_base_rejects_preparation_before_it_records_an_effect() -> Result<()> {
        let (root, journal, app, _worker) = fixture().await?;
        journal.pause_dispatch(Uuid::now_v7(), true).await?;
        assert!(
            journal
                .change_inheritance(Change::Begin(Box::new(app.clone())))
                .await
                .is_err()
        );
        assert!(journal.operation(app.operation).await?.is_none());
        journal.pause_dispatch(Uuid::now_v7(), false).await?;
        let db = Connection::open(root.path().join("db"))?;
        crate::lineage_store::set_basis(
            &db,
            &app.dependencies.track,
            &Basis::root("f".repeat(40)),
        )?;
        assert!(
            journal
                .change_inheritance(Change::Begin(Box::new(app.clone())))
                .await
                .is_err()
        );
        assert!(journal.operation(app.operation).await?.is_none());
        assert!(available(&db, &app.dependencies.track, "another-input")?);
        Ok(())
    }
    #[tokio::test]
    async fn an_active_turn_and_an_unsaved_checkpoint_each_block_inheritance() -> Result<()> {
        let (_root, journal, app, mut worker) = fixture().await?;
        worker.active_turn = Some(Uuid::now_v7());
        journal.worker_state(worker.clone()).await?;
        assert!(
            journal
                .change_inheritance(Change::Begin(Box::new(app.clone())))
                .await
                .is_err()
        );
        assert!(journal.operation(app.operation).await?.is_none());
        worker.active_turn = None;
        worker.checkpoint_pending = true;
        journal.worker_state(worker.clone()).await?;
        assert!(
            journal
                .change_inheritance(Change::Begin(Box::new(app.clone())))
                .await
                .is_err()
        );
        assert!(journal.operation(app.operation).await?.is_none());
        worker.checkpoint_pending = false;
        journal.worker_state(worker).await?;
        journal
            .change_inheritance(Change::Begin(Box::new(app.clone())))
            .await?;
        assert_eq!(
            journal.operation(app.operation).await?.unwrap()["state"],
            "prepared"
        );
        Ok(())
    }
    #[tokio::test]
    async fn an_old_failure_cannot_hold_a_newer_request() -> Result<()> {
        let (root, journal, app, _worker) = fixture().await?;
        let db = Connection::open(root.path().join("db"))?;
        db.execute(
            "UPDATE utterances SET text='New dummy request',revision=revision+1 WHERE id=?1",
            [&app.dependencies.input],
        )?;
        journal
            .change_inheritance(Change::Failure {
                input: app.dependencies.input.clone(),
                revision: app.revision,
                blocker: crate::inheritance_issue::Blocker::Other {
                    message: "Old dummy failure".into(),
                },
            })
            .await?;
        let current = journal.read_utterance(app.dependencies.input).await?;
        assert_eq!(current["state"], "placed");
        assert_eq!(current["text"], "New dummy request");
        assert_eq!(current["revision"], app.revision + 1);
        Ok(())
    }
    #[tokio::test]
    async fn recovery_is_bound_to_the_idle_worker_and_its_saved_workspace() -> Result<()> {
        let (root, journal, app, mut worker) = fixture().await?;
        journal
            .change_inheritance(Change::Begin(Box::new(app.clone())))
            .await?;
        assert!(
            journal
                .inheritance_recovery(app.operation, "another-track".into(), app.session.clone())
                .await
                .is_err()
        );
        assert!(
            journal
                .inheritance_recovery(
                    app.operation,
                    app.dependencies.track.clone(),
                    "new-session".into()
                )
                .await
                .is_err()
        );
        worker.active_turn = Some(Uuid::now_v7());
        journal.worker_state(worker.clone()).await?;
        assert!(
            journal
                .inheritance_recovery(
                    app.operation,
                    app.dependencies.track.clone(),
                    app.session.clone()
                )
                .await
                .is_err()
        );
        worker.active_turn = None;
        journal.worker_state(worker).await?;
        assert!(
            journal
                .inheritance_recovery(
                    app.operation,
                    app.dependencies.track.clone(),
                    app.session.clone()
                )
                .await?
                .is_some()
        );
        let db = Connection::open(root.path().join("db"))?;
        let mut moved = app.workspace.clone();
        moved.path = root.path().join("different-worker");
        db.execute(
            "UPDATE workspaces SET record=?2 WHERE track_id=?1",
            params![app.dependencies.track, serde_json::to_string(&moved)?],
        )?;
        assert!(
            journal
                .inheritance_recovery(app.operation, app.dependencies.track, app.session)
                .await
                .is_err()
        );
        Ok(())
    }
    #[tokio::test]
    async fn reopening_holds_the_request_even_if_failure_was_never_saved() -> Result<()> {
        let (root, journal, app, _worker) = fixture().await?;
        journal
            .change_inheritance(Change::Begin(Box::new(app.clone())))
            .await?;
        drop(journal);
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let word = journal
            .read_utterance(app.dependencies.input.clone())
            .await?;
        assert_eq!(word["state"], "held");
        assert_eq!(word["revision"], app.revision + 1);
        assert_eq!(
            journal.snapshot().await?["pendingInheritance"][0]["sourceState"],
            "held"
        );
        journal.advance_plans().await?;
        assert_eq!(
            journal
                .read_utterance(app.dependencies.input.clone())
                .await?["state"],
            "held"
        );
        drop(journal);
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        assert_eq!(
            journal.read_utterance(app.dependencies.input).await?["revision"],
            app.revision + 1
        );
        Ok(())
    }
    #[tokio::test]
    async fn recovery_holds_its_source_even_after_the_request_moves_to_another_track() -> Result<()>
    {
        let (root, journal, app, _worker) = fixture().await?;
        journal
            .change_inheritance(Change::Begin(Box::new(app.clone())))
            .await?;
        let db = Connection::open(root.path().join("db"))?;
        db.execute(
            "INSERT INTO tracks VALUES('other-track','B','Other dummy track','waiting')",
            [],
        )?;
        db.execute("UPDATE utterances SET decision=json_set(decision,'$.trackId','other-track'),revision=revision+1 WHERE id=?1",[&app.dependencies.input])?;
        journal
            .change_inheritance(Change::HoldSource(app.operation))
            .await?;
        let word = journal.read_utterance(app.dependencies.input).await?;
        assert_eq!(word["state"], "held");
        assert_eq!(word["decision"]["trackId"], "other-track");
        Ok(())
    }
    #[tokio::test]
    async fn a_concurrent_base_change_is_never_overwritten_by_checkpoint_completion() -> Result<()>
    {
        let (root, journal, app, _worker) = fixture().await?;
        journal
            .change_inheritance(Change::Begin(Box::new(app.clone())))
            .await?;
        journal
            .finish(&app.operation.to_string(), "dispatched")
            .await?;
        let db = Connection::open(root.path().join("db"))?;
        let newer = Basis::root("f".repeat(40));
        crate::lineage_store::set_basis(&db, &app.dependencies.track, &newer)?;
        let checkpoint = Checkpoint {
            turn_id: app.checkpoint_turn,
            sha: "a".repeat(40),
            reference: format!(
                "{}-checkpoints/{}",
                app.workspace.reference, app.checkpoint_turn
            ),
            unmerged_paths: vec![],
            live_processes: false,
        };
        assert!(
            journal
                .change_inheritance(Change::Checkpoint {
                    operation: app.operation,
                    checkpoint
                })
                .await
                .is_err()
        );
        assert_eq!(
            crate::lineage_store::basis(&db, &app.dependencies.track)?,
            newer
        );
        assert!(
            journal
                .operation(Uuid::new_v5(&app.checkpoint_turn, b"checkpoint"))
                .await?
                .is_none()
        );
        Ok(())
    }
}
