//! A folder-selection issue is an explicit choice before project creation.
//! Root choices retain the order of the folders selected by the user.
use crate::Result;
use intercat_git::Repository;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Issue {
    Duplicate { roots: Vec<PathBuf> },
    RepositoryRoot { root: PathBuf, roots: Vec<PathBuf> },
    Ignored { path: PathBuf, root: PathBuf },
    Submodules { path: PathBuf },
}
impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Duplicate { .. } => "Same repository already added · Use the root / Cancel",
            Self::RepositoryRoot { .. } => {
                "This folder belongs to a repository · Use the root / Cancel"
            }
            Self::Ignored { .. } => {
                "Ignored by its repository · changes here can’t be committed · Cancel"
            }
            Self::Submodules { .. } => "Repositories with submodules aren’t supported yet · Cancel",
        })
    }
}
impl std::error::Error for Issue {}

pub async fn inspect(paths: Vec<PathBuf>) -> Result<Option<Issue>> {
    if paths.is_empty() || paths.len() > 8 || paths.iter().any(|p| !p.is_absolute()) {
        return Err("Choose one to eight project folders.".into());
    }
    tokio::task::spawn_blocking(move || -> Result<_> {
        let mut identities = BTreeSet::new();
        let mut roots = Vec::new();
        let mut duplicate = false;
        let mut subfolder = None;
        for path in paths {
            let location = Repository::project_location(&path)?;
            if location.ignored {
                return Ok(Some(Issue::Ignored {
                    path: location.selected,
                    root: location.root,
                }));
            }
            if location.submodules {
                return Ok(Some(Issue::Submodules {
                    path: location.root,
                }));
            }
            if location.selected != location.root {
                subfolder.get_or_insert_with(|| location.root.clone());
            }
            if identities.insert(location.common_dir) {
                roots.push(location.root);
            } else {
                duplicate = true;
            }
        }
        Ok(if duplicate {
            Some(Issue::Duplicate { roots })
        } else {
            subfolder.map(|root| Issue::RepositoryRoot { root, roots })
        })
    })
    .await?
}
