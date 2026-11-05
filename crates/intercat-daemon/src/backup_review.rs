//! Metadata-only review while a restored database is barred from opening history.
use crate::{
    Result,
    fingerprint_receipt::Receipt,
    journal::Journal,
    process::{Identity, Tracker},
};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Request {
    Inspect,
    ChooseSession(crate::backup_choices::Request),
    Apply {
        op_id: Uuid,
        acknowledge_unverified: bool,
    },
    ConfirmApply {
        op_id: Uuid,
    },
    CancelApply {
        op_id: Uuid,
    },
    ReviewFiles {
        op_id: Uuid,
        track_id: String,
    },
    Stop {
        op_id: Uuid,
        host_id: String,
        root: Identity,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub(crate) track_id: String,
    pub(crate) session_id: Option<String>,
    pub(crate) path: PathBuf,
    pub(crate) reference: String,
    pub(crate) base: String,
    pub(crate) checkpoint: Option<crate::checkpoint::Checkpoint>,
    pub(crate) attachment_roots: Vec<String>,
    pub(crate) attachment_ids: Vec<Uuid>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub(crate) fingerprints: Value,
    pub(crate) workspaces: Vec<Workspace>,
    pub(crate) pending_operations: i64,
    pub(crate) known_work: Vec<String>,
    pub(crate) known_repair: Vec<String>,
    pub(crate) known_summary: Vec<String>,
    pub(crate) expected_hosts: Vec<(String, Identity)>,
    pub(crate) originals: Vec<crate::attachment_store::Stored>,
    pub(crate) catalog: crate::secrets::Seal,
    pub(crate) choices: Vec<crate::backup_choices::Record>,
    pub(crate) file_reviews: std::collections::BTreeMap<String, Uuid>,
}
pub(crate) fn plan(db: &Connection, receipt: &Receipt, key: &crate::secrets::Key) -> Result<Plan> {
    receipt.validate()?;
    let catalog = crate::secret_store::catalog(db)?;
    let fingerprints = if let Some(state) = crate::fingerprint_recovery::status(db, Some(receipt))?
    {
        state
    } else {
        if catalog.accept(Some(receipt)).is_ok()
            || catalog.current.revision > receipt.catalog.revision
        {
            return Err("This database is not awaiting restored-history review.".into());
        }
        let mut state = serde_json::to_value(catalog.proof.difference(&receipt.proof))?;
        state["historyReviewRequired"] = json!(true);
        state
    };
    let mut query = db.prepare("SELECT w.track_id,json_extract(s.record,'$.sessionId'),json_extract(w.record,'$.path'),json_extract(w.record,'$.reference'),json_extract(w.record,'$.base'),json_extract(s.record,'$.checkpoint'),json_extract(w.record,'$.attachmentRoots') FROM workspaces w LEFT JOIN workers s ON s.track_id=w.track_id ORDER BY w.track_id")?;
    let workspaces = query
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?
        .map(|r| {
            let (track_id, session_id, path, reference, base, checkpoint, roots) = r?;
            let attachment_ids = session_id
                .as_ref()
                .map(|s| crate::attachment_history::restorable(db, s))
                .transpose()?
                .unwrap_or_default();
            Ok(Workspace {
                track_id,
                session_id,
                path: path.into(),
                reference,
                base,
                checkpoint: checkpoint.map(|v| serde_json::from_str(&v)).transpose()?,
                attachment_roots: serde_json::from_str(&roots)?,
                attachment_ids,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let ids = |sql: &str| -> Result<Vec<String>> {
        Ok(db
            .prepare(sql)?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    };
    let mut expected_hosts = Vec::new();
    for (kind, sql) in [
        (
            "work",
            "SELECT track_id,json_extract(record,'$.identity') FROM workers WHERE json_extract(record,'$.identity') IS NOT NULL",
        ),
        (
            "repair",
            "SELECT id,json_extract(record,'$.host') FROM repair_runs WHERE json_extract(record,'$.host') IS NOT NULL",
        ),
    ] {
        let mut query = db.prepare(sql)?;
        for row in query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, identity) = row?;
            expected_hosts.push((format!("{kind}/{id}"), serde_json::from_str(&identity)?));
        }
    }
    let original_ids = ids("SELECT id FROM attachments ORDER BY id LIMIT 50001")?;
    if original_ids.len() > 50000 {
        return Err("Too many recorded originals to review.".into());
    }
    let originals = original_ids
        .into_iter()
        .map(|id| crate::attachment_store::get(db, id.parse()?))
        .collect::<Result<Vec<_>>>()?;
    let digest = serde_json::to_string(&Sha256::digest(serde_json::to_vec(receipt)?).to_vec())?;
    let mut file_reviews = std::collections::BTreeMap::new();
    for workspace in &workspaces {
        let id:Option<String>=db.query_row("SELECT id FROM operations WHERE kind='backup.files.review' AND state='complete' AND json_extract(payload,'$.request.trackId')=?1 AND json_extract(payload,'$.receiptDigest')=?2 ORDER BY rowid DESC LIMIT 1",rusqlite::params![workspace.track_id,digest],|r|r.get(0)).optional()?;
        if let Some(id) = id {
            file_reviews.insert(workspace.track_id.clone(), id.parse()?);
        }
    }
    Ok(Plan {
        fingerprints,
        workspaces,
        pending_operations: db.query_row(
            "SELECT COUNT(*) FROM operations WHERE state IN ('prepared','dispatched')",
            [],
            |r| r.get(0),
        )?,
        known_work: ids("SELECT track_id FROM workers")?,
        known_repair: ids("SELECT id FROM repair_runs")?,
        known_summary: ids("SELECT id FROM operations WHERE kind='handoff.summary'")?,
        expected_hosts,
        originals,
        catalog: catalog.current,
        choices: crate::backup_choices::load(db, key)?,
        file_reviews,
    })
}
impl Plan {
    pub(crate) fn binding(&self, receipt: &Receipt) -> Result<String> {
        Ok(Sha256::digest(serde_json::to_vec(&(
            receipt,
            &self.catalog,
            &self.fingerprints,
            &self.workspaces,
            &self.originals,
            &self.expected_hosts,
        ))?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
    }
    pub(crate) fn owns_affected_session(&self, session: &str, track: &str) -> bool {
        self.fingerprints["sessions"]
            .as_array()
            .is_some_and(|sessions| {
                sessions
                    .iter()
                    .any(|s| s["sessionId"] == session && s["trackId"] == track)
            })
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Host {
    id: String,
    workspace: Option<PathBuf>,
    root: Option<Identity>,
    processes: usize,
    groups: usize,
    known_to_log: bool,
    issue: Option<String>,
}
fn hosts(data: &Path, plan: &Plan) -> Result<Vec<Host>> {
    crate::process_ledger::host_paths(data)?
        .into_iter()
        .map(|(id, owner, path)| {
            let known_to_log = if id.starts_with("work/") {
                plan.known_work.contains(&owner)
            } else if id.starts_with("repair/") {
                plan.known_repair.contains(&owner)
            } else {
                plan.known_summary.contains(&owner)
            };
            let result = (|| -> Result<_> {
                let record = crate::process_ledger::read(&path.join(crate::process_ledger::NAME))?;
                let mut tracker = Tracker::restore_host(&path, None)?;
                let scan = tracker.refresh()?;
                Ok((
                    record.root,
                    record.workspace,
                    scan.processes.len(),
                    scan.groups.len(),
                ))
            })();
            Ok(match result {
                Ok((root, workspace, processes, groups)) => Host {
                    id,
                    workspace: Some(workspace),
                    root,
                    processes,
                    groups,
                    known_to_log,
                    issue: None,
                },
                Err(_) => Host {
                    id,
                    workspace: None,
                    root: None,
                    processes: 0,
                    groups: 0,
                    known_to_log,
                    issue: Some(
                        "The saved process ownership could not be verified. Keep history paused."
                            .into(),
                    ),
                },
            })
        })
        .collect()
}
fn inspect(data: &Path, plan: Plan, receipt: &Receipt) -> Result<Value> {
    let binding = plan.binding(receipt)?;
    let hosts = hosts(data, &plan)?;
    let missing_process_records = plan
        .expected_hosts
        .iter()
        .filter(|(id, root)| {
            !hosts.iter().any(|host| {
                (host.id == *id || host.id.starts_with(&format!("{id}/")))
                    && host.root == Some(*root)
            })
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let workspaces=plan.workspaces.iter().map(|workspace|{
        let checked=(||->Result<Value>{
            let repo=intercat_git::Repository::open(&workspace.path)?;
            let reference=workspace.checkpoint.as_ref().map_or(workspace.reference.as_str(),|c|&c.reference);
            let expected=workspace.checkpoint.as_ref().map_or(workspace.base.as_str(),|c|&c.sha);
            let actual=repo.reference_target(reference)?.map(|id|id.to_hex().to_string());
            let object_available=repo.tree_id(expected.parse()?).is_ok();
            Ok(json!({"reference":reference,"expected":expected,"actual":actual,"objectAvailable":object_available,"matches":object_available && actual.as_deref()==Some(expected)}))
        })();
        json!({"trackId":workspace.track_id,"sessionId":workspace.session_id,"path":workspace.path,"savedReference":checked.unwrap_or_else(|_|json!({"matches":false,"issue":"The saved repository reference is unavailable."})),"filesReviewed":false,"attachmentsReviewed":false,"lastReviewId":plan.file_reviews.get(&workspace.track_id)})
    }).collect::<Vec<_>>();
    let sessions=plan.fingerprints["sessions"].as_array().into_iter().flatten().map(|session| {
        let id=session["sessionId"].as_str().unwrap(); let track=session["trackId"].as_str().unwrap();
        json!({"sessionId":id,"trackId":track,"reason":session["reason"],"canPickUp":plan.workspaces.iter().any(|w|w.track_id==track && w.session_id.as_deref()==Some(id)),"choice":plan.choices.iter().find(|c|c.request.session_id==id).map(|c|c.display(&binding))})
    }).collect::<Vec<_>>();
    Ok(
        json!({"fingerprints":plan.fingerprints,"workspaces":workspaces,"sessions":sessions,"hosts":hosts,"missingProcessRecords":missing_process_records,"pendingOperations":plan.pending_operations,"historyPaused":true}),
    )
}
pub(crate) async fn review_files(
    journal: &Journal,
    data: &Path,
    receipt: &Receipt,
    plan: Plan,
    id: Uuid,
    track: &str,
) -> Result<crate::backup_files::Review> {
    let request = Request::ReviewFiles {
        op_id: id,
        track_id: track.into(),
    };
    let op_id = &id;
    let track_id = &track.to_owned();
    if op_id.get_version_num() != 7 {
        return Err("Invalid file review operation ID.".into());
    }
    let workspace = plan
        .workspaces
        .iter()
        .find(|workspace| &workspace.track_id == track_id)
        .cloned()
        .ok_or("The restored workspace is unavailable.")?;
    let check_data = data.to_path_buf();
    let check_plan = plan.clone();
    tokio::task::spawn_blocking(move || ensure_stopped(&check_data, &check_plan)).await??;
    let path = workspace.path.clone();
    let baseline = workspace
        .checkpoint
        .as_ref()
        .map_or(&workspace.base, |checkpoint| &checkpoint.sha)
        .clone();
    let id = *op_id;
    let track = track_id.clone();
    let source = crate::backup_attachments::Source {
        store: journal.attachment_store(),
        records: plan.originals.clone(),
        roots: workspace.attachment_roots.clone(),
        required: workspace.attachment_ids.clone(),
    };
    let mut inspection = tokio::task::spawn_blocking(move || {
        crate::backup_files::inspect(&path, &baseline, id, track, &source)
    })
    .await??;
    let check_data = data.to_path_buf();
    let check_plan = plan.clone();
    tokio::task::spawn_blocking(move || ensure_stopped(&check_data, &check_plan)).await??;
    let receipt_digest = Sha256::digest(serde_json::to_vec(&receipt)?).to_vec();
    let material = serde_json::to_vec(&(
        &request,
        &workspace,
        &receipt_digest,
        Sha256::digest(&inspection.material).to_vec(),
        &inspection.review,
    ))?;
    inspection.material.fill(0);
    inspection.review.seal = journal.backup_file_tag(&material);
    let old = journal.operation(*op_id).await?;
    let report = if let Some(old) = &old {
        if old["kind"] != "backup.files.review"
            || old["state"] != "complete"
            || old["payload"]["request"] != serde_json::to_value(&request)?
            || old["payload"]["receiptDigest"] != serde_json::to_value(&receipt_digest)?
        {
            return Err("File review operation ID reused.".into());
        }
        let mut report: crate::backup_files::Review =
            serde_json::from_value(old["payload"]["review"].clone())?;
        report.stale = report != inspection.review;
        report
    } else {
        journal.save_backup_files(*op_id,json!({"request":request,"receiptDigest":receipt_digest,"review":inspection.review})).await?;
        inspection.review
    };
    Ok(report)
}
pub async fn run(
    journal: &Journal,
    data: &Path,
    receipt: Receipt,
    request: Request,
) -> Result<Value> {
    match &request {
        Request::Apply {
            op_id,
            acknowledge_unverified,
        } => {
            return crate::backup_apply::apply(
                journal,
                data,
                receipt,
                *op_id,
                *acknowledge_unverified,
            )
            .await;
        }
        Request::ConfirmApply { op_id } => {
            return crate::backup_apply::confirm(journal, data, receipt, *op_id).await;
        }
        Request::CancelApply { op_id } => {
            return crate::backup_apply::cancel(journal, &receipt, *op_id).await;
        }
        _ => {}
    }
    let pending = journal.backup_apply(None).await?;
    if let Some(state) = &pending {
        if !state.accepts(&receipt) {
            return Err("The backup recovery confirmation changed.".into());
        }
        if !matches!(request, Request::Inspect | Request::Stop { .. }) {
            return Err("Finish the prepared backup recovery first.".into());
        }
    }
    let mut plan = if let Some(state) = &pending {
        state.plan.clone()
    } else {
        journal.backup_review(receipt.clone()).await?
    };
    let mut file_review = None;
    if let Request::ReviewFiles { op_id, track_id } = &request {
        file_review =
            Some(review_files(journal, data, &receipt, plan.clone(), *op_id, track_id).await?);
        plan = journal.backup_review(receipt.clone()).await?;
    }
    if let Request::ChooseSession(choice) = &request {
        if choice.op_id.get_version_num() != 7 {
            return Err("Invalid recovery decision operation ID.".into());
        }
        let session = plan.fingerprints["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["sessionId"] == choice.session_id)
            .ok_or("This session is not awaiting recovery.")?;
        let track = session["trackId"].as_str().unwrap().to_owned();
        let workspace = plan
            .workspaces
            .iter()
            .find(|w| w.track_id == track && w.session_id.as_deref() == Some(&choice.session_id));
        let check_data = data.to_path_buf();
        let check_plan = plan.clone();
        tokio::task::spawn_blocking(move || ensure_stopped(&check_data, &check_plan)).await??;
        let review_seal = if workspace.is_some() {
            let review_id = choice
                .review_id
                .ok_or("Compare the current files before choosing recovery.")?;
            if journal.operation(review_id).await?.is_none() {
                return Err("The selected file review is unavailable.".into());
            }
            let review =
                review_files(journal, data, &receipt, plan.clone(), review_id, &track).await?;
            if review.stale {
                return Err(
                    "Files or originals changed. Compare them again before choosing recovery."
                        .into(),
                );
            }
            if choice.choice == crate::backup_choices::Choice::PickUp
                && (!review.complete || !review.attachments.complete)
            {
                return Err("Pick up requires verified files and attachment originals.".into());
            }
            if (!review.complete || !review.attachments.complete) && !choice.acknowledge_unread {
                return Err(
                    "Acknowledge the unverified files and originals before discarding history."
                        .into(),
                );
            }
            let seal = review.seal.clone();
            file_review = Some(review);
            Some(seal)
        } else {
            if choice.choice != crate::backup_choices::Choice::DiscardHistory
                || choice.review_id.is_some()
                || !choice.acknowledge_unread
            {
                return Err("This session has no matching workspace. Only discarding its history after acknowledgment is available.".into());
            }
            None
        };
        let record = crate::backup_choices::Record {
            request: choice.clone(),
            track_id: track,
            binding: plan.binding(&receipt)?,
            review_seal,
            seal: String::new(),
        };
        journal.save_backup_choice(receipt.clone(), record).await?;
        plan = journal.backup_review(receipt.clone()).await?;
    }
    if let Request::Stop {
        op_id,
        host_id,
        root,
    } = &request
    {
        if op_id.get_version_num() != 7 {
            return Err("Invalid stop operation ID.".into());
        }
        let receipt_digest = Sha256::digest(serde_json::to_vec(&receipt)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let payload = json!({"request":request,"receiptDigest":receipt_digest});
        let old = journal.operation(*op_id).await?;
        if let Some(old) = &old
            && (old["kind"] != "backup.host.stop" || old["payload"] != payload)
        {
            return Err("Stop operation ID reused.".into());
        }
        if old.as_ref().is_none_or(|old| old["state"] != "complete") {
            let paths = crate::process_ledger::host_paths(data)?;
            let path = paths
                .into_iter()
                .find(|(id, _, _)| id == host_id)
                .ok_or("This host is no longer in the saved records.")?
                .2;
            let record = crate::process_ledger::read(&path.join(crate::process_ledger::NAME))?;
            if record.root != Some(*root) {
                return Err(
                    "The recorded process owner changed. Review again before stopping.".into(),
                );
            }
            journal
                .intent(&op_id.to_string(), "backup.host.stop", payload)
                .await?;
            journal.finish(&op_id.to_string(), "dispatched").await?;
            let expected = *root;
            tokio::task::spawn_blocking(move || -> Result<()> {
                Tracker::restore_host(&path, Some(expected))?.stop_confirmed()?;
                Ok(())
            })
            .await??;
            journal.finish(&op_id.to_string(), "complete").await?;
        }
        if pending.is_none() {
            plan = journal.backup_review(receipt.clone()).await?;
        }
    }
    let data = data.to_path_buf();
    let inspection_receipt = pending
        .as_ref()
        .map_or(&receipt, |state| &state.source)
        .clone();
    let mut result =
        tokio::task::spawn_blocking(move || inspect(&data, plan, &inspection_receipt)).await??;
    if let Some(state) = pending {
        result["apply"] = state.status();
        result["publication"] = state.view()["publication"].clone();
    }
    if let Some(review) = file_review {
        for workspace in result["workspaces"].as_array_mut().into_iter().flatten() {
            if workspace["trackId"] == review.track_id {
                workspace["filesReviewed"] = json!(review.complete && !review.stale);
                workspace["attachmentsReviewed"] =
                    json!(review.attachments.complete && !review.stale);
            }
        }
        result["fileReview"] = serde_json::to_value(review)?;
    }
    Ok(result)
}

/// A file review has no external mutation: publish its complete receipt in one
/// transaction instead of leaving a dispatch reservation to recover.
pub(crate) fn save_files(db: &mut Connection, id: Uuid, payload: Value) -> Result<()> {
    use rusqlite::{OptionalExtension, params};
    let tx = db.transaction()?;
    let old: Option<(String, String, String)> = tx
        .query_row(
            "SELECT kind,payload,state FROM operations WHERE id=?1",
            [id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((kind, old, state)) = old {
        if kind != "backup.files.review"
            || state != "complete"
            || serde_json::from_str::<Value>(&old)? != payload
        {
            return Err("File review operation ID reused.".into());
        }
        return Ok(());
    }
    let encoded = serde_json::to_string(&payload)?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'backup.files.review',?2,'complete')",
        params![id.to_string(), encoded],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'backup.files.review',?2)",
        params![crate::journal::timestamp(), encoded],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn ensure_stopped(data: &Path, plan: &Plan) -> Result<()> {
    let records = hosts(data, plan)?;
    if records
        .iter()
        .any(|host| host.issue.is_some() || host.processes > 0 || host.groups > 0)
        || plan.expected_hosts.iter().any(|(id, root)| {
            !records.iter().any(|host| {
                (host.id == *id || host.id.starts_with(&format!("{id}/")))
                    && host.root == Some(*root)
            })
        })
    {
        return Err(
            "Confirm all recorded processes stopped before reviewing current files.".into(),
        );
    }
    Ok(())
}
