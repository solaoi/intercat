//! RPC-facing Push presentation. Reading a review never starts a sender.
use crate::{
    Result,
    journal::Journal,
    push_review::{Request, Review},
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};
use tokio::sync::{OnceCell, watch};
use uuid::Uuid;
pub(crate) struct Context {
    journal: Journal,
    root: PathBuf,
    services: OnceCell<Services>,
}
struct Services {
    inspector: Arc<crate::push_inspect::Inspector>,
    reviews: crate::push_review_service::Service,
    sender: crate::push_service::Service,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Presentation {
    pub review: Review,
    pub commits: BTreeMap<String, Vec<String>>,
    pub additional_commits: BTreeMap<String, usize>,
}
impl Presentation {
    fn read(review: Review) -> Result<Self> {
        let mut commits = BTreeMap::new();
        let mut additional_commits = BTreeMap::new();
        for member in &review.members {
            let repo = crate::push_review::repository(member)?;
            commits.insert(
                member.folder.id.clone(),
                repo.push_commits(
                    member.target.sha.parse()?,
                    &[member.folder.base.parse()?],
                    &AtomicBool::new(false),
                )?
                .iter()
                .map(ToString::to_string)
                .collect(),
            );
            let later = if let Some(head) = repo.reference_target(&member.target.source_ref)? {
                repo.push_commits(head, &[member.target.sha.parse()?], &AtomicBool::new(false))?
                    .len()
            } else {
                0
            };
            additional_commits.insert(member.folder.id.clone(), later);
        }
        Ok(Self {
            review,
            commits,
            additional_commits,
        })
    }
}
impl Context {
    pub fn new(journal: Journal, root: PathBuf) -> Self {
        Self {
            journal,
            root,
            services: OnceCell::new(),
        }
    }
    async fn services(&self) -> Result<&Services> {
        self.services.get_or_try_init(||async {
            let git=crate::git_tool::GitTool::system().await?;
            let home=PathBuf::from(std::env::var_os("HOME").ok_or("Git needs a home directory.")?);
            let xdg=std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(||home.join(".config"));
            let inspector=Arc::new(crate::push_inspect::Inspector::new(git.clone(),home,xdg,true)?);
            let executable=std::env::var_os("INTERCAT_GIT_LFS").ok_or("Intercat's pinned Git LFS executable is unavailable. Restart with the installed application tools.")?;
            let lfs=Arc::new(crate::push_lfs::Tool::open(git.clone(),executable.into())?);
            let reviews=crate::push_review_service::Service::new(self.journal.clone(),inspector.clone(),lfs.clone());
            let sender=crate::push_service::Service::new(self.journal.clone(),crate::push_service::Configuration{git,inspector:inspector.clone(),lfs,root:self.root.clone(),path:std::env::var_os("PATH").ok_or("Git needs a tool path.")?,agent:std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from)}).await?;
            Ok(Services{inspector,reviews,sender})
        }).await
    }
    pub async fn remotes(&self, folder: String) -> Result<Vec<String>> {
        let context = self
            .journal
            .push_context(Request {
                op_id: Uuid::now_v7(),
                choices: vec![crate::push_review::Choice {
                    folder_id: folder,
                    remote: "origin".into(),
                    target_ref: None,
                    through: None,
                }],
            })
            .await?;
        self.services()
            .await?
            .inspector
            .remotes(&context.sources[0].folder, watch::channel(false).1)
            .await
    }
    pub async fn review(&self, request: Request) -> Result<Presentation> {
        let review = self
            .services()
            .await?
            .reviews
            .open(request, watch::channel(false).1)
            .await?;
        tokio::task::spawn_blocking(move || Presentation::read(review)).await?
    }
    pub async fn command(&self, request: crate::push_job::Command) -> Result<crate::push_job::Job> {
        self.services().await?.sender.command(request).await
    }
    pub async fn presentation(&self, id: Uuid) -> Result<Presentation> {
        let review = self
            .journal
            .push_review(id)
            .await?
            .ok_or("Missing Push review.")?;
        tokio::task::spawn_blocking(move || Presentation::read(review)).await?
    }
    pub async fn shutdown(&self) {
        if let Some(services) = self.services.get() {
            services.sender.shutdown().await;
        }
    }
}
