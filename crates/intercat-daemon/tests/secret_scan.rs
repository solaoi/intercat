use intercat_daemon::{Result, secret_scan::Engine};
use intercat_git::Repository;
use std::{fs, path::Path, process::Command};
use tokio::sync::watch;

const TOKEN: &str = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");

#[tokio::test]
async fn completed_display_scans_before_truncating_a_credential_at_the_cut() -> Result<()> {
    let scanner = Engine::capture()?.start(&[])?;
    let text = format!(
        "{}{}\n{}",
        "x\n".repeat(32_758),
        TOKEN,
        "y\n".repeat(600_000)
    );
    let value = intercat_daemon::recording::redact(
        &scanner,
        serde_json::json!({
            "method":"item/completed", "params":{"text":text}
        }),
    )
    .await?;
    let output = value["params"]["text"].as_str().unwrap();
    assert!(!output.contains("ghp_"));
    assert!(output.contains("[redacted]"));
    assert!(output.len() < 1024 * 1024);
    assert_eq!(value["_intercatTruncated"][0], "/params/text");
    Ok(())
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn repository(files: &[(&str, &str)]) -> Result<tempfile::TempDir> {
    let root = tempfile::tempdir()?;
    git(root.path(), &["init", "-q", "-b", "main"]);
    git(root.path(), &["config", "user.name", "Fixture"]);
    git(
        root.path(),
        &["config", "user.email", "fixture@example.invalid"],
    );
    for (path, text) in files {
        fs::write(root.path().join(path), text)?;
    }
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "base", "--allow-empty"]);
    Ok(root)
}
fn commit(root: &Path, message: &str) -> intercat_git::ObjectId {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", message, "--allow-empty"]);
    git(root, &["rev-parse", "HEAD"]).parse().unwrap()
}

fn lfs_file(root: &Path, name: &str, bytes: &[u8]) -> Result<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    let oid = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let object = root
        .join(".git/lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(&oid);
    fs::create_dir_all(object.parent().unwrap())?;
    fs::write(&object, bytes)?;
    fs::write(
        root.join(name),
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
            bytes.len()
        ),
    )?;
    Ok(object)
}

#[tokio::test]
async fn lfs_text_scans_payload_added_lines_and_known_check_values() -> Result<()> {
    let root = repository(&[])?;
    let old = format!("old_token = '{TOKEN}'\nsafe = 1\n");
    lfs_file(root.path(), "dummy.lfs", old.as_bytes())?;
    let base = commit(root.path(), "dummy LFS base");
    let value = "agent-created-check-value-41";
    let new = format!("{old}added = '{value}'\n");
    lfs_file(root.path(), "dummy.lfs", new.as_bytes())?;
    let head = commit(root.path(), "dummy LFS change");
    let repo = Repository::open(root.path())?;
    let scan = Engine::capture()?
        .start(&[value.into()])?
        .commit(
            &repo,
            base,
            head,
            &root.path().canonicalize()?.join(".git/lfs"),
            watch::channel(false).1,
        )
        .await?;
    assert!(scan.unscanned_lfs.is_empty());
    assert_eq!(scan.findings.len(), 1, "{scan:?}");
    assert_eq!(scan.findings[0].path, "dummy.lfs");
    assert_eq!(scan.findings[0].line, 3);
    assert_eq!(scan.findings[0].rule, "check-environment-value");
    let encoded = serde_json::to_string(&scan)?;
    assert!(!encoded.contains(value));
    assert!(!encoded.contains(TOKEN));
    Ok(())
}

