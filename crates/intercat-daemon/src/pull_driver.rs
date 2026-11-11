//! Durable confirmation is the only route to GitHub writes.
use crate::{
    Result,
    editor::Manager,
    github_auth::Credentials,
    github_pull::{Client, Token},
    journal::Journal,
    pull_changes::Change,
    pull_request::{Job, Phase},
    push_inspect::Inspector,
};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;
#[derive(Debug)]
struct Problem(&'static str);
impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Problem {}
#[derive(Clone)]
pub(crate) struct Driver {
    pub journal: Journal,
    pub editor: Manager,
    pub client: Arc<Client>,
    pub credentials: Arc<Credentials>,
    pub inspector: Arc<Inspector>,
}
impl Driver {
    async fn read(&self, id: Uuid) -> Result<Job> {
        self.journal.pull_job(id).await
    }
    async fn change(&self, id: Uuid, change: Change) -> Result<Job> {
        self.journal.change_pull(id, change).await
    }
    async fn destinations(&self, job: &Job, cancel: watch::Receiver<bool>) -> Result<()> {
        let current = self.journal.push_context(job.push.request.clone()).await?;
        if current.project_id != job.push.project_id || current.session_id != job.push.session_id {
            return Err(Box::new(Problem(
                "The project or session changed. Reopen its PR review.",
            )));
        }
        for member in &job.members {
            let source = job
                .push
                .members
                .iter()
                .find(|source| source.folder.id == member.folder_id)
                .ok_or("Missing reviewed PR folder.")?;
            let raw = self
                .inspector
                .destinations(&source.folder, &source.target.remote, cancel.clone())
                .await?;
            source.target.verify_destinations(
                &source.target.remote,
                &source.target.target_ref,
                &raw,
            )?;
        }
        Ok(())
    }
    async fn token(&self, job: &Job, cancel: watch::Receiver<bool>) -> Result<Token> {
        let folders = job
            .push
            .members
            .iter()
            .map(|source| source.folder.clone())
            .collect::<Vec<_>>();
        self.credentials.token(&folders, cancel).await.map_err(|_|Box::new(Problem("Sign in to GitHub CLI, then retry PR preparation. The authentication value is not recorded.")) as Box<dyn std::error::Error+Send+Sync>)
    }
    async fn checked_token(&self, job: &Job, cancel: watch::Receiver<bool>) -> Result<Token> {
        self.destinations(job, cancel.clone()).await?;
        let token = self.token(job, cancel).await?;
        let account = self.client.account(&token).await?;
        if job
            .account
            .as_ref()
            .is_none_or(|reviewed| reviewed.id != account.id)
        {
            return Err(Box::new(Problem(
                "The GitHub account changed. Sign in as the reviewed account before retrying.",
            )));
        }
        Ok(token)
    }
    async fn lookup(&self, job: &Job, cancel: watch::Receiver<bool>) -> Result<()> {
        self.destinations(job, cancel.clone()).await?;
        let token = self.token(job, cancel).await?;
        let account = self.client.account(&token).await?;
        let mut pulls = Vec::new();
        let mut drafts = Vec::new();
        for member in &job.members {
            self.client.verify_head(&member.target, &token).await?;
            let mut matching = self.client.existing(&member.target, &token).await?;
            if matching.len() > 1 {
                return Err(Box::new(Problem(
                    "More than one PR matches this branch. Resolve the duplicate PRs before continuing.",
                )));
            }
            // The exact existing text is retained for concurrent-edit checks;
            // only its screened draft is exposed for review.
            let pull = matching.pop();
            let draft = if let Some(pull) = &pull {
                crate::github_repository::Draft {
                    title: pull.title.clone(),
                    body: pull.body.clone(),
                }
            } else {
                member.draft.clone().ok_or("Missing generated PR text.")?
            };
            drafts.push(crate::pull_text::screen(&self.journal, &draft).await?.0);
            pulls.push(pull);
        }
        self.change(
            job.id,
            Change::LookedUp {
                account,
                pulls,
                drafts,
            },
        )
        .await?;
        Ok(())
    }
    async fn create(&self, job: &Job, index: usize, cancel: watch::Receiver<bool>) -> Result<()> {
        let member = &job.members[index];
        let token = self.checked_token(job, cancel).await?;
        let draft = member.draft.as_ref().ok_or("Missing approved PR text.")?;
        let (safe, _lease) = crate::pull_text::screen(&self.journal, draft).await?;
        if safe != *draft {
            return Err(Box::new(Problem(
                "Privacy rules changed the PR text. Review it before writing to GitHub.",
            )));
        }
        if let Some(existing) = &member.existing {
            let current = self
                .client
                .pull(&member.target, existing.number, &token)
                .await?;
            if current != *existing {
                return Err(
                    "The existing PR changed after review. Its text has not been overwritten."
                        .into(),
                );
            }
            self.change(
                job.id,
                Change::Created {
                    folder: member.folder_id.clone(),
                    pull: current,
                },
            )
            .await?;
        } else {
            // Recheck before POST, including a PR created by another client.
            if !self
                .client
                .existing(&member.target, &token)
                .await?
                .is_empty()
            {
                return Err(
                    "A PR was created after review. Reopen its current result before continuing."
                        .into(),
                );
            }
            self.change(
                job.id,
                Change::BeginCreate {
                    folder: member.folder_id.clone(),
                },
            )
            .await?;
            let result = self.client.create(&member.target, draft, &token).await?;
            // Retain a known URL even if the live branch advanced during POST.
            self.change(
                job.id,
                Change::Created {
                    folder: member.folder_id.clone(),
                    pull: result,
                },
            )
            .await?;
        }
        Ok(())
    }
    async fn find_result(
        &self,
        job: &Job,
        index: usize,
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        let member = &job.members[index];
        let token = self.checked_token(job, cancel).await?;
        let expected = member.draft.as_ref().ok_or("Missing approved PR text.")?;
        let pulls = self.client.history(&member.target, &token).await?;
        let matching = pulls
            .into_iter()
            .filter(|pull| {
                pull.head_sha == member.target.sha
                    && pull.title == expected.title
                    && pull.body == expected.body
            })
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            return Err(Box::new(Problem(
                "The interrupted creation has no unique verified result. No new PR was created. Check this branch on GitHub before trying again.",
            )));
        }
        self.change(
            job.id,
            Change::Created {
                folder: member.folder_id.clone(),
                pull: matching.into_iter().next().unwrap(),
            },
        )
        .await?;
        Ok(())
    }
    async fn link(&self, job: &Job, index: usize, cancel: watch::Receiver<bool>) -> Result<()> {
        let member = &job.members[index];
        let result = member.result.as_ref().ok_or("Missing saved PR result.")?;
        let expected = crate::pull_text::linked(job, index)?;
        let (safe, _lease) = crate::pull_text::screen(&self.journal, &expected).await?;
        if safe != expected {
            return Err(Box::new(Problem(
                "Privacy rules changed the linked PR text. Nothing was updated.",
            )));
        }
        let token = self.checked_token(job, cancel).await?;
        let current = self
            .client
            .pull(&member.target, result.number, &token)
            .await?;
        if current.head_sha != member.target.sha || current.state != "open" {
            return Err(Box::new(Problem(
                "The PR branch or state changed. Its saved result is retained.",
            )));
        }
        let already = current.title == expected.title && current.body == expected.body;
        if !already && (current.title != result.title || current.body != result.body) {
            return Err(Box::new(Problem(
                "The PR text was edited elsewhere. Intercat has not overwritten it.",
            )));
        }
        self.change(
            job.id,
            Change::BeginLink {
                folder: member.folder_id.clone(),
            },
        )
        .await?;
        let pull = if already {
            current
        } else {
            self.client
                .edit(&member.target, result.number, &expected, &token)
                .await?
        };
        if pull.title != expected.title
            || pull.body != expected.body
            || pull.head_sha != member.target.sha
        {
            return Err(
                "GitHub returned different PR content. Read the existing result before continuing."
                    .into(),
            );
        }
        self.change(
            job.id,
            Change::Linked {
                folder: member.folder_id.clone(),
                pull,
            },
        )
        .await?;
        Ok(())
    }
    async fn step(&self, job: &Job, index: usize, cancel: watch::Receiver<bool>) -> Result<()> {
        let member = &job.members[index];
        match member.phase {
            Phase::Drafting => {
                let draft =
                    crate::pull_text::generate(&self.journal, &self.editor, &member.sources)
                        .await?;
                self.change(
                    job.id,
                    Change::Drafted {
                        folder: member.folder_id.clone(),
                        draft,
                    },
                )
                .await?;
            }
            Phase::LookingUp => self.lookup(job, cancel).await?,
            Phase::Ready => self.create(job, index, cancel).await?,
            Phase::Checking => self.find_result(job, index, cancel).await?,
            Phase::Created | Phase::Linking => self.link(job, index, cancel).await?,
            _ => return Err("This PR phase needs an explicit review or recovery.".into()),
        }
        Ok(())
    }
    pub async fn run(&self, id: Uuid, mut cancel: watch::Receiver<bool>) -> Result<()> {
        loop {
            let job = self.read(id).await?;
            if !job.running() {
                return Ok(());
            }
            let index = job
                .members
                .iter()
                .position(|member| member.phase == Phase::Drafting)
                .or_else(|| {
                    job.members.iter().position(|member| {
                        matches!(
                            member.phase,
                            Phase::LookingUp | Phase::Ready | Phase::Checking
                        )
                    })
                })
                .or_else(|| {
                    job.members
                        .iter()
                        .position(|member| matches!(member.phase, Phase::Created | Phase::Linking))
                })
                .ok_or("Missing active PR step.")?;
            let work = self.step(&job, index, cancel.clone());
            let result = tokio::select! {biased; _=async{if !*cancel.borrow(){let _=cancel.changed().await;}}=>{self.change(id,Change::Hold).await?;return Ok(());},result=work=>result};
            if let Err(error) = result {
                let now = self.read(id).await?;
                let phase = now.members[index].phase;
                // Error strings from our boundaries contain no raw server data.
                let fallback = if phase == Phase::Checking {
                    "The interrupted PR result could not be verified. Check GitHub before creating again."
                } else if matches!(phase, Phase::Creating | Phase::Uncertain) {
                    "The PR creation result is unknown. Check existing PRs before creating again."
                } else {
                    "PR preparation or update stopped. Review authentication, destinations, privacy, branch and existing PR changes before retrying."
                };
                let reason = error
                    .downcast_ref::<Problem>()
                    .map(|problem| problem.0)
                    .unwrap_or(fallback);
                self.change(
                    id,
                    Change::Failed {
                        folder: job.members[index].folder_id.clone(),
                        uncertain: matches!(
                            phase,
                            Phase::Checking | Phase::Creating | Phase::Uncertain
                        ),
                        reason,
                    },
                )
                .await?;
                return Ok(());
            }
        }
    }
}
