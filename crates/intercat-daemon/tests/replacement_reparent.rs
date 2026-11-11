use intercat_daemon::{
    Result,
    lineage::{Frozen, Point, include},
    replacement_reparent::{self, Destination, Inputs, Outcome, Stage},
};
use intercat_git::{Merge, ObjectId, Repository};

#[path = "support/shared_context_planning.rs"]
mod shared_context_planning;

struct Fixture {
    _root: tempfile::TempDir,
    repo: Repository,
    root: ObjectId,
    points: std::cell::RefCell<Vec<Point>>,
}

#[test]
fn reparenting_preserves_borrowed_base_coverage_and_a_restored_own_boundary() -> Result<()> {
    for restored in [false, true] {
        let f = Fixture::new()?;
        let old = f.point("A", 3, &[("retired", "A later")], &[])?;
        let new = f.point("A2", 1, &[("chosen", "A branch")], &[])?;
        let borrowed = f.point("W", 1, &[("borrowed", "W shared")], &[])?;
        let integrated = f.point(
            "B",
            1,
            &[("retired", "A later"), ("own", "B committed")],
            &[&old],
        )?;
        let mut child = f.point(
            "B",
            2,
            &[
                ("retired", "A later"),
                ("own", "B committed"),
                ("borrowed", "W shared"),
                ("pending", "B pending"),
            ],
            &[&old],
        )?;
        child.base_at = f
            .repo
            .checkpoint_commit(
                f.tree(&[
                    ("retired", "A later"),
                    ("own", "B committed"),
                    ("borrowed", "W shared"),
                ])?,
                f.root,
                "Own borrowed baseline",
            )?
            .to_string();
        child.base_inherits_at = child.inherits.clone();
        include(&mut child.base_inherits_at, "B", &integrated.inherited())?;
        include(&mut child.base_inherits_at, "W", &borrowed.inherited())?;
        *f.points.borrow_mut().last_mut().unwrap() = child.clone();
        let inputs = Inputs {
            destination: Destination::Publication {
                coverage: Default::default(),
            },
            views: Vec::new(),
            worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
            checkpoint: child,
            inherited: old,
            replacement: new.clone(),
            integrated: (!restored).then_some(integrated.clone()),
            resolutions: vec![],
        };
        let Outcome::Prepared(prepared) =
            replacement_reparent::prepare(&f.repo, &f.graph(), &inputs)?
        else {
            panic!("Unrelated borrowed files should reparent");
        };
        assert_eq!(prepared.base_inherits.get("W"), Some(&borrowed.inherited()));
        assert_eq!(
            prepared.base_inherits.get("B"),
            Some(&integrated.inherited())
        );
        assert!(!prepared.base_inherits.contains_key("A"));
        assert_eq!(f.names(&prepared.base)?, ["borrowed", "chosen", "own"]);
        let mut point = f.point(
            "B",
            3,
            &[
                ("chosen", "A branch"),
                ("own", "B committed"),
                ("borrowed", "W shared"),
                ("pending", "B pending"),
            ],
            &[&new],
        )?;
        point.base_at = f
            .repo
            .checkpoint_commit(
                prepared.base.parse()?,
                new.sha.parse()?,
                "Own normalized borrowed baseline",
            )?
            .to_string();
        point.base_inherits_at = prepared.base_inherits.clone();
        *f.points.borrow_mut().last_mut().unwrap() = point.clone();
        let mut merges = intercat_daemon::merge_choices::Merges::new(&f.repo, &[])?;
        let Merge::Clean(base) = f.graph().track_baseline(
            &mut merges,
            &point,
            &point.basis(),
            &prepared.base_inherits,
        )?
        else {
            panic!("Saved own coverage should preserve the normalized base")
        };
        let target = f.tree(&[("chosen", "A branch"), ("borrowed", "W shared")])?;
        let Merge::Clean(result) = f
            .repo
            .merge_trees(base, target, prepared.worktree.parse()?)?
        else {
            panic!("The independently removed own file must stay removed")
        };
        assert_eq!(
            f.names(&result.to_string())?,
            ["borrowed", "chosen", "pending"]
        );
    }
    Ok(())
}

#[test]
fn a_never_integrated_descendant_keeps_borrowed_files_in_its_base_after_reparenting() -> Result<()>
{
    let f = Fixture::new()?;
    let old = f.point("A", 3, &[("retired", "A later")], &[])?;
    let new = f.point("A2", 1, &[("chosen", "A branch")], &[])?;
    let borrowed = f.point("W", 1, &[("borrowed", "W shared")], &[])?;
    let mut child = f.point(
        "B",
        1,
        &[
            ("retired", "A later"),
            ("borrowed", "W shared"),
            ("pending", "B pending"),
        ],
        &[&old],
    )?;
    child.base_at = borrowed.sha.clone();
    child
        .base_inherits_at
        .insert("W".into(), borrowed.inherited());
    *f.points.borrow_mut().last_mut().unwrap() = child.clone();
    let inputs = Inputs {
        destination: Destination::Publication {
            coverage: Default::default(),
        },
        views: Vec::new(),
        worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
        checkpoint: child,
        inherited: old,
        replacement: new,
        integrated: None,
        resolutions: vec![],
    };
    let Outcome::Prepared(prepared) = replacement_reparent::prepare(&f.repo, &f.graph(), &inputs)?
    else {
        panic!("Independent borrowed context should reparent")
    };
    assert_eq!(prepared.base_inherits.get("W"), Some(&borrowed.inherited()));
    let target = f.tree(&[("chosen", "A branch"), ("unrelated", "Another worker")])?;
    let Merge::Clean(result) =
        f.repo
            .merge_trees(prepared.base.parse()?, target, prepared.worktree.parse()?)?
    else {
        panic!("A later deletion must remain deleted")
    };
    assert_eq!(
        f.names(&result.to_string())?,
        ["chosen", "pending", "unrelated"],
        "borrowed context must not become B's addition when B has no own integrated checkpoint"
    );
    Ok(())
}
#[test]
fn an_unpublished_child_replaces_only_the_parent_part_recorded_in_its_borrowed_base() -> Result<()>
{
    for covered_through in 0..=3 {
        let f = Fixture::new()?;
        let fork = f.point("A", 1, &[("shared", "A fork")], &[])?;
        let middle = f.point("A", 2, &[("shared", "A fork"), ("middle", "A middle")], &[])?;
        let old = f.point(
            "A",
            3,
            &[
                ("shared", "A fork"),
                ("middle", "A middle"),
                ("later", "A later"),
            ],
            &[],
        )?;
        let new = f.point(
            "A-alt",
            1,
            &[("shared", "A fork"), ("chosen", "A branch")],
            &[&fork],
        )?;
        let borrowed = f.point("W", 1, &[("borrowed", "W shared")], &[])?;
        let mut child = f.point(
            "B",
            1,
            &[
                ("shared", "A fork"),
                ("middle", "A middle"),
                ("later", "A later"),
                ("borrowed", "W shared"),
                ("pending", "B pending"),
            ],
            &[&old],
        )?;
        let mut files = vec![("borrowed", "W shared")];
        for (number, path, text) in [
            (1, "shared", "A fork"),
            (2, "middle", "A middle"),
            (3, "later", "A later"),
        ] {
            if covered_through >= number {
                files.push((path, text));
            }
        }
        child.base_at = f
            .repo
            .checkpoint_commit(f.tree(&files)?, f.root, "Own partial borrowed base")?
            .to_string();
        child
            .base_inherits_at
            .insert("W".into(), borrowed.inherited());
        if covered_through > 0 {
            let mark = [&fork, &middle, &old][covered_through - 1].inherited();
            child.base_inherits_at.insert("A".into(), mark);
        }
        *f.points.borrow_mut().last_mut().unwrap() = child.clone();
        let inputs = Inputs {
            destination: Destination::Publication {
                coverage: Default::default(),
            },
            views: Vec::new(),
            worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
            checkpoint: child,
            inherited: old,
            replacement: new,
            integrated: None,
            resolutions: vec![],
        };
        let Outcome::Prepared(prepared) =
            replacement_reparent::prepare(&f.repo, &f.graph(), &inputs)?
        else {
            panic!("Partial parent coverage {covered_through} should be replaceable")
        };
        assert_eq!(f.names(&prepared.base)?, ["borrowed", "chosen", "shared"]);
        assert_eq!(prepared.base_inherits["A"], fork.inherited());
        assert_eq!(prepared.base_inherits["W"], borrowed.inherited());
        let target = f.tree(&[("chosen", "A branch"), ("shared", "A fork")])?;
        let Merge::Clean(result) =
            f.repo
                .merge_trees(prepared.base.parse()?, target, prepared.worktree.parse()?)?
        else {
            panic!("A target deletion should remain deleted")
        };
        assert_eq!(
            f.names(&result.to_string())?,
            ["chosen", "pending", "shared"]
        );
    }
    Ok(())
}

