//! All candidates are admitted in one SQLite transaction. Git publication stays
//! in the existing per-folder operations, including their recovery and CAS.
use crate::{
    Result,
    commit_group::{Change, Changed, Group, Member, Status},
    integration::{Action, Command, Job},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE commit_groups(id TEXT PRIMARY KEY,session TEXT NOT NULL,active INTEGER NOT NULL,record TEXT NOT NULL);
        CREATE UNIQUE INDEX one_active_commit_group ON commit_groups(session) WHERE active=1;")?;
    Ok(())
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Group> {
    let raw: String = db.query_row(
        "SELECT record FROM commit_groups WHERE id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&raw)?)
}
fn save(db: &Connection, group: &Group) -> Result<()> {
    db.execute("INSERT INTO commit_groups VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET active=excluded.active,record=excluded.record",params![group.id.to_string(),group.session_id,!matches!(group.status,Status::Done|Status::Discarded),serde_json::to_string(group)?])?;
    db.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'commit.group.changed',?3)",
        params![
            crate::journal::timestamp(),
            group.session_id,
            json!({"id":group.id,"revision":group.revision,"status":group.status}).to_string()
        ],
    )?;
    Ok(())
}
fn bump(group: &mut Group) -> Result<()> {
    group.revision = group
        .revision
        .checked_add(1)
        .ok_or("Commit group revision overflow.")?;
    Ok(())
}
fn scope(db: &Connection) -> Result<(String, crate::project::Project)> {
    let raw: String = db.query_row(
        "SELECT value FROM settings WHERE key='session'",
        [],
        |row| row.get(0),
    )?;
    let session: Value = serde_json::from_str(&raw)?;
    if session["bound"] != true {
        return Err("Wait for the session to be prepared.".into());
    }
    let project = crate::folders::project(db)?.ok_or("Choose a project folder.")?;
    Ok((
        session["id"].as_str().ok_or("Missing session ID.")?.into(),
        project,
    ))
}
fn same_scope(db: &Connection, group: &Group) -> Result<()> {
    let (session, project) = scope(db)?;
    let current = project
        .folders
        .iter()
        .map(|folder| folder.id.as_str())
        .collect::<BTreeSet<_>>();
    let saved = group
        .members
        .iter()
        .map(|member| member.folder_id.as_str())
        .collect::<BTreeSet<_>>();
    if group.session_id != session || group.project_id != project.id || current != saved {
        return Err(
            "The project's folders changed. Discard this group review and open it again.".into(),
        );
    }
    Ok(())
}
fn jobs(db: &Connection, group: &Group) -> Result<Vec<Job>> {
    group
        .members
        .iter()
        .filter_map(|member| member.job_id)
        .map(|id| crate::integration_store::read(db, id))
        .collect()
}
fn replay(db: &Connection, op: Uuid, kind: &str, request: &Value) -> Result<Option<Changed>> {
    if op.get_version_num() != 7 {
        return Err("Invalid commit group operation.".into());
    }
    let old: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [op.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((old_kind, raw)) = old else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(&raw)?;
    if old_kind != kind || value["request"] != *request {
        return Err("Commit group operation ID reused.".into());
    }
    let group = read(db, serde_json::from_value(value["groupId"].clone())?)?;
    Ok(Some(Changed {
        jobs: jobs(db, &group)?,
        group,
    }))
}
fn receipt(db: &Connection, op: Uuid, kind: &str, request: Value, group: &Group) -> Result<()> {
    db.execute(
        "INSERT INTO operations VALUES(?1,?2,?3,'complete')",
        params![
            op.to_string(),
            kind,
            json!({"request":request,"groupId":group.id}).to_string()
        ],
    )?;
    Ok(())
}
fn member(
    db: &Connection,
    session: &str,
    folder: &crate::project::Folder,
    operation: Uuid,
    selected: Option<&[String]>,
) -> Result<Member> {
    let active = crate::integration_store::active_for_folder(db, session, &folder.id)?;
    if selected.is_none()
        && active
            .as_ref()
            .is_some_and(|job| !job.tracks.iter().any(|track| track.included))
    {
        return Ok(Member {
            folder_id: folder.id.clone(),
            name: folder.name.clone(),
            job_id: None,
        });
    }
    if let Some(selected) = selected
        && let Some(job) = &active
    {
        let wanted = selected
            .iter()
            .filter(|track| {
                crate::folders::track(db, track).is_ok_and(|owner| owner.id == folder.id)
            })
            .collect::<BTreeSet<_>>();
        let included = job
            .tracks
            .iter()
            .filter(|track| track.included)
            .map(|track| &track.id)
            .collect::<BTreeSet<_>>();
        if !wanted.is_empty() && wanted != included {
            return Err("A saved folder review contains different tracks. Finish or discard it before opening this group.".into());
        }
    }
    if let Some(job) = &active
        && selected.is_none_or(|selected| {
            job.tracks
                .iter()
                .filter(|track| track.included)
                .all(|track| selected.contains(&track.id))
        })
        && job
            .tracks
            .iter()
            .any(|track| track.included && track.checkpoint.is_some())
    {
        return Ok(Member {
            folder_id: folder.id.clone(),
            name: folder.name.clone(),
            job_id: Some(job.id),
        });
    }
    let mut tracks=db.prepare("SELECT t.id FROM tracks t JOIN track_folders f ON f.track=t.id JOIN workspaces w ON w.track_id=t.id
        WHERE f.folder=?1 AND json_extract(w.record,'$.state')='ready' AND EXISTS(SELECT 1 FROM checkpoint_lineage c WHERE c.track=t.id AND c.complete=1) ORDER BY t.rowid")?
        .query_map([&folder.id],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some(selected) = selected {
        tracks.retain(|track| selected.contains(track));
    }
    if tracks.is_empty() {
        return Ok(Member {
            folder_id: folder.id.clone(),
            name: folder.name.clone(),
            job_id: None,
        });
    }
    let action = match active {
        Some(job) => Action::Refresh {
            id: job.id,
            revision: job.revision,
        },
        None => Action::Open {
            tracks: Some(tracks),
        },
    };
    let op_id = Uuid::new_v5(&operation, format!("review/{}", folder.id).as_bytes());
    let job = crate::integration_store::command_in(db, &Command { op_id, action })?;
    Ok(Member {
        folder_id: folder.id.clone(),
        name: folder.name.clone(),
        job_id: Some(job.id),
    })
}
fn reconciled(db: &Connection, mut group: Group) -> Result<Group> {
    if matches!(group.status, Status::Discarded | Status::Done) {
        return Ok(group);
    }
    let children = jobs(db, &group)?;
    let complete = !children.is_empty()
        && children
            .iter()
            .all(|job| job.status == crate::integration::Status::Done);
    let next = if complete {
        Status::Done
    } else if group.last_commit.is_none() {
        Status::Open
    } else if children
        .iter()
        .any(|job| job.status == crate::integration::Status::Committing)
    {
        Status::Committing
    } else {
        Status::Partial
    };
    let error = (next == Status::Partial
        && children.iter().any(|job| {
            matches!(
                job.status,
                crate::integration::Status::Failed
                    | crate::integration::Status::CommitBlocked
                    | crate::integration::Status::Discarded
            )
        }))
    .then(|| "Review the remaining folders before committing.".to_owned());
    if next != group.status || error != group.error {
        group.status = next;
        group.error = error;
        bump(&mut group)?;
        save(db, &group)?;
    }
    Ok(group)
}

pub(crate) fn change(db: &mut Connection, change: Change) -> Result<Changed> {
    let tx = db.transaction()?;
    let group = match change {
        Change::Open(request) => {
            let kind = "commit.group.open";
            let value = serde_json::to_value(&request)?;
            if let Some(old) = replay(&tx, request.op_id, kind, &value)? {
                return Ok(old);
            }
            let (session, project) = scope(&tx)?;
            if let Some(tracks) = &request.tracks {
                if tracks.is_empty() || tracks.iter().collect::<BTreeSet<_>>().len() != tracks.len()
                {
                    return Err("Choose saved tracks exactly once.".into());
                }
                for track in tracks {
                    let folder = crate::folders::track(&tx, track)?;
                    project.folder(&folder.id)?;
                }
            }
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM commit_groups WHERE session=?1 AND active=1",
                    [&session],
                    |row| row.get(0),
                )
                .optional()?;
            let mut group = if let Some(id) = existing {
                let mut group = read(&tx, Uuid::parse_str(&id)?)?;
                same_scope(&tx, &group)?;
                if let Some(tracks) = &request.tracks
                    && group
                        .tracks
                        .as_ref()
                        .map(|saved| saved.iter().collect::<BTreeSet<_>>())
                        != Some(tracks.iter().collect())
                {
                    return Err("The saved all-folder review contains different tracks. Finish or discard it before opening another group.".into());
                }
                if group.status != Status::Committing {
                    let mut changed = false;
                    for old in &mut group.members {
                        if old.job_id.is_none()
                            || old.job_id.is_some_and(|id| {
                                crate::integration_store::read(&tx, id).is_ok_and(|job| {
                                    job.status == crate::integration::Status::Discarded
                                })
                            })
                        {
                            let folder = project.folder(&old.folder_id)?;
                            let next = member(
                                &tx,
                                &session,
                                folder,
                                request.op_id,
                                group.tracks.as_deref(),
                            )?;
                            if next != *old {
                                *old = next;
                                changed = true;
                            }
                        }
                    }
                    if changed {
                        bump(&mut group)?;
                        save(&tx, &group)?;
                    }
                }
                group
            } else {
                let mut members = Vec::new();
                for folder in &project.folders {
                    members.push(member(
                        &tx,
                        &session,
                        folder,
                        request.op_id,
                        request.tracks.as_deref(),
                    )?);
                }
                let group = Group {
                    id: request.op_id,
                    revision: 1,
                    session_id: session,
                    project_id: project.id,
                    status: Status::Open,
                    members,
                    tracks: request.tracks.clone(),
                    last_commit: None,
                    error: None,
                };
                save(&tx, &group)?;
                group
            };
            group = reconciled(&tx, group)?;
            receipt(&tx, request.op_id, kind, value, &group)?;
            group
        }
        Change::Commit(request) => {
            let kind = "commit.group.commit";
            let value = serde_json::to_value(&request)?;
            if let Some(old) = replay(&tx, request.op_id, kind, &value)? {
                return Ok(old);
            }
            let mut group = read(&tx, request.id)?;
            same_scope(&tx, &group)?;
            if group.revision != request.revision
                || !matches!(group.status, Status::Open | Status::Partial)
            {
                return Err("The all-folder review changed. Review it again.".into());
            }
            let expected = group
                .members
                .iter()
                .filter_map(|member| member.job_id)
                .collect::<BTreeSet<_>>();
            let selected = request
                .targets
                .iter()
                .map(|target| target.job_id)
                .collect::<BTreeSet<_>>();
            if expected.is_empty()
                || expected != selected
                || selected.len() != request.targets.len()
            {
                return Err("Confirm every folder's saved candidate exactly once.".into());
            }
            let mut pending = 0;
            for target in &request.targets {
                let job = crate::integration_store::read(&tx, target.job_id)?;
                let owner = group
                    .members
                    .iter()
                    .find(|member| member.job_id == Some(job.id))
                    .ok_or("Unknown group member.")?;
                if job.session_id != group.session_id || job.project.id != owner.folder_id {
                    return Err("The folder review belongs to another group.".into());
                }
                if job.status == crate::integration::Status::Done {
                    let candidate = job
                        .candidate
                        .as_ref()
                        .ok_or("Missing committed candidate.")?;
                    let publication = job
                        .publication
                        .as_ref()
                        .ok_or("Missing committed operation.")?;
                    let complete:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND kind='integration.publish' AND state='complete')",[publication.operation.to_string()],|row|row.get(0))?;
                    if !complete
                        || job.revision != target.revision
                        || candidate.head != target.head
                        || candidate.tree != target.tree
                        || candidate.settings_hash != target.settings_hash
                    {
                        return Err("A committed folder changed. Review the group again.".into());
                    }
                } else {
                    // If any later validation fails, the transaction rolls back
                    // every preceding publication intent before Git can run.
                    crate::integration_store::command_in(
                        &tx,
                        &Command {
                            op_id: Uuid::new_v5(
                                &request.op_id,
                                format!("commit/{}", owner.folder_id).as_bytes(),
                            ),
                            action: Action::Commit {
                                id: job.id,
                                revision: target.revision,
                                head: target.head.clone(),
                                tree: target.tree.clone(),
                                settings_hash: target.settings_hash.clone(),
                            },
                        },
                    )?;
                    pending += 1;
                }
            }
            if pending == 0 {
                return Err("These folders have already been committed.".into());
            }
            group.status = Status::Committing;
            group.last_commit = Some(request.op_id);
            group.error = None;
            bump(&mut group)?;
            save(&tx, &group)?;
            receipt(&tx, request.op_id, kind, value, &group)?;
            group
        }
        Change::Discard(request) => {
            let kind = "commit.group.discard";
            let value = serde_json::to_value(&request)?;
            if let Some(old) = replay(&tx, request.op_id, kind, &value)? {
                return Ok(old);
            }
            let mut group = read(&tx, request.id)?;
            if group.revision != request.revision || group.status == Status::Committing {
                return Err("The all-folder review changed or is committing.".into());
            }
            for job in jobs(&tx, &group)? {
                if matches!(
                    job.status,
                    crate::integration::Status::Done | crate::integration::Status::Discarded
                ) {
                    continue;
                }
                crate::integration_store::command_in(
                    &tx,
                    &Command {
                        op_id: Uuid::new_v5(&request.op_id, job.id.as_bytes()),
                        action: Action::Discard {
                            id: job.id,
                            revision: job.revision,
                        },
                    },
                )?;
            }
            group.status = Status::Discarded;
            group.error = None;
            bump(&mut group)?;
            save(&tx, &group)?;
            receipt(&tx, request.op_id, kind, value, &group)?;
            group
        }
        Change::Reconcile(id) => reconciled(&tx, read(&tx, id)?)?,
    };
    let jobs = jobs(&tx, &group)?;
    tx.commit()?;
    Ok(Changed { group, jobs })
}
pub(crate) fn active(db: &Connection) -> Result<Vec<Group>> {
    db.prepare("SELECT record FROM commit_groups WHERE active=1 ORDER BY rowid")?
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str(&raw?)?))
        .collect()
}
pub(crate) fn snapshot(db: &Connection) -> Result<Value> {
    let raw:Option<String>=db.query_row("SELECT g.record FROM commit_groups g WHERE g.session=(SELECT json_extract(value,'$.id') FROM settings WHERE key='session') ORDER BY g.rowid DESC LIMIT 1",[],|row|row.get(0)).optional()?;
    let Some(raw) = raw else {
        return Ok(json!([]));
    };
    let group: Group = serde_json::from_str(&raw)?;
    Ok(if group.status == Status::Discarded {
        json!([])
    } else {
        serde_json::to_value(vec![group])?
    })
}
