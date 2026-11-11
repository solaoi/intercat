//! Frozen dependency files for a new worker workspace. Existing workspaces
//! require an idle worker boundary; their requests remain held until applied.
use crate::{
    Result,
    journal::Journal,
    lineage::{Basis, Frozen, Point},
    preparation::Workspace,
};
use intercat_git::{Merge, Repository};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Dependencies {
    pub input: String,
    pub track: String,
    pub points: Vec<Point>,
}
pub(crate) struct Source {
    pub dependencies: Dependencies,
    pub graph: Frozen,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Initial {
    pub operation: Uuid,
    pub dependencies: Dependencies,
    pub head: String,
    pub basis: Basis,
    pub original: Basis,
}

async fn protect(
    repository: &std::path::Path,
    workspace: &Workspace,
    initial: &Initial,
) -> Result<()> {
    let repository = repository.to_owned();
    let reference = format!("{}-inherited", workspace.reference);
    let head = initial.head.parse()?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(repository)?;
        repo.compare_and_swap(&reference, None, head)?;
        repo.sync_ref(&reference)?;
        Ok(())
    })
    .await?
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS input_inheritance(input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,track TEXT NOT NULL REFERENCES tracks(id),record TEXT NOT NULL);")?;
    Ok(())
}
pub(crate) fn queue(db: &Connection, dependencies: &Dependencies) -> Result<()> {
    if dependencies.points.is_empty() {
        return Ok(());
    }
    db.execute(
        "INSERT INTO input_inheritance VALUES(?1,?2,?3)",
        params![
            dependencies.input,
            dependencies.track,
            serde_json::to_string(dependencies)?
        ],
    )?;
    Ok(())
}
pub(crate) fn dependencies(db: &Connection, input: &str) -> Result<Option<Dependencies>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM input_inheritance WHERE input=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}
pub(crate) fn source(db: &Connection, input: &str) -> Result<Option<Source>> {
    dependencies(db, input)?
        .map(|dependencies| {
            Ok(Source {
                dependencies,
                graph: crate::lineage_store::freeze(db)?,
            })
        })
        .transpose()
}
pub(crate) fn waiting(db: &Connection, input: &str) -> Result<Option<String>> {
    let Some(dependencies) = dependencies(db, input)? else {
        return Ok(Some("Preparing inherited files".into()));
    };
    if crate::inheritance_apply_store::pending(db, &dependencies.track)?.is_some() {
        return Ok(Some("Applying inherited files".into()));
    }
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM track_lineage WHERE track=?1)",
        [&dependencies.track],
        |row| row.get(0),
    )?;
    let basis = if exists {
        Some(crate::lineage_store::basis(db, &dependencies.track)?)
    } else {
        None
    };
    for point in dependencies.points {
        if !basis
            .as_ref()
            .and_then(|basis| basis.inherits.get(&point.track))
            .is_some_and(|saved| {
                saved.number > point.number
                    || (saved.number == point.number && saved.sha == point.sha)
            })
        {
            return Ok(Some("Preparing inherited files".into()));
        }
    }
    Ok(None)
}

