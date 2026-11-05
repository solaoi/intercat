//! External actions are derived from policy, host permission and execution facts.
//! Neither a model report nor an approval by itself proves that an action ran.
use crate::{Result, policy::Impact};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Effect {
    pub id: Uuid,
    pub track_id: String,
    pub session_id: String,
    pub item_id: String,
    pub turn_id: String,
    pub clip_id: Option<String>,
    pub kind: String,
    pub target: String,
    pub rule_id: String,
    pub outcome: String,
    pub running: bool,
    pub at: i64,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Gate {
    track: String,
    session: String,
    approval: String,
    requirement: Value,
    item: String,
    turn: String,
    impact: Option<Impact>,
    permission: String,
    sealed: bool,
    at: i64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Item {
    revision: u64,
    started: bool,
    at: i64,
    turn: String,
    status: String,
    failure: bool,
    interrupted: bool,
}

fn read<T: serde::de::DeserializeOwned>(
    db: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> Result<Option<T>> {
    db.query_row(sql, args, |r| r.get::<_, String>(0))
        .optional()?
        .map(|s| serde_json::from_str(&s).map_err(Into::into))
        .transpose()
}
fn gate_id(session: &str, approval: &str, requirement: &Value) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("effect/{session}/{approval}/{requirement}").as_bytes(),
    )
}
fn save_gate(db: &Connection, id: Uuid, gate: &Gate) -> Result<()> {
    db.execute("INSERT INTO effect_gates VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![id.to_string(),gate.session,gate.approval,gate.item,serde_json::to_string(gate)?])?;
    Ok(())
}
fn gates(db: &Connection, session: &str, item: &str) -> Result<Vec<(Uuid, Gate)>> {
    let mut query = db.prepare(
        "SELECT id,record FROM effect_gates WHERE session=?1 AND item=?2 ORDER BY rowid",
    )?;
    let rows = query
        .query_map(params![session, item], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(id, record)| Ok((id.parse()?, serde_json::from_str(&record)?)))
        .collect()
}
/// Runs inside the policy transaction, before any approval command is sent.
pub fn register(db: &Transaction<'_>, record: &Value) -> Result<()> {
    let session = record["sessionId"]
        .as_str()
        .ok_or("Effect session is missing")?;
    let approval = record["approvalId"]
        .as_str()
        .ok_or("Effect approval is missing")?;
    let id = gate_id(session, approval, &record["requirement"]);
    let old: Option<Gate> = read(
        db,
        "SELECT record FROM effect_gates WHERE id=?1",
        [id.to_string()],
    )?;
    if old.as_ref().is_some_and(|g| g.permission != "pending") {
        return Ok(());
    }
    let gate = Gate {
        track: record["trackId"].as_str().unwrap_or("").into(),
        session: session.into(),
        approval: approval.into(),
        requirement: record["requirement"].clone(),
        item: record["itemId"].as_str().unwrap_or("").into(),
        turn: record["turnId"].as_str().unwrap_or("").into(),
        impact: serde_json::from_value(record["verdict"]["impact"].clone())?,
        permission: "pending".into(),
        sealed: false,
        at: record["at"].as_i64().ok_or("Policy time is missing")?,
    };
    save_gate(db, id, &gate)?;
    reconcile(db, session, &gate.item)
}
fn granted(value: &str) -> bool {
    matches!(
        value,
        "approved" | "approvedForSession" | "approvedPolicyAmendment"
    )
}
fn denied(value: &str) -> bool {
    matches!(
        value,
        "denied" | "deniedPolicyAmendment" | "abort" | "timedOut"
    )
}
fn decide(
    db: &Connection,
    session: &str,
    approval: &str,
    requirement: Option<&Value>,
    permission: &str,
    sealed: bool,
    at: i64,
) -> Result<()> {
    let mut query =
        db.prepare("SELECT id,record FROM effect_gates WHERE session=?1 AND approval=?2")?;
    let rows = query
        .query_map(params![session, approval], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, record) in rows {
        let mut gate: Gate = serde_json::from_str(&record)?;
        if gate.sealed || requirement.is_some_and(|r| *r != gate.requirement) {
            continue;
        }
        gate.permission = permission.into();
        gate.sealed = sealed;
        gate.at = at;
        save_gate(db, id.parse()?, &gate)?;
        reconcile(db, session, &gate.item)?;
    }
    Ok(())
}
/// Fold only primary-session frames, in the transaction that stores the frame.
pub fn fold(db: &Transaction<'_>, primary: &str, at: i64, frame: &Value) -> Result<()> {
    let p = &frame["params"];
    let Some(session) = p["sessionId"].as_str().filter(|s| *s == primary) else {
        return Ok(());
    };
    let method = frame["method"].as_str().unwrap_or("");
    if method == "approval/updated" && p["change"]["kind"] == "stageResolved" {
        if let (Some(approval), Some(decision), Some(requirement)) = (
            p["approvalId"].as_str(),
            p["change"]["decision"].as_str(),
            p["change"].get("requirementId"),
        ) {
            let permission = if granted(decision) {
                "allowed"
            } else if denied(decision) {
                "denied"
            } else {
                "pending"
            };
            decide(
                db,
                session,
                approval,
                Some(requirement),
                permission,
                false,
                at,
            )?;
        }
    } else if method == "approval/resolved" {
        if let Some(approval) = p["approvalId"].as_str() {
            if let Some(stages) = p["stageEvidence"].as_array().filter(|s| !s.is_empty()) {
                for stage in stages {
                    let permission = match stage["resolution"]["kind"].as_str() {
                        Some("allowOnce" | "policyAllow" | "knownSafe" | "policyAmendment") => {
                            "allowed"
                        }
                        _ if p["policyResult"] == "deny" => "denied",
                        _ => "pending",
                    };
                    decide(
                        db,
                        session,
                        approval,
                        stage.get("requirementId"),
                        permission,
                        true,
                        at,
                    )?;
                }
            } else {
                let permission = match p["policyResult"].as_str() {
                    Some("allow") => "allowed",
                    Some("deny") => "denied",
                    _ => "pending",
                };
                decide(db, session, approval, None, permission, true, at)?;
            }
        }
    } else if matches!(method, "item/started" | "item/updated" | "item/completed")
        && p["item"]["kind"] == "toolCall"
    {
        let i = &p["item"];
        let Some(id) = i["itemId"].as_str() else {
            return Ok(());
        };
        let Some(revision) = i["revision"].as_u64().filter(|r| *r > 0) else {
            return Ok(());
        };
        let old: Item = read(
            db,
            "SELECT record FROM effect_items WHERE session=?1 AND item=?2",
            params![session, id],
        )?
        .unwrap_or_default();
        if revision <= old.revision {
            return Ok(());
        }
        let status = i["status"].as_str().unwrap_or("unknown").to_owned();
        let started = old.started || method == "item/started" || status == "completed";
        let item = Item {
            revision,
            started,
            at: if old.started { old.at } else { at },
            turn: i["turnId"].as_str().unwrap_or(&old.turn).into(),
            status,
            failure: i["failureKind"].as_str().is_some_and(|s| !s.is_empty())
                || i["failureReason"].as_str().is_some_and(|s| !s.is_empty()),
            interrupted: false,
        };
        db.execute("INSERT INTO effect_items VALUES(?1,?2,?3) ON CONFLICT(session,item) DO UPDATE SET record=excluded.record",params![session,id,serde_json::to_string(&item)?])?;
        reconcile(db, session, id)?;
    }
    Ok(())
}
fn reconcile(db: &Connection, session: &str, item_id: &str) -> Result<()> {
    let Some(item) = read::<Item>(
        db,
        "SELECT record FROM effect_items WHERE session=?1 AND item=?2",
        params![session, item_id],
    )?
    else {
        return Ok(());
    };
    if !item.started {
        return Ok(());
    }
    let gates = gates(db, session, item_id)?;
    let blocked = gates.iter().any(|(_, g)| g.permission == "denied");
    let terminal = matches!(
        item.status.as_str(),
        "completed" | "failed" | "rejected" | "cancelled"
    );
    let running = !terminal && !item.interrupted;
    let (outcome, reason) = if item.interrupted && !terminal {
        (
            "unknown",
            Some("Connection ended before a terminal result.".into()),
        )
    } else if terminal && (blocked || item.failure || item.status != "completed") {
        (
            "failed",
            Some(
                if blocked {
                    "A policy gate denied part of this action."
                } else {
                    "The worker reported a failed or interrupted action."
                }
                .into(),
            ),
        )
    } else if terminal {
        ("done", None)
    } else {
        ("unknown", None)
    };
    let clip_id: Option<String> = db
        .query_row(
            "SELECT clip FROM timeline_items WHERE session=?1 AND item=?2",
            params![session, item_id],
            |r| r.get(0),
        )
        .optional()?;
    for (id, gate) in &gates {
        let Some(impact) = &gate.impact else { continue };
        let old: Option<Effect> = read(
            db,
            "SELECT record FROM effects WHERE id=?1",
            [id.to_string()],
        )?;
        if gate.permission != "allowed" && old.is_none() {
            continue;
        }
        let record = Effect {
            id: *id,
            track_id: gate.track.clone(),
            session_id: session.into(),
            item_id: item_id.into(),
            turn_id: if item.turn.is_empty() {
                gate.turn.clone()
            } else {
                item.turn.clone()
            },
            clip_id: clip_id.clone(),
            kind: impact.kind.clone(),
            target: impact.target.clone(),
            rule_id: impact.rule_id.clone(),
            outcome: outcome.into(),
            running,
            at: old.as_ref().map(|e| e.at).unwrap_or(item.at.max(gate.at)),
            reason: reason.clone(),
        };
        let serialized = serde_json::to_string(&record)?;
        if old
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
            .as_ref()
            == Some(&serialized)
        {
            continue;
        }
        db.execute("INSERT INTO effects VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET clip=excluded.clip,record=excluded.record",params![id.to_string(),gate.track,session,item_id,clip_id,serialized])?;
    }
    Ok(())
}
pub fn list(db: &Connection, track: Option<&str>, item: Option<&str>) -> Result<Vec<Effect>> {
    let mut query=db.prepare("SELECT record FROM effects WHERE (?1 IS NULL OR track=?1) AND (?2 IS NULL OR item=?2) ORDER BY rowid DESC LIMIT 200")?;
    query
        .query_map(params![track, item], |r| r.get::<_, String>(0))?
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect()
}
fn mark_interrupted(db: &Connection, session: Option<&str>) -> Result<usize> {
    let mut query=db.prepare("SELECT session,item,record FROM effect_items WHERE (?1 IS NULL OR session=?1) AND json_extract(record,'$.started')=1 AND json_extract(record,'$.interrupted')=0 AND json_extract(record,'$.status') NOT IN ('completed','failed','rejected','cancelled')")?;
    let rows = query
        .query_map([session], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(query);
    let count = rows.len();
    for (session, item, record) in rows {
        let mut state: Item = serde_json::from_str(&record)?;
        state.interrupted = true;
        db.execute(
            "UPDATE effect_items SET record=?3 WHERE session=?1 AND item=?2",
            params![session, item, serde_json::to_string(&state)?],
        )?;
        reconcile(db, &session, &item)?;
    }
    Ok(count)
}
pub fn interrupt(db: &mut Connection, session: Option<&str>) -> Result<()> {
    let tx = db.transaction()?;
    if mark_interrupted(&tx, session)? > 0 {
        tx.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(?1,'effects.interrupted',?2)",
            params![
                crate::journal::timestamp(),
                json!({"sessionId":session}).to_string()
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}
/// Regenerate only derived state. Original events and policy decisions stay intact.
pub fn rebuild(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let mut query=tx.prepare("SELECT kind,session_id,ts,payload FROM events WHERE kind IN ('policy.evaluated','msp.applied','effects.interrupted') ORDER BY seq")?;
    let rows = query
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(query);
    tx.execute_batch("DELETE FROM effects;DELETE FROM effect_items;DELETE FROM effect_gates;")?;
    for (kind, session, at, payload) in rows {
        let value: Value = serde_json::from_str(&payload)?;
        match kind.as_str() {
            "policy.evaluated" => register(&tx, &value)?,
            "msp.applied" => {
                if let Some(session) = session {
                    fold(&tx, &session, at, &value)?;
                }
            }
            "effects.interrupted" => {
                mark_interrupted(&tx, value["sessionId"].as_str())?;
            }
            _ => unreachable!(),
        }
    }
    tx.commit()?;
    Ok(())
}
