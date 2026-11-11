use intercat_daemon::{
    Result,
    integration_record::Record,
    lineage::{Ancestors, Basis, Frozen, Point},
    merge_choices::{Merges, Solved},
    path_restore::{self, Conflict, Inputs, Outcome, Stage},
};
use intercat_git::{Merge, ObjectId, Repository};
use uuid::Uuid;

struct Fixture {
    _directory: tempfile::TempDir,
    repo: Repository,
    root: ObjectId,
    graph: Frozen,
}
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        Repository::init_isolated(directory.path())?;
        let repo = Repository::open(directory.path())?;
        let empty = repo.empty_starting_point()?;
        let mut fixture = Self {
            _directory: directory,
            repo,
            root: empty,
            graph: Frozen::default(),
        };
        fixture.root = fixture.commit(files, empty)?;
        Ok(fixture)
    }
    fn tree(&self, files: &[(&str, &str)]) -> Result<ObjectId> {
        let files = files
            .iter()
            .map(|(name, body)| Ok((*name, self.repo.write_blob(body.as_bytes())?)))
            .collect::<Result<Vec<_>>>()?;
        self.repo.write_tree(&files)
    }
    fn commit(&self, files: &[(&str, &str)], parent: ObjectId) -> Result<ObjectId> {
        self.repo
            .checkpoint_commit(self.tree(files)?, parent, "Own dummy Restore files")
    }
    fn point(&mut self, track: &str, number: u64, sha: ObjectId, basis: &Basis) -> Point {
        let point = Point {
            views: basis.views.clone(),
            parents: basis.parents.clone(),
            track: track.into(),
            number,
            sha: sha.to_string(),
            base_at: basis.base.clone(),
            base_inherits_at: basis.base_inherits.clone(),
            inherits: basis.inherits.clone(),
        };
        self.graph.points.push(point.clone());
        point
    }
    fn receipt(
        &self,
        point: &Point,
        parent: ObjectId,
        sha: ObjectId,
        position: u64,
        before: Ancestors,
    ) -> Result<Record> {
        let job_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, &position.to_be_bytes());
        let mut after = before.clone();
        after.insert(point.track.clone(), point.inherited());
        let record = Record {
            id: Uuid::new_v5(&job_id, sha.to_string().as_bytes()),
            job_id,
            operation: Uuid::new_v5(&job_id, b"publication"),
            session_id: "own-dummy-session".into(),
            folder_id: "own-dummy-folder".into(),
            track_id: point.track.clone(),
            checkpoint: point.inherited(),
            ordinal: position,
            position,
            reference: format!("refs/intercat/dummy/integrations/{position}"),
            sha: sha.to_string(),
            parent: parent.to_string(),
            tree: self.repo.tree_id(sha)?.to_string(),
            coverage_before: before,
            coverage_after: after,
        };
        self.repo.compare_and_swap(&record.reference, None, sha)?;
        record.validate()?;
        Ok(record)
    }
    fn names(&self, tree: &str) -> Result<Vec<String>> {
        Ok(self.repo.tree_files(tree.parse()?)?.into_keys().collect())
    }
}
fn prepared(f: &Fixture, inputs: &Inputs) -> Result<path_restore::Prepared> {
    match path_restore::prepare(&f.repo, &f.graph, inputs)? {
        Outcome::Prepared(prepared) => Ok(*prepared),
        Outcome::Conflict(conflict) => {
            Err(format!("Unexpected Restore conflict: {conflict:?}").into())
        }
    }
}
fn conflict(f: &Fixture, inputs: &Inputs) -> Result<Conflict> {
    match path_restore::prepare(&f.repo, &f.graph, inputs)? {
        Outcome::Conflict(conflict) => Ok(*conflict),
        Outcome::Prepared(_) => Err("Expected a frozen Restore conflict.".into()),
    }
}
fn solve(conflict: &Conflict, tree: ObjectId) -> Solved {
    Solved {
        base: conflict.base.clone(),
        target: conflict.target.clone(),
        incoming: conflict.incoming.clone(),
        tree: tree.to_string(),
    }
}

