use std::{fs, path::Path, process::Command};

use intercat_git::{Merge, ObjectId, Repository, Result};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Fixture"]);
    git(
        dir.path(),
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::write(dir.path().join("file.txt"), "one\ntwo\nthree\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "--quiet", "-m", "base"]);
    dir
}

#[test]
fn remote_hosts_include_fetch_push_and_rewrites_but_never_local_paths() -> Result<()> {
    let dir = fixture();
    git(
        dir.path(),
        &[
            "remote",
            "add",
            "origin",
            "git@code.example.invalid:team/repo.git",
        ],
    );
    git(
        dir.path(),
        &[
            "config",
            "remote.origin.pushurl",
            "https://push.example.invalid/repo.git",
        ],
    );
    git(dir.path(), &["remote", "add", "alias", "team:repo.git"]);
    git(
        dir.path(),
        &[
            "config",
            "url.https://rewritten.example.invalid/.insteadOf",
            "team:",
        ],
    );
    git(
        dir.path(),
        &["remote", "add", "local", "/private/tmp/example.git"],
    );
    assert_eq!(
        Repository::open(dir.path())?.remote_hosts()?,
        vec![
            "code.example.invalid",
            "push.example.invalid",
            "rewritten.example.invalid"
        ]
    );
    Ok(())
}

fn author() -> gix::actor::Signature {
    gix::actor::Signature {
        name: "Fixture".into(),
        email: "fixture@example.invalid".into(),
        time: gix::date::Time::now_local_or_utc(),
    }
}

#[test]
fn cas_replay_rejects_stale_updates_and_protects_user_refs() -> Result<()> {
    let dir = fixture();
    let repo = Repository::open(dir.path())?;
    let first = ObjectId::from_hex(git(dir.path(), &["rev-parse", "HEAD"]).as_bytes())?;
    let blob = repo.write_blob(b"changed\n")?;
    let tree = repo.write_tree(&[("file.txt", blob)])?;
    let second = repo.write_commit(tree, &[first], "change", author())?;
    let reference = "refs/intercat/fixture/tracks/A";
    repo.compare_and_swap(reference, None, first)?;
    repo.compare_and_swap(reference, Some(first), second)?;
    repo.sync_objects(&[second])?;
    repo.sync_ref(reference)?;
    repo.compare_and_swap(reference, Some(first), second)?; // replay
    assert!(
        repo.compare_and_swap(reference, Some(first), first)
            .is_err()
    );
    assert!(
        repo.compare_and_swap("refs/heads/main", Some(first), second)
            .is_err()
    );
    assert_eq!(repo.merge_base(first, second)?, Some(first));
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), first.to_string());
    assert!(repo.delete_ref(reference, first).is_err());
    assert!(repo.delete_ref("refs/heads/main", first).is_err());
    git(dir.path(), &["pack-refs", "--all"]);
    repo.delete_ref(reference, second)?;
    repo.delete_ref(reference, second)?;
    assert_eq!(repo.reference_target(reference)?, None);
    Ok(())
}

#[test]
fn shallow_clone_cannot_read_old_blobs_and_import_keeps_commit() -> Result<()> {
    let dir = fixture();
    let old_blob = git(dir.path(), &["rev-parse", "HEAD:file.txt"]);
    fs::write(dir.path().join("file.txt"), "new base\n")?;
    git(dir.path(), &["commit", "-am", "new base"]);
    let clone = dir.path().join("integration");
    git(
        dir.path(),
        &[
            "clone",
            "--quiet",
            "--depth",
            "1",
            &format!("file://{}", dir.path().display()),
            clone.to_str().unwrap(),
        ],
    );
    let status = Command::new("git")
        .current_dir(&clone)
        .args(["cat-file", "-e", &old_blob])
        .status()?;
    assert!(!status.success());
    assert!(!clone.join(".git/objects/info/alternates").exists());
    let source = Repository::open(&clone)?;
    let target = Repository::open(dir.path())?;
    let base = ObjectId::from_hex(git(&clone, &["rev-parse", "HEAD"]).as_bytes())?;
    let tree = source.write_tree(&[("file.txt", source.write_blob(b"integration result\n")?)])?;
    let commit = source.write_commit(tree, &[base], "integrated", author())?;
    source.sync_objects(&[commit])?;
    target.import_commit(&source, commit)?;
    target.sync_objects(&[commit])?;
    target.compare_and_swap("refs/heads/intercat/fixture", None, commit)?;
    target.sync_ref("refs/heads/intercat/fixture")?;
    assert_eq!(
        git(dir.path(), &["show", &format!("{commit}:file.txt")]),
        "integration result"
    );
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), base.to_string());
    Ok(())
}

