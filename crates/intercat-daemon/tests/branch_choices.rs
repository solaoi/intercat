use intercat_daemon::{
    Result,
    branch::{Branch, InstructionMode, Phase, Request},
    branch_choices::{self, Choice, Family, Member},
    branch_points::Origin,
    checkpoint::Checkpoint,
    lineage::{Frozen, Point},
    preparation::Workspace,
    replacement_delta::Boundary,
};
use uuid::Uuid;

fn point(id: &str, number: u64, object: u64) -> Point {
    Point {
        views: Vec::new(),
        parents: Default::default(),
        track: id.into(),
        number,
        sha: format!("{object:040x}"),
        base_at: "0".repeat(40),
        base_inherits_at: Default::default(),
        inherits: Default::default(),
    }
}
fn member(id: &str, label: &str, saved: &Point) -> Member {
    Member {
        id: id.into(),
        label: label.into(),
        checkpoint: Some(saved.inherited()),
        available: true,
    }
}
fn branch(parent: &str, child: &str, label: &str, fork: &Point) -> Branch {
    let turn = Uuid::now_v7();
    Branch {
        request: Request {
            op_id: Uuid::now_v7(),
            parent_track_id: parent.into(),
            point: intercat_daemon::branch_start::Selection::Turn { turn_id: turn },
            instructions: InstructionMode::Checkpoint,
        },
        track_id: child.into(),
        parent_session_id: "own-dummy-parent-msp-session".into(),
        session_id: Some("own-dummy-child-msp-session".into()),
        workspace: Workspace {
            track_id: child.into(),
            path: format!("/private/tmp/intercat-branch-choice-dummy/{child}").into(),
            reference: format!("refs/intercat/test/tracks/{label}"),
            base: fork.sha.clone(),
            state: "ready".into(),
            error: None,
            denied_roots: vec![],
            attachment_roots: vec![],
        },
        point: intercat_daemon::branch_start::Point::Turn {
            checkpoint: Checkpoint {
                turn_id: turn,
                sha: fork.sha.clone(),
                reference: format!("refs/intercat/test/tracks/{parent}-checkpoints/{turn}"),
                unmerged_paths: vec![],
                live_processes: false,
            },
        },
        phase: Phase::Ready,
        instructions: None,
        origin: Origin {
            ended_at: Some(1000),
            timeline_at: Some(900),
            turn_number: 99,
        },
    }
}
fn graph(points: Vec<Point>) -> Frozen {
    Frozen {
        points,
        integrated: Default::default(),
    }
}
#[test]
fn a_choice_uses_the_saved_checkpoint_number_instead_of_turn_count_or_latest_parent() -> Result<()>
{
    let fork = point("a", 2, 2);
    let later = point("a", 5, 5);
    let child = point("a2", 1, 11);
    let members = vec![member("a", "A", &later), member("a2", "A2", &child)];
    let branches = vec![branch("a", "a2", "A2", &fork)];
    let families = branch_choices::capture(
        &members,
        &graph(vec![fork.clone(), later, child]),
        "test",
        &branches,
        &[],
    )?;
    assert_eq!(families.len(), 1);
    assert_eq!(families[0].forks[0].checkpoint, fork.inherited());
    assert_eq!(families[0].forks[0].origin.turn_number, 99);
    let choice = families[0].choose("a2")?;
    assert_eq!(
        choice.retired,
        std::collections::BTreeMap::from([("a".into(), Boundary::Fork(2))])
    );
    let saved_choice: Choice = serde_json::from_value(serde_json::to_value(&choice)?)?;
    assert_eq!(saved_choice, choice);
    let decoded: Family = serde_json::from_value(serde_json::to_value(&families[0])?)?;
    assert_eq!(decoded.choose("a2")?, choice);
    Ok(())
}
#[test]
fn choosing_a_nested_path_keeps_each_fork_context_and_retires_siblings_in_full() -> Result<()> {
    let a2 = point("a", 2, 2);
    let a4 = point("a", 4, 4);
    let a5 = point("a", 5, 5);
    let child2 = point("a2", 2, 12);
    let child3 = point("a2", 3, 13);
    let leaf = point("a3", 1, 21);
    let sibling = point("a4", 1, 31);
    let members = vec![
        member("a", "A", &a5),
        member("a2", "A2", &child3),
        member("a3", "A3", &leaf),
        member("a4", "A4", &sibling),
    ];
    let branches = vec![
        branch("a", "a2", "A2", &a2),
        branch("a2", "a3", "A3", &child2),
        branch("a", "a4", "A4", &a4),
    ];
    let frames = branch_choices::capture(
        &members,
        &graph(vec![a2, a4, a5, child2, child3, leaf, sibling]),
        "test",
        &branches,
        &[],
    )?;
    let family = &frames[0];
    let choice = family.choose("a3")?;
    assert_eq!(
        choice.retired,
        std::collections::BTreeMap::from([
            ("a".into(), Boundary::Fork(2)),
            ("a2".into(), Boundary::Fork(2)),
            ("a4".into(), Boundary::EntireTrack)
        ])
    );
    assert!(
        family
            .choose("a")?
            .retired
            .values()
            .all(|cut| *cut == Boundary::EntireTrack)
    );
    let middle = family.choose("a2")?;
    assert_eq!(middle.retired["a"], Boundary::Fork(2));
    assert_eq!(middle.retired["a3"], Boundary::EntireTrack);
    assert_eq!(middle.retired["a4"], Boundary::EntireTrack);
    Ok(())
}
#[test]
fn an_unsealed_or_unsaved_path_is_visible_but_cannot_be_chosen() -> Result<()> {
    let a = point("a", 1, 1);
    let child = point("a2", 1, 2);
    let mut members = vec![member("a", "A", &a), member("a2", "A2", &child)];
    let mut fork = branch("a", "a2", "A2", &a);
    fork.phase = Phase::AwaitingReceipt;
    let graph = graph(vec![a, child]);
    let family = branch_choices::capture(&members, &graph, "test", &[fork.clone()], &[])?.remove(0);
    assert!(!family.members[1].available);
    assert!(family.choose("a2").is_err());
    assert_eq!(family.choose("a")?.retired["a2"], Boundary::EntireTrack);
    members[1].checkpoint = None;
    members[1].available = false;
    fork.phase = Phase::Ready;
    let family = branch_choices::capture(&members, &graph, "test", &[fork], &[])?.remove(0);
    assert!(family.choose("a2").is_err());
    assert!(family.choose("not-in-family").is_err());
    Ok(())
}
#[test]
fn stale_scope_duplicate_sources_and_branch_cycles_are_rejected() -> Result<()> {
    let a = point("a", 1, 1);
    let child = point("a2", 1, 2);
    let members = vec![member("a", "A", &a), member("a2", "A2", &child)];
    let graph = graph(vec![a.clone(), child.clone()]);
    let fork = branch("a", "a2", "A2", &a);
    let mut other = fork.clone();
    other.workspace.reference = "refs/intercat/other/tracks/A2".into();
    assert!(branch_choices::capture(&members, &graph, "test", &[other], &[]).is_err());
    assert!(
        branch_choices::capture(&members, &graph, "test", &[fork.clone(), fork.clone()], &[])
            .is_err()
    );
    let mut other = fork.clone();
    let intercat_daemon::branch_start::Point::Turn { checkpoint } = &mut other.point else {
        panic!("Expected a completed turn")
    };
    checkpoint.sha = "f".repeat(40);
    assert!(branch_choices::capture(&members, &graph, "test", &[other], &[]).is_err());
    let cycle = branch("a2", "a", "A", &child);
    assert!(branch_choices::capture(&members, &graph, "test", &[fork, cycle], &[]).is_err());
    Ok(())
}
#[test]
fn unrelated_tracks_and_branches_outside_the_review_do_not_join_a_family() -> Result<()> {
    let a = point("a", 1, 1);
    let child = point("a2", 1, 2);
    let b = point("b", 1, 3);
    let members = vec![
        member("b", "B", &b),
        member("a", "A", &a),
        member("a2", "A2", &child),
    ];
    let branches = vec![
        branch("a", "a2", "A2", &a),
        branch("elsewhere", "elsewhere2", "Z2", &a),
    ];
    let frames =
        branch_choices::capture(&members, &graph(vec![a, child, b]), "test", &branches, &[])?;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].root, "a");
    assert_eq!(
        frames[0]
            .members
            .iter()
            .map(|member| member.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "a2"]
    );
    assert!(frames[0].choose("b").is_err());
    Ok(())
}