#[test]
fn restore_returns_cancelled_own_edits_without_reviving_deleted_borrowed_files() -> Result<()> {
    let mut f = Fixture::new(&[])?;
    let first = f.commit(&[("prefix", "A first")], f.root)?;
    let a1 = f.point("A", 1, first, &Basis::root(f.root.to_string()));
    let x = f.commit(&[("prefix", "A first"), ("x", "X borrowed")], first)?;
    let x1 = f.point("X", 1, x, &Basis::root(first.to_string()));
    let borrowed_coverage =
        Ancestors::from([("A".into(), a1.inherited()), ("X".into(), x1.inherited())]);
    let borrowed = Basis {
        base_inherits: borrowed_coverage.clone(),
        ..Basis::root(x.to_string())
    };
    let own = f.commit(
        &[("prefix", "A first"), ("x", "X borrowed"), ("g", "A later")],
        x,
    )?;
    let a2 = f.point("A", 2, own, &borrowed);
    let record = f.receipt(&a2, x, own, 3, borrowed_coverage.clone())?;
    f.graph.integrated = record.coverage_after.clone();
    let before = f.commit(
        &[
            ("prefix", "A first"),
            ("x", "X borrowed"),
            ("g", "A later"),
            ("pending", "A not published yet"),
        ],
        own,
    )?;
    // Replacement cancels A@2. B then removes X and adds independent work.
    let target = f.commit(&[("prefix", "A first"), ("b", "B independent")], x)?;
    let b1 = f.point("B", 1, target, &Basis::root(x.to_string()));
    let inputs = Inputs {
        track: "A".into(),
        before: before.to_string(),
        basis: Basis {
            base_inherits: record.coverage_after.clone(),
            ..Basis::root(own.to_string())
        },
        target: target.to_string(),
        coverage: Ancestors::from([
            ("A".into(), a1.inherited()),
            ("X".into(), x1.inherited()),
            ("B".into(), b1.inherited()),
        ]),
        prefix: Ancestors::new(),
        cancelled: vec![record.clone()],
        resolutions: vec![],
    };
    let restored = prepared(&f, &inputs)?;
    assert_eq!(f.names(&restored.tree)?, ["b", "g", "pending", "prefix"]);
    assert_eq!(restored.integrated, Some(a1.inherited()));
    assert_eq!(restored.reversed, vec![record.id]);
    assert_eq!(restored.basis.base, inputs.target);
    assert_eq!(restored.basis.base_inherits, inputs.coverage);
    restored.verify(&f.repo, &f.graph)?;

    // The next Commit must also see g and pending as A's own changes.
    f.graph.integrated.insert("A".into(), a1.inherited());
    let checkpoint = f.repo.checkpoint_commit(
        restored.tree.parse()?,
        before,
        "Own dummy restored checkpoint",
    )?;
    let a3 = f.point("A", 3, checkpoint, &restored.basis);
    let mut merges = Merges::new(&f.repo, &[])?;
    let Merge::Clean(baseline) =
        f.graph
            .track_baseline(&mut merges, &a3, &a3.basis(), &f.graph.integrated)?
    else {
        panic!("Restored baseline must be ready for Commit")
    };
    merges.finish()?;
    assert_eq!(baseline, f.repo.tree_id(target)?);
    let Merge::Clean(next) =
        f.repo
            .merge_trees(baseline, f.repo.tree_id(target)?, restored.tree.parse()?)?
    else {
        panic!("Restored own edits must publish cleanly")
    };
    assert_eq!(f.names(&next.to_string())?, ["b", "g", "pending", "prefix"]);
    Ok(())
}

