//! Confirmation stays attached to the interpretation the user actually saw.
use crate::{
    Result,
    decision::{Decision, Kind, Outcome},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    #[default]
    Automatic,
    Always,
    Off,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub revision: u64,
    pub policy: Policy,
    pub active: bool,
    pub dismissed_through: i64,
    pub suggested: bool,
    pub suggested_for: Option<String>,
    pub notice: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Rates {
    pub requests: usize,
    pub explicit: usize,
    pub inferred: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    #[serde(flatten)]
    pub state: State,
    pub rates: Rates,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Action {
    Policy { policy: Policy },
    BackToAutomatic,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Change {
    pub op_id: Uuid,
    pub revision: u64,
    pub action: Action,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Accept {
    pub op_id: Uuid,
    pub utterance_id: Uuid,
    pub revision: i64,
    pub decision_id: Uuid,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS input_confirmations(
        input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,
        decision_id TEXT NOT NULL, meaning TEXT NOT NULL, turn TEXT, approved_revision INTEGER);
        CREATE INDEX IF NOT EXISTS confirmation_recent_inputs ON utterances(created_at DESC);",
    )?;
    Ok(())
}

fn state(db: &Connection) -> Result<State> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='confirmation'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?
        .unwrap_or_default())
}

fn save(db: &Connection, state: &State) -> Result<()> {
    if self::state(db)?.active != state.active {
        crate::editor_log::confirmation(
            db,
            state.revision,
            state.active,
            state.policy == Policy::Automatic,
        )?;
    }
    let raw = serde_json::to_string(state)?;
    db.execute("INSERT INTO settings VALUES('confirmation',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&raw])?;
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'confirmation.changed',?2)",
        params![crate::journal::timestamp(), raw],
    )?;
    Ok(())
}

fn rates(db: &Connection) -> Result<Rates> {
    let mut query =
        db.prepare("SELECT id FROM utterances ORDER BY created_at DESC,rowid DESC LIMIT 10")?;
    let inputs = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut rates = Rates {
        requests: inputs.len(),
        explicit: 0,
        inferred: 0,
    };
    for input in inputs {
        if let Some(summary) = crate::input_feedback::summary(db, &input)? {
            rates.explicit += usize::from(summary["explicit"].as_u64().unwrap_or(0) > 0);
            rates.inferred += usize::from(summary["inferred"].as_u64().unwrap_or(0) > 0);
        }
    }
    Ok(rates)
}

pub(crate) fn status(db: &Connection) -> Result<Status> {
    Ok(Status {
        state: state(db)?,
        rates: rates(db)?,
    })
}

fn latest_correction(db: &Connection) -> Result<i64> {
    Ok(db.query_row("SELECT coalesce(max(seq),0) FROM events WHERE kind='utterance.corrected' AND json_extract(payload,'$.kind')='explicit'",[],|row|row.get(0))?)
}

pub(crate) fn observe(db: &Connection, correction: bool, input: &str) -> Result<()> {
    let rates = rates(db)?;
    if rates.requests < 10 {
        return Ok(());
    }
    let mut current = state(db)?;
    if !current.active
        && current.policy == Policy::Automatic
        && rates.explicit > 3
        && latest_correction(db)? > current.dismissed_through
    {
        current.active = true;
        current.suggested = false;
        current.suggested_for = None;
        current.notice = "I’ll ask you to confirm each interpretation before placing it.".into();
    } else if !correction
        && current.active
        && current.policy == Policy::Automatic
        && !current.suggested
        && rates.explicit <= 3
    {
        current.suggested = true;
        current.suggested_for = Some(input.into());
        current.notice = "The recent correction rate is lower. Back to automatic?".into();
    } else {
        return Ok(());
    }
    current.revision += 1;
    save(db, &current)
}

pub(crate) fn change(db: &mut Connection, request: &Change) -> Result<Status> {
    if request.op_id.get_version_num() != 7 {
        return Err("Invalid confirmation setting operation.".into());
    }
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let payload = serde_json::to_string(request)?;
    if replay(&tx, request.op_id, "confirmation.change", &payload)? {
        return status(&tx);
    }
    let mut current = state(&tx)?;
    if current.revision != request.revision {
        return Err("The confirmation setting changed. Refresh it before trying again.".into());
    }
    match request.action {
        Action::BackToAutomatic => {
            current.policy = Policy::Automatic;
            current.active = false;
            current.dismissed_through = latest_correction(&tx)?;
            current.suggested = false;
            current.suggested_for = None;
            current.notice = "I’ll place new interpretations automatically. Requests already waiting for OK still need your confirmation.".into();
        }
        Action::Policy { policy } => {
            if current.active && policy != current.policy {
                return Err("Use Back to automatic to leave confirmation mode.".into());
            }
            current.policy = policy;
            if policy == Policy::Always {
                current.active = true;
                current.suggested = false;
                current.suggested_for = None;
                current.notice =
                    "I’ll ask you to confirm each interpretation before placing it.".into();
            } else {
                current.notice = if policy == Policy::Off {
                    "Automatic confirmation-mode entry is off."
                } else {
                    "I’ll watch for repeated interpretation corrections."
                }
                .into();
            }
        }
    }
    current.revision += 1;
    save(&tx, &current)?;
    receipt(&tx, request.op_id, "confirmation.change", &payload)?;
    tx.commit()?;
    status(db)
}

fn meaning(decision: &Decision) -> String {
    json!({"kind":decision.kind,"trackId":decision.track_id,"boundary":decision.boundary,"command":decision.command}).to_string()
}

