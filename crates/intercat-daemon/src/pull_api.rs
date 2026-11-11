//! Authentication is initialized only when a person opens the PR workflow.
use crate::{
    Result,
    editor::Manager,
    journal::Journal,
    pull_request::{Command, Open},
};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::OnceCell;
use uuid::Uuid;
pub(crate) struct Context {
    journal: Journal,
    editor: Manager,
    service: OnceCell<crate::pull_service::Service>,
}
impl Context {
    pub fn new(journal: Journal, editor: Manager) -> Self {
        Self {
            journal,
            editor,
            service: OnceCell::new(),
        }
    }
    async fn service(&self) -> Result<&crate::pull_service::Service> {
        self.service
            .get_or_try_init(|| async {
                let git = crate::git_tool::GitTool::system().await?;
                let home = PathBuf::from(
                    std::env::var_os("HOME").ok_or("GitHub needs a home directory.")?,
                );
                let xdg = std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".config"));
                let inspector =
                    Arc::new(crate::push_inspect::Inspector::new(git, home, xdg, true)?);
                crate::pull_service::Service::new(
                    self.journal.clone(),
                    crate::pull_service::Configuration {
                        editor: self.editor.clone(),
                        client: Arc::new(crate::github_pull::Client::github()?),
                        credentials: Arc::new(crate::github_auth::Credentials::system()?),
                        inspector,
                    },
                )
                .await
            })
            .await
    }
    pub async fn open(&self, request: Open) -> Result<serde_json::Value> {
        self.service().await?.open(request).await?.project()
    }
    pub async fn command(&self, request: Command) -> Result<serde_json::Value> {
        self.service().await?.command(request).await?.project()
    }
    pub async fn read(&self, id: Uuid) -> Result<serde_json::Value> {
        self.journal.pull_job(id).await?.project()
    }
    pub async fn shutdown(&self) {
        if let Some(service) = self.service.get() {
            service.shutdown().await;
        }
    }
}
