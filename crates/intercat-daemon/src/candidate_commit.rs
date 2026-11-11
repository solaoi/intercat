//! A commit is made and screened inside its private checkout before import.
//! Worker contributions and replacement undo commits use the same hooks.
use crate::{
    Result,
    candidate::{Settings, State},
    checkout::Checkout,
    hooks,
};
use intercat_git::{CommitDetails, ObjectId, Repository};
use tokio::sync::watch;

pub(crate) struct Request<'a> {
    pub tree: ObjectId,
    pub parent: ObjectId,
    pub attachment_base: ObjectId,
    pub author: &'a str,
    pub message: &'a str,
    pub allow_empty: bool,
}
pub(crate) struct Made {
    pub sha: ObjectId,
    pub details: CommitDetails,
    pub scan: crate::secret_scan::Scan,
    pub left_out: Vec<String>,
}
pub(crate) struct Rejected {
    pub state: State,
    pub reason: String,
    pub left_out: Vec<String>,
}
pub(crate) enum Outcome {
    Made(Box<Made>),
    Rejected(Rejected),
}
pub(crate) async fn create(
    checkout: &Checkout,
    settings: &Settings,
    request: Request<'_>,
    scanner: &crate::secret_scan::Scanner,
    cancel: watch::Receiver<bool>,
) -> Result<Outcome> {
    let mut left_out = checkout
        .stage(request.attachment_base, cancel.clone())
        .await?;
    let hooks = if settings.skip_hooks {
        checkout.root.join("control/empty-hooks")
    } else {
        match hooks::prepare(
            checkout,
            checkout.source(),
            request.tree,
            settings
                .environment
                .as_ref()
                .and_then(|value| value.selection.file.as_deref()),
            hooks::Purpose::Commit,
        ) {
            Ok(hooks) => hooks.directory,
            Err(error) => {
                return Ok(Outcome::Rejected(Rejected {
                    state: State::HooksOutside,
                    reason: error.to_string(),
                    left_out,
                }));
            }
        }
    };
    let message_path = checkout.root.join("control/commit-message");
    std::fs::write(&message_path, request.message)?;
    let mut arguments = vec![
        "-c".into(),
        format!("core.hooksPath={}", hooks.display()).into(),
        "-c".into(),
        "user.name=Intercat".into(),
        "-c".into(),
        "user.email=intercat@localhost".into(),
        "-c".into(),
        "committer.name=Intercat".into(),
        "-c".into(),
        "committer.email=intercat@localhost".into(),
        "commit".into(),
        "--quiet".into(),
        "--cleanup=verbatim".into(),
        format!("--author={}", request.author).into(),
        "--file".into(),
        message_path.into_os_string(),
    ];
    if request.allow_empty {
        arguments.push("--allow-empty".into());
    }
    let output = checkout.git(&arguments, true, cancel.clone()).await?;
    if !output.success() {
        return Ok(Outcome::Rejected(Rejected {
            state: if output.cancelled {
                State::Cancelled
            } else {
                State::HookRejected
            },
            reason: format!(
                "Commit failed (exit {:?}): {}",
                output.code,
                output.stderr_text()
            ),
            left_out,
        }));
    }
    let output = checkout
        .git(&["rev-parse".into(), "HEAD".into()], false, cancel.clone())
        .await?
        .require_success("Read candidate")?;
    let raw_sha: ObjectId = String::from_utf8(output.stdout)?.trim().parse()?;
    checkout.verify_storage()?;
    let repository = Repository::open(&checkout.workdir)?;
    let (sha, removed) = repository.commit_without_attachments(
        raw_sha,
        request.attachment_base,
        &checkout.attachment_roots,
    )?;
    left_out.extend(removed);
    if sha != raw_sha {
        repository.sync_objects(&[sha])?;
        checkout
            .git(
                &[
                    "update-ref".into(),
                    "HEAD".into(),
                    sha.to_string().into(),
                    raw_sha.to_string().into(),
                ],
                true,
                cancel.clone(),
            )
            .await?
            .require_success("Exclude attachment copies from candidate")?;
        checkout
            .git(
                &["read-tree".into(), sha.to_string().into()],
                true,
                cancel.clone(),
            )
            .await?
            .require_success("Update candidate index after excluding attachments")?;
    }
    let details = repository.commit_details(sha)?;
    if details.parents != vec![request.parent] {
        return Ok(Outcome::Rejected(Rejected {
            state: State::EnvFailed,
            reason: "A hook changed the candidate's parent.".into(),
            left_out,
        }));
    }
    let scan = scanner
        .commit(
            &repository,
            request.parent,
            sha,
            &checkout.workdir.join(".git/lfs"),
            cancel,
        )
        .await?;
    Ok(Outcome::Made(Box::new(Made {
        sha,
        details,
        scan,
        left_out,
    })))
}
