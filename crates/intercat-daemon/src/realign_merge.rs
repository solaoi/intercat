//! Immutable conflict inputs for a realignment. These are not Commit reviews:
//! the worker's files stay untouched until its complete plan can be replayed.
use crate::{
    Result,
    candidate::Settings,
    realign::{Plan, Request, Resolution},
};
use intercat_git::Repository;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Frozen {
    pub id: Uuid,
    pub request: Request,
    pub workspace: crate::preparation::Workspace,
    pub session: String,
    pub project: crate::project::Folder,
    pub session_name: String,
    pub label: String,
    pub plan: Plan,
    pub settings: Settings,
    pub head: String,
}
impl Frozen {
    pub(crate) fn automatic_operation(&self) -> Uuid {
        Uuid::new_v5(&self.id, b"automatic-realignment-merge")
    }
    pub(crate) fn identity(request: &Request, plan: &Plan) -> Result<Uuid> {
        let digest = Sha256::digest(serde_json::to_vec(plan)?);
        Ok(Uuid::new_v5(&request.op_id, &digest))
    }
    pub(crate) fn files(&self, marker: Uuid) -> Result<crate::merge_workspace::Input> {
        let Resolution::Conflict {
            base,
            target,
            incoming,
            paths,
            ..
        } = &self.plan.resolution
        else {
            return Err("The saved realignment has no conflict.".into());
        };
        Ok(crate::merge_workspace::Input {
            binding: Sha256::digest(serde_json::to_vec(self)?)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            track: self.request.track_id.clone(),
            head: self.head.clone(),
            base: base.clone(),
            target: target.clone(),
            incoming: incoming.clone(),
            conflicts: paths.clone(),
            marker,
        })
    }
    pub(crate) fn pin(&self) -> Result<()> {
        if self.id != Self::identity(&self.request, &self.plan)?
            || self.plan.track != self.request.track_id
        {
            return Err("The saved realignment inputs belong to another request.".into());
        }
        let input = self.files(self.id)?;
        let repo = Repository::open(&self.workspace.path)?;
        if repo.common_directory()? != self.project.common_dir
            || repo.working_directory()? != self.workspace.path.canonicalize()?
        {
            return Err("The realignment worker repository changed.".into());
        }
        let objects = [
            ("before", self.plan.before.as_str()),
            ("head", self.head.as_str()),
            ("base", input.base.as_str()),
            ("target", input.target.as_str()),
            ("incoming", input.incoming.as_str()),
        ];
        let ids: Vec<intercat_git::ObjectId> = objects
            .iter()
            .map(|(_, sha)| sha.parse())
            .collect::<std::result::Result<_, _>>()?;
        repo.sync_objects(&ids)?;
        for (name, sha) in objects {
            let reference = format!(
                "{}-realign-merges/{}/{name}",
                self.workspace.reference, self.id
            );
            repo.compare_and_swap(&reference, None, sha.parse()?)?;
            repo.sync_ref(&reference)?;
        }
        Ok(())
    }
    pub(crate) fn sync_objects(&self) -> Result<()> {
        let input = self.files(self.id)?;
        let repo = Repository::open(&self.workspace.path)?;
        if repo.common_directory()? != self.project.common_dir {
            return Err("The realignment repository changed.".into());
        }
        let ids: Vec<intercat_git::ObjectId> = [
            &self.plan.before,
            &self.head,
            &input.base,
            &input.target,
            &input.incoming,
        ]
        .into_iter()
        .map(|sha| sha.parse())
        .collect::<std::result::Result<_, _>>()?;
        repo.sync_objects(&ids)
    }
    async fn original(
        &self,
        parent: &std::path::Path,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<crate::checkout::Checkout> {
        crate::merge_source::open(
            &self.workspace.path,
            parent,
            self.id,
            self.files(self.id)?,
            &self.settings,
            cancel,
        )
        .await
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
        let parent = root.parent().ok_or("Missing realignment Run directory.")?;
        std::fs::create_dir_all(parent)?;
        let original = input.original(parent, cancel.clone()).await?;
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
            return Err("The merge files belong to another realignment.".into());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct Required;
impl std::fmt::Display for Required {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Realignment has conflicts · Resolve in an isolated merge")
    }
}
impl std::error::Error for Required {}

pub(crate) async fn ensure_ready(journal: &crate::journal::Journal, id: Uuid) -> Result<Frozen> {
    let saved = journal
        .operation(id)
        .await?
        .ok_or("Missing realignment conflict inputs.")?;
    if saved["kind"] != "realign.merge.inputs" {
        return Err("The conflict inputs belong to another operation.".into());
    }
    let input: Frozen = serde_json::from_value(saved["payload"].clone())?;
    if input.id != id {
        return Err("The realignment conflict identity changed.".into());
    }
    if saved["state"] == "prepared" {
        let pin = input.clone();
        tokio::task::spawn_blocking(move || pin.pin()).await??;
        journal
            .change_realign(crate::realign_store::Change::MergeReady(id))
            .await?;
    } else if saved["state"] != "complete" {
        return Err("The conflict inputs are unavailable.".into());
    }
    Ok(input)
}
