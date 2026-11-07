//! Separate from the event log: only key verification and one-way fingerprints.
use crate::{
    Result,
    progress::Fingerprint,
    secrets::{Catalog, Registration, Seal},
};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

pub fn install(db: &Connection) -> Result<()> {
    crate::session_retirement::install(db)?;
    db.execute_batch("CREATE TABLE secret_key(singleton INTEGER PRIMARY KEY CHECK(singleton=1),check_tag BLOB NOT NULL);
      CREATE TABLE secret_fingerprints(session TEXT NOT NULL,track TEXT NOT NULL,name TEXT NOT NULL,byte_len INTEGER NOT NULL,tag BLOB NOT NULL,PRIMARY KEY(session,name,byte_len,tag));
      CREATE TABLE secret_registrations(operation TEXT PRIMARY KEY,request BLOB NOT NULL);
      CREATE TABLE secret_catalog(singleton INTEGER PRIMARY KEY CHECK(singleton=1),revision INTEGER NOT NULL,digest BLOB NOT NULL,sealed_revision INTEGER,sealed_digest BLOB);")?;
    crate::project_secrets::install(db)?;
    crate::project_secret_usage::install(db)?;
    db.execute(
        "INSERT INTO secret_catalog VALUES(1,0,?1,NULL,NULL)",
        [fingerprint_digest(db)?.as_slice()],
    )?;
    Ok(())
}

fn fingerprint_digest(db: &Connection) -> Result<[u8; 32]> {
    fn feed(hash: &mut Sha256, bytes: &[u8]) {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    let mut hash = Sha256::new();
    feed(&mut hash, b"intercat.fingerprint-catalog.v2");
    feed(&mut hash, &crate::project_secrets::catalog_bytes(db)?);
    feed(&mut hash, &crate::session_retirement::catalog_bytes(db)?);
    let mut query=db.prepare("SELECT session,track,name,byte_len,tag FROM secret_fingerprints ORDER BY session,track,name,byte_len,tag")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        feed(&mut hash, b"fingerprint");
        for index in 0..3 {
            feed(&mut hash, row.get::<_, String>(index)?.as_bytes());
        }
        feed(&mut hash, &row.get::<_, i64>(3)?.to_be_bytes());
        feed(&mut hash, &row.get::<_, Vec<u8>>(4)?);
    }
    let mut query =
        db.prepare("SELECT operation,request FROM secret_registrations ORDER BY operation")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        feed(&mut hash, b"registration");
        feed(&mut hash, row.get::<_, String>(0)?.as_bytes());
        feed(&mut hash, &row.get::<_, Vec<u8>>(1)?);
    }
    Ok(hash.finalize().into())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn proof(db: &Connection) -> Result<crate::fingerprint_receipt::Proof> {
    use crate::fingerprint_receipt::{Proof, Session};
    let mut groups =
        std::collections::BTreeMap::<String, (String, Vec<(String, i64, Vec<u8>)>)>::new();
    let mut query = db.prepare("SELECT session,track,name,byte_len,tag FROM secret_fingerprints ORDER BY session,name,byte_len,tag")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let session: String = row.get(0)?;
        let track: String = row.get(1)?;
        let group = groups
            .entry(session)
            .or_insert_with(|| (track.clone(), Vec::new()));
        if group.0 != track {
            return Err("A fingerprint session has multiple owners.".into());
        }
        group.1.push((row.get(2)?, row.get(3)?, row.get(4)?));
    }
    let sessions = groups
        .into_iter()
        .map(|(session_id, (track_id, values))| {
            let digest = hex(&Sha256::digest(serde_json::to_vec(&(
                &session_id,
                &track_id,
                values,
            ))?));
            Ok(Session {
                session_id,
                track_id,
                digest,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    // Retired sessions remain external witnesses even when they never had a
    // secret. Restoring a database from before retirement must not revive them.
    let mut sessions = sessions
        .into_iter()
        .map(|s| (s.session_id.clone(), s))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut query =
        db.prepare("SELECT session,track,operation FROM retired_sessions ORDER BY session")?;
    for row in query.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (session_id, track_id, operation) = row?;
        let previous = sessions.remove(&session_id);
        if previous.as_ref().is_some_and(|s| s.track_id != track_id) {
            return Err("A retired fingerprint session changed owner.".into());
        }
        let digest = hex(&Sha256::digest(serde_json::to_vec(&(
            "intercat.retired-session.v1",
            &session_id,
            &track_id,
            &operation,
            previous.map(|s| s.digest),
        ))?));
        sessions.insert(
            session_id.clone(),
            Session {
                session_id,
                track_id,
                digest,
            },
        );
    }
    Ok(Proof {
        sessions: sessions.into_values().collect(),
        projects: crate::project_secrets::proof(db)?,
    })
}

pub fn catalog(db: &Connection) -> Result<Catalog> {
    let (revision,digest,sealed_revision,sealed_digest):(i64,Vec<u8>,Option<i64>,Option<Vec<u8>>)=db.query_row("SELECT revision,digest,sealed_revision,sealed_digest FROM secret_catalog WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    if digest.as_slice() != fingerprint_digest(db)?
        || sealed_revision.is_some() != sealed_digest.is_some()
    {
        return Err("The saved fingerprint catalog is inconsistent.".into());
    }
    let current = Seal {
        revision: revision
            .try_into()
            .map_err(|_| "Invalid fingerprint revision.")?,
        digest: hex(&digest),
    };
    current.validate()?;
    let sealed = sealed_revision
        .zip(sealed_digest)
        .map(|(revision, digest)| {
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(Seal {
                revision: revision
                    .try_into()
                    .map_err(|_| "Invalid fingerprint revision.")?,
                digest: hex(&digest),
            })
        })
        .transpose()?;
    if let Some(sealed) = &sealed {
        sealed.validate()?;
        if sealed.revision > current.revision
            || (sealed.revision == current.revision && sealed.digest != current.digest)
        {
            return Err("Invalid saved fingerprint confirmation.".into());
        }
    }
    let catalog = Catalog {
        current,
        sealed,
        proof: proof(db)?,
    };
    crate::fingerprint_receipt::Receipt {
        catalog: catalog.current.clone(),
        proof: catalog.proof.clone(),
    }
    .validate()?;
    Ok(catalog)
}

pub fn confirm(db: &mut Connection, seal: &Seal) -> Result<()> {
    seal.validate()?;
    let tx = db.transaction()?;
    if catalog(&tx)?.current != *seal {
        return Err("The fingerprint catalog changed before confirmation.".into());
    }
    tx.execute(
        "UPDATE secret_catalog SET sealed_revision=?1,sealed_digest=digest WHERE singleton=1",
        [seal.revision as i64],
    )?;
    tx.commit()?;
    Ok(())
}
pub fn verify_key(db: &Connection, check: &[u8; 32], fresh: bool) -> Result<()> {
    use subtle::ConstantTimeEq;
    let saved: Option<Vec<u8>> = db
        .query_row(
            "SELECT check_tag FROM secret_key WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match saved {
        None if fresh => { db.execute("INSERT INTO secret_key VALUES(1,?1)",[check.as_slice()])?; }
        Some(saved) if bool::from(saved.as_slice().ct_eq(check)) => {}
        _ => return Err("The Keychain fingerprint key changed or its saved verification is missing. Restore its original key before opening this data.".into()),
    }
    Ok(())
}
fn owner(db: &Connection, track: &str, session: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM workers WHERE track_id=?1 AND json_extract(record,'$.sessionId')=?2 UNION ALL SELECT 1 FROM repair_runs WHERE json_extract(record,'$.track')=?1 AND json_extract(record,'$.session')=?2 UNION ALL SELECT 1 FROM msp_positions WHERE track=?1 AND session=?2)",params![track,session],|row|row.get(0))?)
}
pub fn read(db: &Connection, track: &str, session: &str) -> Result<Vec<Fingerprint>> {
    crate::session_retirement::ensure_open(db, session)?;
    if db.query_row(
        "SELECT EXISTS(SELECT 1 FROM secret_fingerprints WHERE session=?1 AND track<>?2)",
        params![session, track],
        |r| r.get::<_, bool>(0),
    )? {
        return Err("The secret fingerprints belong to another track.".into());
    }
    fingerprints(db, Some(session))
}
pub(crate) fn all(db: &Connection) -> Result<Vec<Fingerprint>> {
    fingerprints(db, None)
}
fn fingerprints(db: &Connection, session: Option<&str>) -> Result<Vec<Fingerprint>> {
    let mut query=db.prepare("SELECT name,byte_len,tag FROM secret_fingerprints WHERE (?1 IS NULL OR session=?1) ORDER BY name,byte_len,tag")?;
    let rows = query.query_map([session], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    let mut fingerprints = rows
        .map(|row| {
            let (name, byte_len, tag) = row?;
            Ok(Fingerprint {
                name,
                byte_len: byte_len
                    .try_into()
                    .map_err(|_| "Invalid saved fingerprint length.")?,
                tag: tag.try_into().map_err(|_| "Invalid saved fingerprint.")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    fingerprints.extend(crate::project_secrets::fingerprints(db)?);
    let mut seen = std::collections::HashSet::new();
    fingerprints.retain(|value| seen.insert((value.name.clone(), value.byte_len, value.tag)));
    Ok(fingerprints)
}
pub fn register(
    db: &mut Connection,
    request: &Registration,
    fingerprint: &Fingerprint,
) -> Result<Catalog> {
    crate::session_retirement::ensure_open(db, &request.session_id)?;
    let tx = db.transaction()?;
    let before = catalog(&tx)?;
    if !owner(&tx, &request.track_id, &request.session_id)? {
        return Err("The secret session does not belong to this track.".into());
    }
    // The operation comparison contains only metadata and irreversible tags.
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(serde_json::to_vec(&(
        &request.track_id,
        &request.session_id,
        &fingerprint.name,
        fingerprint.byte_len as i64,
        &fingerprint.tag,
    ))?);
    let old: Option<Vec<u8>> = tx
        .query_row(
            "SELECT request FROM secret_registrations WHERE operation=?1",
            [request.op_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(old) = old {
        if old.as_slice() != digest.as_slice() {
            return Err(
                "The secret operation ID was already used for another registration.".into(),
            );
        }
        tx.commit()?;
        return Ok(before);
    }
    if before.current.revision > 0 && before.sealed.as_ref() != Some(&before.current) {
        return Err(
            "Confirm the previous fingerprint registration before adding another value.".into(),
        );
    }
    tx.execute(
        "INSERT OR IGNORE INTO secret_fingerprints VALUES(?1,?2,?3,?4,?5)",
        params![
            request.session_id,
            request.track_id,
            fingerprint.name,
            fingerprint.byte_len as i64,
            fingerprint.tag.as_slice()
        ],
    )?;
    if read(&tx, &request.track_id, &request.session_id)?.len() > 256 {
        return Err("Too many saved secret fingerprints for this session.".into());
    }
    tx.execute(
        "INSERT INTO secret_registrations VALUES(?1,?2)",
        params![request.op_id.to_string(), digest.as_slice()],
    )?;
    advance(&tx, &before)?;
    let result = catalog(&tx)?;
    tx.commit()?;
    Ok(result)
}
pub(crate) fn advance(tx: &rusqlite::Transaction<'_>, before: &Catalog) -> Result<()> {
    let next = before
        .current
        .revision
        .checked_add(1)
        .ok_or("Fingerprint revision overflow.")?;
    if next > 9_007_199_254_740_991 {
        return Err("Fingerprint revision limit reached.".into());
    }
    let digest = fingerprint_digest(tx)?;
    tx.execute(
        "UPDATE secret_catalog SET revision=?1,digest=?2 WHERE singleton=1",
        params![next as i64, digest.as_slice()],
    )?;
    Ok(())
}
pub(crate) fn advance_recovery(
    tx: &rusqlite::Transaction<'_>,
    before: &Catalog,
    external_revision: u64,
) -> Result<Catalog> {
    let mut floor = before.clone();
    floor.current.revision = floor.current.revision.max(external_revision);
    advance(tx, &floor)?;
    catalog(tx)
}