#[test]
fn restore_keeps_a_targets_deleted_prefix_when_one_receipt_crosses_the_fork() -> Result<()> {
    for continued in [false, true] {
        let mut f = Fixture::new(&[])?;
        let first = f.commit(&[("shared", "A shared prefix")], f.root)?;
        let a1 = f.point("A", 1, first, &Basis::root(f.root.to_string()));
        let last = f.commit(
            &[("shared", "A shared prefix"), ("tail", "A later work")],
            first,
        )?;
        let a3 = f.point("A", 3, last, &Basis::root(f.root.to_string()));
        let published = f.repo.checkpoint_commit(
            f.repo.tree_id(last)?,
            f.root,
            "Own publication includes the fork and later changes",
        )?;
        let record = f.receipt(&a3, f.root, published, 1, Ancestors::new())?;
        f.graph.integrated = record.coverage_after.clone();
        let before = f.commit(
            &[
                ("shared", "A shared prefix"),
                ("tail", "A later work"),
                ("own", "Pending own correction"),
            ],
            last,
        )?;
        // Replacement retained A@1. Independent work then deleted its file.
        let target = f.commit(&[("other", "Independent work")], first)?;
        let inputs = Inputs {
            track: if continued { "continued" } else { "A" }.into(),
            before: before.to_string(),
            basis: Basis {
                parents: if continued {
                    Ancestors::from([("A".into(), a3.inherited())])
                } else {
                    Ancestors::new()
                },
                inherits: if continued {
                    Ancestors::from([("A".into(), a3.inherited())])
                } else {
                    Ancestors::new()
                },
                base_inherits: record.coverage_after.clone(),
                ..Basis::root(last.to_string())
            },
            target: target.to_string(),
            coverage: Ancestors::from([("A".into(), a1.inherited())]),
            prefix: if continued {
                Ancestors::from([("A".into(), a3.inherited())])
            } else {
                Ancestors::new()
            },
            cancelled: vec![record.clone()],
            resolutions: vec![],
        };
        let restored = prepared(&f, &inputs)?;
        assert_eq!(f.names(&restored.tree)?, ["other", "own", "tail"]);
        assert_eq!(restored.reversed, [record.id]);
        restored.verify(&f.repo, &f.graph)?;
        assert_eq!(f.repo.reference_target(&record.reference)?, Some(published));
        assert_eq!(f.graph.integrated["A"], a3.inherited());
    }
    Ok(())
}

#[test]
fn restoring_all_cancelled_publications_resets_only_the_own_cursor() -> Result<()> {
    let mut f = Fixture::new(&[])?;
    let first = f.commit(&[("one", "A first")], f.root)?;
    let a1 = f.point("A", 1, first, &Basis::root(f.root.to_string()));
    let r1 = f.receipt(&a1, f.root, first, 1, Ancestors::new())?;
    let second = f.commit(&[("one", "A first"), ("two", "A second")], first)?;
    let a2 = f.point("A", 2, second, &Basis::root(first.to_string()));
    let r2 = f.receipt(&a2, first, second, 2, r1.coverage_after.clone())?;
    f.graph.integrated = r2.coverage_after.clone();
    let target = f.commit(&[("other", "B independent")], f.root)?;
    let inputs = Inputs {
        track: "A".into(),
        before: second.to_string(),
        basis: Basis {
            base_inherits: r2.coverage_after.clone(),
            ..Basis::root(second.to_string())
        },
        target: target.to_string(),
        coverage: Ancestors::new(),
        prefix: Ancestors::new(),
        cancelled: vec![r2.clone(), r1.clone()],
        resolutions: vec![],
    };
    let restored = prepared(&f, &inputs)?;
    assert_eq!(f.names(&restored.tree)?, ["one", "other", "two"]);
    assert_eq!(restored.integrated, None);
    assert_eq!(restored.reversed, [r2.id, r1.id]);
    // Planning is pure: receipts, protected refs and the published cursor persist.
    assert_eq!(f.graph.integrated["A"], a2.inherited());
    assert_eq!(f.repo.reference_target(&r1.reference)?, Some(first));
    assert_eq!(f.repo.reference_target(&r2.reference)?, Some(second));
    let mut bad = inputs.clone();
    bad.cancelled.reverse();
    assert!(path_restore::prepare(&f.repo, &f.graph, &bad).is_err());
    bad = inputs.clone();
    bad.cancelled.push(r1);
    assert!(path_restore::prepare(&f.repo, &f.graph, &bad).is_err());
    Ok(())
}

