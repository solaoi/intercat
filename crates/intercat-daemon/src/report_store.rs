//! Report-question transactions, executed only by Journal's writer thread.
use crate::{
    Result,
    reports::{Analysis, Answer, Observation, Question, Report},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use uuid::Uuid;
fn event(tx: &Transaction<'_>, kind: &str, payload: Value) -> Result<i64> {
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,?2,?3)",
        params![
            crate::journal::timestamp(),
            kind,
            serde_json::to_string(&payload)?
        ],
    )?;
    Ok(tx.last_insert_rowid())
}
fn question(row: &rusqlite::Row<'_>) -> rusqlite::Result<String> {
    row.get(0)
}
pub fn active_questions(connection: &Connection) -> Result<Vec<Value>> {
    let mut query = connection.prepare(
        "SELECT q.record,coalesce(
            json_extract(w.record,'$.activeTurn'),
            CASE WHEN json_extract(w.record,'$.checkpointPending')=1 THEN (
                SELECT json_extract(e.payload,'$.activeTurn') FROM events e
                WHERE e.kind='worker.state' AND json_type(e.payload,'$.activeTurn')='text'
                AND json_extract(e.payload,'$.trackId')=q.track_id
                AND json_extract(e.payload,'$.sessionId')=json_extract(q.record,'$.sessionId')
                ORDER BY e.seq DESC LIMIT 1
            ) ELSE json_extract(w.record,'$.checkpoint.turnId') END
        ) FROM report_questions q LEFT JOIN workers w ON w.track_id=q.track_id
          AND json_extract(w.record,'$.sessionId')=json_extract(q.record,'$.sessionId')
          WHERE q.state IN ('open','queued','held') ORDER BY q.rowid",
    )?;
    query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .map(|row| {
            let (record, current) = row?;
            let question: Question = serde_json::from_str(&record)?;
            let late = current
                .as_deref()
                .is_some_and(|turn| question.turn_id.to_string() != turn);
            let mut value = serde_json::to_value(&question)?;
            if let Some((plan, item)) = crate::plan_execution::turn_owner(
                connection,
                &question.session_id,
                &question.turn_id.to_string(),
            )? {
                value["planId"] = plan.to_string().into();
                value["planItemId"] = item.to_string().into();
            }
            value["askedBeforeLastMessage"] = late.into();
            Ok(value)
        })
        .collect()
}
pub fn pending_reports(connection: &Connection) -> Result<Vec<Report>> {
    connection
        .prepare("SELECT record FROM reports WHERE pending=1 ORDER BY rowid")?
        .query_map([], question)?
        .map(|s| Ok(serde_json::from_str(&s?)?))
        .collect()
}
/// Retain old questions as history while the replacement receives them in its handoff.
pub(crate) fn retire_session(tx: &Transaction<'_>, track: &str, session: &str) -> Result<()> {
    tx.execute("UPDATE utterances SET question_id=NULL,question_revision=NULL WHERE state='held' AND question_id IN (SELECT id FROM report_questions WHERE track_id=?1 AND json_extract(record,'$.sessionId')=?2)",params![track,session])?;
    tx.execute("UPDATE report_questions SET state='superseded',record=json_set(record,'$.state','superseded') WHERE track_id=?1 AND json_extract(record,'$.sessionId')=?2 AND state IN ('open','queued','held')",params![track,session])?;
    tx.execute("UPDATE reports SET pending=0,record=json_set(record,'$.pending',json('false')) WHERE track_id=?1 AND json_extract(record,'$.sessionId')=?2",params![track,session])?;
    Ok(())
}
pub(crate) fn read_question(tx: &Transaction<'_>, id: &str) -> Result<Question> {
    Ok(serde_json::from_str(&tx.query_row(
        "SELECT record FROM report_questions WHERE id=?1",
        [id],
        question,
    )?)?)
}
pub(crate) fn put_question(tx: &Transaction<'_>, q: &Question) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO operations VALUES(?1,'report.question',?2,'complete')",
        params![
            question_version_id(&q.id, q.revision),
            serde_json::to_string(q)?
        ],
    )?;
    tx.execute("INSERT INTO report_questions(id,report_id,track_id,state,record) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET state=excluded.state,record=excluded.record",params![q.id,q.report_id,q.track_id,q.state,serde_json::to_string(q)?])?;
    Ok(())
}
fn question_version_id(id: &str, revision: i64) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("report.question:{id}:{revision}").as_bytes(),
    )
    .to_string()
}
pub(crate) fn original_question(
    connection: &Connection,
    id: &str,
    revision: i64,
) -> Result<Option<Question>> {
    let value: Option<String> = connection
        .query_row(
            "SELECT payload FROM operations WHERE id=?1 AND kind='report.question'",
            [question_version_id(id, revision)],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}
fn invalidate(tx: &Transaction<'_>, report: &str) -> Result<()> {
    let values: Vec<String> = tx
        .prepare("SELECT record FROM report_questions WHERE report_id=?1 AND json_extract(record,'$.assumed') IS NULL")?
        .query_map([report], question)?
        .collect::<rusqlite::Result<_>>()?;
    for value in values {
        let mut q: Question = serde_json::from_str(&value)?;
        if let Some(input) = &q.reply_utterance_id {
            tx.execute("UPDATE utterances SET state='orphaned',reason='the question changed meanwhile',revision=revision+1 WHERE id=?1 AND state IN ('placed','held','starting')",[input])?;
        }
        q.state = "superseded".into();
        put_question(tx, &q)?;
    }
    Ok(())
}
fn apply(tx: &Transaction<'_>, report: &mut Report, analysis: Analysis) -> Result<()> {
    report.pending = false;
    let (kind, questions) = match analysis {
        Analysis::Clear => ("question", vec![]),
        Analysis::Questions(q) => ("question", q),
        Analysis::Review => ("review", vec![report.text.clone()]),
    };
    for (index, text) in questions.into_iter().enumerate() {
        let id =
            Uuid::new_v5(&Uuid::parse_str(&report.id)?, index.to_string().as_bytes()).to_string();
        let q = Question {
            id,
            report_id: report.id.clone(),
            track_id: report.track_id.clone(),
            session_id: report.session_id.clone(),
            turn_id: report.turn_id,
            revision: report.revision,
            kind: kind.into(),
            text,
            state: "open".into(),
            assumed: None,
            reply_utterance_id: None,
        };
        put_question(tx, &q)?;
    }
    tx.execute(
        "UPDATE reports SET pending=0,record=?2 WHERE id=?1",
        params![report.id, serde_json::to_string(report)?],
    )?;
    Ok(())
}
pub fn observe(connection: &mut Connection, mut report: Report) -> Result<Observation> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let tx = connection.transaction()?;
    let old: Option<String> = tx
        .query_row(
            "SELECT record FROM reports WHERE id=?1",
            [&report.id],
            question,
        )
        .optional()?;
    if let Some(old) = old {
        let old: Report = serde_json::from_str(&old)?;
        if old.track_id != report.track_id
            || old.session_id != report.session_id
            || old.turn_id != report.turn_id
        {
            return Err(std::io::Error::other("Report identity changed.").into());
        }
        if old.text == report.text {
            let sequence =
                tx.query_row("SELECT coalesce(max(seq),0) FROM events", [], |r| r.get(0))?;
            return Ok(Observation {
                report: old,
                sequence,
            });
        }
        report.revision = old.revision + 1;
        invalidate(&tx, &report.id)?;
    }
    tx.execute("INSERT INTO reports(id,track_id,pending,record) VALUES(?1,?2,1,?3) ON CONFLICT(id) DO UPDATE SET pending=1,record=excluded.record",params![report.id,report.track_id,serde_json::to_string(&report)?])?;
    let explicit = crate::reports::explicit_questions(&report.text);
    if !explicit.is_empty() {
        let analysis = if explicit.iter().any(String::is_empty) {
            Analysis::Review
        } else {
            Analysis::Questions(explicit)
        };
        apply(&tx, &mut report, analysis)?;
    }
    let sequence = event(
        &tx,
        "report.observed",
        json!({"reportId":report.id,"revision":report.revision,"pending":report.pending}),
    )?;
    tx.commit()?;
    Ok(Observation { report, sequence })
}
pub fn analyzed(
    connection: &mut Connection,
    id: &str,
    revision: i64,
    analysis: Analysis,
) -> Result<i64> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let tx = connection.transaction()?;
    let current: Option<String> = tx
        .query_row("SELECT record FROM reports WHERE id=?1", [id], question)
        .optional()?;
    let mut report = current
        .map(|record| serde_json::from_str::<Report>(&record))
        .transpose()?;
    let sequence = if let Some(report) = report.as_mut()
        && report.revision == revision
        && report.pending
    {
        apply(&tx, report, analysis)?;
        event(
            &tx,
            "report.analyzed",
            json!({"reportId":id,"revision":revision}),
        )?
    } else {
        event(
            &tx,
            "report.analysis-stale",
            json!({"reportId":id,"revision":revision,"state":"stale"}),
        )?
    };
    tx.commit()?;
    Ok(sequence)
}

