//! Durable reservation for a genuinely new SDK conversation. Backup recovery
//! and Start fresh retain distinct records and never import a parent history.
use crate::Result;
use rusqlite::Connection;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub(crate) struct Reservation {
    pub session_id: String,
    pub start_command: Uuid,
    pub opened: bool,
}
pub(crate) fn read(db: &Connection, track: &str) -> Result<Option<Reservation>> {
    crate::continuation::ensure_writer(db, track)?;
    let backup = crate::session_retirement::resume(db, track)?;
    let fresh = crate::continuation::start(db, track)?;
    if backup.is_some() && fresh.is_some() {
        return Err("This writer has conflicting new-session reservations.".into());
    }
    if let Some((value, opened)) = backup {
        return Ok(Some(Reservation {
            session_id: value.session_id,
            start_command: value.start_command,
            opened,
        }));
    }
    if let Some(value) = fresh {
        crate::session_retirement::ensure_open(db, &value.session_id)?;
        return Ok(Some(Reservation {
            session_id: value.session_id,
            start_command: value.start_command,
            opened: value.opened,
        }));
    }
    Ok(None)
}
pub(crate) fn opened(db: &Connection, track: &str, session: &str) -> Result<()> {
    let Some(value) = read(db, track)? else {
        return Ok(());
    };
    if value.session_id != session {
        return Err("The reserved new session has a different identity.".into());
    }
    if crate::continuation::start(db, track)?.is_some() {
        crate::continuation::opened(db, track, session)
    } else {
        crate::session_retirement::opened(db, track, session)
    }
}
