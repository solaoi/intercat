use intercat_daemon::{
    Result,
    lineage::{Ancestors, Basis, Frozen, Point},
};
use intercat_git::{Merge, ObjectId, Repository};
use std::collections::BTreeMap;

struct Fixture {
    _root: tempfile::TempDir,
    repo: Repository,
    root: String,
    graph: Frozen,
}
impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        Repository::init_isolated(root.path())?;
        let repo = Repository::open(root.path())?;
        let start = repo.empty_starting_point()?.to_string();
        Ok(Self {
            _root: root,
            repo,
            root: start,
            graph: Frozen::default(),
        })
    }
    fn tree(&self, files: &[(&str, &str)]) -> Result<ObjectId> {
        let blobs = files
            .iter()
            .map(|(name, text)| Ok((*name, self.repo.write_blob(text.as_bytes())?)))
            .collect::<Result<Vec<_>>>()?;
        self.repo.write_tree(&blobs)
    }
    fn point(&mut self, track: &str, files: &[(&str, &str)], inherits: &[&Point]) -> Result<Point> {
        let mut ancestry = Ancestors::new();
        for parent in inherits {
            self.graph.incorporate(&mut ancestry, parent)?;
        }
        let number = self
            .graph
            .points
            .iter()
            .filter(|p| p.track == track)
            .count() as u64
            + 1;
        let sha = self
            .repo
            .checkpoint_commit(
                self.tree(files)?,
                self.root.parse()?,
                &format!("{track}@{number}"),
            )?
            .to_string();
        let point = Point {
            track: track.into(),
            number,
            sha,
            base_at: self.root.clone(),
            base_inherits_at: Ancestors::new(),
            inherits: ancestry,
        };
        self.graph.points.push(point.clone());
        Ok(point)
    }
    fn merge(
        &self,
        child: &Point,
        head: &[(&str, &str)],
        integrated: &[&Point],
    ) -> Result<BTreeMap<String, String>> {
        let mut done = Ancestors::new();
        for point in integrated {
            self.graph.incorporate(&mut done, point)?;
        }
        let base = match self
            .graph
            .track_baseline(&self.repo, child, &child.basis(), &done)?
        {
            Merge::Clean(base) => base,
            Merge::Conflicts(paths) => panic!("baseline conflict: {paths:?}"),
        };
        let merged = match self.repo.merge_trees(
            base,
            self.tree(head)?,
            self.repo.tree_id(child.sha.parse()?)?,
        )? {
            Merge::Clean(tree) => tree,
            Merge::Conflicts(paths) => panic!("delta conflict: {paths:?}"),
        };
        self.repo
            .tree_files(merged)?
            .into_iter()
            .map(|(name, file)| Ok((name, String::from_utf8(self.repo.saved_bytes(&file)?)?)))
            .collect()
    }
}

#[test]
fn a_partial_ancestor_commit_does_not_resurrect_a_later_deletion() -> Result<()> {
    let mut f = Fixture::new()?;
    let a1 = f.point("A", &[("f", "one")], &[])?;
    let a2 = f.point("A", &[("f", "one"), ("g", "two")], &[])?;
    let b = f.point("B", &[("f", "one"), ("g", "two"), ("b", "child")], &[&a2])?;
    let result = f.merge(&b, &[], &[&a1])?;
    assert_eq!(
        result,
        BTreeMap::from([("g".into(), "two".into()), ("b".into(), "child".into())])
    );
    Ok(())
}

#[test]
fn several_parents_keep_only_the_unintegrated_parent_delta() -> Result<()> {
    let mut f = Fixture::new()?;
    let a1 = f.point("A", &[("f", "one")], &[])?;
    let a2 = f.point("A", &[], &[])?;
    let b = f.point("B", &[("b", "parent")], &[])?;
    let c = f.point(
        "C",
        &[("f", "one"), ("b", "parent"), ("c", "child")],
        &[&a1, &b],
    )?;
    let result = f.merge(&c, &[], &[&a2])?;
    assert!(!result.contains_key("f"));
    assert_eq!(result.len(), 2);
    assert_eq!(result["b"], "parent");
    Ok(())
}

