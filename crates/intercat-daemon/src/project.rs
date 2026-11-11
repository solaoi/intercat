//! A clean repository and its durably pinned starting point.
use crate::Result;
use intercat_git::{ObjectId, Repository, WorktreeSource};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub base: String,
    pub base_branch: Option<String>,
}

impl Folder {
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

async fn inspect_folder(path: PathBuf) -> Result<Folder> {
    let source = crate::source_snapshot::clean_project(&path).await?;
    tokio::task::spawn_blocking(move || {
        let base_branch = Repository::open(&source.root)?.base_branch()?;
        Ok(Folder {
            id: Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                source.common_dir.as_os_str().as_encoded_bytes(),
            )
            .to_string(),
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

impl Folder {
    pub fn id(&self) -> String {
        self.id.clone()
    }
}

/// A named collection of repositories. Repository state belongs to each folder.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub folders: Vec<Folder>,
}
impl Project {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() || self.folders.is_empty() || self.folders.len() > 8 {
            return Err("A project needs a name and one to eight folders.".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for folder in &self.folders {
            let expected = Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                folder.common_dir.as_os_str().as_encoded_bytes(),
            )
            .to_string();
            if folder.id != expected
                || !folder.path.is_absolute()
                || !folder.root.is_absolute()
                || !folder.common_dir.is_absolute()
            {
                return Err("The folder identity does not match its repository.".into());
            }
            if !ids.insert(folder.common_dir.clone()) {
                return Err("Same repository already added · Use the root / Cancel".into());
            }
        }
        Ok(())
    }
    pub fn folder(&self, id: &str) -> Result<&Folder> {
        self.folders
            .iter()
            .find(|folder| folder.id() == id)
            .ok_or_else(|| "The folder is no longer part of this project.".into())
    }
    pub fn only_folder(&self) -> Result<&Folder> {
        match self.folders.as_slice() {
            [folder] => Ok(folder),
            [] => Err("Choose a project folder.".into()),
            _ => Err("Choose which project folder to use.".into()),
        }
    }
    pub async fn pin(&self) -> Result<()> {
        self.validate()?;
        for folder in &self.folders {
            folder.pin().await?;
        }
        Ok(())
    }
}

pub async fn inspect(id: Uuid, name: String, paths: Vec<PathBuf>) -> Result<Project> {
    if paths.len() > 8 {
        return Err("A project can contain at most eight folders.".into());
    }
    if let Some(issue) = crate::project_selection::inspect(paths.clone()).await? {
        return Err(issue.into());
    }
    let mut project = Project {
        id,
        name,
        folders: Vec::new(),
    };
    for path in paths {
        project.folders.push(inspect_folder(path).await?);
    }
    project.validate()?;
    Ok(project)
}
