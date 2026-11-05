//! Attachment provenance follows acknowledged turns, not paths found in text.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

const SNAPSHOT_LIMIT: usize = 2 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Turn {
    command: String,
    turn: Option<String>,
    ordinal: i64,
    attachments: Vec<Uuid>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    session: String,
    through: Option<String>,
    turns: Vec<Turn>,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE attachment_turns(session TEXT NOT NULL,command TEXT NOT NULL,turn TEXT,ordinal INTEGER NOT NULL,inherited INTEGER NOT NULL,PRIMARY KEY(session,command),UNIQUE(session,turn),UNIQUE(session,ordinal));
      CREATE TABLE attachment_turn_items(session TEXT NOT NULL,command TEXT NOT NULL,attachment TEXT NOT NULL REFERENCES attachments(id),PRIMARY KEY(session,command,attachment),FOREIGN KEY(session,command) REFERENCES attachment_turns(session,command));
      CREATE TABLE attachment_snapshots(owner TEXT PRIMARY KEY,record TEXT NOT NULL);")?;
    Ok(())
}
pub(crate) fn reserve(db: &Connection, command: &str, session: &str, input: &str) -> Result<()> {
    let ordinal: i64 = db.query_row(
        "SELECT coalesce(max(ordinal),0)+1 FROM attachment_turns WHERE session=?1",
        [session],
        |r| r.get(0),
    )?;
    db.execute(
        "INSERT INTO attachment_turns VALUES(?1,?2,NULL,?3,0)",
        params![session, command, ordinal],
    )?;
    db.execute("INSERT INTO attachment_turn_items SELECT ?1,?2,attachment FROM input_attachments WHERE input=?3",params![session,command,input])?;
    Ok(())
}
pub(crate) fn observe(db: &Connection, session: &str, frame: &Value) -> Result<()> {
    let p = &frame["params"];
    let command = if frame["method"] == "turn/started" {
        p["commandId"].as_str()
    } else if p["item"]["kind"] == "userMessage" {
        p["item"]["commandId"].as_str()
    } else {
        None
    };
    let Some(command) = command else {
        return Ok(());
    };
    let Some(turn) = p["turnId"]
        .as_str()
        .or_else(|| p["item"]["turnId"].as_str())
    else {
        return Ok(());
    };
    let old: Option<Option<String>> = db
        .query_row(
            "SELECT turn FROM attachment_turns WHERE session=?1 AND command=?2",
            params![session, command],
            |r| r.get(0),
        )
        .optional()?;
    let Some(old) = old else {
        return Ok(());
    };
    if old.as_deref().is_some_and(|old| old != turn) {
        return Err("An attachment dispatch changed its turn identity.".into());
    }
    db.execute(
        "UPDATE attachment_turns SET turn=?3 WHERE session=?1 AND command=?2",
        params![session, command, turn],
    )?;
    Ok(())
}
fn items(db: &Connection, session: &str, command: &str) -> Result<Vec<Uuid>> {
    let mut q=db.prepare("SELECT attachment FROM attachment_turn_items WHERE session=?1 AND command=?2 ORDER BY attachment")?;
    q.query_map(params![session, command], |r| r.get::<_, String>(0))?
        .map(|r| Ok(r?.parse()?))
        .collect()
}
pub(crate) fn available(db: &Connection, session: &str) -> Result<Vec<Uuid>> {
    let mut q=db.prepare("SELECT DISTINCT i.attachment FROM attachment_turn_items i JOIN attachment_turns t USING(session,command) WHERE t.session=?1 AND (t.turn IS NOT NULL OR t.inherited=1) ORDER BY i.attachment")?;
    q.query_map([session], |r| r.get::<_, String>(0))?
        .map(|r| Ok(r?.parse()?))
        .collect()
}
pub(crate) fn restorable(db: &Connection, session: &str) -> Result<Vec<Uuid>> {
    let mut q=db.prepare("SELECT DISTINCT i.attachment FROM attachment_turn_items i JOIN attachment_turns t USING(session,command) LEFT JOIN operations o ON o.id=t.command WHERE t.session=?1 AND (t.turn IS NOT NULL OR t.inherited=1 OR o.state IN ('prepared','dispatched','complete')) ORDER BY i.attachment")?;
    q.query_map([session], |r| r.get::<_, String>(0))?
        .map(|r| Ok(r?.parse()?))
        .collect()
}
fn read(db: &Connection, owner: Uuid) -> Result<Snapshot> {
    let raw: String = db.query_row(
        "SELECT record FROM attachment_snapshots WHERE owner=?1",
        [owner.to_string()],
        |r| r.get(0),
    )?;
    if raw.len() > SNAPSHOT_LIMIT {
        return Err("The attachment history exceeds its limit.".into());
    }
    Ok(serde_json::from_str(&raw)?)
}
pub(crate) fn freeze(
    db: &Connection,
    owner: Uuid,
    session: &str,
    through: Option<&str>,
) -> Result<()> {
    let old: Option<String> = db
        .query_row(
            "SELECT record FROM attachment_snapshots WHERE owner=?1",
            [owner.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(old) = old {
        let old: Snapshot = serde_json::from_str(&old)?;
        if old.session != session || old.through.as_deref() != through {
            return Err("Attachment snapshot owner was reused.".into());
        }
        return Ok(());
    }
    let limit = if let Some(turn) = through {
        let ordinal: Option<i64> = db
            .query_row(
                "SELECT ordinal FROM attachment_turns WHERE session=?1 AND turn=?2",
                params![session, turn],
                |r| r.get(0),
            )
            .optional()?;
        match ordinal {
            Some(ordinal) => ordinal,
            None if db.query_row(
                "SELECT count(*) FROM attachment_turn_items WHERE session=?1",
                [session],
                |r| r.get::<_, i64>(0),
            )? == 0 =>
            {
                0
            }
            None => {
                return Err(
                    "The attachment history has no verified boundary for this turn.".into(),
                );
            }
        }
    } else {
        i64::MAX
    };
    let mut q=db.prepare("SELECT t.command,t.turn,t.ordinal,t.inherited,o.state FROM attachment_turns t LEFT JOIN operations o ON o.id=t.command WHERE t.session=?1 AND t.ordinal<=?2 ORDER BY t.ordinal LIMIT 50001")?;
    let rows = q
        .query_map(params![session, limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() > 50000 {
        return Err("Too many attachment history entries to inherit.".into());
    }
    let mut turns = Vec::new();
    for (command, turn, ordinal, inherited, state) in rows {
        let attachments = items(db, session, &command)?;
        if turn.is_none() && !inherited {
            if state.as_deref() == Some("superseded") {
                continue;
            }
            if through.is_some() {
                if !attachments.is_empty() {
                    return Err("An earlier attachment dispatch is still unconfirmed.".into());
                }
                continue;
            }
            if !matches!(
                state.as_deref(),
                Some("prepared" | "dispatched" | "complete")
            ) {
                return Err("Attachment dispatch ownership is unavailable.".into());
            }
        }
        turns.push(Turn {
            command,
            turn,
            ordinal,
            attachments,
        });
    }
    let snapshot = Snapshot {
        session: session.into(),
        through: through.map(str::to_owned),
        turns,
    };
    let raw = serde_json::to_string(&snapshot)?;
    if raw.len() > SNAPSHOT_LIMIT {
        return Err("The attachment history exceeds its limit.".into());
    }
    db.execute(
        "INSERT INTO attachment_snapshots VALUES(?1,?2)",
        params![owner.to_string(), raw],
    )?;
    Ok(())
}
pub(crate) fn frozen_ids(db: &Connection, owner: Uuid) -> Result<Vec<Uuid>> {
    let snapshot = read(db, owner)?;
    Ok(snapshot
        .turns
        .into_iter()
        .flat_map(|t| t.attachments)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect())
}
pub(crate) fn inherit(db: &Connection, owner: Uuid, session: &str) -> Result<()> {
    for entry in read(db, owner)?.turns {
        let old: Option<(Option<String>, i64)> = db
            .query_row(
                "SELECT turn,ordinal FROM attachment_turns WHERE session=?1 AND command=?2",
                params![session, entry.command],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some(old) = old {
            if old != (entry.turn.clone(), entry.ordinal)
                || items(db, session, &entry.command)? != entry.attachments
            {
                return Err("The inherited attachment history changed.".into());
            }
            continue;
        }
        db.execute(
            "INSERT INTO attachment_turns VALUES(?1,?2,?3,?4,1)",
            params![session, entry.command, entry.turn, entry.ordinal],
        )?;
        for attachment in entry.attachments {
            db.execute(
                "INSERT INTO attachment_turn_items VALUES(?1,?2,?3)",
                params![session, entry.command, attachment.to_string()],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Result<Connection> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE attachments(id TEXT PRIMARY KEY); CREATE TABLE operations(id TEXT PRIMARY KEY,state TEXT); CREATE TABLE input_attachments(input TEXT,attachment TEXT);")?;
        install(&db)?;
        Ok(db)
    }
    fn dispatch(db: &Connection, session: &str, attachment: Uuid) -> Result<String> {
        let command = Uuid::now_v7().to_string();
        db.execute(
            "INSERT OR IGNORE INTO attachments VALUES(?1)",
            [attachment.to_string()],
        )?;
        db.execute("INSERT INTO operations VALUES(?1,'prepared')", [&command])?;
        db.execute(
            "INSERT INTO input_attachments VALUES(?1,?2)",
            params![command, attachment.to_string()],
        )?;
        reserve(db, &command, session, &command)?;
        Ok(command)
    }
    fn arrived(db: &Connection, session: &str, command: &str, turn: &str) -> Result<()> {
        observe(
            db,
            session,
            &serde_json::json!({"method":"turn/started","params":{"sessionId":session,"commandId":command,"turnId":turn}}),
        )
    }
    #[test]
    fn branch_cut_is_frozen_and_nested_branches_cannot_receive_later_references() -> Result<()> {
        let db = database()?;
        let early = Uuid::now_v7();
        let late = Uuid::now_v7();
        let child_late = Uuid::now_v7();
        let a = dispatch(&db, "parent", early)?;
        arrived(&db, "parent", &a, "early")?;
        let branch = Uuid::now_v7();
        freeze(&db, branch, "parent", Some("early"))?;
        let b = dispatch(&db, "parent", late)?;
        arrived(&db, "parent", &b, "late")?;
        freeze(&db, branch, "parent", Some("early"))?;
        inherit(&db, branch, "child")?;
        inherit(&db, branch, "child")?;
        assert_eq!(available(&db, "child")?, vec![early]);
        let c = dispatch(&db, "child", child_late)?;
        arrived(&db, "child", &c, "child-late")?;
        let grandchild = Uuid::now_v7();
        freeze(&db, grandchild, "child", Some("early"))?;
        inherit(&db, grandchild, "grandchild")?;
        assert_eq!(available(&db, "grandchild")?, vec![early]);
        assert!(freeze(&db, branch, "parent", Some("late")).is_err());
        Ok(())
    }
    #[test]
    fn pickup_keeps_unknown_dispatches_but_not_superseded_or_future_queued_files() -> Result<()> {
        let db = database()?;
        let early = Uuid::now_v7();
        let unknown = Uuid::now_v7();
        let cancelled = Uuid::now_v7();
        let later = Uuid::now_v7();
        let a = dispatch(&db, "source", early)?;
        arrived(&db, "source", &a, "early")?;
        dispatch(&db, "source", unknown)?;
        let cancelled_command = dispatch(&db, "source", cancelled)?;
        db.execute(
            "UPDATE operations SET state='superseded' WHERE id=?1",
            [cancelled_command],
        )?;
        let pickup = Uuid::now_v7();
        freeze(&db, pickup, "source", None)?;
        dispatch(&db, "source", later)?;
        inherit(&db, pickup, "new")?;
        assert_eq!(
            available(&db, "new")?
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            [early, unknown].into_iter().collect()
        );
        let next = dispatch(&db, "new", later)?;
        arrived(&db, "new", &next, "handoff")?;
        let branch = Uuid::now_v7();
        freeze(&db, branch, "new", Some("handoff"))?;
        inherit(&db, branch, "branch")?;
        assert_eq!(available(&db, "branch")?.len(), 3);
        Ok(())
    }
    #[test]
    fn unconfirmed_or_conflicting_turn_boundaries_fail_closed() -> Result<()> {
        let db = database()?;
        let a = dispatch(&db, "source", Uuid::now_v7())?;
        let b = dispatch(&db, "source", Uuid::now_v7())?;
        arrived(&db, "source", &b, "later")?;
        assert!(freeze(&db, Uuid::now_v7(), "source", Some("later")).is_err());
        assert!(freeze(&db, Uuid::now_v7(), "source", Some("missing")).is_err());
        arrived(&db, "source", &a, "early")?;
        assert!(arrived(&db, "source", &a, "different").is_err());
        assert!(arrived(&db, "source", &b, "early").is_err());
        let frozen = Uuid::now_v7();
        freeze(&db, frozen, "source", Some("later"))?;
        assert_eq!(frozen_ids(&db, frozen)?.len(), 2);
        Ok(())
    }
}