/// Reuse L composition to advance shared ancestors before merging the
/// remaining parents. A branch inherits the first parent's historical base.
pub(crate) fn combine(repo: &Repository, source: &Source) -> Result<(String, Basis)> {
    let first = source
        .dependencies
        .points
        .first()
        .ok_or("Missing inherited checkpoint.")?;
    let mut desired = Default::default();
    let mut parents = crate::lineage::Ancestors::new();
    for point in &source.dependencies.points {
        crate::lineage::include(&mut parents, &point.track, &point.inherited())?;
        if source.graph.by_sha(&point.track, &point.sha)? != point {
            return Err("The saved dependency checkpoint changed.".into());
        }
        source.graph.incorporate(&mut desired, point)?;
    }
    let mut covered = Default::default();
    source.graph.incorporate(&mut covered, first)?;
    let views = crate::lineage::view::merge(
        source
            .dependencies
            .points
            .iter()
            .map(|point| point.views.clone()),
    )?;
    let recipe = Basis {
        views: views.clone(),
        parents: parents.clone(),
        base: first.sha.clone(),
        base_inherits: covered,
        inherits: desired.clone(),
    };
    let tree = match source.graph.baseline(
        &mut crate::merge_choices::Merges::new(repo, &[])?,
        &recipe,
        &desired,
    )? {
        Merge::Clean(tree) => tree,
        Merge::Conflicts { paths, .. } => {
            return Err(crate::inheritance_issue::Blocker::Conflict {
                paths: paths.iter().map(ToString::to_string).collect(),
                base: false,
            }
            .into());
        }
    };
    let head = if tree == repo.tree_id(first.sha.parse()?)? {
        first.sha.parse()?
    } else {
        repo.checkpoint_commit(
            tree,
            first.sha.parse()?,
            &format!("Intercat inherited workspace {}", source.dependencies.input),
        )?
    };
    repo.sync_objects(&[head])?;
    let mut basis = first.basis();
    basis.views = views;
    basis.inherits = desired;
    basis.parents = parents;
    Ok((head.to_string(), basis))
}