#[test]
fn a_direct_parent_keeps_its_earlier_own_files_after_its_base_advanced() -> Result<()> {
    let f = Fixture::new()?;
    let old = f.point("A", 3, &[("retired", "A later")], &[])?;
    let new = f.point("A-alt", 1, &[("chosen", "A chosen")], &[])?;
    let first = f.point("X", 1, &[("early", "X already published")], &[])?;
    let mut parent = f.point(
        "X",
        2,
        &[("early", "X already published"), ("later", "X new edit")],
        &[],
    )?;
    parent.base_at = first.sha.clone();
    parent
        .base_inherits_at
        .insert("X".into(), first.inherited());
    *f.points.borrow_mut().last_mut().unwrap() = parent.clone();
    let child = f.point(
        "B",
        1,
        &[
            ("retired", "A later"),
            ("early", "X already published"),
            ("later", "X new edit"),
            ("pending", "B pending"),
        ],
        &[&old, &parent],
    )?;
    let inputs = Inputs {
        destination: Destination::Publication {
            coverage: Default::default(),
        },
        views: Vec::new(),
        worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
        checkpoint: child,
        inherited: old,
        replacement: new,
        integrated: None,
        resolutions: vec![],
    };
    let Outcome::Prepared(prepared) = replacement_reparent::prepare(&f.repo, &f.graph(), &inputs)?
    else {
        panic!("Independent direct parents should compose")
    };
    assert_eq!(
        f.names(&prepared.base)?,
        ["chosen", "early", "later"],
        "the parent's previous own commit is still part of its complete checkpoint"
    );
    let target = f.tree(&[("chosen", "A chosen"), ("later", "X new edit")])?;
    let Merge::Clean(tree) =
        f.repo
            .merge_trees(prepared.base.parse()?, target, prepared.worktree.parse()?)?
    else {
        panic!("A target deletion must remain deleted")
    };
    assert_eq!(f.names(&tree.to_string())?, ["chosen", "later", "pending"]);
    Ok(())
}

#[test]
fn a_parent_with_a_normalized_own_base_keeps_borrowed_context_without_its_retired_parent()
-> Result<()> {
    let f = Fixture::new()?;
    let old = f.point("A", 3, &[("retired", "A later")], &[])?;
    let new = f.point("A-alt", 1, &[("chosen", "A branch")], &[])?;
    let borrowed = f.point("W", 1, &[("borrowed", "W shared")], &[])?;
    let first = f.point(
        "X",
        1,
        &[("retired", "A later"), ("early", "X published")],
        &[&old],
    )?;
    let base = f.repo.checkpoint_commit(
        f.tree(&[
            ("chosen", "A branch"),
            ("early", "X published"),
            ("borrowed", "W shared"),
        ])?,
        new.sha.parse()?,
        "Own normalized X base",
    )?;
    let mut parent = f.point(
        "X",
        2,
        &[
            ("chosen", "A branch"),
            ("early", "X published"),
            ("borrowed", "W shared"),
            ("later", "X pending"),
        ],
        &[&new],
    )?;
    parent.base_at = base.to_string();
    parent.base_inherits_at = parent.inherits.clone();
    parent
        .base_inherits_at
        .insert("X".into(), first.inherited());
    parent
        .base_inherits_at
        .insert("W".into(), borrowed.inherited());
    parent.sha = f
        .repo
        .checkpoint_commit(
            f.repo.tree_id(parent.sha.parse()?)?,
            base,
            "Own X checkpoint after reparent",
        )?
        .to_string();
    *f.points.borrow_mut().last_mut().unwrap() = parent.clone();
    let child = f.point(
        "B",
        1,
        &[
            ("retired", "A later"),
            ("chosen", "A branch"),
            ("early", "X published"),
            ("borrowed", "W shared"),
            ("later", "X pending"),
            ("pending", "B pending"),
        ],
        &[&old, &parent],
    )?;
    let inputs = Inputs {
        destination: Destination::Publication {
            coverage: Default::default(),
        },
        views: Vec::new(),
        worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
        checkpoint: child,
        inherited: old,
        replacement: new,
        integrated: None,
        resolutions: vec![],
    };
    let Outcome::Prepared(prepared) = replacement_reparent::prepare(&f.repo, &f.graph(), &inputs)?
    else {
        panic!("A normalized independent parent should compose")
    };
    assert_eq!(
        f.names(&prepared.base)?,
        ["borrowed", "chosen", "early", "later"]
    );
    assert!(!prepared.base_inherits.contains_key("A"));
    assert_eq!(prepared.base_inherits["X"], parent.inherited());
    assert_eq!(
        prepared.base_inherits.get("W"),
        Some(&borrowed.inherited()),
        "a full parent's borrowed base is part of the new base coverage"
    );
    let target = f.tree(&[("chosen", "A branch"), ("later", "X pending")])?;
    let Merge::Clean(tree) =
        f.repo
            .merge_trees(prepared.base.parse()?, target, prepared.worktree.parse()?)?
    else {
        panic!("Deleted own and borrowed files must stay deleted")
    };
    assert_eq!(f.names(&tree.to_string())?, ["chosen", "later", "pending"]);
    Ok(())
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        Repository::init_isolated(root.path())?;
        let repo = Repository::open(root.path())?;
        let base = repo.empty_starting_point()?;
        Ok(Self {
            _root: root,
            repo,
            root: base,
            points: Default::default(),
        })
    }
    fn tree(&self, files: &[(&str, &str)]) -> Result<ObjectId> {
        let blobs = files
            .iter()
            .map(|(path, text)| Ok((*path, self.repo.write_blob(text.as_bytes())?)))
            .collect::<Result<Vec<_>>>()?;
        self.repo.write_tree(&blobs)
    }
    fn point(
        &self,
        track: &str,
        number: u64,
        files: &[(&str, &str)],
        parents: &[&Point],
    ) -> Result<Point> {
        let tree = self.tree(files)?;
        let sha =
            self.repo
                .checkpoint_commit(tree, self.root, &format!("Own dummy {track}@{number}"))?;
        let mut inherits = Default::default();
        for parent in parents {
            for (track, mark) in &parent.inherits {
                include(&mut inherits, track, mark)?;
            }
            include(&mut inherits, &parent.track, &parent.inherited())?;
        }
        let point = Point {
            views: Vec::new(),
            parents: parents
                .iter()
                .map(|point| (point.track.clone(), point.inherited()))
                .collect(),
            track: track.into(),
            number,
            sha: sha.to_string(),
            base_at: self.root.to_string(),
            base_inherits_at: Default::default(),
            inherits,
        };
        self.points.borrow_mut().push(point.clone());
        Ok(point)
    }
    fn graph(&self) -> Frozen {
        Frozen {
            points: self.points.borrow().clone(),
            integrated: Default::default(),
        }
    }
    fn names(&self, tree: &str) -> Result<Vec<String>> {
        Ok(self.repo.tree_files(tree.parse()?)?.into_keys().collect())
    }
}

