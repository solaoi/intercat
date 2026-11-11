use intercat_daemon::{
    Result,
    lineage::{Ancestors, Basis, Frozen, Point, include},
    realign::{ConflictStage, Plan, Resolution},
};
use intercat_git::{ObjectId, Repository};

struct Fixture {
    root: tempfile::TempDir,
    repo: Repository,
    start: String,
    graph: Frozen,
}
impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        Repository::init_isolated(root.path())?;
        let repo = Repository::open(root.path())?;
        let start = repo.empty_starting_point()?.to_string();
        Ok(Self {
            root,
            repo,
            start,
            graph: Frozen::default(),
        })
    }
    fn commit(&self, files: &[(&str, &str)], parent: &str) -> Result<String> {
        let blobs = files
            .iter()
            .map(|(path, text)| Ok((*path, self.repo.write_blob(text.as_bytes())?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(self
            .repo
            .checkpoint_commit(
                self.repo.write_tree(&blobs)?,
                parent.parse()?,
                "Agent-created realignment fixture",
            )?
            .to_string())
    }
    fn point(&mut self, track: &str, files: &[(&str, &str)], basis: &Basis) -> Result<Point> {
        let point = Point {
            views: Vec::new(),
            parents: basis.parents.clone(),
            track: track.into(),
            number: self
                .graph
                .points
                .iter()
                .filter(|point| point.track == track)
                .count() as u64
                + 1,
            sha: self.commit(files, &basis.base)?,
            base_at: basis.base.clone(),
            base_inherits_at: basis.base_inherits.clone(),
            inherits: basis.inherits.clone(),
        };
        self.graph.points.push(point.clone());
        Ok(point)
    }
    fn files(&self, plan: &Plan) -> Result<std::collections::BTreeMap<String, String>> {
        let Resolution::Ready { tree } = &plan.resolution else {
            panic!("Unexpected conflict: {:?}", plan.resolution)
        };
        self.repo
            .tree_files(tree.parse()?)?
            .into_iter()
            .map(|(path, file)| Ok((path, String::from_utf8(self.repo.saved_bytes(&file)?)?)))
            .collect()
    }
}
#[test]
fn realignment_uses_the_current_copy_and_retains_the_published_head_as_its_new_base() -> Result<()>
{
    let mut f = Fixture::new()?;
    let root = Basis::root(f.start.clone());
    let a = f.point("A", &[("a", "A")], &root)?;
    let x = f.point("X", &[("x", "X")], &root)?;
    let mut basis = Basis::root(a.sha.clone());
    include(&mut basis.base_inherits, "A", &a.inherited())?;
    let before = f.commit(&[("a", "A"), ("own", "uncommitted A")], &a.sha)?;
    let target = f.commit(&[("a", "A"), ("x", "X")], &a.sha)?;
    let coverage = Ancestors::from([("A".into(), a.inherited()), ("X".into(), x.inherited())]);
    let head = std::fs::read(f.repo.common_directory()?.join("HEAD"))?;
    let plan = Plan::prepare(
        &f.repo,
        &f.graph,
        intercat_daemon::realign::Input {
            track: "A",
            basis: &basis,
            before: before.parse()?,
            target: target.parse()?,
            coverage: &coverage,
            resolutions: &[],
        },
    )?;
    assert_eq!(
        f.files(&plan)?,
        [
            ("a".into(), "A".into()),
            ("own".into(), "uncommitted A".into()),
            ("x".into(), "X".into())
        ]
        .into()
    );
    assert_eq!(plan.basis.base, target);
    assert_eq!(plan.basis.base_inherits, coverage);
    assert_eq!(plan.before_basis, basis);
    assert_eq!(
        std::fs::read(f.repo.common_directory()?.join("HEAD"))?,
        head
    );
    assert!(!f.root.path().join("index").exists());
    Ok(())
}
#[test]
fn inherited_realign_does_not_restore_a_later_deletion_or_undo_another_workers_edit() -> Result<()>
{
    let mut f = Fixture::new()?;
    let root = Basis::root(f.start.clone());
    let a1 = f.point("A", &[("a", "200")], &root)?;
    let mut a_basis = Basis::root(a1.sha.clone());
    include(&mut a_basis.base_inherits, "A", &a1.inherited())?;
    let a2 = f.point("A", &[("a", "300"), ("g", "added by A2")], &a_basis)?;
    let mut c_initial = root;
    include(&mut c_initial.inherits, "A", &a1.inherited())?;
    include(&mut c_initial.parents, "A", &a1.inherited())?;
    let c1 = f.point("C", &[("a", "200"), ("c", "C")], &c_initial)?;
    let basis = Basis {
        views: Vec::new(),
        parents: Ancestors::from([("A".into(), a2.inherited())]),
        base: c1.sha.clone(),
        base_inherits: Ancestors::from([
            ("A".into(), a1.inherited()),
            ("C".into(), c1.inherited()),
        ]),
        inherits: Ancestors::from([("A".into(), a2.inherited())]),
    };
    let coverage = Ancestors::from([("A".into(), a2.inherited()), ("C".into(), c1.inherited())]);
    let before = f.commit(
        &[
            ("a", "300"),
            ("g", "added by A2"),
            ("c", "C"),
            ("late", "unpublished C"),
        ],
        &c1.sha,
    )?;
    let target = f.commit(&[("a", "400 by Y"), ("c", "C")], &a2.sha)?;
    let plan = Plan::prepare(
        &f.repo,
        &f.graph,
        intercat_daemon::realign::Input {
            track: "C",
            basis: &basis,
            before: before.parse()?,
            target: target.parse()?,
            coverage: &coverage,
            resolutions: &[],
        },
    )?;
    let files = f.files(&plan)?;
    assert!(!files.contains_key("g"));
    assert_eq!(files["a"], "400 by Y");
    assert_eq!(files["late"], "unpublished C");
    assert_eq!(files["c"], "C");
    assert_eq!(plan.basis.inherits, basis.inherits);
    assert_eq!(plan.basis.base, target);
    Ok(())
}
#[test]
fn a_conflict_preserves_all_three_frozen_inputs_for_a_separate_merge() -> Result<()> {
    let mut f = Fixture::new()?;
    let root = Basis::root(f.start.clone());
    let a = f.point("A", &[("a", "old")], &root)?;
    let mut basis = Basis::root(a.sha.clone());
    include(&mut basis.base_inherits, "A", &a.inherited())?;
    let coverage = Ancestors::from([("A".into(), a.inherited())]);
    let before = f.commit(&[("a", "my later work")], &a.sha)?;
    let target = f.commit(&[("a", "other worker changed it")], &a.sha)?;
    let plan = Plan::prepare(
        &f.repo,
        &f.graph,
        intercat_daemon::realign::Input {
            track: "A",
            basis: &basis,
            before: before.parse()?,
            target: target.parse()?,
            coverage: &coverage,
            resolutions: &[],
        },
    )?;
    let Resolution::Conflict {
        stage,
        base,
        target: ours,
        incoming,
        paths,
    } = plan.resolution
    else {
        panic!("Conflict was incorrectly accepted")
    };
    assert_eq!(stage, ConflictStage::Overlay);
    assert_eq!(base.parse::<ObjectId>()?, f.repo.tree_id(a.sha.parse()?)?);
    assert_eq!(ours.parse::<ObjectId>()?, f.repo.tree_id(target.parse()?)?);
    assert_eq!(
        incoming.parse::<ObjectId>()?,
        f.repo.tree_id(before.parse()?)?
    );
    assert_eq!(paths, ["a"]);
    assert!(
        Plan::prepare(
            &f.repo,
            &f.graph,
            intercat_daemon::realign::Input {
                track: "A",
                basis: &basis,
                before: before.parse()?,
                target: target.parse()?,
                coverage: &Ancestors::new(),
                resolutions: &[]
            }
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn inherited_baseline_conflict_preserves_the_actual_synthesis_inputs() -> Result<()> {
    let mut f = Fixture::new()?;
    let root = Basis::root(f.start.clone());
    let p = f.point("P", &[("shared.txt", "P")], &root)?;
    let q = f.point("Q", &[("shared.txt", "Q")], &root)?;
    let inherits = Ancestors::from([("P".into(), p.inherited()), ("Q".into(), q.inherited())]);
    let basis = Basis {
        views: Vec::new(),
        parents: inherits.clone(),
        base: f.start.clone(),
        base_inherits: Ancestors::new(),
        inherits: inherits.clone(),
    };
    let before = f.commit(
        &[("shared.txt", "C current"), ("own.txt", "C only")],
        &f.start,
    )?;
    let target = f.commit(&[("shared.txt", "Published resolution")], &f.start)?;
    let head = std::fs::read(f.repo.common_directory()?.join("HEAD"))?;
    let plan = Plan::prepare(
        &f.repo,
        &f.graph,
        intercat_daemon::realign::Input {
            track: "C",
            basis: &basis,
            before: before.parse()?,
            target: target.parse()?,
            coverage: &inherits,
            resolutions: &[],
        },
    )?;
    let Resolution::Conflict {
        stage,
        base,
        target: ours,
        incoming,
        paths,
    } = &plan.resolution
    else {
        panic!("Expected a baseline conflict")
    };
    assert_eq!(*stage, ConflictStage::Baseline);
    assert_eq!(paths, &["shared.txt"]);
    assert_eq!(base.parse::<ObjectId>()?, f.repo.tree_id(f.start.parse()?)?);
    assert_eq!(ours.parse::<ObjectId>()?, f.repo.tree_id(p.sha.parse()?)?);
    assert_eq!(
        incoming.parse::<ObjectId>()?,
        f.repo.tree_id(q.sha.parse()?)?
    );
    assert_ne!(
        incoming.parse::<ObjectId>()?,
        f.repo.tree_id(before.parse()?)?
    );
    assert_eq!(plan.before, before);
    assert_eq!(plan.target, target);
    assert_eq!(plan.before_basis, basis);
    assert_eq!(
        serde_json::from_value::<Plan>(serde_json::to_value(&plan)?)?,
        plan
    );
    assert_eq!(
        std::fs::read(f.repo.common_directory()?.join("HEAD"))?,
        head
    );
    Ok(())
}

#[test]
fn saved_baseline_and_overlay_resolutions_replay_only_for_their_frozen_trees() -> Result<()> {
    use intercat_daemon::{merge_choices::Solved, realign::Input};
    let mut f = Fixture::new()?;
    let root = Basis::root(f.start.clone());
    let p = f.point("P", &[("shared.txt", "P")], &root)?;
    let q = f.point("Q", &[("shared.txt", "Q")], &root)?;
    let coverage = Ancestors::from([("P".into(), p.inherited()), ("Q".into(), q.inherited())]);
    let basis = Basis {
        views: Vec::new(),
        parents: coverage.clone(),
        base: f.start.clone(),
        base_inherits: Ancestors::new(),
        inherits: coverage.clone(),
    };
    let before = f.commit(
        &[("shared.txt", "C current"), ("own.txt", "C only")],
        &f.start,
    )?;
    let target = f.commit(&[("shared.txt", "Published")], &f.start)?;
    let prepare = |saved: &[Solved]| {
        Plan::prepare(
            &f.repo,
            &f.graph,
            Input {
                track: "C",
                basis: &basis,
                before: before.parse().unwrap(),
                target: target.parse().unwrap(),
                coverage: &coverage,
                resolutions: saved,
            },
        )
    };
    let initial = prepare(&[])?;
    let Resolution::Conflict {
        stage,
        base,
        target: ours,
        incoming,
        ..
    } = initial.resolution
    else {
        panic!("Expected a baseline conflict")
    };
    assert_eq!(stage, ConflictStage::Baseline);
    let synthesized = f.commit(&[("shared.txt", "Synthesized baseline")], &f.start)?;
    let first = Solved {
        base,
        target: ours,
        incoming,
        tree: f.repo.tree_id(synthesized.parse()?)?.to_string(),
    };
    let overlay = prepare(std::slice::from_ref(&first))?;
    let Resolution::Conflict {
        stage,
        base,
        target: ours,
        incoming,
        paths,
    } = overlay.resolution
    else {
        panic!("Expected an overlay conflict")
    };
    assert_eq!(stage, ConflictStage::Overlay);
    assert_eq!(paths, ["shared.txt"]);
    assert_eq!(base, first.tree);
    assert_eq!(incoming, f.repo.tree_id(before.parse()?)?.to_string());
    let resolved = f.commit(
        &[("shared.txt", "C resolved"), ("own.txt", "C only")],
        &target,
    )?;
    let second = Solved {
        base,
        target: ours,
        incoming,
        tree: f.repo.tree_id(resolved.parse()?)?.to_string(),
    };
    let choices = vec![first.clone(), second];
    let ready = prepare(&choices)?;
    assert_eq!(
        f.files(&ready)?,
        [
            ("shared.txt".into(), "C resolved".into()),
            ("own.txt".into(), "C only".into())
        ]
        .into()
    );
    assert_eq!(ready.resolutions, choices);
    assert_eq!(prepare(&choices)?, ready);
    assert!(prepare(&[first.clone(), first.clone()]).is_err());
    let mut stale = first.clone();
    stale.incoming = f.repo.tree_id(before.parse()?)?.to_string();
    assert!(prepare(&[stale]).is_err());
    let mut invalid = first;
    invalid.tree = f.repo.write_blob(b"Not a tree")?.to_string();
    assert!(prepare(&[invalid]).is_err());
    Ok(())
}

#[tokio::test]
async fn realignment_resolves_in_shared_private_files_without_a_commit_job() -> Result<()> {
    use intercat_daemon::{
        checkout::{Checkout, Configuration},
        merge_choices::Solved,
        merge_workspace,
        realign::Input,
    };
    use sha2::{Digest, Sha256};
    let f = Fixture::new()?;
    let base = f.commit(&[("shared.txt", "base\n")], &f.start)?;
    let before = f.commit(
        &[("shared.txt", "worker\n"), ("own.txt", "worker only\n")],
        &base,
    )?;
    let target = f.commit(&[("shared.txt", "published\n")], &base)?;
    let basis = Basis::root(base.clone());
    let coverage = Ancestors::new();
    let plan = Plan::prepare(
        &f.repo,
        &f.graph,
        Input {
            track: "dummy-worker",
            basis: &basis,
            before: before.parse()?,
            target: target.parse()?,
            coverage: &coverage,
            resolutions: &[],
        },
    )?;
    let Resolution::Conflict {
        base: merge_base,
        target: merge_target,
        incoming,
        paths,
        ..
    } = &plan.resolution
    else {
        panic!("Expected a realignment conflict")
    };
    let root = tempfile::tempdir()?;
    let source_root = root.path().join("source");
    gix::init(&source_root)?;
    let source_repo = Repository::open(&source_root)?;
    for sha in [&f.start, &base, &target] {
        source_repo.import_commit(&f.repo, sha.parse()?)?;
    }
    source_repo.compare_and_swap("refs/intercat/dummy/realign-source", None, target.parse()?)?;
    source_repo.sync_objects(&[target.parse()?])?;
    let saved_head = std::fs::read(source_repo.common_directory()?.join("HEAD"))?;
    let saved_index = std::fs::read(source_repo.common_directory()?.join("index")).ok();
    let worker = root.path().join("dummy-worker");
    std::fs::create_dir(&worker)?;
    std::fs::write(worker.join("shared.txt"), "worker\n")?;
    let source = Repository::worktree_source(&source_root, Some(target.parse()?))?;
    let original = Checkout::create(
        &root.path().join("original"),
        source.clone(),
        target.parse()?,
        Configuration::capture(&source)?,
        "/usr/bin:/bin".into(),
        vec![],
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    let original_repo = Repository::open(&original.workdir)?;
    for tree in [merge_base, merge_target, incoming] {
        original_repo.import_tree(f.root.path(), tree.parse()?)?;
    }
    let frozen = merge_workspace::Input {
        binding: Sha256::digest(serde_json::to_vec(&plan)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        track: "dummy-worker".into(),
        head: target.clone(),
        base: merge_base.clone(),
        target: merge_target.clone(),
        incoming: incoming.clone(),
        conflicts: paths.clone(),
        marker: uuid::Uuid::now_v7(),
    };
    let directory = root.path().join("merge");
    let files = merge_workspace::Workspace::prepare(
        &original,
        &directory,
        frozen.clone(),
        "/usr/bin:/bin",
        &[],
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert!(
        std::fs::read_to_string(files.checkout.workdir.join("shared.txt"))?
            .contains(&frozen.marker.to_string())
    );
    let mut wrong = frozen.clone();
    wrong.binding = "different realignment".into();
    assert!(
        merge_workspace::Workspace::resume(&original, &directory, wrong, "/usr/bin:/bin", &[])
            .await
            .is_err()
    );
    let files =
        merge_workspace::Workspace::resume(&original, &directory, frozen, "/usr/bin:/bin", &[])
            .await?;
    std::fs::write(
        files.checkout.workdir.join("shared.txt"),
        "chosen resolution\n",
    )?;
    let staged = files
        .checkout
        .git(
            &["add".into(), "--".into(), "shared.txt".into()],
            true,
            tokio::sync::watch::channel(false).1,
        )
        .await?;
    assert!(staged.success());
    let result = files.finish(tokio::sync::watch::channel(false).1).await?;
    assert!(result.left_out.is_empty());
    f.repo
        .import_tree(&files.checkout.workdir, result.tree.parse()?)?;
    let choices = [Solved {
        base: merge_base.clone(),
        target: merge_target.clone(),
        incoming: incoming.clone(),
        tree: result.tree,
    }];
    let ready = Plan::prepare(
        &f.repo,
        &f.graph,
        Input {
            track: "dummy-worker",
            basis: &basis,
            before: before.parse()?,
            target: target.parse()?,
            coverage: &coverage,
            resolutions: &choices,
        },
    )?;
    let actual = f.files(&ready)?;
    assert_eq!(actual["shared.txt"], "chosen resolution\n");
    assert_eq!(actual["own.txt"], "worker only\n");
    assert_eq!(
        std::fs::read_to_string(worker.join("shared.txt"))?,
        "worker\n"
    );
    assert_eq!(
        std::fs::read(source_repo.common_directory()?.join("HEAD"))?,
        saved_head
    );
    assert_eq!(
        std::fs::read(source_repo.common_directory()?.join("index")).ok(),
        saved_index
    );
    Ok(())
}
