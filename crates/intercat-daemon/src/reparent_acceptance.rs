use crate::{
    Result,
    checkpoint::{self, Checkpoint},
    decision::{Decision, Kind, Outcome, Target},
    integration::{Action, Command as ReviewCommand, Job, Service, Status},
    journal::Journal,
    preparation::{Preparation, Workspace},
    realign::{Request, Source},
    runtime::WorkerState,
};
use rusqlite::params;
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, atomic::AtomicU64},
    time::Duration,
};
use uuid::Uuid;
#[path = "comparison_acceptance.rs"]
mod comparisons;
#[path = "reparent_conflict_acceptance.rs"]
mod conflicts;
#[path = "fresh_boundary_acceptance.rs"]
mod fresh;
#[path = "restore_merge_acceptance.rs"]
mod restore;
#[path = "shared_context_acceptance.rs"]
mod shared;
#[path = "reparent_stages_acceptance.rs"]
mod stages;
#[path = "undo_merge_acceptance.rs"]
mod undo;
#[path = "replacement_world_acceptance.rs"]
mod world;
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("/usr/bin/git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string().into());
    }
    Ok(String::from_utf8(out.stdout)?.trim().into())
}
struct Fixture {
    root: tempfile::TempDir,
    source: PathBuf,
    journal: Journal,
    review: Service,
    workspaces: Vec<Workspace>,
    head: String,
    index: Vec<u8>,
    _host: crate::test_host::IdleTestHost,
    // Release admission after all fixture resources, including the host.
    _admission: tokio::sync::SemaphorePermit<'static>,
}
impl Fixture {
    async fn new(count: usize) -> Result<Self> {
        let admission = crate::test_host::FILE_FIXTURES.acquire().await?;
        let host = crate::test_host::IdleTestHost::new()?;
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        fs::create_dir(&source)?;
        git(&source, &["init", "-q", "-b", "main"])?;
        fs::write(source.join("parent.txt"), "Own initial base\n")?;
        git(&source, &["add", "."])?;
        git(
            &source,
            &[
                "-c",
                "user.name=Own dummy",
                "-c",
                "user.email=dummy@example.invalid",
                "commit",
                "-qm",
                "Own initial base",
            ],
        )?;
        let head = git(&source, &["rev-parse", "HEAD"])?;
        let index = fs::read(source.join(".git/index"))?;
        let journal = Journal::open(
            root.path().join("events.sqlite"),
            crate::secrets::Key::new([7; 32]),
        )
        .await?;
        journal
            .configure_project(
                Uuid::now_v7(),
                crate::project::inspect(
                    Uuid::now_v7(),
                    "Own reparent test".into(),
                    vec![source.clone()],
                )
                .await?,
            )
            .await?;
        let project = journal.snapshot().await?["projectSecrets"]["projectId"]
            .as_str()
            .unwrap()
            .to_owned();
        journal
            .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
            .await?;
        let catalog = journal
            .prepare_project_secrets(crate::project_secrets::Prepare {
                op_id: Uuid::now_v7(),
                project_id: project.clone(),
                revision: 0,
                fingerprints: vec![],
                key_check: crate::secrets::Key::new([7; 32])
                    .check()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
            })
            .await?;
        journal.confirm_fingerprints(catalog.current).await?;
        journal
            .activate_project_secrets(crate::project_secrets::Load {
                project_id: project,
                revision: 0,
                entries: vec![],
            })
            .await?;
        for i in 0..count {
            journal
                .stage_utterance(
                    Uuid::now_v7(),
                    format!("Own dummy track {i}"),
                    Target::New,
                    None,
                    vec![],
                )
                .await?;
            let input = journal.claim_input().await?.unwrap();
            journal
                .apply_input(
                    input.id,
                    input.revision,
                    Outcome::Classified(Decision {
                        kind: Kind::Fork,
                        track_id: None,
                        boundary: Some("when-told".into()),
                        command: None,
                        confidence: 1.,
                        inference_ms: 0.,
                        timed_out: false,
                        scores: json!({}),
                    }),
                )
                .await?;
        }
        let prep = Preparation::start(&root.path().join("workers"), journal.clone())?;
        let mut workspaces = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let workspaces: Vec<Workspace> =
                    serde_json::from_value(journal.snapshot().await?["workspaces"].clone())?;
                if workspaces.len() == count && workspaces.iter().all(|w| w.state == "ready") {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(workspaces);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        prep.shutdown().await;
        let db = rusqlite::Connection::open(root.path().join("events.sqlite"))?;
        // Release the three original private names before assigning A2, B, C.
        for workspace in &workspaces[1..] {
            git(&source, &["update-ref", "-d", &workspace.reference])?;
        }
        for (workspace, label) in workspaces[1..].iter_mut().zip(["A2", "B", "C", "D"]) {
            workspace.reference = format!(
                "{}/{label}",
                workspace.reference.rsplit_once('/').unwrap().0
            );
            git(&source, &["update-ref", &workspace.reference, &head])?;
            db.execute(
                "UPDATE tracks SET label=?2 WHERE id=?1",
                params![workspace.track_id, label],
            )?;
            db.execute(
                "UPDATE workspaces SET record=?2 WHERE track_id=?1",
                params![workspace.track_id, serde_json::to_string(workspace)?],
            )?;
        }
        let review = Service::new(journal.clone(), root.path().join("integration"));
        Ok(Self {
            _admission: admission,
            root,
            source,
            journal,
            review,
            workspaces,
            head,
            index,
            _host: host,
        })
    }
    async fn save(&self, index: usize, previous: Option<Checkpoint>) -> Result<Checkpoint> {
        checkpoint::save(
            self.workspaces[index].clone(),
            Uuid::now_v7(),
            previous,
            false,
            self.journal.clone(),
        )
        .await
    }
    fn inherit(&self, child: usize, parent: usize, checkpoint: &Checkpoint) -> Result<()> {
        let db = rusqlite::Connection::open(self.root.path().join("events.sqlite"))?;
        let point =
            crate::lineage_store::point(&db, &self.workspaces[parent].track_id, &checkpoint.sha)?;
        // Initial inheritance copies the saved base and its coverage together,
        // just as inheritance::combine does for a newly created worker.
        let mut basis = point.basis();
        basis.parents =
            std::collections::BTreeMap::from([(point.track.clone(), point.inherited())]);
        basis.inherits = point.inherits.clone();
        crate::lineage::include(&mut basis.inherits, &point.track, &point.inherited())?;
        crate::lineage_store::set_basis(&db, &self.workspaces[child].track_id, &basis)?;
        git(
            &self.workspaces[child].path,
            &["read-tree", "--reset", "-u", &checkpoint.sha],
        )?;
        // Inheritance changes files, without staging them against the worker's
        // detached HEAD. Keep this fixture's index at that original HEAD too.
        git(
            &self.workspaces[child].path,
            &["read-tree", "--reset", "HEAD"],
        )?;
        Ok(())
    }
    fn branch(&self, point: &Checkpoint) -> Result<()> {
        let branch = crate::branch::Branch {
            request: crate::branch::Request {
                op_id: Uuid::now_v7(),
                parent_track_id: self.workspaces[0].track_id.clone(),
                point: crate::branch_start::Selection::Turn {
                    turn_id: point.turn_id,
                },
                instructions: crate::branch::InstructionMode::Checkpoint,
            },
            track_id: self.workspaces[1].track_id.clone(),
            parent_session_id: "own-parent-session".into(),
            session_id: Some("own-alt-session".into()),
            workspace: self.workspaces[1].clone(),
            point: crate::branch_start::Point::Turn {
                checkpoint: point.clone(),
            },
            phase: crate::branch::Phase::Ready,
            instructions: None,
            origin: crate::branch_points::Origin {
                ended_at: Some(1000),
                timeline_at: Some(1000),
                turn_number: 1,
            },
        };
        let db = rusqlite::Connection::open(self.root.path().join("events.sqlite"))?;
        db.execute(
            "INSERT INTO branches VALUES(?1,?2)",
            params![
                branch.request.op_id.to_string(),
                serde_json::to_string(&branch)?
            ],
        )?;
        self.inherit(1, 0, point)
    }
    async fn action(&self, action: Action) -> Result<Job> {
        // Nested world sequences must not embed the large service future in
        // every fixture future on the test thread's stack.
        let job = Box::pin(self.review.command(ReviewCommand {
            op_id: Uuid::now_v7(),
            action,
        }))
        .await?;
        if job.status == Status::Ordering {
            Box::pin(self.review.command(ReviewCommand {
                op_id: Uuid::now_v7(),
                action: Action::Order {
                    id: job.id,
                    revision: job.revision,
                    tracks: job.order_review.as_ref().unwrap().order.clone(),
                },
            }))
            .await
        } else {
            Ok(job)
        }
    }
    async fn settled(&self, id: Uuid) -> Result<Job> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let job = self.journal.integration_job(id).await?;
                if !matches!(
                    job.status,
                    Status::Building | Status::Staging | Status::Committing | Status::Preparing
                ) {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?
    }
    async fn publish(&self, tracks: Vec<String>) -> Result<Job> {
        let open = self
            .action(Action::Open {
                tracks: Some(tracks),
            })
            .await?;
        self.action(Action::Prepare {
            id: open.id,
            revision: open.revision,
        })
        .await?;
        let checked = self.settled(open.id).await?;
        assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
        self.action(commit(&checked)).await?;
        let done = self.settled(checked.id).await?;
        assert_eq!(done.status, Status::Done);
        Ok(done)
    }
    async fn worker(&self, index: usize, checkpoint: Checkpoint) -> Result<WorkerState> {
        let worker = WorkerState {
            track_id: self.workspaces[index].track_id.clone(),
            phase: "ready".into(),
            session_id: Some(format!("own-worker-{index}")),
            secret_revision: Some(0),
            identity: Some(self._host.identity()?),
            detail: None,
            active_turn: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: Some(checkpoint),
            prompts: vec![],
        };
        self.journal.worker_state(worker.clone()).await?;
        Ok(worker)
    }
    async fn apply_files(
        &self,
        index: usize,
        request: &Request,
    ) -> Result<Option<crate::realign_apply::Applied>> {
        let snapshot = self.journal.snapshot().await?;
        let session = snapshot["workers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|worker| worker["trackId"] == self.workspaces[index].track_id)
            .and_then(|worker| worker["sessionId"].as_str())
            .ok_or("Missing own realignment session")?
            .to_owned();
        let identity = self._host.identity()?;
        let tracker = Arc::new(Mutex::new(crate::process::Tracker::new(
            format!("own-reparent-{index}"),
            identity,
        )?));
        let gate = Arc::new(crate::dispatch_control::Gate::new(false));
        for _ in 0..4 {
            let boundary = crate::inheritance_apply::Boundary {
                guard: crate::inheritance_apply::Guard::Recovery {
                    generation: Arc::new(AtomicU64::new(0)),
                    expected: 0,
                    epoch: gate.epoch(),
                    gate: gate.clone(),
                },
                tracker: tracker.clone(),
                identity,
            };
            match Box::pin(crate::realign_apply::apply(
                &self.journal,
                request.clone(),
                &session,
                boundary,
            ))
            .await
            {
                Ok(result) => return Ok(result),
                Err(error) => {
                    let Some(required) =
                        error.downcast_ref::<crate::workspace_access::ConfirmationRequired>()
                    else {
                        return Err(error);
                    };
                    // This consent is limited to the fixture's own temporary
                    // files; no user's checkout or application is changed.
                    self.journal
                        .approve_realign_access(crate::workspace_access::Consent {
                            op_id: Uuid::now_v7(),
                            request: request.clone(),
                            fingerprint: required.inspection.fingerprint.clone(),
                        })
                        .await?;
                }
            }
        }
        Err("The own dummy access boundary did not settle".into())
    }
    async fn reparent(&self, index: usize, parent: usize, job: &Job) -> Result<Job> {
        let request = Request {
            op_id: Uuid::now_v7(),
            track_id: self.workspaces[index].track_id.clone(),
            source: Source::Reparent {
                job_id: job.id,
                revision: job.revision,
                parent: self.workspaces[parent].track_id.clone(),
            },
        };
        let applied = self
            .apply_files(index, &request)
            .await?
            .ok_or("Missing own dummy file application")?;
        assert!(
            self.action(Action::ChoosePath {
                id: job.id,
                revision: job.revision,
                track: self.workspaces[0].track_id.clone()
            })
            .await
            .is_err(),
            "the file application owns the saved path review until its worker state is durable"
        );
        let mut worker: WorkerState = serde_json::from_value(
            self.journal.snapshot().await?["workers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|worker| worker["trackId"] == request.track_id)
                .unwrap()
                .clone(),
        )?;
        worker.checkpoint = Some(applied.checkpoint);
        self.journal.worker_state(worker).await?;
        if index == 2 {
            let db = rusqlite::Connection::open(self.root.path().join("events.sqlite"))?;
            db.execute_batch("CREATE TRIGGER own_reparent_review_fault BEFORE UPDATE ON integration_jobs WHEN json_extract(NEW.record,'$.revision')>json_extract(OLD.record,'$.revision') BEGIN SELECT RAISE(ABORT,'Own dummy review write failure'); END;")?;
            assert!(
                self.journal
                    .change_realign(crate::realign_store::Change::Complete(applied.operation))
                    .await
                    .is_err()
            );
            assert_eq!(
                self.journal.operation(applied.operation).await?.unwrap()["state"],
                "dispatched"
            );
            assert_eq!(
                self.journal.integration_job(job.id).await?.revision,
                job.revision
            );
            db.execute_batch("DROP TRIGGER own_reparent_review_fault;")?;
        }
        self.journal
            .change_realign(crate::realign_store::Change::Complete(applied.operation))
            .await?;
        assert!(
            self.apply_files(index, &request).await?.is_none(),
            "a saved acknowledgement replays without another file update or checkpoint"
        );
        let next = self.journal.integration_job(job.id).await?;
        assert_eq!(
            next.tracks
                .iter()
                .find(|track| track.id == self.workspaces[index].track_id)
                .unwrap()
                .checkpoint_count,
            job.tracks
                .iter()
                .find(|track| track.id == self.workspaces[index].track_id)
                .unwrap()
                .checkpoint_count
                + 1
        );
        self.journal.integration_job(job.id).await
    }
}
fn commit(job: &Job) -> Action {
    let c = job.candidate.as_ref().unwrap();
    Action::Commit {
        id: job.id,
        revision: job.revision,
        head: c.head.clone(),
        tree: c.tree.clone(),
        settings_hash: c.settings_hash.clone(),
    }
}

#[tokio::test]
async fn reparenting_with_an_advanced_independent_parent_keeps_a_target_deletion() -> Result<()> {
    let f = Fixture::new(4).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own fork context\n",
    )?;
    let fork = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own retired later parent\n",
    )?;
    let old = f.save(0, Some(fork.clone())).await?;
    f.publish(vec![f.workspaces[0].track_id.clone()]).await?;
    fs::write(
        f.workspaces[3].path.join("early.txt"),
        "Own earlier C file\n",
    )?;
    let first = f.save(3, None).await?;
    let published = f.publish(vec![f.workspaces[3].track_id.clone()]).await?;
    // Another publication deletes C's old file while keeping its coverage.
    let repo = intercat_git::Repository::open(&f.source)?;
    let parent = published.candidate.as_ref().unwrap().head.parse()?;
    let blob = repo.write_blob(b"Own retired later parent\n")?;
    let tree = repo.write_tree(&[("parent.txt", blob)])?;
    let external = repo.checkpoint_commit(tree, parent, "Own later C file deletion")?;
    repo.sync_objects(&[external])?;
    let target = &published.plan.as_ref().unwrap().target_ref;
    repo.compare_and_swap(target, Some(parent), external)?;
    repo.sync_ref(target)?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    db.execute(
        "UPDATE published_lineage SET head=?2 WHERE reference=?1",
        params![target, external.to_string()],
    )?;
    fs::write(f.workspaces[3].path.join("later.txt"), "Own later C file\n")?;
    let current = f.save(3, Some(first)).await?;
    f.inherit(2, 0, &old)?;
    for (path, content) in [
        ("early.txt", "Own earlier C file\n"),
        ("later.txt", "Own later C file\n"),
        ("pending.txt", "Own B edit\n"),
    ] {
        fs::write(f.workspaces[2].path.join(path), content)?;
    }
    let point = crate::lineage_store::point(&db, &f.workspaces[3].track_id, &current.sha)?;
    let mut basis = crate::lineage_store::basis(&db, &f.workspaces[2].track_id)?;
    basis.parents.insert(point.track.clone(), point.inherited());
    crate::lineage::include(&mut basis.inherits, &point.track, &point.inherited())?;
    crate::lineage_store::set_basis(&db, &f.workspaces[2].track_id, &basis)?;
    let child = f.save(2, None).await?;
    f.worker(2, child).await?;
    f.branch(&fork)?;
    fs::write(
        f.workspaces[1].path.join("parent.txt"),
        "Own chosen alternative\n",
    )?;
    f.save(1, None).await?;
    let opened = f.action(Action::Open { tracks: None }).await?;
    let chosen = f
        .action(Action::ChoosePath {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    let changed = f.reparent(2, 0, &chosen).await?;
    assert_eq!(changed.status, Status::Preparing);
    assert!(
        f.workspaces[2].path.join("early.txt").exists(),
        "a reparent keeps the worker's inherited files"
    );
    let ready = f.action(Action::Open { tracks: None }).await?;
    f.action(Action::Prepare {
        id: ready.id,
        revision: ready.revision,
    })
    .await?;
    let checked = f.settled(ready.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let files = repo.tree_files(checked.candidate.as_ref().unwrap().tree.parse()?)?;
    assert!(
        !files.contains_key("early.txt"),
        "the independent parent's earlier file is base context, not B's addition"
    );
    assert!(files.contains_key("later.txt"));
    assert!(files.contains_key("pending.txt"));
    assert_eq!(
        repo.saved_bytes(&files["parent.txt"])?,
        b"Own chosen alternative\n"
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    f.review.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn reparent_chain_applies_files_and_checkpoints_and_preserves_a_later_target_deletion()
-> Result<()> {
    let f = Fixture::new(4).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own fork context\n",
    )?;
    let fork = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own retired later parent\n",
    )?;
    let later = f.save(0, Some(fork.clone())).await?;
    f.publish(vec![f.workspaces[0].track_id.clone()]).await?;
    f.inherit(2, 0, &later)?;
    fs::write(
        f.workspaces[2].path.join("done.txt"),
        "Own integrated child edit\n",
    )?;
    let b1 = f.save(2, None).await?;
    let bdone = f.publish(vec![f.workspaces[2].track_id.clone()]).await?;
    // A later independent edit deletes an already integrated B file.
    let repo = intercat_git::Repository::open(&f.source)?;
    let parent = bdone.candidate.as_ref().unwrap().head.parse()?;
    let parent_blob = repo.write_blob(b"Own retired later parent\n")?;
    let tree = repo.write_tree(&[("parent.txt", parent_blob)])?;
    let external = repo.checkpoint_commit(tree, parent, "Own later target deletion")?;
    repo.sync_objects(&[external])?;
    let target = &bdone.plan.as_ref().unwrap().target_ref;
    repo.compare_and_swap(target, Some(parent), external)?;
    repo.sync_ref(target)?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    db.execute(
        "UPDATE published_lineage SET head=?2 WHERE reference=?1",
        params![target, external.to_string()],
    )?;
    fs::write(
        f.workspaces[2].path.join("pending.txt"),
        "Own pending child edit\n",
    )?;
    let b2 = f.save(2, Some(b1.clone())).await?;
    f.inherit(3, 2, &b2)?;
    fs::write(
        f.workspaces[3].path.join("child.txt"),
        "Own grandchild edit\n",
    )?;
    let c1 = f.save(3, None).await?;
    f.branch(&fork)?;
    fs::write(
        f.workspaces[1].path.join("parent.txt"),
        "Own chosen alternative\n",
    )?;
    let alt = f.save(1, None).await?;
    f.worker(2, b2).await?;
    f.worker(3, c1).await?;
    fs::write(
        f.workspaces[2].path.join("dirty.txt"),
        "Own unsaved child edit\n",
    )?;
    let open = f.action(Action::Open { tracks: None }).await?;
    assert_eq!(open.status, Status::Choosing);
    let chosen = f
        .action(Action::ChoosePath {
            id: open.id,
            revision: open.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    assert_eq!(
        chosen
            .path_state
            .as_ref()
            .unwrap()
            .descendants
            .iter()
            .map(|d| d.track.as_str())
            .collect::<Vec<_>>(),
        vec![
            f.workspaces[2].track_id.as_str(),
            f.workspaces[3].track_id.as_str()
        ]
    );
    let premature = Request {
        op_id: Uuid::now_v7(),
        track_id: f.workspaces[3].track_id.clone(),
        source: Source::Reparent {
            job_id: chosen.id,
            revision: chosen.revision,
            parent: f.workspaces[2].track_id.clone(),
        },
    };
    assert!(f.journal.realign_context(premature).await.is_err());
    let after_b = f.reparent(2, 0, &chosen).await?;
    assert_eq!(after_b.revision, chosen.revision + 1);
    assert_eq!(after_b.status, Status::Choosing);
    assert_eq!(
        after_b
            .tracks
            .iter()
            .find(|track| track.id == f.workspaces[2].track_id)
            .unwrap()
            .basis
            .parents[&f.workspaces[1].track_id]
            .sha,
        alt.sha
    );
    assert_eq!(
        after_b.lineage.integrated[&f.workspaces[2].track_id].sha, b1.sha,
        "own integration cursor is unchanged"
    );
    assert_eq!(
        fs::read_to_string(f.workspaces[2].path.join("parent.txt"))?,
        "Own chosen alternative\n"
    );
    assert!(f.workspaces[2].path.join("done.txt").exists());
    assert!(f.workspaces[2].path.join("dirty.txt").exists());
    let after_c = f.reparent(3, 2, &after_b).await?;
    assert_eq!(after_c.status, Status::Preparing);
    assert!(after_c.path_state.as_ref().unwrap().ready());
    assert_eq!(
        fs::read_to_string(f.workspaces[3].path.join("parent.txt"))?,
        "Own chosen alternative\n"
    );
    assert!(f.workspaces[3].path.join("child.txt").exists());
    let ready = f.action(Action::Open { tracks: None }).await?;
    assert_eq!(ready.status, Status::Open, "{:?}", ready.error);
    f.action(Action::Prepare {
        id: ready.id,
        revision: ready.revision,
    })
    .await?;
    let checked = f.settled(ready.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let files = repo.tree_files(checked.candidate.as_ref().unwrap().tree.parse()?)?;
    assert!(
        !files.contains_key("done.txt"),
        "realignment must not re-add an already integrated file deleted from the target"
    );
    for file in ["pending.txt", "dirty.txt", "child.txt"] {
        assert!(files.contains_key(file), "{file}");
    }
    assert_eq!(
        repo.saved_bytes(&files["parent.txt"])?,
        b"Own chosen alternative\n"
    );
    let committing = f
        .journal
        .integration_command(ReviewCommand {
            op_id: Uuid::now_v7(),
            action: commit(&checked),
        })
        .await?;
    assert_eq!(committing.status, Status::Committing);
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    let entries = entries
        .into_iter()
        .filter(|entry| entry.job == committing.id)
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.track, f.workspaces[0].track_id);
    // A has never hosted a session in this fixture, so its saved checkpoint is
    // already the stopped boundary. Exercise the driver's journal/ref steps
    // without starting real Muse hosts for B and C's synthetic session IDs.
    f.journal
        .change_path_retirement(crate::path_retirement::Change::Freeze {
            id: entry.id,
            session: None,
            checkpoint: None,
        })
        .await?;
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    let saved = entries
        .iter()
        .find(|saved| saved.id == entry.id)
        .unwrap()
        .saved
        .as_ref()
        .unwrap();
    assert_eq!(saved.sha, later.sha);
    repo.compare_and_swap(&saved.reference, None, saved.sha.parse()?)?;
    repo.sync_ref(&saved.reference)?;
    f.journal
        .change_path_retirement(crate::path_retirement::Change::Ready(entry.id))
        .await?;
    db.execute_batch("CREATE TRIGGER own_reparent_publication_fault BEFORE INSERT ON integration_cancelled BEGIN SELECT RAISE(ABORT,'Own dummy publication write failure'); END;")?;
    assert!(
        crate::publication::publish(
            committing.clone(),
            f.journal.clone(),
            tokio::sync::watch::channel(false).1
        )
        .await
        .is_err()
    );
    let candidate = checked.candidate.as_ref().unwrap();
    assert_eq!(
        repo.reference_target(target)?,
        Some(candidate.head.parse()?)
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM integration_cancelled", [], |row| row
            .get::<_, i64>(
            0
        ))?,
        0
    );
    assert_eq!(
        f.journal.integration_job(checked.id).await?.status,
        Status::Committing
    );
    db.execute_batch("DROP TRIGGER own_reparent_publication_fault;")?;
    let done = crate::publication::publish(
        f.journal.integration_job(checked.id).await?,
        f.journal.clone(),
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert_eq!(done.status, Status::Done, "{:?}", done.error);
    assert_eq!(done.candidate.as_ref().unwrap().head, candidate.head);
    assert_eq!(
        repo.reference_target(target)?,
        Some(candidate.head.parse()?)
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM integration_cancelled", [], |row| row
            .get::<_, i64>(
            0
        ))?,
        1
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM integration_undos", [], |row| row
            .get::<_, i64>(0))?,
        1
    );
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    assert_eq!(
        entries
            .iter()
            .find(|saved| saved.id == entry.id)
            .unwrap()
            .phase,
        crate::path_retirement::Phase::Aside
    );
    assert_eq!(
        repo.tree_id(candidate.head.parse()?)?.to_string(),
        candidate.tree
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    assert_eq!(
        fs::read_to_string(f.source.join("parent.txt"))?,
        "Own initial base\n"
    );
    f.review.shutdown().await;
    Ok(())
}