#[test]
fn reparent_keeps_own_integrated_edits_in_base_so_later_deletions_do_not_reappear() -> Result<()> {
    let f = Fixture::new()?;
    let old = f.point("A", 3, &[("common", "shared"), ("retired", "A later")], &[])?;
    let new = f.point(
        "A2",
        1,
        &[("common", "shared"), ("replacement", "A branch")],
        &[],
    )?;
    let integrated = f.point(
        "B",
        1,
        &[
            ("common", "shared"),
            ("retired", "A later"),
            ("own", "B committed"),
        ],
        &[&old],
    )?;
    let child = f.point(
        "B",
        2,
        &[
            ("common", "shared"),
            ("retired", "A later"),
            ("own", "B committed"),
            ("pending", "B pending"),
        ],
        &[&old],
    )?;
    let inputs = Inputs {
        destination: Destination::Publication {
            coverage: Default::default(),
        },
        views: Vec::new(),
        resolutions: vec![],
        checkpoint: child,
        inherited: old,
        replacement: new,
        worktree: f
            .tree(&[
                ("common", "shared"),
                ("retired", "A later"),
                ("own", "B committed"),
                ("pending", "B pending"),
                ("dirty", "B unsaved"),
            ])?
            .to_string(),
        integrated: Some(integrated),
    };
    let Outcome::Prepared(plan) = replacement_reparent::prepare(&f.repo, &f.graph(), &inputs)?
    else {
        panic!("Independent edits should reparent");
    };
    assert_eq!(
        f.names(&plan.worktree)?,
        ["common", "dirty", "own", "pending", "replacement"]
    );
    assert_eq!(f.names(&plan.base)?, ["common", "own", "replacement"]);
    plan.verify(&f.repo, &f.graph())?;
    let basis = intercat_daemon::lineage::Basis {
        views: Vec::new(),
        base: f
            .repo
            .checkpoint_commit(
                plan.base.parse()?,
                inputs.replacement.sha.parse()?,
                "Own B reparented baseline",
            )?
            .to_string(),
        parents: plan.parents.clone(),
        base_inherits: plan.base_inherits.clone(),
        inherits: plan.inherits.clone(),
    };
    assert_eq!(
        f.graph()
            .missing_base_parents(&basis, &Default::default())?,
        std::collections::BTreeMap::from([("A2".into(), inputs.replacement.inherited())])
    );
    // B's already integrated own number does not force B to precede itself.
    assert!(
        f.graph()
            .missing_base_parents(&basis, &plan.inherits)?
            .is_empty()
    );
    let mut changed = (*plan).clone();
    changed.base = plan.worktree.clone();
    assert!(changed.verify(&f.repo, &f.graph()).is_err());
    let target = f.tree(&[
        ("common", "shared"),
        ("replacement", "A branch"),
        ("independent", "X"),
    ])?;
    let Merge::Clean(tree) =
        f.repo
            .merge_trees(plan.base.parse()?, target, plan.worktree.parse()?)?
    else {
        panic!("The deleted B file should stay deleted");
    };
    assert_eq!(
        f.names(&tree.to_string())?,
        ["common", "dirty", "independent", "pending", "replacement"]
    );
    Ok(())
}

#[test]
fn a_never_integrated_descendant_uses_the_replacement_base_and_keeps_its_current_files()
-> Result<()> {
    let f = Fixture::new()?;
    let old = f.point("A", 3, &[("retired", "A later")], &[])?;
    let new = f.point("A2", 1, &[("replacement", "A branch")], &[])?;
    let child = f.point("B", 1, &[("retired", "A later"), ("own", "B")], &[&old])?;
    let inputs = Inputs {
        destination: Destination::Publication {
            coverage: Default::default(),
        },
        views: Vec::new(),
        resolutions: vec![],
        worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
        checkpoint: child,
        inherited: old,
        replacement: new.clone(),
        integrated: None,
    };
    let Outcome::Prepared(plan) = replacement_reparent::prepare(&f.repo, &f.graph(), &inputs)?
    else {
        panic!("The saved B delta is independent");
    };
    assert_eq!(plan.base, f.repo.tree_id(new.sha.parse()?)?.to_string());
    assert_eq!(f.names(&plan.worktree)?, ["own", "replacement"]);
    Ok(())
}

#[test]
fn a_descendant_chain_uses_the_reparented_parents_new_checkpoint_in_order() -> Result<()> {
    let f = Fixture::new()?;
    let a = f.point("A", 3, &[("retired", "A later")], &[])?;
    let a2 = f.point("A2", 1, &[("replacement", "A branch")], &[])?;
    let b1 = f.point("B", 1, &[("retired", "A later"), ("b1", "B old")], &[&a])?;
    let b2 = f.point(
        "B",
        2,
        &[("retired", "A later"), ("b1", "B old"), ("b2", "B later")],
        &[&a],
    )?;
    let c1 = f.point(
        "C",
        1,
        &[
            ("retired", "A later"),
            ("b1", "B old"),
            ("c1", "C committed"),
        ],
        &[&b1],
    )?;
    let c2 = f.point(
        "C",
        2,
        &[
            ("retired", "A later"),
            ("b1", "B old"),
            ("c1", "C committed"),
            ("c2", "C pending"),
        ],
        &[&b1],
    )?;
    let Outcome::Prepared(b) = replacement_reparent::prepare(
        &f.repo,
        &f.graph(),
        &Inputs {
            destination: Destination::Publication {
                coverage: Default::default(),
            },
            views: Vec::new(),
            resolutions: vec![],
            worktree: f.repo.tree_id(b2.sha.parse()?)?.to_string(),
            checkpoint: b2,
            inherited: a,
            replacement: a2.clone(),
            integrated: Some(b1.clone()),
        },
    )?
    else {
        panic!("B should reparent first");
    };
    let new_b = Point {
        views: Vec::new(),
        parents: std::collections::BTreeMap::from([("A2".into(), a2.inherited())]),
        track: "B".into(),
        number: 3,
        sha: f
            .repo
            .checkpoint_commit(b.worktree.parse()?, b1.sha.parse()?, "Own reparented B")?
            .to_string(),
        base_at: f
            .repo
            .checkpoint_commit(b.base.parse()?, f.root, "Own B baseline")?
            .to_string(),
        base_inherits_at: Default::default(),
        inherits: std::collections::BTreeMap::from([("A2".into(), a2.inherited())]),
    };
    f.points.borrow_mut().push(new_b.clone());
    let Outcome::Prepared(c) = replacement_reparent::prepare(
        &f.repo,
        &f.graph(),
        &Inputs {
            destination: Destination::Publication {
                coverage: Default::default(),
            },
            views: Vec::new(),
            resolutions: vec![],
            worktree: f.repo.tree_id(c2.sha.parse()?)?.to_string(),
            checkpoint: c2,
            inherited: b1,
            replacement: new_b,
            integrated: Some(c1),
        },
    )?
    else {
        panic!("C should use the reparented B checkpoint");
    };
    assert_eq!(
        f.names(&c.worktree)?,
        ["b1", "b2", "c1", "c2", "replacement"]
    );
    assert_eq!(f.names(&c.base)?, ["b1", "b2", "c1", "replacement"]);
    Ok(())
}

