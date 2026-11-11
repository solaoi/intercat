//! PR transitions separate human confirmation from asynchronous observations.
use crate::{
    Result,
    github_repository::Draft,
    pull_request::{Account, Action, Job, Phase, Pull},
};
use uuid::Uuid;
pub(crate) enum Change {
    Drafted {
        folder: String,
        draft: Draft,
    },
    LookedUp {
        account: Account,
        pulls: Vec<Option<Pull>>,
        drafts: Vec<Draft>,
    },
    BeginCreate {
        folder: String,
    },
    Created {
        folder: String,
        pull: Pull,
    },
    BeginLink {
        folder: String,
    },
    Linked {
        folder: String,
        pull: Pull,
    },
    Failed {
        folder: String,
        uncertain: bool,
        reason: &'static str,
    },
    Hold,
}
fn member<'a>(job: &'a mut Job, folder: &str) -> Result<&'a mut crate::pull_request::Member> {
    job.members
        .iter_mut()
        .find(|member| member.folder_id == folder)
        .ok_or_else(|| "Missing PR folder.".into())
}
pub(crate) fn command(job: &mut Job, op: Uuid, action: &Action) -> Result<()> {
    let (id, revision) = match action {
        Action::Edit { id, revision, .. }
        | Action::Confirm { id, revision }
        | Action::Retry { id, revision }
        | Action::ReviewAgain { id, revision }
        | Action::Discard { id, revision }
        | Action::Stop { id, revision } => (*id, *revision),
    };
    if job.id != id || job.revision != revision {
        return Err("The PR review changed. Read its current state.".into());
    }
    match action {
        Action::Edit { drafts, .. } => {
            if job.confirmed.is_some()
                || job.held
                || !job
                    .members
                    .iter()
                    .all(|member| member.phase == Phase::Review)
                || drafts.len() != job.members.len()
            {
                return Err("Wait for an editable PR review.".into());
            }
            let mut edited = std::collections::BTreeSet::new();
            for edit in drafts {
                edit.draft.validate()?;
                if !edited.insert(&edit.folder_id) {
                    return Err("A PR folder was edited twice.".into());
                }
                member(job, &edit.folder_id)?.draft = Some(edit.draft.clone());
            }
        }
        Action::Confirm { .. } => {
            if job.confirmed.is_some()
                || job.held
                || job.account.is_none()
                || !job
                    .members
                    .iter()
                    .all(|member| member.phase == Phase::Review && member.draft.is_some())
            {
                return Err(
                    "Review the PR text, account and every folder before confirming.".into(),
                );
            }
            job.confirmed = Some(op);
            for member in &mut job.members {
                member.phase = Phase::Ready;
                member.error = None;
            }
        }
        Action::Retry { .. } => {
            if job.running() {
                return Err("This PR request is still running.".into());
            }
            job.held = false;
            for member in &mut job.members {
                if member.phase == Phase::Uncertain {
                    member.phase = Phase::Checking;
                    member.error = None;
                } else if member.phase == Phase::Failed {
                    member.phase = if job.confirmed.is_some() {
                        if member.result.is_some() {
                            Phase::Created
                        } else {
                            Phase::Ready
                        }
                    } else if member.draft.is_some() {
                        Phase::LookingUp
                    } else {
                        Phase::Drafting
                    };
                    member.error = None;
                }
            }
        }
        Action::ReviewAgain { .. } => {
            if job.running()
                || job.members.iter().any(|member| {
                    member.result.is_some()
                        || matches!(
                            member.phase,
                            Phase::Creating | Phase::Uncertain | Phase::Checking | Phase::Discarded
                        )
                })
            {
                return Err("Known or uncertain PR results must be checked instead of replaced by a new review.".into());
            }
            job.confirmed = None;
            job.account = None;
            job.held = false;
            for member in &mut job.members {
                member.phase = if member.draft.is_some() {
                    Phase::LookingUp
                } else {
                    Phase::Drafting
                };
                member.existing = None;
                member.error = None;
            }
        }
        Action::Discard { .. } => {
            if job.confirmed.is_some() {
                return Err("Confirmed PR requests retain their external results. Stop instead of discarding.".into());
            }
            job.held = true;
            for member in &mut job.members {
                member.phase = Phase::Discarded;
            }
        }
        Action::Stop { .. } => {
            job.held = true;
        }
    }
    job.validate()
}
pub(crate) fn apply(job: &mut Job, change: Change) -> Result<()> {
    match change {
        Change::Drafted { folder, draft } => {
            draft.validate()?;
            if job.confirmed.is_some() {
                return Err("PR text is already confirmed.".into());
            }
            let member = member(job, &folder)?;
            if member.phase != Phase::Drafting {
                return Err("The PR draft has already changed.".into());
            }
            member.draft = Some(draft);
            member.phase = Phase::LookingUp;
            member.error = None;
        }
        Change::LookedUp {
            account,
            pulls,
            drafts,
        } => {
            if job.confirmed.is_some()
                || pulls.len() != job.members.len()
                || drafts.len() != job.members.len()
                || job
                    .members
                    .iter()
                    .any(|member| !matches!(member.phase, Phase::LookingUp | Phase::Review))
            {
                return Err("PR lookup no longer belongs to this review.".into());
            }
            job.account = Some(account);
            for ((member, pull), draft) in job.members.iter_mut().zip(pulls).zip(drafts) {
                if member.phase == Phase::Review {
                    continue;
                }
                if let Some(pull) = &pull
                    && pull.head_sha != member.target.sha
                {
                    return Err(
                        "The existing PR's head changed. Review the current pushed commit.".into(),
                    );
                }
                draft.validate()?;
                member.draft = Some(draft);
                member.existing = pull;
                member.phase = Phase::Review;
                member.error = None;
            }
        }
        Change::BeginCreate { folder } => {
            if job.confirmed.is_none() || job.held {
                return Err("PR creation is not confirmed or is stopped.".into());
            }
            let member = member(job, &folder)?;
            if member.phase != Phase::Ready || member.existing.is_some() {
                return Err("PR creation has already started or this PR already exists.".into());
            }
            member.phase = Phase::Creating;
        }
        Change::Created { folder, pull } => {
            if job.confirmed.is_none() {
                return Err("PR creation was not confirmed.".into());
            }
            let member = member(job, &folder)?;
            if !matches!(
                member.phase,
                Phase::Ready | Phase::Creating | Phase::Uncertain | Phase::Checking
            ) {
                return Err("The PR result has already changed.".into());
            }
            if member.phase == Phase::Ready
                && member
                    .existing
                    .as_ref()
                    .is_none_or(|existing| existing.number != pull.number)
            {
                return Err("The existing PR result changed.".into());
            }
            member.phase = Phase::Created;
            member.error = if pull.head_sha != member.target.sha {
                Some("The branch advanced after confirmation. The PR is retained; review it before continuing.".into())
            } else if member.existing.is_none()
                && member
                    .draft
                    .as_ref()
                    .is_none_or(|draft| draft.title != pull.title || draft.body != pull.body)
            {
                Some("GitHub returned different PR text. The saved URL is retained; review it before continuing.".into())
            } else {
                None
            };
            let changed = member.error.is_some();
            member.result = Some(pull);
            if changed {
                job.held = true;
            }
        }
        Change::BeginLink { folder } => {
            if job.confirmed.is_none()
                || job.held
                || job.members.iter().any(|member| member.result.is_none())
            {
                return Err("Wait for every PR result before adding related links.".into());
            }
            let member = member(job, &folder)?;
            if !matches!(member.phase, Phase::Created | Phase::Linking) {
                return Err("PR linking no longer belongs to this result.".into());
            }
            member.phase = Phase::Linking;
        }
        Change::Linked { folder, pull } => {
            let member = member(job, &folder)?;
            if member.phase != Phase::Linking
                || member
                    .result
                    .as_ref()
                    .is_none_or(|result| result.number != pull.number)
            {
                return Err("The linked PR result changed.".into());
            }
            member.result = Some(pull);
            member.phase = Phase::Done;
            member.error = None;
        }
        Change::Failed {
            folder,
            uncertain,
            reason,
        } => {
            let member = member(job, &folder)?;
            if matches!(member.phase, Phase::Done | Phase::Discarded) {
                return Err("The PR operation already ended.".into());
            }
            member.phase = if uncertain || member.phase == Phase::Creating {
                Phase::Uncertain
            } else if member.phase == Phase::Linking {
                Phase::Linking
            } else {
                Phase::Failed
            };
            member.error = Some(reason.into());
            job.held = true;
        }
        Change::Hold => {
            recover(job)?;
            job.held = true;
        }
    }
    job.validate()
}
pub(crate) fn recover(job: &mut Job) -> Result<bool> {
    if !job.members.iter().any(|member| member.phase.running()) {
        return Ok(false);
    }
    let mut changed = !job.held;
    job.held = true;
    for member in &mut job.members {
        if matches!(member.phase, Phase::Creating | Phase::Checking) {
            changed = true;
            member.phase = Phase::Uncertain;
            member.error = Some(
                "PR creation was interrupted. Check the existing result before creating again."
                    .into(),
            );
        } else if matches!(
            member.phase,
            Phase::Drafting | Phase::LookingUp | Phase::Ready
        ) {
            changed = true;
            member.phase = Phase::Failed;
            member.error =
                Some("PR preparation was interrupted. Retry keeps its reviewed inputs.".into());
        }
    }
    job.validate()?;
    Ok(changed)
}
