//! Ancestry is frozen in the same transaction as the checkpoint intent.
use crate::{
    Result,
    lineage::{Ancestors, Basis, Frozen, Point},
};
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS track_lineage(track TEXT PRIMARY KEY REFERENCES tracks(id),record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS checkpoint_lineage(operation TEXT PRIMARY KEY,track TEXT NOT NULL REFERENCES tracks(id),number INTEGER NOT NULL,sha TEXT NOT NULL,record TEXT NOT NULL,complete INTEGER NOT NULL CHECK(complete IN (0,1)),UNIQUE(track,number),UNIQUE(track,sha));
        CREATE TABLE IF NOT EXISTS integrated_lineage(track TEXT PRIMARY KEY REFERENCES tracks(id),record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS published_lineage(session TEXT NOT NULL,folder TEXT NOT NULL,head TEXT NOT NULL,coverage TEXT NOT NULL,PRIMARY KEY(session,folder));")?;
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
pub(crate) fn reserve(db: &Connection, operation: &str, track: &str, sha: &str) -> Result<()> {
    let basis = basis(db, track)?;
    let number: i64 = db.query_row(
        "SELECT coalesce(max(number),0)+1 FROM checkpoint_lineage WHERE track=?1",
        [track],
        |row| row.get(0),
    )?;
    let point = Point {
        views: basis.views,
        parents: basis.parents,
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
        crate::lineage::include(&mut current.base_inherits, track, &point.inherited())?;
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

/// Coverage belongs to the exact published head of one session and folder.
/// The global integrated map alone cannot identify what a realignment target
/// contains, particularly after publishing only some tracks or folders.
pub(crate) struct Published {
    pub head: String,
    pub coverage: Ancestors,
}

pub(crate) fn read_published(
    db: &Connection,
    session: &str,
    folder: &str,
) -> Result<Option<Published>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT head,coverage FROM published_lineage WHERE session=?1 AND folder=?2",
            params![session, folder],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(head, coverage)| {
        Ok(Published {
            head,
            coverage: serde_json::from_str(&coverage)?,
        })
    })
    .transpose()
}

pub(crate) fn capture_coverage(
    db: &Connection,
    job: &crate::integration::Job,
) -> Result<Ancestors> {
    let plan = job.plan.as_ref().ok_or("Missing publication plan.")?;
    if let Some(saved) = read_published(db, &job.session_id, &job.project.id)? {
        if saved.head != plan.target_head {
            return Err("The session branch does not match its saved checkpoint coverage.".into());
        }
        Ok(saved.coverage)
    } else if plan.target_head == job.project.base {
        Ok(Ancestors::new())
    } else {
        Err("The session branch has no saved checkpoint coverage.".into())
    }
}
pub(crate) fn coverage_before_publication(
    db: &Connection,
    job: &crate::integration::Job,
) -> Result<Ancestors> {
    let coverage = capture_coverage(db, job)?;
    if job.plan.as_ref().and_then(|plan| plan.coverage.as_ref()) != Some(&coverage) {
        return Err("The published checkpoint coverage changed. Refresh the Commit review.".into());
    }
    Ok(coverage)
}

pub(crate) fn publish_head(
    db: &Connection,
    job: &crate::integration::Job,
    included: &[(&str, &str)],
) -> Result<()> {
    let candidate = job
        .candidate
        .as_ref()
        .ok_or("Missing published candidate.")?;
    let mut coverage = coverage_before_publication(db, job)?;
    if let Some(undo) = &job.undo {
        crate::replacement_delta::rewind(&undo.records, &mut coverage);
    }
    for (track, sha) in included {
        job.lineage
            .incorporate(&mut coverage, job.lineage.by_sha(track, sha)?)?;
    }
    db.execute(
        "INSERT INTO published_lineage VALUES(?1,?2,?3,?4) ON CONFLICT(session,folder) DO UPDATE SET head=excluded.head,coverage=excluded.coverage",
        params![job.session_id, job.project.id, candidate.head, serde_json::to_string(&coverage)?],
    )?;
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
        reserve(db, id, track, sha)
    }
    #[test]
    fn the_next_checkpoint_freezes_own_published_coverage_without_rewriting_older_points()
    -> Result<()> {
        let db = db()?;
        checkpoint(&db, "first", "A", &"1".repeat(40))?;
        complete(&db, "first")?;
        let first = point(&db, "A", &"1".repeat(40))?;
        published(&db, &freeze(&db)?, &[("A", &"1".repeat(40))])?;
        checkpoint(&db, "second", "A", &"2".repeat(40))?;
        complete(&db, "second")?;
        let second = point(&db, "A", &"2".repeat(40))?;
        assert_eq!(second.number, 2);
        assert_eq!(second.base_at, first.sha);
        assert_eq!(second.base_inherits_at["A"], first.inherited());
        assert!(
            point(&db, "A", &"1".repeat(40))?
                .base_inherits_at
                .is_empty()
        );
        Ok(())
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
        include(&mut child.parents, "A", &parent.inherited())?;
        set_basis(&db, "B", &child)?;
        checkpoint(&db, "two", "B", &"2".repeat(40))?;
        complete(&db, "two")?;
        let frozen = freeze(&db)?;
        assert_eq!(
            frozen.by_sha("B", &"2".repeat(40))?.parents,
            crate::lineage::Ancestors::from([("A".into(), parent.inherited())])
        );
        published(&db, &frozen, &[("B", &"2".repeat(40))])?;
        let after = freeze(&db)?;
        assert_eq!(after.integrated["A"], parent.inherited());
        assert_eq!(after.integrated["B"].number, 1);
        assert_eq!(basis(&db, "A")?.base, "0".repeat(40));
        assert_eq!(basis(&db, "B")?.base, "2".repeat(40));
        assert_eq!(basis(&db, "B")?.base_inherits["A"], parent.inherited());
        assert_eq!(basis(&db, "B")?.parents["A"], parent.inherited());
        Ok(())
    }

    #[test]
    fn a_checkpoint_keeps_its_direct_parent_when_the_live_dependency_advances() -> Result<()> {
        let db = db()?;
        checkpoint(&db, "a1", "A", &"1".repeat(40))?;
        complete(&db, "a1")?;
        let a1 = point(&db, "A", &"1".repeat(40))?;
        checkpoint(&db, "a2", "A", &"2".repeat(40))?;
        complete(&db, "a2")?;
        let a2 = point(&db, "A", &"2".repeat(40))?;
        let mut child = basis(&db, "B")?;
        child.parents.insert("A".into(), a1.inherited());
        child.inherits = child.parents.clone();
        set_basis(&db, "B", &child)?;
        checkpoint(&db, "b1", "B", &"3".repeat(40))?;
        complete(&db, "b1")?;
        child.parents.insert("A".into(), a2.inherited());
        child.inherits = child.parents.clone();
        set_basis(&db, "B", &child)?;
        checkpoint(&db, "b2", "B", &"4".repeat(40))?;
        complete(&db, "b2")?;
        let graph = freeze(&db)?;
        assert_eq!(graph.point("B", 1)?.parents["A"], a1.inherited());
        assert_eq!(graph.point("B", 2)?.parents["A"], a2.inherited());
        assert_eq!(graph.point("B", 1)?.basis().parents["A"], a1.inherited());
        assert_eq!(basis(&db, "B")?.parents["A"], a2.inherited());
        Ok(())
    }
}
