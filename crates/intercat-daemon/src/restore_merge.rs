//! Each Restore conflict has an immutable source, independent of mutable job
//! progress and earlier/later conflicts in the same Restore.
use crate::{
    Result,
    candidate::Settings,
    path_restore_store::{Anchor, Application, Plan},
};
use intercat_git::Repository;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Summary {
    pub id: Uuid,
    pub revision: u64,
    pub track: String,
    pub label: String,
    pub stage: crate::path_restore::Stage,
    pub paths: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Frozen {
    pub id: Uuid,
    pub revision: u64,
    pub request: crate::path_restore::Request,
    pub source: crate::path_restore_source::Context,
    pub inputs: crate::path_restore::Inputs,
    pub conflict: crate::path_restore::Conflict,
    pub anchor: Anchor,
    pub head: String,
    pub project: crate::project::Folder,
    pub session_name: String,
    pub settings: Settings,
}
impl Frozen {
    pub(crate) fn identity(request: Uuid, revision: u64) -> Uuid {
        Uuid::new_v5(&request, format!("restore-merge/{revision}").as_bytes())
    }
    pub(crate) fn summary(&self) -> Summary {
        Summary {
            id: self.request.op_id,
            revision: self.revision,
            track: self.source.entry.track.clone(),
            label: self.source.entry.label.clone(),
            stage: self.conflict.stage,
            paths: self.conflict.paths.clone(),
        }
    }
    pub(crate) fn automatic_operation(&self) -> Uuid {
        Uuid::new_v5(&self.id, b"automatic-restore-merge")
    }
    pub(crate) fn files(&self, marker: Uuid) -> Result<crate::merge_workspace::Input> {
        self.anchor.validate(&self.request)?;
        if self.revision
            != u64::try_from(self.inputs.resolutions.len())?
                .checked_add(1)
                .ok_or("Restore merge revision overflow.")?
            || self.id != Self::identity(self.request.op_id, self.revision)
            || self.inputs.track != self.source.entry.track
        {
            return Err("The frozen Restore merge identity changed.".into());
        }
        Ok(crate::merge_workspace::Input {
            binding: Sha256::digest(serde_json::to_vec(self)?)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            track: self.source.entry.track.clone(),
            head: self.head.clone(),
            base: self.conflict.base.clone(),
            target: self.conflict.target.clone(),
            incoming: self.conflict.incoming.clone(),
            conflicts: self.conflict.paths.clone(),
            marker,
        })
    }
    pub(crate) fn matches(&self, app: &Application) -> Result<bool> {
        let Plan::Conflict {
            inputs,
            conflict,
            anchor,
        } = &app.plan
        else {
            return Ok(false);
        };
        Ok(app.request == self.request
            && serde_json::to_value(&app.source)? == serde_json::to_value(&self.source)?
            && **inputs == self.inputs
            && **conflict == self.conflict
            && *anchor == self.anchor)
    }
    pub(crate) fn pin(&self) -> Result<()> {
        self.files(self.id)?;
        let repo = Repository::open(&self.project.root)?;
        if repo.common_directory()? != self.project.common_dir
            || !repo.owns_session(&self.source.entry.slug, &self.source.entry.session)?
        {
            return Err("The Restore merge repository reservation changed.".into());
        }
        let crate::path_restore::Outcome::Conflict(actual) =
            crate::path_restore::prepare(&repo, &self.source.graph, &self.inputs)?
        else {
            return Err("The saved Restore no longer has this merge conflict.".into());
        };
        let head = repo.commit_details(self.head.parse()?)?;
        if *actual != self.conflict
            || head.tree != self.conflict.target.parse::<intercat_git::ObjectId>()?
            || head.parents != vec![self.inputs.before.parse::<intercat_git::ObjectId>()?]
        {
            return Err("The Restore merge no longer reproduces its saved inputs.".into());
        }
        let objects = [
            ("head", self.head.as_str()),
            ("before", self.inputs.before.as_str()),
            ("base", self.conflict.base.as_str()),
            ("target", self.conflict.target.as_str()),
            ("incoming", self.conflict.incoming.as_str()),
        ];
        repo.sync_objects(
            &objects
                .iter()
                .map(|(_, sha)| sha.parse())
                .collect::<std::result::Result<Vec<_>, _>>()?,
        )?;
        for (name, sha) in objects {
            let reference = format!(
                "refs/intercat/{}/restore-merges/{}/{name}",
                self.source.entry.slug, self.id
            );
            repo.compare_and_swap(&reference, None, sha.parse()?)?;
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
            != (crate::repair_run::Owner::Restore {
                id: input.request.op_id,
                revision: input.revision,
            })
            || run.track != input.source.entry.track
            || run.purpose != crate::repair_run::Purpose::Merge
        {
            return Err("The Restore merge belongs to another Run.".into());
        }
        let pins = input.clone();
        tokio::task::spawn_blocking(move || pins.pin()).await??;
        let original = crate::merge_source::open(
            &input.project.root,
            root.parent().ok_or("Missing Restore merge directory.")?,
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
            return Err("The private files belong to another Restore conflict.".into());
        }
        Ok(())
    }
}
pub(crate) async fn read(journal: &crate::journal::Journal, summary: &Summary) -> Result<Frozen> {
    let saved = journal
        .operation(Frozen::identity(summary.id, summary.revision))
        .await?
        .ok_or("Missing frozen Restore merge.")?;
    if saved["kind"] != "restore.merge.inputs" || saved["state"] != "complete" {
        return Err("The Restore merge inputs are unavailable.".into());
    }
    let input: Frozen = serde_json::from_value(saved["payload"].clone())?;
    if input.summary() != *summary {
        return Err("The Restore merge summary changed.".into());
    }
    Ok(input)
}
pub(crate) async fn freeze(journal: &crate::journal::Journal, app: Application) -> Result<Frozen> {
    let Plan::Conflict {
        inputs,
        conflict,
        anchor,
    } = &app.plan
    else {
        return Err("The Restore has no conflict to freeze.".into());
    };
    let revision = u64::try_from(inputs.resolutions.len())?
        .checked_add(1)
        .ok_or("Restore merge revision overflow.")?;
    let id = Frozen::identity(app.request.op_id, revision);
    let input = if let Some(saved) = journal.operation(id).await? {
        if saved["kind"] != "restore.merge.inputs" || saved["state"] != "complete" {
            return Err("The Restore merge input identity was reused.".into());
        }
        let input: Frozen = serde_json::from_value(saved["payload"].clone())?;
        if !input.matches(&app)? {
            return Err("The saved Restore conflict changed.".into());
        }
        input
    } else {
        let job = journal.integration_job(app.source.entry.job).await?;
        let mut settings = journal
            .integration_settings(job.id, job.revision)
            .await?
            .ok_or("Missing saved Restore merge settings.")?;
        let (path, before, target) = (
            app.source.entry.workspace.path.clone(),
            inputs.before.clone(),
            conflict.target.clone(),
        );
        let (checkout, head) = tokio::task::spawn_blocking(move || -> Result<_> {
            let repo = Repository::open(&path)?;
            let head =
                repo.checkpoint_commit(target.parse()?, before.parse()?, "Restore merge target")?;
            let source = Repository::worktree_source(&path, Some(before.parse()?))?;
            let checkout = crate::checkout::Configuration::capture(&source)?;
            repo.sync_objects(&[head])?;
            Ok((checkout, head.to_string()))
        })
        .await??;
        settings.checkout = checkout;
        Frozen {
            id,
            revision,
            request: app.request.clone(),
            source: app.source.clone(),
            inputs: *inputs.clone(),
            conflict: *conflict.clone(),
            anchor: anchor.clone(),
            head,
            project: job.project,
            session_name: job.session_name,
            settings,
        }
    };
    journal
        .change_restore_merge(crate::restore_merge_store::Change::Freeze(Box::new(
            input.clone(),
        )))
        .await?;
    let pins = input.clone();
    tokio::task::spawn_blocking(move || pins.pin()).await??;
    Ok(input)
}
