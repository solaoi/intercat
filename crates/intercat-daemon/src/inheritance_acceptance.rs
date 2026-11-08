//! Exercise inheritance through saved file application and real local commits.
//! All repositories and worker inputs belong to this disposable fixture.
use crate::{
    Result,
    checkpoint::{self, Checkpoint},
    decision::{Decision, Kind, Outcome, Target},
    inheritance::Dependencies,
    inheritance_apply::{Boundary, Guard},
    inheritance_apply_store::Change,
    integration::{Action, Command, Job, Service, Status},
    journal::Journal,
    preparation::{Preparation, Workspace},
    runtime::WorkerState,
};
use intercat_git::Repository;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

struct Fixture {
    root: tempfile::TempDir,
    source: std::path::PathBuf,
    journal: Journal,
    service: Service,
    workspaces: Vec<Workspace>,
    points: Vec<Option<Checkpoint>>,
    original_index: Vec<u8>,
    original_head: String,
}

fn git(path: &Path, arguments: &[&str]) -> Result<String> {
    let result = std::process::Command::new("/usr/bin/git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()?;
    if !result.status.success() {
        return Err(String::from_utf8_lossy(&result.stderr).into_owned().into());
    }
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

impl Fixture {
    async fn new(count: usize) -> Result<Self> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        let source = root.path().join("source");
        fs::create_dir(&source)?;
        git(&source, &["init", "-q", "-b", "main"])?;
        fs::write(source.join("root.txt"), "unchanged project root\n")?;
        fs::write(source.join("value.txt"), "100\n")?;
        git(&source, &["add", "."])?;
        git(
            &source,
            &[
                "-c",
                "user.name=Dummy",
                "-c",
                "user.email=dummy@example.invalid",
                "commit",
                "-qm",
                "Dummy root",
            ],
        )?;
        let original_head = git(&source, &["rev-parse", "HEAD"])?;
        let original_index = fs::read(source.join(".git/index"))?;
        let journal =
            Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
        journal
            .configure_project(
                Uuid::now_v7(),
                crate::project::inspect(
                    Uuid::now_v7(),
                    "Dummy inheritance".into(),
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
        let mut tracks = Vec::new();
        for index in 0..count {
            let id = Uuid::now_v7();
            journal
                .stage_utterance(
                    id,
                    format!("Dummy worker {index}"),
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
            tracks.push(
                journal.read_utterance(id.to_string()).await?["decision"]["trackId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        let preparation = Preparation::start(&root.path().join("workspaces"), journal.clone())?;
        let prepared = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let values: Vec<Workspace> =
                    serde_json::from_value(journal.snapshot().await?["workspaces"].clone())?;
                if let Some(failed) = values.iter().find(|w| w.state == "failed") {
                    return Err(failed.error.clone().unwrap_or_default().into());
                }
                if values.len() == count && values.iter().all(|w| w.state == "ready") {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                        tracks
                            .iter()
                            .map(|id| values.iter().find(|w| &w.track_id == id).unwrap().clone())
                            .collect(),
                    );
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        preparation.shutdown().await;
        let workspaces = prepared??;
        let service = Service::new(journal.clone(), root.path().join("integration"));
        let value = Self {
            root,
            source,
            journal,
            service,
            workspaces,
            points: vec![None; count],
            original_index,
            original_head,
        };
        for index in 0..count {
            value.worker(index, None).await?;
        }
        Ok(value)
    }
    fn db(&self) -> Result<rusqlite::Connection> {
        let db = rusqlite::Connection::open(self.root.path().join("db"))?;
        crate::clock::register(&db)?;
        Ok(db)
    }
    async fn worker(&self, index: usize, checkpoint: Option<Checkpoint>) -> Result<()> {
        self.journal
            .worker_state(WorkerState {
                track_id: self.workspaces[index].track_id.clone(),
                phase: "ready".into(),
                session_id: Some(format!("dummy-session-{index}")),
                secret_revision: Some(0),
                identity: None,
                detail: None,
                active_turn: None,
                approvals: vec![],
                output: None,
                checkpoint_pending: false,
                checkpoint,
                prompts: vec![],
            })
            .await
    }
    fn edit(&self, index: usize, path: &str, value: Option<&str>) -> Result<()> {
        let path = self.workspaces[index].path.join(path);
        if let Some(value) = value {
            fs::write(path, value)?;
        } else {
            fs::remove_file(path)?;
        }
        Ok(())
    }
    async fn save(&mut self, index: usize) -> Result<crate::lineage::Point> {
        let checkpoint = checkpoint::save(
            self.workspaces[index].clone(),
            Uuid::now_v7(),
            self.points[index].clone(),
            false,
            self.journal.clone(),
        )
        .await?;
        self.worker(index, Some(checkpoint.clone())).await?;
        self.points[index] = Some(checkpoint.clone());
        crate::lineage_store::point(
            &self.db()?,
            &self.workspaces[index].track_id,
            &checkpoint.sha,
        )
    }
    async fn inherit(&mut self, index: usize, parents: &[crate::lineage::Point]) -> Result<()> {
        let id = Uuid::now_v7();
        let track = self.workspaces[index].track_id.clone();
        self.journal
            .stage_utterance(
                id,
                "Use the saved dummy dependencies".into(),
                Target::Lock(track.clone()),
                None,
                vec![],
            )
            .await?;
        let input = self.journal.claim_input().await?.unwrap();
        assert_eq!(input.id, id.to_string());
        self.journal
            .apply_input(
                input.id,
                input.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Schedule,
                    track_id: Some(track.clone()),
                    boundary: Some("when-told".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        crate::inheritance::queue(
            &self.db()?,
            &Dependencies {
                input: id.to_string(),
                track,
                points: parents.to_vec(),
            },
        )?;
        let revision = self.journal.read_utterance(id.to_string()).await?["revision"]
            .as_i64()
            .unwrap();
        let identity = crate::process::identity(std::process::id())?;
        let index_path = gix::open(&self.workspaces[index].path)?
            .git_dir()
            .join("index");
        let before_index = fs::read(&index_path)?;
        let before_head = git(&self.workspaces[index].path, &["rev-parse", "HEAD"])?;
        let applied = crate::inheritance_apply::prepare(
            &self.journal,
            id.to_string(),
            revision,
            format!("dummy-session-{index}"),
            self.workspaces[index].clone(),
            self.points[index].clone(),
            Boundary {
                guard: Guard::Work(crate::msp::StartGuard::Stopped(Arc::new(
                    std::sync::atomic::AtomicBool::new(false),
                ))),
                tracker: Arc::new(Mutex::new(crate::process::Tracker::new(
                    Uuid::now_v7().to_string(),
                    identity,
                )?)),
                identity,
            },
        )
        .await?
        .ok_or("The dummy inheritance was not applied")?;
        self.points[index] = Some(applied.checkpoint.clone());
        self.worker(index, Some(applied.checkpoint)).await?;
        self.journal
            .change_inheritance(Change::Complete(applied.operation))
            .await?;
        assert_eq!(fs::read(index_path)?, before_index);
        assert_eq!(
            git(&self.workspaces[index].path, &["rev-parse", "HEAD"])?,
            before_head
        );
        assert!(crate::inheritance::waiting(&self.db()?, &id.to_string())?.is_none());
        Ok(())
    }
    async fn open(&self, tracks: &[usize]) -> Result<Job> {
        let job = self
            .service
            .command(Command {
                op_id: Uuid::now_v7(),
                action: Action::Open {
                    tracks: Some(
                        tracks
                            .iter()
                            .map(|i| self.workspaces[*i].track_id.clone())
                            .collect(),
                    ),
                },
            })
            .await?;
        if job.status == Status::Ordering {
            self.service
                .command(Command {
                    op_id: Uuid::now_v7(),
                    action: Action::Order {
                        id: job.id,
                        revision: job.revision,
                        tracks: job.order_review.as_ref().unwrap().order.clone(),
                    },
                })
                .await
        } else {
            Ok(job)
        }
    }
    async fn settled(&self) -> Result<Job> {
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                let value = self.journal.snapshot().await?["integration"].clone();
                let job: Job = serde_json::from_value(value)?;
                if !matches!(
                    job.status,
                    Status::Preparing | Status::Building | Status::Staging | Status::Committing
                ) {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }
    async fn publish_job(&self, job: Job) -> Result<BTreeMap<String, String>> {
        assert_eq!(job.status, Status::Open, "{:?}", job.error);
        self.service
            .command(Command {
                op_id: Uuid::now_v7(),
                action: Action::Prepare {
                    id: job.id,
                    revision: job.revision,
                },
            })
            .await?;
        let checked = self.settled().await?;
        assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
        let candidate = checked.candidate.as_ref().unwrap();
        self.service
            .command(Command {
                op_id: Uuid::now_v7(),
                action: Action::Commit {
                    id: checked.id,
                    revision: checked.revision,
                    head: candidate.head.clone(),
                    tree: candidate.tree.clone(),
                    settings_hash: candidate.settings_hash.clone(),
                },
            })
            .await?;
        let done = self.settled().await?;
        assert_eq!(done.status, Status::Done, "{:?}", done.error);
        assert_eq!(
            fs::read(self.source.join(".git/index"))?,
            self.original_index
        );
        assert_eq!(
            git(&self.source, &["rev-parse", "HEAD"])?,
            self.original_head
        );
        let repo = Repository::open(&self.source)?;
        if let Ok(directory) = std::env::var("INTERCAT_INHERITANCE_OUTPUT") {
            let directory =
                std::path::PathBuf::from(directory).join(self.root.path().file_name().unwrap());
            fs::create_dir_all(&directory)?;
            fs::write(
                directory.join(format!("{}.json", done.id)),
                serde_json::to_vec_pretty(&done)?,
            )?;
        }
        repo.saved_files(done.candidate.unwrap().head.parse()?)?
            .into_iter()
            .map(|(path, file)| Ok((path, String::from_utf8(repo.saved_bytes(&file)?)?)))
            .collect()
    }
    async fn publish(&self, tracks: &[usize]) -> Result<BTreeMap<String, String>> {
        self.publish_job(self.open(tracks).await?).await
    }
}

#[tokio::test]
async fn multiple_parents_keep_child_work_after_an_updated_parent_deletes_its_file() -> Result<()> {
    for integrate_parents_first in [false, true] {
        let mut f = Fixture::new(3).await?;
        f.edit(0, "a.txt", Some("A\n"))?;
        let a1 = f.save(0).await?;
        f.edit(1, "b.txt", Some("B\n"))?;
        let b1 = f.save(1).await?;
        f.inherit(2, &[a1, b1]).await?;
        f.edit(2, "c.txt", Some("C\n"))?;
        f.save(2).await?;
        if integrate_parents_first {
            f.publish(&[0, 1]).await?;
        }
        f.edit(0, "g.txt", Some("later A\n"))?;
        let a2 = f.save(0).await?;
        f.inherit(2, &[a2]).await?;
        f.edit(0, "g.txt", None)?;
        f.save(0).await?;
        f.publish(&[0]).await?;
        let files = f.publish(&[2]).await?;
        assert!(!files.contains_key("g.txt"));
        for name in ["a.txt", "b.txt", "c.txt"] {
            assert!(files.contains_key(name), "{name}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn three_generations_keep_the_intermediate_ancestor_deletion_after_partial_commits()
-> Result<()> {
    let mut f = Fixture::new(3).await?;
    f.edit(0, "a.txt", Some("A\n"))?;
    f.save(0).await?;
    f.edit(0, "g.txt", Some("A second\n"))?;
    let a2 = f.save(0).await?;
    f.inherit(1, &[a2]).await?;
    f.edit(1, "b.txt", Some("B\n"))?;
    let b1 = f.save(1).await?;
    f.inherit(2, &[b1]).await?;
    f.edit(2, "c.txt", Some("C\n"))?;
    f.save(2).await?;
    f.edit(0, "g.txt", None)?;
    f.save(0).await?;
    f.publish(&[0]).await?;
    f.publish(&[1]).await?;
    let files = f.publish(&[2]).await?;
    assert!(!files.contains_key("g.txt"));
    for name in ["a.txt", "b.txt", "c.txt"] {
        assert!(files.contains_key(name), "{name}");
    }
    Ok(())
}

#[tokio::test]
async fn a_diamond_applies_the_newer_shared_ancestor_once_before_merging_children() -> Result<()> {
    for reverse in [false, true] {
        let mut f = Fixture::new(4).await?;
        f.edit(0, "f.txt", Some("old A\n"))?;
        let a1 = f.save(0).await?;
        f.inherit(1, &[a1]).await?;
        f.edit(1, "b.txt", Some("B\n"))?;
        let b1 = f.save(1).await?;
        f.edit(0, "f.txt", None)?;
        let a2 = f.save(0).await?;
        f.inherit(2, &[a2]).await?;
        f.edit(2, "c.txt", Some("C\n"))?;
        let c1 = f.save(2).await?;
        f.inherit(3, &if reverse { [c1, b1] } else { [b1, c1] })
            .await?;
        f.edit(3, "d.txt", Some("D\n"))?;
        f.save(3).await?;
        assert!(!f.workspaces[3].path.join("f.txt").exists());
        let files = f.publish(&[3]).await?;
        assert!(!files.contains_key("f.txt"));
        for name in ["b.txt", "c.txt", "d.txt"] {
            assert!(files.contains_key(name), "{name}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_published_child_can_inherit_again_without_restoring_a_new_parent_deletion() -> Result<()>
{
    let mut f = Fixture::new(2).await?;
    f.edit(0, "a.txt", Some("A\n"))?;
    let a1 = f.save(0).await?;
    f.inherit(1, &[a1]).await?;
    f.edit(1, "c.txt", Some("C\n"))?;
    f.save(1).await?;
    f.publish(&[1]).await?;
    f.edit(0, "g.txt", Some("A later\n"))?;
    let a2 = f.save(0).await?;
    f.inherit(1, &[a2]).await?;
    f.edit(0, "g.txt", None)?;
    f.save(0).await?;
    f.publish(&[0]).await?;
    let files = f.publish(&[1]).await?;
    assert!(!files.contains_key("g.txt"));
    assert_eq!(files["c.txt"], "C\n");
    Ok(())
}

#[tokio::test]
async fn same_line_inherited_changes_integrate_in_order_or_with_the_parent_left_out() -> Result<()>
{
    for selected in [vec![0, 1], vec![1]] {
        let mut f = Fixture::new(2).await?;
        f.edit(0, "value.txt", Some("200\n"))?;
        let a1 = f.save(0).await?;
        f.inherit(1, std::slice::from_ref(&a1)).await?;
        f.edit(1, "value.txt", Some("300\n"))?;
        f.save(1).await?;
        let files = f.publish(&selected).await?;
        assert_eq!(files["value.txt"], "300\n");
        assert_eq!(
            crate::lineage_store::freeze(&f.db()?)?.integrated[&a1.track],
            a1.inherited()
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_frozen_early_parent_commit_and_later_deletion_do_not_return_through_its_child()
-> Result<()> {
    let mut f = Fixture::new(3).await?;
    f.edit(0, "f.txt", Some("A first\n"))?;
    let a1 = f.save(0).await?;
    let frozen = f.open(&[0]).await?;
    f.edit(0, "g.txt", Some("A second\n"))?;
    let a2 = f.save(0).await?;
    f.inherit(1, &[a2]).await?;
    f.edit(1, "b.txt", Some("B\n"))?;
    f.save(1).await?;
    f.publish_job(frozen).await?;
    f.inherit(2, &[a1]).await?;
    f.edit(2, "f.txt", None)?;
    f.save(2).await?;
    f.publish(&[2]).await?;
    let files = f.publish(&[1]).await?;
    assert!(!files.contains_key("f.txt"));
    assert_eq!(files["g.txt"], "A second\n");
    assert_eq!(files["b.txt"], "B\n");
    Ok(())
}