/// The writer rechecks urgency so an old background result cannot release a new follow-up.
pub fn analyzed_background(
    connection: &mut Connection,
    results: &[(Report, Analysis)],
) -> Result<i64> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let tx = connection.transaction()?;
    let mut recorded = Vec::new();
    for (source, analysis) in results {
        let current: Option<String> = tx
            .query_row(
                "SELECT record FROM reports WHERE id=?1",
                [&source.id],
                question,
            )
            .optional()?;
        let Some(current) = current else {
            recorded.push(json!({"reportId":source.id,"revision":source.revision,"state":"stale"}));
            continue;
        };
        let mut report: Report = serde_json::from_str(&current)?;
        let urgent: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM utterances WHERE state IN ('staged','reading','placed','starting') AND coalesce(json_extract(decision,'$.kind'),'')<>'park' AND (json_extract(decision,'$.trackId')=?1 OR provisional=?1 OR (json_extract(target,'$.mode')='lock' AND json_extract(target,'$.trackId')=?1)))", [&report.track_id], |row| row.get(0))?;
        let state = if report.revision != source.revision || !report.pending {
            "stale"
        } else if urgent {
            "priority"
        } else {
            apply(&tx, &mut report, analysis.clone())?;
            event(
                &tx,
                "report.analyzed",
                json!({"reportId":report.id,"revision":report.revision}),
            )?;
            "applied"
        };
        recorded.push(json!({"reportId":source.id,"revision":source.revision,"state":state}));
    }
    let sequence = event(&tx, "report.batch", json!({"reports":recorded}))?;
    tx.commit()?;
    Ok(sequence)
}
pub fn respond(
    connection: &mut Connection,
    id: Uuid,
    question_id: &str,
    revision: i64,
    answer: Answer,
) -> Result<Option<String>> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let tx = connection.transaction()?;
    let payload = serde_json::to_string(
        &json!({"questionId":question_id,"revision":revision,"answer":answer}),
    )?;
    if let Some(old) = tx
        .query_row(
            "SELECT payload FROM operations WHERE id=?1 AND kind='report.respond'",
            [id.to_string()],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        if old != payload {
            return Err(std::io::Error::other("Operation ID reused.").into());
        }
        return Ok(matches!(answer, Answer::Reply { .. }).then(|| id.to_string()));
    }
    let mut q = read_question(&tx, question_id)?;
    if revision < 1 || revision > q.revision {
        return Err("Invalid question revision.".into());
    }
    let orphaned = if q.revision != revision || q.state == "superseded" {
        Some("the question changed meanwhile")
    } else if !["open", "queued", "held"].contains(&q.state.as_str()) {
        Some("the question was answered meanwhile")
    } else {
        None
    };
    let secret_pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM secret_inputs WHERE state IN ('prepared','stopping','restarting') AND json_extract(record,'$.owner.kind')='work' AND json_extract(record,'$.question.kind')='report' AND json_extract(record,'$.question.id')=?1)",[question_id],|r|r.get(0))?;
    if secret_pending && orphaned.is_none() {
        return Err("A secret is being prepared for this question. Stop the worker before changing its answer.".into());
    }
    if orphaned.is_some() && !matches!(answer, Answer::Reply { .. }) {
        return Err(
            std::io::Error::other("The question changed or has already been answered.").into(),
        );
    }
    let input = match answer {
        Answer::Reply { text } => {
            if text.trim().is_empty() || text.len() > 32 * 1024 {
                return Err(std::io::Error::other("Reply must contain 1–32768 bytes.").into());
            }
            if let Some(old) = &q.reply_utterance_id
                && orphaned.is_none()
            {
                let state: String =
                    tx.query_row("SELECT state FROM utterances WHERE id=?1", [old], |r| {
                        r.get(0)
                    })?;
                if ["starting", "sent", "landed"].contains(&state.as_str()) {
                    return Err(std::io::Error::other("A reply is already being sent.").into());
                }
                tx.execute("UPDATE utterances SET state='orphaned',reason='Another reply replaced this one',revision=revision+1 WHERE id=?1",[old])?;
            }
            let id = id.to_string();
            let decision = json!({"kind":"extend","trackId":q.track_id,"boundary":"after-turn","command":null,"confidence":1.0,"inferenceMs":0.0,"timedOut":false,"scores":null});
            tx.execute("INSERT INTO utterances(id,text,created_at,target,selected,state,revision,decision,reason,provisional,question_id,question_revision) VALUES(?1,?2,?3,?4,?5,?6,1,?7,?8,?5,?9,?10)",params![id,text,crate::journal::timestamp(),json!({"mode":"lock","trackId":q.track_id}).to_string(),q.track_id,if orphaned.is_some() { "orphaned" } else { "placed" },orphaned.is_none().then(|| decision.to_string()),orphaned,q.id,revision])?;
            if orphaned.is_none() {
                crate::review_fix_store::human_input(&tx, &id, 1, &q.track_id)?;

                q.reply_utterance_id = Some(id.clone());
                q.state = "queued".into();
            }
            Some(id)
        }
        Answer::Continue | Answer::Hold => {
            if q.kind != "review" || q.reply_utterance_id.is_some() {
                return Err(std::io::Error::other("This question needs a reply.").into());
            }
            q.state = if matches!(answer, Answer::Continue) {
                "answered"
            } else {
                "held"
            }
            .into();
            None
        }
    };
    put_question(&tx, &q)?;
    if let Some(input) = &input {
        crate::fresh_words::route_reply(&tx, input, &q.track_id)?;
        if orphaned.is_none() {
            crate::plan_continuation::attach_to_turn(
                &tx,
                input,
                &q.session_id,
                &q.turn_id.to_string(),
            )?;
        }
        q = read_question(&tx, question_id)?;
    }

    tx.execute(
        "INSERT INTO operations VALUES(?1,'report.respond',?2,'complete')",
        params![id.to_string(), payload],
    )?;
    event(
        &tx,
        "report.responded",
        json!({"questionId":q.id,"revision":q.revision,"state":q.state,"inputId":input}),
    )?;
    tx.commit()?;
    Ok(input)
}
pub fn validate_dispatch(tx: &Transaction<'_>, input: &str, track: &str) -> Result<()> {
    let link: (Option<String>, Option<i64>) = tx.query_row(
        "SELECT question_id,question_revision FROM utterances WHERE id=?1",
        [input],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if let Some(id) = link.0 {
        let q = read_question(tx, &id)?;
        if Some(q.revision) != link.1
            || q.track_id != track
            || q.state != "queued"
            || q.reply_utterance_id.as_deref() != Some(input)
        {
            return Err(crate::journal::StartSuperseded.into());
        }
        let earlier:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM report_questions q JOIN utterances u ON u.id=json_extract(q.record,'$.replyUtteranceId') WHERE q.track_id=?1 AND q.state='queued' AND u.state='placed' AND q.rowid<(SELECT rowid FROM report_questions WHERE id=?2))",params![track,id],|r|r.get(0))?;
        if earlier {
            return Err(crate::journal::StartSuperseded.into());
        }
    } else {
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM reports WHERE track_id=?1 AND pending=1) OR EXISTS(SELECT 1 FROM report_questions WHERE track_id=?1 AND state IN ('open','queued','held'))",[track],|r|r.get(0))?;
        if blocked {
            return Err(crate::journal::StartSuperseded.into());
        }
    }
    Ok(())
}
pub fn input_state(tx: &Transaction<'_>, input: &str, state: &str) -> Result<()> {
    let link: Option<String> = tx.query_row(
        "SELECT question_id FROM utterances WHERE id=?1",
        [input],
        |r| r.get(0),
    )?;
    if let Some(id) = link {
        let mut q = read_question(tx, &id)?;
        if q.reply_utterance_id.as_deref() == Some(input)
            && ["open", "queued"].contains(&q.state.as_str())
        {
            match state {
                "sent" | "landed" => q.state = "answered".into(),
                "held" => q.state = "open".into(),
                "cut" => {
                    q.state = "open".into();
                    q.reply_utterance_id = None;
                }
                _ => {}
            }
            put_question(tx, &q)?;
        }
    }
    Ok(())
}
pub fn retry_reply(
    tx: &Transaction<'_>,
    input: &str,
    target: &crate::decision::Target,
) -> Result<bool> {
    let link: Option<String> = tx.query_row(
        "SELECT question_id FROM utterances WHERE id=?1",
        [input],
        |r| r.get(0),
    )?;
    let Some(id) = link else {
        return Ok(false);
    };
    let mut q = read_question(tx, &id)?;
    if !matches!(target,crate::decision::Target::Lock(track) if track==&q.track_id)
        || q.reply_utterance_id.as_deref() != Some(input)
        || q.state != "open"
    {
        return Err(std::io::Error::other("Reply destination changed.").into());
    }
    q.state = "queued".into();
    put_question(tx, &q)?;
    tx.execute(
        "UPDATE utterances SET state='placed',revision=revision+1,reason=NULL WHERE id=?1",
        [input],
    )?;
    Ok(true)
}

