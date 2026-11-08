//! Review queue and publication share the journal's single writer.
use crate::{
    Result,
    reviews::{self, Clip, Finished, Job, Note, State, Target},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS review_jobs(
        id TEXT PRIMARY KEY,track TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS review_state ON review_jobs(json_extract(record,'$.state'));
        CREATE TABLE IF NOT EXISTS review_targets(job TEXT NOT NULL REFERENCES review_jobs(id) ON DELETE CASCADE,
            clip TEXT NOT NULL REFERENCES timeline_clips(id) ON DELETE CASCADE,PRIMARY KEY(job,clip));
        CREATE TABLE IF NOT EXISTS review_notes(id TEXT PRIMARY KEY,job TEXT NOT NULL REFERENCES review_jobs(id) ON DELETE CASCADE,
            clip TEXT NOT NULL REFERENCES timeline_clips(id) ON DELETE CASCADE,record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS review_notes_clip ON review_notes(clip);
        UPDATE review_jobs SET record=json_set(record,'$.state','held','$.detail','Review was interrupted. Retry to review the saved checkpoint.')
            WHERE json_extract(record,'$.state')='running';")?;
    for event in ["INSERT", "UPDATE", "DELETE"] {
        let row = if event == "DELETE" { "OLD" } else { "NEW" };
        db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS review_note_{} AFTER {event} ON review_notes BEGIN
            UPDATE timeline_revision SET revision=revision+1;
            INSERT INTO timeline_warning_versions SELECT {row}.clip,revision FROM timeline_revision WHERE true
                ON CONFLICT(clip) DO UPDATE SET version=excluded.version;
        END;",event.to_lowercase()))?;
    }
    Ok(())
}
fn write(db: &Connection, job: &Job) -> Result<()> {
    db.execute("INSERT INTO review_jobs VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![job.id.to_string(),job.target.track_id,serde_json::to_string(job)?])?;
    Ok(())
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Option<Job>> {
    db.query_row(
        "SELECT record FROM review_jobs WHERE id=?1",
        [id.to_string()],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|s| Ok(serde_json::from_str(&s)?))
    .transpose()
}
pub(crate) fn list(db: &Connection) -> Result<Vec<Job>> {
    db.prepare("SELECT record FROM review_jobs ORDER BY rowid")?
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect()
}
pub(crate) fn observe(db: &Connection, state: &crate::runtime::WorkerState) -> Result<()> {
    if !matches!(state.phase.as_str(), "ready" | "reviewing-fix")
        || state.active_turn.is_some()
        || state.checkpoint_pending
    {
        return Ok(());
    }
    let (Some(session), Some(checkpoint)) = (&state.session_id, &state.checkpoint) else {
        return Ok(());
    };
    let turn = checkpoint.turn_id.to_string();
    if crate::report_result::terminal_status(db, session, &turn)?.as_deref() != Some("completed") {
        return Ok(());
    }
    // A disputed or failed correction requires a human choice, not another automatic review.
    if crate::review_fix_store::list(db)?.iter().any(|fix| {
        fix.track_id == state.track_id && fix.state == crate::review_fixes::State::Blocked
    }) {
        return Ok(());
    }
    let row:Option<(String,i64)>=db.query_row("SELECT item,revision FROM timeline_items WHERE session=?1
        AND json_extract(record,'$.turnId')=?2 AND json_extract(record,'$.kind')='agentMessage'
        AND json_extract(record,'$.messagePhase')='finalAnswer' AND json_extract(record,'$.status')='completed'
        AND json_extract(record,'$._intercatTruncated') IS NULL ORDER BY rowid DESC LIMIT 1",
        params![session,turn],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((report_id, report_revision)) = row else {
        return Ok(());
    };
    let workspace: Option<String> = db
        .query_row(
            "SELECT record FROM workspaces WHERE track_id=?1",
            [&state.track_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(workspace) = workspace else {
        return Ok(());
    };
    let workspace: crate::preparation::Workspace = serde_json::from_str(&workspace)?;
    if workspace.state != "ready" {
        return Ok(());
    }
    let missing:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM timeline_items i JOIN timeline_clips c ON c.id=i.clip
        WHERE i.session=?1 AND json_extract(c.record,'$.turnId')=?2 AND json_extract(i.record,'$.kind')='toolCall'
        AND NOT EXISTS(SELECT 1 FROM step_file_sources s JOIN step_source_clips b ON b.source=s.id,json_each(s.record,'$.calls') k
          WHERE b.clip=i.clip AND k.value=json_extract(i.record,'$.callId') AND json_type(s.record,'$.paths')='array'))",
        params![session,turn],|r|r.get(0))?;
    if missing {
        return Ok(());
    }
    let rows=db.prepare("SELECT c.id,c.ordinal,json_extract(s.record,'$.paths') FROM timeline_clips c
        JOIN step_source_clips b ON b.clip=c.id JOIN step_file_sources s ON s.id=b.source
        WHERE c.track=?1 AND json_extract(c.record,'$.sessionId')=?2
        AND json_extract(c.record,'$.turnId')=?3 AND json_extract(c.record,'$.category')='step'
        AND json_extract(c.record,'$.repairRunId') IS NULL AND json_extract(c.record,'$.end') IS NOT NULL
        AND json_type(s.record,'$.paths')='array' ORDER BY c.ordinal")?
        .query_map(params![state.track_id,session,turn],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut clips = rows
        .into_iter()
        .map(|(id, ordinal, paths)| {
            Ok(Clip {
                id,
                ordinal,
                paths: serde_json::from_str(&paths)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut owned = std::collections::BTreeSet::new();
    for clip in clips.iter_mut().rev() {
        clip.paths.retain(|path| owned.insert(path.clone()));
    }
    let pending_fixes = crate::review_fix_store::pending(db, &state.track_id, &checkpoint.sha)?;
    for pending in &pending_fixes {
        if !clips
            .iter()
            .any(|clip| clip.paths.contains(&pending.note.draft.path))
        {
            let original =
                read(db, pending.note.review_id)?.ok_or("The original review is unavailable.")?;
            if let Some(clip) = original
                .target
                .clips
                .iter()
                .find(|clip| clip.id == pending.note.draft.clip_id)
            {
                if let Some(existing) = clips.iter_mut().find(|value| value.id == clip.id) {
                    existing.paths.push(pending.note.draft.path.clone());
                } else {
                    clips.push(Clip {
                        id: clip.id.clone(),
                        ordinal: clip.ordinal,
                        paths: vec![pending.note.draft.path.clone()],
                    });
                }
            }
        }
    }
    if clips.is_empty() {
        return Ok(());
    }
    let target = Target {
        track_id: state.track_id.clone(),
        session_id: session.clone(),
        turn_id: turn,
        checkpoint: checkpoint.clone(),
        workspace,
        clips,
        report_id,
        report_revision,
        pending_fixes,
    };
    let id = reviews::identity(&target);
    if read(db, id)?.is_some() {
        return Ok(());
    }
    let job = Job {
        id,
        generation: Uuid::now_v7(),
        revision: 1,
        state: if crate::dispatch_control::paused(db)? {
            State::Held
        } else {
            State::Queued
        },
        target,
        summary: None,
        detail: None,
    };
    write(db, &job)?;
    for clip in &job.target.clips {
        db.execute(
            "INSERT INTO review_targets VALUES(?1,?2)",
            params![job.id.to_string(), clip.id],
        )?;
    }
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'review.queued',?2)",
        params![
            crate::journal::timestamp(),
            json!({"id":job.id,"targetSha":job.target.checkpoint.sha}).to_string()
        ],
    )?;
    Ok(())
}
pub(crate) fn context(db: &Connection, job: &Job) -> Result<Value> {
    crate::session_retirement::ensure_open(db, &job.target.session_id)?;
    let current = read(db, job.id)?.ok_or("The review no longer exists.")?;
    if current.generation != job.generation
        || current.revision != job.revision
        || current.state != State::Running
    {
        return Err("The review changed.".into());
    }
    let row: Option<(i64, String)> = db
        .query_row(
            "SELECT revision,record FROM timeline_items WHERE session=?1 AND item=?2",
            params![job.target.session_id, job.target.report_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((revision, raw)) = row else {
        return Err("The reviewed report is unavailable.".into());
    };
    if revision != job.target.report_revision {
        return Err("The reviewed report changed.".into());
    }
    let report: Value = serde_json::from_str(&raw)?;
    let text = report["text"].as_str().ok_or("The report has no text.")?;
    if text.len() > 64 * 1024 {
        return Err("The report exceeds the review limit.".into());
    }
    Ok(
        json!({"report":text,"clips":job.target.clips,"targetSha":job.target.checkpoint.sha,"priorFindings":job.target.pending_fixes}),
    )
}
pub(crate) fn begin(
    db: &mut Connection,
    id: Uuid,
    generation: Uuid,
    revision: u64,
) -> Result<Option<Job>> {
    let tx = db.transaction()?;
    if crate::dispatch_control::paused(&tx)? {
        return Ok(None);
    }
    let Some(mut job) = read(&tx, id)? else {
        return Ok(None);
    };
    crate::session_retirement::ensure_open(&tx, &job.target.session_id)?;
    if job.state != State::Queued || job.generation != generation || job.revision != revision {
        return Ok(None);
    }
    job.state = State::Running;
    write(&tx, &job)?;
    crate::review_timeline::begin(&tx, &job)?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'review.started',?2)",
        params![
            crate::journal::timestamp(),
            json!({"reviewId":job.id,"generation":job.generation}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(Some(job))
}
pub(crate) fn finish(db: &mut Connection, expected: &Job, outcome: Finished) -> Result<bool> {
    let tx = db.transaction()?;
    let Some(mut job) = read(&tx, expected.id)? else {
        return Ok(false);
    };
    if job.state != State::Running
        || job.generation != expected.generation
        || job.revision != expected.revision
    {
        return Ok(false);
    }
    match outcome {
        Finished::Complete {
            draft,
            checked_sha,
            outdated,
        } => {
            reviews::validate(&draft, &job.target)?;
            let report_revision: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM timeline_items WHERE session=?1 AND item=?2",
                    params![job.target.session_id, job.target.report_id],
                    |row| row.get(0),
                )
                .optional()?;
            if report_revision != Some(job.target.report_revision) {
                return Ok(false);
            }
            let latest: Option<String> = tx
                .query_row(
                    "SELECT json_extract(record,'$.checkpoint.sha') FROM workers WHERE track_id=?1",
                    [&job.target.track_id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if latest.as_deref() != Some(&checked_sha) {
                return Ok(false);
            }
            if !job.target.pending_fixes.is_empty() && checked_sha != job.target.checkpoint.sha {
                return Err("The correction checkpoint changed before re-review finished.".into());
            }
            if outdated.len() != draft.notes.len() {
                return Err("Missing review freshness results.".into());
            }
            let forced = crate::review_fix_store::reviewed(&tx, &job.target, &draft.resolutions)?;
            let notes =
                draft
                    .notes
                    .into_iter()
                    .zip(outdated)
                    .chain(draft.resolutions.iter().filter_map(|resolution| {
                        resolution.remaining.clone().map(|note| (note, false))
                    }));
            for (index, (draft, outdated)) in notes.enumerate() {
                let note = Note {
                    id: Uuid::new_v5(&job.id, index.to_string().as_bytes()),
                    review_id: job.id,
                    target_sha: job.target.checkpoint.sha.clone(),
                    checked_sha: checked_sha.clone(),
                    outdated,
                    fix: None,
                    draft,
                };
                tx.execute(
                    "INSERT INTO review_notes VALUES(?1,?2,?3,?4)",
                    params![
                        note.id.to_string(),
                        job.id.to_string(),
                        note.draft.clip_id,
                        serde_json::to_string(&note)?
                    ],
                )?;
                crate::review_fix_store::enqueue_with_reason(
                    &tx,
                    &note,
                    &job.target.track_id,
                    forced.clone(),
                )?;
            }
            job.summary = Some(draft.summary);
            job.state = State::Complete;
        }
        Finished::Failed(detail) => {
            job.detail = Some(detail);
            job.state = State::Failed
        }
        Finished::Held => {
            job.detail =
                Some("Review was interrupted. Retry to review the saved checkpoint.".into());
            job.state = State::Held
        }
    }
    write(&tx, &job)?;
    crate::review_timeline::finish(&tx, &job)?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'review.finished',?2)",
        params![crate::journal::timestamp(), serde_json::to_string(&job)?],
    )?;
    tx.commit()?;
    Ok(true)
}
pub(crate) fn notes(db: &Connection, clip: &str) -> Result<Vec<Note>> {
    db.prepare("SELECT record FROM review_notes WHERE clip=?1 ORDER BY rowid")?
        .query_map([clip], |r| r.get::<_, String>(0))?
        .map(|row| {
            let mut note: Note = serde_json::from_str(&row?)?;
            let fix: Option<String> = db
                .query_row(
                    "SELECT record FROM review_fixes WHERE note=?1",
                    [note.id.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(fix) = fix {
                let fix: crate::review_fixes::Fix = serde_json::from_str(&fix)?;
                let input = fix
                    .input_id
                    .map(|id| {
                        db.query_row(
                            "SELECT state,revision FROM utterances WHERE id=?1",
                            [id.to_string()],
                            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                        )
                    })
                    .transpose()?;
                let state = if matches!(
                    fix.state,
                    crate::review_fixes::State::Planned | crate::review_fixes::State::Held
                ) && input.as_ref().is_some_and(|(state, _)| state == "held")
                {
                    crate::review_fixes::State::Held
                } else {
                    fix.state
                };
                note.fix = Some(reviews::NoteFix {
                    state,
                    detail: fix.reason,
                    input_id: fix.input_id,
                    input_revision: input.map(|(_, revision)| revision),
                });
            }
            Ok(note)
        })
        .collect()
}
pub(crate) fn stop(db: &mut Connection, request: &reviews::Stop) -> Result<Job> {
    if request.op_id.get_version_num() != 7 {
        return Err("Invalid review stop identity.".into());
    }
    let tx = db.transaction()?;
    let payload = serde_json::to_string(request)?;
    if let Some((kind, old)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if kind != "review.stop" || old != payload {
            return Err("Operation ID reused.".into());
        }
        return read(&tx, request.review_id)?.ok_or_else(|| "The review is unavailable.".into());
    }
    let mut job = read(&tx, request.review_id)?.ok_or("The review is unavailable.")?;
    if job.revision != request.revision || !matches!(job.state, State::Running | State::Queued) {
        return Err("The review changed before it could be stopped.".into());
    }
    job.state = State::Held;
    job.revision += 1;
    job.detail = Some("Review stopped by you. Retry to review this checkpoint.".into());
    write(&tx, &job)?;
    crate::review_timeline::finish(&tx, &job)?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'review.stop',?2,'complete')",
        params![request.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'review.stopped',?2)",
        params![crate::journal::timestamp(), serde_json::to_string(&job)?],
    )?;
    tx.commit()?;
    Ok(job)
}
pub(crate) fn notes_for_job(db: &Connection, job: Uuid) -> Result<Vec<Note>> {
    let clips = db
        .prepare("SELECT DISTINCT clip FROM review_notes WHERE job=?1")?
        .query_map([job.to_string()], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut output = Vec::new();
    for clip in clips {
        output.extend(
            notes(db, &clip)?
                .into_iter()
                .filter(|note| note.review_id == job),
        );
    }
    Ok(output)
}

pub(crate) fn hold_all(db: &Connection) -> Result<()> {
    db.execute("UPDATE review_jobs SET record=json_set(record,'$.state','held','$.detail','Review was stopped. Retry to review the saved checkpoint.') WHERE json_extract(record,'$.state') IN ('queued','running')",[])?;
    crate::review_timeline::interrupt_open(db)
}
pub(crate) fn retry(db: &mut Connection, request: &reviews::Retry) -> Result<Job> {
    let tx = db.transaction()?;
    let payload = serde_json::to_string(request)?;
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = old {
        if kind != "review.retry" || old != payload {
            return Err("Operation ID reused.".into());
        }
        return read(&tx, request.review_id)?.ok_or_else(|| "Review is unavailable.".into());
    }
    if crate::dispatch_control::paused(&tx)? {
        return Err("Resume new work before retrying this review.".into());
    }
    let mut job = read(&tx, request.review_id)?.ok_or("Review is unavailable.")?;
    crate::session_retirement::ensure_open(&tx, &job.target.session_id)?;
    if job.revision != request.revision || !matches!(job.state, State::Held | State::Failed) {
        return Err("The review changed. Reload before retrying.".into());
    }
    job.generation = Uuid::now_v7();
    job.revision = job
        .revision
        .checked_add(1)
        .ok_or("Review revision overflow.")?;
    job.state = State::Queued;
    job.detail = None;
    job.target.report_revision = tx.query_row(
        "SELECT revision FROM timeline_items WHERE session=?1 AND item=?2",
        params![job.target.session_id, job.target.report_id],
        |row| row.get(0),
    )?;
    write(&tx, &job)?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'review.retry',?2,'complete')",
        params![request.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'review.retried',?2)",
        params![
            crate::journal::timestamp(),
            json!({"reviewId":job.id,"revision":job.revision}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(job)
}
pub(crate) fn for_clip(db: &Connection, clip: &str) -> Result<Vec<Value>> {
    db.prepare("SELECT j.record FROM review_jobs j JOIN review_targets t ON t.job=j.id WHERE t.clip=?1 ORDER BY j.rowid")?
        .query_map([clip],|r|r.get::<_,String>(0))?.map(|row|{
            let job:Job=serde_json::from_str(&row?)?;
            Ok(json!({"id":job.id,"revision":job.revision,"state":job.state,"summary":job.summary,"detail":job.detail}))
        }).collect()
}
