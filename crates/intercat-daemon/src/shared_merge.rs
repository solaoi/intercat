//! A shared checkpoint's conflict owns a private merge, independently of the
//! retired worker and every descendant that will consume its normalized view.
use crate::{
    Result,
    candidate::Settings,
    shared_context::Outcome,
    shared_context_store::{Phase, Record},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Summary {
    pub id: Uuid,
    pub revision: u64,
    pub track: String,
    pub label: String,
    pub checkpoint: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Frozen {
    pub id: Uuid,
    pub record: Record,
    pub settings: Settings,
    pub session_name: String,
    pub label: String,
}
impl Frozen {
    pub(crate) fn identity(step: Uuid, revision: u64) -> Uuid {
        Uuid::new_v5(&step, format!("shared-merge/{revision}").as_bytes())
    }
    pub(crate) fn summary(&self) -> Summary {
        Summary {
            id: self.record.id,
            revision: self.record.revision,
            track: self.record.input.projection.source.track.clone(),
            label: self.label.clone(),
            checkpoint: self.record.input.projection.source.number,
        }
    }
    pub(crate) fn automatic_operation(&self) -> Uuid {
        Uuid::new_v5(&self.id, b"automatic-shared-merge")
    }
    pub(crate) fn files(&self, marker: Uuid) -> Result<crate::merge_workspace::Input> {
        if self.id != Self::identity(self.record.id, self.record.revision)
            || self.record.phase != Phase::NeedsMerge
        {
            return Err("The shared merge's frozen identity changed.".into());
        }
        let computed = self
            .record
            .computed
            .as_ref()
            .ok_or("Missing shared conflict.")?;
        let Outcome::Conflict(conflict) = &computed.outcome else {
            return Err("The shared step has no conflict.".into());
        };
        Ok(crate::merge_workspace::Input {
            binding: Sha256::digest(serde_json::to_vec(self)?)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            track: self.record.input.projection.source.track.clone(),
            head: computed.anchor.clone(),
            base: conflict.base.clone(),
            target: conflict.target.clone(),
            incoming: conflict.incoming.clone(),
            conflicts: conflict.paths.clone(),
            marker,
        })
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
            != (crate::repair_run::Owner::Shared {
                id: input.record.id,
                revision: input.record.revision,
            })
            || run.track != input.record.input.projection.source.track
            || run.purpose != crate::repair_run::Purpose::Merge
        {
            return Err("The shared merge belongs to another Run.".into());
        }
        let parent = root.parent().ok_or("Missing shared merge directory.")?;
        let original = crate::merge_source::open(
            &input.record.input.project.root,
            parent,
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
            return Err("The merge files belong to another shared checkpoint.".into());
        }
        Ok(())
    }
}

pub(crate) async fn freeze(
    journal: &crate::journal::Journal,
    request: crate::realign::Request,
    record: Record,
) -> Result<Frozen> {
    let id = Frozen::identity(record.id, record.revision);
    let input = if let Some(saved) = journal.operation(id).await? {
        if saved["kind"] != "shared.merge.inputs" || saved["state"] != "complete" {
            return Err("The shared merge input identity was reused.".into());
        }
        let input: Frozen = serde_json::from_value(saved["payload"].clone())?;
        if input.id != id || serde_json::to_value(&input.record)? != serde_json::to_value(&record)?
        {
            return Err("The frozen shared source changed.".into());
        }
        input
    } else {
        let job = journal.realign_context(request.clone()).await?.source;
        let label = job
            .tracks
            .iter()
            .find(|track| track.id == record.input.projection.source.track)
            .ok_or("The shared source has no saved label.")?
            .label
            .clone();
        let mut settings = journal
            .integration_settings(job.id, job.revision)
            .await?
            .ok_or("Missing shared merge settings.")?;
        let source = record.clone();
        settings.checkout = tokio::task::spawn_blocking(move || -> Result<_> {
            let computed = source
                .computed
                .as_ref()
                .ok_or("Missing shared merge anchor.")?;
            let checkout = intercat_git::Repository::worktree_source(
                &source.input.project.root,
                Some(computed.anchor.parse()?),
            )?;
            crate::checkout::Configuration::capture(&checkout)
        })
        .await??;
        Frozen {
            id,
            record,
            settings,
            session_name: job.session_name,
            label,
        }
    };
    journal
        .change_shared_merge(crate::shared_merge_store::Change::Freeze {
            request,
            input: Box::new(input.clone()),
        })
        .await?;
    Ok(input)
}
pub(crate) async fn read(journal: &crate::journal::Journal, summary: &Summary) -> Result<Frozen> {
    let id = Frozen::identity(summary.id, summary.revision);
    let saved = journal
        .operation(id)
        .await?
        .ok_or("Missing shared merge inputs.")?;
    if saved["kind"] != "shared.merge.inputs" || saved["state"] != "complete" {
        return Err("Shared merge inputs are unavailable.".into());
    }
    let input: Frozen = serde_json::from_value(saved["payload"].clone())?;
    if input.id != id || serde_json::to_value(input.summary())? != serde_json::to_value(summary)? {
        return Err("The shared merge summary changed.".into());
    }
    input.files(id)?;
    Ok(input)
}