#[test]
fn three_generation_inheritance_keeps_ancestor_deletions() -> Result<()> {
    let mut f = Fixture::new()?;
    let a1 = f.point("A", &[("a", "one")], &[])?;
    let a2 = f.point("A", &[], &[])?;
    let b = f.point("B", &[("a", "one"), ("b", "middle")], &[&a1])?;
    let c = f.point("C", &[("a", "one"), ("b", "middle"), ("c", "child")], &[&b])?;
    for integrated in [vec![&a2], vec![&a2, &b]] {
        let head = if integrated.len() == 1 {
            vec![]
        } else {
            vec![("b", "middle")]
        };
        let result = f.merge(&c, &head, &integrated)?;
        assert!(!result.contains_key("a"));
        assert_eq!(result.len(), 2);
    }
    Ok(())
}

#[test]
fn a_diamond_uses_the_newer_ancestor_once_including_its_deletion() -> Result<()> {
    let mut f = Fixture::new()?;
    let a1 = f.point("A", &[("f", "old")], &[])?;
    let a2 = f.point("A", &[], &[])?;
    let b = f.point("B", &[("f", "old"), ("b", "left")], &[&a1])?;
    let c = f.point("C", &[("c", "right")], &[&a2])?;
    let d = f.point(
        "D",
        &[("b", "left"), ("c", "right"), ("d", "last")],
        &[&b, &c],
    )?;
    let result = f.merge(&d, &[("b", "left"), ("c", "right")], &[&b, &c])?;
    assert_eq!(result.len(), 3);
    assert!(!result.contains_key("f"));
    Ok(())
}

#[test]
fn a_child_edit_is_taken_from_the_parent_value_it_inherited() -> Result<()> {
    let mut f = Fixture::new()?;
    let a = f.point("A", &[("a", "200\n")], &[])?;
    let b = f.point("B", &[("a", "300\n")], &[&a])?;
    assert_eq!(f.merge(&b, &[("a", "200\n")], &[&a])?["a"], "300\n");
    Ok(())
}

#[test]
fn integrating_a_child_advances_the_unselected_parent_boundary() -> Result<()> {
    let mut f = Fixture::new()?;
    let a1 = f.point("A", &[("a", "one")], &[])?;
    let child = f.point("B", &[("a", "one"), ("b", "child")], &[&a1])?;
    let a2 = f.point("A", &[("a", "one"), ("later", "two")], &[])?;
    let result = f.merge(&a2, &[("b", "child")], &[&child])?;
    assert!(!result.contains_key("a"));
    assert_eq!(result.len(), 2);
    Ok(())
}

#[test]
fn a_real_child_conflict_is_not_hidden_by_the_baseline() -> Result<()> {
    let mut f = Fixture::new()?;
    let a = f.point("A", &[("a", "200\n")], &[])?;
    let b = f.point("B", &[("a", "300\n")], &[&a])?;
    let done = Ancestors::from([("A".into(), a.inherited())]);
    let Merge::Clean(base) = f.graph.track_baseline(&f.repo, &b, &b.basis(), &done)? else {
        panic!()
    };
    assert!(matches!(
        f.repo.merge_trees(
            base,
            f.tree(&[("a", "400\n")])?,
            f.repo.tree_id(b.sha.parse()?)?
        )?,
        Merge::Conflicts(_)
    ));
    Ok(())
}

#[test]
fn the_snapshot_keeps_its_base_when_the_live_track_is_realigned() -> Result<()> {
    let mut f = Fixture::new()?;
    let a = f.point("A", &[("a", "one")], &[])?;
    let b = f.point("B", &[("a", "one"), ("b", "child")], &[&a])?;
    let saved = b.basis();
    let newer = f.point("A", &[("a", "one"), ("x", "unrelated")], &[])?;
    let mut live = Basis::root(newer.sha);
    live.base_inherits.insert("A".into(), a.inherited());
    assert_ne!(live, saved);
    let result = f.merge(&b, &[("a", "one"), ("x", "unrelated")], &[&a])?;
    assert_eq!(result.len(), 3);
    assert_eq!(result["x"], "unrelated");
    Ok(())
}

