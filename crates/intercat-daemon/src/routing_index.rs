//! Derived, private FTS over track titles and valid generated clip summaries.
use crate::{Result, clip_summaries::Summary, routing::Track};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeSet, HashSet};

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TEMP TABLE routing_documents(
            id INTEGER PRIMARY KEY, source TEXT UNIQUE NOT NULL, track TEXT NOT NULL,
            clip TEXT, ordinal INTEGER NOT NULL, title TEXT NOT NULL, summary TEXT NOT NULL);
        CREATE INDEX routing_document_track ON routing_documents(track,ordinal DESC);
        CREATE VIRTUAL TABLE temp.routing_search USING fts5(title,summary,
            content='routing_documents',content_rowid='id',tokenize='trigram');
        CREATE TEMP TRIGGER routing_document_insert AFTER INSERT ON routing_documents BEGIN
            INSERT INTO routing_search(rowid,title,summary) VALUES(NEW.id,NEW.title,NEW.summary);
        END;
        CREATE TEMP TRIGGER routing_document_delete AFTER DELETE ON routing_documents BEGIN
            INSERT INTO routing_search(routing_search,rowid,title,summary) VALUES('delete',OLD.id,OLD.title,OLD.summary);
        END;
        CREATE TEMP TRIGGER routing_document_update AFTER UPDATE ON routing_documents BEGIN
            INSERT INTO routing_search(routing_search,rowid,title,summary) VALUES('delete',OLD.id,OLD.title,OLD.summary);
            INSERT INTO routing_search(rowid,title,summary) VALUES(NEW.id,NEW.title,NEW.summary);
        END;
        CREATE TEMP TABLE routing_dirty(clip TEXT PRIMARY KEY);
        CREATE TEMP TRIGGER routing_clip_insert AFTER INSERT ON main.timeline_clips BEGIN
            INSERT INTO routing_dirty SELECT NEW.id WHERE NOT EXISTS(SELECT 1 FROM routing_dirty WHERE clip=NEW.id);
        END;
        CREATE TEMP TRIGGER routing_clip_update AFTER UPDATE OF record ON main.timeline_clips BEGIN
            INSERT INTO routing_dirty SELECT NEW.id WHERE NOT EXISTS(SELECT 1 FROM routing_dirty WHERE clip=NEW.id);
        END;
        CREATE TEMP TRIGGER routing_clip_delete AFTER DELETE ON main.timeline_clips BEGIN
            DELETE FROM routing_documents WHERE source='clip:'||OLD.id;
            DELETE FROM routing_dirty WHERE clip=OLD.id;
        END;
        CREATE TEMP TRIGGER routing_track_delete AFTER DELETE ON main.tracks BEGIN
            DELETE FROM routing_documents WHERE track=OLD.id;
        END;
        INSERT INTO routing_documents(source,track,clip,ordinal,title,summary)
            SELECT 'track:'||id,id,NULL,0,title,'' FROM tracks ORDER BY rowid;
        INSERT INTO routing_documents(source,track,clip,ordinal,title,summary)
            SELECT 'clip:'||c.id,c.track,c.id,c.ordinal,json_extract(s.result,'$.title'),json_extract(s.result,'$.summary')
            FROM clip_summaries s JOIN timeline_clips c ON c.id=s.clip
            JOIN timeline_versions v ON v.id=c.id JOIN tracks t ON t.id=c.track
            WHERE s.state='complete' AND s.version=v.version AND v.deleted=0 ORDER BY c.rowid;",
    )?;
    Ok(())
}

pub(crate) fn refresh(db: &Connection, tracks: &[Track]) -> Result<()> {
    let mut track_update = db.prepare(
        "INSERT INTO routing_documents(source,track,clip,ordinal,title,summary) VALUES(?1,?2,NULL,0,?3,'')
        ON CONFLICT(source) DO UPDATE SET title=excluded.title WHERE title<>excluded.title",
    )?;
    for track in tracks {
        track_update.execute(params![
            format!("track:{}", track.candidate.id),
            track.candidate.id,
            track.candidate.title
        ])?;
    }
    let dirty = db
        .prepare("SELECT clip FROM routing_dirty")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for clip in dirty {
        let owner: Option<(String,i64)> = db.query_row(
            "SELECT c.track,c.ordinal FROM timeline_clips c JOIN tracks t ON t.id=c.track WHERE c.id=?1",
            [&clip],|row|Ok((row.get(0)?,row.get(1)?)),
        ).optional()?;
        let source = format!("clip:{clip}");
        let summary = crate::clip_summary_store::read(db, &clip)?;
        if let (Some((track, ordinal)), Some(summary)) = (owner, summary) {
            db.execute("INSERT INTO routing_documents(source,track,clip,ordinal,title,summary) VALUES(?1,?2,?3,?4,?5,?6)
                ON CONFLICT(source) DO UPDATE SET track=excluded.track,ordinal=excluded.ordinal,title=excluded.title,summary=excluded.summary
                WHERE track<>excluded.track OR ordinal<>excluded.ordinal OR title<>excluded.title OR summary<>excluded.summary",
                params![source,track,clip,ordinal,summary.title,summary.summary])?;
        } else {
            db.execute("DELETE FROM routing_documents WHERE source=?1", [source])?;
        }
        db.execute("DELETE FROM routing_dirty WHERE clip=?1", [clip])?;
    }
    Ok(())
}