#[test]
fn reparenting_retains_both_worktree_and_integrated_baseline_conflicts() -> Result<()> {
    let f = Fixture::new()?;
    let old = f.point("A", 3, &[("file", "A later\n")], &[])?;
    let new = f.point("A2", 1, &[("file", "A branch\n")], &[])?;
    let integrated = f.point("B", 1, &[("file", "B own\n")], &[&old])?;
    let child = f.point("B", 2, &[("file", "A later\n")], &[&old])?;
    for (worktree, stage) in [
        (f.repo.tree_id(integrated.sha.parse()?)?, Stage::Worktree),
        (f.repo.tree_id(child.sha.parse()?)?, Stage::Base),
    ] {
        let Outcome::Conflict(conflict) = replacement_reparent::prepare(
            &f.repo,
            &f.graph(),
            &Inputs {
                destination: Destination::Publication {
                    coverage: Default::default(),
                },
                views: Vec::new(),
                resolutions: vec![],
                checkpoint: child.clone(),
                inherited: old.clone(),
                replacement: new.clone(),
                worktree: worktree.to_string(),
                integrated: Some(integrated.clone()),
            },
        )?
        else {
            panic!("A conflicting B edit cannot be silently discarded");
        };
        assert_eq!(conflict.stage, stage);
        assert_eq!(conflict.paths, ["file"]);
        assert_eq!(conflict.base, f.repo.tree_id(old.sha.parse()?)?.to_string());
        assert_eq!(
            conflict.target,
            f.repo.tree_id(new.sha.parse()?)?.to_string()
        );
        assert_eq!(
            conflict.incoming,
            f.repo.tree_id(integrated.sha.parse()?)?.to_string()
        );
    }
    Ok(())
}

#[test]
fn reparenting_rejects_a_changed_inherited_checkpoint_or_a_dependency_cycle() -> Result<()> {
    let f = Fixture::new()?;
    let old = f.point("A", 3, &[("retired", "A later")], &[])?;
    let new = f.point("A2", 1, &[("replacement", "A branch")], &[])?;
    let child = f.point("B", 1, &[("retired", "A later"), ("own", "B")], &[&old])?;
    let original = Inputs {
        destination: Destination::Publication {
            coverage: Default::default(),
        },
        views: Vec::new(),
        resolutions: vec![],
        worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
        checkpoint: child.clone(),
        inherited: old,
        replacement: new,
        integrated: None,
    };
    let mut unchanged = original.clone();
    unchanged.replacement = unchanged.inherited.clone();
    assert!(replacement_reparent::prepare(&f.repo, &f.graph(), &unchanged).is_err());
    let mut changed = original.clone();
    changed.inherited.number = 2;
    assert!(replacement_reparent::prepare(&f.repo, &f.graph(), &changed).is_err());
    changed = original.clone();
    include(&mut changed.replacement.inherits, "B", &child.inherited())?;
    assert!(replacement_reparent::prepare(&f.repo, &f.graph(), &changed).is_err());
    changed = original;
    let mut future = child;
    future.number = 2;
    changed.integrated = Some(future);
    assert!(replacement_reparent::prepare(&f.repo, &f.graph(), &changed).is_err());
    Ok(())
}

#[test]
fn reparenting_keeps_an_independent_direct_parent_while_removing_the_retired_parents_coverage()
-> Result<()> {
    let f = Fixture::new()?;
    let x1 = f.point("X", 1, &[("x", "one")], &[])?;
    let x3 = f.point("X", 3, &[("x", "three")], &[])?;
    let old = f.point("A", 3, &[("x", "three"), ("retired", "A later")], &[&x3])?;
    let new = f.point("A2", 1, &[("replacement", "A branch")], &[])?;
    let integrated = f.point(
        "B",
        1,
        &[("x", "three"), ("retired", "A later"), ("own", "B")],
        &[&old, &x1],
    )?;
    let child = f.point(
        "B",
        2,
        &[
            ("x", "three"),
            ("retired", "A later"),
            ("own", "B"),
            ("pending", "B later"),
        ],
        &[&old, &x1],
    )?;
    assert_eq!(child.parents["X"].number, 1);
    assert_eq!(child.inherits["X"].number, 3);
    let inputs = Inputs {
        destination: Destination::Publication {
            coverage: Default::default(),
        },
        views: Vec::new(),
        resolutions: vec![],
        worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
        checkpoint: child,
        inherited: old,
        replacement: new.clone(),
        integrated: Some(integrated.clone()),
    };
    let Outcome::Prepared(plan) = replacement_reparent::prepare(&f.repo, &f.graph(), &inputs)?
    else {
        panic!("The independent X root should remain");
    };
    assert_eq!(
        f.names(&plan.worktree)?,
        ["own", "pending", "replacement", "x"]
    );
    let files = f.repo.tree_files(plan.worktree.parse()?)?;
    assert_eq!(f.repo.saved_bytes(&files["x"])?, b"one");
    assert_eq!(plan.parents.len(), 2);
    assert_eq!(plan.parents["A2"], new.inherited());
    assert_eq!(plan.parents["X"], x1.inherited());
    assert_eq!(plan.inherits["X"], x1.inherited());
    assert!(!plan.inherits.contains_key("A"));
    assert_eq!(plan.base_inherits["B"], integrated.inherited());
    assert!(!plan.base_inherits.contains_key("A"));
    plan.verify(&f.repo, &f.graph())?;
    Ok(())
}

#[test]
fn a_borrowed_base_keeps_newer_coverage_than_the_retired_parent_supplied() -> Result<()> {
    let f = Fixture::new()?;
    let early = f.point("W", 2, &[("shared", "W earlier")], &[])?;
    let borrowed = f.point(
        "W",
        5,
        &[("shared", "W earlier"), ("borrowed", "W later")],
        &[],
    )?;
    let old = f.point(
        "A",
        3,
        &[("shared", "W earlier"), ("retired", "A later")],
        &[&early],
    )?;
    let new = f.point(
        "A-alt",
        1,
        &[("shared", "W earlier"), ("chosen", "A alternative")],
        &[&early],
    )?;
    let mut child = f.point(
        "B",
        1,
        &[
            ("shared", "W earlier"),
            ("retired", "A later"),
            ("borrowed", "W later"),
            ("pending", "B pending"),
        ],
        &[&old],
    )?;
    child.base_at = f
        .repo
        .checkpoint_commit(
            f.tree(&[
                ("shared", "W earlier"),
                ("retired", "A later"),
                ("borrowed", "W later"),
            ])?,
            f.root,
            "Own later borrowed base",
        )?
        .to_string();
    child.base_inherits_at = std::collections::BTreeMap::from([
        (old.track.clone(), old.inherited()),
        (borrowed.track.clone(), borrowed.inherited()),
    ]);
    *f.points.borrow_mut().last_mut().unwrap() = child.clone();
    let Outcome::Prepared(prepared) = replacement_reparent::prepare(
        &f.repo,
        &f.graph(),
        &Inputs {
            destination: Destination::Publication {
                coverage: Default::default(),
            },
            views: Vec::new(),
            worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
            checkpoint: child,
            inherited: old,
            replacement: new,
            integrated: None,
            resolutions: vec![],
        },
    )?
    else {
        panic!("An independent later base should remain clean");
    };
    assert_eq!(
        prepared.base_inherits["W"],
        borrowed.inherited(),
        "A supplied W@2, so replacing A must not forget W@5 borrowed independently into B's base"
    );
    assert!(!prepared.base_inherits.contains_key("A"));
    assert_eq!(f.names(&prepared.base)?, ["borrowed", "chosen", "shared"]);
    let target = f.tree(&[("shared", "W earlier"), ("chosen", "A alternative")])?;
    let Merge::Clean(result) =
        f.repo
            .merge_trees(prepared.base.parse()?, target, prepared.worktree.parse()?)?
    else {
        panic!("A later target deletion should remain deleted");
    };
    assert_eq!(
        f.names(&result.to_string())?,
        ["chosen", "pending", "shared"]
    );
    Ok(())
}