#[tokio::test]
async fn lfs_binary_is_explicitly_unscanned_and_known_values_are_still_detected() -> Result<()> {
    let root = repository(&[])?;
    let base = git(root.path(), &["rev-parse", "HEAD"]).parse()?;
    let value = "agent-created-binary-check-value-83";
    let bytes = format!("\0dummy\0{value}\0").into_bytes();
    let object = lfs_file(root.path(), "dummy-image.lfs", &bytes)?;
    let head = commit(root.path(), "dummy binary LFS change");
    let repo = Repository::open(root.path())?;
    let scanner = Engine::capture()?.start(&[value.into()])?;
    let scan = scanner
        .commit(
            &repo,
            base,
            head,
            &root.path().canonicalize()?.join(".git/lfs"),
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(scan.unscanned_lfs, ["dummy-image.lfs"]);
    assert!(
        scan.findings
            .iter()
            .any(|finding| finding.rule == "check-environment-value")
    );
    assert!(!serde_json::to_string(&scan)?.contains(value));
    fs::write(&object, b"corrupt dummy payload")?;
    let error = scanner
        .commit(
            &repo,
            base,
            head,
            &root.path().canonicalize()?.join(".git/lfs"),
            watch::channel(false).1,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.downcast_ref(), Some(intercat_daemon::lfs_store::Problem::Corrupt { path }) if path == "dummy-image.lfs")
    );
    fs::remove_file(object)?;
    let error = scanner
        .commit(
            &repo,
            base,
            head,
            &root.path().canonicalize()?.join(".git/lfs"),
            watch::channel(false).1,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref(),
        Some(intercat_daemon::lfs_store::Problem::Missing { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn only_added_lines_are_flagged_and_each_intermediate_commit_is_checked() -> Result<()> {
    let initial = format!("old_token = '{TOKEN}'\nsafe = 1\n");
    let root = repository(&[("test.txt", &initial)])?;
    let repo = Repository::open(root.path())?;
    let base = git(root.path(), &["rev-parse", "HEAD"]).parse()?;
    let scanner = Engine::capture()?.start(&[])?;
    fs::write(
        root.path().join("test.txt"),
        initial.replace("safe = 1", "safe = 2"),
    )?;
    let safe = commit(root.path(), "safe edit");
    let scan = scanner
        .commit(
            &repo,
            base,
            safe,
            &root.path().canonicalize()?.join(".git/lfs"),
            watch::channel(false).1,
        )
        .await?;
    assert!(scan.findings.is_empty(), "{scan:?}");
    fs::write(
        root.path().join("new.txt"),
        format!("token = '{TOKEN}' # gitleaks:allow\n"),
    )?;
    fs::write(
        root.path().join(".gitleaks.toml"),
        "[allowlist]\nregexes = ['.*']\n",
    )?;
    let leaked = commit(root.path(), "add fixture token");
    let detected = scanner
        .commit(
            &repo,
            safe,
            leaked,
            &root.path().canonicalize()?.join(".git/lfs"),
            watch::channel(false).1,
        )
        .await?;
    assert!(
        detected
            .findings
            .iter()
            .any(|finding| finding.path == "new.txt"
                && finding.line == 1
                && finding.rule == "github-pat"),
        "{detected:?}"
    );
    fs::remove_file(root.path().join("new.txt"))?;
    let removed = commit(root.path(), "remove fixture token");
    assert!(
        scanner
            .commit(
                &repo,
                leaked,
                removed,
                &root.path().canonicalize()?.join(".git/lfs"),
                watch::channel(false).1
            )
            .await?
            .findings
            .is_empty()
    );
    assert_ne!(detected.binding.tree, scan.binding.tree);
    assert_ne!(detected.fingerprint()?, scan.fingerprint()?);
    let encoded = serde_json::to_string(&detected)?;
    assert!(!encoded.contains(TOKEN));
    assert!(encoded.contains("messageHash"));
    Ok(())
}

#[tokio::test]
async fn final_messages_and_exact_values_are_scanned_and_output_is_redacted() -> Result<()> {
    let root = repository(&[])?;
    let repo = Repository::open(root.path())?;
    let base = git(root.path(), &["rev-parse", "HEAD"]).parse()?;
    let known = "not-a-provider-token\nsecond-line".to_string();
    let scanner = Engine::capture()?.start(std::slice::from_ref(&known))?;
    fs::write(
        root.path().join("fixture.txt"),
        format!("plain\n{known}\nplain\n"),
    )?;
    let head = commit(root.path(), &format!("subject\n\n{TOKEN}"));
    let scan = scanner
        .commit(
            &repo,
            base,
            head,
            &root.path().canonicalize()?.join(".git/lfs"),
            watch::channel(false).1,
        )
        .await?;
    assert!(
        scan.findings
            .iter()
            .any(|finding| finding.path == "commit-message.txt")
    );
    assert!(
        scan.findings
            .iter()
            .any(|finding| finding.rule == "check-environment-value"
                && finding.line == 2
                && finding.end_line == 3)
    );
    assert_eq!(scan.binding.value_fingerprints.len(), 1);
    let texts = [
        format!("before\n日本語 token={TOKEN}\nafter\n"),
        format!("before\n{known}\nafter\n"),
        "token = '${NPM_TOKEN}'\n".into(),
    ];
    let redacted = scanner.redact(&texts, watch::channel(false).1).await?;
    assert_eq!(redacted[0], "before\n[redacted]\nafter\n");
    assert_eq!(redacted[1], "before\n[redacted]\n[redacted]\nafter\n");
    assert_eq!(redacted[2], texts[2]);
    assert!(!serde_json::to_string(&scan)?.contains(&known));
    let changed_values = Engine::capture()?
        .start(&[known, "another-value".into()])?
        .commit(
            &repo,
            base,
            head,
            &root.path().canonicalize()?.join(".git/lfs"),
            watch::channel(false).1,
        )
        .await?;
    assert_ne!(
        scan.binding.value_fingerprints,
        changed_values.binding.value_fingerprints
    );
    git(
        root.path(),
        &["commit", "--amend", "-qm", "new final message"],
    );
    let amended = git(root.path(), &["rev-parse", "HEAD"]).parse()?;
    let changed_message = scanner
        .commit(
            &repo,
            base,
            amended,
            &root.path().canonicalize()?.join(".git/lfs"),
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(scan.binding.tree, changed_message.binding.tree);
    assert_ne!(
        scan.binding.message_hash,
        changed_message.binding.message_hash
    );
    Ok(())
}

#[tokio::test]
async fn boolean_words_inside_credentials_do_not_suppress_secret_detection() -> Result<()> {
    let root = repository(&[])?;
    let repo = Repository::open(root.path())?;
    let base = git(root.path(), &["rev-parse", "HEAD"]).parse()?;
    let suffix = &TOKEN[4..];
    let values = [
        TOKEN.to_owned(),
        format!("ghp_{}false{}", &suffix[..10], &suffix[15..]),
        format!("ghp_{}FALSE{}", &suffix[..10], &suffix[15..]),
        format!("ghp_{}null", &suffix[..suffix.len() - 4]),
        format!("ghp_{}NULL", &suffix[..suffix.len() - 4]),
        format!("true{}", suffix),
    ];
    let texts = values
        .iter()
        .map(|v| format!("before\ntoken = '{v}'\nafter\n"))
        .collect::<Vec<_>>();
    for (i, text) in texts.iter().enumerate() {
        fs::write(root.path().join(format!("credential-{i}.txt")), text)?;
    }
    let head = commit(root.path(), "Add synthetic credential cases");
    let scanner = Engine::capture()?.start(&[])?;
    let scan = scanner
        .commit(
            &repo,
            base,
            head,
            &root.path().canonicalize()?.join(".git/lfs"),
            watch::channel(false).1,
        )
        .await?;
    for i in 0..values.len() {
        assert!(
            scan.findings
                .iter()
                .any(|f| f.path == format!("credential-{i}.txt") && f.line == 2),
            "missing fixture {i}"
        );
    }
    let redacted = scanner.redact(&texts, watch::channel(false).1).await?;
    assert!(
        redacted
            .iter()
            .all(|text| text == "before\n[redacted]\nafter\n")
    );
    let ordinary = vec![
        "token = false\n".into(),
        "token = true\n".into(),
        "token = null\n".into(),
    ];
    assert_eq!(
        scanner.redact(&ordinary, watch::channel(false).1).await?,
        ordinary
    );
    Ok(())
}

#[tokio::test]
async fn corrupt_detector_cancellation_and_excess_findings_fail_closed() -> Result<()> {
    let root = repository(&[])?;
    let bad = root.path().join("gitleaks");
    fs::write(&bad, "#!/bin/sh\nexit 0\n")?;
    assert!(Engine::open(bad.clone()).is_err());
    fs::remove_file(bad)?;
    let scanner = Engine::capture()?.start(&[])?;
    let cancelled = watch::channel(true).1;
    assert!(
        scanner
            .redact(&["anything".into()], cancelled)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    let many = Engine::capture()?.start(&["x".into()])?;
    assert!(
        many.redact(&["x".repeat(3000)], watch::channel(false).1)
            .await
            .unwrap_err()
            .to_string()
            .contains("Too many secret findings")
    );
    Ok(())
}

#[tokio::test]
async fn binary_coverage_and_known_values_require_separate_review() -> Result<()> {
    let root = repository(&[])?;
    let repo = Repository::open(root.path())?;
    let base = git(root.path(), &["rev-parse", "HEAD"]).parse()?;
    let value = "agent-created-binary-check-value-92";
    fs::write(root.path().join("image.bin"), format!("\0{value}\0"))?;
    let head = commit(root.path(), "binary addition");
    let scanner = Engine::capture()?.start(&[value.into()])?;
    let storage = root.path().canonicalize()?.join(".git/lfs");
    let scan = scanner
        .commit(&repo, base, head, &storage, watch::channel(false).1)
        .await?;
    let binary = scan
        .findings
        .iter()
        .find(|item| {
            item.path == "image.bin"
                && item.rule == intercat_daemon::secret_scan::BINARY_REVIEW_RULE
        })
        .expect("binary coverage finding");
    let secret = scan
        .findings
        .iter()
        .find(|item| item.path == "image.bin" && item.rule == "check-environment-value")
        .expect("known value in binary contents");
    assert!(!scan.accepted(&[binary.id.clone()].into()));
    assert!(!scan.accepted(&[secret.id.clone()].into()));
    assert!(scan.accepted(&[binary.id.clone(), secret.id.clone()].into()));
    assert!(scan.unscanned_lfs.is_empty());
    assert!(!serde_json::to_string(&scan)?.contains(value));
    let push = scanner
        .push_commit(&repo, head, &storage, watch::channel(false).1)
        .await?;
    assert_eq!(scan, push);
    Ok(())
}

#[tokio::test]
async fn symlink_blob_is_inspected_without_reading_its_target_and_index_is_unchanged() -> Result<()>
{
    let root = repository(&[])?;
    let outside = tempfile::NamedTempFile::new()?;
    fs::write(outside.path(), TOKEN)?;
    let base = git(root.path(), &["rev-parse", "HEAD"]).parse()?;
    std::os::unix::fs::symlink(outside.path(), root.path().join("link.txt"))?;
    let head = commit(root.path(), "symlink");
    let index = fs::read(root.path().join(".git/index"))?;
    let repo = Repository::open(root.path())?;
    let scanner = Engine::capture()?.start(&[])?;
    assert!(
        scanner
            .commit(
                &repo,
                base,
                head,
                &root.path().canonicalize()?.join(".git/lfs"),
                watch::channel(false).1
            )
            .await?
            .findings
            .is_empty()
    );
    assert_eq!(fs::read(root.path().join(".git/index"))?, index);
    assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), head.to_string());
    Ok(())
}

#[tokio::test]
async fn push_range_scans_roots_merge_resolutions_and_intermediate_removed_secrets() -> Result<()> {
    use std::sync::atomic::AtomicBool;
    let value = "dummy-agent-push-range-value-75";
    let root = repository(&[("base.txt", "dummy safe base\n")])?;
    let base = git(root.path(), &["rev-parse", "HEAD"]).parse()?;
    fs::write(
        root.path().join("temporary.txt"),
        format!("value={value}\n"),
    )?;
    let contains = commit(root.path(), "dummy intermediate secret");
    fs::remove_file(root.path().join("temporary.txt"))?;
    let removed = commit(root.path(), "dummy removed secret");
    let repo = Repository::open(root.path())?;
    let original = repo.commit_details(base)?;
    let blob = repo.write_blob(format!("value={value}\n").as_bytes())?;
    let merge_tree = repo.write_tree(&[("resolution.txt", blob)])?;
    let merge = repo.write_commit(
        merge_tree,
        &[removed, base],
        "dummy merge resolution",
        gix::actor::Signature {
            name: "Dummy".into(),
            email: "dummy@example.invalid".into(),
            time: gix::date::Time::now_local_or_utc(),
        },
    )?;
    let scanner = Engine::capture()?.start(&[value.into()])?;
    let lfs = root.path().canonicalize()?.join(".git/lfs");
    let root_scan = scanner
        .push_commit(&repo, base, &lfs, watch::channel(false).1)
        .await?;
    assert!(root_scan.findings.is_empty());
    assert_eq!(root_scan.binding.tree, original.tree.to_string());
    let commits = repo.push_commits(merge, &[base], &AtomicBool::new(false))?;
    assert_eq!(commits.len(), 3);
    let mut detected = Vec::new();
    for sha in commits {
        let scan = scanner
            .push_commit(&repo, sha, &lfs, watch::channel(false).1)
            .await?;
        if !scan.findings.is_empty() {
            assert_eq!(scan.findings.len(), 1);
            assert!(!serde_json::to_string(&scan)?.contains(value));
            detected.push(sha);
        }
    }
    assert!(detected.contains(&contains));
    assert!(detected.contains(&merge));
    assert!(!detected.contains(&removed));
    assert!(
        scanner
            .commit(&repo, removed, merge, &lfs, watch::channel(false).1)
            .await
            .is_err()
    );
    let gitlink_tree = gix::open(root.path())?
        .write_object(&gix::objs::Tree {
            entries: vec![gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Commit.into(),
                filename: "historical-gitlink".into(),
                oid: "ab".repeat(20).parse()?,
            }],
        })?
        .detach();
    let gitlink = repo.checkpoint_commit(gitlink_tree, base, "dummy historic gitlink")?;
    assert!(
        scanner
            .push_commit(&repo, gitlink, &lfs, watch::channel(false).1)
            .await?
            .findings
            .is_empty()
    );
    assert!(
        scanner
            .commit(&repo, base, gitlink, &lfs, watch::channel(false).1)
            .await
            .is_err()
    );
    Ok(())
}
