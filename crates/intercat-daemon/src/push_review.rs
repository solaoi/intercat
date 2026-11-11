//! Immutable confirmation input. A review never authorizes sending by itself.
use crate::{Result, project::Folder, push_target::Target};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Selection {
    pub folders: Vec<String>,
    pub through: std::collections::BTreeMap<String, String>,
}
impl Selection {
    pub fn validate(&self) -> Result<()> {
        let folders = self
            .folders
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        if self.folders.is_empty()
            || self.folders.len() > 8
            || folders.len() != self.folders.len()
            || folders != self.through.keys().collect()
        {
            return Err("Invalid saved publication selection.".into());
        }
        for (folder, sha) in &self.through {
            Uuid::parse_str(folder)?;
            sha.parse::<intercat_git::ObjectId>()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Choice {
    pub folder_id: String,
    pub remote: String,
    pub target_ref: Option<String>,
    pub through: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub choices: Vec<Choice>,
}
impl Request {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.op_id.get_version_num() != 7 || self.choices.is_empty() || self.choices.len() > 8 {
            return Err("Choose between one and eight folders to review for Push.".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for choice in &self.choices {
            if !ids.insert(&choice.folder_id) {
                return Err("A folder was selected more than once.".into());
            }
            crate::push_target::validate_remote(&choice.remote)?;
            if let Some(reference) = &choice.target_ref {
                intercat_git::validate_branch_reference(reference)?;
            }
            if let Some(sha) = &choice.through {
                sha.parse::<intercat_git::ObjectId>()?;
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    pub folder: Folder,
    pub slug: String,
    pub reference: String,
    pub head: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Context {
    pub project_id: Uuid,
    pub session_id: String,
    pub session_name: String,
    pub sources: Vec<Source>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Member {
    pub folder: Folder,
    pub slug: String,
    pub target: Target,
    pub lfs: Vec<crate::push_lfs::Binding>,
    pub ssh: Vec<crate::push_ssh_config::Binding>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Review {
    pub request: Request,
    pub project_id: Uuid,
    pub session_id: String,
    pub session_name: String,
    pub members: Vec<Member>,
}
impl Review {
    pub(crate) fn validate(&self) -> Result<()> {
        self.request.validate()?;
        if self.members.len() != self.request.choices.len() {
            return Err("The Push review has a different set of folders.".into());
        }
        for (member, choice) in self.members.iter().zip(&self.request.choices) {
            member.target.validate()?;
            let mut git = member.target.destinations.clone();
            let mut lfs_git = member
                .lfs
                .iter()
                .map(|binding| binding.git.clone())
                .collect::<Vec<_>>();
            git.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
            lfs_git.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
            if git != lfs_git {
                return Err("The Push review has a different set of LFS destinations.".into());
            }
            let mut expected = std::collections::BTreeMap::new();
            for destination in member
                .target
                .destinations
                .iter()
                .chain(member.lfs.iter().map(|binding| &binding.upload))
            {
                if intercat_git::RemoteAddress::parse(&destination.display)?.transport
                    == intercat_git::Transport::Ssh
                {
                    expected.insert(destination.fingerprint.clone(), destination);
                }
            }
            if member.ssh.len() != expected.len() {
                return Err("The Push review has a different set of SSH destinations.".into());
            }
            for binding in &member.ssh {
                if expected.remove(&binding.destination.fingerprint) != Some(&binding.destination)
                    || binding.host.is_empty()
                    || binding.host.chars().any(char::is_control)
                    || binding.port == 0
                    || binding.user_fingerprint.len() != 64
                    || !binding
                        .user_fingerprint
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err("The SSH review does not match its Push destinations.".into());
                }
            }
            if member.folder.id != choice.folder_id
                || member.target.remote != choice.remote
                || member.target.source_ref != format!("refs/heads/intercat/{}", member.slug)
                || choice
                    .target_ref
                    .as_ref()
                    .is_some_and(|reference| reference != &member.target.target_ref)
                || choice
                    .through
                    .as_ref()
                    .is_some_and(|sha| sha != &member.target.sha)
            {
                return Err("The Push review belongs to another folder or branch.".into());
            }
        }
        Ok(())
    }
    pub(crate) fn sync_objects(&self) -> Result<()> {
        self.validate()?;
        for member in &self.members {
            let repo = repository(member)?;
            repo.sync_objects(&[member.target.sha.parse()?])?;
        }
        Ok(())
    }
    pub(crate) fn pin(&self) -> Result<()> {
        self.sync_objects()?;
        for member in &self.members {
            let repo = repository(member)?;
            let reference = format!(
                "refs/intercat/{}/push-reviews/{}",
                member.slug, self.request.op_id
            );
            repo.compare_and_swap(&reference, None, member.target.sha.parse()?)?;
            repo.sync_ref(&reference)?;
        }
        Ok(())
    }
}
pub(crate) fn repository(member: &Member) -> Result<intercat_git::Repository> {
    let repo = intercat_git::Repository::open(&member.folder.root)?;
    if repo.common_directory()? != member.folder.common_dir
        || repo.working_directory()? != member.folder.root
    {
        return Err("The Push source now belongs to another repository.".into());
    }
    Ok(repo)
}
