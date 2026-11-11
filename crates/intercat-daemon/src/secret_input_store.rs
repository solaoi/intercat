use crate::{
    Result,
    repair_run::{Control, Phase, Run},
    secret_input::{Owner, Question, Target},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use uuid::Uuid;

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE secret_inputs(id TEXT PRIMARY KEY,owner TEXT NOT NULL,track TEXT NOT NULL,state TEXT NOT NULL,record TEXT NOT NULL,host TEXT,detail TEXT);")?;
    Ok(())
}
pub fn repair_pending(db: &Connection, id: Uuid) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM secret_inputs WHERE owner=?1 AND state IN ('prepared','stopping','settled','restarting'))", [Owner::Repair {run_id:id}.key("")], |r|r.get(0))?)
}
fn report_question(
    tx: &Transaction<'_>,
    target: &Target,
) -> Result<Option<crate::reports::Question>> {
    if matches!(target.owner, Owner::Repair { .. }) {
        repair_question(tx, target)?;
        return Ok(None);
    }
    let crate::secret_input::Question::Report { id, revision } = &target.question else {
        return Ok(None);
    };
    let question = crate::report_store::read_question(tx, id)?;
    if question.revision != *revision
        || question.track_id != target.track_id
        || question.session_id != target.session_id
        || question.turn_id != target.turn_id
        || !["open", "held"].contains(&question.state.as_str())
        || question.reply_utterance_id.is_some()
    {
        return Err("The report question changed or already has a reply.".into());
    }
    Ok(Some(question))
}
fn repair_question(tx: &Transaction<'_>, target: &Target) -> Result<Run> {
    let Owner::Repair { run_id } = target.owner else {
        return Err("Expected a repair destination.".into());
    };
    let (run, _) = crate::repair_run_store::source(tx, run_id)?;
    let question_matches = match &target.question {
        Question::Report { id, revision } => {
            id == &run_id.to_string()
                && u64::try_from(*revision).ok() == Some(run.revision)
                && run.phase == Phase::Held
                && run.host.is_none()
                && run.output.as_ref().is_some_and(|output| {
                    output
                        .lines()
                        .any(|line| line.trim_start().starts_with("Waiting for you:"))
                })
        }
        Question::Msp { id, revision } => {
            run.phase == Phase::Running
                && run.host.is_some()
                && run.prompts.iter().any(|prompt| {
                    prompt.request.user_input_id == *id
                        && prompt.revision == *revision
                        && prompt.request.turn_id == target.turn_id.to_string()
                        && prompt.phase == crate::questions::Phase::Open
                })
        }
    };
    if !question_matches
        || run.track != target.track_id
        || run.session.as_deref() != Some(&target.session_id)
        || run.turn != Some(target.turn_id)
        || run.restoring.is_some()
        || run
            .refresh
            .as_ref()
            .is_some_and(|refresh| refresh.error.is_none())
    {
        return Err("The repair question changed or is no longer waiting.".into());
    }
    Ok(run)
}