#[test]
fn reparenting_uses_only_resolutions_for_the_exact_frozen_trees() -> Result<()> {
    let f = Fixture::new()?;
    let old = f.point("A", 3, &[("file", "A later\n")], &[])?;
    let new = f.point("A2", 1, &[("file", "A branch\n")], &[])?;
    let child = f.point("B", 1, &[("file", "B own\n")], &[&old])?;
    let mut inputs = Inputs {
        destination: Destination::Publication {
            coverage: Default::default(),
        },
        views: Vec::new(),
        resolutions: vec![],
        worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
        checkpoint: child,
        inherited: old,
        replacement: new,
        integrated: None,
    };
    let Outcome::Conflict(conflict) = replacement_reparent::prepare(&f.repo, &f.graph(), &inputs)?
    else {
        panic!("The edits conflict before manual resolution");
    };
    let resolved = f.tree(&[("file", "Own explicit resolution\n")])?;
    inputs
        .resolutions
        .push(intercat_daemon::merge_choices::Solved {
            base: conflict.base,
            target: conflict.target,
            incoming: conflict.incoming,
            tree: resolved.to_string(),
        });
    let Outcome::Prepared(plan) = replacement_reparent::prepare(&f.repo, &f.graph(), &inputs)?
    else {
        panic!("The exact explicit resolution should be used");
    };
    assert_eq!(plan.worktree, resolved.to_string());
    plan.verify(&f.repo, &f.graph())?;
    inputs.replacement = f.point("A2", 2, &[("file", "A later\n")], &[])?;
    assert!(replacement_reparent::prepare(&f.repo, &f.graph(), &inputs).is_err());
    Ok(())
}

#[test]
fn a_conflict_between_the_new_direct_parents_stops_before_the_child_delta() -> Result<()> {
    let f = Fixture::new()?;
    let old = f.point("A", 3, &[("file", "A later\n")], &[])?;
    let new = f.point("A2", 1, &[("file", "A branch\n")], &[])?;
    let other = f.point("Q", 1, &[("file", "Q own\n")], &[])?;
    let child = f.point(
        "B",
        1,
        &[("file", "Own previous resolution\n")],
        &[&old, &other],
    )?;
    let inputs = Inputs {
        destination: Destination::Publication {
            coverage: Default::default(),
        },
        views: Vec::new(),
        resolutions: vec![],
        worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
        checkpoint: child,
        inherited: old,
        replacement: new.clone(),
        integrated: None,
    };
    let Outcome::Conflict(conflict) = replacement_reparent::prepare(&f.repo, &f.graph(), &inputs)?
    else {
        panic!("New direct parents need a separate resolution");
    };
    assert_eq!(conflict.stage, Stage::Parents);
    assert_eq!(conflict.paths, ["file"]);
    assert_eq!(
        conflict.target,
        f.repo.tree_id(new.sha.parse()?)?.to_string()
    );
    assert_eq!(
        conflict.incoming,
        f.repo.tree_id(other.sha.parse()?)?.to_string()
    );
    Ok(())
}

#[test]
fn only_the_declared_parent_part_already_in_the_base_requires_prior_publication() -> Result<()> {
    use intercat_daemon::lineage::{Ancestors, Basis};
    let f = Fixture::new()?;
    let a1 = f.point("A", 1, &[("file", "one")], &[])?;
    let a2 = f.point("A", 2, &[("file", "two")], &[])?;
    let a3 = f.point("A", 3, &[("file", "three")], &[])?;
    let mut basis = Basis {
        views: Vec::new(),
        base: a2.sha.clone(),
        parents: Ancestors::from([("A".into(), a3.inherited())]),
        base_inherits: Ancestors::from([("A".into(), a2.inherited())]),
        inherits: Ancestors::from([("A".into(), a3.inherited())]),
    };
    assert_eq!(
        f.graph()
            .missing_base_parents(&basis, &Ancestors::from([("A".into(), a1.inherited())]))?,
        Ancestors::from([("A".into(), a2.inherited())])
    );
    assert!(
        f.graph()
            .missing_base_parents(&basis, &Ancestors::from([("A".into(), a2.inherited())]))?
            .is_empty()
    );
    basis.base = a3.sha.clone();
    basis.base_inherits.insert("A".into(), a3.inherited());
    basis.parents.insert("A".into(), a1.inherited());
    basis.inherits = basis.parents.clone();
    assert!(
        f.graph()
            .missing_base_parents(&basis, &Ancestors::from([("A".into(), a1.inherited())]))?
            .is_empty()
    );
    // With no parent part in the base, a child still carries the full parent
    // delta implicitly, preserving the existing child-only Commit behavior.
    basis.base = f.root.to_string();
    basis.base_inherits.clear();
    assert!(
        f.graph()
            .missing_base_parents(&basis, &Ancestors::new())?
            .is_empty()
    );
    Ok(())
}

#[test]
fn a_second_parent_replacement_preserves_an_independent_file_added_after_the_first_undo()
-> Result<()> {
    let f = Fixture::new()?;
    let a = f.point("A", 3, &[("retired-a", "A retired")], &[])?;
    let alt = f.point("A2", 1, &[("chosen-a", "A chosen")], &[])?;
    let x = f.point("X", 3, &[("x", "X old")], &[])?;
    let x_alt = f.point("X2", 1, &[("x", "X chosen")], &[])?;
    let integrated = f.point(
        "B",
        1,
        &[
            ("retired-a", "A retired"),
            ("x", "X old"),
            ("done", "B integrated"),
        ],
        &[&a, &x],
    )?;
    let current = f.point(
        "B",
        2,
        &[
            ("retired-a", "A retired"),
            ("x", "X old"),
            ("done", "B integrated"),
            ("pending", "B pending"),
        ],
        &[&a, &x],
    )?;
    let first = match replacement_reparent::prepare(
        &f.repo,
        &f.graph(),
        &Inputs {
            destination: Destination::Publication {
                coverage: Default::default(),
            },
            views: Vec::new(),
            checkpoint: current,
            inherited: a,
            replacement: alt.clone(),
            worktree: f
                .tree(&[
                    ("retired-a", "A retired"),
                    ("x", "X old"),
                    ("done", "B integrated"),
                    ("pending", "B pending"),
                ])?
                .to_string(),
            integrated: Some(integrated.clone()),
            resolutions: vec![],
        },
    )? {
        Outcome::Prepared(prepared) => prepared,
        other => panic!("First replacement should be clean: {other:?}"),
    };
    let normalized_base = f.repo.checkpoint_commit(
        first.base.parse()?,
        alt.sha.parse()?,
        "Own first normalized child baseline",
    )?;
    let mut after = f.point(
        "B",
        3,
        &[
            ("chosen-a", "A chosen"),
            ("x", "X old"),
            ("done", "B integrated"),
            ("pending", "B pending"),
        ],
        &[&alt, &x],
    )?;
    after.base_at = normalized_base.to_string();
    after.base_inherits_at = first.base_inherits.clone();
    *f.points.borrow_mut().last_mut().unwrap() = after.clone();
    // A's undo and A2 were published with B left out. B's own integrated
    // marker therefore still points to B1. Another worker then independently
    // adds a file with A's old bytes. Its edit must survive replacing X next.
    let second = match replacement_reparent::prepare(
        &f.repo,
        &f.graph(),
        &Inputs {
            destination: Destination::Publication {
                coverage: Default::default(),
            },
            views: Vec::new(),
            checkpoint: after,
            inherited: x,
            replacement: x_alt.clone(),
            worktree: first.worktree.clone(),
            integrated: Some(integrated.clone()),
            resolutions: vec![],
        },
    )? {
        Outcome::Prepared(prepared) => prepared,
        other => panic!("Second replacement should be clean: {other:?}"),
    };
    let base = f.repo.checkpoint_commit(
        second.base.parse()?,
        x_alt.sha.parse()?,
        "Own second normalized child baseline",
    )?;
    let mut point = f.point(
        "B",
        4,
        &[
            ("chosen-a", "A chosen"),
            ("x", "X chosen"),
            ("done", "B integrated"),
            ("pending", "B pending"),
        ],
        &[&alt, &x_alt],
    )?;
    point.base_at = base.to_string();
    point.base_inherits_at = second.base_inherits.clone();
    *f.points.borrow_mut().last_mut().unwrap() = point.clone();
    let coverage = std::collections::BTreeMap::from([
        (alt.track.clone(), alt.inherited()),
        (x_alt.track.clone(), x_alt.inherited()),
        (integrated.track.clone(), integrated.inherited()),
    ]);
    let graph = f.graph();
    let mut merges = intercat_daemon::merge_choices::Merges::new(&f.repo, &[])?;
    let Merge::Clean(baseline) =
        graph.track_baseline(&mut merges, &point, &point.basis(), &coverage)?
    else {
        panic!("The real Commit baseline should be clean")
    };
    let target = f.tree(&[
        ("chosen-a", "A chosen"),
        ("x", "X chosen"),
        ("retired-a", "A retired"),
        ("unrelated", "Other worker"),
    ])?;
    let Merge::Clean(result) = f
        .repo
        .merge_trees(baseline, target, second.worktree.parse()?)?
    else {
        panic!("Independent target files must not conflict")
    };
    let files = f.repo.tree_files(result)?;
    assert!(
        files.contains_key("retired-a"),
        "a second reparent must not turn the earlier retired parent's file into B's deletion of another worker's independent file"
    );
    assert!(files.contains_key("unrelated"));
    assert!(files.contains_key("pending"));
    assert!(
        !files.contains_key("done"),
        "the independently deleted B file must remain deleted"
    );
    Ok(())
}