#[test]
fn uses_detached_worktree_index_and_leaves_both_checkouts_untouched() -> Result<()> {
    let dir = fixture();
    let worker = dir.path().join("worker");
    git(
        dir.path(),
        &["worktree", "add", "--detach", worker.to_str().unwrap()],
    );
    fs::write(worker.join("file.txt"), "worker contents\n")?;
    git(&worker, &["add", "file.txt"]);
    let expected = git(&worker, &["write-tree"]);
    let index = git(
        &worker,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    );
    let before = fs::read(&index)?;
    let repo = Repository::open(&worker)?;
    assert_eq!(
        repo.tree_from_index(Path::new(&index))?.to_string(),
        expected
    );
    assert_eq!(fs::read(&index)?, before);
    assert_eq!(
        fs::read_to_string(dir.path().join("file.txt"))?,
        "one\ntwo\nthree\n"
    );
    Ok(())
}

#[test]
fn merge_returns_conflicting_paths_without_running_configured_programs() -> Result<()> {
    let dir = fixture();
    // All programs only write a marker inside this fixture if accidentally invoked.
    let marker = dir.path().join("external-program-ran");
    let program = format!("touch '{}'", marker.display());
    git(dir.path(), &["config", "merge.fixture.driver", &program]);
    git(dir.path(), &["config", "filter.fixture.clean", &program]);
    git(dir.path(), &["config", "filter.fixture.smudge", &program]);
    git(dir.path(), &["config", "core.fsmonitor", &program]);
    git(dir.path(), &["config", "commit.gpgsign", "true"]);
    git(dir.path(), &["config", "gpg.program", &program]);
    // Set the attributes through objects so fixture preparation cannot run filters.
    let repo = Repository::open(dir.path())?;
    let attrs = repo.write_blob(b"file.txt merge=fixture filter=fixture\n")?;
    let base = repo.write_tree(&[
        ("file.txt", repo.write_blob(b"base\n")?),
        (".gitattributes", attrs),
    ])?;
    let ours = repo.write_tree(&[
        ("file.txt", repo.write_blob(b"ours\n")?),
        (".gitattributes", attrs),
    ])?;
    let theirs = repo.write_tree(&[
        ("file.txt", repo.write_blob(b"theirs\n")?),
        (".gitattributes", attrs),
    ])?;
    // Attribute lookup uses the current index/HEAD, so give it the same tracked attributes.
    let commit = repo.write_commit(base, &[], "attribute base", author())?;
    repo.compare_and_swap("refs/heads/intercat/fixture", None, commit)?;
    // Update only this disposable fixture's index through read-tree (no worktree filters).
    // Disable fsmonitor for this fixture-only Git oracle invocation.
    git(
        dir.path(),
        &["-c", "core.fsmonitor=false", "read-tree", &base.to_string()],
    );
    let before_config = fs::read(dir.path().join(".git/config"))?;
    let index_before = fs::read(dir.path().join(".git/index"))?;
    let Merge::Conflicts {
        paths,
        base: recorded_base,
        target: recorded_target,
        incoming: recorded_incoming,
    } = repo.merge_trees(base, ours, theirs)?
    else {
        panic!("conflict expected")
    };
    assert_eq!(
        (recorded_base, recorded_target, recorded_incoming),
        (base, ours, theirs)
    );
    assert_eq!(paths, vec![gix::bstr::BString::from("file.txt")]);
    assert!(!marker.exists());
    assert_eq!(fs::read(dir.path().join(".git/config"))?, before_config);
    assert_eq!(fs::read(dir.path().join(".git/index"))?, index_before);
    let resolution = dir.path().join("resolution.index");
    let prepared = repo.merge_for_resolution(base, ours, theirs, "dummy-merge", &resolution)?;
    assert_eq!(prepared.conflicts, paths);
    let files = repo.tree_files(prepared.tree)?;
    let contents = String::from_utf8(repo.saved_bytes(&files["file.txt"])?)?;
    assert!(contents.contains("<<<<<<< Intercat target dummy-merge\nours\n"));
    assert!(contents.contains(">>>>>>> Intercat change dummy-merge"));
    let stages = gix::index::File::at(
        &resolution,
        gix::hash::Kind::Sha1,
        false,
        Default::default(),
    )?;
    let staged = stages
        .entries()
        .iter()
        .filter(|entry| entry.path(&stages) == "file.txt")
        .map(|entry| entry.stage_raw())
        .collect::<Vec<_>>();
    assert_eq!(staged, [1, 2, 3]);
    assert!(repo.tree_from_index(&resolution).is_err());
    assert!(!marker.exists());
    assert_eq!(fs::read(dir.path().join(".git/config"))?, before_config);
    assert_eq!(fs::read(dir.path().join(".git/index"))?, index_before);
    assert!(
        repo.merge_for_resolution(
            base,
            ours,
            theirs,
            "dummy-merge",
            &dir.path().join(".git/index")
        )
        .is_err()
    );
    let clean = repo.merge_trees(base, ours, base)?;
    assert!(matches!(clean, Merge::Clean(id) if id == ours));
    Ok(())
}

