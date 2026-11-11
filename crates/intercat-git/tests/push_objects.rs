use intercat_git::{ObjectId, Repository, Result};
use std::{fs, sync::atomic::AtomicBool};

fn signature() -> gix::actor::Signature {
    gix::actor::Signature {
        name: "Dummy Push".into(),
        email: "dummy@example.invalid".into(),
        time: gix::date::Time::now_local_or_utc(),
    }
}
#[test]
fn transport_history_is_independent_and_preserves_merge_parents_and_old_gitlinks() -> Result<()> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    Repository::init_isolated(&source)?;
    let repo = Repository::open(&source)?;
    let base = repo.empty_starting_point()?;
    let blob = repo.write_blob(b"Dummy current content\n")?;
    let tree = repo.write_tree(&[("dummy.txt", blob)])?;
    let left = repo.checkpoint_commit(tree, base, "Dummy left")?;
    let ghost: ObjectId = "ab".repeat(20).parse()?;
    let raw = gix::open(&source)?;
    let old_tree = raw
        .write_object(&gix::objs::Tree {
            entries: vec![gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Commit.into(),
                filename: "old-submodule".into(),
                oid: ghost,
            }],
        })?
        .detach();
    let right = repo.checkpoint_commit(old_tree, base, "Dummy old gitlink")?;
    assert!(repo.tree_files(old_tree).is_err());
    assert!(repo.push_tree_files(old_tree)?.is_empty());
    let head = repo.write_commit(
        tree,
        &[left, right],
        "Dummy merge removes old gitlink",
        signature(),
    )?;
    repo.compare_and_swap("refs/heads/intercat/dummy", None, head)?;
    let unrelated_tree =
        repo.write_tree(&[("private.txt", repo.write_blob(b"Dummy unrelated data")?)])?;
    let unrelated = repo.checkpoint_commit(unrelated_tree, base, "Dummy unrelated branch")?;
    repo.compare_and_swap("refs/heads/intercat/private", None, unrelated)?;
    repo.sync_objects(&[base, left, right, head])?;
    let source_config = fs::read(source.join("config"))?;
    let destination = root.path().join("transport");
    Repository::init_isolated(&destination)?;
    let snapshot = Repository::open(&destination)?;
    let copied = snapshot.import_push_history(&repo, head, &AtomicBool::new(false))?;
    assert_eq!(copied.commits.len(), 4);
    assert!(copied.shallow.is_empty());
    assert!(snapshot.commit_details(unrelated).is_err());
    assert!(
        snapshot
            .push_commits(head, &[head], &AtomicBool::new(false))?
            .is_empty()
    );
    let range = snapshot.push_commits(head, &[left], &AtomicBool::new(false))?;
    assert_eq!(range.len(), 2);
    assert!(range.contains(&head));
    assert!(range.contains(&right));
    assert!(!range.contains(&base));
    assert!(!range.contains(&left));
    let full = snapshot.push_commits(head, &[ghost], &AtomicBool::new(false))?;
    assert_eq!(full.len(), 4);
    assert!(
        snapshot
            .push_commits(head, &[], &AtomicBool::new(true))
            .is_err()
    );
    snapshot.compare_and_swap("refs/heads/intercat/send", None, head)?;
    snapshot.sync_ref("refs/heads/intercat/send")?;
    drop(repo);
    drop(raw);
    fs::rename(source.join("objects"), source.join("objects-unavailable"))?;
    assert_eq!(snapshot.commit_details(head)?.parents, vec![left, right]);
    assert_eq!(snapshot.commit_details(right)?.tree, old_tree);
    assert!(snapshot.commit_details(base)?.parents.is_empty());
    let files = snapshot.tree_files(tree)?;
    assert_eq!(
        snapshot.saved_bytes(&files["dummy.txt"])?,
        b"Dummy current content\n"
    );
    assert!(!destination.join("objects/info/alternates").exists());
    assert_eq!(fs::read(source.join("config"))?, source_config);
    Ok(())
}

#[test]
fn transport_history_keeps_a_shallow_boundary_and_stops_before_copy_when_cancelled() -> Result<()> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    Repository::init_isolated(&source)?;
    let repo = Repository::open(&source)?;
    let base = repo.empty_starting_point()?;
    let blob = repo.write_blob(b"Dummy shallow content\n")?;
    let head = repo.checkpoint_commit(
        repo.write_tree(&[("dummy.txt", blob)])?,
        base,
        "Dummy shallow head",
    )?;
    repo.sync_objects(&[base, head])?;
    fs::write(source.join("shallow"), format!("{head}\n"))?;
    drop(repo);
    let repo = Repository::open(&source)?;
    let destination = root.path().join("transport");
    Repository::init_isolated(&destination)?;
    let snapshot = Repository::open(&destination)?;
    assert!(
        snapshot
            .import_push_history(&repo, head, &AtomicBool::new(true))
            .is_err()
    );
    assert!(snapshot.commit_details(head).is_err());
    let copied = snapshot.import_push_history(&repo, head, &AtomicBool::new(false))?;
    assert_eq!(copied.commits, vec![head]);
    assert_eq!(copied.shallow, vec![head]);
    assert_eq!(
        fs::read_to_string(destination.join("shallow"))?,
        format!("{head}\n")
    );
    assert!(snapshot.commit_details(base).is_err());
    assert_eq!(
        fs::read_to_string(source.join("shallow"))?,
        format!("{head}\n")
    );
    Ok(())
}