#[test]
fn repeated_shared_projection_retains_both_choices_and_the_original_source() -> Result<()> {
    use intercat_daemon::shared_context::{self, Projection};
    let f = Fixture::new()?;
    let a1 = f.point("A", 1, &[("shared-a", "A fork")], &[])?;
    let a3 = f.point(
        "A",
        3,
        &[("shared-a", "A fork"), ("retired-a", "A later")],
        &[],
    )?;
    let a_alt = f.point(
        "A-alt",
        1,
        &[("shared-a", "A fork"), ("chosen-a", "A choice")],
        &[&a1],
    )?;
    let x1 = f.point("X", 1, &[("shared-x", "X fork")], &[])?;
    let x3 = f.point(
        "X",
        3,
        &[("shared-x", "X fork"), ("retired-x", "X later")],
        &[],
    )?;
    let x_alt = f.point(
        "X-alt",
        1,
        &[("shared-x", "X fork"), ("chosen-x", "X choice")],
        &[&x1],
    )?;
    let shared = f.point(
        "B",
        1,
        &[
            ("shared-a", "A fork"),
            ("retired-a", "A later"),
            ("shared-x", "X fork"),
            ("retired-x", "X later"),
            ("shared-b", "B fork"),
        ],
        &[&a3, &x3],
    )?;
    let child = f.point(
        "B-alt",
        1,
        &[
            ("shared-a", "A fork"),
            ("retired-a", "A later"),
            ("shared-x", "X fork"),
            ("retired-x", "X later"),
            ("shared-b", "B fork"),
            ("chosen-b", "B choice"),
        ],
        &[&shared],
    )?;
    let graph = f.graph();
    let original = graph.clone();
    for [(first_from, first_onto), (second_from, second_onto)] in [
        [(&a3, &a_alt), (&x3, &x_alt)],
        [(&x3, &x_alt), (&a3, &a_alt)],
    ] {
        let shared_context::Outcome::View(first) = shared_context::project(
            &f.repo,
            &graph,
            &Projection {
                source: shared.clone(),
                previous: None,
                from: first_from.clone(),
                onto: first_onto.clone(),
                views: vec![],
            },
            &[],
        )?
        else {
            panic!("The first parent should normalize");
        };
        let project_next = || {
            shared_context::project(
                &f.repo,
                &graph,
                &Projection {
                    source: shared.clone(),
                    previous: Some((*first).clone()),
                    from: second_from.clone(),
                    onto: second_onto.clone(),
                    views: vec![],
                },
                &[],
            )
        };
        let shared_context::Outcome::View(second) = project_next()? else {
            panic!("The second parent should normalize without restoring the first");
        };
        let shared_context::Outcome::View(replayed) = project_next()? else {
            panic!("The exact saved projection should replay");
        };
        assert_eq!(second, replayed);
        assert_eq!(second.source, shared.inherited());
        assert_eq!(
            f.names(&f.repo.tree_id(second.head.parse()?)?.to_string())?,
            ["chosen-a", "chosen-x", "shared-a", "shared-b", "shared-x"]
        );
        assert_eq!(
            f.names(&f.repo.tree_id(second.origin.base.parse()?)?.to_string())?,
            ["chosen-a", "chosen-x", "shared-a", "shared-x"],
            "B's own shared changes must remain outside its pre-own origin"
        );
        assert_eq!(
            second
                .origin
                .parents
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["A-alt", "X-alt"]
        );
        assert_eq!(second.origin.inherits["A"], a1.inherited());
        assert_eq!(second.origin.inherits["X"], x1.inherited());
        assert!(!second.origin.base_inherits.contains_key("B"));
        let Outcome::Prepared(descendant) = replacement_reparent::prepare(
            &f.repo,
            &graph,
            &Inputs {
                destination: Destination::Publication {
                    coverage: Default::default(),
                },
                views: vec![*second.clone()],
                checkpoint: child.clone(),
                inherited: shared.clone(),
                replacement: shared.clone(),
                worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
                integrated: None,
                resolutions: vec![],
            },
        )?
        else {
            panic!("A descendant should retain the fully normalized shared source");
        };
        assert_eq!(
            f.names(&descendant.worktree)?,
            [
                "chosen-a", "chosen-b", "chosen-x", "shared-a", "shared-b", "shared-x"
            ]
        );
        assert_eq!(
            f.names(&descendant.base)?,
            ["chosen-a", "chosen-x", "shared-a", "shared-x"]
        );
        intercat_daemon::lineage::view::validate(&graph, &f.repo, &vec![*second])?;
        assert!(
            shared_context::project(
                &f.repo,
                &graph,
                &Projection {
                    source: a_alt.clone(),
                    previous: Some((*first).clone()),
                    from: second_from.clone(),
                    onto: second_onto.clone(),
                    views: vec![],
                },
                &[],
            )
            .is_err(),
            "A saved context cannot be used as another source checkpoint"
        );
    }
    assert_eq!(
        graph, original,
        "Projection must not rewrite canonical checkpoint ancestry"
    );
    assert_eq!(
        f.names(&f.repo.tree_id(shared.sha.parse()?)?.to_string())?,
        ["retired-a", "retired-x", "shared-a", "shared-b", "shared-x"]
    );
    Ok(())
}

