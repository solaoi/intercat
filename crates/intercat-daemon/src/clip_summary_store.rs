use crate::{
    Result,
    clip_summaries::{Finish, Phase, Summary, Ticket, View},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const THINKING_QUERY: &str = "SELECT c.id,v.version,c.record FROM timeline_clips c
        JOIN timeline_versions v ON v.id=c.id LEFT JOIN thinking_summaries s ON s.clip=c.id
        WHERE v.deleted=0 AND json_extract(c.record,'$.category')='step'
        AND json_extract(c.record,'$.end') IS NULL AND json_extract(c.record,'$.generationEnd') IS NULL
        AND EXISTS(SELECT 1 FROM timeline_observers o WHERE o.track=c.track
            AND o.repair IS json_extract(c.record,'$.repairRunId') AND o.expires>?1)
        AND EXISTS(SELECT 1 FROM timeline_items i WHERE i.clip=c.id AND json_extract(i.record,'$.kind')<>'userMessage'
            AND length(json_extract(i.record,'$.text'))>0)
        AND (s.clip IS NULL OR (s.state<>'running' AND (
            ((s.version<>v.version OR s.state='stale') AND s.started<=?1-5000)
            OR (s.version=v.version AND s.state='pending' AND s.attempts<2))))
        ORDER BY coalesce(s.started,0),v.version DESC LIMIT 1";

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS clip_summaries(
        clip TEXT PRIMARY KEY REFERENCES timeline_clips(id) ON DELETE CASCADE,
        version INTEGER NOT NULL, request TEXT NOT NULL, attempts INTEGER NOT NULL,
        state TEXT NOT NULL, result TEXT);
        UPDATE clip_summaries SET state='pending' WHERE state='running';
        CREATE INDEX IF NOT EXISTS timeline_thinking ON timeline_clips(track)
            WHERE json_extract(record,'$.category')='step'
            AND json_extract(record,'$.end') IS NULL AND json_extract(record,'$.generationEnd') IS NULL;
        CREATE INDEX IF NOT EXISTS clip_summary_shell ON timeline_clips(
            json_extract(record,'$.sessionId'),json_extract(record,'$.shellItem'));
        CREATE TRIGGER IF NOT EXISTS clip_summary_shell_insert AFTER INSERT ON timeline_items BEGIN
            UPDATE timeline_clips SET record=record WHERE json_extract(record,'$.shellItem')=NEW.item
                AND json_extract(record,'$.sessionId')=NEW.session;
        END;
        CREATE TRIGGER IF NOT EXISTS clip_summary_shell_update AFTER UPDATE ON timeline_items BEGIN
            UPDATE timeline_clips SET record=record WHERE json_extract(record,'$.shellItem')=NEW.item
                AND json_extract(record,'$.sessionId')=NEW.session;
        END;
        CREATE TRIGGER IF NOT EXISTS clip_summary_shell_delete AFTER DELETE ON timeline_items BEGIN
            UPDATE timeline_clips SET record=record WHERE json_extract(record,'$.shellItem')=OLD.item
                AND json_extract(record,'$.sessionId')=OLD.session;
        END;",
    )?;
    db.execute_batch(
        "CREATE TEMP TABLE timeline_observers(
            owner TEXT PRIMARY KEY, revision INTEGER NOT NULL,
            track TEXT, repair TEXT, expires INTEGER NOT NULL);
        CREATE TEMP TABLE thinking_summaries(
            clip TEXT PRIMARY KEY, version INTEGER NOT NULL, request TEXT NOT NULL,
            attempts INTEGER NOT NULL, state TEXT NOT NULL, result TEXT,
            context TEXT NOT NULL, started INTEGER NOT NULL, published_context TEXT);
        CREATE TEMP TABLE closed_summary_scan(revision INTEGER NOT NULL,checked INTEGER NOT NULL);
        INSERT INTO closed_summary_scan VALUES(0,-1);
        CREATE TEMP TRIGGER closed_summary_insert AFTER INSERT ON main.timeline_clips
            WHEN json_extract(NEW.record,'$.end') IS NOT NULL BEGIN
            UPDATE closed_summary_scan SET revision=revision+1;
        END;
        CREATE TEMP TRIGGER closed_summary_update AFTER UPDATE OF record ON main.timeline_clips
            WHEN json_extract(NEW.record,'$.end') IS NOT NULL OR json_extract(OLD.record,'$.end') IS NOT NULL BEGIN
            UPDATE closed_summary_scan SET revision=revision+1;
        END;
        CREATE TEMP TRIGGER closed_summary_delete AFTER DELETE ON main.timeline_clips
            WHEN json_extract(OLD.record,'$.end') IS NOT NULL BEGIN
            UPDATE closed_summary_scan SET revision=revision+1;
        END;",
    )?;
    Ok(())
}