#[test]
fn missing_and_cyclic_ancestry_are_rejected() -> Result<()> {
    let mut f = Fixture::new()?;
    let a = f.point("A", &[("a", "one")], &[])?;
    let b = f.point("B", &[("a", "one"), ("b", "two")], &[&a])?;
    let done = Ancestors::from([("A".into(), a.inherited()), ("B".into(), b.inherited())]);
    let mut basis = Basis::root(f.root.clone());
    basis.inherits = done.clone();
    f.graph.points[0].inherits.insert("B".into(), b.inherited());
    assert!(f.graph.baseline(&f.repo, &basis, &done).is_err());
    f.graph.points.clear();
    assert!(f.graph.baseline(&f.repo, &basis, &done).is_err());
    Ok(())
}

#[test]
fn a_branch_from_a_parent_with_an_advanced_base_does_not_reapply_that_base() -> Result<()> {
    let mut f = Fixture::new()?;
    let a1 = f.point("A", &[("f", "old")], &[])?;
    let mut a2 = f.point("A", &[("f", "old"), ("g", "new")], &[])?;
    a2.base_at = a1.sha.clone();
    *f.graph.points.last_mut().unwrap() = a2.clone();
    let mut b = f.point("B", &[("f", "old"), ("g", "new"), ("b", "child")], &[&a2])?;
    b.base_at = a2.base_at.clone();
    *f.graph.points.last_mut().unwrap() = b.clone();
    let result = f.merge(&b, &[], &[&a1])?;
    assert_eq!(result.len(), 2);
    assert!(!result.contains_key("f"));
    assert_eq!(result["g"], "new");
    Ok(())
}

#[test]
fn intermediate_edits_do_not_conflict_while_composing_the_ancestor_baseline() -> Result<()> {
    let mut f = Fixture::new()?;
    let a = f.point("A", &[("a", "200\n")], &[])?;
    let b = f.point("B", &[("b", "B\n")], &[])?;
    let middle = f.point(
        "X",
        &[("a", "300\n"), ("b", "B\n"), ("x", "X\n")],
        &[&a, &b],
    )?;
    let child = f.point(
        "C",
        &[("a", "300\n"), ("b", "B\n"), ("x", "X\n"), ("c", "C\n")],
        &[&middle],
    )?;
    for integrated in [vec![&a], vec![&a, &b], vec![&a, &b, &middle]] {
        let mut head = vec![("a", "200\n")];
        if integrated.len() >= 2 {
            head.push(("b", "B\n"));
        }
        if integrated.len() == 3 {
            head[0] = ("a", "300\n");
            head.push(("x", "X\n"));
        }
        let files = f.merge(&child, &head, &integrated)?;
        assert_eq!(files["a"], "300\n");
        assert_eq!(files["b"], "B\n");
        assert_eq!(files["x"], "X\n");
        assert_eq!(files["c"], "C\n");
    }
    Ok(())
}

#[test]
fn a_realigned_child_does_not_restore_unrelated_files_deleted_from_the_target() -> Result<()> {
    let mut f = Fixture::new()?;
    let a = f.point("A", &[("a", "A\n")], &[])?;
    let x = f.point("X", &[("x", "X\n")], &[])?;
    let mut child = f.point("C", &[("a", "A\n"), ("x", "X\n"), ("c", "C\n")], &[&a])?;
    child.base_at = x.sha.clone();
    child.base_inherits_at.insert("X".into(), x.inherited());
    *f.graph.points.last_mut().unwrap() = child.clone();
    for integrated in [vec![&x], vec![&x, &a]] {
        let head = if integrated.len() == 1 {
            vec![]
        } else {
            vec![("a", "A\n")]
        };
        let files = f.merge(&child, &head, &integrated)?;
        assert!(!files.contains_key("x"));
        assert_eq!(files["a"], "A\n");
        assert_eq!(files["c"], "C\n");
    }
    Ok(())
}

#[test]
fn a_late_parent_update_cannot_restore_an_already_integrated_other_parents_file() -> Result<()> {
    let mut f = Fixture::new()?;
    let a1 = f.point("A", &[("a", "one\n")], &[])?;
    let a2 = f.point("A", &[("a", "two\n")], &[])?;
    let b = f.point("B", &[("b", "B\n")], &[])?;
    let child = f.point(
        "C",
        &[("a", "two\n"), ("b", "B\n"), ("c", "C\n")],
        &[&a2, &b],
    )?;
    let files = f.merge(&child, &[("a", "one\n")], &[&a1, &b])?;
    assert!(!files.contains_key("b"));
    assert_eq!(files["a"], "two\n");
    assert_eq!(files["c"], "C\n");
    Ok(())
}