#[test]
fn a_saved_shared_projection_binds_its_solution_to_the_exact_source_and_choices() -> Result<()> {
    use intercat_daemon::{
        merge_choices::Solved,
        shared_context::{self, Projection},
    };
    let f = Fixture::new()?;
    let old = f.point("A", 3, &[("config", "A later\n")], &[])?;
    let chosen = f.point("A-alt", 1, &[("config", "A chosen\n")], &[])?;
    let different = f.point("A-other", 1, &[("config", "A different\n")], &[])?;
    let shared = f.point(
        "B",
        1,
        &[("config", "B shared\n"), ("shared-b", "B own")],
        &[&old],
    )?;
    let graph = f.graph();
    let input = Projection {
        source: shared.clone(),
        previous: None,
        from: old,
        onto: chosen.clone(),
        views: vec![],
    };
    let shared_context::Outcome::Conflict(conflict) =
        shared_context::project(&f.repo, &graph, &input, &[])?
    else {
        panic!("The shared source edit must not be silently chosen over the new parent");
    };
    assert_eq!(conflict.stage, Stage::Worktree);
    assert_eq!(conflict.paths, ["config"]);
    let solution = Solved {
        base: conflict.base,
        target: conflict.target,
        incoming: conflict.incoming,
        tree: f
            .tree(&[
                ("config", "A chosen with B shared\n"),
                ("shared-b", "B own"),
            ])?
            .to_string(),
    };
    let saved = serde_json::to_vec(&(input.clone(), vec![solution.clone()]))?;
    let (restored, choices): (Projection, Vec<Solved>) = serde_json::from_slice(&saved)?;
    assert_eq!(restored, input);
    let namespace = uuid::Uuid::from_u128(1);
    assert_eq!(restored.identity(namespace)?, input.identity(namespace)?);
    assert_ne!(
        input.identity(namespace)?,
        input.identity(uuid::Uuid::from_u128(2))?
    );
    let shared_context::Outcome::View(view) =
        shared_context::project(&f.repo, &graph, &restored, &choices)?
    else {
        panic!("The saved solution should reproduce the normalized shared files");
    };
    assert_eq!(
        f.repo.tree_id(view.head.parse()?)?.to_string(),
        solution.tree
    );
    assert_eq!(
        f.repo.tree_id(view.origin.base.parse()?)?,
        f.repo.tree_id(chosen.sha.parse()?)?
    );
    let shared_context::Outcome::View(replayed) =
        shared_context::project(&f.repo, &graph, &input, &choices)?
    else {
        panic!("The same frozen choice should replay");
    };
    assert_eq!(view, replayed);
    let changed = Projection {
        onto: different,
        ..input.clone()
    };
    assert_ne!(input.identity(namespace)?, changed.identity(namespace)?);
    let shared_context::Outcome::Conflict(changed_conflict) =
        shared_context::project(&f.repo, &graph, &changed, &choices)?
    else {
        panic!("A different parent must not reuse the previous solution");
    };
    assert_ne!(changed_conflict.target, solution.target);
    let mut forged = input;
    forged.source.parents.clear();
    assert!(shared_context::project(&f.repo, &graph, &forged, &choices).is_err());
    assert_eq!(
        f.names(&f.repo.tree_id(shared.sha.parse()?)?.to_string())?,
        ["config", "shared-b"]
    );
    Ok(())
}

#[test]
fn a_shared_fork_keeps_its_own_changes_without_restoring_its_retired_ancestor() -> Result<()> {
    use intercat_daemon::{
        lineage::{Basis, View},
        shared_context,
    };
    let f = Fixture::new()?;
    let a1 = f.point("A", 1, &[("shared-a", "A fork")], &[])?;
    let a3 = f.point(
        "A",
        3,
        &[("shared-a", "A fork"), ("retired-a", "A later")],
        &[],
    )?;
    let alt = f.point(
        "A-alt",
        1,
        &[("shared-a", "A fork"), ("chosen-a", "A alternative")],
        &[&a1],
    )?;
    let b1 = f.point(
        "B",
        1,
        &[
            ("shared-a", "A fork"),
            ("retired-a", "A later"),
            ("shared-b", "B fork"),
        ],
        &[&a3],
    )?;
    let _later_b = f.point(
        "B",
        2,
        &[
            ("shared-a", "A fork"),
            ("retired-a", "A later"),
            ("shared-b", "B fork"),
            ("later-b", "B after its fork"),
        ],
        &[&a3],
    )?;
    let child = f.point(
        "B-alt",
        1,
        &[
            ("shared-a", "A fork"),
            ("retired-a", "A later"),
            ("shared-b", "B fork"),
            ("chosen-b", "B alternative"),
        ],
        &[&b1],
    )?;
    let graph = f.graph();
    let shared_context::Outcome::View(view) = shared_context::project(
        &f.repo,
        &graph,
        &shared_context::Projection {
            source: b1.clone(),
            previous: None,
            from: a3.clone(),
            onto: alt.clone(),
            views: vec![],
        },
        &[],
    )?
    else {
        panic!("Independent shared B files should normalize cleanly");
    };
    let shared_context::Outcome::View(replayed) = shared_context::project(
        &f.repo,
        &graph,
        &shared_context::Projection {
            source: b1.clone(),
            previous: None,
            from: a3.clone(),
            onto: alt.clone(),
            views: vec![],
        },
        &[],
    )?
    else {
        panic!("Same shared context must replay");
    };
    assert_eq!(
        view, replayed,
        "private derived commits must be deterministic"
    );
    assert_eq!(view.source, b1.inherited());
    let Outcome::Prepared(prepared) = replacement_reparent::prepare(
        &f.repo,
        &graph,
        &Inputs {
            destination: Destination::Publication {
                coverage: Default::default(),
            },
            views: vec![*view.clone()],
            worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
            checkpoint: child.clone(),
            inherited: b1.clone(),
            replacement: b1.clone(),
            integrated: None,
            resolutions: vec![],
        },
    )?
    else {
        panic!("The shared parent keeps its identity while changing its private context");
    };
    assert_eq!(
        f.names(&prepared.worktree)?,
        ["chosen-a", "chosen-b", "shared-a", "shared-b"]
    );
    assert_eq!(
        f.names(&prepared.base)?,
        ["chosen-a", "shared-a"],
        "unpublished B shared edits belong in B-alt's delta, not its base"
    );
    assert_eq!(prepared.parents["B"], b1.inherited());
    assert_eq!(prepared.inherits["A"], a1.inherited());
    assert_eq!(prepared.inherits["A-alt"], alt.inherited());
    assert_eq!(prepared.inherits["B"], b1.inherited());
    assert!(!prepared.base_inherits.contains_key("B"));
    assert_eq!(
        f.names(&f.repo.tree_id(b1.sha.parse()?)?.to_string())?,
        ["retired-a", "shared-a", "shared-b"],
        "the retired worker checkpoint stays unchanged"
    );
    let base = f.repo.context_commit(
        prepared.base.parse()?,
        alt.sha.parse()?,
        "Own normalized B-alt base",
    )?;
    let head = f.repo.context_commit(
        prepared.worktree.parse()?,
        child.sha.parse()?,
        "Own normalized B-alt checkpoint",
    )?;
    let normalized = Point {
        views: prepared.views.clone(),
        track: child.track.clone(),
        number: 2,
        sha: head.to_string(),
        base_at: base.to_string(),
        base_inherits_at: prepared.base_inherits.clone(),
        parents: prepared.parents.clone(),
        inherits: prepared.inherits.clone(),
    };
    f.points.borrow_mut().push(normalized.clone());
    let mut coverage = normalized.inherits.clone();
    include(&mut coverage, &normalized.track, &normalized.inherited())?;
    let graph = f.graph();
    let mut merges = intercat_daemon::merge_choices::Merges::new(&f.repo, &[])?;
    let Merge::Clean(baseline) = graph.track_baseline(
        &mut merges,
        &normalized,
        &normalized.basis(),
        &std::collections::BTreeMap::from([
            (alt.track.clone(), alt.inherited()),
            (a1.track.clone(), a1.inherited()),
        ]),
    )?
    else {
        panic!("The first normalized Commit should have a baseline");
    };
    assert_eq!(f.names(&baseline.to_string())?, ["chosen-a", "shared-a"]);
    let target = f.repo.tree_id(alt.sha.parse()?)?;
    let Merge::Clean(first) = f
        .repo
        .merge_trees(baseline, target, prepared.worktree.parse()?)?
    else {
        panic!("A-alt then B-alt should integrate");
    };
    assert_eq!(
        f.names(&first.to_string())?,
        ["chosen-a", "chosen-b", "shared-a", "shared-b"]
    );
    // After B-alt's first publication, another worker removes B's shared file.
    // The still-present worker file must not become a fresh addition next time.
    let mut next = normalized.clone();
    next.number = 3;
    next.sha = f
        .repo
        .context_commit(
            f.tree(&[
                ("shared-a", "A fork"),
                ("chosen-a", "A alternative"),
                ("shared-b", "B fork"),
                ("chosen-b", "B alternative"),
                ("next-b", "B next edit"),
            ])?,
            head,
            "Own later B-alt checkpoint",
        )?
        .to_string();
    next.base_at = head.to_string();
    next.base_inherits_at = coverage.clone();
    f.points.borrow_mut().push(next.clone());
    let graph = f.graph();
    let mut merges = intercat_daemon::merge_choices::Merges::new(&f.repo, &[])?;
    let Merge::Clean(baseline) =
        graph.track_baseline(&mut merges, &next, &next.basis(), &coverage)?
    else {
        panic!("Published normalized B-alt should keep its baseline");
    };
    let deleted = f.tree(&[
        ("shared-a", "A fork"),
        ("chosen-a", "A alternative"),
        ("chosen-b", "B alternative"),
    ])?;
    let Merge::Clean(again) =
        f.repo
            .merge_trees(baseline, deleted, f.repo.tree_id(next.sha.parse()?)?)?
    else {
        panic!("A later deletion must not conflict");
    };
    assert_eq!(
        f.names(&again.to_string())?,
        ["chosen-a", "chosen-b", "next-b", "shared-a"]
    );
    let encoded = serde_json::to_vec(&next.basis())?;
    let restored: Basis = serde_json::from_slice(&encoded)?;
    assert_eq!(
        restored.views, prepared.views,
        "branch/checkpoint basis serialization retains the projected source"
    );
    let mut forged: View = (*view).clone();
    forged
        .origin
        .inherits
        .insert(a3.track.clone(), a3.inherited());
    assert!(
        intercat_daemon::lineage::view::validate(&graph, &f.repo, &vec![forged]).is_err(),
        "a different ancestor origin cannot reuse the private context head"
    );
    let mut invalid_origin = (*view.origin).clone();
    invalid_origin
        .base_inherits
        .insert(b1.track.clone(), b1.inherited());
    assert!(
        View::create(
            &f.repo,
            &graph,
            &b1,
            invalid_origin,
            f.repo.tree_id(view.head.parse()?)?
        )
        .is_err(),
        "the pre-own origin cannot claim the shared source's own changes are already integrated"
    );
    Ok(())
}