pub(crate) async fn initial(
    journal: &Journal,
    workspace: &Workspace,
    repository: &std::path::Path,
    source: Source,
) -> Result<Initial> {
    if source.dependencies.track != workspace.track_id {
        return Err("The inherited files belong to another track.".into());
    }
    for point in &source.dependencies.points {
        if source.graph.by_sha(&point.track, &point.sha)? != point {
            return Err("The saved dependency checkpoint changed.".into());
        }
    }
    let operation = Uuid::new_v5(&workspace.track_id.parse()?, b"initial.inheritance");
    if let Some(saved) = journal.operation(operation).await? {
        let initial: Initial = serde_json::from_value(saved["payload"].clone())?;
        let parents = |dependencies: &Dependencies| {
            dependencies
                .points
                .iter()
                .map(|point| point.track.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        if saved["kind"] != "workspace.inherit"
            || initial.dependencies.track != source.dependencies.track
            || parents(&initial.dependencies) != parents(&source.dependencies)
        {
            return Err(
                "The planned dependencies changed after workspace preparation began.".into(),
            );
        }
        // Finish the first durable workspace intent exactly as recorded. A
        // newer unstarted input applies updated checkpoints at the idle boundary.
        protect(repository, workspace, &initial).await?;
        return Ok(initial);
    }
    let read_repository = repository.to_owned();
    let dependencies = source.dependencies.clone();
    let (head, basis) =
        tokio::task::spawn_blocking(move || combine(&Repository::open(read_repository)?, &source))
            .await??;
    let initial = Initial {
        operation,
        dependencies,
        head,
        basis,
        original: Basis::root(workspace.base.clone()),
    };
    journal
        .intent(
            &operation.to_string(),
            "workspace.inherit",
            serde_json::to_value(&initial)?,
        )
        .await?;
    protect(repository, workspace, &initial).await?;
    Ok(initial)
}
pub(crate) async fn finish(
    journal: &Journal,
    initial: &Initial,
    workspace: Workspace,
) -> Result<()> {
    let saved = journal
        .operation(initial.operation)
        .await?
        .ok_or("The inherited workspace was not reserved.")?;
    if saved["state"] == "prepared" {
        journal
            .finish(&initial.operation.to_string(), "dispatched")
            .await?;
    }
    if saved["state"] != "complete" {
        journal
            .finish(&initial.operation.to_string(), "complete")
            .await?;
    }
    journal
        .inherited_workspace(workspace, initial.clone())
        .await
}

pub(crate) fn ready(db: &Connection, workspace: &Workspace, initial: &Initial) -> Result<()> {
    if workspace.state != "ready"
        || workspace.track_id != initial.dependencies.track
        || workspace.base != initial.head
    {
        return Err("The inherited workspace does not match its saved source.".into());
    }
    let saved:String=db.query_row("SELECT payload FROM operations WHERE id=?1 AND kind='workspace.inherit' AND state='complete'",[initial.operation.to_string()],|row|row.get(0))?;
    if serde_json::from_str::<Initial>(&saved)? != *initial {
        return Err("The inherited workspace record changed.".into());
    }
    let used:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM checkpoint_lineage WHERE track=?1) OR EXISTS(SELECT 1 FROM workers WHERE track_id=?1)",[&workspace.track_id],|row|row.get(0))?;
    if used {
        return Err("Inherited files need an idle worker boundary on this existing track.".into());
    }
    crate::lineage_store::set_basis(db, &workspace.track_id, &initial.basis)?;
    crate::plan_files::initial(db, initial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        decision::{Decision, Kind, Outcome, Target},
        secrets::Key,
    };
    use std::{fs, path::Path, time::Duration};
    async fn track(journal: &Journal, title: &str) -> Result<(String, String)> {
        let id = Uuid::now_v7();
        journal
            .stage_utterance(id, title.into(), Target::New, None, vec![])
            .await?;
        let pending = journal.claim_input().await?.unwrap();
        assert_eq!(pending.id, id.to_string());
        journal
            .apply_input(
                pending.id,
                pending.revision,
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
        let track = journal.read_utterance(id.to_string()).await?["decision"]["trackId"]
            .as_str()
            .unwrap()
            .to_string();
        Ok((id.to_string(), track))
    }
    async fn workspace(journal: &Journal, track: &str) -> Result<Workspace> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(workspace) = journal.workspace(track.into()).await {
                    if workspace.state == "ready" {
                        return Ok(workspace);
                    }
                    if workspace.state == "failed" {
                        return Err(workspace.error.unwrap_or_default().into());
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }
    #[tokio::test]
    async fn initial_workspace_uses_the_saved_parent_and_retains_changed_files_on_retry()
    -> Result<()> {
        let root = tempfile::tempdir()?;
        let project = root.path().join("project");
        fs::create_dir(&project)?;
        assert!(
            std::process::Command::new("/usr/bin/git")
                .args(["init", "-q", "-b", "main"])
                .arg(&project)
                .status()?
                .success()
        );
        let db_path = root.path().join("db");
        let journal = Journal::open(&db_path, Key::new([7; 32])).await?;
        journal
            .configure_project(
                Uuid::now_v7(),
                crate::project::inspect(
                    uuid::Uuid::now_v7(),
                    "Dummy project".into(),
                    vec![project.clone()],
                )
                .await?,
            )
            .await?;
        let (_, a) = track(&journal, "Dummy parent").await?;
        let prep = crate::preparation::Preparation::start(
            &root.path().join("workspaces"),
            journal.clone(),
        )?;
        let parent = workspace(&journal, &a).await?;
        prep.shutdown().await;
        fs::write(parent.path.join("one.txt"), "SAVED_PARENT\n")?;
        fs::write(parent.path.join(".env"), "DUMMY_SECRET=not-a-user-secret\n")?;
        let checkpoint =
            crate::checkpoint::save(parent.clone(), Uuid::now_v7(), None, false, journal.clone())
                .await?;
        let db = Connection::open(&db_path)?;
        let point = crate::lineage_store::point(&db, &a, &checkpoint.sha)?;
        fs::write(parent.path.join("one.txt"), "UNSAVED_PARENT\n")?;
        let (input, b) = track(&journal, "Dummy child").await?;
        queue(
            &db,
            &Dependencies {
                input: input.clone(),
                track: b.clone(),
                points: vec![point.clone()],
            },
        )?;
        let prep = crate::preparation::Preparation::start(
            &root.path().join("workspaces"),
            journal.clone(),
        )?;
        let child = workspace(&journal, &b).await?;
        prep.shutdown().await;
        assert_eq!(
            fs::read_to_string(child.path.join("one.txt"))?,
            "SAVED_PARENT\n"
        );
        assert!(!child.path.join(".env").exists());
        assert!(!project.join("one.txt").exists());
        assert!(waiting(&db, &input)?.is_none());
        let basis = crate::lineage_store::basis(&db, &b)?;
        assert_eq!(basis.base, point.base_at);
        assert_eq!(basis.inherits[&a], point.inherited());
        let mut changed = source(&db, &input)?.unwrap();
        changed.dependencies.points[0].sha = "f".repeat(40);
        assert!(initial(&journal, &child, &project, changed).await.is_err());
        assert_eq!(
            fs::read_to_string(child.path.join("one.txt"))?,
            "SAVED_PARENT\n"
        );
        fs::write(parent.path.join("one.txt"), "UPDATED_PARENT\n")?;
        let updated = crate::checkpoint::save(
            parent.clone(),
            Uuid::now_v7(),
            Some(checkpoint.clone()),
            false,
            journal.clone(),
        )
        .await?;
        let next_input = Uuid::now_v7();
        journal
            .stage_utterance(
                next_input,
                "Use the updated dummy parent".into(),
                Target::Lock(b.clone()),
                None,
                vec![],
            )
            .await?;
        let claimed = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                claimed.id,
                claimed.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Extend,
                    track_id: Some(b.clone()),
                    boundary: Some("after-turn".into()),
                    command: None,
                    confidence: 1.0,
                    inference_ms: 0.0,
                    timed_out: false,
                    scores: serde_json::json!({}),
                }),
            )
            .await?;
        queue(
            &db,
            &Dependencies {
                input: next_input.to_string(),
                track: b.clone(),
                points: vec![crate::lineage_store::point(&db, &a, &updated.sha)?],
            },
        )?;
        db.execute("UPDATE utterances SET state='cut' WHERE id=?1", [&input])?;
        let resumed = initial(
            &journal,
            &child,
            &project,
            source(&db, &next_input.to_string())?.unwrap(),
        )
        .await?;
        assert_eq!(
            resumed.head, child.base,
            "Resume the first durable workspace, then apply the newer input at its boundary"
        );
        assert_eq!(resumed.dependencies.input, input);
        let mut retry = child.clone();
        retry.state = "pending".into();
        journal.workspace_result(retry).await?;
        let prep = crate::preparation::Preparation::start(
            &root.path().join("workspaces"),
            journal.clone(),
        )?;
        let recovered = workspace(&journal, &b).await?;
        prep.shutdown().await;
        assert_eq!(recovered.base, child.base);
        assert_eq!(
            fs::read_to_string(child.path.join("one.txt"))?,
            "SAVED_PARENT\n"
        );
        assert!(
            waiting(&db, &next_input.to_string())?.is_some(),
            "The newer parent must still be applied before the new input runs"
        );
        fs::write(child.path.join("one.txt"), "KEEP_CHILD_EDIT\n")?;
        let mut retry = child.clone();
        retry.state = "pending".into();
        journal.workspace_result(retry).await?;
        let prep = crate::preparation::Preparation::start(
            &root.path().join("workspaces"),
            journal.clone(),
        )?;
        assert!(workspace(&journal, &b).await.is_err());
        prep.shutdown().await;
        assert_eq!(
            fs::read_to_string(child.path.join("one.txt"))?,
            "KEEP_CHILD_EDIT\n"
        );
        db.execute("DELETE FROM input_inheritance WHERE input=?1", [&input])?;
        assert!(waiting(&db, &input)?.is_some());
        Ok(())
    }

    #[test]
    fn new_inheritance_retains_shared_views_for_later_baselines_and_target_deletions() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        Repository::init_isolated(root.path())?;
        let repo = Repository::open(root.path())?;
        let base = repo.empty_starting_point()?;
        let tree = |files: &[(&str, &str)]| -> Result<intercat_git::ObjectId> {
            repo.write_tree(
                &files
                    .iter()
                    .map(|(name, text)| Ok((*name, repo.write_blob(text.as_bytes())?)))
                    .collect::<Result<Vec<_>>>()?,
            )
        };
        let saved = |track: &str,
                     number: u64,
                     files: &[(&str, &str)],
                     parents: &[&Point]|
         -> Result<Point> {
            let mut inherits = crate::lineage::Ancestors::new();
            for parent in parents {
                for (t, m) in &parent.inherits {
                    crate::lineage::include(&mut inherits, t, m)?;
                }
                crate::lineage::include(&mut inherits, &parent.track, &parent.inherited())?;
            }
            Ok(Point {
                views: vec![],
                track: track.into(),
                number,
                sha: repo
                    .checkpoint_commit(tree(files)?, base, "Own inheritance context")?
                    .to_string(),
                base_at: base.to_string(),
                base_inherits_at: Default::default(),
                parents: parents
                    .iter()
                    .map(|p| (p.track.clone(), p.inherited()))
                    .collect(),
                inherits,
            })
        };
        let a1 = saved("A", 1, &[("shared-a", "A fork")], &[])?;
        let a3 = saved(
            "A",
            3,
            &[("shared-a", "A fork"), ("retired-a", "A later")],
            &[],
        )?;
        let alt = saved(
            "A-alt",
            1,
            &[("shared-a", "A fork"), ("chosen-a", "A choice")],
            &[&a1],
        )?;
        let b = saved(
            "B",
            1,
            &[
                ("shared-a", "A fork"),
                ("retired-a", "A later"),
                ("shared-b", "B fork"),
            ],
            &[&a3],
        )?;
        let mut graph = Frozen {
            points: vec![a1, a3.clone(), alt.clone(), b.clone()],
            integrated: Default::default(),
        };
        let crate::shared_context::Outcome::View(view) = crate::shared_context::project(
            &repo,
            &graph,
            &crate::shared_context::Projection {
                source: b.clone(),
                previous: None,
                from: a3.clone(),
                onto: alt.clone(),
                views: vec![],
            },
            &[],
        )?
        else {
            panic!("Shared B should normalize cleanly");
        };
        let mut child = saved(
            "B-alt",
            2,
            &[
                ("shared-a", "A fork"),
                ("chosen-a", "A choice"),
                ("shared-b", "B fork"),
                ("chosen-b", "B choice"),
            ],
            &[&b],
        )?;
        child.views = vec![*view.clone()];
        child.inherits = view.origin.inherits.clone();
        crate::lineage::include(&mut child.inherits, "B", &b.inherited())?;
        child.base_at = view.origin.base.clone();
        child.base_inherits_at = view.origin.base_inherits.clone();
        let independent = saved("X", 1, &[("independent", "X own")], &[])?;
        graph.points.extend([child.clone(), independent.clone()]);
        let source = Source {
            dependencies: Dependencies {
                input: "own context inheritance".into(),
                track: "C".into(),
                points: vec![child.clone(), independent.clone()],
            },
            graph,
        };
        let (head, basis) = combine(&repo, &source)?;
        assert_eq!(basis.views, child.views);
        let files = repo.saved_files(head.parse()?)?;
        assert!(!files.contains_key("retired-a"));
        assert!(files.contains_key("shared-b"));
        assert!(files.contains_key("independent"));
        let mut coverage = crate::lineage::Ancestors::new();
        source.graph.incorporate(&mut coverage, &child)?;
        source.graph.incorporate(&mut coverage, &independent)?;
        let mut merges = crate::merge_choices::Merges::new(&repo, &[])?;
        let Merge::Clean(baseline) = source.graph.baseline(&mut merges, &basis, &coverage)? else {
            panic!("C's baseline must retain the projected shared source");
        };
        assert!(!repo.tree_files(baseline)?.contains_key("retired-a"));
        let target = tree(&[
            ("shared-a", "A fork"),
            ("chosen-a", "A choice"),
            ("chosen-b", "B choice"),
            ("independent", "X own"),
            ("retired-a", "Own later unrelated file"),
        ])?;
        let incoming = tree(&[
            ("shared-a", "A fork"),
            ("chosen-a", "A choice"),
            ("shared-b", "B fork"),
            ("chosen-b", "B choice"),
            ("independent", "X own"),
            ("own-c", "C pending"),
        ])?;
        let Merge::Clean(result) = repo.merge_trees(baseline, target, incoming)? else {
            panic!("A normalized inherited baseline must preserve the later target edit");
        };
        let files = repo.tree_files(result)?;
        assert!(!files.contains_key("shared-b"));
        assert!(files.contains_key("own-c"));
        assert_eq!(
            repo.saved_bytes(&files["retired-a"])?,
            b"Own later unrelated file"
        );
        Ok(())
    }

    fn point(repo: &Repository, root: &str, track: &str, value: &str) -> Result<Point> {
        let blob = repo.write_blob(value.as_bytes())?;
        let tree = repo.write_tree(&[("same.txt", blob)])?;
        let sha = repo
            .checkpoint_commit(tree, root.parse()?, track)?
            .to_string();
        Ok(Point {
            views: Vec::new(),
            parents: Default::default(),
            track: track.into(),
            number: 1,
            sha,
            base_at: root.into(),
            base_inherits_at: Default::default(),
            inherits: Default::default(),
        })
    }
    #[test]
    fn conflicting_parents_do_not_produce_an_initial_workspace() -> Result<()> {
        let root = tempfile::tempdir()?;
        Repository::init_isolated(root.path())?;
        let repo = Repository::open(root.path())?;
        let base = repo.empty_starting_point()?.to_string();
        let a = point(&repo, &base, "A", "left\n")?;
        let b = point(&repo, &base, "B", "right\n")?;
        let source = Source {
            dependencies: Dependencies {
                input: "dummy".into(),
                track: "C".into(),
                points: vec![a.clone(), b.clone()],
            },
            graph: Frozen {
                points: vec![a, b],
                integrated: Default::default(),
            },
        };
        assert!(
            combine(&repo, &source)
                .unwrap_err()
                .to_string()
                .contains("conflict")
        );
        assert!(!Path::new(root.path()).join("worker").exists());
        Ok(())
    }

    #[tokio::test]
    async fn conflicting_initial_parents_create_a_durable_card_without_a_worktree() -> Result<()> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        let project = root.path().join("project");
        fs::create_dir(&project)?;
        assert!(
            std::process::Command::new("/usr/bin/git")
                .args(["init", "-q", "-b", "main"])
                .arg(&project)
                .status()?
                .success()
        );
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        journal
            .configure_project(
                Uuid::now_v7(),
                crate::project::inspect(
                    uuid::Uuid::now_v7(),
                    "Dummy project".into(),
                    vec![project.clone()],
                )
                .await?,
            )
            .await?;
        let (_, a) = track(&journal, "Dummy left").await?;
        let (_, b) = track(&journal, "Dummy right").await?;
        let preparation = crate::preparation::Preparation::start(
            &root.path().join("workspaces"),
            journal.clone(),
        )?;
        let left = workspace(&journal, &a).await?;
        let right = workspace(&journal, &b).await?;
        preparation.shutdown().await;
        fs::write(left.path.join("same.txt"), "LEFT\n")?;
        fs::write(right.path.join("same.txt"), "RIGHT\n")?;
        let left_checkpoint =
            crate::checkpoint::save(left.clone(), Uuid::now_v7(), None, false, journal.clone())
                .await?;
        let right_checkpoint =
            crate::checkpoint::save(right.clone(), Uuid::now_v7(), None, false, journal.clone())
                .await?;
        let (input, child) = track(&journal, "Dummy blocked child").await?;
        let db = Connection::open(root.path().join("db"))?;
        queue(
            &db,
            &Dependencies {
                input: input.clone(),
                track: child.clone(),
                points: vec![
                    crate::lineage_store::point(&db, &a, &left_checkpoint.sha)?,
                    crate::lineage_store::point(&db, &b, &right_checkpoint.sha)?,
                ],
            },
        )?;
        let preparation = crate::preparation::Preparation::start(
            &root.path().join("workspaces"),
            journal.clone(),
        )?;
        let snapshot = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = journal.snapshot().await?;
                if !snapshot["inheritanceIssues"].as_array().unwrap().is_empty() {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(snapshot);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        preparation.shutdown().await;
        assert_eq!(
            snapshot["inheritanceIssues"][0]["blocker"]["kind"],
            "conflict"
        );
        assert_eq!(
            snapshot["inheritanceIssues"][0]["blocker"]["paths"][0],
            "same.txt"
        );
        assert_eq!(journal.read_utterance(input).await?["state"], "held");
        assert!(!journal.workspace(child).await?.path.exists());
        assert_eq!(fs::read_to_string(left.path.join("same.txt"))?, "LEFT\n");
        assert_eq!(fs::read_to_string(right.path.join("same.txt"))?, "RIGHT\n");
        assert!(snapshot["workers"].as_array().unwrap().is_empty());
        Ok(())
    }

    #[test]
    fn initial_merge_advances_a_shared_ancestor_once_and_keeps_its_deletion() -> Result<()> {
        let root = tempfile::tempdir()?;
        Repository::init_isolated(root.path())?;
        let repo = Repository::open(root.path())?;
        let base = repo.empty_starting_point()?.to_string();
        let saved = |track: &str,
                     number: u64,
                     files: &[(&str, &str)],
                     parents: &[&Point]|
         -> Result<Point> {
            let blobs = files
                .iter()
                .map(|(path, text)| Ok((*path, repo.write_blob(text.as_bytes())?)))
                .collect::<Result<Vec<_>>>()?;
            let tree = repo.write_tree(&blobs)?;
            let sha = repo
                .checkpoint_commit(tree, base.parse()?, &format!("{track}{number}"))?
                .to_string();
            let mut inherits = Default::default();
            for parent in parents {
                for (track, mark) in &parent.inherits {
                    crate::lineage::include(&mut inherits, track, mark)?;
                }
                crate::lineage::include(&mut inherits, &parent.track, &parent.inherited())?;
            }
            Ok(Point {
                views: Vec::new(),
                parents: parents
                    .iter()
                    .map(|point| (point.track.clone(), point.inherited()))
                    .collect(),
                track: track.into(),
                number,
                sha,
                base_at: base.clone(),
                base_inherits_at: Default::default(),
                inherits,
            })
        };
        let a1 = saved("A", 1, &[("f", "old")], &[])?;
        let a2 = saved("A", 2, &[], &[])?;
        let b = saved("B", 1, &[("f", "old"), ("b", "left")], &[&a1])?;
        let c = saved("C", 1, &[("c", "right")], &[&a2])?;
        let source = Source {
            dependencies: Dependencies {
                input: "dummy".into(),
                track: "D".into(),
                points: vec![b.clone(), c.clone()],
            },
            graph: Frozen {
                points: vec![a1, a2.clone(), b, c],
                integrated: Default::default(),
            },
        };
        let (head, basis) = combine(&repo, &source)?;
        let files = repo.saved_files(head.parse()?)?;
        assert!(!files.contains_key("f"));
        assert!(files.contains_key("b") && files.contains_key("c"));
        assert_eq!(basis.inherits["A"], a2.inherited());
        assert_eq!(
            basis.parents.keys().map(String::as_str).collect::<Vec<_>>(),
            ["B", "C"]
        );
        assert_eq!(
            source.graph.inherited_from(&basis.parents, &basis.views)?,
            basis.inherits
        );
        Ok(())
    }
}
