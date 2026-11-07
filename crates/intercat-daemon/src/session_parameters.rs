//! Observed model settings, projected with the same session history as execution.
use crate::{Result, reconcile_store::Entry, session_view::SessionView};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Default, Deserialize, Serialize)]
struct Stored {
    model: Option<Value>,
    effort: Option<Value>,
}

impl Stored {
    fn observe(&mut self, session: &str, frame: &Value) -> bool {
        let params = &frame["params"];
        if params["sessionId"] != session {
            return false;
        }
        let slot = match frame["method"].as_str() {
            Some("session/modelChanged") => &mut self.model,
            Some("session/reasoningEffortChanged") => &mut self.effort,
            _ => return false,
        };
        if let Some(cursor) = params.get("viewCursor")
            && slot.as_ref().and_then(|value| value.get("viewCursor")) == Some(cursor)
        {
            return false;
        }
        *slot = Some(params.clone());
        true
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Parameters {
    pub model_id: Option<String>,
    pub provider_id: Option<String>,
    pub reasoning_effort: Option<String>,
}

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS session_parameters(session TEXT PRIMARY KEY,record TEXT NOT NULL);")?;
    let sessions = db.prepare("SELECT DISTINCT session_id FROM events WHERE session_id IS NOT NULL
        AND (kind='history.snapshot' OR (kind='msp.applied' AND json_extract(payload,'$.method') IN ('session/modelChanged','session/reasoningEffortChanged')))
        AND session_id NOT IN (SELECT session FROM session_parameters)")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for session in sessions {
        let mut stored = Stored::default();
        crate::reconcile_store::visit_frames(db, &session, |entry| {
            match entry {
                Entry::Frame(frame, _) => {
                    stored.observe(&session, frame);
                }
                Entry::Snapshot(snapshot, _) => {
                    stored = Stored::default();
                    for (method, params) in snapshot.families(&session) {
                        stored.observe(
                            &session,
                            &serde_json::json!({"method":method,"params":params}),
                        );
                    }
                }
            }
            Ok(())
        })?;
        save(db, &session, &stored)?;
    }
    Ok(())
}

fn load(db: &Connection, session: &str) -> Result<Stored> {
    let value: Option<String> = db
        .query_row(
            "SELECT record FROM session_parameters WHERE session=?1",
            [session],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn save(db: &Connection, session: &str, stored: &Stored) -> Result<()> {
    db.execute("INSERT INTO session_parameters VALUES(?1,?2) ON CONFLICT(session) DO UPDATE SET record=excluded.record", params![session, serde_json::to_string(stored)?])?;
    Ok(())
}

pub fn observe(db: &Connection, session: &str, frame: &Value) -> Result<()> {
    if !matches!(
        frame["method"].as_str(),
        Some("session/modelChanged" | "session/reasoningEffortChanged")
    ) {
        return Ok(());
    }
    let mut stored = load(db, session)?;
    if stored.observe(session, frame) {
        save(db, session, &stored)?;
    }
    Ok(())
}

pub fn replay(db: &Connection, session: &str, sequence: i64) -> Result<()> {
    let value: Option<String> = db.query_row("SELECT payload FROM events WHERE seq=?1 AND kind='msp.applied'
        AND json_extract(payload,'$.method') IN ('session/modelChanged','session/reasoningEffortChanged')", [sequence], |row| row.get(0)).optional()?;
    if let Some(value) = value {
        observe(db, session, &serde_json::from_str(&value)?)?;
    }
    Ok(())
}

pub fn from_view(db: &Connection, session: &str, view: &SessionView) -> Result<()> {
    save(
        db,
        session,
        &Stored {
            model: view.session_state.get("session/modelChanged").cloned(),
            effort: view
                .session_state
                .get("session/reasoningEffortChanged")
                .cloned(),
        },
    )
}

pub fn read(db: &Connection, session: &str, turn: Option<&str>) -> Result<Option<Parameters>> {
    let stored = load(db, session)?;
    // A submitted turn can override the standing session default. Fresh MSP
    // turns use their command ID as their turn ID, so this is a primary-key read.
    let turn_effort = turn
        .map(|turn| {
            db.query_row(
                "SELECT json_extract(payload,'$.reasoningEffort') FROM operations WHERE id=?1 AND kind='turn.start' AND json_extract(payload,'$.sessionId')=?2",
                params![turn, session],
                |row| row.get::<_, Option<String>>(0),
            ).optional().map(Option::flatten)
        })
        .transpose()?;
    let turn_effort = turn_effort.flatten();
    if stored.model.is_none() && stored.effort.is_none() && turn_effort.is_none() {
        return Ok(None);
    }
    let text = |value: &Option<Value>, key: &str| {
        value
            .as_ref()
            .and_then(|value| value[key].as_str())
            .map(str::to_owned)
    };
    Ok(Some(Parameters {
        model_id: text(&stored.model, "modelId"),
        provider_id: text(&stored.model, "providerId"),
        reasoning_effort: turn_effort.or_else(|| text(&stored.effort, "reasoningEffort")),
    }))
}

pub fn project(db: &Connection, snapshot: &mut Value) -> Result<()> {
    for (field, key, turn_key) in [
        ("workers", "sessionId", "activeTurn"),
        ("repairRuns", "session", "turn"),
    ] {
        for worker in snapshot[field].as_array_mut().into_iter().flatten() {
            if let Some(session) = worker[key].as_str() {
                worker["model"] =
                    serde_json::to_value(read(db, session, worker[turn_key].as_str())?)?;
            }
        }
    }
    Ok(())
}
