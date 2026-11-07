//! A statement keeps its original timeline position while its interpretation changes.
use crate::Result;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS input_timeline(
        input TEXT PRIMARY KEY, position INTEGER NOT NULL, version INTEGER NOT NULL, deleted INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS input_timeline_position ON input_timeline(position);
        CREATE INDEX IF NOT EXISTS input_timeline_version ON input_timeline(version);
        CREATE INDEX IF NOT EXISTS input_timeline_origins ON events(kind)
          WHERE kind IN ('utterance.staged','report.responded');
        CREATE TRIGGER IF NOT EXISTS input_timeline_insert AFTER INSERT ON input_timeline BEGIN
          UPDATE timeline_revision SET revision=revision+1;
          UPDATE input_timeline SET version=(SELECT revision FROM timeline_revision) WHERE input=NEW.input;
          UPDATE timeline_marks SET input=input WHERE input=NEW.input;
        END;
        CREATE TRIGGER IF NOT EXISTS input_timeline_origin AFTER INSERT ON events
        WHEN NEW.kind IN ('utterance.staged','report.responded') BEGIN
          INSERT INTO input_timeline
          SELECT id,NEW.position,0,0 FROM utterances
          WHERE id=CASE WHEN NEW.kind='utterance.staged' THEN json_extract(NEW.payload,'$.id') ELSE json_extract(NEW.payload,'$.inputId') END
          ON CONFLICT(input) DO NOTHING;
        END;
        CREATE TRIGGER IF NOT EXISTS input_timeline_change AFTER UPDATE OF text,decision,state,revision ON utterances
        WHEN EXISTS(SELECT 1 FROM input_timeline WHERE input=NEW.id) BEGIN
          UPDATE timeline_revision SET revision=revision+1;
          UPDATE input_timeline SET version=(SELECT revision FROM timeline_revision) WHERE input=NEW.id;
        END;
        CREATE TRIGGER IF NOT EXISTS input_timeline_delete AFTER DELETE ON utterances
        WHEN EXISTS(SELECT 1 FROM input_timeline WHERE input=OLD.id) BEGIN
          UPDATE timeline_revision SET revision=revision+1;
          UPDATE input_timeline SET deleted=1,version=(SELECT revision FROM timeline_revision) WHERE input=OLD.id;
        END;")?;
    // Rebuild only missing positions from recorded creation receipts. A later
    // delivery, rereading or edit never moves the original statement in time.
    db.execute_batch("INSERT INTO input_timeline
        SELECT u.id,min(e.position),0,0 FROM events e JOIN utterances u
        ON u.id=CASE WHEN e.kind='utterance.staged' THEN json_extract(e.payload,'$.id') ELSE json_extract(e.payload,'$.inputId') END
        WHERE e.kind IN ('utterance.staged','report.responded')
        AND NOT EXISTS(SELECT 1 FROM input_timeline p WHERE p.input=u.id)
        GROUP BY u.id;")?;
    Ok(())
}

pub fn window(
    db: &Connection,
    start: i64,
    end: i64,
    since: Option<i64>,
) -> Result<(Vec<Value>, Vec<String>)> {
    let filter = if since.is_some() {
        "p.version>?3"
    } else {
        "p.position BETWEEN ?1 AND ?2"
    };
    let sql = format!("SELECT p.input,p.position,p.deleted,u.state,u.decision,
        coalesce(nullif(u.text,''),json_extract(s.record,'$.preview'),''),s.input IS NOT NULL
        FROM input_timeline p LEFT JOIN utterances u ON u.id=p.input
        LEFT JOIN utterance_sources s ON s.input=p.input WHERE {filter} ORDER BY p.position,p.input");
    let mut query = db.prepare(&sql)?;
    // Numbered parameters stay stable between full and incremental reads.
    let mut rows = if let Some(since) = since {
        query.query(params![rusqlite::types::Null, rusqlite::types::Null, since])?
    } else {
        query.query(params![start, end])?
    };
    let mut marks = Vec::new();
    let mut removed = Vec::new();
    while let Some(row) = rows.next()? {
        let input: String = row.get(0)?;
        let at: i64 = row.get(1)?;
        let state: Option<String> = row.get(3)?;
        let decision: Option<String> = row.get(4)?;
        let decision: Value = decision
            .map(|value| serde_json::from_str(&value))
            .transpose()?
            .unwrap_or(Value::Null);
        let visible = state.as_deref() == Some("landed")
            || state.as_deref() == Some("asked")
            || state.as_deref() == Some("placed")
                && matches!(decision["kind"].as_str(), Some("park" | "plan"));
        let id = format!("input/{input}");
        if row.get::<_, bool>(2)? || !visible || at < start || at > end {
            if since.is_some() {
                removed.push(id);
            }
            continue;
        }
        let operation = if state.as_deref() == Some("asked")
            && !(decision["kind"] == "plan" && decision["timedOut"] == false)
        {
            json!("ask")
        } else {
            decision["kind"].clone()
        };
        let track = if matches!(operation.as_str(), Some("ask" | "park" | "plan")) {
            Value::Null
        } else {
            decision["trackId"].clone()
        };
        let text: String = row.get(5)?;
        let mut characters = text.chars();
        let mut preview: String = characters.by_ref().take(200).collect();
        if characters.next().is_some() {
            preview.push('…');
        }
        marks.push(
            json!({"id":id,"inputId":input,"at":at,"trackId":track,"repairRunId":null,
            "text":preview,"operation":operation,"command":decision["command"],"state":state,
            "attached":row.get::<_,bool>(6)?}),
        );
    }
    Ok((marks, removed))
}
