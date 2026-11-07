//! Rule-only warnings over valid, closed work-step titles.
//! Warning revisions are separate from content versions: marking a peer must
//! never invalidate its summary or start another model request.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, functions::FunctionFlags};
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

const PEERS: &str = "SELECT t.label,p.ordinal,count(*) OVER()
    FROM clip_title_index own JOIN clip_title_index p
        ON p.normalized=own.normalized AND p.track<>own.track
        AND p.closed_at BETWEEN own.closed_at-600000 AND own.closed_at+600000
    JOIN tracks t ON t.id=p.track
    WHERE own.clip=?1 ORDER BY p.closed_at,p.clip LIMIT 1";

fn normalize(title: &str) -> String {
    title
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn register(db: &Connection) -> Result<()> {
    db.create_scalar_function(
        "intercat_normalize_title",
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |context| Ok(normalize(&context.get::<String>(0)?)),
    )?;
    Ok(())
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS clip_title_index(
            clip TEXT PRIMARY KEY REFERENCES timeline_clips(id) ON DELETE CASCADE,
            track TEXT NOT NULL, ordinal INTEGER NOT NULL, title TEXT NOT NULL,
            normalized TEXT NOT NULL, closed_at INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS clip_title_matches ON clip_title_index(normalized,closed_at);
        CREATE TABLE IF NOT EXISTS timeline_warning_versions(clip TEXT PRIMARY KEY,version INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS timeline_warning_changes ON timeline_warning_versions(version,clip);
        CREATE TRIGGER IF NOT EXISTS clip_title_added AFTER INSERT ON clip_title_index BEGIN
            UPDATE timeline_revision SET revision=revision+1;
            INSERT INTO timeline_warning_versions
                SELECT clip,(SELECT revision FROM timeline_revision) FROM clip_title_index
                WHERE clip=NEW.clip OR (normalized=NEW.normalized AND track<>NEW.track
                    AND closed_at BETWEEN NEW.closed_at-600000 AND NEW.closed_at+600000)
                ON CONFLICT(clip) DO UPDATE SET version=excluded.version;
        END;
        CREATE TRIGGER IF NOT EXISTS clip_title_removed BEFORE DELETE ON clip_title_index BEGIN
            UPDATE timeline_revision SET revision=revision+1;
            INSERT INTO timeline_warning_versions
                SELECT clip,(SELECT revision FROM timeline_revision) FROM clip_title_index
                WHERE clip=OLD.clip OR (normalized=OLD.normalized AND track<>OLD.track
                    AND closed_at BETWEEN OLD.closed_at-600000 AND OLD.closed_at+600000)
                ON CONFLICT(clip) DO UPDATE SET version=excluded.version;
        END;
        CREATE TRIGGER IF NOT EXISTS clip_title_invalidated AFTER UPDATE ON timeline_versions BEGIN
            DELETE FROM clip_title_index WHERE clip=NEW.id;
        END;",
    )?;
    // A title becomes eligible only when its closed-step summary is published.
    // Content changes remove it immediately, including late MSP corrections.
    let insert = "INSERT INTO clip_title_index
        SELECT c.id,c.track,c.ordinal,json_extract(s.result,'$.title'),
            intercat_normalize_title(json_extract(s.result,'$.title')),
            json_extract(c.record,'$.end.timelineMs')
        FROM clip_summaries s JOIN timeline_clips c ON c.id=s.clip
        JOIN timeline_versions v ON v.id=c.id
        WHERE s.state='complete' AND s.version=v.version AND v.deleted=0
            AND json_extract(c.record,'$.category')='step'
            AND json_extract(c.record,'$.repairRunId') IS NULL
            AND json_extract(c.record,'$.end') IS NOT NULL";
    for event in ["INSERT", "UPDATE OF version,state,result"] {
        db.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS clip_title_summary_{} AFTER {event} ON clip_summaries BEGIN
                DELETE FROM clip_title_index WHERE clip=NEW.clip;
                {insert} AND s.clip=NEW.clip;
            END;",
            if event == "INSERT" { "insert" } else { "update" }
        ))?;
    }
    db.execute_batch(&format!(
        "{insert} AND NOT EXISTS(SELECT 1 FROM clip_title_index i WHERE i.clip=c.id);"
    ))?;
    Ok(())
}

pub(crate) fn read(db: &Connection, clip: &str) -> Result<Vec<Value>> {
    let peer: Option<(String, i64, i64)> = db
        .query_row(PEERS, [clip], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()?;
    let mut warnings: Vec<_> = peer.into_iter().map(|(label, ordinal, count)| {
        let others = match count {
            1 => String::new(),
            2 => " and 1 other step".to_owned(),
            _ => format!(" and {} other steps", count - 1),
        };
        json!({"kind":"duplicate-title","text":format!("Same title as {label}:{ordinal}{others} within 10 minutes; possible duplicate work.")})
    }).collect();
    warnings.extend(crate::clip_files::warnings(db, clip)?);
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_normalize_width_case_spacing_and_combining_marks() {
        assert_eq!(normalize("  Ｃａｃｈｅ\u{3000}RETRY  "), "cache retry");
        assert_eq!(normalize("Cafe\u{301}"), normalize("CAFÉ"));
        assert_eq!(normalize("キャッシュ　修正"), "キャッシュ 修正");
        assert_ne!(normalize("cache retry"), normalize("cache reset"));
    }

    #[test]
    fn peer_lookup_uses_title_and_time_index() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE tracks(id TEXT PRIMARY KEY,label TEXT);
            CREATE TABLE timeline_clips(id TEXT PRIMARY KEY,track TEXT,ordinal INTEGER,record TEXT);
            CREATE TABLE timeline_versions(id TEXT PRIMARY KEY,version INTEGER,deleted INTEGER);
            CREATE TABLE timeline_revision(revision INTEGER); INSERT INTO timeline_revision VALUES(0);
            CREATE TABLE clip_summaries(clip TEXT PRIMARY KEY,state TEXT,version INTEGER,result TEXT);")?;
        register(&db)?;
        install(&db)?;
        let plan = db
            .prepare(&format!("EXPLAIN QUERY PLAN {PEERS}"))?
            .query_map(["dummy"], |row| row.get::<_, String>(3))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert!(
            plan.iter().any(
                |line| line.contains("SEARCH p USING INDEX clip_title_matches")
                    && line.contains("closed_at>?")
                    && line.contains("closed_at<?")
            ),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|line| line.starts_with("SCAN p")),
            "{plan:?}"
        );
        Ok(())
    }
}
