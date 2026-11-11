//! The journal binds a confirmation to the project, session and owned branch.
use crate::{
    Result,
    push_review::{Context, Request, Review, Source},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use uuid::Uuid;

fn owned_commit(db: &Connection, session: &str, folder: &str, sha: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM integration_jobs WHERE session=?1 AND json_extract(record,'$.project.id')=?2 AND json_extract(record,'$.status')='done' AND (json_extract(record,'$.candidate.head')=?3 OR json_extract(record,'$.candidate.undo.sha')=?3 OR EXISTS(SELECT 1 FROM json_each(json_extract(record,'$.candidate.commits')) WHERE json_extract(value,'$.sha')=?3)))",params![session,folder,sha],|row|row.get(0))?)
}

pub(crate) fn context(db: &Connection, request: &Request) -> Result<Context> {
    request.validate()?;
    let project = crate::folders::project(db)?.ok_or("Choose a project before Push.")?;
    let raw: String = db.query_row(
        "SELECT value FROM settings WHERE key='session'",
        [],
        |row| row.get(0),
    )?;
    let session: Value = serde_json::from_str(&raw)?;
    if session["bound"] != true {
        return Err("The session is still being prepared.".into());
    }
    let session_id = session["id"]
        .as_str()
        .ok_or("Missing session ID.")?
        .to_owned();
    let session_name = session["name"]
        .as_str()
        .ok_or("Missing session name.")?
        .to_owned();
    let mut sources = vec![];
    for choice in &request.choices {
        let folder = project.folder(&choice.folder_id)?.clone();
        let slug: String = db.query_row(
            "SELECT slug FROM session_folders WHERE session=?1 AND folder=?2",
            params![session_id, folder.id],
            |row| row.get(0),
        )?;
        let reference = format!("refs/heads/intercat/{slug}");
        let head = crate::lineage_store::read_published(db, &session_id, &folder.id)?
            .ok_or("Commit this folder's saved work before opening Push.")?
            .head;
        if let Some(sha) = &choice.through {
            let owned = owned_commit(db, &session_id, &folder.id, sha)?;
            if !owned {
                return Err(
                    "Push to here must select a committed result in this folder and session."
                        .into(),
                );
            }
        }
        sources.push(Source {
            folder,
            slug,
            reference,
            head,
        });
    }
    Ok(Context {
        project_id: project.id,
        session_id,
        session_name,
        sources,
    })
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Option<Review>> {
    let saved: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    saved
        .map(|(kind, raw)| {
            if kind != "push.review" {
                return Err("The operation ID belongs to another action.".into());
            }
            let review: Review = serde_json::from_str(&raw)?;
            if review.request.op_id != id {
                return Err("The saved Push review belongs to another operation.".into());
            }
            review.validate()?;
            Ok(review)
        })
        .transpose()
}
pub(crate) fn begin(db: &mut Connection, review: Review) -> Result<Review> {
    review.validate()?;
    let tx = db.transaction()?;
    if let Some(old) = read(&tx, review.request.op_id)? {
        if old.request != review.request {
            return Err("The Push review operation ID was reused.".into());
        }
        return Ok(old);
    }
    let current = context(&tx, &review.request)?;
    if current.project_id != review.project_id || current.session_id != review.session_id {
        return Err("The project or session changed. Open its current Push review.".into());
    }
    for (source, member) in current.sources.iter().zip(&review.members) {
        if serde_json::to_value(&source.folder)? != serde_json::to_value(&member.folder)?
            || source.slug != member.slug
            || source.reference != member.target.source_ref
        {
            return Err("The folder changed before its Push review was saved.".into());
        }
        if !owned_commit(
            &tx,
            &review.session_id,
            &member.folder.id,
            &member.target.sha,
        )? {
            return Err("The reviewed commit was not made in this folder and session.".into());
        }
    }
    tx.execute(
        "INSERT INTO operations VALUES(?1,'push.review',?2,'prepared')",
        params![
            review.request.op_id.to_string(),
            serde_json::to_string(&review)?
        ],
    )?;
    tx.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'push.review',?3)",
        params![
            crate::journal::timestamp(),
            review.session_id,
            serde_json::json!({"id":review.request.op_id,"folders":review.members.len()})
                .to_string()
        ],
    )?;
    tx.commit()?;
    Ok(review)
}
pub(crate) fn complete(db: &mut Connection, id: Uuid) -> Result<()> {
    let tx = db.transaction()?;
    read(&tx, id)?.ok_or("Missing Push review.")?;
    tx.execute(
        "UPDATE operations SET state='complete' WHERE id=?1 AND kind='push.review'",
        [id.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}
