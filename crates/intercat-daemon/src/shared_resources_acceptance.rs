//! A shared merge uses private LFS bodies and scans the resulting body for
//! a generated dummy Check secret before allowing publication.
use super::*;

#[tokio::test]
async fn shared_lfs_resolution_is_private_and_a_dummy_secret_requires_explicit_keep() -> Result<()>
{
    let mut f = Fixture::new(4).await?;
    let tool = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.tools/git-lfs-3.8.1/git-lfs")
        .canonicalize()?;
    for (field, command) in [
        ("clean", "clean -- %f"),
        ("smudge", "smudge -- %f"),
        ("process", "filter-process"),
    ] {
        git(
            &f.source,
            &[
                "config",
                &format!("filter.lfs.{field}"),
                &format!("'{}' {command}", tool.display()),
            ],
        )?;
    }
    git(&f.source, &["config", "filter.lfs.required", "true"])?;
    let store = f.root.path().canonicalize()?.join("own-shared-lfs");
    git(
        &f.source,
        &["config", "lfs.storage", store.to_str().unwrap()],
    )?;
    let secret = format!("own-dummy-shared-{}", Uuid::now_v7());
    let environment = f.source.join(".env.test");
    fs::write(&environment, format!("DUMMY_TOKEN={secret}\n"))?;
    f.journal
        .change_check_environment(crate::check_environment::Change {
            op_id: Uuid::now_v7(),
            revision: 0,
            file: Some(environment.clone()),
        })
        .await?;
    let folder = f.journal.snapshot().await?["project"]["folders"][0]["id"]
        .as_str()
        .ok_or("Missing dummy folder")?
        .to_owned();
    f.journal
        .change_check_settings(crate::check_settings::Change {
            op_id: Uuid::now_v7(),
            revision: 0,
            folder,
            command: crate::check_run::CheckCommand::Run(
                "printf '%s\\n' \"$DUMMY_TOKEN\"; printf '%s\\n' \"$DUMMY_TOKEN\" >&2".into(),
            ),
            history: false,
        })
        .await?;
    fs::write(
        f.workspaces[0].path.join(".gitattributes"),
        "asset.lfs filter=lfs -text\n",
    )?;
    fs::write(f.workspaces[0].path.join("asset.lfs"), "Own A fork\n")?;
    let fork = f.save(0, None).await?;
    f.branch(&fork)?;
    fs::write(f.workspaces[0].path.join("asset.lfs"), "Own A later\n")?;
    fs::write(
        f.workspaces[0].path.join("retired-a.txt"),
        "Own retired A\n",
    )?;
    let old = f.save(0, Some(fork)).await?;
    fs::write(f.workspaces[1].path.join("asset.lfs"), "Own chosen A\n")?;
    f.save(1, None).await?;
    f.inherit(2, 0, &old)?;
    fs::write(f.workspaces[2].path.join("asset.lfs"), "Own shared B\n")?;
    let shared = f.save(2, None).await?;
    branch_shared(&mut f, &shared)?;
    fs::write(f.workspaces[3].path.join("chosen-b.txt"), "Own chosen B\n")?;
    let child = f.save(3, None).await?;
    f.worker(3, child).await?;
    fs::write(
        f.workspaces[2].path.join("later-b.txt"),
        "Own B after fork\n",
    )?;
    f.save(2, Some(shared.clone())).await?;
    let job = f.action(Action::Open { tracks: None }).await?;
    let job = f
        .action(Action::ChoosePath {
            id: job.id,
            revision: job.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    let job = f
        .action(Action::ChoosePath {
            id: job.id,
            revision: job.revision,
            track: f.workspaces[3].track_id.clone(),
        })
        .await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        track_id: f.workspaces[3].track_id.clone(),
        source: Source::Reparent {
            job_id: job.id,
            revision: job.revision,
            parent: f.workspaces[2].track_id.clone(),
        },
    };
    let pending = queue_shared(&f, &request).await?;
    let summary = pending
        .shared_input
        .as_ref()
        .ok_or("Missing LFS shared input")?;
    let runtime = start_runtime(&f, true);
    let run = runtime
        .service
        .start_manual(crate::repair_run::Request {
            op_id: Uuid::now_v7(),
            owner: Owner::Shared {
                id: summary.id,
                revision: summary.revision,
            },
            track: summary.track.clone(),
            purpose: Purpose::Merge,
        })
        .await?;
    let view = runtime
        .service
        .read_manual(crate::merge_manual::Read {
            id: run.id,
            revision: run.revision,
            path: Some("asset.lfs".into()),
        })
        .await?;
    assert_eq!(
        view.file.sides.target.as_ref().unwrap().text.as_deref(),
        Some("Own chosen A\n")
    );
    assert_eq!(
        view.file.sides.incoming.as_ref().unwrap().text.as_deref(),
        Some("Own shared B\n")
    );
    let (_, source) = f.journal.repair_source(run.id).await?;
    let root = f
        .root
        .path()
        .join("integration")
        .join(summary.id.to_string())
        .join(format!("repair-{}", run.id));
    let workspace = crate::run_workspace::Workspace::open_source(
        source,
        &root,
        &run,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert!(!workspace.checkout().workdir.join(".env.test").exists());
    let repo = intercat_git::Repository::open(&f.source)?;
    let entry = repo
        .lfs_entries(repo.tree_id(shared.sha.parse()?)?)?
        .remove(0);
    let oid = entry.pointer.oid();
    let object = store
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid);
    for forbidden in [&environment, &object] {
        let script = format!("/bin/cat '{}' >/dev/null", forbidden.display());
        let denied = workspace
            .checkout()
            .run_check(
                &script,
                &Default::default(),
                tokio::sync::watch::channel(false).1,
                Duration::from_secs(10),
            )
            .await?;
        assert!(
            !denied.success(),
            "Code must not read the original LFS store or Check file"
        );
        assert!(!String::from_utf8_lossy(&denied.stdout).contains(&secret));
    }
    drop(workspace);
    let body = format!("Own shared resolution\n{secret}\n");
    let captured = runtime
        .service
        .apply_manual(crate::merge_manual::Apply {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            files: vec![crate::merge_manual::Edit::Text {
                path: "asset.lfs".into(),
                version: view.file.version,
                text: body.clone(),
            }],
        })
        .await?;
    assert_eq!(captured.phase, RunPhase::Captured);
    let (_, source) = f.journal.repair_source(run.id).await?;
    let workspace = Arc::new(
        crate::run_workspace::Workspace::open_source(
            source,
            &root,
            &captured,
            tokio::sync::watch::channel(false).1,
        )
        .await?,
    );
    let done = crate::repair_application::apply_source(
        &f.journal,
        &f.review,
        &captured,
        workspace,
        f.root.path().join("integration"),
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert_eq!(done.phase, RunPhase::Done);
    resume_shared(&f, &request).await?;
    assert_eq!(
        fs::read_to_string(f.workspaces[3].path.join("asset.lfs"))?,
        body
    );
    let opened = f.action(Action::Open { tracks: None }).await?;
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let mut blocked = f.settled(opened.id).await?;
    let candidate = blocked.candidate.as_ref().ok_or("Missing LFS candidate")?;
    assert_eq!(candidate.state, crate::candidate::State::SecretFound);
    assert!(f.action(commit(&blocked)).await.is_err());
    let owner = candidate.blocked_commit.as_ref().unwrap().track.clone();
    let diff = f
        .review
        .diff(crate::integration::DiffRequest {
            id: blocked.id,
            revision: blocked.revision,
            owner: crate::integration::DiffOwner::Worker { track: owner },
            path: "asset.lfs".into(),
        })
        .await?;
    assert!(intercat_git::lfs::Pointer::parse(diff.after.as_bytes()).is_some());
    assert!(!diff.after.contains(&secret));
    let findings = candidate.secret_scans.last().unwrap().findings.clone();
    assert!(!findings.is_empty());
    assert!(findings.iter().any(|finding| finding.path == "asset.lfs"));
    assert!(!serde_json::to_string(&findings)?.contains(&secret));
    for finding in findings {
        blocked = f
            .action(Action::KeepSecret {
                id: blocked.id,
                revision: blocked.revision,
                finding: finding.id,
            })
            .await?;
    }
    let checked = f.settled(blocked.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let output = checked.candidate.as_ref().unwrap().check.as_ref().unwrap();
    for text in [&output.stdout, &output.stderr] {
        assert!(text.contains("[redacted]"));
        assert!(!text.contains(&secret));
    }
    let done = publish_unhosted_replacement(&f, &checked).await?;
    let target = repo
        .reference_target(&done.plan.as_ref().unwrap().target_ref)?
        .unwrap();
    let entry = repo.lfs_entries(repo.tree_id(target)?)?.remove(0);
    let mut file = crate::lfs_store::read(&store, &entry)?.ok_or("Missing resolved LFS body")?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    assert_eq!(bytes, body.as_bytes());
    assert!(
        !repo
            .tree_files(repo.tree_id(target)?)?
            .contains_key("retired-a.txt")
    );
    assert!(
        !repo
            .tree_files(repo.tree_id(target)?)?
            .contains_key("later-b.txt")
    );
    assert_eq!(
        fs::read_to_string(f.workspaces[2].path.join("asset.lfs"))?,
        "Own shared B\n"
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    runtime.shutdown().await;
    f.review.shutdown().await;
    Ok(())
}
