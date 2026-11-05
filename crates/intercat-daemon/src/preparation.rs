//! Prepare session-owned worktrees from durable input and project state.
use crate::{
    Result,
    journal::{Admission, Journal},
    project::Project,
    worktree,
};
use intercat_git::Repository;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::{sync::watch, task::JoinHandle};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub attachment_roots: Vec<String>,
    pub track_id: String,
    pub path: PathBuf,
    pub reference: String,
    pub base: String,
    pub state: String,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub phase: String,
    pub detail: Option<String>,
}
pub struct Preparation {
    stop: watch::Sender<bool>,
    pub status: watch::Receiver<Status>,
    task: Option<JoinHandle<()>>,
}
impl Preparation {
    pub fn start(root: &Path, journal: Journal) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let meta = std::fs::symlink_metadata(root)?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::other("Unsafe workspace directory.").into());
        }
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        let root = root.canonicalize()?;
        let (stop, mut stopping) = watch::channel(false);
        let (state, status) = watch::channel(Status {
            phase: "waiting".into(),
            detail: None,
        });
        let task = tokio::spawn(async move {
            let mut changes = journal.subscribe();
            loop {
                changes.borrow_and_update();
                if *stopping.borrow() {
                    return;
                }
                let result = tokio::select! {
                    _ = stopping.changed() => return,
                    result = prepare_one(&root, &journal, &state) => result,
                };
                match result {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => {
                        state.send_replace(Status {
                            phase: "failed".into(),
                            detail: Some(error.to_string()),
                        });
                        changes.borrow_and_update();
                    }
                }
                tokio::select! {
                    _ = stopping.changed() => return,
                    result = changes.changed() => { if result.is_err() { return; } },
                }
            }
        });
        Ok(Self {
            stop,
            status,
            task: Some(task),
        })
    }
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Preparation {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

async fn finish(journal: &Journal, id: &str, admission: Admission) -> Result<()> {
    match admission {
        Admission::New => {
            journal.finish(id, "dispatched").await?;
            journal.finish(id, "complete").await?;
        }
        Admission::Replay(state) if state == "prepared" => {
            journal.finish(id, "dispatched").await?;
            journal.finish(id, "complete").await?;
        }
        Admission::Replay(state) if state == "dispatched" => journal.finish(id, "complete").await?,
        Admission::Replay(state) if state == "complete" => {}
        _ => return Err(std::io::Error::other("Session reservation was superseded.").into()),
    }
    Ok(())
}

async fn reserve_session(project: &Project, session: &Value, journal: &Journal) -> Result<String> {
    project.source().await?;
    let session_id = Uuid::parse_str(
        session["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("Missing session ID."))?,
    )?;
    let preferred = session["slug"]
        .as_str()
        .ok_or_else(|| std::io::Error::other("Missing session slug."))?;
    let root = project.root.clone();
    let marker = tokio::task::spawn_blocking(move || -> Result<_> {
        let repo = Repository::open(root)?;
        let marker = repo.write_blob(format!("Intercat session {session_id}\n").as_bytes())?;
        repo.sync_objects(&[marker])?;
        Ok(marker)
    })
    .await??;
    for suffix in 1..=10000 {
        let slug = if suffix == 1 {
            preferred.to_string()
        } else {
            format!("{preferred}-{suffix}")
        };
        let reference = format!("refs/intercat/{slug}/session");
        let operation_id = Uuid::new_v5(&session_id, format!("reserve:{slug}").as_bytes());
        let root = project.root.clone();
        let candidate = slug.clone();
        let name = reference.clone();
        let available = tokio::task::spawn_blocking(move || -> Result<bool> {
            let repo = Repository::open(root)?;
            Ok(repo.reference_target(&name)? == Some(marker)
                || !repo.session_slug_occupied(&candidate)?)
        })
        .await??;
        if !available {
            if let Some(previous) = journal.operation(operation_id).await? {
                if previous["state"] == "prepared" {
                    journal
                        .finish(&operation_id.to_string(), "superseded")
                        .await?;
                } else if previous["state"] != "superseded" {
                    return Err(std::io::Error::other(
                        "The session reservation changed during preparation.",
                    )
                    .into());
                }
            }
            continue;
        }
        let operation = operation_id.to_string();
        let admitted = journal
            .intent(
                &operation,
                "session.reserve",
                json!({"repository":project.root,"ref":reference,"next":marker.to_string()}),
            )
            .await?;
        if admitted == Admission::Replay("superseded".into()) {
            continue;
        }
        let root = project.root.clone();
        let name = reference.clone();
        let reserved = tokio::task::spawn_blocking(move || -> Result<()> {
            let repo = Repository::open(root)?;
            repo.compare_and_swap(&name, None, marker)?;
            repo.sync_ref(&name)?;
            Ok(())
        })
        .await?;
        if let Err(error) = reserved {
            let root = project.root.clone();
            let name = reference.clone();
            let occupied = tokio::task::spawn_blocking(move || {
                Repository::open(root)?.reference_target(&name)
            })
            .await??;
            if occupied.is_some_and(|value| value != marker) {
                if admitted == Admission::New || admitted == Admission::Replay("prepared".into()) {
                    journal.finish(&operation, "superseded").await?;
                }
                continue;
            }
            return Err(error);
        }
        finish(journal, &operation, admitted).await?;
        journal
            .bind_session(session_id.to_string(), slug.clone())
            .await?;
        return Ok(slug);
    }
    Err(std::io::Error::other("No unused session name is available.").into())
}

async fn prepare_one(
    root: &Path,
    journal: &Journal,
    status: &watch::Sender<Status>,
) -> Result<bool> {
    if let Some((id, project)) = journal.pending_project().await? {
        journal.configure_project(id, project).await?;
        return Ok(true);
    }
    let snapshot = journal.snapshot().await?;
    if snapshot["project"].is_null() || snapshot["session"].is_null() {
        return Ok(false);
    }
    let project: Project = serde_json::from_value(snapshot["project"].clone())?;
    let session = &snapshot["session"];
    let session_id = session["id"]
        .as_str()
        .ok_or_else(|| std::io::Error::other("Missing session ID."))?;
    Uuid::parse_str(session_id)?;
    if session["bound"] != true {
        status.send_replace(Status {
            phase: "preparing".into(),
            detail: None,
        });
        reserve_session(&project, session, journal).await?;
        return Ok(true);
    }
    for track in snapshot["tracks"].as_array().into_iter().flatten() {
        let id = track["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("Missing track ID."))?;
        let track_id = Uuid::parse_str(id)?;
        let previous = snapshot["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w["trackId"] == id);
        if previous.is_some_and(|w| w["state"] == "ready" || w["state"] == "failed") {
            continue;
        }
        let queued = snapshot["utterances"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|w| {
                w["state"] == "placed"
                    && w["decision"]["trackId"] == id
                    && ["fork", "extend", "schedule"]
                        .contains(&w["decision"]["kind"].as_str().unwrap_or(""))
            });
        if !queued {
            continue;
        }
        let slug = session["slug"].as_str().unwrap();
        let repository = project.root.clone();
        let bound_slug = slug.to_string();
        let identity = session_id.to_string();
        if !tokio::task::spawn_blocking(move || {
            Repository::open(repository)?.owns_session(&bound_slug, &identity)
        })
        .await??
        {
            return Err(std::io::Error::other(
                "This session's Git reservation changed. Restore it before preparing more work.",
            )
            .into());
        }
        let label = track["label"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("Missing track label."))?;
        if !label.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(std::io::Error::other("Invalid track label.").into());
        }
        let mut workspace = Workspace {
            attachment_roots: Vec::new(),
            track_id: id.into(),
            path: root.join(session_id).join(id),
            reference: format!("refs/intercat/{slug}/tracks/{label}"),
            base: project.base.clone(),
            state: "preparing".into(),
            error: None,
        };
        journal.workspace_result(workspace.clone()).await?;
        status.send_replace(Status {
            phase: "preparing".into(),
            detail: None,
        });
        let result = async {
            let source = project.source().await?;
            let operation = Uuid::new_v5(&track_id, b"worktree.prepare");
            worktree::prepare(
                &source,
                &workspace.path,
                &workspace.reference,
                operation,
                journal,
            )
            .await?;
            workspace.attachment_roots =
                crate::attachments::reserve(&workspace.path, &workspace.base)?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }
        .await;
        workspace.state = if result.is_ok() { "ready" } else { "failed" }.into();
        workspace.error = result.err().map(|error| error.to_string());
        journal.workspace_result(workspace).await?;
        return Ok(true);
    }
    status.send_replace(Status {
        phase: "ready".into(),
        detail: None,
    });
    Ok(false)
}
