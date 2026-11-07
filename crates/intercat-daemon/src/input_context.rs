//! Bounded, recorded context for the next local Editor decision.
use crate::{Result, decision::Decision};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashSet;

struct CurrentRequestLast<'a>(&'a serde_json::Map<String, Value>);

impl serde::Serialize for CurrentRequestLast<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in self.0 {
            if name != "request" {
                map.serialize_entry(name, value)?;
            }
        }
        map.serialize_entry("request", &self.0["request"])?;
        map.end()
    }
}

pub(crate) fn serialize_tail(value: &Value) -> Result<String> {
    if let Some(object) = value.as_object()
        && object.contains_key("request")
        && object.contains_key("tracks")
    {
        return Ok(serde_json::to_string(&CurrentRequestLast(object))?);
    }
    Ok(serde_json::to_string(value)?)
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Planned {
    pub title: String,
    pub boundary: Option<String>,
}

pub(crate) fn track(db: &Connection, id: &str) -> Result<(Option<String>, Vec<Planned>)> {
    let clip: Option<(String,String)> = db.query_row("SELECT id,json_extract(record,'$.category') FROM timeline_clips
        WHERE track=?1 AND json_extract(record,'$.end') IS NULL AND json_extract(record,'$.repairRunId') IS NULL
        ORDER BY ordinal DESC LIMIT 1", [id], |row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    let running = if let Some((clip, category)) = clip {
        if let Some(summary) = crate::clip_summary_store::read(db, &clip)? {
            Some(summary.title)
        } else if category == "shell" {
            Some("Running shell".into())
        } else {
            db.query_row("SELECT coalesce(json_extract(record,'$.tool'),CASE WHEN json_extract(record,'$.kind')='agentMessage' THEN 'Reply' ELSE 'Thinking' END)
                FROM timeline_items WHERE clip=?1 ORDER BY json_extract(record,'$.kind')='toolCall' DESC,rowid DESC LIMIT 1", [clip], |row|row.get(0)).optional()?
        }
    } else {
        None
    };
    let planned = db
        .prepare(
            "SELECT CASE WHEN length(trim(u.text))>0 THEN u.text ELSE coalesce(json_extract(s.record,'$.preview'),'') END,json_extract(u.decision,'$.boundary') FROM utterances u LEFT JOIN utterance_sources s ON s.input=u.id
        WHERE u.state='placed' AND json_extract(u.decision,'$.trackId')=?1 ORDER BY u.created_at,u.id",
        )?
        .query_map([id], |row| {
            Ok(Planned {
                title: row.get(0)?,
                boundary: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((running, planned))
}

#[derive(Clone, Debug, Default)]
pub struct Context {
    pub decisions: Vec<Value>,
    pub utterances: Vec<Value>,
    pub cards: Vec<Value>,
}

impl Context {
    pub fn apply(&self, tail: &mut Value) {
        for (name, records) in [
            ("recentDecisions", &self.decisions),
            ("previousUtterances", &self.utterances),
            ("waitingCards", &self.cards),
        ] {
            if !records.is_empty() {
                let mut records = records.clone();
                for record in &mut records {
                    if record.get("text").and_then(Value::as_str).is_some()
                        && record.get("title") == record.get("text")
                    {
                        record.as_object_mut().unwrap().remove("title");
                    }
                }
                tail[name] = json!(records);
            }
        }
    }
}

fn remove_fields(records: Option<&mut Value>, fields: &[&str]) -> bool {
    let mut changed = false;
    for record in records.and_then(Value::as_array_mut).into_iter().flatten() {
        if let Some(record) = record.as_object_mut() {
            for field in fields {
                changed |= record.remove(*field).is_some();
            }
        }
    }
    changed
}

fn shorten_records(records: Option<&mut Value>) -> bool {
    let mut changed = false;
    for record in records.and_then(Value::as_array_mut).into_iter().flatten() {
        if let Some(record) = record.as_object_mut() {
            let text = record.remove("text");
            changed |= text.is_some();
            if let Some(Value::String(text)) = text
                && !record.contains_key("title")
            {
                record.insert("title".into(), crate::privacy::truncate(&text, 30).into());
            }
            changed |= record.remove("choices").is_some();
        }
    }
    changed
}

pub(crate) fn reduce(wire: &mut Value) -> Result<bool> {
    let mut tail: Value =
        serde_json::from_str(wire["tail"].as_str().ok_or("Missing Editor context.")?)?;
    if let Some(decisions) = tail
        .get_mut("recentDecisions")
        .and_then(Value::as_array_mut)
        && !decisions.is_empty()
    {
        decisions.remove(0);
        if decisions.is_empty() {
            tail.as_object_mut().unwrap().remove("recentDecisions");
        }
    } else if tail["tracks"]
        .as_array()
        .is_some_and(|tracks| tracks.len() > 4)
    {
        tail["tracks"].as_array_mut().unwrap().truncate(4);
        if tail["selected"]
            .as_u64()
            .is_some_and(|selected| selected > 4)
        {
            tail["selected"] = Value::Null;
        }
        for stage in wire["stages"].as_array_mut().into_iter().flatten() {
            if stage["name"] == "lane" {
                stage["candidates"].as_array_mut().unwrap().retain(|label| {
                    label == "9"
                        || label
                            .as_str()
                            .and_then(|label| label.parse::<usize>().ok())
                            .is_some_and(|lane| lane <= 4)
                });
            }
        }
    } else {
        let mut changed = shorten_records(tail.get_mut("previousUtterances"));
        changed |= shorten_records(tail.get_mut("waitingCards"));
        changed |= remove_fields(tail.get_mut("tracks"), &["activity"]);
        if !changed {
            for name in ["recentDecisions", "previousUtterances", "waitingCards"] {
                changed |= remove_fields(tail.get_mut(name), &["at"]);
            }
        }
        if !changed {
            return Ok(false);
        }
    }
    wire["tail"] = serialize_tail(&tail)?.into();
    Ok(true)
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE INDEX IF NOT EXISTS input_context_decisions ON events(json_extract(payload,'$.id'),seq DESC) WHERE kind='utterance.classified';
        CREATE INDEX IF NOT EXISTS input_context_recent_decisions ON events(seq DESC) WHERE kind='utterance.classified';
        CREATE INDEX IF NOT EXISTS input_context_running ON timeline_clips(track,ordinal DESC)
            WHERE json_extract(record,'$.end') IS NULL AND json_extract(record,'$.repairRunId') IS NULL;
        CREATE INDEX IF NOT EXISTS input_context_planned ON utterances(json_extract(decision,'$.trackId'),created_at,id) WHERE state='placed';")?;
    Ok(())
}

fn decision(db: &Connection, text: &str) -> Result<Value> {
    let decision: Decision = serde_json::from_str(text)?;
    let worker: Option<String> = decision
        .track_id
        .as_deref()
        .map(|id| {
            db.query_row("SELECT label FROM tracks WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()
        })
        .transpose()?
        .flatten();
    Ok(
        json!({"kind":decision.kind,"worker":worker,"boundary":decision.boundary,"command":decision.command}),
    )
}

pub(crate) fn read(db: &Connection, input: &str, reading: i64) -> Result<Context> {
    let anchor: Option<String> = db
        .query_row(
            "SELECT context_input FROM input_readings WHERE input=?1 AND number=?2 AND kind='park'",
            params![input, reading],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let mut context = Context::default();
    let mut query = db.prepare("SELECT u.id,e.seq,e.ts,CASE WHEN length(trim(u.text))>0 THEN u.text ELSE coalesce(json_extract(s.record,'$.preview'),'') END,u.decision,u.state FROM events e
        JOIN utterances u ON u.id=json_extract(e.payload,'$.id') LEFT JOIN utterance_sources s ON s.input=u.id
        WHERE e.kind='utterance.classified' AND u.id<>?1 AND u.decision IS NOT NULL
        AND u.state IN ('placed','held','landed','redirecting') AND json_type(e.payload,'$.decision')='object'
        ORDER BY e.seq DESC")?;
    let mut stream = query.query([input])?;
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    while let Some(row) = stream.next()? {
        if seen.insert(row.get::<_, String>(0)?) {
            rows.push((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ));
            if rows.len() == 8 {
                break;
            }
        }
    }
    rows.reverse();
    for (id, at, title, serialized, state) in rows {
        let mut record =
            json!({"title":title,"at":at,"state":state,"decision":decision(db,&serialized)?});
        if let Some(correction) = crate::input_feedback::latest(db, &id)? {
            record["corrected"] = true.into();
            record["correctionKind"] = correction["kind"].clone();
            record["misread"] = (correction["reason"] == "misread").into();
            if correction["before"].is_object() {
                record["previous"] = decision(db, &correction["before"].to_string())?;
            }
        }
        context.decisions.push(record);
    }
    let rows = db.prepare("SELECT CASE WHEN length(trim(u.text))>0 THEN u.text ELSE coalesce(json_extract(s.record,'$.preview'),'') END,u.created_at,t.label FROM events e JOIN utterances u ON u.id=json_extract(e.payload,'$.id') LEFT JOIN utterance_sources s ON s.input=u.id
        LEFT JOIN tracks t ON t.id=json_extract(u.decision,'$.trackId')
        WHERE e.kind='utterance.staged' AND e.seq<(SELECT seq FROM events WHERE kind='utterance.staged' AND json_extract(payload,'$.id')=?1 ORDER BY seq LIMIT 1)
        AND u.id<>?2 AND u.state NOT IN ('cut','orphaned') ORDER BY e.seq DESC LIMIT 3")?
        .query_map(params![anchor.as_deref().unwrap_or(input),input], |row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,Option<String>>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (text, at, worker) in rows.into_iter().rev() {
        context
            .utterances
            .push(json!({"title":text,"text":text,"worker":worker,"at":at}));
    }
    let rows = db.prepare("SELECT t.label,u.text,u.reason FROM utterances u LEFT JOIN tracks t ON t.id=json_extract(u.decision,'$.trackId')
        WHERE u.state='asked' AND u.id<>?1 ORDER BY u.created_at,u.id")?
        .query_map([input], |row|Ok((row.get::<_,Option<String>>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (worker, title, text) in rows {
        context
            .cards
            .push(json!({"kind":"request","worker":worker,"title":title,"text":text}));
    }
    let rows = db.prepare("SELECT t.label,json_extract(q.record,'$.text'),q.state FROM report_questions q JOIN tracks t ON t.id=q.track_id
        WHERE q.state IN ('open','queued','held') ORDER BY q.rowid")?
        .query_map([], |row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (worker, text, state) in rows {
        context.cards.push(json!({"kind":"report-question","worker":worker,"title":text,"text":text,"state":state}));
    }
    let rows = db.prepare("SELECT t.label,json_extract(q.value,'$.header'),json_extract(q.value,'$.question'),
        (SELECT json_group_array(json_extract(o.value,'$.label')) FROM json_each(q.value,'$.options') o)
        FROM workers w JOIN tracks t ON t.id=w.track_id,json_each(w.record,'$.prompts') p,json_each(p.value,'$.request.questions') q
        WHERE json_extract(p.value,'$.phase')='open' ORDER BY w.rowid,p.key,q.key")?
        .query_map([], |row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (worker, title, text, choices) in rows {
        context.cards.push(json!({"kind":"question","worker":worker,"title":title,"text":text,"choices":serde_json::from_str::<Value>(&choices)?}));
    }
    let rows = db.prepare("SELECT t.label,json_extract(a.value,'$.subject') FROM workers w JOIN tracks t ON t.id=w.track_id,json_each(w.record,'$.approvals') a ORDER BY w.rowid,a.key")?
        .query_map([], |row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (worker, subject) in rows {
        context
            .cards
            .push(json!({"kind":"approval","worker":worker,"title":subject,"text":subject}));
    }
    let rows = db.prepare("SELECT t.label,json_extract(w.record,'$.phase') FROM workers w JOIN tracks t ON t.id=w.track_id
        WHERE json_extract(w.record,'$.phase') IN ('failed','needs-recovery','lost','after-connection-loss') ORDER BY w.rowid")?
        .query_map([], |row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (worker, state) in rows {
        context
            .cards
            .push(json!({"kind":"worker","worker":worker,"title":state,"state":state}));
    }
    Ok(context)
}
