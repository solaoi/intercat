use intercat_daemon::{
    Result,
    branch_choices::{Family, Fork, Member},
    branch_points::Origin,
    integration_record::Record,
    lineage::{Frozen, Point},
    replacement_review::Review,
};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
fn point(track: &str, number: u64, parents: &[Point]) -> Point {
    let mut inherits = BTreeMap::new();
    for parent in parents {
        inherits.extend(parent.inherits.clone());
        inherits.insert(parent.track.clone(), parent.inherited());
    }
    Point {
        views: Vec::new(),
        track: track.into(),
        number,
        sha: format!(
            "{:040x}",
            track.bytes().map(u64::from).sum::<u64>() * 100 + number
        ),
        parents: parents
            .iter()
            .map(|p| (p.track.clone(), p.inherited()))
            .collect(),
        base_at: "0".repeat(40),
        base_inherits_at: Default::default(),
        inherits,
    }
}
fn review(a: &Point, fork: &Point, child: &Point) -> Review {
    Review {
        families: vec![Family {
            root: a.track.clone(),
            members: vec![a, child]
                .into_iter()
                .map(|p| Member {
                    id: p.track.clone(),
                    label: if p.track == a.track { "A" } else { "A2" }.into(),
                    checkpoint: Some(p.inherited()),
                    available: true,
                })
                .collect(),
            forks: vec![Fork {
                kind: intercat_daemon::branch_choices::ForkKind::Branch,
                parent: a.track.clone(),
                child: child.track.clone(),
                checkpoint: fork.inherited(),
                origin: Origin {
                    ended_at: Some(100),
                    timeline_at: Some(100),
                    turn_number: 1,
                },
            }],
        }],
        choices: Default::default(),
    }
}
fn graph(points: &[Point]) -> Frozen {
    Frozen {
        points: points.to_vec(),
        integrated: Default::default(),
    }
}
fn selection(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}
fn receipt(point: &Point, position: u64) -> Record {
    let job = Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("dummy/{position}").as_bytes());
    let sha = format!("{:040x}", position + 9000);
    let mut after = point.inherits.clone();
    after.insert(point.track.clone(), point.inherited());
    Record {
        id: Uuid::new_v5(&job, sha.as_bytes()),
        job_id: job,
        operation: Uuid::new_v5(&job, b"publish"),
        session_id: "own-session".into(),
        folder_id: "own-folder".into(),
        track_id: point.track.clone(),
        checkpoint: point.inherited(),
        ordinal: position,
        position,
        reference: format!("refs/intercat/dummy/integration/{}/{position}", point.track),
        sha,
        parent: "1".repeat(40),
        tree: "2".repeat(40),
        coverage_before: Default::default(),
        coverage_after: after,
    }
}