pub fn queue_repair(
    db: &mut Connection,
    target: &Target,
    checkpoint: crate::checkpoint::Checkpoint,
) -> Result<()> {
    let tx = db.transaction()?;
    let (record, state): (String, String) = tx.query_row(
        "SELECT record,state FROM secret_inputs WHERE id=?1",
        [target.op_id.to_string()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if record != serde_json::to_string(target)?
        || state
            != if matches!(target.question, Question::Msp { .. }) {
                "settled"
            } else {
                "stopping"
            }
        || checkpoint.turn_id != target.turn_id
        || checkpoint.live_processes
    {
        return Err("The repair secret preparation changed.".into());
    }
    let mut run = if let (Owner::Repair { run_id }, Question::Msp { .. }) =
        (&target.owner, &target.question)
    {
        let (run, _) = crate::repair_run_store::source(&tx, *run_id)?;
        if run.phase != Phase::Held
            || run.host.is_some()
            || run.secret_input != Some(target.op_id)
            || run.session.as_deref() != Some(&target.session_id)
            || run.turn != Some(target.turn_id)
            || run.restoring.is_some()
            || run
                .refresh
                .as_ref()
                .is_some_and(|refresh| refresh.error.is_none())
        {
            return Err("The stopped repair secret destination changed.".into());
        }
        run
    } else {
        repair_question(&tx, target)?
    };
    if checkpoint.reference != format!("refs/intercat/repairs/{}-secrets/{}", run.id, target.op_id)
    {
        return Err("The checkpoint belongs to another repair.".into());
    }
    run.control(Control::Resume)?;
    run.secret_input = Some(target.op_id);
    run.checkpoint = Some(checkpoint);
    run.revision += 1;
    crate::repair_run_store::save(&tx, &run)?;
    crate::repair_run_store::record(&tx, "repair.secret-queued", &run)?;
    tx.execute(
        "UPDATE secret_inputs SET state='restarting' WHERE id=?1",
        [target.op_id.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn validate_repair_start(db: &Connection, id: Uuid, run: &Run) -> Result<()> {
    let (state, record, host): (String, String, Option<String>) = db.query_row(
        "SELECT state,record,host FROM secret_inputs WHERE id=?1",
        [id.to_string()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let target: Target = serde_json::from_str(&record)?;
    if state != "ready"
        || target.owner != (Owner::Repair { run_id: run.id })
        || target.session_id != run.session.clone().unwrap_or_default()
        || host != run.host_nonce.map(|id| id.to_string())
    {
        return Err(crate::journal::StartSuperseded.into());
    }
    Ok(())
}
fn hold_resume(tx: &Transaction<'_>, id: Uuid) -> Result<()> {
    let record: String = tx.query_row(
        "SELECT record FROM secret_inputs WHERE id=?1",
        [id.to_string()],
        |r| r.get(0),
    )?;
    let target: Target = serde_json::from_str(&record)?;
    if let Owner::Repair { run_id } = target.owner {
        let mut run = crate::repair_run_store::read(tx, run_id)?;
        if run.secret_input == Some(id) && run.phase == Phase::Queued {
            run.control(Control::Stop)?;
            run.revision += 1;
            crate::repair_run_store::save(tx, &run)?;
            crate::repair_run_store::record(tx, "repair.secret-unavailable", &run)?;
        }
        return Ok(());
    }
    let input = Uuid::new_v5(&id, b"secret resume").to_string();
    if tx.execute("UPDATE utterances SET state='held',reason='The secret is no longer available. Enter it again.' WHERE id=?1 AND state='placed'",[&input])? != 0 {
        crate::report_store::input_state(tx, &input, "cut")?;
    }
    Ok(())
}
pub fn prepare(db: &mut Connection, target: &Target) -> Result<bool> {
    let tx = db.transaction()?;
    let catalog = crate::secret_store::catalog(&tx)?;
    if catalog.sealed.as_ref() != Some(&catalog.current) {
        return Err("Confirm the saved fingerprint before providing the value.".into());
    }
    let project = crate::project_secrets::project_id(&tx)?.unwrap_or_default();
    let matching:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM secret_fingerprints WHERE session=?1 AND track=?2 AND name=?3 AND byte_len=?4 AND tag=?5 UNION ALL SELECT 1 FROM project_fingerprints WHERE project=?6 AND name=?3 AND byte_len=?4 AND tag=?5)",params![target.session_id,target.track_id,target.name,target.byte_len as i64,target.tag,project],|r|r.get(0))?;
    if !matching {
        return Err("The secret value does not match its saved fingerprint.".into());
    }
    let record = serde_json::to_string(target)?;
    let previous: Option<(String, String)> = tx
        .query_row(
            "SELECT record,state FROM secret_inputs WHERE id=?1",
            [target.op_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((old, state)) = previous {
        if old != record {
            return Err("The secret operation ID was reused.".into());
        }
        if matches!(
            state.as_str(),
            "stopping" | "settled" | "restarting" | "ready" | "expired"
        ) {
            return Ok(false);
        }
        if state == "failed" {
            return Err("Enter the secret again with a new request.".into());
        }
        return Ok(true);
    }
    let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM secret_inputs WHERE owner=?1 AND state IN ('prepared','stopping','settled','restarting'))",[target.owner.key(&target.track_id)],|r|r.get(0))?;
    if active {
        return Err("A secret is already being prepared for this worker.".into());
    }
    report_question(&tx, target)?;
    tx.execute(
        "INSERT INTO secret_inputs VALUES(?1,?2,?3,'prepared',?4,NULL,NULL)",
        params![
            target.op_id.to_string(),
            target.owner.key(&target.track_id),
            target.track_id,
            record
        ],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'secret.prepared',?2)",
        params![
            crate::journal::timestamp(),
            json!({"opId":target.op_id,"owner":target.owner,"trackId":target.track_id,"name":target.name}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(true)
}
pub fn accepted(db: &mut Connection, id: Uuid) -> Result<()> {
    let tx = db.transaction()?;
    let record: String = tx.query_row(
        "SELECT record FROM secret_inputs WHERE id=?1",
        [id.to_string()],
        |r| r.get(0),
    )?;
    let target: Target = serde_json::from_str(&record)?;
    report_question(&tx, &target)?;
    if tx.execute(
        "UPDATE secret_inputs SET state='stopping' WHERE id=?1 AND state='prepared'",
        [id.to_string()],
    )? != 1
    {
        return Err("The secret preparation changed.".into());
    }
    if matches!(target.owner, Owner::Repair { .. })
        && matches!(target.question, Question::Msp { .. })
    {
        let mut run = repair_question(&tx, &target)?;
        run.secret_input = Some(id);
        run.revision += 1;
        crate::repair_run_store::save(&tx, &run)?;
        crate::repair_run_store::record(&tx, "repair.secret-reserved", &run)?;
    }
    tx.commit()?;
    Ok(())
}
pub fn settled(db: &mut Connection, id: Uuid) -> Result<()> {
    let tx = db.transaction()?;
    let (record, state): (String, String) = tx.query_row(
        "SELECT record,state FROM secret_inputs WHERE id=?1",
        [id.to_string()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let target: Target = serde_json::from_str(&record)?;
    let Owner::Repair { run_id } = target.owner else {
        return Err("Only a repair can settle this secret.".into());
    };
    let run = crate::repair_run_store::read(&tx, run_id)?;
    if state != "stopping"
        || !matches!(target.question, Question::Msp { .. })
        || run.phase != Phase::Held
        || run.host.is_some()
        || run.secret_input != Some(id)
        || run.session.as_deref() != Some(&target.session_id)
        || run.turn != Some(target.turn_id)
    {
        return Err("The repair secret stop was not confirmed.".into());
    }
    tx.execute(
        "UPDATE secret_inputs SET state='settled' WHERE id=?1",
        [id.to_string()],
    )?;
    tx.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,'secret.settled',?2)",params![crate::journal::timestamp(),json!({"opId":id,"owner":target.owner,"trackId":target.track_id,"sessionId":target.session_id,"turnId":target.turn_id}).to_string()])?;
    tx.commit()?;
    Ok(())
}
pub fn restarting(db: &Connection, id: Uuid) -> Result<()> {
    if db.execute(
        "UPDATE secret_inputs SET state='restarting' WHERE id=?1 AND state='stopping'",
        [id.to_string()],
    )? != 1
    {
        return Err("Secret preparation changed.".into());
    }
    Ok(())
}
pub fn ready(db: &mut Connection, target: &Target, host: &str) -> Result<()> {
    let tx = db.transaction()?;
    let (state, record): (String, String) = tx.query_row(
        "SELECT state,record FROM secret_inputs WHERE id=?1",
        [target.op_id.to_string()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if record != serde_json::to_string(target)? {
        return Err("The secret destination changed.".into());
    }
    if state == "ready" {
        return Ok(());
    }
    if state != "restarting" {
        return Err("Secret preparation is not ready.".into());
    }
    let question = if let Owner::Repair { run_id } = target.owner {
        let run = crate::repair_run_store::read(&tx, run_id)?;
        if run.secret_input != Some(target.op_id)
            || run.phase != Phase::Preparing
            || run.session.as_deref() != Some(&target.session_id)
            || run.host_nonce.map(|id| id.to_string()).as_deref() != Some(host)
            || !run.session_ready
        {
            return Err("The repair secret destination changed.".into());
        }
        None
    } else {
        report_question(&tx, target)?
    };
    let input = Uuid::new_v5(&target.op_id, b"secret resume").to_string();
    let text = format!("The token is in ${}. Run it again.", target.name);
    let decision = json!({"kind":"extend","trackId":target.track_id,"boundary":"now","command":null,"confidence":1.0,"inferenceMs":0.0,"timedOut":false,"scores":{}});
    if target.owner == Owner::Work {
        tx.execute("INSERT INTO utterances(id,text,created_at,target,selected,state,revision,decision,reason,provisional,question_id,question_revision) VALUES(?1,?2,?3,?4,?5,'placed',1,?6,'secret ready',?5,?7,?8)",params![input,text,crate::journal::timestamp(),json!({"mode":"lock","trackId":target.track_id}).to_string(),target.track_id,decision.to_string(),question.as_ref().map(|q|q.id.as_str()),question.as_ref().map(|q|q.revision)])?;
    }
    if let Some(mut question) = question {
        question.state = "queued".into();
        question.reply_utterance_id = Some(input.clone());
        crate::report_store::put_question(&tx, &question)?;
    }
    tx.execute(
        "UPDATE secret_inputs SET state='ready',host=?2 WHERE id=?1",
        params![target.op_id.to_string(), host],
    )?;
    tx.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,'secret.ready',?2)",params![crate::journal::timestamp(),json!({"opId":target.op_id,"owner":target.owner,"trackId":target.track_id,"name":target.name,"inputId":input}).to_string()])?;
    tx.commit()?;
    Ok(())
}
pub fn fail(db: &mut Connection, id: Uuid) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute("UPDATE secret_inputs SET state='failed',detail='Enter the secret again after reconnecting the worker.' WHERE id=?1 AND state IN ('prepared','stopping','settled','restarting','ready')",[id.to_string()])?;
    hold_resume(&tx, id)?;
    tx.commit()?;
    Ok(())
}
pub fn expire(db: &mut Connection, host: &str) -> Result<()> {
    let tx = db.transaction()?;
    let ids = tx
        .prepare("SELECT id FROM secret_inputs WHERE host=?1 AND state='ready'")?
        .query_map([host], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        hold_resume(&tx, Uuid::parse_str(&id)?)?;
    }
    tx.execute("UPDATE secret_inputs SET state='expired',detail='This secret entry ended with its host. Re-enter one-time values if needed; check saved project values and remaining processes in Settings.' WHERE host=?1 AND state='ready'",[host])?;
    tx.commit()?;
    Ok(())
}
pub fn interrupt(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let ids = tx
        .prepare("SELECT id FROM secret_inputs WHERE state='ready'")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        hold_resume(&tx, Uuid::parse_str(&id)?)?;
    }
    tx.execute("UPDATE secret_inputs SET state='expired',detail='This secret entry ended when the app restarted. Re-enter one-time values if needed; check saved project values in Settings.' WHERE state='ready'",[])?;
    tx.execute("UPDATE secret_inputs SET state='failed',detail='The app restarted before the value was delivered. Enter it again.' WHERE state IN ('prepared','stopping','settled','restarting')",[])?;
    tx.commit()?;
    Ok(())
}
pub fn snapshot(db: &Connection) -> Result<Vec<Value>> {
    db.prepare("SELECT id,track,state,json_extract(record,'$.name'),detail,json_extract(record,'$.owner') FROM secret_inputs ORDER BY rowid DESC LIMIT 100")?
        .query_map([],|r|{let id=r.get::<_,String>(0)?;let owner:String=r.get(5)?;Ok(json!({"inputId":Uuid::new_v5(&Uuid::parse_str(&id).map_err(|error|rusqlite::Error::FromSqlConversionFailure(0,rusqlite::types::Type::Text,Box::new(error)))?,b"secret resume"),"opId":id,"owner":serde_json::from_str::<Value>(&owner).unwrap(),"trackId":r.get::<_,String>(1)?,"state":r.get::<_,String>(2)?,"name":r.get::<_,String>(3)?,"detail":r.get::<_,Option<String>>(4)?}))})?
        .collect::<std::result::Result<Vec<_>,_>>().map_err(Into::into)
}