fn conflicting_fixture() -> Result<(Fixture, Inputs)> {
    let mut f = Fixture::new(&[("f", "original\n")])?;
    let own = f.commit(&[("f", "A published\n")], f.root)?;
    let a1 = f.point("A", 1, own, &Basis::root(f.root.to_string()));
    let record = f.receipt(&a1, f.root, own, 1, Ancestors::new())?;
    f.graph.integrated = record.coverage_after.clone();
    let borrowed = f.commit(&[("f", "later borrowed\n")], own)?;
    let before = f.commit(&[("f", "A pending\n")], borrowed)?;
    let target = f.commit(&[("f", "B independent\n")], f.root)?;
    let inputs = Inputs {
        track: "A".into(),
        before: before.to_string(),
        basis: Basis {
            base_inherits: record.coverage_after.clone(),
            ..Basis::root(borrowed.to_string())
        },
        target: target.to_string(),
        coverage: Ancestors::new(),
        prefix: Ancestors::new(),
        cancelled: vec![record],
        resolutions: vec![],
    };
    Ok((f, inputs))
}

#[test]
fn restore_freezes_each_conflict_and_reuses_choices_across_undo_and_file_stages() -> Result<()> {
    let (f, mut inputs) = conflicting_fixture()?;
    let undo = conflict(&f, &inputs)?;
    assert_eq!(undo.stage, Stage::Undo);
    assert_eq!(undo.paths, ["f"]);
    assert!(undo.reversed.is_empty());
    let original = f.repo.tree_id(f.root)?;
    inputs.resolutions.push(solve(&undo, original));
    let files = conflict(&f, &inputs)?;
    assert_eq!(files.stage, Stage::Files);
    assert_eq!(files.paths, ["f"]);
    assert_eq!(files.reversed, [inputs.cancelled[0].id]);
    assert_ne!(files.base, undo.base);
    let resolved = f.tree(&[("f", "A and B explicitly resolved\n")])?;
    inputs.resolutions.push(solve(&files, resolved));
    let saved = prepared(&f, &inputs)?;
    assert_eq!(saved.tree, resolved.to_string());
    saved.verify(&f.repo, &f.graph)?;
    let mut changed = saved.clone();
    changed.tree = original.to_string();
    assert!(changed.verify(&f.repo, &f.graph).is_err());
    let mut wrong = inputs.clone();
    wrong.resolutions[1].target = original.to_string();
    assert!(path_restore::prepare(&f.repo, &f.graph, &wrong).is_err());
    wrong = inputs.clone();
    wrong.resolutions.push(wrong.resolutions[0].clone());
    assert!(path_restore::prepare(&f.repo, &f.graph, &wrong).is_err());
    wrong = inputs.clone();
    wrong.target = f.root.to_string();
    assert!(path_restore::prepare(&f.repo, &f.graph, &wrong).is_err());
    Ok(())
}

#[test]
fn restore_rejects_another_owner_changed_checkpoint_and_unprotected_receipt() -> Result<()> {
    let (f, inputs) = conflicting_fixture()?;
    let mut changed = inputs.clone();
    changed.track = "B".into();
    assert!(path_restore::prepare(&f.repo, &f.graph, &changed).is_err());
    changed = inputs.clone();
    changed.cancelled[0].checkpoint.sha = f.root.to_string();
    assert!(path_restore::prepare(&f.repo, &f.graph, &changed).is_err());
    changed = inputs.clone();
    changed.coverage.insert(
        "A".into(),
        intercat_daemon::lineage::Inherited {
            number: 1,
            sha: f.root.to_string(),
        },
    );
    assert!(path_restore::prepare(&f.repo, &f.graph, &changed).is_err());
    let record = &inputs.cancelled[0];
    f.repo
        .compare_and_swap(&record.reference, Some(record.sha.parse()?), f.root)?;
    assert!(path_restore::prepare(&f.repo, &f.graph, &inputs).is_err());
    Ok(())
}

