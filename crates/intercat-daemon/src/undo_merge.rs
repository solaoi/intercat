//! The app's undo prefix owns its conflict independently of worker commits.
use crate::{
    Result,
    candidate::{Settings, State},
    integration::{Job, Status},
    replacement_undo::{Conflict, Plan},
};
use intercat_git::{Merge, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

fn inputs(job: &Job) -> Result<(&Plan, &Conflict, &str)> {
    let candidate = job
        .candidate
        .as_ref()
        .ok_or("Build the conflicted undo first.")?;
    let plan = job.undo.as_ref().ok_or("Missing frozen undo plan.")?;
    plan.validate(job)?;
    let conflict = candidate
        .undo_conflict
        .as_ref()
        .ok_or("Missing undo conflict.")?;
    let ids = plan.ids();
    let count = conflict.applied.len();
    if job.status != Status::MergeConflict
        || candidate.state != State::Conflict
        || candidate.job_id != job.id
        || candidate.revision != job.revision
        || candidate.head != plan.parent
        || !candidate.commits.is_empty()
        || candidate.undo.is_some()
        || candidate.blocked_commit.is_some()
        || candidate.failed_track.is_some()
        || count >= ids.len()
        || conflict.track != plan.records[count].track_id
        || conflict.applied != ids[..count]
        || conflict.remaining != ids[count..]
        || job
            .plan
            .as_ref()
            .and_then(|plan| plan.undo_conflict.as_ref())
            != Some(conflict)
    {
        return Err("The frozen undo conflict changed. Review it again.".into());
    }
    Ok((plan, conflict, &candidate.settings_hash))
}
fn identity(job: &Job, plan: &Plan, conflict: &Conflict, settings: &str) -> Result<Uuid> {
    Ok(Uuid::new_v5(
        &job.id,
        &Sha256::digest(serde_json::to_vec(&(
            job.revision,
            &job.project.id,
            &plan.parent,
            plan.ids(),
            &plan.resolutions,
            conflict,
            settings,
        ))?),
    ))
}
pub(crate) fn automatic_request(job: &Job) -> Result<Option<crate::repair_run::Request>> {
    if job.status != Status::MergeConflict
        || job
            .candidate
            .as_ref()
            .is_none_or(|c| c.undo_conflict.is_none())
    {
        return Ok(None);
    }
    let (plan, conflict, settings) = inputs(job)?;
    let id = identity(job, plan, conflict, settings)?;
    Ok(Some(crate::repair_run::Request {
        op_id: Uuid::new_v5(&id, b"automatic-undo-merge"),
        owner: crate::repair_run::Owner::Undo {
            id: job.id,
            revision: job.revision,
        },
        track: plan.records[conflict.applied.len()].track_id.clone(),
        purpose: crate::repair_run::Purpose::Merge,
    }))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Frozen {
    pub id: Uuid,
    pub job: Uuid,
    pub revision: u64,
    pub track: String,
    pub label: String,
    pub session_id: String,
    pub session_name: String,
    pub slug: String,
    pub project: crate::project::Folder,
    pub plan: Plan,
    pub conflict: Conflict,
    pub settings: Settings,
}
impl Frozen {
    pub(crate) fn automatic_operation(&self) -> Uuid {
        Uuid::new_v5(&self.id, b"automatic-undo-merge")
    }
    pub(crate) fn capture(job: &Job, settings: Settings) -> Result<Self> {
        let (plan, conflict, hash) = inputs(job)?;
        if settings.hash()? != hash {
            return Err("The undo merge settings changed.".into());
        }
        let track = plan.records[conflict.applied.len()].track_id.clone();
        let label = job
            .tracks
            .iter()
            .find(|t| t.id == track)
            .ok_or("The undo record has no saved source track.")?
            .label
            .clone();
        Ok(Self {
            id: identity(job, plan, conflict, hash)?,
            job: job.id,
            revision: job.revision,
            track,
            label,
            session_id: job.session_id.clone(),
            session_name: job.session_name.clone(),
            slug: job.slug.clone(),
            project: job.project.clone(),
            plan: plan.clone(),
            conflict: conflict.clone(),
            settings,
        })
    }
    pub(crate) fn files(&self, marker: Uuid) -> Result<crate::merge_workspace::Input> {
        Ok(crate::merge_workspace::Input {
            binding: Sha256::digest(serde_json::to_vec(self)?)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            track: self.track.clone(),
            head: self.plan.parent.clone(),
            base: self.conflict.base.clone(),
            target: self.conflict.target.clone(),
            incoming: self.conflict.incoming.clone(),
            conflicts: self.conflict.paths.clone(),
            marker,
        })
    }
    pub(crate) fn pin(&self) -> Result<()> {
        let repo = Repository::open(&self.project.root)?;
        if repo.common_directory()? != self.project.common_dir
            || !repo.owns_session(&self.slug, &self.session_id)?
        {
            return Err("The undo merge repository reservation changed.".into());
        }
        self.plan.verify_records(&repo)?;
        let mut merges = crate::merge_choices::Merges::new(&repo, &self.plan.resolutions)?;
        let reversed = crate::replacement_delta::reverse(
            &mut merges,
            repo.tree_id(self.plan.parent.parse()?)?,
            &self.plan.records,
        )?;
        merges.finish()?;
        let Merge::Conflicts {
            base,
            target,
            incoming,
            paths,
        } = reversed.result
        else {
            return Err("The saved undo no longer has this conflict.".into());
        };
        if reversed.applied != self.conflict.applied
            || base != self.conflict.base
            || target != self.conflict.target
            || incoming != self.conflict.incoming
            || paths.iter().map(ToString::to_string).collect::<Vec<_>>() != self.conflict.paths
        {
            return Err("The undo merge no longer reproduces its frozen inputs.".into());
        }
        let objects = [
            ("head", self.plan.parent.parse()?),
            ("base", base),
            ("target", target),
            ("incoming", incoming),
        ];
        repo.sync_objects(&objects.iter().map(|(_, id)| *id).collect::<Vec<_>>())?;
        for (name, object) in objects {
            let reference = format!("refs/intercat/{}/undo-merges/{}/{name}", self.slug, self.id);
            repo.compare_and_swap(&reference, None, object)?;
            repo.sync_ref(&reference)?;
        }
        Ok(())
    }
}

pub struct Workspace {
    pub files: crate::merge_workspace::Workspace,
    pub(crate) input: Box<Frozen>,
}
impl Workspace {
    pub(crate) async fn open(
        input: Frozen,
        root: &std::path::Path,
        run: &crate::repair_run::Run,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<Self> {
        if run.owner
            != (crate::repair_run::Owner::Undo {
                id: input.job,
                revision: input.revision,
            })
            || run.track != input.track
            || run.purpose != crate::repair_run::Purpose::Merge
        {
            return Err("The undo merge belongs to another Run.".into());
        }
        let pin = input.clone();
        tokio::task::spawn_blocking(move || pin.pin()).await??;
        let original = crate::merge_source::open(
            &input.project.root,
            root.parent().ok_or("Missing undo Run directory.")?,
            input.id,
            input.files(input.id)?,
            &input.settings,
            cancel.clone(),
        )
        .await?;
        let files = if run.workspace_ready {
            crate::merge_workspace::Workspace::resume(
                &original,
                root,
                input.files(run.id)?,
                &input.settings.path,
                &input.settings.toolchain,
            )
            .await?
        } else {
            if root.exists() {
                crate::repair_service::remove_unprepared(root)?;
            }
            crate::merge_workspace::Workspace::prepare(
                &original,
                root,
                input.files(run.id)?,
                &input.settings.path,
                &input.settings.toolchain,
                cancel,
            )
            .await?
        };
        Ok(Self {
            files,
            input: Box::new(input),
        })
    }
    pub(crate) fn validate(&self, input: &Frozen) -> Result<()> {
        if serde_json::to_value(&self.input)? != serde_json::to_value(input)?
            || self.files.input != input.files(self.files.input.marker)?
        {
            return Err("The merge files belong to another frozen undo.".into());
        }
        Ok(())
    }
}
