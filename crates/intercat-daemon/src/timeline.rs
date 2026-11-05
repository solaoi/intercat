//! Transactional projection of received MSP items. Bodies stay out of status snapshots.
use crate::{Result, clock::Stamp};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE timeline_turns(session TEXT NOT NULL,turn TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(session,turn));
    CREATE TABLE timeline_clips(id TEXT PRIMARY KEY,track TEXT NOT NULL,ordinal INTEGER NOT NULL,record TEXT NOT NULL,UNIQUE(track,ordinal));
    CREATE TABLE timeline_items(session TEXT NOT NULL,item TEXT NOT NULL,clip TEXT NOT NULL,revision INTEGER NOT NULL,record TEXT NOT NULL,summary TEXT NOT NULL,PRIMARY KEY(session,item));
    CREATE INDEX timeline_item_clip ON timeline_items(clip);
    CREATE TABLE timeline_marks(session TEXT NOT NULL,item TEXT NOT NULL,track TEXT NOT NULL,position INTEGER NOT NULL,input TEXT,repair TEXT,PRIMARY KEY(session,item));
    CREATE TABLE timeline_outputs(session TEXT NOT NULL,item TEXT NOT NULL,reference TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(session,item,reference));")?;
    db.execute_batch("CREATE TABLE timeline_revision(revision INTEGER NOT NULL);
    INSERT INTO timeline_revision VALUES(0);
    CREATE TABLE timeline_versions(id TEXT PRIMARY KEY,track TEXT NOT NULL,version INTEGER NOT NULL,deleted INTEGER NOT NULL);
    CREATE INDEX timeline_window_versions ON timeline_versions(track,version);
    CREATE INDEX timeline_mark_position ON timeline_marks(position);
    CREATE INDEX timeline_mark_input ON timeline_marks(input);
    CREATE TABLE timeline_mark_versions(session TEXT NOT NULL,item TEXT NOT NULL,version INTEGER NOT NULL,deleted INTEGER NOT NULL,PRIMARY KEY(session,item));
    CREATE INDEX timeline_mark_changes ON timeline_mark_versions(version);
    CREATE TRIGGER timeline_mark_insert AFTER INSERT ON timeline_marks BEGIN
      UPDATE timeline_revision SET revision=revision+1;
      INSERT INTO timeline_mark_versions VALUES(NEW.session,NEW.item,(SELECT revision FROM timeline_revision),0) ON CONFLICT(session,item) DO UPDATE SET version=excluded.version,deleted=0;
    END;
    CREATE TRIGGER timeline_mark_update AFTER UPDATE ON timeline_marks BEGIN
      UPDATE timeline_revision SET revision=revision+1;
      INSERT INTO timeline_mark_versions VALUES(NEW.session,NEW.item,(SELECT revision FROM timeline_revision),0) ON CONFLICT(session,item) DO UPDATE SET version=excluded.version,deleted=0;
    END;
    CREATE TRIGGER timeline_mark_delete AFTER DELETE ON timeline_marks BEGIN
      UPDATE timeline_revision SET revision=revision+1;
      UPDATE timeline_mark_versions SET version=(SELECT revision FROM timeline_revision),deleted=1 WHERE session=OLD.session AND item=OLD.item;
    END;
    CREATE TRIGGER timeline_mark_text AFTER UPDATE OF text ON utterances BEGIN
      UPDATE timeline_marks SET input=input WHERE input=NEW.id;
    END;
    CREATE TRIGGER timeline_clip_insert AFTER INSERT ON timeline_clips BEGIN
      UPDATE timeline_revision SET revision=revision+1;
      INSERT INTO timeline_versions VALUES(NEW.id,NEW.track,(SELECT revision FROM timeline_revision),0) ON CONFLICT(id) DO UPDATE SET track=excluded.track,version=excluded.version,deleted=0;
    END;
    CREATE TRIGGER timeline_clip_update AFTER UPDATE OF record ON timeline_clips BEGIN
      UPDATE timeline_revision SET revision=revision+1;
      INSERT INTO timeline_versions VALUES(NEW.id,NEW.track,(SELECT revision FROM timeline_revision),0) ON CONFLICT(id) DO UPDATE SET track=excluded.track,version=excluded.version,deleted=0;
    END;
    CREATE TRIGGER timeline_clip_delete AFTER DELETE ON timeline_clips BEGIN
      UPDATE timeline_revision SET revision=revision+1;
      UPDATE timeline_versions SET version=(SELECT revision FROM timeline_revision),deleted=1 WHERE id=OLD.id;
    END;
    CREATE TRIGGER timeline_item_insert AFTER INSERT ON timeline_items BEGIN UPDATE timeline_clips SET record=record WHERE id=NEW.clip; END;
    CREATE TRIGGER timeline_item_update AFTER UPDATE ON timeline_items BEGIN UPDATE timeline_clips SET record=record WHERE id=NEW.clip; END;
    CREATE TRIGGER timeline_item_delete AFTER DELETE ON timeline_items BEGIN UPDATE timeline_clips SET record=record WHERE id=OLD.clip; END;
    CREATE TRIGGER timeline_effect_insert AFTER INSERT ON effects BEGIN UPDATE timeline_clips SET record=record WHERE id=NEW.clip OR id IN (SELECT clip FROM timeline_items WHERE session=NEW.session AND item=NEW.item); END;
    CREATE TRIGGER timeline_effect_update AFTER UPDATE ON effects BEGIN UPDATE timeline_clips SET record=record WHERE id IN (OLD.clip,NEW.clip) OR id IN (SELECT clip FROM timeline_items WHERE session=NEW.session AND item=NEW.item); END;
    CREATE TRIGGER timeline_effect_delete AFTER DELETE ON effects BEGIN UPDATE timeline_clips SET record=record WHERE id=OLD.clip OR id IN (SELECT clip FROM timeline_items WHERE session=OLD.session AND item=OLD.item); END;")?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Turn {
    track: String,
    repair_run_id: Option<String>,
    current: Option<String>,
    index: u64,
    terminal: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Clip {
    id: String,
    track_id: String,
    repair_run_id: Option<String>,
    ordinal: i64,
    session_id: String,
    turn_id: Option<String>,
    start: Stamp,
    end: Option<Stamp>,
    generation_end: Option<Stamp>,
    category: String,
    shell_item: Option<String>,
    recovered: bool,
}
fn get_json(db: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<Option<Value>> {
    db.query_row(sql, args, |row| row.get::<_, String>(0))
        .optional()?
        .map(|s| serde_json::from_str(&s).map_err(Into::into))
        .transpose()
}
fn clip(db: &Connection, id: &str) -> Result<Clip> {
    Ok(serde_json::from_value(
        get_json(db, "SELECT record FROM timeline_clips WHERE id=?1", [id])?
            .ok_or("Step is unavailable.")?,
    )?)
}
fn save_clip(db: &Connection, clip: &Clip) -> Result<()> {
    db.execute("INSERT INTO timeline_clips VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![clip.id,clip.track_id,clip.ordinal,serde_json::to_string(clip)?])?;
    Ok(())
}
fn open_step(
    db: &Connection,
    session: &str,
    turn_id: &str,
    turn: &mut Turn,
    at: &Stamp,
) -> Result<String> {
    turn.index += 1;
    let id = Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("{session}/{turn_id}/step/{}", turn.index).as_bytes(),
    )
    .to_string();
    let ordinal: i64 = db.query_row(
        "SELECT coalesce(max(ordinal),0)+1 FROM timeline_clips WHERE track=?1",
        [&turn.track],
        |r| r.get(0),
    )?;
    save_clip(
        db,
        &Clip {
            id: id.clone(),
            track_id: turn.track.clone(),
            repair_run_id: turn.repair_run_id.clone(),
            ordinal,
            session_id: session.into(),
            turn_id: Some(turn_id.into()),
            start: at.clone(),
            end: None,
            generation_end: None,
            category: "step".into(),
            shell_item: None,
            recovered: false,
        },
    )?;
    turn.current = Some(id.clone());
    Ok(id)
}
fn items(db: &Connection, id: &str) -> Result<Vec<Value>> {
    let mut stmt = db.prepare("SELECT summary FROM timeline_items WHERE clip=?1 ORDER BY rowid")?;
    let rows = stmt
        .query_map([id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|s| serde_json::from_str(&s).map_err(Into::into))
        .collect()
}
fn brief(item: &Value) -> Value {
    let args = item["args"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or(Value::Null);
    let mut short = serde_json::Map::new();
    for name in ["path", "command", "query"] {
        if let Some(text) = args[name].as_str() {
            short.insert(
                name.into(),
                json!(text.chars().take(100).collect::<String>()),
            );
        }
    }
    json!({"itemId":item["itemId"],"kind":item["kind"],"status":item["status"],"tool":item["tool"],"args":Value::Object(short).to_string()})
}

fn repair_owner(db: &Connection, session: &str, track: &str) -> Result<Option<String>> {
    let mut statement = db.prepare("SELECT id,json_extract(record,'$.track') FROM repair_runs WHERE json_extract(record,'$.session')=?1")?;
    let rows = statement
        .query_map([session], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    match rows.as_slice() {
        [] => Ok(None),
        [(id, owner)] if owner == track => Ok(Some(id.clone())),
        _ => Err("The repair session has conflicting ownership.".into()),
    }
}
fn terminal(item: &Value) -> bool {
    item["status"].as_str().is_some_and(|s| s != "inProgress")
}

fn save_item(
    db: &Connection,
    session: &str,
    item_id: &str,
    clip: &str,
    revision: i64,
    item: &Value,
) -> Result<()> {
    db.execute("INSERT INTO timeline_items VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session,item) DO UPDATE SET revision=excluded.revision,record=excluded.record,summary=excluded.summary",params![session,item_id,clip,revision,serde_json::to_string(item)?,serde_json::to_string(&brief(item))?])?;
    Ok(())
}

/// Project a recovered view without replaying input delivery or other effects.
pub(crate) fn project_snapshot(
    db: &Transaction<'_>,
    track: &str,
    session: &str,
    view: &crate::session_view::SessionView,
    at: &Stamp,
) -> Result<()> {
    let repair = repair_owner(db, session, track)?;
    for item in &view.items {
        let item_id = item["itemId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("Snapshot item has no identity.")?;
        let revision = item["revision"]
            .as_i64()
            .filter(|r| *r > 0)
            .ok_or("Snapshot item has an invalid revision.")?;
        let stored: Option<(String, String)> = db
            .query_row(
                "SELECT clip,record FROM timeline_items WHERE session=?1 AND item=?2",
                params![session, item_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((_, old)) = &stored
            && serde_json::from_str::<Value>(old)? == *item
        {
            continue;
        }
        let id = if let Some((id, _)) = stored {
            id
        } else {
            let turn = item["turnId"].as_str().filter(|id| !id.is_empty());
            let group = turn
                .map(|id| format!("turn/{id}"))
                .unwrap_or_else(|| "session".into());
            let id = Uuid::new_v5(
                &Uuid::NAMESPACE_OID,
                format!("{session}/snapshot/{group}").as_bytes(),
            )
            .to_string();
            if get_json(db, "SELECT record FROM timeline_clips WHERE id=?1", [&id])?.is_none() {
                let ordinal: i64 = db.query_row(
                    "SELECT coalesce(max(ordinal),0)+1 FROM timeline_clips WHERE track=?1",
                    [track],
                    |r| r.get(0),
                )?;
                save_clip(
                    db,
                    &Clip {
                        id: id.clone(),
                        track_id: track.into(),
                        repair_run_id: repair.clone(),
                        ordinal,
                        session_id: session.into(),
                        turn_id: turn.map(str::to_owned),
                        start: at.clone(),
                        end: Some(at.clone()),
                        generation_end: None,
                        category: "snapshot".into(),
                        shell_item: None,
                        recovered: true,
                    },
                )?;
            }
            id
        };
        let mut current = clip(db, &id)?;
        if current.track_id != track || current.session_id != session {
            return Err("Snapshot item belongs to another timeline.".into());
        }
        if !current.recovered {
            current.recovered = true;
            save_clip(db, &current)?;
        }
        save_item(db, session, item_id, &id, revision, item)?;
        if item["kind"] == "userMessage" {
            let input: Option<String> = db
                .query_row(
                    "SELECT id FROM utterances WHERE dispatch_command=?1",
                    [item["commandId"].as_str().unwrap_or("")],
                    |r| r.get(0),
                )
                .optional()?;
            db.execute("INSERT INTO timeline_marks VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session,item) DO NOTHING",params![session,item_id,track,current.start.timeline_ms,input,repair])?;
        }
    }
    Ok(())
}

fn update_unassociated_snapshot_item(db: &Connection, session: &str, item: &Value) -> Result<()> {
    let (Some(id), Some(revision)) = (item["itemId"].as_str(), item["revision"].as_i64()) else {
        return Ok(());
    };
    let prior:Option<(String,i64)>=db.query_row("SELECT i.clip,i.revision FROM timeline_items i JOIN timeline_clips c ON c.id=i.clip WHERE i.session=?1 AND i.item=?2 AND json_extract(c.record,'$.category')='snapshot'",params![session,id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((clip, old)) = prior
        && revision > old
    {
        save_item(db, session, id, &clip, revision, item)?;
    }
    Ok(())
}

/// Called in the same transaction as the append-only event insert.
pub fn fold(
    db: &Transaction<'_>,
    track: &str,
    primary_session: &str,
    at: &Stamp,
    frame: &Value,
) -> Result<()> {
    let method = frame["method"].as_str().unwrap_or("");
    let p = &frame["params"];
    let Some(session) = p["sessionId"].as_str() else {
        return Ok(());
    };
    if session != primary_session {
        return Ok(());
    }
    let item = &p["item"];
    let Some(turn_id) = p["turnId"].as_str().or_else(|| item["turnId"].as_str()) else {
        if matches!(method, "item/started" | "item/updated" | "item/completed") {
            update_unassociated_snapshot_item(db, session, item)?;
        }
        return Ok(());
    };
    if !matches!(
        method,
        "turn/started" | "turn/completed" | "item/started" | "item/updated" | "item/completed"
    ) {
        return Ok(());
    }
    let old = get_json(
        db,
        "SELECT record FROM timeline_turns WHERE session=?1 AND turn=?2",
        params![session, turn_id],
    )?;
    let repair_run_id = if old.is_none() {
        repair_owner(db, session, track)?
    } else {
        None
    };
    let mut turn: Turn = old
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or(Turn {
            track: track.into(),
            repair_run_id,
            current: None,
            index: 0,
            terminal: false,
        });
    if turn.track != track {
        return Err("MSP session changed tracks.".into());
    }
    if method == "turn/completed" {
        if !turn.terminal {
            turn.terminal = true;
            if let Some(id) = &turn.current {
                let mut current = clip(db, id)?;
                current.recovered |= frame["_intercatRecovered"] == true;
                current.end = Some(at.clone());
                save_clip(db, &current)?;
                // An unfinished shell belongs to its own interval after foreground ends.
                for item in items(db, id)?
                    .iter()
                    .filter(|i| i["kind"] == "toolCall" && i["tool"] == "bash" && !terminal(i))
                {
                    let item_id = item["itemId"].as_str().ok_or("Missing shell item ID.")?;
                    let shell_id = Uuid::new_v5(
                        &Uuid::NAMESPACE_OID,
                        format!("{session}/{item_id}/shell").as_bytes(),
                    )
                    .to_string();
                    let ordinal: i64 = db.query_row(
                        "SELECT coalesce(max(ordinal),0)+1 FROM timeline_clips WHERE track=?1",
                        [track],
                        |r| r.get(0),
                    )?;
                    save_clip(
                        db,
                        &Clip {
                            id: shell_id,
                            track_id: track.into(),
                            repair_run_id: turn.repair_run_id.clone(),
                            ordinal,
                            session_id: session.into(),
                            turn_id: Some(turn_id.into()),
                            start: at.clone(),
                            end: None,
                            generation_end: Some(at.clone()),
                            category: "shell".into(),
                            shell_item: Some(item_id.into()),
                            recovered: frame["_intercatRecovered"] == true,
                        },
                    )?;
                }
            }
        }
    } else if method.starts_with("item/") {
        let Some(item_id) = item["itemId"].as_str() else {
            return Ok(());
        };
        let Some(revision) = item["revision"]
            .as_u64()
            .filter(|v| *v > 0 && *v <= i64::MAX as u64)
        else {
            return Ok(());
        };
        let previous: Option<(String, i64)> = db
            .query_row(
                "SELECT clip,revision FROM timeline_items WHERE session=?1 AND item=?2",
                params![session, item_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if previous
            .as_ref()
            .is_some_and(|(_, old)| *old >= revision as i64)
        {
            return Ok(());
        }
        let id = if let Some((id, _)) = &previous {
            id.clone()
        } else if let Some(id) = &turn.current {
            id.clone()
        } else {
            open_step(db, session, turn_id, &mut turn, at)?
        };
        let mut stored = item.clone();
        if let Some(truncated) = frame.get("_intercatTruncated") {
            stored["_intercatTruncated"] = truncated.clone();
        }
        save_item(db, session, item_id, &id, revision as i64, &stored)?;
        if item["kind"] == "userMessage" {
            let input: Option<String> = db
                .query_row(
                    "SELECT id FROM utterances WHERE dispatch_command=?1",
                    [item["commandId"].as_str().unwrap_or("")],
                    |r| r.get(0),
                )
                .optional()?;
            db.execute(
                "INSERT INTO timeline_marks VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session,item) DO UPDATE SET input=coalesce(timeline_marks.input,excluded.input)",
                params![session, item_id, track, at.timeline_ms, input, turn.repair_run_id],
            )?;
            if let Some(input) = input {
                db.execute("UPDATE utterances SET state='landed' WHERE id=?1 AND state IN ('starting','sent')",[&input])?;
                crate::report_store::input_state(db, &input, "landed")?;
                crate::redirect::input_state(db, &input, "landed")?;
            }
        }
        let mut current = clip(db, &id)?;
        if frame["_intercatRecovered"] == true {
            current.recovered = true;
            save_clip(db, &current)?;
        }
        if item["kind"] == "toolCall" && current.generation_end.is_none() {
            current.generation_end = Some(at.clone());
            save_clip(db, &current)?;
        }
        if terminal(item) {
            let shell_id = Uuid::new_v5(
                &Uuid::NAMESPACE_OID,
                format!("{session}/{item_id}/shell").as_bytes(),
            )
            .to_string();
            if get_json(
                db,
                "SELECT record FROM timeline_clips WHERE id=?1",
                [&shell_id],
            )?
            .is_some()
            {
                let mut shell = clip(db, &shell_id)?;
                if shell.end.is_none() {
                    shell.end = Some(at.clone());
                    save_clip(db, &shell)?;
                }
            }
        }
        if !turn.terminal && current.end.is_none() {
            let values = items(db, &id)?;
            let tools: Vec<_> = values.iter().filter(|i| i["kind"] == "toolCall").collect();
            if !tools.is_empty() && tools.iter().all(|i| terminal(i)) {
                current.end = Some(at.clone());
                save_clip(db, &current)?;
                open_step(db, session, turn_id, &mut turn, at)?;
            }
        }
    }
    db.execute("INSERT INTO timeline_turns VALUES(?1,?2,?3) ON CONFLICT(session,turn) DO UPDATE SET record=excluded.record",params![session,turn_id,serde_json::to_string(&turn)?])?;
    Ok(())
}

pub fn cache_output(
    db: &mut Connection,
    session: &str,
    item: &str,
    reference: &str,
    result: &Value,
) -> Result<()> {
    // The only source is item/readOutput; arbitrary paths from a worker are never opened.
    let tx = db.transaction()?;
    tx.execute("INSERT INTO timeline_outputs VALUES(?1,?2,?3,?4) ON CONFLICT(session,item,reference) DO UPDATE SET record=excluded.record",params![session,item,reference,serde_json::to_string(result)?])?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'timeline.output',?2)",
        params![
            crate::journal::timestamp(),
            serde_json::to_string(
                &json!({"sessionId":session,"itemId":item,"reference":reference})
            )?
        ],
    )?;
    tx.commit()?;
    Ok(())
}

fn summary(db: &Connection, clip: &Clip, now: &Stamp) -> Result<Value> {
    let all = if let Some(item) = &clip.shell_item {
        get_json(
            db,
            "SELECT summary FROM timeline_items WHERE session=?1 AND item=?2",
            params![clip.session_id, item],
        )?
        .into_iter()
        .collect()
    } else {
        items(db, &clip.id)?
    };
    let tools: Vec<_> = all.iter().filter(|i| i["kind"] == "toolCall").collect();
    let main = tools.iter().max_by_key(|i| match i["tool"].as_str() {
        Some("edit_file" | "write_file") => 3,
        Some("bash") => 2,
        _ => 1,
    });
    let kind = match main.and_then(|i| i["tool"].as_str()) {
        Some("edit_file" | "write_file") => "edit",
        Some("bash") => "shell",
        Some(_) => "read",
        None => "think",
    };
    let label = if clip.category == "snapshot" {
        "Recovered history".into()
    } else if let Some(i) = main {
        let tool = i["tool"].as_str().unwrap_or("tool");
        let args = i["args"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(Value::Null);
        let target = args["path"]
            .as_str()
            .or_else(|| args["command"].as_str())
            .or_else(|| args["query"].as_str())
            .unwrap_or("");
        format!("{tool} {}", target.chars().take(100).collect::<String>())
            .trim()
            .to_owned()
    } else if all.iter().any(|i| i["kind"] == "agentMessage") {
        "Reply".into()
    } else {
        "Thinking".into()
    };
    let (duration, approximate) = clip.end.as_ref().unwrap_or(now).elapsed(&clip.start);
    let approximate = approximate || clip.recovered;
    let generation = clip
        .generation_end
        .as_ref()
        .map(|at| clip.start.timeline_ms + at.elapsed(&clip.start).0);
    let effect_count: i64 = if let Some(item) = &clip.shell_item {
        db.query_row(
            "SELECT count(*) FROM effects WHERE session=?1 AND item=?2",
            params![clip.session_id, item],
            |r| r.get(0),
        )?
    } else {
        db.query_row(
            "SELECT count(*) FROM effects WHERE clip=?1",
            [&clip.id],
            |r| r.get(0),
        )?
    };
    Ok(
        json!({"id":clip.id,"trackId":clip.track_id,"repairRunId":clip.repair_run_id,"ordinal":clip.ordinal,"sessionId":clip.session_id,"turnId":clip.turn_id,"category":clip.category,"label":label,"kind":kind,"start":clip.start.timeline_ms,"end":clip.start.timeline_ms+duration,"running":clip.end.is_none(),"generationEnd":generation,"approximate":approximate,"effectCount":effect_count,"toolTicks":tools.iter().map(|i|json!({"itemId":i["itemId"],"tool":i["tool"],"status":i["status"]})).collect::<Vec<_>>()}),
    )
}
pub fn snapshot(db: &Connection) -> Result<Value> {
    let now = Stamp::now();
    let mut stmt=db.prepare("SELECT payload FROM events WHERE kind='history.compacted' AND seq IN (SELECT max(seq) FROM events WHERE kind='history.compacted' GROUP BY session_id) ORDER BY seq")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut summaries = Vec::new();
    for row in rows {
        let mut value: Value = serde_json::from_str(&row)?;
        let session = value["sessionId"]
            .as_str()
            .ok_or("Summary has no session.")?;
        let track = value["trackId"].as_str().ok_or("Summary has no track.")?;
        value["repairRunId"] = json!(repair_owner(db, session, track)?);
        summaries.push(value);
    }
    Ok(json!({"clock":now,"summaries":summaries}))
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Window {
    pub tracks: Vec<String>,
    pub start: i64,
    pub end: i64,
    pub since: Option<i64>,
}
pub fn window(db: &Connection, request: Window) -> Result<Value> {
    if request.tracks.is_empty()
        || request.tracks.len() > 300
        || request
            .tracks
            .iter()
            .any(|id| id.is_empty() || id.len() > 128)
        || request.start < 0
        || request.end < request.start
        || request.since.is_some_and(|value| value < 0)
    {
        return Err("Invalid timeline window.".into());
    }
    let revision: i64 = db.query_row("SELECT revision FROM timeline_revision", [], |r| r.get(0))?;
    let reset = request.since.is_none_or(|since| since > revision);
    let since = if reset { -1 } else { request.since.unwrap() };
    let placeholders = std::iter::repeat_n("?", request.tracks.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT v.id,v.deleted,c.record FROM timeline_versions v LEFT JOIN timeline_clips c ON c.id=v.id WHERE v.version>? AND v.track IN ({placeholders}) ORDER BY c.ordinal,v.id"
    );
    let args = std::iter::once(rusqlite::types::Value::Integer(since))
        .chain(request.tracks.into_iter().map(rusqlite::types::Value::Text));
    let mut query = db.prepare(&sql)?;
    let rows = query
        .query_map(rusqlite::params_from_iter(args), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let now = Stamp::now();
    let mut clips = Vec::new();
    let mut removed = Vec::new();
    for (id, deleted, record) in rows {
        if deleted {
            if !reset {
                removed.push(id);
            }
            continue;
        }
        let clip: Clip = serde_json::from_str(&record.ok_or("Timeline version has no clip.")?)?;
        if clip.start.timeline_ms <= request.end
            && clip
                .end
                .as_ref()
                .is_none_or(|end| end.timeline_ms >= request.start)
        {
            clips.push(summary(db, &clip, &now)?);
        } else if !reset {
            removed.push(id);
        }
    }
    // The You row covers every track, including those below the viewport.
    // Its time window is shared with clips; its history has no last-N cutoff.
    let sql = if reset {
        "SELECT m.session,m.item,0,m.track,m.position,m.input,coalesce(u.text,'Received message'),m.repair FROM timeline_marks m LEFT JOIN utterances u ON u.id=m.input WHERE m.position BETWEEN ?1 AND ?2 ORDER BY m.position,m.session,m.item"
    } else {
        "SELECT v.session,v.item,v.deleted,m.track,m.position,m.input,coalesce(u.text,'Received message'),m.repair FROM timeline_mark_versions v LEFT JOIN timeline_marks m USING(session,item) LEFT JOIN utterances u ON u.id=m.input WHERE v.version>?1 ORDER BY m.position,v.session,v.item"
    };
    let mut query = db.prepare(sql)?;
    let args: &[i64] = if reset {
        &[request.start, request.end]
    } else {
        &[since]
    };
    let mut rows = query.query(rusqlite::params_from_iter(args))?;
    let mut marks = Vec::new();
    let mut removed_marks = Vec::new();
    while let Some(row) = rows.next()? {
        let id = format!("{}/{}", row.get::<_, String>(0)?, row.get::<_, String>(1)?);
        let position: Option<i64> = row.get(4)?;
        if row.get::<_, bool>(2)?
            || position.is_none_or(|at| at < request.start || at > request.end)
        {
            if !reset {
                removed_marks.push(id);
            }
        } else {
            marks.push(json!({"id":id,"trackId":row.get::<_,String>(3)?,"at":position,"inputId":row.get::<_,Option<String>>(5)?,"text":row.get::<_,String>(6)?,"repairRunId":row.get::<_,Option<String>>(7)?}));
        }
    }
    Ok(
        json!({"revision":revision,"reset":reset,"clips":clips,"removed":removed,"marks":marks,"removedMarks":removed_marks}),
    )
}
pub fn detail(db: &Connection, id: &str) -> Result<Value> {
    let clip = clip(db, id)?;
    let mut stmt = db.prepare("SELECT record FROM timeline_items WHERE clip=?1 ORDER BY rowid")?;
    let rows = stmt
        .query_map([&clip.id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut all = rows
        .into_iter()
        .map(|s| serde_json::from_str::<Value>(&s).map_err(Into::into))
        .collect::<Result<Vec<_>>>()?;
    if let Some(item) = &clip.shell_item {
        all = get_json(
            db,
            "SELECT record FROM timeline_items WHERE session=?1 AND item=?2",
            params![clip.session_id, item],
        )?
        .into_iter()
        .collect();
    }
    for item in &mut all {
        let mut outputs = serde_json::Map::new();
        for key in ["patchRef", "outputRef"] {
            if let Some(reference) = item[key]["id"].as_str()
                && let Some(result) = get_json(
                    db,
                    "SELECT record FROM timeline_outputs WHERE session=?1 AND item=?2 AND reference=?3",
                    params![
                        clip.session_id,
                        item["itemId"].as_str().unwrap_or(""),
                        reference
                    ],
                )?
            {
                outputs.insert(key.into(), result);
            }
        }
        item["outputs"] = Value::Object(outputs);
    }
    let mut snapshots = Vec::new();
    let mut effects = Vec::new();
    for item in &all {
        if let Some(id) = item["itemId"].as_str() {
            effects.extend(crate::effects::list(db, Some(&clip.track_id), Some(id))?);
            snapshots.extend(crate::deletion::summaries(
                db,
                Some(&clip.track_id),
                Some(id),
                clip.repair_run_id.as_deref().map(str::parse).transpose()?,
            )?);
        }
    }
    let pending = crate::deletion::pending(db)?;
    let snapshots = snapshots
        .into_iter()
        .map(|snapshot| -> Result<Value> {
            let mut value = serde_json::to_value(&snapshot)?;
            if let Some(operation) = pending
                .iter()
                .find(|p| p["snapshotId"] == snapshot.id.to_string())
            {
                value["restoreOperationId"] = operation["operationId"].clone();
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({"clip":summary(db,&clip,&Stamp::now())?,"items":all,"deletionSnapshots":snapshots,"effects":effects}),
    )
}