pub(crate) fn detach_orphaned_reply(tx: &Transaction<'_>, input: &str) -> Result<()> {
    let link: (Option<String>, Option<i64>) = tx.query_row(
        "SELECT question_id,question_revision FROM utterances WHERE id=?1",
        [input],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if let Some(id) = link.0 {
        let q = read_question(tx, &id)?;
        if Some(q.revision) != link.1
            || !["open", "queued", "held"].contains(&q.state.as_str())
            || q.reply_utterance_id.as_deref() != Some(input)
        {
            tx.execute(
                "UPDATE utterances SET question_id=NULL,question_revision=NULL WHERE id=?1",
                [input],
            )?;
        }
    }
    Ok(())
}

pub(crate) fn restore_reply(tx: &Transaction<'_>, input: &str) -> Result<()> {
    let (id, revision, state): (Option<String>, Option<i64>, String) = tx.query_row(
        "SELECT question_id,question_revision,state FROM utterances WHERE id=?1",
        [input],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let Some(id) = id else {
        return Ok(());
    };
    let mut question = read_question(tx, &id)?;
    if Some(question.revision) != revision
        || !["open", "queued", "held"].contains(&question.state.as_str())
        || question
            .reply_utterance_id
            .as_deref()
            .is_some_and(|other| other != input)
    {
        let reason = if question.state == "answered" {
            "the question was answered meanwhile"
        } else {
            "the question changed meanwhile"
        };
        tx.execute(
            "UPDATE utterances SET state='orphaned',reason=?2 WHERE id=?1",
            params![input, reason],
        )?;
        return Ok(());
    }
    question.reply_utterance_id = Some(input.into());
    question.state = if state == "held" { "open" } else { "queued" }.into();
    put_question(tx, &question)
}