pub fn observe(
    db: &Connection,
    owner: uuid::Uuid,
    revision: i64,
    view: Option<View>,
) -> Result<()> {
    if revision < 1 {
        return Err("Invalid observation revision.".into());
    }
    db.execute(
        "INSERT INTO timeline_observers VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(owner) DO UPDATE SET revision=excluded.revision,
         track=excluded.track,repair=excluded.repair,expires=excluded.expires
         WHERE excluded.revision>timeline_observers.revision",
        params![
            owner.to_string(),
            revision,
            view.as_ref().map(|view| &view.track_id),
            view.as_ref().and_then(|view| view.repair_run_id.as_ref()),
            crate::clock::Stamp::now().mono_ms + 15_000
        ],
    )?;
    Ok(())
}

fn context(db: &Connection, clip: &str, record: &Value, phase: &Phase) -> Result<Value> {
    let records = db
        .prepare("SELECT record FROM timeline_items WHERE clip=?1 OR (session=?2 AND item=?3) ORDER BY rowid")?
        .query_map(params![clip, record["sessionId"].as_str(), record["shellItem"].as_str()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut items = Vec::new();
    for record in records {
        let item: Value = serde_json::from_str(&record)?;
        items.push(json!({"itemId":item["itemId"],"kind":item["kind"],"status":item["status"],"tool":item["tool"],
            "text":item["text"],"arguments":item["args"],"failureReason":item["failureReason"],
            "visibleOutput":item["visibleOutput"]}));
    }
    Ok(
        json!({"phase":match phase {Phase::Thinking=>"thinking",Phase::Closed=>"closed"},"category":record["category"],"items":items}),
    )
}

fn extends(previous: &Value, current: &Value) -> bool {
    let (Some(previous), Some(current)) =
        (previous["items"].as_array(), current["items"].as_array())
    else {
        return false;
    };
    previous.len() == current.len()
        && previous.iter().zip(current).all(|(before, after)| {
            let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
                return false;
            };
            before.iter().all(|(key, value)| {
                if matches!(key.as_str(), "text" | "visibleOutput") {
                    match (value.as_str(), after.get(key).and_then(Value::as_str)) {
                        (Some(before), Some(after)) => after.starts_with(before),
                        _ => after.get(key) == Some(value),
                    }
                } else {
                    after.get(key) == Some(value)
                }
            })
        })
}

pub fn current(db: &Connection, ticket: &Ticket) -> Result<bool> {
    if matches!(ticket.phase, Phase::Thinking) {
        let record: Option<String> = db.query_row(
            "SELECT c.record FROM timeline_clips c JOIN thinking_summaries s ON s.clip=c.id
            WHERE c.id=?1 AND s.request=?2 AND s.state='running'
            AND json_extract(c.record,'$.end') IS NULL AND json_extract(c.record,'$.generationEnd') IS NULL
            AND EXISTS(SELECT 1 FROM timeline_observers o WHERE o.track=c.track
                AND o.repair IS json_extract(c.record,'$.repairRunId') AND o.expires>?3)",
            params![ticket.clip,ticket.request.input,crate::clock::Stamp::now().mono_ms],|row|row.get(0)
        ).optional()?;
        return record
            .map(|record| -> Result<bool> {
                Ok(extends(
                    &ticket.context,
                    &context(
                        db,
                        &ticket.clip,
                        &serde_json::from_str(&record)?,
                        &ticket.phase,
                    )?,
                ))
            })
            .transpose()
            .map(|valid| valid.unwrap_or(false));
    }
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM clip_summaries s
        JOIN timeline_versions v ON v.id=s.clip JOIN timeline_clips c ON c.id=s.clip
        WHERE s.clip=?1 AND s.request=?2 AND s.state='running' AND v.version=?3
        AND v.deleted=0 AND json_extract(c.record,'$.end') IS NOT NULL)",
        params![ticket.clip, ticket.request.input, ticket.version],
        |row| row.get(0),
    )?)
}

