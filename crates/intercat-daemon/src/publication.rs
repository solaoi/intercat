//! Publish a reviewed candidate. The journal owns the immutable intent; Git
//! writes one ref at a time, branch first, and safely replays a durable prefix.
use crate::{
    Result, candidate,
    integration::{Job, RowState},
    journal::Journal,
};
use intercat_git::{ObjectId, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefChange {
    pub name: String,
    pub old: Option<String>,
    pub new: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Publication {
    pub operation: Uuid,
    pub refs: Vec<RefChange>,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Complete,
    NotPublished(String),
    Blocked(String),
}

pub fn intent(job: &Job, operation: Uuid) -> Result<Publication> {
    if job.path_state.as_ref().is_some_and(|state| !state.ready()) {
        return Err(
            "Review the saved path choices and affected dependencies before committing.".into(),
        );
    }
    let candidate = job.candidate.as_ref().ok_or("No checked candidate.")?;
    let plan = job.plan.as_ref().ok_or("No frozen integration plan.")?;
    let check = candidate
        .check
        .as_ref()
        .ok_or("The candidate has not been checked.")?;
    if candidate.state != candidate::State::Checked
        || candidate.job_id != job.id
        || candidate.revision != job.revision
        || check.tree != candidate.tree
        || check.settings_hash != candidate.settings_hash
        || (check.command.is_some() && check.code != Some(0))
        || candidate.secret_scans.len() != candidate.commits.len()
        || plan.coverage.is_none()
        || candidate.pending_undo()
        || job.undo.is_some() != candidate.undo.is_some()
        || candidate.scans().any(|scan| {
            scan.binding.ruleset != crate::secret_scan::ruleset()
                || !scan.accepted(&job.kept_secrets)
        })
    {
        return Err("The candidate does not have complete checks and secret scans.".into());
    }
    if !job
        .tracks
        .iter()
        .any(|track| track.included && track.checkpoint.is_some())
    {
        return Err("No saved tracks are selected for Commit.".into());
    }
    if let Some(undo) = &job.undo {
        undo.validate(job)?;
    }
    let mut refs = vec![RefChange {
        name: plan.target_ref.clone(),
        old: plan.expected_target.clone(),
        new: candidate.head.clone(),
    }];
    for track in job
        .tracks
        .iter()
        .filter(|track| track.included && track.checkpoint.is_some())
    {
        let row = plan
            .rows
            .iter()
            .find(|row| row.track == track.id)
            .ok_or("Missing track base.")?;
        if !matches!(row.state, RowState::Included | RowState::NoChanges) {
            return Err("Unresolved integration row.".into());
        }
        let sha = track.checkpoint.as_ref().unwrap().sha.clone();
        refs.push(RefChange {
            name: row.integrated_ref.clone(),
            old: row.expected_integrated.clone(),
            new: sha.clone(),
        });
        refs.push(RefChange {
            name: row.base_ref.clone(),
            old: row.expected_base.clone(),
            new: sha,
        });
    }
    if let Some(undo) = &candidate.undo {
        refs.push(RefChange {
            name: format!("refs/intercat/{}/undo/{}", job.slug, job.id),
            old: None,
            new: undo.sha.clone(),
        });
    }
    for commit in &candidate.commits {
        let track = job
            .tracks
            .iter()
            .find(|track| track.id == commit.track && track.included)
            .ok_or("Unknown candidate track.")?;
        refs.push(RefChange {
            name: format!(
                "refs/intercat/{}/integration/{}/{}",
                job.slug,
                track.label,
                track
                    .integration_count
                    .checked_add(1)
                    .ok_or("Integration count overflow.")?
            ),
            old: None,
            new: commit.sha.clone(),
        });
    }
    refs.push(RefChange {
        name: format!("refs/intercat/{}/operations/{operation}", job.slug),
        old: None,
        new: candidate.head.clone(),
    });
    Ok(Publication { operation, refs })
}

fn verify(
    job: &Job,
    repository: &Repository,
    settings: &candidate::Settings,
    published: bool,
) -> Result<()> {
    let candidate = job.candidate.as_ref().ok_or("Missing candidate.")?;
    let plan = job.plan.as_ref().ok_or("Missing plan.")?;
    if !published && repository.branch_is_checked_out(&plan.target_ref)? {
        return Err("The session branch is checked out outside the integration job.".into());
    }
    if candidate.settings_hash != settings.hash()? {
        return Err("The Check settings changed.".into());
    }
    let mut parent: ObjectId = plan.target_head.parse()?;
    let expected_values = crate::secret_scan::value_fingerprints(
        &settings
            .environment_values()?
            .into_values()
            .collect::<Vec<_>>(),
    );
    if let Some(undo) = &candidate.undo {
        job.undo
            .as_ref()
            .ok_or("Missing undo plan.")?
            .verify_records(repository)?;
        undo.verify(
            repository,
            job.undo.as_ref().ok_or("Missing undo plan.")?,
            settings,
        )?;
        crate::lfs_store::sync(
            &settings.checkout.lfs_directory,
            &repository.lfs_entries(undo.tree.parse()?)?,
        )?;
        parent = undo.sha.parse()?;
    }
    let mut tracks = std::collections::HashSet::new();
    for (commit, scan) in candidate.commits.iter().zip(&candidate.secret_scans) {
        let sha = commit.sha.parse()?;
        let actual = repository.commit_details(sha)?;
        let message_hash: String = Sha256::digest(actual.message.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if !tracks.insert(&commit.track)
            || actual.parents != vec![parent]
            || commit.parent != parent
            || actual.tree != commit.tree
            || actual.author_name != commit.author
            || scan.binding.commit != commit.sha
            || scan.binding.tree != commit.tree
            || scan.binding.parent_tree != repository.tree_id(parent)?
            || scan.binding.message_hash != message_hash
            || scan.binding.ruleset != crate::secret_scan::ruleset()
            || scan.binding.value_fingerprints != expected_values
        {
            return Err("The candidate no longer matches its reviewed secret scan.".into());
        }
        crate::lfs_store::sync(
            &settings.checkout.lfs_directory,
            &repository.lfs_entries(actual.tree)?,
        )?;
        parent = sha;
    }
    if parent != candidate.head || repository.tree_id(parent)? != candidate.tree {
        return Err("The candidate chain or tree changed.".into());
    }
    crate::lfs_store::sync(
        &settings.checkout.lfs_directory,
        &repository.lfs_entries(repository.tree_id(parent)?)?,
    )?;
    if !published
        && (!candidate.commits.is_empty() || candidate.undo.is_some())
        && repository.reference_target(&format!("refs/intercat/{}/staging", job.slug))?
            != Some(parent)
    {
        return Err("The staged candidate changed. Refresh and review again.".into());
    }
    for track in job.tracks.iter().filter(|track| track.included) {
        if let Some(checkpoint) = &track.checkpoint
            && repository.reference_target(&checkpoint.reference)? != Some(checkpoint.sha.parse()?)
        {
            return Err("A frozen checkpoint reference changed.".into());
        }
    }
    Ok(())
}
fn branch_contains(
    repository: &Repository,
    current: Option<ObjectId>,
    next: ObjectId,
) -> Result<bool> {
    Ok(match current {
        Some(current) => current == next || repository.merge_base(next, current)? == Some(next),
        None => false,
    })
}
fn apply(job: &Job, settings: &candidate::Settings) -> Result<()> {
    let publication = job
        .publication
        .as_ref()
        .ok_or("Missing publication intent.")?;
    if *publication != intent(job, publication.operation)? {
        return Err("The publication intent changed.".into());
    }
    let repository = crate::integration::repository(job)?;
    let branch = &publication.refs[0];
    let published = branch_contains(
        &repository,
        repository.reference_target(&branch.name)?,
        branch.new.parse()?,
    )?;
    verify(job, &repository, settings, published)?;
    // Validate every ref before moving the branch. A later race is still
    // guarded by each locked CAS and remains a durable recovery operation.
    for (index, change) in publication.refs.iter().enumerate() {
        if index == 0 && published {
            continue;
        }
        let current = repository.reference_target(&change.name)?;
        let old = change.old.as_ref().map(|id| id.parse()).transpose()?;
        let new = change.new.parse()?;
        if current != old && current != Some(new) {
            return Err(format!("A ref changed outside this Commit: {}", change.name).into());
        }
    }
    let roots = publication
        .refs
        .iter()
        .map(|change| change.new.parse())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    repository.sync_objects(&roots)?;
    for (index, change) in publication.refs.iter().enumerate() {
        if index == 0 && published {
            repository.sync_ref(&change.name)?;
            continue;
        }
        repository.compare_and_swap(
            &change.name,
            change.old.as_ref().map(|id| id.parse()).transpose()?,
            change.new.parse()?,
        )?;
        repository.sync_ref(&change.name)?;
    }
    Ok(())
}

pub async fn publish(
    job: Job,
    journal: Journal,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Job> {
    let mut changes = journal.subscribe();
    while journal.pending_path_retirement(job.id).await? {
        if *cancel.borrow() {
            return journal.integration_job(job.id).await;
        }
        tokio::select! {
            _=changes.changed()=>{},
            changed=cancel.changed()=>{if changed.is_err()||*cancel.borrow(){return journal.integration_job(job.id).await;}},
        }
    }
    let settings = journal
        .integration_settings(job.id, job.revision)
        .await?
        .ok_or("Missing frozen Check settings.")?;
    let input = job.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        match apply(&input, &settings) {
            Ok(()) => Outcome::Complete,
            Err(error) => {
                // After the branch is published, never reopen or discard this
                // operation. Finish the exact remaining refs after recovery.
                let published = (|| -> Result<bool> {
                    let repo = crate::integration::repository(&input)?;
                    let branch = &input.publication.as_ref().ok_or("Missing intent.")?.refs[0];
                    branch_contains(
                        &repo,
                        repo.reference_target(&branch.name)?,
                        branch.new.parse()?,
                    )
                })();
                if matches!(published, Ok(false)) {
                    Outcome::NotPublished(error.to_string())
                } else {
                    Outcome::Blocked(error.to_string())
                }
            }
        }
    })
    .await?;
    let refs_applied = matches!(outcome, Outcome::Complete);
    match journal
        .integration_published(job.id, job.revision, outcome)
        .await
    {
        Err(error) if refs_applied => {
            // A failed journal transaction did not undo the Git writes. Keep
            // the same intent available for recovery and expose the reason.
            journal
                .integration_published(job.id, job.revision, Outcome::Blocked(error.to_string()))
                .await
        }
        result => result,
    }
}