#[test]
fn a_shared_view_uses_only_the_parent_number_already_present_in_the_borrowed_base() -> Result<()> {
    use intercat_daemon::shared_context;
    for source_has_own_base in [false, true] {
        for (through, remaining) in [(0, 0), (1, 0), (1, 1), (3, 0), (3, 1), (3, 3)] {
            let f = Fixture::new()?;
            let a1 = f.point("A", 1, &[("a", "A fork")], &[])?;
            let old = f.point("A", 3, &[("a", "A fork"), ("retired", "A later")], &[])?;
            let alt = f.point(
                "A-alt",
                1,
                &[("a", "A fork"), ("chosen", "A alternative")],
                &[&a1],
            )?;
            let b1 = f.point(
                "B",
                1,
                &[
                    ("a", "A fork"),
                    ("retired", "A later"),
                    ("b-first", "B first"),
                ],
                &[&old],
            )?;
            let mut b3 = f.point(
                "B",
                3,
                &[
                    ("a", "A fork"),
                    ("retired", "A later"),
                    ("b-first", "B first"),
                    ("b-later", "B later"),
                ],
                &[&old],
            )?;
            if source_has_own_base {
                b3.base_at = b1.sha.clone();
                b3.base_inherits_at = b1.inherits.clone();
                include(&mut b3.base_inherits_at, "B", &b1.inherited())?;
                *f.points.borrow_mut().last_mut().unwrap() = b3.clone();
            }
            let mut child = f.point(
                "B-alt",
                1,
                &[
                    ("a", "A fork"),
                    ("retired", "A later"),
                    ("b-first", "B first"),
                    ("b-later", "B later"),
                    ("own", "B alternative"),
                ],
                &[&b3],
            )?;
            if through != 0 {
                let previous = if through == 1 { &b1 } else { &b3 };
                child.base_at = previous.sha.clone();
                child.base_inherits_at = previous.inherits.clone();
                include(&mut child.base_inherits_at, "B", &previous.inherited())?;
                *f.points.borrow_mut().last_mut().unwrap() = child.clone();
            }
            let graph = f.graph();
            let shared_context::Outcome::View(first) = shared_context::project(
                &f.repo,
                &graph,
                &shared_context::Projection {
                    source: b1.clone(),
                    previous: None,
                    from: old.clone(),
                    onto: alt.clone(),
                    views: vec![],
                },
                &[],
            )?
            else {
                panic!("B@1 should normalize");
            };
            let shared_context::Outcome::View(last) = shared_context::project(
                &f.repo,
                &graph,
                &shared_context::Projection {
                    source: b3.clone(),
                    previous: None,
                    from: old.clone(),
                    onto: alt.clone(),
                    views: vec![*first.clone()],
                },
                &[],
            )?
            else {
                panic!("B@3 should normalize");
            };
            let mut coverage = alt.inherits.clone();
            include(&mut coverage, &alt.track, &alt.inherited())?;
            if remaining != 0 {
                let mark = if remaining == 1 {
                    b1.inherited()
                } else {
                    b3.inherited()
                };
                include(&mut coverage, "B", &mark)?;
            }
            let Outcome::Prepared(prepared) = replacement_reparent::prepare(
                &f.repo,
                &graph,
                &Inputs {
                    destination: Destination::Publication {
                        coverage: coverage.clone(),
                    },
                    views: vec![*first, *last],
                    worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
                    checkpoint: child,
                    inherited: b3.clone(),
                    replacement: b3.clone(),
                    integrated: None,
                    resolutions: vec![],
                },
            )?
            else {
                panic!("Partial shared base {through} should normalize");
            };
            let mut expected = vec!["a", "chosen"];
            if remaining > 0 {
                expected.push("b-first");
            }
            if remaining == 3 {
                expected.push("b-later");
            }
            expected.sort();
            assert_eq!(
                f.names(&prepared.base)?,
                expected,
                "base must contain retained B@{remaining} after borrowing B@{through}; source own base: {source_has_own_base}"
            );
            assert_eq!(
                prepared.base_inherits.get("B").map(|mark| mark.number),
                (remaining != 0).then_some(remaining)
            );
            assert_eq!(
                f.names(&prepared.worktree)?,
                ["a", "b-first", "b-later", "chosen", "own"]
            );
            let base = f.repo.context_commit(
                prepared.base.parse()?,
                alt.sha.parse()?,
                "Own partial shared base",
            )?;
            let point = Point {
                views: prepared.views.clone(),
                track: "B-alt".into(),
                number: 2,
                sha: f
                    .repo
                    .context_commit(
                        prepared.worktree.parse()?,
                        base,
                        "Own normalized partial fork",
                    )?
                    .to_string(),
                base_at: base.to_string(),
                base_inherits_at: prepared.base_inherits,
                parents: prepared.parents,
                inherits: prepared.inherits,
            };
            f.points.borrow_mut().push(point.clone());
            let graph = f.graph();
            let mut merges = intercat_daemon::merge_choices::Merges::new(&f.repo, &[])?;
            let Merge::Clean(baseline) =
                graph.track_baseline(&mut merges, &point, &point.basis(), &coverage)?
            else {
                panic!("A partial shared origin should give a clean Commit baseline");
            };
            assert_eq!(f.names(&baseline.to_string())?, expected);
            let Merge::Clean(result) =
                f.repo
                    .merge_trees(baseline, prepared.base.parse()?, prepared.worktree.parse()?)?
            else {
                panic!("The remaining shared changes should integrate exactly once");
            };
            assert_eq!(
                f.names(&result.to_string())?,
                ["a", "b-first", "b-later", "chosen", "own"]
            );
        }
    }
    Ok(())
}
