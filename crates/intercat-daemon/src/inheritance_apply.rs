//! Apply dependency updates at the worker's idle boundary, before turn/start.
use crate::{
    Result,
    checkpoint::Checkpoint,
    inheritance_apply_store::{Application, Change},
    journal::Journal,
    preparation::Workspace,
};
use intercat_git::{Merge, Repository};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub(crate) struct Boundary {
    pub guard: Guard,
    pub tracker: Arc<Mutex<crate::process::Tracker>>,
    pub identity: crate::process::Identity,
}
pub(crate) enum Guard {
    Work(crate::msp::StartGuard),
    Recovery {
        generation: Arc<std::sync::atomic::AtomicU64>,
        expected: u64,
        gate: Arc<crate::dispatch_control::Gate>,
        epoch: u64,
    },
}
impl Guard {
    pub(crate) fn current(&self) -> bool {
        match self {
            Self::Work(guard) => guard.current(),
            Self::Recovery {
                generation,
                expected,
                gate,
                epoch,
            } => {
                generation.load(std::sync::atomic::Ordering::Acquire) == *expected
                    && gate.epoch() == *epoch
            }
        }
    }
}
impl Boundary {
    async fn idle(&self) -> Result<()> {
        if !self.guard.current() {
            return Err(crate::journal::StartSuperseded.into());
        }
        self.scan().await
    }
    async fn scan(&self) -> Result<()> {
        let tracker = self.tracker.clone();
        let scan = tokio::task::spawn_blocking(move || tracker.lock().unwrap().refresh()).await??;
        if scan
            .processes
            .iter()
            .any(|process| process.identity != self.identity)
            || scan.groups.iter().any(|group| *group != self.identity)
        {
            return Err(crate::inheritance_issue::Blocker::Processes.into());
        }
        Ok(())
    }
}
pub(crate) async fn stop_children(
    tracker: Arc<Mutex<crate::process::Tracker>>,
    host: crate::process::Identity,
    guard: Guard,
) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        for (force, seconds) in [(false, 5), (true, 1)] {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
            loop {
                if !guard.current() {
                    return Err(crate::journal::StartSuperseded.into());
                }
                {
                    let mut tracker = tracker.lock().unwrap();
                    let scan = tracker.refresh()?;
                    if !scan
                        .processes
                        .iter()
                        .any(|process| process.identity != host)
                        && !scan.groups.iter().any(|group| *group != host)
                    {
                        return Ok(());
                    }
                    for process in scan
                        .processes
                        .into_iter()
                        .filter(|process| process.identity != host)
                    {
                        if !guard.current() {
                            return Err(crate::journal::StartSuperseded.into());
                        }
                        tracker.terminate(process.identity, force)?;
                    }
                }
                if std::time::Instant::now() >= end {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        Err(crate::inheritance_issue::Blocker::Processes.into())
    })
    .await?
}
pub(crate) struct Applied {
    pub operation: Uuid,
    pub checkpoint: Checkpoint,
}

pub(crate) fn merge_update(
    repo: &Repository,
    source: &crate::inheritance::Source,
    before: &str,
    basis: &crate::lineage::Basis,
) -> Result<(String, crate::lineage::Basis)> {
    let mut inherited = basis.inherits.clone();
    for point in &source.dependencies.points {
        source.graph.incorporate(&mut inherited, point)?;
    }
    let recipe = crate::lineage::Basis {
        base: before.into(),
        base_inherits: basis.inherits.clone(),
        inherits: inherited.clone(),
    };
    let tree = match source.graph.baseline(repo, &recipe, &inherited)? {
        Merge::Clean(tree) => tree,
        Merge::Conflicts(paths) => {
            return Err(crate::inheritance_issue::Blocker::Conflict {
                paths: paths.iter().map(ToString::to_string).collect(),
                base: false,
            }
            .into());
        }
    };
    let mut next = basis.clone();
    for point in &source.dependencies.points {
        let Some(old) = basis.inherits.get(&point.track) else {
            // First inheritance brings only the parent's contribution. Its
            // unrelated base must not replace the child's existing base.
            continue;
        };
        if point.number <= old.number {
            continue;
        }
        let previous = source.graph.point(&point.track, old.number)?;
        if previous.sha != old.sha {
            return Err("The inherited checkpoint identity changed.".into());
        }
        let tree = match repo.merge_trees(
            repo.tree_id(previous.base_at.parse()?)?,
            repo.tree_id(next.base.parse()?)?,
            repo.tree_id(point.base_at.parse()?)?,
        )? {
            Merge::Clean(tree) => tree,
            Merge::Conflicts(paths) => {
                return Err(crate::inheritance_issue::Blocker::Conflict {
                    paths: paths.iter().map(ToString::to_string).collect(),
                    base: true,
                }
                .into());
            }
        };
        if tree != repo.tree_id(next.base.parse()?)? {
            next.base = repo
                .checkpoint_commit(tree, next.base.parse()?, "Advance inherited base")?
                .to_string();
        }
        for (track, mark) in &point.base_inherits_at {
            crate::lineage::include(&mut next.base_inherits, track, mark)?;
        }
    }
    next.inherits = inherited;
    let after = repo.checkpoint_commit(tree, before.parse()?, "Apply inherited files")?;
    repo.sync_objects(&[after, next.base.parse()?])?;
    Ok((after.to_string(), next))
}

