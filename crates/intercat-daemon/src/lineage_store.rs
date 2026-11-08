//! Ancestry is frozen in the same transaction as the checkpoint intent.
use crate::{
    Result,
    lineage::{Ancestors, Basis, Frozen, Point},
};
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS track_lineage(track TEXT PRIMARY KEY REFERENCES tracks(id),record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS checkpoint_lineage(operation TEXT PRIMARY KEY,track TEXT NOT NULL REFERENCES tracks(id),number INTEGER NOT NULL,sha TEXT NOT NULL,record TEXT NOT NULL,complete INTEGER NOT NULL CHECK(complete IN (0,1)),UNIQUE(track,number),UNIQUE(track,sha));
        CREATE TABLE IF NOT EXISTS integrated_lineage(track TEXT PRIMARY KEY REFERENCES tracks(id),record TEXT NOT NULL);")?;
    Ok(())
}
pub(crate) fn initialize(db: &Connection, track: &str, basis: &Basis) -> Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO track_lineage VALUES(?1,?2)",
        params![track, serde_json::to_string(basis)?],
    )?;
    Ok(())
}
pub(crate) fn basis(db: &Connection, track: &str) -> Result<Basis> {
    let raw: String = db.query_row(
        "SELECT record FROM track_lineage WHERE track=?1",
        [track],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&raw)?)
}
pub(crate) fn reserve(db: &Connection, operation: &str, payload: &serde_json::Value) -> Result<()> {
    let track = payload["trackId"]
        .as_str()
        .ok_or("Missing checkpoint track.")?;
    let sha = payload["checkpoint"]["sha"]
        .as_str()
        .ok_or("Missing checkpoint tree.")?;
    let basis = basis(db, track)?;
    let number: i64 = db.query_row(
        "SELECT coalesce(max(number),0)+1 FROM checkpoint_lineage WHERE track=?1",
        [track],
        |row| row.get(0),
    )?;
    let point = Point {
        track: track.into(),
        number: number.try_into()?,
        sha: sha.into(),
        base_at: basis.base,
        base_inherits_at: basis.base_inherits,
        inherits: basis.inherits,
    };
    db.execute(
        "INSERT INTO checkpoint_lineage VALUES(?1,?2,?3,?4,?5,0)",
        params![
            operation,
            track,
            number,
            sha,
            serde_json::to_string(&point)?
        ],
    )?;
    Ok(())
}
pub(crate) fn point(db: &Connection, track: &str, sha: &str) -> Result<Point> {
    let raw: String = db.query_row(
        "SELECT record FROM checkpoint_lineage WHERE track=?1 AND sha=?2 AND complete=1",
        params![track, sha],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&raw)?)
}
pub(crate) fn freeze(db: &Connection) -> Result<Frozen> {
    let mut query =
        db.prepare("SELECT record FROM checkpoint_lineage WHERE complete=1 ORDER BY track,number")?;
    let points = query
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<Vec<_>>>()?;
    let mut query = db.prepare("SELECT track,record FROM integrated_lineage")?;
    let integrated = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .map(|row| {
            let (track, raw) = row?;
            Ok((track, serde_json::from_str(&raw)?))
        })
        .collect::<Result<Ancestors>>()?;
    Ok(Frozen { points, integrated })
}
pub(crate) fn complete(db: &Connection, operation: &str) -> Result<()> {
    // Session retirement removes old run operations, but descendants still
    // need their immutable ancestor trees. Keep the completed record itself.
    db.execute(
        "UPDATE checkpoint_lineage SET complete=1 WHERE operation=?1",
        [operation],
    )?;
    Ok(())
}
pub(crate) fn set_basis(db: &Connection, track: &str, basis: &Basis) -> Result<()> {
    if db.execute(
        "UPDATE track_lineage SET record=?2 WHERE track=?1",
        params![track, serde_json::to_string(basis)?],
    )? != 1
    {
        return Err("The track ancestry is unavailable.".into());
    }
    Ok(())
}
pub(crate) fn published(db: &Connection, frozen: &Frozen, included: &[(&str, &str)]) -> Result<()> {
    let mut integrated = frozen.integrated.clone();
    for (track, sha) in included {
        let point = frozen.by_sha(track, sha)?;
        frozen.incorporate(&mut integrated, point)?;
        let mut current = basis(db, track)?;
        current.base = point.sha.clone();
        current.base_inherits = point.inherits.clone();
        set_basis(db, track, &current)?;
    }
    for (track, mark) in integrated {
        let old: Option<String> = db
            .query_row(
                "SELECT record FROM integrated_lineage WHERE track=?1",
                [&track],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            let old: crate::lineage::Inherited = serde_json::from_str(&old)?;
            if old.number > mark.number {
                return Err("The integrated checkpoint moved backwards.".into());
            }
        }
        db.execute("INSERT INTO integrated_lineage VALUES(?1,?2) ON CONFLICT(track) DO UPDATE SET record=excluded.record",params![track,serde_json::to_string(&mark)?])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lineage::include;
    fn db() -> Result<Connection> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE tracks(id TEXT PRIMARY KEY); CREATE TABLE operations(id TEXT PRIMARY KEY,state TEXT);")?;
        install(&db)?;
        for track in ["A", "B"] {
            db.execute("INSERT INTO tracks VALUES(?1)", [track])?;
            initialize(&db, track, &Basis::root("0".repeat(40)))?;
        }
        Ok(db)
    }
    fn checkpoint(db: &Connection, id: &str, track: &str, sha: &str) -> Result<()> {
        db.execute("INSERT INTO operations VALUES(?1,'prepared')", [id])?;
        reserve(
            db,
            id,
            &serde_json::json!({"trackId":track,"checkpoint":{"sha":sha}}),
        )
    }
    #[test]
    fn checkpoint_metadata_is_frozen_with_its_intent_and_visible_only_when_complete() -> Result<()>
    {
        let db = db()?;
        checkpoint(&db, "one", "A", &"1".repeat(40))?;
        assert!(freeze(&db)?.points.is_empty());
        set_basis(&db, "A", &Basis::root("f".repeat(40)))?;
        complete(&db, "one")?;
        let first = point(&db, "A", &"1".repeat(40))?;
        assert_eq!(first.base_at, "0".repeat(40));
        checkpoint(&db, "two", "A", &"2".repeat(40))?;
        complete(&db, "two")?;
        let second = point(&db, "A", &"2".repeat(40))?;
        assert_eq!(second.number, 2);
        assert_eq!(second.base_at, "f".repeat(40));
        db.execute("DELETE FROM operations", [])?;
        assert_eq!(freeze(&db)?.points.len(), 2);
        Ok(())
    }
    #[test]
    fn publishing_only_a_child_advances_ancestor_numbers_without_changing_its_working_base()
    -> Result<()> {
        let db = db()?;
        checkpoint(&db, "one", "A", &"1".repeat(40))?;
        complete(&db, "one")?;
        let parent = point(&db, "A", &"1".repeat(40))?;
        let mut child = parent.basis();
        include(&mut child.inherits, "A", &parent.inherited())?;
        set_basis(&db, "B", &child)?;
        checkpoint(&db, "two", "B", &"2".repeat(40))?;
        complete(&db, "two")?;
        let frozen = freeze(&db)?;
        published(&db, &frozen, &[("B", &"2".repeat(40))])?;
        let after = freeze(&db)?;
        assert_eq!(after.integrated["A"], parent.inherited());
        assert_eq!(after.integrated["B"].number, 1);
        assert_eq!(basis(&db, "A")?.base, "0".repeat(40));
        assert_eq!(basis(&db, "B")?.base, "2".repeat(40));
        assert_eq!(basis(&db, "B")?.base_inherits["A"], parent.inherited());
        Ok(())
    }
}
