//! Build one undo prefix from immutable integration receipts. This does not
//! publish refs or mark receipts cancelled; the final candidate still needs Check.
use crate::{
    Result,
    candidate::{Settings, State},
    candidate_commit,
    checkout::Checkout,
    integration::Job,
    integration_record::Record,
};
use intercat_git::{Merge, ObjectId, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use tokio::sync::watch;
use uuid::Uuid;

const TRAILER: &str = "Intercat-Undo-Record: ";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Plan {
    pub parent: String,
    pub message: String,
    pub records: Vec<Record>,
    pub resolutions: Vec<crate::merge_choices::Solved>,
}
impl Plan {
    pub fn verify_records(&self, repository: &Repository) -> Result<()> {
        let parent = self.parent.parse()?;
        for record in &self.records {
            let sha = record.sha.parse()?;
            let actual = repository.commit_details(sha)?;
            if repository.reference_target(&record.reference)? != Some(sha)
                || repository.merge_base(sha, parent)? != Some(sha)
                || actual.parents != vec![record.parent.parse::<ObjectId>()?]
                || actual.tree != record.tree
            {
                return Err(
                    "The undo target does not contain its protected integration records.".into(),
                );
            }
        }
        Ok(())
    }
    pub fn validate(&self, job: &Job) -> Result<()> {
        if self.parent
            != job
                .plan
                .as_ref()
                .ok_or("Missing replacement target.")?
                .target_head
            || self.records.is_empty()
            || self.records.len() > 4096
            || self.resolutions.len() > 4096
            || self.message.trim().is_empty()
            || self.message.len() > 16 * 1024
            || self.message.contains('\0')
            || self.message.contains(TRAILER)
        {
            return Err("Invalid frozen replacement undo plan.".into());
        }
        self.parent.parse::<ObjectId>()?;
        let mut ids = BTreeSet::new();
        let mut previous = None;
        for record in &self.records {
            record.validate()?;
            if record.session_id != job.session_id
                || record.folder_id != job.project.id
                || !ids.insert(record.id)
                || previous.is_some_and(|position| position <= record.position)
            {
                return Err(
                    "Undo records must identify one folder in reverse publication order.".into(),
                );
            }
            previous = Some(record.position);
        }
        Ok(())
    }
    pub fn ids(&self) -> Vec<Uuid> {
        self.records.iter().map(|record| record.id).collect()
    }
    fn commit_message(&self) -> String {
        let mut message = format!("{}\n\n", self.message.trim_end());
        for record in &self.records {
            message.push_str(&format!("{TRAILER}{}\n", record.id));
        }
        message
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Commit {
    pub sha: String,
    pub parent: String,
    pub tree: String,
    pub message: String,
    pub author: String,
    pub records: Vec<Uuid>,
    pub changes: Vec<crate::integration::Change>,
    pub scan: crate::secret_scan::Scan,
}
impl Commit {
    /// Verify exact objects and scan inputs again immediately before publication.
    /// A caller must additionally require this scan's findings to be accepted.
    pub fn verify(&self, repository: &Repository, plan: &Plan, settings: &Settings) -> Result<()> {
        let details = repository.commit_details(self.sha.parse()?)?;
        let message_hash = Sha256::digest(details.message.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if self.parent != plan.parent
            || self.records != plan.ids()
            || details.parents != vec![self.parent.parse::<ObjectId>()?]
            || details.tree != self.tree
            || details.author_name != self.author
            || self.author != "Intercat"
            || self.scan.binding.commit != self.sha
            || self.scan.binding.tree != self.tree
            || self.scan.binding.parent_tree != repository.tree_id(self.parent.parse()?)?
            || self.scan.binding.message_hash != message_hash
            || self.scan.binding.ruleset != crate::secret_scan::ruleset()
            || self.scan.binding.value_fingerprints
                != crate::secret_scan::value_fingerprints(
                    &settings
                        .environment_values()?
                        .into_values()
                        .collect::<Vec<_>>(),
                )
        {
            return Err(
                "The replacement undo no longer matches its reviewed objects and scan.".into(),
            );
        }
        verify_trailers(&details.message, &self.records)
    }
}
fn verify_trailers(message: &str, expected: &[Uuid]) -> Result<()> {
    let actual = message
        .lines()
        .filter_map(|line| line.strip_prefix(TRAILER))
        .map(Uuid::parse_str)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if actual != expected {
        return Err("A commit hook changed the replacement undo record IDs.".into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Conflict {
    pub track: String,
    pub applied: Vec<Uuid>,
    pub remaining: Vec<Uuid>,
    pub base: String,
    pub target: String,
    pub incoming: String,
    pub paths: Vec<String>,
}
pub struct Built {
    pub commit: Commit,
    pub dependency_runs: Vec<crate::dependencies::Run>,
    pub left_out: Vec<String>,
}
pub struct Rejected {
    pub state: State,
    pub reason: String,
    pub dependency_error: Option<crate::dependencies::Failure>,
    pub left_out: Vec<String>,
}
pub enum Outcome {
    Built(Box<Built>),
    Conflict(Conflict),
    Rejected(Rejected),
}

pub async fn build(
    job: &Job,
    checkout: &Checkout,
    plan: &Plan,
    settings: &Settings,
    dependencies: &mut crate::dependencies::Preparer,
    scanner: &crate::secret_scan::Scanner,
    cancel: watch::Receiver<bool>,
) -> Result<Outcome> {
    plan.validate(job)?;
    if *cancel.borrow() {
        return Ok(Outcome::Rejected(Rejected {
            state: State::Cancelled,
            reason: "Replacement undo stopped.".into(),
            dependency_error: None,
            left_out: vec![],
        }));
    }
    let parent = plan.parent.parse()?;
    let source = Repository::open(&job.project.root)?;
    plan.verify_records(&source)?;
    let private = Repository::open(&checkout.workdir)?;
    if private.head_commit()? != parent {
        return Err("The replacement checkout no longer starts at the reviewed target.".into());
    }
    let mut merges = crate::merge_choices::Merges::new(&source, &plan.resolutions)?;
    let reversed =
        crate::replacement_delta::reverse(&mut merges, source.tree_id(parent)?, &plan.records)?;
    merges.finish()?;
    let tree = match reversed.result {
        Merge::Clean(tree) => tree,
        Merge::Conflicts {
            base,
            target,
            incoming,
            paths,
        } => {
            return Ok(Outcome::Conflict(Conflict {
                track: plan.records[reversed.applied.len()].track_id.clone(),
                remaining: plan
                    .ids()
                    .into_iter()
                    .skip(reversed.applied.len())
                    .collect(),
                applied: reversed.applied,
                base: base.to_string(),
                target: target.to_string(),
                incoming: incoming.to_string(),
                paths: paths.into_iter().map(|path| path.to_string()).collect(),
            }));
        }
    };
    private.import_tree(&job.project.root, tree)?;
    crate::lfs_store::transfer(
        &settings.checkout.lfs_directory,
        &checkout.workdir.join(".git/lfs"),
        &private.lfs_entries(tree)?,
    )?;
    private.sync_objects(&[tree])?;
    checkout.deploy(tree, cancel.clone()).await?;
    let runs = match dependencies
        .prepare(checkout, &private, tree, false, cancel.clone())
        .await
    {
        Ok(runs) => runs,
        Err(error) => {
            return Ok(Outcome::Rejected(Rejected {
                state: if error.kind == crate::dependencies::FailureKind::Cancelled {
                    State::Cancelled
                } else {
                    State::EnvFailed
                },
                reason: error.message.clone(),
                dependency_error: Some(error),
                left_out: vec![],
            }));
        }
    };
    let made = match candidate_commit::create(
        checkout,
        settings,
        candidate_commit::Request {
            tree,
            parent,
            attachment_base: parent,
            author: "Intercat <intercat@localhost>",
            message: &plan.commit_message(),
            allow_empty: true,
        },
        scanner,
        cancel,
    )
    .await?
    {
        candidate_commit::Outcome::Made(made) => made,
        candidate_commit::Outcome::Rejected(rejected) => {
            return Ok(Outcome::Rejected(Rejected {
                state: rejected.state,
                reason: rejected.reason,
                dependency_error: None,
                left_out: rejected.left_out,
            }));
        }
    };
    verify_trailers(&made.details.message, &plan.ids())?;
    let commit = Commit {
        sha: made.sha.to_string(),
        parent: plan.parent.clone(),
        tree: made.details.tree.to_string(),
        message: made.details.message,
        author: made.details.author_name,
        records: plan.ids(),
        changes: crate::integration::changes(&private, source.tree_id(parent)?, made.details.tree)?,
        scan: made.scan,
    };
    commit.verify(&private, plan, settings)?;
    Ok(Outcome::Built(Box::new(Built {
        commit,
        dependency_runs: runs,
        left_out: made.left_out,
    })))
}
