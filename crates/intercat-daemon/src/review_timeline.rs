//! A saved review attempt occupies the read-only Reviewer lane.
use crate::{
    Result,
    reviews::{Job, State},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) const LANE: &str = "reviewer";
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS review_clips(clip TEXT PRIMARY KEY REFERENCES timeline_clips(id) ON DELETE CASCADE,
        job TEXT NOT NULL REFERENCES review_jobs(id) ON DELETE CASCADE,generation TEXT NOT NULL UNIQUE,
        state TEXT NOT NULL,summary TEXT,detail TEXT,source_label TEXT NOT NULL);")?;
    interrupt_open(db)
}
pub(crate) fn begin(db: &Connection, job: &Job) -> Result<()> {
    let id = Uuid::new_v5(&job.generation, b"review-activity").to_string();
    let label: String = db.query_row(
        "SELECT label FROM tracks WHERE id=?1",
        [&job.target.track_id],
        |r| r.get(0),
    )?;
    crate::timeline::open_review(db, &id, LANE, &job.generation.to_string())?;
    db.execute(
        "INSERT INTO review_clips VALUES(?1,?2,?3,'running',NULL,NULL,?4)",
        params![id, job.id.to_string(), job.generation.to_string(), label],
    )?;
    Ok(())
}
pub(crate) fn finish(db: &Connection, job: &Job) -> Result<()> {
    let clip: Option<String> = db
        .query_row(
            "SELECT clip FROM review_clips WHERE generation=?1",
            [job.generation.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(clip) = clip {
        let state = serde_json::to_value(job.state)?
            .as_str()
            .unwrap()
            .to_owned();
        db.execute(
            "UPDATE review_clips SET state=?2,summary=?3,detail=?4 WHERE clip=?1",
            params![clip, state, job.summary, job.detail],
        )?;
        crate::timeline::close_review(db, &clip, job.state == State::Held)?;
    }
    Ok(())
}
pub(crate) fn interrupt_open(db: &Connection) -> Result<()> {
    let clips = db
        .prepare("SELECT clip FROM review_clips WHERE state='running'")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for clip in clips {
        db.execute("UPDATE review_clips SET state='held',detail='Review was interrupted. Retry to review the saved checkpoint.' WHERE clip=?1",[&clip])?;
        crate::timeline::close_review(db, &clip, true)?;
    }
    Ok(())
}
pub(crate) fn lane(db: &Connection) -> Result<Option<Value>> {
    let jobs = crate::review_store::list(db)?;
    if jobs.is_empty() {
        return Ok(None);
    }
    let active = jobs
        .iter()
        .filter(|job| job.state == State::Running)
        .count();
    let queued = jobs.iter().filter(|job| job.state == State::Queued).count();
    let state = if active > 0 {
        "working"
    } else if queued > 0 {
        "waiting"
    } else if jobs
        .iter()
        .any(|job| matches!(job.state, State::Failed | State::Held))
    {
        "stopped"
    } else {
        "done"
    };
    Ok(Some(
        json!({"id":LANE,"label":"Reviewer","title":"Code review","status":state,"active":active,"queued":queued,"jobs":jobs.iter().filter(|job|matches!(job.state,State::Queued|State::Running)).map(|job|json!({"id":job.id,"revision":job.revision,"state":job.state})).collect::<Vec<_>>()}),
    ))
}
pub(crate) fn decorate(db: &Connection, clip: &str, value: &mut Value) -> Result<()> {
    let (job, label, state): (String, String, String) = db.query_row(
        "SELECT job,source_label,state FROM review_clips WHERE clip=?1",
        [clip],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let required: i64 = db.query_row(
        "SELECT count(*) FROM review_notes WHERE job=?1 AND json_extract(record,'$.blocking')=1",
        [&job],
        |r| r.get(0),
    )?;
    value["kind"] = json!("read");
    value["reviewId"] = json!(job);
    value["reviewState"] = json!(state);
    value["label"] = json!(match state.as_str() {
        "complete" if required > 0 => format!("Review {label} · {required} required"),
        "complete" => format!("Review {label} · complete"),
        "running" => format!("Review {label}"),
        "held" => format!("Review {label} · held"),
        _ => format!("Review {label} · failed"),
    });
    Ok(())
}
pub(crate) fn detail(db: &Connection, clip: Value) -> Result<Value> {
    let id = clip["id"].as_str().ok_or("Missing review clip")?;
    let (job, generation, state, summary, detail): (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    ) = db.query_row(
        "SELECT job,generation,state,summary,detail FROM review_clips WHERE clip=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let job = crate::review_store::read(db, job.parse()?)?.ok_or("The review is unavailable.")?;
    let notes = if state == "complete" {
        crate::review_store::notes_for_job(db, job.id)?
    } else {
        vec![]
    };
    let label: String = db.query_row(
        "SELECT label FROM tracks WHERE id=?1",
        [&job.target.track_id],
        |r| r.get(0),
    )?;
    let targets = job
        .target
        .clips
        .iter()
        .map(|target| json!({"id":target.id,"label":format!("{label}:{}",target.ordinal)}))
        .collect::<Vec<_>>();
    Ok(
        json!({"clip":clip,"items":[],"reviewNotes":notes,"reviewedClips":targets,
        "reviews":[{"id":job.id,"revision":job.revision,"state":state,"summary":summary,"detail":detail,
            "retryable":generation==job.generation.to_string() && matches!(job.state,State::Held|State::Failed)}]}),
    )
}