#[test]
fn merge_resolution_keeps_binary_deletion_and_rename_conflicts_unmerged() -> Result<()> {
    let dir = fixture();
    let repo = Repository::open(dir.path())?;
    let original = fs::read(dir.path().join(".git/index"))?;
    let tree = |files: &[(&str, &[u8])]| -> Result<ObjectId> {
        let entries = files
            .iter()
            .map(|(path, bytes)| Ok((*path, repo.write_blob(bytes)?)))
            .collect::<Result<Vec<_>>>()?;
        repo.write_tree(&entries)
    };
    let cases = [
        (
            tree(&[("binary.bin", b"base\0bytes")])?,
            tree(&[("binary.bin", b"ours\0bytes")])?,
            tree(&[("binary.bin", b"theirs\0bytes")])?,
        ),
        (
            tree(&[("file.txt", b"base\n")])?,
            tree(&[])?,
            tree(&[("file.txt", b"theirs\n")])?,
        ),
        (
            tree(&[("old.txt", b"base\n")])?,
            tree(&[("ours.txt", b"base\n")])?,
            tree(&[("theirs.txt", b"base\n")])?,
        ),
    ];
    for (number, (base, ours, theirs)) in cases.into_iter().enumerate() {
        let resolution = dir.path().join(format!("resolution-{number}.index"));
        let prepared = repo.merge_for_resolution(base, ours, theirs, "dummy-merge", &resolution)?;
        assert!(!prepared.conflicts.is_empty(), "case {number}");
        assert!(repo.tree_from_index(&resolution).is_err(), "case {number}");
        assert_eq!(fs::read(dir.path().join(".git/index"))?, original);
    }
    Ok(())
}

#[test]
fn merge_uses_attributes_from_the_target_tree_instead_of_the_checkout_index() -> Result<()> {
    let dir = fixture();
    let repo = Repository::open(dir.path())?;
    let base = repo.write_tree(&[("file.txt", repo.write_blob(b"base\n")?)])?;
    let attributes = repo.write_blob(b"file.txt merge=union\n")?;
    let ours = repo.write_tree(&[
        ("file.txt", repo.write_blob(b"ours\n")?),
        (".gitattributes", attributes),
    ])?;
    let theirs = repo.write_tree(&[("file.txt", repo.write_blob(b"theirs\n")?)])?;
    let index = fs::read(dir.path().join(".git/index"))?;
    let Merge::Clean(tree) = repo.merge_trees(base, ours, theirs)? else {
        panic!("the target tree selects the union driver");
    };
    let files = repo.tree_files(tree)?;
    assert_eq!(repo.saved_bytes(&files["file.txt"])?, b"ours\ntheirs\n");
    assert_eq!(
        repo.saved_bytes(&files[".gitattributes"])?,
        b"file.txt merge=union\n"
    );
    assert_eq!(fs::read(dir.path().join(".git/index"))?, index);
    assert!(!dir.path().join(".gitattributes").exists());
    Ok(())
}

#[test]
fn plumbing_merge_keeps_the_validated_writer_and_rejects_git_metadata_paths() -> Result<()> {
    let dir = fixture();
    let repo = Repository::open(dir.path())?;
    let base = repo.write_tree(&[("file.txt", repo.write_blob(b"base\n")?)])?;
    // Deliberately bypass the validated helper to simulate an unsafe incoming
    // tree from another Git writer. The merge result must not accept it.
    let raw = gix::open(dir.path())?;
    let unsafe_tree = raw
        .write_object(&gix::objs::Tree {
            entries: vec![gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: ".git".into(),
                oid: repo.write_blob(b"unsafe\n")?,
            }],
        })?
        .detach();
    assert!(repo.merge_trees(base, base, unsafe_tree).is_err());
    Ok(())
}
