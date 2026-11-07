use intercat_daemon::{
    Result, clip_files,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    preparation::Workspace,
    secrets::Key,
    timeline::Window,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
struct Fixture {
    root: tempfile::TempDir,
    journal: Journal,
    tracks: Vec<String>,
    workspaces: Vec<PathBuf>,
    base: String,
}
impl Fixture {
    async fn new(shared_repo: bool) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let repo = root.path().join("repository");
        fs::create_dir(&repo)?;
        git(&repo, &["init", "-q", "--template=", "-b", "main"]);
        fs::write(repo.join("shared.txt"), "base\n")?;
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Dummy",
                "-c",
                "user.email=dummy@example.invalid",
                "commit",
                "-qm",
                "Dummy base",
            ],
        );
        let base = git(&repo, &["rev-parse", "HEAD"]);
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let mut tracks = vec![];
        let mut workspaces = vec![];
        for index in 0..2 {
            journal
                .stage_utterance(
                    Uuid::now_v7(),
                    format!("Dummy {index}"),
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
                        boundary: Some("now".into()),
                        command: None,
                        confidence: 1.,
                        inference_ms: 0.,
                        timed_out: false,
                        scores: json!({}),
                    }),
                )
                .await?;
            let snapshot = journal.snapshot().await?;
            let track = snapshot["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|track| !tracks.contains(&track["id"].as_str().unwrap().to_owned()))
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let workspace = root.path().join(format!("worker-{index}"));
            if index == 1 && !shared_repo {
                git(
                    root.path(),
                    &[
                        "clone",
                        "-q",
                        repo.to_str().unwrap(),
                        workspace.to_str().unwrap(),
                    ],
                );
            } else {
                git(
                    &repo,
                    &[
                        "worktree",
                        "add",
                        "--detach",
                        workspace.to_str().unwrap(),
                        "HEAD",
                    ],
                );
            }
            let workspace = workspace.canonicalize()?;
            journal
                .workspace_result(Workspace {
                    track_id: track.clone(),
                    path: workspace.clone(),
                    reference: format!("refs/intercat/dummy-{index}"),
                    base: base.clone(),
                    state: "ready".into(),
                    error: None,
                    attachment_roots: vec![],
                })
                .await?;
            tracks.push(track);
            workspaces.push(workspace);
        }
        Ok(Self {
            root,
            journal,
            tracks,
            workspaces,
            base,
        })
    }
    fn patch(&self, index: usize, path: &str) -> Value {
        let content=json!({"files":[{"path":self.workspaces[index].join(path),"hunks":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-base","+changed"]}]}]}).to_string();
        json!({"content":content,"encoding":"utf8","byteLen":content.len(),"offsetBytes":0,"eof":true,"mediaType":"application/json"})
    }
    async fn edit(
        &self,
        index: usize,
        item: &str,
        path: &str,
        revision: u64,
        cache: bool,
    ) -> Result<String> {
        let output = self.patch(index, path);
        self.journal.try_msp(self.tracks[index].clone(),format!("session-{index}"),Stamp::now(),json!({"method":"item/completed","params":{"sessionId":format!("session-{index}"),"item":{"itemId":item,"turnId":format!("turn-{index}"),"kind":"toolCall","tool":"edit_file","revision":revision,"status":"completed","args":json!({"path":path}).to_string(),"patchRef":{"id":format!("patch-{item}"),"kind":"tool_patch","availability":"available","mediaType":"application/json","byteLen":output["byteLen"]}}}}),None,None)?.await??;
        if cache {
            self.cache(index, item, output).await?;
        }
        let db = rusqlite::Connection::open(self.root.path().join("db"))?;
        Ok(db.query_row(
            "SELECT clip FROM timeline_items WHERE session=?1 AND item=?2",
            rusqlite::params![format!("session-{index}"), item],
            |row| row.get(0),
        )?)
    }
    async fn cache(&self, index: usize, item: &str, output: Value) -> Result<()> {
        self.journal
            .cache_output(
                format!("session-{index}"),
                item.into(),
                format!("patch-{item}"),
                output,
            )
            .await
    }
    async fn process(&self) -> Result<()> {
        for _ in 0..20 {
            let Some(ticket) = self.journal.next_clip_files().await? else {
                return Ok(());
            };
            let comparison = clip_files::inspect(&ticket);
            assert!(self.journal.finish_clip_files(ticket, comparison).await?);
        }
        panic!("file scan did not settle");
    }
    async fn window(&self, since: Option<i64>) -> Result<Value> {
        self.journal
            .timeline_window(Window {
                tracks: self.tracks.clone(),
                start: 0,
                end: i64::MAX,
                since,
            })
            .await
    }
    async fn warnings(&self, id: &str) -> Result<Vec<Value>> {
        Ok(
            self.journal.clip_detail(id.into()).await?["clip"]["warnings"]
                .as_array()
                .unwrap()
                .clone(),
        )
    }
}

#[tokio::test]
async fn exact_patch_files_mark_both_clips_and_leave_content_versions_alone() -> Result<()> {
    let f = Fixture::new(true).await?;
    fs::write(f.workspaces[0].join("shared.txt"), "A\n")?;
    let a = f.edit(0, "edit-a", "shared.txt", 1, true).await?;
    f.process().await?;
    assert!(f.warnings(&a).await?.is_empty());
    let before = f.window(None).await?;
    let db = rusqlite::Connection::open(f.root.path().join("db"))?;
    let version: i64 = db.query_row(
        "SELECT version FROM timeline_versions WHERE id=?1",
        [&a],
        |r| r.get(0),
    )?;
    fs::write(f.workspaces[1].join("shared.txt"), "B\n")?;
    let b = f.edit(1, "edit-b", "shared.txt", 1, true).await?;
    f.process().await?;
    for id in [&a, &b] {
        let warnings = f.warnings(id).await?;
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["kind"], "same-file");
        assert!(warnings[0]["text"].as_str().unwrap().contains("shared.txt"));
    }
    assert_eq!(
        db.query_row(
            "SELECT version FROM timeline_versions WHERE id=?1",
            [&a],
            |r| r.get::<_, i64>(0)
        )?,
        version
    );
    assert_eq!(db.query_row("SELECT count(*) FROM events WHERE kind='clip.files.compared' AND json_array_length(payload,'$.overlaps')>0",[],|row|row.get::<_,i64>(0))?,1);
    let delta = f.window(before["revision"].as_i64()).await?;
    assert!(
        delta["clips"]
            .as_array()
            .unwrap()
            .iter()
            .any(|clip| clip["id"] == a)
    );
    // An in-flight edit to another file never becomes part of this clip.
    fs::write(f.workspaces[0].join("unrecorded.txt"), "later work\n")?;
    assert!(!serde_json::to_string(&f.warnings(&a).await?)?.contains("unrecorded"));
    Ok(())
}

#[tokio::test]
async fn missing_and_partial_outputs_never_guess_files_and_late_complete_output_is_used()
-> Result<()> {
    let f = Fixture::new(true).await?;
    for index in 0..2 {
        fs::write(f.workspaces[index].join("shared.txt"), "changed\n")?;
    }
    let a = f.edit(0, "a", "shared.txt", 1, true).await?;
    f.process().await?;
    let b = f.edit(1, "b", "shared.txt", 1, false).await?;
    f.process().await?;
    assert!(f.warnings(&a).await?.is_empty());
    let mut partial = f.patch(1, "shared.txt");
    partial["eof"] = false.into();
    f.cache(1, "b", partial).await?;
    f.process().await?;
    assert!(f.warnings(&b).await?.is_empty());
    f.cache(1, "b", f.patch(1, "shared.txt")).await?;
    f.process().await?;
    assert_eq!(f.warnings(&a).await?.len(), 1);
    assert_eq!(f.warnings(&b).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn foreign_repositories_and_already_integrated_files_are_not_overlaps() -> Result<()> {
    let f = Fixture::new(false).await?;
    for index in 0..2 {
        fs::write(f.workspaces[index].join("shared.txt"), "changed\n")?;
        f.edit(index, &format!("foreign-{index}"), "shared.txt", 1, true)
            .await?;
    }
    f.process().await?;
    assert!(
        f.window(None).await?["clips"]
            .as_array()
            .unwrap()
            .iter()
            .all(|clip| clip["warnings"].as_array().unwrap().is_empty())
    );
    let f = Fixture::new(true).await?;
    fs::write(f.workspaces[0].join("shared.txt"), "A\n")?;
    git(&f.workspaces[0], &["add", "."]);
    git(
        &f.workspaces[0],
        &[
            "-c",
            "user.name=Dummy",
            "-c",
            "user.email=dummy@example.invalid",
            "commit",
            "-qm",
            "Saved dummy",
        ],
    );
    let integrated = git(&f.workspaces[0], &["rev-parse", "HEAD"]);
    f.journal.append("integration.published",json!({"status":"done","tracks":[{"id":f.tracks[0],"checkpoint":{"sha":integrated}}],"candidate":{"commits":[{"track":f.tracks[0]}]}})).await?;
    let a = f.edit(0, "integrated", "shared.txt", 1, true).await?;
    f.process().await?;
    fs::write(f.workspaces[1].join("shared.txt"), "B\n")?;
    f.edit(1, "other", "shared.txt", 1, true).await?;
    f.process().await?;
    assert!(f.warnings(&a).await?.is_empty());
    assert_ne!(integrated, f.base);
    Ok(())
}

#[tokio::test]
async fn changed_patch_invalidates_both_marks_and_rejects_an_old_scan() -> Result<()> {
    let f = Fixture::new(true).await?;
    for index in 0..2 {
        fs::write(f.workspaces[index].join("shared.txt"), "changed\n")?;
    }
    let a = f.edit(0, "a", "shared.txt", 1, true).await?;
    f.process().await?;
    let b = f.edit(1, "b", "shared.txt", 1, true).await?;
    let ticket = f.journal.next_clip_files().await?.unwrap();
    let comparison = clip_files::inspect(&ticket);
    f.edit(1, "b", "shared.txt", 2, false).await?;
    assert!(!f.journal.finish_clip_files(ticket, comparison).await?);
    f.process().await?;
    assert_eq!(f.warnings(&a).await?.len(), 1);
    let mut wrong = f.patch(1, "shared.txt");
    wrong["content"] =
        json!({"files":[{"path":"../not-the-workspace","hunks":[{"lines":["+bad"]}]}]})
            .to_string()
            .into();
    wrong["byteLen"] = wrong["content"].as_str().unwrap().len().into();
    f.cache(1, "b", wrong).await?;
    assert!(f.warnings(&a).await?.is_empty());
    assert!(f.warnings(&b).await?.is_empty());
    f.process().await?;
    assert!(f.warnings(&a).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn warning_storage_failure_rolls_back_and_restart_keeps_valid_marks() -> Result<()> {
    let f = Fixture::new(true).await?;
    for index in 0..2 {
        fs::write(f.workspaces[index].join("shared.txt"), "changed\n")?;
    }
    let a = f.edit(0, "a", "shared.txt", 1, true).await?;
    f.process().await?;
    let b = f.edit(1, "b", "shared.txt", 1, true).await?;
    let ticket = f.journal.next_clip_files().await?.unwrap();
    let db = rusqlite::Connection::open(f.root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER fail_file_warning BEFORE INSERT ON clip_file_warnings BEGIN SELECT RAISE(ABORT,'dummy storage full'); END;")?;
    assert!(
        f.journal
            .finish_clip_files(ticket.clone(), clip_files::inspect(&ticket))
            .await
            .is_err()
    );
    assert!(f.warnings(&a).await?.is_empty());
    db.execute_batch("DROP TRIGGER fail_file_warning")?;
    assert!(
        f.journal
            .finish_clip_files(ticket.clone(), clip_files::inspect(&ticket))
            .await?
    );
    let reopened = Journal::open(f.root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        reopened.clip_detail(a).await?["clip"]["warnings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened.clip_detail(b).await?["clip"]["warnings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn symlinked_parent_is_not_followed_and_background_service_stops_cleanly() -> Result<()> {
    let f = Fixture::new(true).await?;
    let external = f.root.path().join("outside");
    fs::create_dir(&external)?;
    fs::write(external.join("shared.txt"), "outside dummy\n")?;
    for index in 0..2 {
        std::os::unix::fs::symlink(&external, f.workspaces[index].join("link"))?;
        f.edit(index, &format!("link-{index}"), "link/shared.txt", 1, true)
            .await?;
    }
    f.process().await?;
    assert!(
        f.window(None).await?["clips"]
            .as_array()
            .unwrap()
            .iter()
            .all(|clip| clip["warnings"].as_array().unwrap().is_empty())
    );
    let db = rusqlite::Connection::open(f.root.path().join("db"))?;
    db.execute("UPDATE clip_file_pending SET retry_at=0", [])?;
    f.process().await?;
    let count: i64 = db.query_row(
        "SELECT count(*) FROM events WHERE kind='clip.files.compared'",
        [],
        |row| row.get(0),
    )?;
    db.execute("UPDATE clip_file_pending SET retry_at=0", [])?;
    f.process().await?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='clip.files.compared'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        count
    );
    for index in 0..2 {
        fs::write(f.workspaces[index].join("shared.txt"), "safe change\n")?;
    }
    let service = clip_files::Service::start(f.journal.clone());
    let a = f.edit(0, "safe-a", "shared.txt", 1, true).await?;
    let b = f.edit(1, "safe-b", "shared.txt", 1, true).await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if f.warnings(&a).await?.len() == 1 && f.warnings(&b).await?.len() == 1 {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await??;
    service.stop().await;
    assert_eq!(
        fs::read_to_string(external.join("shared.txt"))?,
        "outside dummy\n"
    );
    Ok(())
}

#[tokio::test]
async fn line_ending_conversion_is_not_an_uncommitted_change() -> Result<()> {
    let f = Fixture::new(true).await?;
    git(&f.workspaces[0], &["config", "core.autocrlf", "true"]);
    fs::write(f.workspaces[0].join("shared.txt"), "base\r\n")?;
    fs::write(f.workspaces[1].join("shared.txt"), "changed\r\n")?;
    let a = f.edit(0, "a", "shared.txt", 1, true).await?;
    f.process().await?;
    let b = f.edit(1, "b", "shared.txt", 1, true).await?;
    f.process().await?;
    assert!(f.warnings(&a).await?.is_empty());
    assert!(f.warnings(&b).await?.is_empty());
    fs::write(f.workspaces[0].join("shared.txt"), "new\r\n")?;
    let changed = f.edit(0, "changed", "shared.txt", 1, true).await?;
    f.process().await?;
    assert_eq!(f.warnings(&changed).await?.len(), 1);
    Ok(())
}

#[test]
fn a_clean_filter_cannot_execute_during_file_comparison() -> Result<()> {
    let root = tempfile::tempdir()?;
    git(root.path(), &["init", "-q", "--template="]);
    fs::write(root.path().join(".gitattributes"), "*.txt filter=probe\n")?;
    fs::write(root.path().join("value.txt"), "base\n")?;
    git(root.path(), &["add", "."]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=Dummy",
            "-c",
            "user.email=dummy@example.invalid",
            "commit",
            "-qm",
            "Dummy filter baseline",
        ],
    );
    git(
        root.path(),
        &["config", "filter.probe.clean", "touch filter-ran; cat"],
    );
    let base = git(root.path(), &["rev-parse", "HEAD"]);
    let repo = intercat_git::Repository::open(root.path())?;
    assert!(
        repo.normalized_file(base.parse()?, "value.txt", b"changed\n")
            .is_err()
    );
    assert!(!root.path().join("filter-ran").exists());
    Ok(())
}