#[test]
fn independent_families_keep_their_display_order_and_selection_scope() -> Result<()> {
    let a = point("a", 1, 1);
    let a2 = point("a2", 1, 2);
    let b = point("b", 1, 3);
    let b2 = point("b2", 1, 4);
    let members = vec![
        member("b", "B", &b),
        member("b2", "B2", &b2),
        member("a", "A", &a),
        member("a2", "A2", &a2),
    ];
    let branches = vec![branch("a", "a2", "A2", &a), branch("b", "b2", "B2", &b)];
    let frames =
        branch_choices::capture(&members, &graph(vec![a, a2, b, b2]), "test", &branches, &[])?;
    assert_eq!(
        frames
            .iter()
            .map(|family| family.root.as_str())
            .collect::<Vec<_>>(),
        ["b", "a"]
    );
    assert_eq!(
        frames[0].choose("b2")?.retired,
        std::collections::BTreeMap::from([("b".into(), Boundary::Fork(1))])
    );
    assert!(frames[0].choose("a2").is_err());
    Ok(())
}

fn continuation(
    parent: &str,
    child: &str,
    point: &Point,
) -> intercat_daemon::continuation::Continuation {
    intercat_daemon::continuation::Continuation {
        operation: Uuid::now_v7(),
        source: parent.into(),
        successor: child.into(),
        source_session: Uuid::now_v7().to_string(),
        session_id: Uuid::now_v7().to_string(),
        board_session: "own-board".into(),
        folder_id: "own-folder".into(),
        checkpoint: Checkpoint {
            turn_id: Uuid::now_v7(),
            sha: point.sha.clone(),
            reference: format!("refs/own-dummy/{parent}"),
            unmerged_paths: vec![],
            live_processes: false,
        },
        origin: Origin {
            ended_at: Some(1000),
            timeline_at: Some(900),
            turn_number: i64::try_from(point.number).unwrap(),
        },
        first_input: Uuid::now_v7().to_string(),
        start_command: Uuid::now_v7(),
        opened: true,
        ready: true,
    }
}
#[test]
fn a_continuation_keeps_the_saved_prefix_and_multiple_segments_do_not_erase_a_real_alternate()
-> Result<()> {
    let fork = point("a", 2, 2);
    let old = point("a", 5, 5);
    let alternate = point("a2", 1, 21);
    let middle = point("a3", 1, 31);
    let current = point("a4", 1, 41);
    let mut members = vec![
        member("a", "A", &old),
        member("a2", "A2", &alternate),
        member("a3", "A3", &middle),
        member("a4", "A4", &current),
    ];
    members[0].available = false;
    members[2].available = false;
    let continuations = vec![
        continuation("a", "a3", &old),
        continuation("a3", "a4", &middle),
    ];
    let family = branch_choices::capture(
        &members,
        &graph(vec![fork.clone(), old, middle, alternate, current]),
        "test",
        &[branch("a", "a2", "A2", &fork)],
        &continuations,
    )?
    .remove(0);
    assert!(family.has_alternatives());
    assert!(family.choose("a").is_err());
    let kept = family.choose("a4")?;
    assert!(!kept.retired.contains_key("a"));
    assert!(!kept.retired.contains_key("a3"));
    assert_eq!(kept.retired["a2"], Boundary::EntireTrack);
    let alternate = family.choose("a2")?;
    assert_eq!(alternate.retired["a"], Boundary::Fork(2));
    assert_eq!(alternate.retired["a3"], Boundary::EntireTrack);
    assert_eq!(alternate.retired["a4"], Boundary::EntireTrack);
    Ok(())
}
#[test]
fn a_continuation_is_not_an_alternative_and_an_open_source_cannot_claim_one() -> Result<()> {
    let old = point("a", 4, 4);
    let current = point("a2", 1, 21);
    let mut members = vec![member("a", "A", &old), member("a2", "A2", &current)];
    let continuations = vec![continuation("a", "a2", &old)];
    let frozen = graph(vec![old, current]);
    assert!(branch_choices::capture(&members, &frozen, "test", &[], &continuations).is_err());
    members[0].available = false;
    let family = branch_choices::capture(&members, &frozen, "test", &[], &continuations)?.remove(0);
    assert!(!family.has_alternatives());
    assert!(family.choose("a2")?.retired.is_empty());
    Ok(())
}