#[test]
fn replacing_a_commit_cannot_remove_context_extended_by_a_kept_publication() -> Result<()> {
    use intercat_daemon::replacement_delta::{self, Boundary, Selection};
    let x1 = point("x", 1, &[]);
    let x2 = point("x", 2, &[]);
    let x4 = point("x", 4, &[]);
    let a = point("a", 1, std::slice::from_ref(&x2));
    let mut removed = receipt(&a, 1);
    removed.coverage_before.insert("x".into(), x1.inherited());
    let mut kept = receipt(&x4, 2);
    kept.coverage_before = removed.coverage_after.clone();
    kept.coverage_after.extend(removed.coverage_after.clone());
    kept.coverage_after.insert("x".into(), x4.inherited());
    let sources = BTreeMap::from([("a".into(), Boundary::EntireTrack)]);
    let records = [removed.clone(), kept.clone()];
    assert!(
        matches!(
            replacement_delta::select(&records, &sources, &Default::default())?,
            Selection::Mixed(_)
        ),
        "X4 only supplied X2..X4; undoing A would also remove the X1..X2 context on which it relies"
    );
    let sources = BTreeMap::from([
        ("a".into(), Boundary::EntireTrack),
        ("x".into(), Boundary::EntireTrack),
    ]);
    assert!(
        matches!(replacement_delta::select(&records, &sources, &Default::default())?, Selection::Ready(selected) if selected.len()==2)
    );
    // A receipt whose files have already been undone creates no new blocker.
    assert!(matches!(replacement_delta::select(&records,
        &BTreeMap::from([("a".into(), Boundary::EntireTrack)]), &BTreeSet::from([removed.id]))?, Selection::Ready(selected) if selected.is_empty()));
    Ok(())
}
#[test]
fn review_requires_a_choice_for_a_selected_path_or_its_dependent_but_not_unrelated_work()
-> Result<()> {
    let a1 = point("a", 1, &[]);
    let a2 = point("a", 2, &[]);
    let alt = point("alt", 1, std::slice::from_ref(&a1));
    let b = point("b", 1, std::slice::from_ref(&a2));
    let x = point("x", 1, &[]);
    let current = vec![a2.clone(), alt.clone(), b.clone(), x];
    let g = graph(&[a1.clone(), a2.clone(), alt.clone(), b]);
    let mut g = g;
    g.points.push(current[3].clone());
    let r = review(&a2, &a1, &alt);
    assert_eq!(
        r.evaluate(&g, &current, &selection(&["b"]), &[], &Default::default())?
            .pending,
        vec!["a"]
    );
    assert_eq!(
        r.evaluate(&g, &current, &selection(&["alt"]), &[], &Default::default())?
            .pending,
        vec!["a"]
    );
    assert!(
        r.evaluate(&g, &current, &selection(&["x"]), &[], &Default::default())?
            .ready()
    );
    Ok(())
}
#[test]
fn affected_chain_is_ordered_by_direct_parents_and_excluded_children_remain_visible() -> Result<()>
{
    let a1 = point("a", 1, &[]);
    let a3 = point("a", 3, &[]);
    let alt = point("alt", 1, std::slice::from_ref(&a1));
    let b = point("b", 2, std::slice::from_ref(&a3));
    let c = point("c", 1, std::slice::from_ref(&b));
    let current = vec![a3.clone(), alt.clone(), c.clone(), b.clone()];
    let g = graph(&[a1.clone(), a3.clone(), alt.clone(), b, c]);
    let mut r = review(&a3, &a1, &alt);
    r.choose("alt")?;
    let e = r.evaluate(
        &g,
        &current,
        &selection(&["alt", "c"]),
        &[],
        &Default::default(),
    )?;
    assert!(!e.ready());
    assert_eq!(
        e.descendants
            .iter()
            .map(|d| d.track.as_str())
            .collect::<Vec<_>>(),
        vec!["b", "c"]
    );
    assert!(!e.descendants[0].included);
    assert!(e.descendants[1].included);
    assert_eq!(e.descendants[0].parents[0].onto, "alt");
    assert_eq!(e.descendants[0].parents[0].checkpoint.number, 3);
    assert_eq!(e.descendants[1].parents[0].onto, "b");
    assert!(
        r.evaluate(&g, &current, &selection(&["alt"]), &[], &Default::default())?
            .ready()
    );
    Ok(())
}
#[test]
fn choosing_the_original_retires_a_siblings_own_first_checkpoint_and_blocks_a_mixed_child()
-> Result<()> {
    let a = point("a", 1, &[]);
    let alt = point("alt", 1, std::slice::from_ref(&a));
    let b = point("b", 1, std::slice::from_ref(&alt));
    let current = vec![a.clone(), alt.clone(), b.clone()];
    let g = graph(&current);
    let mut r = review(&a, &a, &alt);
    r.choose("a")?;
    let own = receipt(&alt, 1);
    let child = receipt(&b, 2);
    let e = r.evaluate(
        &g,
        &current,
        &selection(&["a"]),
        &[own.clone(), child.clone()],
        &Default::default(),
    )?;
    assert!(!e.ready());
    assert!(e.records.is_empty());
    assert_eq!(e.mixed.len(), 1);
    assert_eq!(e.mixed[0].track, "b");
    let e = r.evaluate(
        &g,
        &current,
        &selection(&["a"]),
        &[own.clone(), child.clone()],
        &BTreeSet::from([child.id]),
    )?;
    assert!(e.ready());
    assert_eq!(e.records, vec![own]);
    Ok(())
}
#[test]
fn shared_fork_is_not_a_later_change_and_already_cancelled_receipts_are_not_selected() -> Result<()>
{
    let a = point("a", 1, &[]);
    let alt = point("alt", 1, std::slice::from_ref(&a));
    let b = point("b", 1, std::slice::from_ref(&a));
    let current = vec![a.clone(), alt.clone(), b];
    let g = graph(&current);
    let mut r = review(&a, &a, &alt);
    r.choose("alt")?;
    let own = receipt(&a, 1);
    let child = receipt(&current[2], 2);
    let e = r.evaluate(
        &g,
        &current,
        &selection(&["alt", "b"]),
        &[own.clone(), child.clone()],
        &Default::default(),
    )?;
    assert!(e.ready());
    assert_eq!(e.records, vec![own.clone()]);
    assert!(e.descendants.is_empty());
    let e = r.evaluate(
        &g,
        &current,
        &selection(&["alt", "b"]),
        &[own.clone(), child],
        &BTreeSet::from([own.id]),
    )?;
    assert!(e.ready());
    assert!(e.records.is_empty());
    Ok(())
}
#[test]
fn saved_choices_reject_forged_boundaries_stale_points_and_a_chosen_path_with_retired_changes()
-> Result<()> {
    let a1 = point("a", 1, &[]);
    let a3 = point("a", 3, &[]);
    let alt = point("alt", 1, std::slice::from_ref(&a1));
    let current = vec![a3.clone(), alt.clone()];
    let g = graph(&[a1.clone(), a3.clone(), alt.clone()]);
    let mut r = review(&a3, &a1, &alt);
    r.choose("alt")?;
    let saved: Review = serde_json::from_value(serde_json::to_value(&r)?)?;
    assert!(
        saved
            .evaluate(&g, &current, &selection(&["alt"]), &[], &Default::default())?
            .ready()
    );
    assert!(
        saved
            .evaluate(
                &g,
                &current,
                &selection(&["a", "alt"]),
                &[],
                &Default::default()
            )
            .is_err()
    );
    r.choices.get_mut("a").unwrap().retired.clear();
    assert!(
        r.evaluate(&g, &current, &selection(&["alt"]), &[], &Default::default())
            .is_err()
    );
    let mut changed = current.clone();
    changed[1].number += 1;
    assert!(
        saved
            .evaluate(&g, &changed, &selection(&["alt"]), &[], &Default::default())
            .is_err()
    );
    let alt = point("alt", 2, std::slice::from_ref(&a3));
    let mut r = review(&a3, &a1, &alt);
    r.choose("alt")?;
    let g = graph(&[a1, a3.clone(), alt.clone()]);
    assert!(
        r.evaluate(
            &g,
            &[a3, alt],
            &selection(&["alt"]),
            &[],
            &Default::default()
        )
        .is_err()
    );
    Ok(())
}
#[test]
fn reparented_parent_can_be_current_while_its_child_still_needs_its_new_checkpoint() -> Result<()> {
    let a1 = point("a", 1, &[]);
    let a3 = point("a", 3, &[]);
    let alt = point("alt", 1, std::slice::from_ref(&a1));
    let b1 = point("b", 1, std::slice::from_ref(&a3));
    let b2 = point("b", 2, std::slice::from_ref(&alt));
    let c = point("c", 1, std::slice::from_ref(&b1));
    let current = vec![a3.clone(), alt.clone(), b2.clone(), c.clone()];
    let g = graph(&[a1.clone(), a3.clone(), alt.clone(), b1, b2, c]);
    let mut r = review(&a3, &a1, &alt);
    r.choose("alt")?;
    let e = r.evaluate(
        &g,
        &current,
        &selection(&["alt", "b", "c"]),
        &[],
        &Default::default(),
    )?;
    assert_eq!(e.descendants.len(), 1);
    assert_eq!(e.descendants[0].track, "c");
    assert_eq!(e.descendants[0].parents[0].onto, "b");
    assert_eq!(e.descendants[0].parents[0].checkpoint.number, 1);
    Ok(())
}

#[test]
fn a_chosen_path_in_another_family_can_be_an_affected_descendant() -> Result<()> {
    let a1 = point("a", 1, &[]);
    let a3 = point("a", 3, &[]);
    let alt = point("alt", 1, std::slice::from_ref(&a1));
    let b = point("b", 1, std::slice::from_ref(&a3));
    let b_alt = point("b_alt", 1, std::slice::from_ref(&b));
    let current = vec![a3.clone(), alt.clone(), b.clone(), b_alt.clone()];
    let g = graph(&[
        a1.clone(),
        a3.clone(),
        alt.clone(),
        b.clone(),
        b_alt.clone(),
    ]);
    let mut r = review(&a3, &a1, &alt);
    let other = review(&b, &b, &b_alt);
    r.families.extend(other.families);
    r.choose("alt")?;
    r.choose("b")?;
    let evaluated = r.evaluate(
        &g,
        &current,
        &selection(&["alt", "b"]),
        &[],
        &Default::default(),
    )?;
    assert!(!evaluated.ready());
    assert_eq!(evaluated.descendants.len(), 1);
    assert_eq!(evaluated.descendants[0].track, "b");
    assert_eq!(evaluated.descendants[0].parents[0].onto, "alt");
    Ok(())
}
