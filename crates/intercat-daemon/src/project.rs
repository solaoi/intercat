//! A clean repository and its durably pinned starting point.
use crate::Result;
use intercat_git::{ObjectId, Repository, WorktreeSource};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub base: String,
    pub base_branch: Option<String>,
}

impl Project {
    pub async fn pin(&self) -> Result<()> {
        if self.base_branch.is_none() {
            let project = self.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                let repo = Repository::open(&project.root)?;
                let expected: ObjectId = project.base.parse()?;
                if repo.commit_details(expected).is_err()
                    && repo.empty_starting_point()? != expected
                {
                    return Err("The saved starting point is missing.".into());
                }
                Ok(())
            })
            .await??;
        }
        let source = self.source().await?;
        tokio::task::spawn_blocking(move || {
            let repo = Repository::open(source.root)?;
            repo.sync_objects(&[source.head])?;
            let reference = format!("refs/intercat/starting-points/{}", source.head);
            repo.compare_and_swap(&reference, None, source.head)?;
            repo.sync_ref(&reference)
        })
        .await?
    }
    pub async fn source(&self) -> Result<WorktreeSource> {
        let project = self.clone();
        tokio::task::spawn_blocking(move || {
            let base = ObjectId::from_hex(project.base.as_bytes())?;
            let source = Repository::worktree_source(&project.root, Some(base))?;
            if source.root != project.root || source.common_dir != project.common_dir {
                return Err(std::io::Error::other(
                    "The configured project has moved or changed repository.",
                )
                .into());
            }
            Ok(source)
        })
        .await?
    }
}

pub async fn inspect(path: PathBuf) -> Result<Project> {
    tokio::task::spawn_blocking(move || {
        let source = Repository::clean_project(&path)?;
        let base_branch = Repository::open(&source.root)?.base_branch()?;
        Ok(Project {
            path,
            name: source
                .root
                .file_name()
                .ok_or_else(|| std::io::Error::other("Choose a project folder."))?
                .to_string_lossy()
                .into_owned(),
            root: source.root,
            common_dir: source.common_dir,
            base: source.head.to_string(),
            base_branch,
        })
    })
    .await?
}
