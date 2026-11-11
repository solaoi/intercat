use super::*;
use intercat_daemon::{
    shared_context,
    shared_plan::{self, Next, Rewrite, Saved},
};

#[test]
fn nested_shared_steps_resume_in_order_and_never_read_past_the_shared_cut() -> Result<()> {
    let f = Fixture::new()?;
    let a1 = f.point("A", 1, &[("a", "A fork")], &[])?;
    let a3 = f.point("A", 3, &[("a", "A fork"), ("retired-a", "A later")], &[])?;
    let alt = f.point(
        "A-alt",
        1,
        &[("a", "A fork"), ("chosen-a", "A chosen")],
        &[&a1],
    )?;
    let x1 = f.point("X", 1, &[("x", "X fork")], &[])?;
    let x3 = f.point("X", 3, &[("x", "X fork"), ("retired-x", "X later")], &[])?;
    let x_alt = f.point(
        "X-alt",
        1,
        &[("x", "X fork"), ("chosen-x", "X chosen")],
        &[&x1],
    )?;
    let x_other = f.point(
        "X-other",
        1,
        &[("x", "X fork"), ("other-x", "X different")],
        &[&x1],
    )?;
    let y_alt = f.point("Y-alt", 1, &[("y", "Unrelated")], &[])?;
    let b1 = f.point(
        "B",
        1,
        &[
            ("a", "A fork"),
            ("retired-a", "A later"),
            ("x", "X fork"),
            ("retired-x", "X later"),
            ("b-first", "B first"),
        ],
        &[&a3, &x3],
    )?;
    let b3 = f.point(
        "B",
        3,
        &[
            ("a", "A fork"),
            ("retired-a", "A later"),
            ("x", "X fork"),
            ("retired-x", "X later"),
            ("b-first", "B first"),
            ("b-shared", "B shared cut"),
        ],
        &[&a3, &x3],
    )?;
    let _b9 = f.point(
        "B",
        9,
        &[("never-read", "Retired worker's later files")],
        &[&a3, &x3],
    )?;
    let c1 = f.point(
        "C",
        1,
        &[
            ("a", "A fork"),
            ("retired-a", "A later"),
            ("x", "X fork"),
            ("retired-x", "X later"),
            ("b-first", "B first"),
            ("b-shared", "B shared cut"),
            ("c-shared", "C shared"),
        ],
        &[&b3],
    )?;
    let graph = f.graph();
    let changes = vec![
        Rewrite {
            track: "A".into(),
            shared_through: 1,
            target: alt,
        },
        Rewrite {
            track: "X".into(),
            shared_through: 1,
            target: x_alt,
        },
    ];
    let mut saved: Vec<Saved> = vec![];
    let views = loop {
        match shared_plan::next(&graph, &c1, &vec![], &changes, &saved)? {
            Next::Project(input) => {
                assert!(
                    saved.len() < 6,
                    "The planner must advance after each immutable result"
                );
                assert_ne!(input.source.number, 9);
                let shared_context::Outcome::View(view) =
                    shared_context::project(&f.repo, &graph, &input, &[])?
                else {
                    panic!("Independent nested files should normalize cleanly");
                };
                let result = Saved {
                    input: *input,
                    view: *view,
                };
                result.verify(&f.repo, &graph, &[])?;
                // A reopen only needs serialized immutable results, rather than
                // a live worker, a prior review revision, or an in-memory cursor.
                saved.push(serde_json::from_slice(&serde_json::to_vec(&result)?)?);
            }
            Next::Ready(views) => break views,
        }
    };
    assert_eq!(saved.len(), 5);
    assert_eq!(
        saved
            .iter()
            .map(|step| (step.input.source.track.as_str(), step.input.source.number))
            .collect::<Vec<_>>(),
        [("B", 1), ("B", 1), ("B", 3), ("B", 3), ("C", 1)]
    );
    for (source, expected) in [
        (&b1, vec!["a", "b-first", "chosen-a", "chosen-x", "x"]),
        (
            &b3,
            vec!["a", "b-first", "b-shared", "chosen-a", "chosen-x", "x"],
        ),
        (
            &c1,
            vec![
                "a", "b-first", "b-shared", "c-shared", "chosen-a", "chosen-x", "x",
            ],
        ),
    ] {
        let view = views
            .iter()
            .find(|view| view.track == source.track && view.source == source.inherited())
            .unwrap();
        assert_eq!(
            f.names(&f.repo.tree_id(view.head.parse()?)?.to_string())?,
            expected
        );
    }
    intercat_daemon::lineage::view::validate(&graph, &f.repo, &views)?;
    saved.reverse();
    assert_eq!(
        shared_plan::next(&graph, &c1, &vec![], &changes, &saved)?,
        Next::Ready(views.clone())
    );
    let mut unrelated = changes.clone();
    unrelated.push(Rewrite {
        track: "Y".into(),
        shared_through: 0,
        target: y_alt,
    });
    assert_eq!(
        shared_plan::next(&graph, &c1, &vec![], &unrelated, &saved)?,
        Next::Ready(views.clone())
    );
    let switches = vec![Rewrite {
        track: changes[1].target.track.clone(),
        shared_through: 0,
        target: x_other.clone(),
    }];
    let mut switched = saved.clone();
    let before = switched.len();
    let changed_views = loop {
        match shared_plan::next(&graph, &c1, &views, &switches, &switched)? {
            Next::Project(input) => {
                assert!(
                    switched.len() < before + 4,
                    "Each affected saved view must advance once"
                );
                assert!(
                    input.previous.is_some(),
                    "A new choice starts from the saved current view"
                );
                let shared_context::Outcome::View(view) =
                    shared_context::project(&f.repo, &graph, &input, &[])?
                else {
                    panic!("Independent nested files should switch their ancestor cleanly");
                };
                let saved = Saved {
                    input: *input,
                    view: *view,
                };
                saved.verify(&f.repo, &graph, &[])?;
                switched.push(serde_json::from_slice(&serde_json::to_vec(&saved)?)?);
            }
            Next::Ready(views) => break views,
        }
    };
    assert_eq!(switched.len() - before, 3);
    let child = changed_views
        .iter()
        .find(|view| view.track == c1.track && view.source == c1.inherited())
        .unwrap();
    assert_eq!(
        f.names(&f.repo.tree_id(child.head.parse()?)?.to_string())?,
        [
            "a", "b-first", "b-shared", "c-shared", "chosen-a", "other-x", "x"
        ]
    );
    let changed = vec![
        changes[0].clone(),
        Rewrite {
            target: x_other,
            ..changes[1].clone()
        },
    ];
    let Next::Project(pending) = shared_plan::next(&graph, &c1, &vec![], &changed, &saved)? else {
        panic!("A changed chosen ancestor must invalidate its dependent shared result");
    };
    assert_eq!(pending.source.inherited(), b1.inherited());
    assert_eq!(pending.from.track, "X");
    assert_eq!(pending.onto.track, "X-other");
    let mut conflicting = saved[0].clone();
    conflicting.view.head = f.root.to_string();
    saved.push(conflicting);
    assert!(shared_plan::next(&graph, &c1, &vec![], &changes, &saved).is_err());
    assert_eq!(graph, f.graph());
    Ok(())
}

