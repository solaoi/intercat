//! A GitHub repository is selected from the already reviewed Git destination.
use crate::Result;
use intercat_git::{RemoteAddress, Transport};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Repository {
    pub owner: String,
    pub name: String,
}
fn component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}
impl Repository {
    pub fn from_git(raw: &str) -> Result<Self> {
        let address = RemoteAddress::parse(raw)?;
        if !address
            .host
            .as_deref()
            .is_some_and(|host| host.eq_ignore_ascii_case("github.com"))
            || !matches!(address.transport, Transport::Https | Transport::Ssh)
            || address.explicit_port.is_some_and(|port| {
                port != if address.transport == Transport::Ssh {
                    22
                } else {
                    443
                }
            })
        {
            return Err("Open PR needs a GitHub HTTPS or SSH destination.".into());
        }
        let path = if address.display.contains("://") {
            reqwest::Url::parse(&address.display)
                .map_err(|_| "Invalid GitHub repository URL.")?
                .path()
                .to_owned()
        } else {
            address
                .display
                .split_once(':')
                .ok_or("Invalid GitHub repository URL.")?
                .1
                .to_owned()
        };
        let path = path.strip_prefix('/').unwrap_or(&path);
        let path = path.strip_suffix(".git").unwrap_or(path);
        let parts = path.split('/').collect::<Vec<_>>();
        if parts.len() != 2 || !component(parts[0]) || !component(parts[1]) {
            return Err("The GitHub destination must identify one owner and repository.".into());
        }
        Ok(Self {
            owner: parts[0].into(),
            name: parts[1].into(),
        })
    }
    pub fn validate(&self) -> Result<()> {
        if !component(&self.owner) || !component(&self.name) {
            return Err("Invalid GitHub repository.".into());
        }
        Ok(())
    }
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub repository: Repository,
    pub base: String,
    pub head: String,
    pub sha: String,
}
impl Target {
    pub fn validate(&self) -> Result<()> {
        self.repository.validate()?;
        for branch in [&self.base, &self.head] {
            intercat_git::validate_branch_reference(&format!("refs/heads/{branch}"))?;
        }
        self.sha.parse::<intercat_git::ObjectId>()?;
        if self.base == self.head {
            return Err("Choose a different base branch before opening PR.".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Draft {
    pub title: String,
    pub body: String,
}
impl Draft {
    pub fn validate(&self) -> Result<()> {
        if self.title.trim().is_empty()
            || self.title.chars().count() > 256
            || self.title.contains(['\0', '\r', '\n'])
            || self.body.len() > 64 * 1024
            || self.body.contains('\0')
        {
            return Err(
                "Review a PR title up to 256 characters and description up to 64 KiB.".into(),
            );
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_selection_accepts_only_an_exact_reviewed_github_path() -> Result<()> {
        let expected = Repository {
            owner: "own-dummy".into(),
            name: "dummy-repo".into(),
        };
        for url in [
            "git@github.com:own-dummy/dummy-repo.git",
            "ssh://git@github.com/own-dummy/dummy-repo.git",
            "https://dummy-user:dummy-password@github.com/own-dummy/dummy-repo.git?dummy=private",
        ] {
            assert_eq!(Repository::from_git(url)?, expected);
        }
        for url in [
            "https://github.com.attacker.invalid/own-dummy/repo",
            "https://github.com/own-dummy/repo/other",
            "https://github.com/own-dummy%2Frepo",
            "http://github.com/own-dummy/repo",
            "ssh://github.com:2222/own-dummy/repo",
            "ext::sh -c DUMMY",
        ] {
            assert!(Repository::from_git(url).is_err());
        }
        Ok(())
    }
}