pub(crate) async fn prepare(
    journal: &Journal,
    input: String,
    revision: i64,
    session: String,
    workspace: Workspace,
    previous: Option<Checkpoint>,
    boundary: Boundary,
) -> Result<Option<Applied>> {
    let Some(context) = journal.inheritance_application(input.clone()).await? else {
        return Ok(None);
    };
    boundary.idle().await?;
    let (prefix, label) = workspace
        .reference
        .rsplit_once("/tracks/")
        .ok_or("Invalid worker reference.")?;
    let base_reference = format!("{prefix}/base/{label}");
    let expected_session = session.clone();
    let app = if let Some(saved) = context.saved {
        saved
    } else {
        let parent = previous
            .as_ref()
            .map(|p| p.sha.as_str())
            .unwrap_or(&workspace.base);
        let before =
            crate::checkpoint::capture(&workspace, parent, "Before inherited files").await?;
        if !before.unmerged_paths.is_empty() {
            return Err(crate::inheritance_issue::Blocker::Conflict {
                paths: before.unmerged_paths,
                base: false,
            }
            .into());
        }
        let root = workspace.path.clone();
        let before_sha = before.sha.clone();
        let basis = context.basis.clone();
        let dependencies = context.source.dependencies.clone();
        let base_for_merge = base_reference.clone();
        let (after, basis, expected_base) = tokio::task::spawn_blocking(move || -> Result<_> {
            let repo = Repository::open(root)?;
            let expected_base = repo
                .reference_target(&base_for_merge)?
                .map(|sha| sha.to_string());
            if expected_base.as_ref().is_some_and(|sha| sha != &basis.base) {
                return Err("The worker base changed before inheritance.".into());
            }
            let (after, basis) = if let Some(origin) = &context.origin {
                crate::plan_files::recompose(&repo, origin, &context.source, &before_sha, &basis)?
            } else {
                merge_update(&repo, &context.source, &before_sha, &basis)?
            };
            Ok((after, basis, expected_base))
        })
        .await??;
        let app = Application {
            operation: Uuid::new_v5(&input.parse()?, b"apply-inherited-files"),
            dependencies,
            revision,
            request_digest: context.request_digest.clone(),
            session,
            workspace: workspace.clone(),
            previous,
            before: before.sha,
            after,
            before_basis: context.basis,
            basis,
            checkpoint_turn: Uuid::new_v5(&input.parse()?, b"inherited-checkpoint"),
            expected_base,
        };
        journal
            .change_inheritance(Change::Begin(Box::new(app.clone())))
            .await?;
        app
    };
    if app.workspace.path != workspace.path
        || app.dependencies.input != input
        || app.revision > revision
        || app.session != expected_session
    {
        return Err("The inherited file operation belongs to another workspace.".into());
    }
    if app.request_digest != context.request_digest {
        return Err("The request changed after inherited files were prepared. Review the request before continuing.".into());
    }
    finish_files(journal, app, workspace, boundary)
        .await
        .map(Some)
}

pub(crate) async fn recover(
    journal: &Journal,
    request: Uuid,
    operation: Uuid,
    workspace: Workspace,
    session: String,
    boundary: Boundary,
) -> Result<Option<Applied>> {
    let app = journal
        .inheritance_recovery(operation, workspace.track_id.clone(), session)
        .await?;
    journal
        .intent(
            &request.to_string(),
            "inheritance.finish",
            serde_json::json!({"applicationId":operation,"trackId":workspace.track_id}),
        )
        .await?;
    let receipt = journal
        .operation(request)
        .await?
        .ok_or("Missing file recovery receipt.")?;
    if receipt["state"] == "prepared" {
        journal.finish(&request.to_string(), "dispatched").await?;
    }
    let Some(app) = app else {
        if receipt["state"] != "complete" {
            journal.finish(&request.to_string(), "complete").await?;
        }
        return Ok(None);
    };
    if app.workspace.path != workspace.path || app.workspace.reference != workspace.reference {
        return Err("The worker workspace changed before recovery.".into());
    }
    boundary.idle().await?;
    // Finishing saved file work does not authorize any pending request.
    journal
        .change_inheritance(Change::HoldSource(operation))
        .await?;
    journal
        .hold_inputs(
            Uuid::new_v5(&request, b"hold-file-recovery"),
            workspace.track_id.clone(),
        )
        .await?;
    let result = finish_files(journal, app, workspace, boundary).await?;
    if receipt["state"] != "complete" {
        journal.finish(&request.to_string(), "complete").await?;
    }
    Ok(Some(result))
}

