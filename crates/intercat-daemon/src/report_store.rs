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
pub fn active_questions(connection: &Connection) -> Result<Vec<Question>> {
    connection.prepare("SELECT record FROM report_questions WHERE state IN ('open','queued','held') ORDER BY rowid")?.query_map([],question)?.map(|s|Ok(serde_json::from_str(&s?)?)).collect()
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
    tx.execute("INSERT INTO report_questions(id,report_id,track_id,state,record) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET state=excluded.state,record=excluded.record",params![q.id,q.report_id,q.track_id,q.state,serde_json::to_string(q)?])?;
    Ok(())
}
fn invalidate(tx: &Transaction<'_>, report: &str) -> Result<()> {
    let values: Vec<String> = tx
        .prepare("SELECT record FROM report_questions WHERE report_id=?1")?
        .query_map([report], question)?
        .collect::<rusqlite::Result<_>>()?;
    for value in values {
        let mut q: Question = serde_json::from_str(&value)?;
        if let Some(input) = &q.reply_utterance_id {
            tx.execute("UPDATE utterances SET state='orphaned',reason='The question changed',revision=revision+1 WHERE id=?1 AND state IN ('placed','held','starting')",[input])?;
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
    let mut report: Report = serde_json::from_str(&tx.query_row(
        "SELECT record FROM reports WHERE id=?1",
        [id],
        question,
    )?)?;
    if report.revision != revision || !report.pending {
        return Ok(tx.query_row("SELECT coalesce(max(seq),0) FROM events", [], |r| r.get(0))?);
    }
    apply(&tx, &mut report, analysis)?;
    let sequence = event(
        &tx,
        "report.analyzed",
        json!({"reportId":id,"revision":revision}),
    )?;
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
    let secret_pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM secret_inputs WHERE state IN ('prepared','stopping','restarting') AND json_extract(record,'$.owner.kind')='work' AND json_extract(record,'$.question.kind')='report' AND json_extract(record,'$.question.id')=?1)",[question_id],|r|r.get(0))?;
    if secret_pending {
        return Err("A secret is being prepared for this question. Stop the worker before changing its answer.".into());
    }
    if q.revision != revision || !["open", "queued", "held"].contains(&q.state.as_str()) {
        return Err(
            std::io::Error::other("The question changed or has already been answered.").into(),
        );
    }
    let input = match answer {
        Answer::Reply { text } => {
            if text.trim().is_empty() || text.len() > 32 * 1024 {
                return Err(std::io::Error::other("Reply must contain 1–32768 bytes.").into());
            }
            if let Some(old) = &q.reply_utterance_id {
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
            tx.execute("INSERT INTO utterances(id,text,created_at,target,selected,state,revision,decision,reason,provisional,question_id,question_revision) VALUES(?1,?2,?3,?4,?5,'placed',1,?6,NULL,?5,?7,?8)",params![id,text,crate::journal::timestamp(),json!({"mode":"lock","trackId":q.track_id}).to_string(),q.track_id,decision.to_string(),q.id,q.revision])?;
            q.reply_utterance_id = Some(id.clone());
            q.state = "queued".into();
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
