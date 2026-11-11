//! Offline pre-push execution. The transport's complete object graph is never
//! made readable to project code, even though a shallow checkout can run hooks.
use crate::{
    Result,
    checkout::Checkout,
    command::Output,
    hooks::{self, Purpose},
    push_target::Destination,
    push_transfer::Snapshot,
};
use intercat_git::{ObjectId, Repository};
use tokio::sync::watch;

pub async fn run(
    checkout: &Checkout,
    snapshot: &Snapshot,
    url: &str,
    remote_head: Option<ObjectId>,
    cancel: watch::Receiver<bool>,
) -> Result<Option<Output>> {
    let member = snapshot.member();
    let source = checkout.source();
    if checkout.root.starts_with(&snapshot.root)
        || snapshot.root.starts_with(&checkout.root)
        || !checkout.configuration.denied_roots.contains(&snapshot.root)
        || source.root != member.folder.root
        || source.common_dir != member.folder.common_dir
        || source.head != member.target.sha
    {
        return Err("The Push hook checkout must be separate from its transport data.".into());
    }
    let destination = Destination::capture(url)?;
    if !member.target.destinations.contains(&destination) {
        return Err("Destination changed since you reviewed · Review".into());
    }
    let repo = Repository::open(&checkout.workdir)?;
    if repo.head_commit()? != source.head || !checkout.workdir.join(".git/shallow").is_file() {
        return Err("Push hooks require the frozen SHA in a shallow checkout.".into());
    }
    let hooks = hooks::prepare(
        checkout,
        source,
        repo.tree_id(source.head)?,
        None,
        Purpose::Push,
    )?;
    let hook = hooks.directory.join("pre-push");
    if !hook.is_file() {
        return Ok(None);
    }
    let remote_head = remote_head.map_or_else(|| "0".repeat(40), |oid| oid.to_string());
    // A raw SHA refspec has the same SHA in Git's local-ref and local-oid
    // fields. Native Git invokes the hook separately for every push URL.
    let input = format!(
        "{sha} {sha} {target} {remote_head}\n",
        sha = member.target.sha,
        target = member.target.target_ref,
    );
    Ok(Some(
        checkout
            .run_pre_push(&hook, &member.target.remote, url, input.as_bytes(), cancel)
            .await?,
    ))
}
