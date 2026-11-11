use crate::{
    Result,
    journal::Journal,
    project::Project,
    push_job::{Action, Command},
    push_review::{Choice, Request},
    push_service,
};
use intercat_git::Repository;
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::watch;
use uuid::Uuid;
fn git(path: &Path, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("/usr/bin/git")
        .current_dir(path)
        .env_clear()
        .env("HOME", path)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err("Dummy multi-folder Git setup failed.".into());
    }
    Ok(())
}
async fn push(sender: &push_service::Service, review: Uuid) -> Result<crate::push_job::Job> {
    let job = sender
        .command(Command {
            op_id: Uuid::now_v7(),
            action: Action::Confirm { review },
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let job = sender.job(job.id).await?;
            if !job.active() {
                assert!(job.done(), "{job:?}");
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}
pub(crate) async fn verify(journal: &Journal, project: &Project, root: &Path) -> Result<()> {
    let mut remotes = Vec::new();
    for (index, folder) in project.folders.iter().take(2).enumerate() {
        let remote = root.join(format!("push-both-{index}.git"));
        git(root, &["init", "--bare", "-q", remote.to_str().unwrap()])?;
        let url = reqwest::Url::from_file_path(&remote).unwrap().to_string();
        git(&folder.root, &["remote", "add", "origin", &url])?;
        git(
            &folder.root,
            &[
                "push",
                "--no-verify",
                &url,
                &format!("{}:refs/heads/dummy-base", folder.base),
            ],
        )?;
        remotes.push(remote);
    }
    let home = root.join("push-home");
    std::fs::create_dir(&home)?;
    let git = crate::git_tool::GitTool::system().await?;
    let inspector = Arc::new(crate::push_inspect::Inspector::new(
        git.clone(),
        home.clone(),
        home.join(".config"),
        false,
    )?);
    let lfs = Arc::new(crate::push_lfs::Tool::open(
        git.clone(),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tools/git-lfs-3.8.1/git-lfs"),
    )?);
    let reviews =
        crate::push_review_service::Service::new(journal.clone(), inspector.clone(), lfs.clone());
    let sender = push_service::Service::new(
        journal.clone(),
        push_service::Configuration {
            git,
            inspector,
            lfs,
            root: root.join("push-both-transport"),
            path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
            agent: None,
        },
    )
    .await?;
    let choice = |index: usize| Choice {
        folder_id: project.folders[index].id.clone(),
        remote: "origin".into(),
        target_ref: None,
        through: None,
    };
    let first = reviews
        .open(
            Request {
                op_id: Uuid::now_v7(),
                choices: vec![choice(0)],
            },
            watch::channel(false).1,
        )
        .await?;
    let sent = push(&sender, first.request.op_id).await?;
    assert_eq!(sent.recipients.len(), 1);
    let both = reviews
        .open(
            Request {
                op_id: Uuid::now_v7(),
                choices: vec![choice(0), choice(1)],
            },
            watch::channel(false).1,
        )
        .await?;
    assert!(
        Repository::open(&remotes[1])?
            .reference_target(&both.members[1].target.target_ref)?
            .is_none()
    );
    let sent = push(&sender, both.request.op_id).await?;
    assert_eq!(sent.recipients.len(), 2);
    assert!(!sent.recipients[0].receipt.as_ref().unwrap().changed);
    assert!(sent.recipients[1].receipt.as_ref().unwrap().changed);
    for (member, remote) in both.members.iter().zip(remotes) {
        assert_eq!(
            Repository::open(remote)?.reference_target(&member.target.target_ref)?,
            Some(member.target.sha.parse()?)
        );
    }
    sender.shutdown().await;
    Ok(())
}