#[test]
fn cancelling_a_sources_own_base_preserves_its_new_parents_and_unrelated_borrowed_files()
-> Result<()> {
    let f = Fixture::new()?;
    let a1 = f.point("A", 1, &[("a", "A shared")], &[])?;
    let old = f.point("A", 3, &[("a", "A shared"), ("retired", "A later")], &[])?;
    let alt = f.point(
        "A-alt",
        1,
        &[("a", "A shared"), ("chosen", "A alternative")],
        &[&a1],
    )?;
    let borrowed = f.point("W", 1, &[("borrowed", "W unrelated")], &[])?;
    let mut b1 = f.point(
        "B",
        1,
        &[
            ("a", "A shared"),
            ("borrowed", "W unrelated"),
            ("b-first", "B first"),
        ],
        &[&a1],
    )?;
    b1.base_at = borrowed.sha.clone();
    include(&mut b1.base_inherits_at, "W", &borrowed.inherited())?;
    *f.points.borrow_mut().last_mut().unwrap() = b1.clone();
    let mut b3 = f.point(
        "B",
        3,
        &[
            ("a", "A shared"),
            ("retired", "A later"),
            ("borrowed", "W unrelated"),
            ("b-first", "B first"),
            ("b-later", "B later"),
        ],
        &[&old],
    )?;
    b3.base_at = f
        .repo
        .context_commit(
            f.tree(&[
                ("a", "A shared"),
                ("retired", "A later"),
                ("borrowed", "W unrelated"),
                ("b-first", "B first"),
            ])?,
            b1.sha.parse()?,
            "Own B baseline after inheriting A later",
        )?
        .to_string();
    b3.base_inherits_at = b3.inherits.clone();
    include(&mut b3.base_inherits_at, "B", &b1.inherited())?;
    include(&mut b3.base_inherits_at, "W", &borrowed.inherited())?;
    *f.points.borrow_mut().last_mut().unwrap() = b3.clone();
    let mut child = f.point(
        "B-alt",
        1,
        &[
            ("a", "A shared"),
            ("retired", "A later"),
            ("borrowed", "W unrelated"),
            ("b-first", "B first"),
            ("b-later", "B later"),
            ("own", "B alternative"),
        ],
        &[&b3],
    )?;
    child.base_at = b3.sha.clone();
    child.base_inherits_at = b3.inherits.clone();
    include(&mut child.base_inherits_at, "B", &b3.inherited())?;
    include(&mut child.base_inherits_at, "W", &borrowed.inherited())?;
    *f.points.borrow_mut().last_mut().unwrap() = child.clone();
    let graph = f.graph();
    let shared_context::Outcome::View(view) = shared_context::project(
        &f.repo,
        &graph,
        &shared_context::Projection {
            source: b3.clone(),
            previous: None,
            from: old,
            onto: alt.clone(),
            views: vec![],
        },
        &[],
    )?
    else {
        panic!("The shared source should normalize cleanly");
    };
    let mut coverage = alt.inherits.clone();
    include(&mut coverage, &alt.track, &alt.inherited())?;
    include(&mut coverage, "W", &borrowed.inherited())?;
    let Outcome::Prepared(prepared) = replacement_reparent::prepare(
        &f.repo,
        &graph,
        &Inputs {
            destination: Destination::Publication {
                coverage: coverage.clone(),
            },
            views: vec![*view],
            worktree: f.repo.tree_id(child.sha.parse()?)?.to_string(),
            checkpoint: child,
            inherited: b3.clone(),
            replacement: b3,
            integrated: None,
            resolutions: vec![],
        },
    )?
    else {
        panic!("The cancelled B prefix should leave its new A parent and W intact");
    };
    assert_eq!(f.names(&prepared.base)?, ["a", "borrowed", "chosen"]);
    assert!(!prepared.base_inherits.contains_key("B"));
    assert_eq!(prepared.base_inherits["W"], borrowed.inherited());
    let base = f
        .repo
        .context_commit(prepared.base.parse()?, alt.sha.parse()?, "Own new base")?;
    let point = Point {
        views: prepared.views,
        track: "B-alt".into(),
        number: 2,
        sha: f
            .repo
            .context_commit(prepared.worktree.parse()?, base, "Own normalized B-alt")?
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
        panic!("The normalized base should remain usable after saving a checkpoint");
    };
    let target = f.tree(&[("a", "A shared"), ("chosen", "A alternative")])?;
    let Merge::Clean(result) = f
        .repo
        .merge_trees(baseline, target, prepared.worktree.parse()?)?
    else {
        panic!("An independent deletion of W must not conflict with shared B edits");
    };
    assert_eq!(
        f.names(&result.to_string())?,
        ["a", "b-first", "b-later", "chosen", "own"]
    );
    Ok(())
}