pub fn claim(db: &mut Connection) -> Result<Option<Ticket>> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    if let Some(ticket) = claim_thinking(&tx)? {
        tx.commit()?;
        return Ok(Some(ticket));
    }
    let (revision, checked): (i64, i64) = tx.query_row(
        "SELECT revision,checked FROM closed_summary_scan",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if revision == checked {
        return Ok(None);
    }
    let candidate: Option<(String, i64, String)> = tx
        .query_row(
            "SELECT c.id,v.version,c.record
        FROM timeline_clips c JOIN timeline_versions v ON v.id=c.id
        LEFT JOIN clip_summaries s ON s.clip=c.id
        WHERE v.deleted=0 AND json_extract(c.record,'$.end') IS NOT NULL
        AND json_extract(c.record,'$.category')<>'snapshot'
        AND (s.clip IS NULL OR s.version<>v.version OR (s.state='pending' AND s.attempts<2))
        ORDER BY v.version DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((clip, version, record)) = candidate else {
        tx.execute("UPDATE closed_summary_scan SET checked=revision", [])?;
        tx.commit()?;
        return Ok(None);
    };
    let record: Value = serde_json::from_str(&record)?;
    let context = context(&tx, &clip, &record, &Phase::Closed)?;
    let request = crate::input_readings::Ticket {
        input: uuid::Uuid::now_v7().to_string(),
        revision: 0,
        index: 1,
    };
    tx.execute("INSERT INTO clip_summaries VALUES(?1,?2,?3,1,'running',NULL)
        ON CONFLICT(clip) DO UPDATE SET version=excluded.version,request=excluded.request,
        attempts=CASE WHEN version=excluded.version THEN attempts+1 ELSE 1 END,state='running',result=NULL",
        params![clip,version,request.input])?;
    tx.commit()?;
    Ok(Some(Ticket {
        clip,
        version,
        context,
        phase: Phase::Closed,
        request,
    }))
}

fn claim_thinking(db: &Connection) -> Result<Option<Ticket>> {
    let now = crate::clock::Stamp::now().mono_ms;
    let candidate: Option<(String, i64, String)> = db
        .query_row(THINKING_QUERY, [now], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()?;
    let Some((clip, version, record)) = candidate else {
        return Ok(None);
    };
    let context = context(db, &clip, &serde_json::from_str(&record)?, &Phase::Thinking)?;
    let request = crate::input_readings::Ticket {
        input: uuid::Uuid::now_v7().to_string(),
        revision: 0,
        index: 1,
    };
    db.execute("INSERT INTO thinking_summaries VALUES(?1,?2,?3,1,'running',NULL,?4,?5,NULL)
        ON CONFLICT(clip) DO UPDATE SET version=excluded.version,request=excluded.request,
        attempts=CASE WHEN version=excluded.version AND state<>'stale' THEN attempts+1 ELSE 1 END,state='running',
        context=excluded.context,started=excluded.started",params![clip,version,request.input,context.to_string(),now])?;
    Ok(Some(Ticket {
        clip,
        version,
        context,
        phase: Phase::Thinking,
        request,
    }))
}

pub fn finish(db: &mut Connection, ticket: &Ticket, outcome: Finish) -> Result<bool> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let valid = current(&tx, ticket)?;
    let mut version = ticket.version;
    let (state, result, interrupted) = match outcome {
        _ if !valid => ("stale", None, false),
        Finish::Complete(summary) => {
            let result = serde_json::to_string(&summary)?;
            Summary::parse(&result)?;
            let captured_version: i64 = tx.query_row(
                "SELECT version FROM timeline_versions WHERE id=?1",
                [&ticket.clip],
                |row| row.get(0),
            )?;
            tx.execute(
                "UPDATE timeline_clips SET record=record WHERE id=?1",
                [&ticket.clip],
            )?;
            version = tx.query_row(
                "SELECT version FROM timeline_versions WHERE id=?1",
                [&ticket.clip],
                |row| row.get(0),
            )?;
            if matches!(ticket.phase, Phase::Thinking) && captured_version != ticket.version {
                version = ticket.version;
            }
            ("complete", Some(result), false)
        }
        Finish::Interrupted => ("pending", None, true),
        Finish::Failed => ("pending", None, false),
    };
    match ticket.phase {
        Phase::Thinking => tx.execute(
            "UPDATE thinking_summaries SET state=?3,result=coalesce(?4,result),attempts=attempts-?5,version=?6,
            published_context=CASE WHEN ?4 IS NOT NULL THEN ?7 ELSE published_context END
            WHERE clip=?1 AND request=?2 AND state='running'",
            params![ticket.clip,ticket.request.input,state,result,i64::from(interrupted),version,ticket.context.to_string()],
        )?,
        Phase::Closed => tx.execute(
            "UPDATE clip_summaries SET state=?3,result=?4,attempts=attempts-?5,version=?6
            WHERE clip=?1 AND request=?2 AND state='running'",
            params![ticket.clip,ticket.request.input,state,result,i64::from(interrupted),version],
        )?,
    };
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'clip.summary',?2)",
        params![
            crate::journal::timestamp(),
            json!({"clip":ticket.clip,"version":ticket.version,"request":ticket.request.input,
                "phase":match ticket.phase { Phase::Thinking => "thinking", Phase::Closed => "closed" },
                "state":state}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(true)
}

pub fn read(db: &Connection, clip: &str) -> Result<Option<Summary>> {
    let value: Option<String> = db
        .query_row(
            "SELECT s.result FROM clip_summaries s
        JOIN timeline_versions v ON v.id=s.clip
        WHERE s.clip=?1 AND s.state='complete' AND s.version=v.version AND v.deleted=0",
            [clip],
            |row| row.get(0),
        )
        .optional()?;
    if value.is_some() {
        return value.map(|text| Summary::parse(&text)).transpose();
    }
    let value: Option<(String,String,String)> = db.query_row(
        "SELECT s.result,s.published_context,c.record FROM thinking_summaries s JOIN timeline_clips c ON c.id=s.clip
        WHERE s.clip=?1 AND s.result IS NOT NULL AND json_extract(c.record,'$.end') IS NULL
        AND json_extract(c.record,'$.generationEnd') IS NULL",[clip],
        |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
    let Some((result, previous, record)) = value else {
        return Ok(None);
    };
    if !extends(
        &serde_json::from_str(&previous)?,
        &context(db, clip, &serde_json::from_str(&record)?, &Phase::Thinking)?,
    ) {
        return Ok(None);
    };
    Ok(Some(Summary::parse(&result)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_candidates_scan_only_the_live_partial_index() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE timeline_clips(id TEXT PRIMARY KEY,track TEXT,ordinal INTEGER,record TEXT,UNIQUE(track,ordinal));
            CREATE TABLE timeline_versions(id TEXT PRIMARY KEY,track TEXT,version INTEGER,deleted INTEGER);
            CREATE TABLE timeline_items(session TEXT,item TEXT,clip TEXT,record TEXT);
            CREATE INDEX timeline_item_clip ON timeline_items(clip);")?;
        install(&db)?;
        let plan = db
            .prepare(&format!("EXPLAIN QUERY PLAN {THINKING_QUERY}"))?
            .query_map([0], |row| row.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert!(
            plan.iter()
                .any(|line| line.contains("USING INDEX timeline_thinking")),
            "{plan:?}"
        );
        assert!(!plan.iter().any(|line| line == "SCAN v"), "{plan:?}");
        let scan_revision = || {
            db.query_row("SELECT revision FROM closed_summary_scan", [], |row| {
                row.get::<_, i64>(0)
            })
        };
        db.execute(
            "INSERT INTO timeline_clips VALUES('live','track',1,?1)",
            [json!({"category":"step","end":null,"generationEnd":null}).to_string()],
        )?;
        assert_eq!(scan_revision()?, 0);
        db.execute(
            "UPDATE timeline_clips SET record=json_set(record,'$.generationEnd',1) WHERE id='live'",
            [],
        )?;
        assert_eq!(scan_revision()?, 0);
        db.execute(
            "UPDATE timeline_clips SET record=json_set(record,'$.end',1) WHERE id='live'",
            [],
        )?;
        assert_eq!(scan_revision()?, 1);
        db.execute("DELETE FROM timeline_clips WHERE id='live'", [])?;
        assert_eq!(scan_revision()?, 2);
        Ok(())
    }
}
