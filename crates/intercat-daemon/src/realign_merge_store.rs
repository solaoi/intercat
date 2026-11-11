//! Conflict input records and their queue binding share the journal writer.
use crate::{Result, realign::Phase, realign_merge::Frozen};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Applied {
    pub id: Uuid,
    pub run: Uuid,
    pub revision: u64,
    pub input: Uuid,
    pub choice: crate::merge_choices::Solved,
}
pub(crate) fn begin_result(db: &Connection, result: &Applied) -> Result<()> {
    let old: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [result.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, raw)) = old {
        if kind != "realign.merge.result"
            || serde_json::from_str::<serde_json::Value>(&raw)? != serde_json::to_value(result)?
        {
            return Err("Realignment resolution identity reused.".into());
        }
        return Ok(());
    }
    let mut run = crate::repair_run_store::read(db, result.run)?;
    let input = for_run(db, &run)?;
    let files = input.files(run.id)?;
    if result.id != Uuid::new_v5(&run.id, b"apply-realignment-merge")
        || result.input != input.id
        || result.revision != run.revision
        || run.phase != crate::repair_run::Phase::Captured
        || run.host.is_some()
        || run.refresh.is_some()
        || run.restoring.is_some()
        || (
            result.choice.base.as_str(),
            result.choice.target.as_str(),
            result.choice.incoming.as_str(),
        ) != (
            files.base.as_str(),
            files.target.as_str(),
            files.incoming.as_str(),
        )
    {
        return Err("The captured realignment merge changed.".into());
    }
    result.choice.tree.parse::<intercat_git::ObjectId>()?;
    run.phase = crate::repair_run::Phase::Applying;
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Run revision overflow.")?;
    db.execute(
        "INSERT INTO operations VALUES(?1,'realign.merge.result',?2,'prepared')",
        params![result.id.to_string(), serde_json::to_string(result)?],
    )?;
    crate::repair_run_store::save(db, &run)?;
    crate::repair_run_store::record(db, "realign.merge.applying", &run)
}
pub(crate) fn complete_result(db: &Connection, id: Uuid) -> Result<()> {
    let (raw, state): (String, String) = db.query_row(
        "SELECT payload,state FROM operations WHERE id=?1 AND kind='realign.merge.result'",
        [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if state == "complete" {
        return Ok(());
    }
    let result: Applied = serde_json::from_str(&raw)?;
    let mut run = crate::repair_run_store::read(db, result.run)?;
    let input = for_run(db, &run)?;
    if input.id != result.input
        || !matches!(
            run.phase,
            crate::repair_run::Phase::Applying | crate::repair_run::Phase::NeedsRecovery
        )
        || run.host.is_some()
    {
        return Err("The realignment result was superseded.".into());
    }
    let mut root =
        crate::realign_queue::read(db, input.request.op_id)?.ok_or("Missing realignment.")?;
    root.resolutions.push(result.choice);
    root.merge_input = None;
    root.phase = Phase::Queued;
    root.error = None;
    root.revision = root
        .revision
        .checked_add(1)
        .ok_or("Realignment revision overflow.")?;
    run.phase = crate::repair_run::Phase::Done;
    run.failure = None;
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Run revision overflow.")?;
    crate::realign_queue::save(db, &root)?;
    crate::repair_run_store::save(db, &run)?;
    crate::repair_run_store::record(db, "realign.merge.applied", &run)?;
    db.execute(
        "UPDATE operations SET state='complete' WHERE id=?1",
        [id.to_string()],
    )?;
    Ok(())
}

pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Frozen> {
    let raw: String = db.query_row(
        "SELECT payload FROM operations WHERE id=?1 AND kind='realign.merge.inputs'",
        [id.to_string()],
        |row| row.get(0),
    )?;
    let input: Frozen = serde_json::from_str(&raw)?;
    if input.id != id || Frozen::identity(&input.request, &input.plan)? != id {
        return Err("The saved realignment inputs changed.".into());
    }
    Ok(input)
}
pub(crate) fn freeze(db: &Connection, input: &Frozen) -> Result<()> {
    if Frozen::identity(&input.request, &input.plan)? != input.id {
        return Err("Invalid realignment conflict identity.".into());
    }
    input.files(input.id)?;
    let old: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [input.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, raw)) = old {
        if kind != "realign.merge.inputs"
            || serde_json::from_str::<serde_json::Value>(&raw)? != serde_json::to_value(input)?
        {
            return Err("Realignment conflict identity reused.".into());
        }
        return Ok(());
    }
    let mut root = crate::realign_queue::read(db, input.request.op_id)?
        .ok_or("Queue this realignment before resolving conflicts.")?;
    let current = crate::realign_store::context(db, &input.request)?;
    if root.phase != Phase::Preparing
        || root.request != input.request
        || root.session_id != input.session
        || current.session != input.session
        || current.target != input.plan.target
        || current.basis != input.plan.before_basis
        || current.origin.coverage() != Some(&input.plan.basis.base_inherits)
        || current.resolutions != input.plan.resolutions
        || serde_json::to_value(current.workspace)? != serde_json::to_value(&input.workspace)?
    {
        return Err("The worker or realignment target changed before saving its conflict.".into());
    }
    db.execute(
        "INSERT INTO operations VALUES(?1,'realign.merge.inputs',?2,'prepared')",
        params![input.id.to_string(), serde_json::to_string(input)?],
    )?;
    root.merge_input = Some(input.id);
    root.merge_boundary.get_or_insert(input.id);
    crate::realign_queue::save(db, &root)
}
pub(crate) fn ready(db: &Connection, id: Uuid) -> Result<()> {
    let input = read(db, id)?;
    let root =
        crate::realign_queue::read(db, input.request.op_id)?.ok_or("Missing realignment.")?;
    if root.merge_input != Some(id) || !matches!(root.phase, Phase::Preparing | Phase::NeedsMerge) {
        return Err("The realignment conflict was superseded.".into());
    }
    db.execute(
        "UPDATE operations SET state='complete' WHERE id=?1 AND kind='realign.merge.inputs'",
        [id.to_string()],
    )?;
    Ok(())
}
pub(crate) fn for_run(db: &Connection, run: &crate::repair_run::Run) -> Result<Frozen> {
    let crate::repair_run::Owner::Realign { id, revision } = run.owner else {
        return Err("This Run does not belong to a realignment.".into());
    };
    let root = crate::realign_queue::read(db, id)?.ok_or("Missing realignment.")?;
    if root.revision != revision
        || root.phase != Phase::NeedsMerge
        || root.request.track_id != run.track
        || !matches!(run.purpose, crate::repair_run::Purpose::Merge)
    {
        return Err("The realignment changed while its merge Run was active.".into());
    }
    let input = read(
        db,
        root.merge_input.ok_or("Missing frozen conflict inputs.")?,
    )?;
    let complete: bool = db.query_row(
        "SELECT state='complete' FROM operations WHERE id=?1",
        [input.id.to_string()],
        |row| row.get(0),
    )?;
    if !complete || input.request != root.request || input.session != root.session_id {
        return Err("The realignment conflict inputs are not ready.".into());
    }
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        candidate::Settings,
        check_run::CheckCommand,
        lineage::Basis,
        realign::{ConflictStage, Job, Plan, Request, Resolution},
        repair_run::{Owner, Purpose, Run},
    };
    use std::collections::BTreeMap;

    #[test]
    fn reopening_applies_a_saved_merge_result_once_without_releasing_the_file_boundary()
    -> Result<()> {
        // This is the journal boundary only. Native acceptance separately
        // imports, syncs and pins real Git objects before completing the result.
        let root = tempfile::tempdir()?;
        let path = root.path().join("journal.sqlite");
        let mut db = Connection::open(&path)?;
        db.execute_batch("CREATE TABLE tracks(id TEXT PRIMARY KEY); INSERT INTO tracks VALUES('dummy-worker'); CREATE TABLE operations(id TEXT PRIMARY KEY,kind TEXT NOT NULL,payload TEXT NOT NULL,state TEXT NOT NULL); CREATE TABLE events(id INTEGER PRIMARY KEY,ts INTEGER,kind TEXT,payload TEXT);")?;
        crate::realign_queue::install(&db)?;
        crate::repair_run_store::install(&db)?;
        let request = Request {
            op_id: Uuid::now_v7(),
            track_id: "dummy-worker".into(),
            source: crate::realign::Source::Commit {
                job_id: Uuid::now_v7(),
                revision: 1,
            },
        };
        let plan = Plan {
            reparent: None,
            track: request.track_id.clone(),
            before: "1".repeat(40),
            target: "2".repeat(40),
            before_basis: Basis::root("3".repeat(40)),
            basis: Basis::root("2".repeat(40)),
            resolution: Resolution::Conflict {
                stage: ConflictStage::Overlay,
                base: "4".repeat(40),
                target: "5".repeat(40),
                incoming: "6".repeat(40),
                paths: vec!["dummy.txt".into()],
            },
            resolutions: vec![],
        };
        let input = Frozen {
            id: Frozen::identity(&request, &plan)?,
            request: request.clone(),
            workspace: crate::preparation::Workspace {
                track_id: request.track_id.clone(),
                path: root.path().join("worker"),
                reference: "refs/intercat/dummy/tracks/A".into(),
                base: plan.before.clone(),
                state: "ready".into(),
                error: None,
                denied_roots: vec![],
                attachment_roots: vec![],
            },
            session: "dummy-sdk-session".into(),
            project: crate::project::Folder {
                id: "dummy-folder".into(),
                name: "Dummy".into(),
                path: root.path().into(),
                root: root.path().into(),
                common_dir: root.path().join(".git"),
                base: plan.before.clone(),
                base_branch: Some("main".into()),
            },
            session_name: "Dummy metadata only".into(),
            label: "A".into(),
            head: "7".repeat(40),
            plan,
            settings: Settings {
                environment: None,
                scanner: crate::secret_scan::Engine::capture()?,
                command: CheckCommand::Disabled,
                checks_revision: 0,
                history: false,
                npm: None,
                registry_origins: vec![],
                timeout_ms: 1000,
                path: "/usr/bin:/bin".into(),
                toolchain: vec![],
                checkout: crate::checkout::Configuration {
                    values: BTreeMap::new(),
                    attributes: None,
                    info_attributes: None,
                    hooks_directory: root.path().join("hooks"),
                    lfs_directory: root.path().join("lfs"),
                    object_directories: vec![],
                    denied_roots: vec![],
                },
                skip_hooks: false,
            },
        };
        db.execute(
            "INSERT INTO operations VALUES(?1,'realign.merge.inputs',?2,'complete')",
            params![input.id.to_string(), serde_json::to_string(&input)?],
        )?;
        let job = Job {
            request: request.clone(),
            session_id: input.session.clone(),
            revision: 3,
            phase: Phase::NeedsMerge,
            keep_processes: false,
            wait_for_access: false,
            stop_command: None,
            stop_checkpoint: None,
            inspection: None,
            error: None,
            merge_input: Some(input.id),
            merge_boundary: Some(input.id),
            shared_input: None,
            resolutions: vec![],
        };
        crate::realign_queue::save(&db, &job)?;
        let owner = Owner::Realign {
            id: request.op_id,
            revision: 3,
        };
        let mut run = Run::new(
            crate::repair_run::Request {
                op_id: Uuid::now_v7(),
                owner,
                track: request.track_id.clone(),
                purpose: Purpose::Merge,
            },
            owner,
            false,
        );
        run.phase = crate::repair_run::Phase::Captured;
        crate::repair_run_store::save(&db, &run)?;
        let result = Applied {
            id: Uuid::new_v5(&run.id, b"apply-realignment-merge"),
            run: run.id,
            revision: run.revision,
            input: input.id,
            choice: crate::merge_choices::Solved {
                base: "4".repeat(40),
                target: "5".repeat(40),
                incoming: "6".repeat(40),
                tree: "8".repeat(40),
            },
        };
        crate::realign_store::change(
            &mut db,
            crate::realign_store::Change::BeginMergeResult(Box::new(result.clone())),
        )?;
        drop(db);
        let mut db = Connection::open(&path)?;
        crate::repair_run_store::interrupt(&mut db)?;
        crate::realign_queue::interrupt(&mut db)?;
        assert_eq!(
            crate::repair_run_store::read(&db, run.id)?.phase,
            crate::repair_run::Phase::NeedsRecovery
        );
        assert!(crate::realign_queue::holds_work(&db, &request.track_id)?);
        let mut stale = crate::repair_run_store::read(&db, run.id)?;
        stale.owner = Owner::Realign {
            id: request.op_id,
            revision: 4,
        };
        assert!(for_run(&db, &stale).is_err());
        for _ in 0..2 {
            crate::realign_store::change(
                &mut db,
                crate::realign_store::Change::CompleteMergeResult(result.id),
            )?;
        }
        let saved = crate::realign_queue::read(&db, request.op_id)?.unwrap();
        assert_eq!(saved.phase, Phase::Queued);
        assert_eq!(saved.revision, 4);
        assert_eq!(saved.resolutions, vec![result.choice]);
        assert_eq!(saved.merge_input, None);
        assert_eq!(saved.merge_boundary, Some(input.id));
        assert!(crate::realign_queue::holds_work(&db, &request.track_id)?);
        assert_eq!(
            crate::repair_run_store::read(&db, run.id)?.phase,
            crate::repair_run::Phase::Done
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM events WHERE kind='realign.merge.applied'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            1
        );
        Ok(())
    }
}
