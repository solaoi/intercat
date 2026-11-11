//! Persistence, per-file limits, and worker-input linkage for review fixes.
use crate::{
    Result,
    review_fixes::{Action, Check, Checked, Command, Fix, State},
    reviews::Note,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use uuid::Uuid;
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS review_fixes(id TEXT PRIMARY KEY,
        note TEXT NOT NULL UNIQUE REFERENCES review_notes(id) ON DELETE CASCADE,
        track TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,input TEXT UNIQUE REFERENCES utterances(id) ON DELETE CASCADE,record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS review_fix_track ON review_fixes(track);
        CREATE TABLE IF NOT EXISTS review_fix_inputs(input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,fix TEXT NOT NULL REFERENCES review_fixes(id) ON DELETE CASCADE,record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS review_fix_budget(track TEXT PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,epoch INTEGER NOT NULL,total INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS review_fix_file_budget(track TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,path TEXT NOT NULL,total INTEGER NOT NULL,PRIMARY KEY(track,path));
        CREATE TABLE IF NOT EXISTS review_human_inputs(input TEXT NOT NULL,revision INTEGER NOT NULL,track TEXT NOT NULL,PRIMARY KEY(input,revision,track));")?;
    Ok(())
}
fn save(db: &Connection, fix: &Fix, event: &str) -> Result<()> {
    db.execute("INSERT INTO review_fixes VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET input=excluded.input,record=excluded.record",
        params![fix.id.to_string(),fix.note_id.to_string(),fix.track_id,fix.input_id.map(|id|id.to_string()),serde_json::to_string(fix)?])?;
    if let Some(input) = fix.input_id {
        db.execute("INSERT INTO review_fix_inputs VALUES(?1,?2,?3) ON CONFLICT(input) DO UPDATE SET record=excluded.record",
            params![input.to_string(),fix.id.to_string(),serde_json::to_string(fix)?])?;
    }
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,?2,?3)",
        params![
            crate::journal::timestamp(),
            event,
            json!({"fixId":fix.id,"state":fix.state,"revision":fix.revision}).to_string()
        ],
    )?;
    Ok(())
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Fix> {
    Ok(serde_json::from_str(&db.query_row(
        "SELECT record FROM review_fixes WHERE id=?1",
        [id.to_string()],
        |r| r.get::<_, String>(0),
    )?)?)
}
pub(crate) fn for_input(db: &Connection, input: &str) -> Result<Option<Fix>> {
    db.query_row(
        "SELECT f.record FROM review_fixes f JOIN review_fix_inputs i ON i.fix=f.id WHERE i.input=?1",
        [input],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|s| Ok(serde_json::from_str(&s)?))
    .transpose()
}
pub(crate) fn note(db: &Connection, id: Uuid) -> Result<Note> {
    Ok(serde_json::from_str(&db.query_row(
        "SELECT record FROM review_notes WHERE id=?1",
        [id.to_string()],
        |r| r.get::<_, String>(0),
    )?)?)
}
pub(crate) fn input_projection(db: &Connection, input: &str) -> Result<Option<Value>> {
    let Some(fix) = for_input(db, input)? else {
        return Ok(None);
    };
    let note = note(db, fix.note_id)?;
    Ok(Some(
        json!({"id":fix.id,"state":fix.state,"clipId":note.draft.clip_id,"path":note.draft.path,
        "current":fix.input_id.is_some_and(|id|id.to_string()==input)}),
    ))
}
pub(crate) fn list(db: &Connection) -> Result<Vec<Fix>> {
    db.prepare("SELECT record FROM review_fixes ORDER BY rowid")?
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect()
}
fn budget(db: &Connection, track: &str, path: &str) -> Result<(i64, i64, i64)> {
    let (epoch, total) = db
        .query_row(
            "SELECT epoch,total FROM review_fix_budget WHERE track=?1",
            [track],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
        .unwrap_or((0, 0));
    let file = db
        .query_row(
            "SELECT total FROM review_fix_file_budget WHERE track=?1 AND path=?2",
            params![track, path],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok((epoch, total, file))
}
fn limit_reason(total: i64, file: i64, path: &str) -> Option<String> {
    if file >= 2 {
        Some(format!("Reviewer still flags {path} after {file} fixes"))
    } else if total >= 4 {
        Some(format!(
            "Reviewer still has required findings after {total} fixes on this track"
        ))
    } else {
        None
    }
}
fn input(db: &Transaction<'_>, fix: &mut Fix, note: &Note, explicit: bool) -> Result<()> {
    let job = crate::review_store::read(db, note.review_id)?.ok_or("The review is unavailable.")?;
    let owner = crate::plan_execution::turn_owner(db, &job.target.session_id, &job.target.turn_id)?;
    if let Some((plan, item)) = owner {
        let plan = crate::plan_store::read(db, plan)?;
        let item = plan
            .items
            .iter()
            .find(|value| value.plan_item_id == item)
            .ok_or("The original plan item is unavailable.")?;
        if !crate::plan_continuation::allowed(db, &plan, item)? {
            fix.state = State::Outdated;
            fix.reason = Some("The original plan item is no longer available.".into());
            return Ok(());
        }
    }
    let id = Uuid::now_v7();
    let text = format!(
        "Address this required review finding, and only the changes needed to resolve it.\n\nFile: {}\nReviewed lines: {}–{}\nReviewed checkpoint: {}\nFinding: {}\n\nRead the latest file: other lines may have moved. Preserve unrelated work. If you disagree that a change is needed, explain why on a standalone line 'Review verdict: disagree — <reason>' before your final Result footer; do not silently change unrelated code. Otherwise use 'Review verdict: agree — <what you changed>' before the final Result footer. Keep the user's original report language.",
        note.draft.path,
        note.draft.start_line,
        note.draft.end_line,
        note.target_sha,
        note.draft.text
    );
    crate::journal::insert_utterance(
        db,
        &id.to_string(),
        &text,
        &crate::decision::Target::Lock(fix.track_id.clone()),
        Some(&fix.track_id),
        &[],
        None,
        true,
        None,
    )?;
    if let Some((plan, item)) = owner {
        crate::plan_continuation::attach(db, &id.to_string(), plan, item)?;
    }
    let held = !explicit
        && !crate::worker_hold::turn_allowed(
            db,
            &fix.track_id,
            &job.target.session_id,
            &job.target.turn_id,
        )?;
    if held {
        db.execute(
            "UPDATE utterances SET state='held',reason='Worker stopped' WHERE id=?1",
            [id.to_string()],
        )?;
    }
    db.execute("UPDATE utterances SET state=CASE WHEN state='held' THEN state ELSE 'placed' END,decision=?2 WHERE id=?1",
        params![id.to_string(),json!({"kind":"schedule","trackId":fix.track_id,"boundary":"after-turn","command":null,"confidence":1.0,"inferenceMs":0.0,"timedOut":false,"scores":{}}).to_string()])?;
    fix.input_id = Some(id);
    let state: String = db.query_row(
        "SELECT state FROM utterances WHERE id=?1",
        [id.to_string()],
        |r| r.get(0),
    )?;
    fix.state = if state == "held" {
        State::Held
    } else {
        State::Planned
    };
    Ok(())
}
pub(crate) fn enqueue_with_reason(
    db: &Transaction<'_>,
    note: &Note,
    track: &str,
    forced: Option<String>,
) -> Result<()> {
    if !note.draft.blocking || note.outdated {
        return Ok(());
    }
    if db.query_row(
        "SELECT EXISTS(SELECT 1 FROM review_fixes WHERE note=?1)",
        [note.id.to_string()],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(());
    }
    let (_, total, file) = budget(db, track, &note.draft.path)?;
    let reason = forced.or_else(|| limit_reason(total, file, &note.draft.path));
    let mut fix = Fix {
        id: Uuid::now_v7(),
        revision: 1,
        note_id: note.id,
        track_id: track.into(),
        input_id: None,
        state: if reason.is_some() {
            State::Blocked
        } else {
            State::Planned
        },
        checked_sha: None,
        charge_epoch: None,
        extra: false,
        reason,
        completed_sha: None,
    };
    let blocked = fix.reason.is_some();
    input(db, &mut fix, note, false)?;
    if blocked && fix.state != State::Outdated {
        fix.state = State::Blocked;
        if let Some(input) = fix.input_id {
            db.execute("UPDATE utterances SET state='held',reason='Review fix needs your decision' WHERE id=?1",[input.to_string()])?;
        }
    }
    save(db, &fix, "review.fix-created")
}
pub(crate) fn prepare(db: &Connection, input: &str) -> Result<Option<Check>> {
    let Some(fix) = for_input(db, input)? else {
        return Ok(None);
    };
    if fix.input_id.map(|id| id.to_string()).as_deref() != Some(input)
        || !matches!(fix.state, State::Planned | State::Held)
    {
        return Err(crate::journal::StartSuperseded.into());
    }
    let raw: String = db.query_row(
        "SELECT record FROM workers WHERE track_id=?1",
        [&fix.track_id],
        |r| r.get(0),
    )?;
    let worker: crate::runtime::WorkerState = serde_json::from_str(&raw)?;
    if worker.active_turn.is_some() || worker.checkpoint_pending {
        return Err(crate::journal::StartSuperseded.into());
    }
    let checkpoint = worker
        .checkpoint
        .ok_or("Wait for the worker checkpoint before fixing the finding.")?
        .sha;
    let root: String = db.query_row(
        "SELECT json_extract(record,'$.path') FROM workspaces WHERE track_id=?1",
        [&fix.track_id],
        |r| r.get(0),
    )?;
    Ok(Some(Check {
        note: note(db, fix.note_id)?,
        fix,
        checkpoint,
        workspace: root.into(),
    }))
}
pub(crate) fn checked(db: &mut Connection, check: &Check, outdated: bool) -> Result<Checked> {
    let tx = db.transaction()?;
    let mut fix = read(&tx, check.fix.id)?;
    if fix.revision != check.fix.revision || !matches!(fix.state, State::Planned | State::Held) {
        return Err(crate::journal::StartSuperseded.into());
    }
    let latest: Option<String> = tx
        .query_row(
            "SELECT json_extract(record,'$.checkpoint.sha') FROM workers WHERE track_id=?1",
            [&fix.track_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if latest.as_deref() != Some(&check.checkpoint) {
        return Ok(Checked::Retry);
    }
    if outdated {
        fix.state = State::Outdated;
        fix.reason = Some("may be outdated".into());
        fix.revision += 1;
        let mut note = note(&tx, fix.note_id)?;
        note.outdated = true;
        tx.execute(
            "UPDATE review_notes SET record=?2 WHERE id=?1",
            params![note.id.to_string(), serde_json::to_string(&note)?],
        )?;
        tx.execute("UPDATE utterances SET state='cut',revision=revision+1,reason='Review finding may be outdated' WHERE id=?1",[fix.input_id.unwrap().to_string()])?;
        save(&tx, &fix, "review.fix-outdated")?;
        tx.commit()?;
        return Ok(Checked::Outdated);
    }
    fix.state = State::Planned;
    fix.checked_sha = Some(check.checkpoint.clone());
    save(&tx, &fix, "review.fix-checked")?;
    tx.commit()?;
    Ok(Checked::Ready)
}
/// Runs inside the same transaction as turn.start. An unknown send keeps its charge.
pub(crate) fn reserve(db: &Connection, input: &str, track: &str) -> Result<bool> {
    let Some(mut fix) = for_input(db, input)? else {
        return Ok(true);
    };
    let note = note(db, fix.note_id)?;
    let latest: Option<String> = db
        .query_row(
            "SELECT json_extract(record,'$.checkpoint.sha') FROM workers WHERE track_id=?1",
            [&fix.track_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if fix.input_id.map(|id| id.to_string()).as_deref() != Some(input)
        || fix.track_id != track
        || fix.state != State::Planned
        || fix.checked_sha.is_none()
        || fix.checked_sha != latest
        || note.outdated
    {
        return Err(crate::journal::StartSuperseded.into());
    }
    let (epoch, total, file) = budget(db, &fix.track_id, &note.draft.path)?;
    if !fix.extra
        && let Some(reason) = limit_reason(total, file, &note.draft.path)
    {
        fix.state = State::Blocked;
        fix.reason = Some(reason);
        fix.revision += 1;
        db.execute("UPDATE utterances SET state='held',reason='Review fix needs your decision',revision=revision+1 WHERE id=?1",[input])?;
        save(db, &fix, "review.fix-blocked")?;
        return Ok(false);
    }
    db.execute("INSERT INTO review_fix_budget VALUES(?1,?2,1) ON CONFLICT(track) DO UPDATE SET total=total+1",params![fix.track_id,epoch])?;
    db.execute("INSERT INTO review_fix_file_budget VALUES(?1,?2,1) ON CONFLICT(track,path) DO UPDATE SET total=total+1",params![fix.track_id,note.draft.path])?;
    fix.charge_epoch = Some(epoch);
    fix.state = State::Reserved;
    save(db, &fix, "review.fix-reserved")?;
    Ok(true)
}
pub(crate) fn refund(db: &Connection, operation: &str) -> Result<()> {
    let input: Option<String> = db
        .query_row(
            "SELECT id FROM utterances WHERE dispatch_command=?1",
            [operation],
            |r| r.get(0),
        )
        .optional()?;
    let Some(input) = input else { return Ok(()) };
    let Some(mut fix) = for_input(db, &input)? else {
        return Ok(());
    };
    if fix.input_id.map(|id| id.to_string()).as_deref() != Some(&input)
        || fix.state != State::Reserved
    {
        return Ok(());
    }
    let note = note(db, fix.note_id)?;
    let (epoch, _, _) = budget(db, &fix.track_id, &note.draft.path)?;
    if fix.charge_epoch == Some(epoch) {
        db.execute(
            "UPDATE review_fix_budget SET total=max(0,total-1) WHERE track=?1",
            [&fix.track_id],
        )?;
        db.execute(
            "UPDATE review_fix_file_budget SET total=max(0,total-1) WHERE track=?1 AND path=?2",
            params![fix.track_id, note.draft.path],
        )?;
    }
    fix.charge_epoch = None;
    fix.state = State::Held;
    fix.revision += 1;
    save(db, &fix, "review.fix-not-sent")
}
fn reset(db: &Connection, track: &str) -> Result<()> {
    db.execute("INSERT INTO review_fix_budget VALUES(?1,1,0) ON CONFLICT(track) DO UPDATE SET epoch=epoch+1,total=0",[track])?;
    db.execute("DELETE FROM review_fix_file_budget WHERE track=?1", [track])?;
    Ok(())
}
pub(crate) fn human_input(db: &Connection, input: &str, revision: i64, track: &str) -> Result<()> {
    if for_input(db, input)?.is_some() {
        return Ok(());
    }
    if db.execute(
        "INSERT OR IGNORE INTO review_human_inputs VALUES(?1,?2,?3)",
        params![input, revision, track],
    )? == 1
    {
        reset(db, track)?;
        for mut fix in list(db)?
            .into_iter()
            .filter(|fix| fix.track_id == track && fix.state == State::Blocked)
        {
            fix.state = State::Superseded;
            fix.reason = Some("A new instruction replaced this review decision.".into());
            fix.revision += 1;
            if let Some(input) = fix.input_id {
                db.execute("UPDATE utterances SET state='cut',revision=revision+1 WHERE id=?1 AND state IN ('placed','held')",[input.to_string()])?;
            }
            save(db, &fix, "review.fix-superseded")?;
        }
    }
    Ok(())
}
pub(crate) fn command(db: &mut Connection, command: &Command) -> Result<Fix> {
    if command.op_id.get_version_num() != 7 {
        return Err("Invalid review choice identity.".into());
    }
    let tx = db.transaction()?;
    let raw = serde_json::to_string(command)?;
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [command.op_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, payload)) = old {
        if kind != "review.fix-command" || payload != raw {
            return Err("Operation ID reused.".into());
        }
        return read(&tx, command.fix_id);
    }
    let mut fix = read(&tx, command.fix_id)?;
    if fix.revision != command.revision || fix.state != State::Blocked {
        return Err("This review finding changed. Reload it before choosing.".into());
    }
    match command.action {
        Action::KeepIt => {
            fix.state = State::Kept;
            fix.reason = Some("kept as is".into());
            reset(&tx, &fix.track_id)?;
            if let Some(input) = fix.input_id {
                tx.execute("UPDATE utterances SET state='cut',revision=revision+1 WHERE id=?1 AND state IN ('placed','held')",[input.to_string()])?;
            }
        }
        Action::FixOnceMore => {
            if crate::dispatch_control::paused(&tx)? {
                return Err("Resume new work before fixing the finding.".into());
            }
            fix.extra = true;
            fix.state = State::Planned;
            fix.reason = None;
            fix.checked_sha = None;
            fix.charge_epoch = None;
            fix.completed_sha = None;
            let note = note(&tx, fix.note_id)?;
            let reused = if let Some(input) = fix.input_id {
                tx.execute("UPDATE utterances SET state='placed',revision=revision+1,dispatch_command=NULL,reason=NULL WHERE id=?1 AND state='held'",[input.to_string()])?==1
            } else {
                false
            };
            if !reused {
                input(&tx, &mut fix, &note, true)?;
            }
        }
    }
    fix.revision += 1;
    save(&tx, &fix, "review.fix-decided")?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'review.fix-command',?2,'complete')",
        params![command.op_id.to_string(), raw],
    )?;
    tx.commit()?;
    Ok(fix)
}
pub(crate) fn cut(db: &Connection, input: &str) -> Result<()> {
    if let Some(mut fix) = for_input(db, input)? {
        if fix.input_id.map(|id| id.to_string()).as_deref() != Some(input)
            || !matches!(fix.state, State::Planned | State::Held)
        {
            return Err("This fix has already started.".into());
        }
        fix.state = State::Cut;
        fix.revision += 1;
        save(db, &fix, "review.fix-cut")?;
    }
    Ok(())
}
pub(crate) fn history(db: &Connection, input: &str, state: &str) -> Result<()> {
    let Some(mut fix) = for_input(db, input)? else {
        return Ok(());
    };
    if fix.input_id.map(|id| id.to_string()).as_deref() != Some(input) {
        return Err("This review fix has a newer attempt.".into());
    }
    if state == "cut" {
        fix.state = State::Cut;
    } else if fix.state == State::Cut {
        fix.state = if state == "held" {
            State::Held
        } else {
            State::Planned
        };
        fix.checked_sha = None;
    } else {
        return Ok(());
    }
    fix.revision += 1;
    save(db, &fix, "review.fix-history")
}
pub(crate) fn projection(db: &Connection) -> Result<Vec<Value>> {
    list(db)?.into_iter().map(|fix|{
        let note=note(db,fix.note_id)?;let(_,total,file)=budget(db,&fix.track_id,&note.draft.path)?;
        let attempts=db.prepare("SELECT record FROM review_fix_inputs WHERE fix=?1 ORDER BY rowid")?.query_map([fix.id.to_string()],|r|r.get::<_,String>(0))?
            .map(|r| Ok(serde_json::from_str::<Value>(&r?)?)).collect::<Result<Vec<_>>>()?;
        Ok(json!({"id":fix.id,"revision":fix.revision,"noteId":fix.note_id,"inputId":fix.input_id,"trackId":fix.track_id,"state":fix.state,
            "path":note.draft.path,"clipId":note.draft.clip_id,"text":note.draft.text,"reason":fix.reason,"fileAttempts":file,"trackAttempts":total,"attempts":attempts}))
    }).collect()
}
pub(crate) fn observe_worker(db: &Connection, worker: &crate::runtime::WorkerState) -> Result<()> {
    let Some(session) = &worker.session_id else {
        return Ok(());
    };
    if let Some(turn) = worker.active_turn {
        for mut fix in list(db)?
            .into_iter()
            .filter(|fix| fix.track_id == worker.track_id && fix.state == State::Reserved)
        {
            if let Some(input) = fix.input_id
                && crate::input_origin::read(db, input)?.is_some_and(|origin| {
                    origin.identity.session_id == *session && origin.identity.turn_id == turn
                })
            {
                fix.state = State::Running;
                fix.revision += 1;
                save(db, &fix, "review.fix-running")?;
            }
        }
        return Ok(());
    }
    let Some(checkpoint) = &worker.checkpoint else {
        return Ok(());
    };
    if worker.checkpoint_pending {
        return Ok(());
    }
    for mut fix in list(db)?.into_iter().filter(|f| {
        f.track_id == worker.track_id && matches!(f.state, State::Reserved | State::Running)
    }) {
        let Some(input) = fix.input_id else { continue };
        let Some(origin) = crate::input_origin::read(db, input)? else {
            continue;
        };
        if origin.identity.session_id != *session || origin.identity.turn_id != checkpoint.turn_id {
            continue;
        }
        let report =
            crate::report_result::final_report(db, session, &checkpoint.turn_id.to_string())?;
        let raw:Option<String>=db.query_row("SELECT json_extract(record,'$.text') FROM timeline_items WHERE session=?1 AND json_extract(record,'$.turnId')=?2 AND json_extract(record,'$.messagePhase')='finalAnswer' ORDER BY rowid DESC LIMIT 1",params![session,checkpoint.turn_id.to_string()],|r|r.get(0)).optional()?;
        let disagree = raw.as_deref().and_then(disagreement);
        if let Some(reason) = disagree {
            fix.state = State::Blocked;
            fix.reason = Some(format!(
                "Worker disagrees: {}",
                crate::privacy::truncate(reason, 500)
            ));
        } else if report.as_ref().is_some_and(|r| r["kind"] == "done") {
            fix.state = State::AwaitingReview;
            fix.completed_sha = Some(checkpoint.sha.clone());
        } else {
            fix.state = State::Blocked;
            fix.reason = Some("The worker could not complete this fix. Review its report.".into());
        }
        fix.revision += 1;
        save(db, &fix, "review.fix-finished")?;
    }
    Ok(())
}

/// Only the standalone line directly before a valid final footer is authoritative.
pub(crate) fn disagreement(text: &str) -> Option<&str> {
    crate::report_result::parse(text)?;
    let mut lines = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty() && !line.starts_with("Waiting for you:"));
    lines.next()?;
    lines
        .next()?
        .strip_prefix("Review verdict: disagree — ")
        .filter(|reason| !reason.trim().is_empty())
}

pub(crate) fn pending(
    db: &Connection,
    track: &str,
    sha: &str,
) -> Result<Vec<crate::reviews::PendingFix>> {
    list(db)?
        .into_iter()
        .filter(|f| {
            f.track_id == track
                && f.state == State::AwaitingReview
                && f.completed_sha.as_deref() == Some(sha)
        })
        .map(|f| {
            Ok(crate::reviews::PendingFix {
                fix_id: f.id,
                note: note(db, f.note_id)?,
            })
        })
        .collect()
}

pub(crate) fn reviewed(
    db: &Connection,
    target: &crate::reviews::Target,
    resolutions: &[crate::reviews::Resolution],
) -> Result<Option<String>> {
    let mut forced = None;
    for pending in &target.pending_fixes {
        let resolution = resolutions
            .iter()
            .find(|r| r.note_id == pending.note.id)
            .ok_or("Missing fix resolution.")?;
        let mut fix = read(db, pending.fix_id)?;
        if fix.state != State::AwaitingReview
            || fix.completed_sha.as_deref() != Some(&target.checkpoint.sha)
        {
            return Err("The reviewed fix changed.".into());
        }
        if resolution.remaining.is_some() {
            fix.state = State::Unresolved;
            if fix.extra
                && fix.charge_epoch == Some(budget(db, &fix.track_id, &pending.note.draft.path)?.0)
            {
                forced = Some("Reviewer still has required findings after the extra fix".into());
            }
        } else {
            fix.state = State::Resolved;
        }
        fix.reason = Some(resolution.explanation.clone());
        fix.revision += 1;
        save(db, &fix, "review.fix-reviewed")?;
    }
    Ok(forced)
}
