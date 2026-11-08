use intercat_daemon::{
    Result, clip_files,
    clock::Stamp,
    journal::Journal,
    preparation::Workspace,
    runtime::WorkerState,
    secrets::Key,
    step_sources::{self, Active, Source},
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
    spaces: Vec<Workspace>,
    sessions: Vec<String>,
}
impl Fixture {
    async fn new(shared: bool) -> Result<Self> {
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
        let db = rusqlite::Connection::open(root.path().join("db"))?;
        let mut spaces = Vec::new();
        let mut sessions = Vec::new();
        for n in 0..2 {
            let path = root.path().join(format!("worker-{n}"));
            if n == 1 && !shared {
                git(
                    root.path(),
                    &[
                        "clone",
                        "-q",
                        repo.to_str().unwrap(),
                        path.to_str().unwrap(),
                    ],
                );
            } else {
                git(
                    &repo,
                    &[
                        "worktree",
                        "add",
                        "--detach",
                        path.to_str().unwrap(),
                        "HEAD",
                    ],
                );
            }
            let track = Uuid::now_v7().to_string();
            let session = Uuid::now_v7().to_string();
            db.execute(
                "INSERT INTO tracks VALUES(?1,?2,'Dummy','waiting')",
                rusqlite::params![track, if n == 0 { "A" } else { "B" }],
            )?;
            let space = Workspace {
                track_id: track.clone(),
                path: path.canonicalize()?,
                reference: format!("refs/intercat/dummy-{n}"),
                base: base.clone(),
                state: "ready".into(),
                error: None,
                attachment_roots: vec![],
            };
            journal.workspace_result(space.clone()).await?;
            journal
                .worker_state(WorkerState {
                    track_id: track,
                    phase: "ready".into(),
                    session_id: Some(session.clone()),
                    secret_revision: None,
                    identity: None,
                    detail: None,
                    active_turn: None,
                    approvals: vec![],
                    output: None,
                    checkpoint_pending: false,
                    checkpoint: None,
                    prompts: vec![],
                })
                .await?;
            spaces.push(space);
            sessions.push(session);
        }
        Ok(Self {
            root,
            journal,
            spaces,
            sessions,
        })
    }
    fn db(&self) -> Result<rusqlite::Connection> {
        Ok(rusqlite::Connection::open(self.root.path().join("db"))?)
    }
    fn path(&self, n: usize) -> &Path {
        &self.spaces[n].path
    }
    async fn begin(&self, n: usize, turn: &str, calls: &[&str]) -> Result<()> {
        let before = intercat_daemon::checkpoint::capture(
            &self.spaces[n],
            &self.spaces[n].base,
            "Dummy before\n",
        )
        .await?
        .sha;
        assert!(
            self.journal
                .begin_step_capture(Active {
                    track: self.spaces[n].track_id.clone(),
                    session: self.sessions[n].clone(),
                    turn: turn.into(),
                    before,
                    calls: calls.iter().map(|s| s.to_string()).collect(),
                    workspace: self.spaces[n].clone()
                })
                .await?
        );
        Ok(())
    }
    async fn item(
        &self,
        n: usize,
        turn: &str,
        call: &str,
        revision: u64,
        status: &str,
        background: bool,
    ) -> Result<String> {
        self.journal.try_msp(self.spaces[n].track_id.clone(),self.sessions[n].clone(),Stamp::now(),json!({"method":if status=="inProgress" {"item/started"} else {"item/completed"},"params":{"sessionId":self.sessions[n],"item":{"itemId":format!("item-{call}"),"callId":call,"turnId":turn,"kind":"toolCall","tool":"bash","revision":revision,"status":status,"background":background,"args":"{\"command\":\"dummy only\"}"}}}),None,None)?.await??;
        Ok(self.db()?.query_row(
            "SELECT clip FROM timeline_items WHERE session=?1 AND item=?2",
            rusqlite::params![self.sessions[n], format!("item-{call}")],
            |r| r.get(0),
        )?)
    }
    async fn source(
        &self,
        n: usize,
        turn: &str,
        calls: &[&str],
        paths: Option<&[&str]>,
    ) -> Result<()> {
        let calls = calls.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(
            self.journal
                .save_step_source(Source {
                    id: step_sources::identity(&self.sessions[n], turn, &calls),
                    track: self.spaces[n].track_id.clone(),
                    session: self.sessions[n].clone(),
                    turn: turn.into(),
                    calls,
                    paths: paths.map(|p| p.iter().map(|s| s.to_string()).collect()),
                    workspace: self.spaces[n].clone(),
                    at: Stamp::now()
                })
                .await?
        );
        Ok(())
    }
    async fn closed(&self, n: usize, call: &str, paths: Option<&[&str]>) -> Result<String> {
        let turn = Uuid::now_v7().to_string();
        self.begin(n, &turn, &[call]).await?;
        let id = self.item(n, &turn, call, 1, "completed", false).await?;
        self.source(n, &turn, &[call], paths).await?;
        Ok(id)
    }
    async fn process(&self) -> Result<()> {
        for _ in 0..40 {
            let Some(ticket) = self.journal.next_clip_files().await? else {
                return Ok(());
            };
            assert!(
                self.journal
                    .finish_clip_files(ticket.clone(), clip_files::inspect(&ticket))
                    .await?
            );
        }
        panic!("file scans did not settle")
    }
    async fn warnings(&self, id: &str) -> Result<Vec<Value>> {
        Ok(
            self.journal.clip_detail(id.into()).await?["clip"]["warnings"]
                .as_array()
                .unwrap()
                .clone(),
        )
    }
    fn ended(&self, id: &str) -> Result<bool> {
        Ok(self.db()?.query_row(
            "SELECT json_extract(record,'$.end') IS NOT NULL FROM timeline_clips WHERE id=?1",
            [id],
            |r| r.get(0),
        )?)
    }
    fn count(&self, sql: &str) -> Result<i64> {
        Ok(self.db()?.query_row(sql, [], |r| r.get(0))?)
    }
}

