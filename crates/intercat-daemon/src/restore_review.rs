//! A spoken Restore opens a review of exact retired paths. A later retirement
//! of the same worker is a different choice, never a replacement for this list.
use crate::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Candidate {
    pub id: Uuid,
    pub track: String,
    pub label: String,
    pub title: String,
    pub folder: String,
    pub session: String,
}

pub(crate) fn capture(db: &Connection, track: Option<&str>) -> Result<Vec<Candidate>> {
    let mut query = db.prepare(
        "SELECT p.id,p.track,t.label,t.title,json_extract(p.record,'$.folder'),json_extract(p.record,'$.session')
        FROM path_retirements p JOIN tracks t ON t.id=p.track
        WHERE p.active=1 AND json_extract(p.record,'$.kind')='aside'
        AND json_extract(p.record,'$.session')=(SELECT json_extract(value,'$.id') FROM settings WHERE key='session')
        AND (?1 IS NULL OR p.track=?1) ORDER BY p.rowid",
    )?;
    let candidates = query
        .query_map(params![track], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .map(|raw| {
            let (id, track, label, title, folder, session) = raw?;
            Ok(Candidate {
                id: id.parse()?,
                track,
                label,
                title,
                folder,
                session,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    validate(&candidates, track)?;
    Ok(candidates)
}

pub(crate) fn validate(candidates: &[Candidate], track: Option<&str>) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut tracks = BTreeSet::new();
    let session = candidates
        .first()
        .map(|candidate| candidate.session.as_str());
    for candidate in candidates {
        if !ids.insert(candidate.id)
            || !tracks.insert(&candidate.track)
            || candidate.label.trim().is_empty()
            || session != Some(candidate.session.as_str())
            || track.is_some_and(|track| track != candidate.track)
        {
            return Err("Invalid saved Restore choices.".into());
        }
        for id in [&candidate.track, &candidate.folder, &candidate.session] {
            Uuid::parse_str(id)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_current_session_asides_are_offered_and_a_named_track_never_expands_the_scope()
    -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT);CREATE TABLE tracks(id TEXT PRIMARY KEY,label TEXT,title TEXT);CREATE TABLE path_retirements(id TEXT PRIMARY KEY,track TEXT,active INTEGER,record TEXT);")?;
        let session = Uuid::now_v7().to_string();
        let folder = Uuid::now_v7().to_string();
        db.execute(
            "INSERT INTO settings VALUES('session',?1)",
            [json!({"id":session}).to_string()],
        )?;
        let mut expected = vec![];
        let mut excluded = vec![];
        for (label, kind, active, owner) in [
            ("A", "aside", true, session.clone()),
            ("B", "aside", true, session.clone()),
            ("C", "leaveOut", true, session.clone()),
            ("D", "aside", false, session.clone()),
            ("E", "aside", true, Uuid::now_v7().to_string()),
        ] {
            let candidate = Candidate {
                id: Uuid::now_v7(),
                track: Uuid::now_v7().to_string(),
                label: label.into(),
                title: format!("Own {label}"),
                folder: folder.clone(),
                session: owner,
            };
            db.execute(
                "INSERT INTO tracks VALUES(?1,?2,?3)",
                params![candidate.track, candidate.label, candidate.title],
            )?;
            db.execute(
                "INSERT INTO path_retirements VALUES(?1,?2,?3,?4)",
                params![
                    candidate.id.to_string(),
                    candidate.track,
                    active,
                    json!({"kind":kind,"folder":candidate.folder,"session":candidate.session})
                        .to_string()
                ],
            )?;
            if label == "A" || label == "B" {
                expected.push(candidate);
            } else {
                excluded.push(candidate);
            }
        }
        assert_eq!(capture(&db, None)?, expected);
        assert_eq!(
            capture(&db, Some(&expected[0].track))?.as_slice(),
            std::slice::from_ref(&expected[0])
        );
        for candidate in excluded {
            assert!(capture(&db, Some(&candidate.track))?.is_empty());
        }
        db.execute("DELETE FROM settings WHERE key='session'", [])?;
        assert!(capture(&db, None)?.is_empty());
        Ok(())
    }

    #[test]
    fn a_saved_restore_list_cannot_repeat_a_worker_or_cross_its_track_and_session() -> Result<()> {
        let candidate = Candidate {
            id: Uuid::now_v7(),
            track: Uuid::now_v7().to_string(),
            label: "A".into(),
            title: "Own saved path".into(),
            folder: Uuid::now_v7().to_string(),
            session: Uuid::now_v7().to_string(),
        };
        validate(std::slice::from_ref(&candidate), Some(&candidate.track))?;
        assert!(
            validate(
                std::slice::from_ref(&candidate),
                Some(&Uuid::now_v7().to_string())
            )
            .is_err()
        );
        let mut repeated = candidate.clone();
        repeated.id = Uuid::now_v7();
        assert!(validate(&[candidate.clone(), repeated.clone()], None).is_err());
        repeated.track = Uuid::now_v7().to_string();
        repeated.session = Uuid::now_v7().to_string();
        assert!(validate(&[candidate, repeated], None).is_err());
        Ok(())
    }
}