async fn finish_files(
    journal: &Journal,
    app: Application,
    workspace: Workspace,
    boundary: Boundary,
) -> Result<Applied> {
    let (prefix, label) = workspace
        .reference
        .rsplit_once("/tracks/")
        .ok_or("Invalid worker reference.")?;
    let base_reference = format!("{prefix}/base/{label}");
    boundary.idle().await?;
    let path = workspace.path.clone();
    let copy = app.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(path)?;
        for (label, sha) in [
            ("before", &copy.before),
            ("after", &copy.after),
            ("base", &copy.basis.base),
        ] {
            let reference = format!(
                "{}-inheritance/{}/{label}",
                copy.workspace.reference, copy.operation
            );
            repo.compare_and_swap(&reference, None, sha.parse()?)?;
            repo.sync_ref(&reference)?;
        }
        Ok(())
    })
    .await??;
    let checkpoint_id = Uuid::new_v5(&app.checkpoint_turn, b"checkpoint");
    if journal.operation(checkpoint_id).await?.is_none() {
        boundary.idle().await?;
        let operation = journal
            .operation(app.operation)
            .await?
            .ok_or("Missing inherited file operation.")?;
        if operation["state"] == "prepared" {
            let observed =
                crate::checkpoint::capture(&workspace, &app.before, "Verify inheritance boundary")
                    .await?;
            let path = workspace.path.clone();
            let expected = app.before.clone();
            let actual = observed.sha;
            if !tokio::task::spawn_blocking(move || -> Result<bool> {
                let repo = Repository::open(path)?;
                Ok(repo.tree_id(expected.parse()?)? == repo.tree_id(actual.parse()?)?)
            })
            .await??
            {
                return Err(
                    "Working files changed before inheritance. Your edits were kept.".into(),
                );
            }
            journal
                .finish(&app.operation.to_string(), "dispatched")
                .await?;
        }
        boundary.idle().await?;
        let path = workspace.path.clone();
        let before = app.before.parse()?;
        let after = app.after.parse()?;
        // Once writing begins, finish this bounded file operation. Stop still
        // invalidates turn/start, and a crash resumes only known before/after paths.
        tokio::task::spawn_blocking(move || {
            let repo = Repository::open(&path)?;
            crate::tree_transition::apply(&path, repo.tree_id(before)?, repo.tree_id(after)?)
        })
        .await??;
        boundary.scan().await?;
        let parent = app
            .previous
            .as_ref()
            .map(|p| p.sha.as_str())
            .unwrap_or(&workspace.base);
        let saved =
            crate::checkpoint::capture(&workspace, parent, "Inherited files checkpoint").await?;
        let path = workspace.path.clone();
        let before = app.before.clone();
        let after = app.after.clone();
        let actual = saved.sha.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let repo = Repository::open(path)?;
            crate::tree_transition::verify(
                &repo,
                repo.tree_id(before.parse()?)?,
                repo.tree_id(after.parse()?)?,
                repo.tree_id(actual.parse()?)?,
            )
        })
        .await??;
        let checkpoint = Checkpoint {
            turn_id: app.checkpoint_turn,
            sha: saved.sha,
            reference: format!(
                "{}-checkpoints/{}",
                workspace.reference, app.checkpoint_turn
            ),
            unmerged_paths: saved.unmerged_paths,
            live_processes: false,
        };
        journal
            .change_inheritance(Change::Checkpoint {
                operation: app.operation,
                checkpoint,
            })
            .await?;
    }
    let checkpoint = crate::checkpoint::save(
        workspace.clone(),
        app.checkpoint_turn,
        app.previous.clone(),
        false,
        journal.clone(),
    )
    .await?;
    let path = workspace.path;
    let expected = app
        .expected_base
        .as_ref()
        .map(|sha| sha.parse())
        .transpose()?;
    let base = app.basis.base.parse()?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(path)?;
        repo.compare_and_swap(&base_reference, expected, base)?;
        repo.sync_ref(&base_reference)?;
        Ok(())
    })
    .await??;
    Ok(Applied {
        operation: app.operation,
        checkpoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        decision::{Decision, Kind, Outcome, Target},
        runtime::WorkerState,
        secrets::Key,
    };
    #[tokio::test]
    async fn stopping_inherited_blockers_keeps_the_host_and_unrelated_processes_alive() -> Result<()>
    {
        use tokio::io::AsyncBufReadExt;
        let mut host=tokio::process::Command::new("/usr/bin/python3").args(["-c","import subprocess,time\np=subprocess.Popen(['/bin/sleep','30'])\nprint(p.pid,flush=True)\ntime.sleep(30)\n"]).stdout(std::process::Stdio::piped()).kill_on_drop(true).spawn()?;
        let host_identity = crate::process::identity(host.id().ok_or("Missing dummy host")?)
            .map_err(|error| format!("Identify dummy host: {error}"))?;
        let tracker = Arc::new(Mutex::new(crate::process::Tracker::new(
            Uuid::now_v7().to_string(),
            host_identity,
        )?));
        let mut outside = tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()?;
        let outside_identity = crate::process::identity(outside.id().ok_or("Missing dummy peer")?)?;
        let result = async {
            let mut line = String::new();
            tokio::io::BufReader::new(host.stdout.take().ok_or("Missing host output")?)
                .read_line(&mut line)
                .await?;
            let child = crate::process::identity(line.trim().parse()?)
                .map_err(|error| format!("Identify dummy child: {error}"))?;
            let generation = Arc::new(std::sync::atomic::AtomicU64::new(1));
            let gate = Arc::new(crate::dispatch_control::Gate::new(false));
            let cancelled = Guard::Recovery {
                generation: generation.clone(),
                expected: 0,
                gate: gate.clone(),
                epoch: gate.epoch(),
            };
            if stop_children(tracker.clone(), host_identity, cancelled)
                .await
                .is_ok()
                || !crate::process::is_alive(child)
            {
                return Err("A cancelled process stop changed the dummy child.".into());
            }
            let allowed = Guard::Recovery {
                generation,
                expected: 1,
                gate: gate.clone(),
                epoch: gate.epoch(),
            };
            stop_children(tracker.clone(), host_identity, allowed)
                .await
                .map_err(|error| format!("Stop dummy child: {error}"))?;
            if crate::process::is_alive(child)
                || !crate::process::is_alive(host_identity)
                || !crate::process::is_alive(outside_identity)
            {
                return Err(
                    "Process stop did not preserve the host and unrelated dummy process.".into(),
                );
            }
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        }
        .await;
        // The test owns the host handle. Stop it directly before asking the
        // tracker to confirm cleanup; the feature itself deliberately keeps it.
        let _ = host.kill().await;
        let _ = host.wait().await;
        let cleanup = tracker.clone();
        let cleaned =
            tokio::task::spawn_blocking(move || cleanup.lock().unwrap().stop_confirmed()).await?;
        let _ = outside.kill().await;
        let _ = outside.wait().await;
        cleaned.map_err(|error| format!("Clean up dummy host: {error}"))?;
        result
    }
    fn commit(
        repo: &Repository,
        parent: intercat_git::ObjectId,
        files: &[(&str, &str)],
    ) -> Result<intercat_git::ObjectId> {
        let entries = files
            .iter()
            .map(|(path, text)| Ok((*path, repo.write_blob(text.as_bytes())?)))
            .collect::<Result<Vec<_>>>()?;
        repo.checkpoint_commit(repo.write_tree(&entries)?, parent, "Dummy saved tree")
    }
    fn source(
        points: Vec<crate::lineage::Point>,
        latest: crate::lineage::Point,
    ) -> crate::inheritance::Source {
        crate::inheritance::Source {
            dependencies: crate::inheritance::Dependencies {
                input: Uuid::now_v7().to_string(),
                track: "child".into(),
                points: vec![latest],
            },
            graph: crate::lineage::Frozen {
                points,
                integrated: Default::default(),
            },
        }
    }
    #[test]
    fn first_inheritance_excludes_the_parents_unrelated_base() -> Result<()> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        gix::init(root.path())?;
        let repo = Repository::open(root.path())?;
        let empty = repo.empty_starting_point()?;
        let parent_base = commit(&repo, empty, &[("deleted-elsewhere", "old")])?;
        let parent = commit(
            &repo,
            parent_base,
            &[("deleted-elsewhere", "old"), ("a", "parent")],
        )?;
        let before = commit(&repo, empty, &[("c", "child")])?;
        let point = crate::lineage::Point {
            track: "A".into(),
            number: 1,
            sha: parent.to_string(),
            base_at: parent_base.to_string(),
            base_inherits_at: Default::default(),
            inherits: Default::default(),
        };
        let (after, basis) = merge_update(
            &repo,
            &source(vec![point.clone()], point),
            &before.to_string(),
            &crate::lineage::Basis::root(empty.to_string()),
        )?;
        let files = repo.saved_files(after.parse()?)?;
        assert!(files.contains_key("a") && files.contains_key("c"));
        assert!(!files.contains_key("deleted-elsewhere"));
        assert_eq!(basis.base, empty.to_string());
        Ok(())
    }
    #[test]
    fn an_updated_parent_base_is_recorded_so_integration_keeps_a_later_deletion() -> Result<()> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        gix::init(root.path())?;
        let repo = Repository::open(root.path())?;
        let empty = repo.empty_starting_point()?;
        let first = commit(&repo, empty, &[("a", "parent")])?;
        let old = crate::lineage::Point {
            track: "A".into(),
            number: 1,
            sha: first.to_string(),
            base_at: empty.to_string(),
            base_inherits_at: Default::default(),
            inherits: Default::default(),
        };
        let before = commit(&repo, first, &[("a", "parent"), ("c", "child")])?;
        let moved_base = commit(&repo, empty, &[("x", "other track")])?;
        let updated = commit(
            &repo,
            moved_base,
            &[("a", "parent"), ("g", "update"), ("x", "other track")],
        )?;
        let next = crate::lineage::Point {
            track: "A".into(),
            number: 2,
            sha: updated.to_string(),
            base_at: moved_base.to_string(),
            base_inherits_at: Default::default(),
            inherits: Default::default(),
        };
        let mut basis = crate::lineage::Basis::root(empty.to_string());
        basis.inherits.insert("A".into(), old.inherited());
        let mut source = source(vec![old, next.clone()], next.clone());
        let (after, basis) = merge_update(&repo, &source, &before.to_string(), &basis)?;
        assert_eq!(
            repo.tree_id(basis.base.parse()?)?,
            repo.tree_id(moved_base)?
        );
        assert_eq!(basis.inherits["A"], next.inherited());
        let child = crate::lineage::Point {
            track: "child".into(),
            number: 1,
            sha: after.clone(),
            base_at: basis.base.clone(),
            base_inherits_at: basis.base_inherits.clone(),
            inherits: basis.inherits.clone(),
        };
        source.graph.points.push(child.clone());
        source.graph.integrated.insert("A".into(), next.inherited());
        let base =
            match source
                .graph
                .track_baseline(&repo, &child, &basis, &source.graph.integrated)?
            {
                Merge::Clean(tree) => tree,
                _ => return Err("Unexpected baseline conflict".into()),
            };
        let deleted = commit(&repo, updated, &[("a", "parent"), ("g", "update")])?;
        let merged =
            match repo.merge_trees(base, repo.tree_id(deleted)?, repo.tree_id(after.parse()?)?)? {
                Merge::Clean(tree) => tree,
                _ => return Err("Unexpected integration conflict".into()),
            };
        let files = repo.tree_files(merged)?;
        assert!(files.contains_key("a") && files.contains_key("g") && files.contains_key("c"));
        assert!(!files.contains_key("x"));
        Ok(())
    }
    #[test]
    fn incompatible_base_movements_refuse_the_update() -> Result<()> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        gix::init(root.path())?;
        let repo = Repository::open(root.path())?;
        let empty = repo.empty_starting_point()?;
        let original = commit(&repo, empty, &[("x", "original\n")])?;
        let mine = commit(&repo, original, &[("x", "mine\n")])?;
        let theirs = commit(&repo, original, &[("x", "theirs\n")])?;
        let first = commit(&repo, original, &[("x", "original\n"), ("a", "parent")])?;
        let updated = commit(&repo, theirs, &[("x", "theirs\n"), ("a", "parent")])?;
        let before = commit(
            &repo,
            mine,
            &[("x", "theirs\n"), ("a", "parent"), ("c", "child")],
        )?;
        let old = crate::lineage::Point {
            track: "A".into(),
            number: 1,
            sha: first.to_string(),
            base_at: original.to_string(),
            base_inherits_at: Default::default(),
            inherits: Default::default(),
        };
        let next = crate::lineage::Point {
            track: "A".into(),
            number: 2,
            sha: updated.to_string(),
            base_at: theirs.to_string(),
            base_inherits_at: Default::default(),
            inherits: Default::default(),
        };
        let mut basis = crate::lineage::Basis::root(mine.to_string());
        basis.inherits.insert("A".into(), old.inherited());
        let error = merge_update(
            &repo,
            &source(vec![old, next.clone()], next),
            &before.to_string(),
            &basis,
        )
        .unwrap_err();
        assert!(error.to_string().starts_with("Inherited base conflicts:"));
        assert!(std::fs::read_dir(root.path())?.all(|entry| entry.unwrap().file_name() == ".git"));
        Ok(())
    }
    #[tokio::test]
    async fn existing_workspace_resumes_a_partial_update_after_the_request_is_taken_back()
    -> Result<()> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        let path = root.path().join("worker");
        std::fs::create_dir(&path)?;
        gix::init(&path)?;
        let repo = Repository::open(&path)?;
        let base = repo.empty_starting_point()?;
        assert!(
            std::process::Command::new("/usr/bin/git")
                .arg("-C")
                .arg(&path)
                .args(["read-tree", &base.to_string()])
                .status()?
                .success()
        );
        let original_index = std::fs::read(path.join(".git/index"))?;
        let mut journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let input = Uuid::now_v7();
        journal
            .stage_utterance(input, "Dummy child".into(), Target::New, None, vec![])
            .await?;
        let claimed = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                claimed.id,
                claimed.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Fork,
                    track_id: None,
                    boundary: None,
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: serde_json::json!({}),
                }),
            )
            .await?;
        let word = journal.read_utterance(input.to_string()).await?;
        let track = word["decision"]["trackId"].as_str().unwrap().to_owned();
        let workspace = Workspace {
            track_id: track.clone(),
            path: path.clone(),
            reference: "refs/intercat/test/tracks/B".into(),
            base: base.to_string(),
            state: "ready".into(),
            error: None,
            attachment_roots: vec![],
        };
        journal.workspace_result(workspace.clone()).await?;
        let mut worker = WorkerState {
            track_id: track.clone(),
            phase: "starting".into(),
            session_id: Some("dummy-session".into()),
            secret_revision: None,
            identity: None,
            detail: None,
            active_turn: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        };
        journal.worker_state(worker.clone()).await?;
        let db = rusqlite::Connection::open(root.path().join("db"))?;
        db.execute(
            "INSERT INTO tracks VALUES('parent','A','Dummy parent','done')",
            [],
        )?;
        crate::lineage_store::initialize(
            &db,
            "parent",
            &crate::lineage::Basis::root(base.to_string()),
        )?;
        let blob = repo.write_blob(b"inherited\n")?;
        let tree = repo.write_tree(&[("parent.txt", blob)])?;
        let parent = repo.checkpoint_commit(tree, base, "Dummy parent")?;
        repo.sync_objects(&[parent])?;
        let op = Uuid::now_v7();
        journal
            .intent(
                &op.to_string(),
                "checkpoint.save",
                serde_json::json!({"trackId":"parent","checkpoint":{"sha":parent.to_string()}}),
            )
            .await?;
        journal.finish(&op.to_string(), "dispatched").await?;
        journal.finish(&op.to_string(), "complete").await?;
        let point = crate::lineage_store::point(&db, "parent", &parent.to_string())?;
        db.execute(
            "INSERT INTO tracks VALUES('other-parent','C','Other dummy parent','done')",
            [],
        )?;
        crate::lineage_store::initialize(
            &db,
            "other-parent",
            &crate::lineage::Basis::root(base.to_string()),
        )?;
        let other_blob = repo.write_blob(b"keep other parent's work\n")?;
        let other_tree = repo.write_tree(&[("other.txt", other_blob)])?;
        let other = repo.checkpoint_commit(other_tree, base, "Other parent")?;
        repo.sync_objects(&[other])?;
        let other_op = Uuid::now_v7();
        journal.intent(&other_op.to_string(),"checkpoint.save",serde_json::json!({"trackId":"other-parent","checkpoint":{"sha":other.to_string()}})).await?;
        journal.finish(&other_op.to_string(), "dispatched").await?;
        journal.finish(&other_op.to_string(), "complete").await?;
        let other_point = crate::lineage_store::point(&db, "other-parent", &other.to_string())?;
        crate::inheritance::queue(
            &db,
            &crate::inheritance::Dependencies {
                input: input.to_string(),
                track: track.clone(),
                points: vec![point.clone(), other_point.clone()],
            },
        )?;
        std::fs::write(path.join("keep.txt"), "keep my edit\n")?;
        let boundary = Boundary {
            guard: Guard::Work(crate::msp::StartGuard::Stopped(Arc::new(
                std::sync::atomic::AtomicBool::new(false),
            ))),
            tracker: Arc::new(Mutex::new(crate::process::Tracker::new(
                Uuid::now_v7().to_string(),
                crate::process::identity(std::process::id())?,
            )?)),
            identity: crate::process::identity(std::process::id())?,
        };
        let applied = prepare(
            &journal,
            input.to_string(),
            word["revision"].as_i64().unwrap(),
            "dummy-session".into(),
            workspace.clone(),
            None,
            boundary,
        )
        .await?
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join("parent.txt"))?,
            "inherited\n"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("keep.txt"))?,
            "keep my edit\n"
        );
        assert!(crate::inheritance::waiting(&db, &input.to_string())?.is_some());
        // Simulate failure after the checkpoint was saved but before the actor
        // persisted it. A restart must wait for Start and then reuse that exact
        // checkpoint, even though Start advances the input's revision.
        journal
            .change_inheritance(Change::Failure {
                input: input.to_string(),
                revision: word["revision"].as_i64().unwrap(),
                blocker: crate::inheritance_issue::Blocker::Other {
                    message: "Dummy interrupted actor save".into(),
                },
            })
            .await?;
        drop(journal);
        journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let held = journal.read_utterance(input.to_string()).await?;
        assert_eq!(held["state"], "held");
        assert_eq!(held["reason"], "Inherited files need review");
        let started = journal
            .start_held_input(
                Uuid::now_v7(),
                input.to_string(),
                held["revision"].as_i64().unwrap(),
            )
            .await?;
        assert_eq!(started["state"], "placed");
        assert!(started["revision"].as_i64() > held["revision"].as_i64());
        db.execute(
            "UPDATE utterances SET text='Changed dummy request' WHERE id=?1",
            [input.to_string()],
        )?;
        let make_boundary = || -> Result<Boundary> {
            let identity = crate::process::identity(std::process::id())?;
            Ok(Boundary {
                guard: Guard::Work(crate::msp::StartGuard::Stopped(Arc::new(
                    std::sync::atomic::AtomicBool::new(false),
                ))),
                tracker: Arc::new(Mutex::new(crate::process::Tracker::new(
                    Uuid::now_v7().to_string(),
                    identity,
                )?)),
                identity,
            })
        };
        let revision = started["revision"].as_i64().unwrap();
        let rejected = prepare(
            &journal,
            input.to_string(),
            revision,
            "dummy-session".into(),
            workspace.clone(),
            None,
            make_boundary()?,
        )
        .await;
        assert!(
            rejected
                .err()
                .unwrap()
                .to_string()
                .contains("request changed")
        );
        db.execute(
            "UPDATE utterances SET text='Dummy child' WHERE id=?1",
            [input.to_string()],
        )?;
        let recovered = prepare(
            &journal,
            input.to_string(),
            revision,
            "dummy-session".into(),
            workspace.clone(),
            None,
            make_boundary()?,
        )
        .await?
        .unwrap();
        assert_eq!(recovered.operation, applied.operation);
        assert_eq!(recovered.checkpoint.sha, applied.checkpoint.sha);
        worker.checkpoint = Some(recovered.checkpoint);
        journal.worker_state(worker.clone()).await?;
        journal
            .change_inheritance(Change::Complete(applied.operation))
            .await?;
        assert!(crate::inheritance::waiting(&db, &input.to_string())?.is_none());
        assert_eq!(
            crate::lineage_store::basis(&db, &track)?.inherits["parent"],
            point.inherited()
        );
        assert_eq!(
            std::fs::read_to_string(path.join("other.txt"))?,
            "keep other parent's work\n"
        );
        let update_blob = repo.write_blob(b"updated parent\n")?;
        let external_blob = repo.write_blob(b"another track\n")?;
        let moved_tree = repo.write_tree(&[("external.txt", external_blob)])?;
        let moved_base =
            repo.checkpoint_commit(moved_tree, base, "Parent rebased onto other work")?;
        let update_tree =
            repo.write_tree(&[("parent.txt", update_blob), ("external.txt", external_blob)])?;
        let update = repo.checkpoint_commit(update_tree, moved_base, "Updated parent")?;
        repo.sync_objects(&[update])?;
        crate::lineage_store::set_basis(
            &db,
            "parent",
            &crate::lineage::Basis::root(moved_base.to_string()),
        )?;
        let update_op = Uuid::now_v7();
        journal
            .intent(
                &update_op.to_string(),
                "checkpoint.save",
                serde_json::json!({"trackId":"parent","checkpoint":{"sha":update.to_string()}}),
            )
            .await?;
        journal.finish(&update_op.to_string(), "dispatched").await?;
        journal.finish(&update_op.to_string(), "complete").await?;
        let update_point = crate::lineage_store::point(&db, "parent", &update.to_string())?;
        let next_input = Uuid::now_v7();
        journal
            .stage_utterance(
                next_input,
                "Apply the new parent".into(),
                Target::Lock(track.clone()),
                None,
                vec![],
            )
            .await?;
        let next = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                next.id,
                next.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Extend,
                    track_id: Some(track.clone()),
                    boundary: Some("after-turn".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: serde_json::json!({}),
                }),
            )
            .await?;
        crate::inheritance::queue(
            &db,
            &crate::inheritance::Dependencies {
                input: next_input.to_string(),
                track: track.clone(),
                points: vec![update_point.clone()],
            },
        )?;
        let context = journal
            .inheritance_application(next_input.to_string())
            .await?
            .unwrap();
        let before = crate::checkpoint::capture(
            &workspace,
            &worker.checkpoint.as_ref().unwrap().sha,
            "Before interrupted update",
        )
        .await?;
        let (after, basis) = merge_update(&repo, &context.source, &before.sha, &context.basis)?;
        let application = Application {
            operation: Uuid::new_v5(&next_input, b"apply-inherited-files"),
            dependencies: context.source.dependencies,
            revision: 1,
            request_digest: context.request_digest,
            session: "dummy-session".into(),
            workspace: workspace.clone(),
            previous: worker.checkpoint.clone(),
            before: before.sha,
            after,
            before_basis: context.basis,
            basis,
            checkpoint_turn: Uuid::new_v5(&next_input, b"inherited-checkpoint"),
            expected_base: repo
                .reference_target("refs/intercat/test/base/B")?
                .map(|sha| sha.to_string()),
        };
        journal
            .change_inheritance(Change::Begin(Box::new(application.clone())))
            .await?;
        journal
            .finish(&application.operation.to_string(), "dispatched")
            .await?;
        // Only the first path reached disk before the simulated process loss.
        std::fs::write(path.join("parent.txt"), "updated parent\n")?;
        journal
            .cut_input(Uuid::now_v7(), next_input.to_string())
            .await?;
        db.execute(
            "UPDATE utterances SET text='Edited after taking back',revision=revision+1 WHERE id=?1",
            [next_input.to_string()],
        )?;
        worker.phase = "held".into();
        journal.worker_state(worker.clone()).await?;
        journal.pause_dispatch(Uuid::now_v7(), true).await?;
        drop(journal);
        journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        assert!(!path.join("external.txt").exists());
        assert_eq!(
            journal.snapshot().await?["pendingInheritance"][0]["sourceState"],
            "cut"
        );
        let stop = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let gate = Arc::new(crate::dispatch_control::Gate::new(true));
        let recovery_boundary = |expected| -> Result<Boundary> {
            let identity = crate::process::identity(std::process::id())?;
            Ok(Boundary {
                guard: Guard::Recovery {
                    generation: stop.clone(),
                    expected,
                    gate: gate.clone(),
                    epoch: gate.epoch(),
                },
                tracker: Arc::new(Mutex::new(crate::process::Tracker::new(
                    Uuid::now_v7().to_string(),
                    identity,
                )?)),
                identity,
            })
        };
        let request = Uuid::now_v7();
        assert!(
            recover(
                &journal,
                request,
                application.operation,
                workspace.clone(),
                "dummy-session".into(),
                recovery_boundary(0)?
            )
            .await
            .is_err()
        );
        assert!(!path.join("external.txt").exists());
        std::fs::write(path.join("external.txt"), "keep this later edit\n")?;
        assert!(
            recover(
                &journal,
                request,
                application.operation,
                workspace.clone(),
                "dummy-session".into(),
                recovery_boundary(1)?
            )
            .await
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(path.join("external.txt"))?,
            "keep this later edit\n"
        );
        std::fs::remove_file(path.join("external.txt"))?;
        // Exercise the actual worker control loop over an empty MSP transport.
        // Recovery must finish files without emitting any model command.
        let mut background = tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()?;
        let background_identity =
            crate::process::identity(background.id().ok_or("Missing dummy process")?)?;
        journal
            .change_inheritance(Change::RecoveryFailure {
                operation: application.operation,
                blocker: crate::inheritance_issue::Blocker::Processes,
            })
            .await?;
        let process_issue: crate::inheritance_issue::Issue =
            serde_json::from_value(journal.snapshot().await?["inheritanceIssues"][0].clone())?;
        let (read, _responses) = tokio::io::duplex(4096);
        let (write, requests) = tokio::io::duplex(4096);
        let mut connection = crate::msp::Connection::new(read, write);
        let (port, commands) = crate::execution::Port::new(gate.clone());
        port.attach(connection.client.clone(), "dummy-session".into());
        let mut context = crate::execution::Context {
            project_secrets: Default::default(),
            port: port.clone(),
            commands,
            editor: crate::editor::Manager::new(root.path(), &root.path().join("editor.log"))?,
            capacity: Arc::new(crate::scheduler::Capacity::new(1)),
        };
        let (stop_run, mut stopping) = tokio::sync::watch::channel(false);
        let identity = crate::process::identity(std::process::id())?;
        let tracker = Arc::new(Mutex::new(crate::process::Tracker::new(
            Uuid::now_v7().to_string(),
            identity,
        )?));
        let mut continuity = crate::execution::Continuity::default();
        let runner = crate::execution::run(
            crate::execution::WorkerIo {
                connection: &mut connection,
                tracker,
                identity,
                recovery: None,
                continuity: &mut continuity,
            },
            &journal,
            &path,
            &mut worker,
            &mut context,
            &mut stopping,
        );
        let controller = async {
            let result = async {
                let command = crate::inheritance_issue::Command {
                    op_id: Uuid::now_v7(),
                    issue_id: process_issue.id,
                    action: crate::inheritance_issue::Action::Stop,
                };
                port.stop_inheritance(command.clone()).await?;
                port.stop_inheritance(command).await?;
                if crate::process::is_alive(background_identity) {
                    return Err("The dummy worker process was not stopped.".into());
                }
                port.finish_inheritance(request, application.operation)
                    .await?;
                port.finish_inheritance(request, application.operation)
                    .await?;
                use tokio::io::AsyncBufReadExt;
                let mut requests = tokio::io::BufReader::new(requests);
                let mut line = String::new();
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(50),
                        requests.read_line(&mut line)
                    )
                    .await
                    .is_err(),
                    "File recovery sent an MSP command: {line}"
                );
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            }
            .await;
            stop_run.send(true)?;
            result
        };
        let (ran, controlled) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(runner, controller)
        })
        .await?;
        ran?;
        controlled?;
        continuity.settle().await?;
        let _ = background.wait().await;
        assert_eq!(
            std::fs::read_to_string(path.join("parent.txt"))?,
            "updated parent\n"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("other.txt"))?,
            "keep other parent's work\n"
        );
        let basis = crate::lineage_store::basis(&db, &track)?;
        assert_eq!(basis.inherits["parent"], update_point.inherited());
        assert_eq!(basis.inherits["other-parent"], other_point.inherited());
        assert_eq!(
            repo.tree_id(basis.base.parse()?)?,
            repo.tree_id(moved_base)?
        );
        assert_eq!(
            repo.reference_target("refs/intercat/test/base/B")?,
            Some(basis.base.parse()?)
        );
        assert_eq!(
            std::fs::read_to_string(path.join("external.txt"))?,
            "another track\n"
        );
        assert_eq!(std::fs::read(path.join(".git/index"))?, original_index);
        let snapshot = journal.snapshot().await?;
        assert!(
            snapshot["pendingInheritance"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(snapshot["dispatchPaused"], true);
        assert_eq!(
            journal.read_utterance(next_input.to_string()).await?["state"],
            "cut"
        );
        assert_eq!(
            journal.read_utterance(next_input.to_string()).await?["text"],
            "Edited after taking back"
        );
        assert_eq!(
            journal.read_utterance(input.to_string()).await?["state"],
            "held"
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM operations WHERE kind='turn.start'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            0
        );
        std::fs::write(path.join("keep.txt"), "edit after recovery\n")?;
        assert!(
            recover(
                &journal,
                request,
                application.operation,
                workspace,
                "dummy-session".into(),
                recovery_boundary(1)?
            )
            .await?
            .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(path.join("keep.txt"))?,
            "edit after recovery\n"
        );
        Ok(())
    }
}