#[tokio::test]
async fn saved_sources_mark_both_clips_without_changing_content_versions() -> Result<()> {
    let f = Fixture::new(true).await?;
    fs::write(f.path(0).join("shared.txt"), "A\n")?;
    let a = f.closed(0, "a", Some(&["shared.txt"])).await?;
    f.process().await?;
    assert!(f.warnings(&a).await?.is_empty());
    let version: i64 = f.db()?.query_row(
        "SELECT version FROM timeline_versions WHERE id=?1",
        [&a],
        |r| r.get(0),
    )?;
    let before = f
        .journal
        .timeline_window(Window {
            tracks: f.spaces.iter().map(|s| s.track_id.clone()).collect(),
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await?;
    fs::write(f.path(1).join("shared.txt"), "B\n")?;
    let b = f.closed(1, "b", Some(&["shared.txt"])).await?;
    f.process().await?;
    for id in [&a, &b] {
        let w = f.warnings(id).await?;
        assert_eq!(w.len(), 1);
        assert_eq!(w[0]["kind"], "same-file");
        assert!(w[0]["text"].as_str().unwrap().contains("shared.txt"));
    }
    assert_eq!(
        f.db()?.query_row(
            "SELECT version FROM timeline_versions WHERE id=?1",
            [&a],
            |r| r.get::<_, i64>(0)
        )?,
        version
    );
    let delta = f
        .journal
        .timeline_window(Window {
            tracks: f.spaces.iter().map(|s| s.track_id.clone()).collect(),
            start: 0,
            end: i64::MAX,
            since: before["revision"].as_i64(),
        })
        .await?;
    assert!(
        delta["clips"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == a)
    );
    fs::write(f.path(0).join("later.txt"), "Later\n")?;
    assert!(!serde_json::to_string(&f.warnings(&a).await?)?.contains("later.txt"));
    Ok(())
}

#[tokio::test]
async fn parallel_batch_waits_for_all_call_ids_in_either_arrival_order() -> Result<()> {
    let f = Fixture::new(true).await?;
    let turn = Uuid::now_v7().to_string();
    f.begin(0, &turn, &["a", "b"]).await?;
    let id = f.item(0, &turn, "a", 1, "completed", false).await?;
    assert!(!f.ended(&id)?);
    f.source(0, &turn, &["a", "b"], Some(&["empty.txt"]))
        .await?;
    assert!(!f.ended(&id)?);
    assert_eq!(f.item(0, &turn, "b", 1, "inProgress", false).await?, id);
    assert!(!f.ended(&id)?);
    assert_eq!(f.item(0, &turn, "b", 2, "completed", false).await?, id);
    assert!(f.ended(&id)?);
    assert_eq!(f.count("SELECT count(*) FROM step_source_clips")?, 1);
    f.source(0, &turn, &["a", "b"], Some(&["empty.txt"]))
        .await?;
    f.begin(0, &turn, &["c"]).await?;
    let next = f.item(0, &turn, "c", 1, "completed", false).await?;
    assert_ne!(id, next);
    assert!(!f.ended(&next)?);
    f.source(0, &turn, &["c"], Some(&[])).await?;
    assert!(f.ended(&next)?);
    assert_eq!(f.count("SELECT count(*) FROM step_source_clips")?, 2);
    // A background shell is split off at the batch boundary and later completed.
    f.begin(0, &turn, &["d"]).await?;
    let bg = f.item(0, &turn, "d", 1, "inProgress", true).await?;
    f.source(0, &turn, &["d"], Some(&[])).await?;
    assert!(f.ended(&bg)?);
    assert_eq!(f.count("SELECT count(*) FROM timeline_clips WHERE json_extract(record,'$.category')='shell' AND json_extract(record,'$.end') IS NULL")?,1);
    f.item(0, &turn, "d", 2, "completed", false).await?;
    assert_eq!(f.count("SELECT count(*) FROM timeline_clips WHERE json_extract(record,'$.category')='shell' AND json_extract(record,'$.end') IS NULL")?,0);
    Ok(())
}

#[tokio::test]
async fn unavailable_sources_never_guess_from_a_later_tree_or_patch() -> Result<()> {
    let f = Fixture::new(true).await?;
    fs::write(f.path(0).join("shared.txt"), "A\n")?;
    let a = f.closed(0, "a", None).await?;
    f.process().await?;
    assert_eq!(
        f.warnings(&a).await?[0]["kind"],
        "file-comparison-unavailable"
    );
    let before = f.count("SELECT count(*) FROM events WHERE kind='clip.files.compared'")?;
    f.journal
        .cache_output(
            f.sessions[0].clone(),
            "item-a".into(),
            "fake-patch".into(),
            json!({"files":[{"path":"shared.txt","hunks":[{"lines":["+fake"]}]}]}),
        )
        .await?;
    f.process().await?;
    assert_eq!(
        before,
        f.count("SELECT count(*) FROM events WHERE kind='clip.files.compared'")?
    );
    let empty = f.closed(1, "empty", Some(&[])).await?;
    f.process().await?;
    assert!(f.warnings(&empty).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn active_and_clean_peers_are_compared_and_later_completion_keeps_the_address() -> Result<()>
{
    let f = Fixture::new(true).await?;
    let turn = Uuid::now_v7().to_string();
    f.begin(1, &turn, &["active"]).await?;
    let b = f.item(1, &turn, "active", 1, "inProgress", false).await?;
    fs::write(f.path(0).join("shared.txt"), "A\n")?;
    let a = f.closed(0, "a", Some(&["shared.txt"])).await?;
    f.process().await?;
    assert!(f.warnings(&a).await?.is_empty());
    fs::write(f.path(1).join("shared.txt"), "B\n")?;
    // A's next closed step checks B while B is still running.
    let a2 = f.closed(0, "a2", Some(&["shared.txt"])).await?;
    f.process().await?;
    assert_eq!(f.warnings(&a2).await?.len(), 1);
    assert_eq!(f.warnings(&b).await?.len(), 1);
    assert!(f.warnings(&a).await?.is_empty());
    f.item(1, &turn, "active", 2, "completed", false).await?;
    f.source(1, &turn, &["active"], Some(&["shared.txt"]))
        .await?;
    f.process().await?;
    assert_eq!(f.warnings(&a2).await?.len(), 1);
    assert_eq!(f.warnings(&b).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn new_boundary_and_commit_invalidate_old_comparisons() -> Result<()> {
    let f = Fixture::new(true).await?;
    for n in 0..2 {
        fs::write(f.path(n).join("shared.txt"), "changed\n")?;
    }
    let a = f.closed(0, "a", Some(&["shared.txt"])).await?;
    let b = f.closed(1, "b", Some(&["shared.txt"])).await?;
    let old = f.journal.next_clip_files().await?.unwrap();
    let result = clip_files::inspect(&old);
    let c = f.closed(1, "c", Some(&["shared.txt"])).await?;
    assert!(!f.journal.finish_clip_files(old, result).await?);
    f.process().await?;
    assert!(f.warnings(&b).await?.is_empty());
    assert_eq!(f.warnings(&c).await?.len(), 1);
    assert_eq!(f.warnings(&a).await?.len(), 1);
    git(f.path(0), &["add", "."]);
    git(
        f.path(0),
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
    let integrated = git(f.path(0), &["rev-parse", "HEAD"]);
    f.journal.append("integration.published",json!({"status":"done","tracks":[{"id":f.spaces[0].track_id,"checkpoint":{"sha":integrated}}],"candidate":{"commits":[{"track":f.spaces[0].track_id}]}})).await?;
    f.process().await?;
    for id in [&a, &c] {
        assert!(f.warnings(id).await?.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn a_late_source_uses_clip_order_instead_of_record_arrival_order() -> Result<()> {
    let f = Fixture::new(true).await?;
    fs::write(f.path(0).join("shared.txt"), "A\n")?;
    let a = f.closed(0, "a", Some(&["shared.txt"])).await?;
    let turn = Uuid::now_v7().to_string();
    f.begin(1, &turn, &["old"]).await?;
    fs::write(f.path(1).join("shared.txt"), "B first\n")?;
    let old = f.item(1, &turn, "old", 1, "completed", false).await?;
    f.journal
        .try_msp(
            f.spaces[1].track_id.clone(),
            f.sessions[1].clone(),
            Stamp::now(),
            json!({"method":"turn/completed","params":{"sessionId":f.sessions[1],"turnId":turn}}),
            None,
            None,
        )?
        .await??;
    fs::write(f.path(1).join("shared.txt"), "B second\n")?;
    let current = f.closed(1, "current", Some(&["shared.txt"])).await?;
    f.source(1, &turn, &["old"], Some(&["shared.txt"])).await?;
    f.process().await?;
    assert!(f.warnings(&old).await?.is_empty());
    assert_eq!(f.warnings(&current).await?.len(), 1);
    let peer: Option<String> = f.db()?.query_row(
        "SELECT peer_clip FROM clip_file_warnings WHERE clip=?1",
        [a],
        |r| r.get(0),
    )?;
    assert_eq!(peer, Some(current));
    Ok(())
}

#[tokio::test]
async fn storage_failure_rolls_back_and_restart_keeps_sources_and_marks() -> Result<()> {
    let f = Fixture::new(true).await?;
    fs::write(f.path(0).join("shared.txt"), "changed\n")?;
    let a = f.closed(0, "a", Some(&["shared.txt"])).await?;
    f.process().await?;
    fs::write(f.path(1).join("shared.txt"), "changed\n")?;
    let b = f.closed(1, "b", Some(&["shared.txt"])).await?;
    let ticket = f.journal.next_clip_files().await?.unwrap();
    let db = f.db()?;
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
    f.process().await?;
    let reopened = Journal::open(f.root.path().join("db"), Key::new([7; 32])).await?;
    for id in [&a, &b] {
        assert_eq!(
            reopened.clip_detail(id.clone()).await?["clip"]["warnings"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    assert_eq!(f.count("SELECT count(*) FROM step_source_clips")?, 2);
    Ok(())
}

#[tokio::test]
async fn foreign_repositories_and_line_endings_do_not_create_false_overlaps() -> Result<()> {
    let f = Fixture::new(false).await?;
    for n in 0..2 {
        fs::write(f.path(n).join("shared.txt"), "changed\n")?;
    }
    let a = f.closed(0, "a", Some(&["shared.txt"])).await?;
    let b = f.closed(1, "b", Some(&["shared.txt"])).await?;
    f.process().await?;
    assert!(f.warnings(&a).await?.is_empty());
    assert!(f.warnings(&b).await?.is_empty());
    let f = Fixture::new(true).await?;
    git(f.path(0), &["config", "core.autocrlf", "true"]);
    fs::write(f.path(0).join("shared.txt"), "base\r\n")?;
    fs::write(f.path(1).join("shared.txt"), "B\n")?;
    let a = f.closed(0, "a", Some(&["shared.txt"])).await?;
    f.closed(1, "b", Some(&["shared.txt"])).await?;
    f.process().await?;
    assert!(f.warnings(&a).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn unsafe_paths_show_unavailable_and_service_stops_without_reading_outside() -> Result<()> {
    let f = Fixture::new(true).await?;
    let outside = f.root.path().join("outside");
    fs::create_dir(&outside)?;
    fs::write(outside.join("shared.txt"), "dummy outside\n")?;
    for n in 0..2 {
        std::os::unix::fs::symlink(&outside, f.path(n).join("link"))?;
    }
    let a = f.closed(0, "a", Some(&["link/shared.txt"])).await?;
    f.process().await?;
    assert_eq!(
        f.warnings(&a).await?[0]["kind"],
        "file-comparison-unavailable"
    );
    f.db()?
        .execute("UPDATE clip_file_pending SET retry_at=0", [])?;
    f.process().await?;
    let count = f.count("SELECT count(*) FROM events WHERE kind='clip.files.compared'")?;
    f.db()?
        .execute("UPDATE clip_file_pending SET retry_at=0", [])?;
    f.process().await?;
    assert_eq!(
        count,
        f.count("SELECT count(*) FROM events WHERE kind='clip.files.compared'")?
    );
    let service = clip_files::Service::start(f.journal.clone());
    service.stop().await;
    assert_eq!(
        fs::read_to_string(outside.join("shared.txt"))?,
        "dummy outside\n"
    );
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

#[tokio::test]
#[ignore = "two paid dummy Muse turns; verifies warnings while the peer is running"]
async fn native_workers_warn_on_the_exact_file_while_the_peer_is_running() -> Result<()> {
    use intercat_daemon::{
        host::{Boundary, Host, Profile},
        msp::Event,
        step_capture::Service,
    };
    let mut f = Fixture::new(true).await?;
    let binary = PathBuf::from(std::env::var("INTERCAT_MUSE_BINARY")?);
    let auth = PathBuf::from(std::env::var("INTERCAT_MUSE_AUTH")?);
    let output = PathBuf::from(std::env::var("INTERCAT_STEP_OUTPUT")?);
    fs::create_dir_all(&output)?;
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    let mut hosts = Vec::new();
    for n in 0..2 {
        let capture = Service::start(f.journal.clone(), f.spaces[n].track_id.clone(), f.path(n))?;
        let host = Host::spawn(
            &binary,
            &f.root.path().join(format!("host-{n}")),
            f.path(n),
            std::ffi::OsStr::new(&std::env::var("PATH")?),
            Some(&auth),
            Boundary {
                profile: Profile::Worker,
                secrets: &[],
                rpc: &rpc,
                denied: &[],
                steps: Some(capture),
            },
        )
        .await?;
        host.connection.client.initialize().await?;
        let opened=host.connection.client.request("session/start",json!({"commandId":Uuid::now_v7(),"workspaceRoot":f.path(n),"providerId":"meta","approvalMode":"promptUnmatched"})).await?;
        f.sessions[n] = opened["session"]["sessionId"].as_str().unwrap().into();
        f.journal
            .worker_state(WorkerState {
                track_id: f.spaces[n].track_id.clone(),
                phase: "ready".into(),
                session_id: Some(f.sessions[n].clone()),
                secret_revision: None,
                identity: Some(host.identity),
                detail: None,
                active_turn: None,
                approvals: vec![],
                output: None,
                checkpoint_pending: false,
                checkpoint: None,
                prompts: vec![],
            })
            .await?;
        hosts.push(host);
    }
    let commands = [
        "printf A > shared.txt; : > empty.txt",
        "printf B > shared.txt; /bin/sleep 30",
    ];
    let result=async {
        let mut started=[false,false];let mut ended=[false,false];let mut decided=std::collections::BTreeSet::new();
        let start=|n:usize|f.journal.screen_start(json!({"commandId":Uuid::now_v7(),"sessionId":f.sessions[n],"reasoningEffort":"minimal","input":[{"type":"text","text":format!("Disposable test. Run exactly one bash call with exactly this command: {}. Do not use any other tools. After it finishes reply DONE.",commands[n])}]}));
        let b=start(1).await?;
        hosts[1].connection.client.start(b,intercat_daemon::msp::StartGuard::Stopped(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))).await?;
        started[1]=true;
        let mut interval=tokio::time::interval(std::time::Duration::from_millis(100));
        let mut active_warning=false;
        tokio::time::timeout(std::time::Duration::from_secs(110),async {
            while !ended.iter().all(|done|*done) {
                let (left,right)=hosts.split_at_mut(1);
                let observed=tokio::select! {
                    e=left[0].connection.next_event()=>Some((0,e)),
                    e=right[0].connection.next_event()=>Some((1,e)),
                    _=interval.tick()=>None,
                };
                if let Some((n,event))=observed {
                    let frame=match event {Some(Event::Frame(value))=>value.frame,Some(Event::Closed(reason))=>return Err(reason.into()),None=>return Err("Native host closed.".into())};
                    let p=&frame["params"];
                    if p["sessionId"]!=f.sessions[n] {continue}
                    f.journal.try_msp(f.spaces[n].track_id.clone(),f.sessions[n].clone(),Stamp::now(),frame.clone(),None,None)?.await??;
                    if matches!(frame["method"].as_str(),Some("approval/request"|"approval/requested")) {
                        let key=(n,p["approvalId"].to_string(),p["currentRequirementId"].to_string());
                        if decided.insert(key) {
                            let allowed=p["subject"]["kind"]=="shell" && p["subject"]["workspaceRoot"]==f.path(n).to_string_lossy().as_ref() && p["subject"]["command"]==commands[n];
                            let choice=p["availableChoices"].as_array().unwrap().iter().find(|c|c["scope"]=="once" && c["decision"]==if allowed {"approved"}else{"abort"}).unwrap();
                            hosts[n].connection.client.request("approval/decide",json!({"commandId":Uuid::now_v7(),"sessionId":f.sessions[n],"approvalId":p["approvalId"],"requirementId":p["currentRequirementId"],"choiceId":choice["choiceId"]})).await?;
                        }
                    }
                    if frame["method"]=="turn/completed" {ended[n]=true;}
                }
                if !started[0] && fs::read_to_string(f.path(1).join("shared.txt"))?=="B" {
                    let a=start(0).await?;
                    hosts[0].connection.client.start(a,intercat_daemon::msp::StartGuard::Stopped(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))).await?;started[0]=true;
                }
                f.process().await?;
                if started[0] && !ended[1] {
                    let count:i64=f.db()?.query_row("SELECT count(*) FROM clip_file_warnings WHERE peer_track=?1",[&f.spaces[1].track_id],|r|r.get(0))?;
                    active_warning |= count>0;
                }
            }
            Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
        }).await??;
        f.process().await?;
        assert!(active_warning,"No warning was observed while B was running.");
        assert_eq!(f.count("SELECT count(*) FROM step_source_clips")?,2);
        let rows=f.db()?.prepare("SELECT p.path,b.clip FROM step_source_paths p JOIN step_source_clips b ON b.source=p.source ORDER BY p.path")?
            .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(rows.iter().filter(|(path,_)|path=="shared.txt").count(),2);
        for (_,clip) in rows.iter().filter(|(path,_)|path=="shared.txt") {let warnings=f.warnings(clip).await?;assert_eq!(warnings.len(),1);assert_eq!(warnings[0]["kind"],"same-file");}
        fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"schema":intercat_daemon::journal::SCHEMA_VERSION,"paidTurns":2,"sources":2,"activePeerWarning":active_warning,"paths":rows.iter().map(|(p,_)|p).collect::<Vec<_>>()}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    while let Some(host) = hosts.pop() {
        host.shutdown().await?;
    }
    result
}
