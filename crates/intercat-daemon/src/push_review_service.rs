//! Inspection and object pinning surround the journal's confirmation record.
use crate::{
    Result,
    journal::Journal,
    push_inspect::Inspector,
    push_review::{Member, Request, Review},
};
use std::sync::Arc;
use tokio::sync::watch;

pub struct Service {
    journal: Journal,
    inspector: Arc<Inspector>,
    lfs: Arc<crate::push_lfs::Tool>,
}
impl Service {
    pub fn new(
        journal: Journal,
        inspector: Arc<Inspector>,
        lfs: Arc<crate::push_lfs::Tool>,
    ) -> Self {
        Self {
            journal,
            inspector,
            lfs,
        }
    }
    pub async fn open(&self, request: Request, cancel: watch::Receiver<bool>) -> Result<Review> {
        let review = if let Some(saved) = self.journal.push_review(request.op_id).await? {
            if saved.request != request {
                return Err("The Push review operation ID was reused.".into());
            }
            saved
        } else {
            let context = self.journal.push_context(request.clone()).await?;
            let mut members = vec![];
            for (source, choice) in context.sources.iter().zip(&request.choices) {
                let sha = choice.through.as_ref().unwrap_or(&source.head).parse()?;
                let target = self
                    .inspector
                    .freeze(
                        &source.folder,
                        &source.reference,
                        &choice.remote,
                        choice.target_ref.as_ref().unwrap_or(&source.reference),
                        Some(sha),
                        cancel.clone(),
                    )
                    .await?;
                let urls = self
                    .inspector
                    .destinations(&source.folder, &choice.remote, cancel.clone())
                    .await?;
                let endpoints = self
                    .lfs
                    .resolve(
                        &self.inspector,
                        &source.folder,
                        &target,
                        &urls,
                        cancel.clone(),
                    )
                    .await?;
                let lfs = endpoints
                    .iter()
                    .map(crate::push_lfs::Binding::capture)
                    .collect::<Result<Vec<_>>>()?;
                let mut ssh = std::collections::BTreeMap::new();
                for url in urls
                    .iter()
                    .chain(endpoints.iter().map(|endpoint| &endpoint.upload_url))
                {
                    if intercat_git::RemoteAddress::parse(url)?.transport
                        == intercat_git::Transport::Ssh
                    {
                        let configuration = crate::push_ssh_config::Configuration::inspect(
                            &self.inspector,
                            &source.folder,
                            url,
                            cancel.clone(),
                        )
                        .await?;
                        let binding = configuration.binding(url)?;
                        ssh.insert(binding.destination.fingerprint.clone(), binding);
                    }
                }
                members.push(Member {
                    folder: source.folder.clone(),
                    slug: source.slug.clone(),
                    target,
                    lfs,
                    ssh: ssh.into_values().collect(),
                });
            }
            let review = Review {
                request,
                project_id: context.project_id,
                session_id: context.session_id,
                session_name: context.session_name,
                members,
            };
            let objects = review.clone();
            tokio::task::spawn_blocking(move || objects.sync_objects()).await??;
            self.journal.begin_push_review(review).await?
        };
        let pins = review.clone();
        tokio::task::spawn_blocking(move || pins.pin()).await??;
        self.journal
            .complete_push_review(review.request.op_id)
            .await?;
        Ok(review)
    }
}