#[test]
fn restore_uses_the_last_published_cursor_instead_of_an_unpublished_checkpoint() -> Result<()> {
    let mut f = Fixture::new(&[])?;
    let first = f.commit(&[("one", "A first")], f.root)?;
    let a1 = f.point("A", 1, first, &Basis::root(f.root.to_string()));
    let r1 = f.receipt(&a1, f.root, first, 1, Ancestors::new())?;
    let middle = f.commit(&[("one", "A first"), ("two", "not published alone")], first)?;
    f.point("A", 2, middle, &Basis::root(first.to_string()));
    let latest = f.commit(
        &[
            ("one", "A first"),
            ("two", "not published alone"),
            ("three", "A latest"),
        ],
        middle,
    )?;
    let a3 = f.point("A", 3, latest, &Basis::root(first.to_string()));
    let published = f.repo.checkpoint_commit(
        f.repo.tree_id(latest)?,
        first,
        "Own dummy publication skips the unpublished checkpoint",
    )?;
    let r3 = f.receipt(&a3, first, published, 2, r1.coverage_after.clone())?;
    f.graph.integrated = r3.coverage_after.clone();
    let inputs = Inputs {
        track: "A".into(),
        before: latest.to_string(),
        basis: Basis {
            base_inherits: r3.coverage_after.clone(),
            ..Basis::root(published.to_string())
        },
        target: first.to_string(),
        coverage: r1.coverage_after,
        prefix: Ancestors::new(),
        cancelled: vec![r3],
        resolutions: vec![],
    };
    let restored = prepared(&f, &inputs)?;
    assert_eq!(restored.integrated, Some(a1.inherited()));
    assert_eq!(f.names(&restored.tree)?, ["one", "three", "two"]);
    Ok(())
}

#[test]
fn restore_preserves_dependency_choices_when_the_baseline_itself_conflicts() -> Result<()> {
    let mut f = Fixture::new(&[("f", "original\n")])?;
    let own = f.commit(&[("f", "original\n"), ("g", "A own")], f.root)?;
    let a1 = f.point("A", 1, own, &Basis::root(f.root.to_string()));
    let receipt = f.receipt(&a1, f.root, own, 1, Ancestors::new())?;
    let x = f.commit(&[("f", "X version\n")], f.root)?;
    let x1 = f.point("X", 1, x, &Basis::root(f.root.to_string()));
    let y = f.commit(&[("f", "Y version\n")], f.root)?;
    let y1 = f.point("Y", 1, y, &Basis::root(f.root.to_string()));
    let parents = Ancestors::from([("X".into(), x1.inherited()), ("Y".into(), y1.inherited())]);
    f.graph.integrated = parents.clone();
    f.graph.integrated.insert("A".into(), a1.inherited());
    let before = f.commit(
        &[
            ("f", "combined\n"),
            ("g", "A own"),
            ("pending", "A pending"),
        ],
        own,
    )?;
    let target = f.commit(&[("f", "combined\n"), ("other", "independent")], f.root)?;
    let mut inputs = Inputs {
        track: "A".into(),
        before: before.to_string(),
        basis: Basis {
            parents: parents.clone(),
            inherits: parents.clone(),
            base_inherits: receipt.coverage_after.clone(),
            ..Basis::root(own.to_string())
        },
        target: target.to_string(),
        coverage: parents,
        prefix: Ancestors::new(),
        cancelled: vec![receipt],
        resolutions: vec![],
    };
    let baseline = conflict(&f, &inputs)?;
    assert_eq!(baseline.stage, Stage::Baseline);
    assert_eq!(baseline.paths, ["f"]);
    inputs.resolutions.push(solve(
        &baseline,
        f.tree(&[("f", "combined\n"), ("g", "A own")])?,
    ));
    let restored = prepared(&f, &inputs)?;
    assert_eq!(f.names(&restored.tree)?, ["f", "g", "other", "pending"]);
    assert_eq!(restored.basis.parents, inputs.basis.parents);
    assert_eq!(restored.basis.inherits, inputs.basis.inherits);
    assert_eq!(restored.basis.views, inputs.basis.views);
    Ok(())
}