pub(crate) fn gate(
    db: &Connection,
    input: &str,
    revision: i64,
    decision: &Decision,
) -> Result<bool> {
    observe(db, false, input)?;
    let previous: Option<(Option<i64>, String)> = db
        .query_row(
            "SELECT approved_revision,meaning FROM input_confirmations WHERE input=?1",
            [input],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let meaning = meaning(decision);
    if previous
        .as_ref()
        .is_some_and(|(approved, saved)| *approved == Some(revision) && *saved == meaning)
    {
        return Ok(false);
    }
    let waiting = previous.is_some_and(|(approved, _)| approved.is_none());
    if !waiting && !state(db)?.active {
        return Ok(false);
    }
    let turn = crate::journal::decision_worker(db, decision)?.and_then(|worker| {
        Some(crate::decision_review::Turn {
            session_id: worker.session_id?,
            turn_id: worker.active_turn?,
        })
    });
    db.execute("INSERT INTO input_confirmations VALUES(?1,?2,?3,?4,NULL)
        ON CONFLICT(input) DO UPDATE SET decision_id=excluded.decision_id,meaning=excluded.meaning,turn=excluded.turn,approved_revision=NULL",
        params![input,Uuid::now_v7().to_string(),meaning,turn.map(|turn|serde_json::to_string(&turn)).transpose()?])?;
    Ok(true)
}

pub(crate) fn pending(db: &Connection, input: &str) -> Result<Option<Value>> {
    let row: Option<(String,String,Option<String>,String)> = db.query_row(
        "SELECT c.decision_id,c.meaning,c.turn,u.decision FROM input_confirmations c JOIN utterances u ON u.id=c.input
         WHERE c.input=?1 AND c.approved_revision IS NULL AND u.state IN ('asked','held') AND u.decision IS NOT NULL",
        [input], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    ).optional()?;
    let Some((id, saved, turn, raw)) = row else {
        return Ok(None);
    };
    let decision: Decision = serde_json::from_str(&raw)?;
    if decision.timed_out
        || saved != meaning(&decision)
        || crate::decision_review::read(&decision)?.is_some()
    {
        return Ok(None);
    }
    Ok(Some(
        json!({"decisionId":id,"turn":turn.map(|raw|serde_json::from_str::<Value>(&raw)).transpose()?}),
    ))
}

pub(crate) fn accept(db: &mut Connection, request: &Accept) -> Result<String> {
    if request.op_id.get_version_num() != 7 || request.revision < 1 {
        return Err("Invalid confirmation.".into());
    }
    let input = request.utterance_id.to_string();
    crate::session_retirement::ensure_input(db, &input)?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let payload = serde_json::to_string(request)?;
    if replay(&tx, request.op_id, "utterance.confirm", &payload)? {
        return Ok(input);
    }
    if crate::edit_hold::active(&tx, &input)? {
        return Err("Finish editing this interpretation before confirming it.".into());
    }
    let (revision, raw, held): (i64, String, bool) = tx.query_row(
        "SELECT revision,decision,state='held' FROM utterances WHERE id=?1",
        [&input],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let shown = pending(&tx, &input)?.ok_or("This interpretation is no longer waiting for OK.")?;
    if revision != request.revision || shown["decisionId"] != request.decision_id.to_string() {
        return Err("The interpretation changed. Review it again before pressing OK.".into());
    }
    let mut decision: Decision = serde_json::from_str(&raw)?;
    let worker = crate::journal::decision_worker(&tx, &decision)?;
    let current = worker.and_then(|worker| {
        Some(crate::decision_review::Turn {
            session_id: worker.session_id?,
            turn_id: worker.active_turn?,
        })
    });
    let captured: Option<crate::decision_review::Turn> =
        serde_json::from_value(shown["turn"].clone())?;
    let stale = decision.kind == Kind::Trim && current != captured;
    if stale {
        crate::decision_review::write(
            &mut decision,
            crate::decision_review::Review {
                stage: crate::decision_review::Stage::StaleTurn,
                turn: captured,
            },
        )?;
    } else {
        tx.execute(
            "UPDATE input_confirmations SET approved_revision=?2 WHERE input=?1",
            params![input, revision + 1],
        )?;
    }
    tx.execute(
        "UPDATE utterances SET revision=revision+1 WHERE id=?1",
        [&input],
    )?;
    crate::journal::apply_outcome(&tx, &input, revision + 1, Outcome::Classified(decision))?;
    if held && !stale {
        tx.execute(
            "UPDATE utterances SET state='held',reason='Held by you' WHERE id=?1",
            [&input],
        )?;
        crate::redirect::input_state(&tx, &input, "held")?;
    }
    if !stale {
        tx.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.confirmed',?2)",params![crate::journal::timestamp(),json!({"inputId":input,"revision":revision,"decisionId":request.decision_id,"decision":serde_json::from_str::<Value>(&raw)?}).to_string()])?;
    }
    receipt(&tx, request.op_id, "utterance.confirm", &payload)?;
    tx.commit()?;
    Ok(input)
}

fn replay(db: &Connection, id: Uuid, kind: &str, payload: &str) -> Result<bool> {
    let previous: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((saved_kind, saved)) = previous {
        if saved_kind != kind || saved != payload {
            return Err("Confirmation operation ID reused.".into());
        }
        return Ok(true);
    }
    Ok(false)
}

fn receipt(db: &Connection, id: Uuid, kind: &str, payload: &str) -> Result<()> {
    db.execute(
        "INSERT INTO operations VALUES(?1,?2,?3,'complete')",
        params![id.to_string(), kind, payload],
    )?;
    Ok(())
}