pub(crate) fn latest(db: &Connection, track: &str) -> Result<Option<Summary>> {
    Ok(db.query_row(
        "SELECT title,summary FROM routing_documents WHERE track=?1 AND clip IS NOT NULL ORDER BY ordinal DESC LIMIT 1",
        [track],|row|Ok(Summary{title:row.get(0)?,summary:row.get(1)?}),
    ).optional()?)
}

pub(crate) fn search(db: &Connection, text: &str) -> Result<Vec<(String, Option<Summary>)>> {
    // Quote each term so request text cannot become FTS operators.
    let terms: BTreeSet<_> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|term| term.chars().count() >= 3)
        .flat_map(|term| {
            let characters: Vec<_> = term.chars().collect();
            if characters
                .iter()
                .any(|&c| matches!(c as u32,0x3040..=0x30ff|0x3400..=0x9fff|0xac00..=0xd7a3))
            {
                characters
                    .windows(3)
                    .map(|part| part.iter().collect::<String>())
                    .collect::<Vec<_>>()
            } else {
                vec![term.to_owned()]
            }
        })
        .map(|term| format!("\"{term}\""))
        .collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = db.prepare(
        "SELECT d.track,d.clip,d.title,d.summary FROM routing_search
        JOIN routing_documents d ON d.id=routing_search.rowid
        WHERE routing_search MATCH ?1 ORDER BY rank,d.id",
    )?;
    let mut rows = query.query([terms.into_iter().collect::<Vec<_>>().join(" OR ")])?;
    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    while let Some(row) = rows.next()? {
        let track: String = row.get(0)?;
        if seen.insert(track.clone()) {
            let clip: Option<String> = row.get(1)?;
            let summary = if clip.is_some() {
                Some(Summary {
                    title: row.get(2)?,
                    summary: row.get(3)?,
                })
            } else {
                None
            };
            matches.push((track, summary));
            // At most eight first-pass tracks and eight different second-pass tracks are needed.
            if matches.len() == 16 {
                break;
            }
        }
    }
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::Candidate;
    use serde_json::json;

    fn database() -> Result<Connection> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE tracks(id TEXT PRIMARY KEY,title TEXT);
            CREATE TABLE timeline_clips(id TEXT PRIMARY KEY,track TEXT,ordinal INTEGER,record TEXT);
            CREATE TABLE timeline_versions(id TEXT PRIMARY KEY,version INTEGER,deleted INTEGER);
            CREATE TABLE timeline_items(session TEXT,item TEXT,clip TEXT,record TEXT);
            CREATE TABLE events(ts INTEGER,kind TEXT,payload TEXT);",
        )?;
        crate::clip_summary_store::install(&db)?;
        Ok(db)
    }

    fn track(db: &Connection, id: &str) -> Result<Track> {
        db.execute(
            "INSERT INTO tracks VALUES(?1,?2)",
            params![id, format!("Work {id}")],
        )?;
        Ok(Track {
            candidate: Candidate {
                id: id.into(),
                label: id.into(),
                title: format!("Work {id}"),
                status: "waiting".into(),
                activity: None,
                running: None,
                planned: Vec::new(),
            },
            spoken_at: None,
        })
    }

    fn clip(db: &Connection, id: &str, track: &str, ordinal: i64, summary: &str) -> Result<()> {
        db.execute("INSERT INTO timeline_versions VALUES(?1,1,0)", [id])?;
        db.execute(
            "INSERT INTO timeline_clips VALUES(?1,?2,?3,?4)",
            params![
                id,
                track,
                ordinal,
                json!({"category":"step","end":2}).to_string()
            ],
        )?;
        db.execute(
            "INSERT INTO clip_summaries VALUES(?1,1,'request',1,'complete',?2)",
            params![
                id,
                json!({"title":"Recorded work","summary":summary}).to_string()
            ],
        )?;
        Ok(())
    }

    fn integrity(db: &Connection) -> Result<()> {
        db.execute(
            "INSERT INTO routing_search(routing_search,rank) VALUES('integrity-check',1)",
            [],
        )?;
        Ok(())
    }

    #[test]
    fn index_tracks_updates_deletes_and_transaction_rollback() -> Result<()> {
        let db = database()?;
        let mut tracks = vec![track(&db, "a")?, track(&db, "b")?];
        clip(&db, "old", "a", 1, "以前の認証処理を改善した")?;
        clip(&db, "new", "a", 2, "Adjusted button spacing")?;
        install(&db)?;
        assert_eq!(search(&db, "認証処理")?[0].0, "a");
        assert_eq!(
            latest(&db, "a")?.unwrap().summary,
            "Adjusted button spacing"
        );
        tracks[1].candidate.title = "Mercury latency".into();
        refresh(&db, &tracks)?;
        assert_eq!(search(&db, "mercury")?[0].0, "b");
        db.execute_batch("BEGIN; UPDATE timeline_versions SET version=2 WHERE id='old'; UPDATE timeline_clips SET record=record WHERE id='old';")?;
        refresh(&db, &tracks)?;
        assert!(search(&db, "認証処理")?.is_empty());
        integrity(&db)?;
        db.execute_batch("ROLLBACK;")?;
        assert_eq!(search(&db, "認証処理")?[0].0, "a");
        integrity(&db)?;
        db.execute("DELETE FROM timeline_clips WHERE id='old'", [])?;
        assert!(search(&db, "認証処理")?.is_empty());
        db.execute("DELETE FROM tracks WHERE id='b'", [])?;
        assert!(search(&db, "mercury")?.is_empty());
        integrity(&db)?;
        Ok(())
    }

    #[test]
    fn repeated_clip_notifications_are_idempotent_inside_an_upsert() -> Result<()> {
        let db = database()?;
        let tracks = vec![track(&db, "a")?];
        install(&db)?;
        clip(&db, "clip", "a", 1, "Exponential backoff")?;
        for _ in 0..3 {
            db.execute("INSERT INTO timeline_clips VALUES('clip','a',1,'{}') ON CONFLICT(id) DO UPDATE SET record=timeline_clips.record",[])?;
        }
        refresh(&db, &tracks)?;
        assert_eq!(search(&db, "exponential")?.len(), 1);
        integrity(&db)?;
        Ok(())
    }

    #[test]
    fn summary_matches_outside_the_first_eight_lead_the_second_pass() -> Result<()> {
        let db = database()?;
        let mut tracks = Vec::new();
        for index in 0..20 {
            let mut candidate = track(&db, &format!("t{index}"))?;
            if index < 5 {
                candidate.spoken_at = Some(index);
            }
            tracks.push(candidate);
        }
        clip(&db, "match", "t19", 1, "Investigated exponential backoff")?;
        install(&db)?;
        let picked = crate::routing::select(
            &db,
            "Continue exponential backoff",
            tracks,
            Some("t8"),
            None,
            &["t6".into(), "t7".into()],
        )?;
        assert_eq!(picked.first.len(), 8);
        assert!(!picked.first.iter().any(|track| track.id == "t19"));
        assert_eq!(picked.expanded[0].id, "t19");
        assert_eq!(
            picked.expanded[0].activity.as_ref().unwrap().summary,
            "Investigated exponential backoff"
        );
        Ok(())
    }

    #[test]
    fn three_hundred_tracks_have_bounded_results_and_reuse_the_search_index() -> Result<()> {
        let db = database()?;
        let mut tracks = Vec::new();
        for index in 0..300 {
            let id = format!("t{index}");
            tracks.push(track(&db, &id)?);
            for ordinal in 0..3 {
                clip(
                    &db,
                    &format!("{id}-{ordinal}"),
                    &id,
                    ordinal,
                    &format!("Reviewed service retries and destination{index}"),
                )?;
            }
        }
        install(&db)?;
        let mut times = Vec::new();
        for _ in 0..25 {
            let started = std::time::Instant::now();
            let picked = crate::routing::select(
                &db,
                "Continue destination277",
                tracks
                    .iter()
                    .map(|t| Track {
                        candidate: t.candidate.clone(),
                        spoken_at: t.spoken_at,
                    })
                    .collect(),
                None,
                None,
                &[],
            )?;
            assert_eq!(picked.first[0].id, "t277");
            assert!(picked.first.len() <= 8 && picked.expanded.len() <= 8);
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "300 tracks / 900 summaries, 25 warm selections: median {:.3} ms, max {:.3} ms",
            times[12], times[24]
        );
        let broad = search(&db, "service retries")?;
        assert_eq!(broad.len(), 16);
        assert_eq!(
            broad.iter().map(|(id, _)| id).collect::<HashSet<_>>().len(),
            16
        );
        integrity(&db)?;
        Ok(())
    }
}
