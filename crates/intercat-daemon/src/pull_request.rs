//! Reviewed PR text, repository and branch. Opening a job cannot create a PR.
use crate::{
    Result,
    github_repository::{Draft, Target},
    push_review::Review,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Pull {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub body: String,
    pub head_sha: String,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Choice {
    pub folder_id: String,
    pub destination: String,
    pub base: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Open {
    pub op_id: Uuid,
    pub push_review: Uuid,
    pub choices: Vec<Choice>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Edit {
    pub folder_id: String,
    pub draft: Draft,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Action {
    Edit {
        id: Uuid,
        revision: u64,
        drafts: Vec<Edit>,
    },
    Confirm {
        id: Uuid,
        revision: u64,
    },
    Retry {
        id: Uuid,
        revision: u64,
    },
    ReviewAgain {
        id: Uuid,
        revision: u64,
    },
    Discard {
        id: Uuid,
        revision: u64,
    },
    Stop {
        id: Uuid,
        revision: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub op_id: Uuid,
    pub action: Action,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Drafting,
    LookingUp,
    Review,
    Ready,
    Creating,
    Uncertain,
    Checking,
    Created,
    Linking,
    Done,
    Failed,
    Discarded,
}
impl Phase {
    pub fn running(self) -> bool {
        matches!(
            self,
            Self::Drafting
                | Self::LookingUp
                | Self::Ready
                | Self::Creating
                | Self::Checking
                | Self::Created
                | Self::Linking
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Account {
    pub id: u64,
    pub login: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Sources {
    pub requests: Vec<String>,
    pub commits: Vec<String>,
    pub assumptions: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Member {
    pub folder_id: String,
    pub name: String,
    pub destination: String,
    pub target: Target,
    pub sources: Sources,
    pub draft: Option<Draft>,
    pub existing: Option<Pull>,
    pub result: Option<Pull>,
    pub phase: Phase,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Job {
    pub id: Uuid,
    pub revision: u64,
    pub request: Open,
    pub push: Review,
    pub account: Option<Account>,
    pub confirmed: Option<Uuid>,
    pub held: bool,
    pub members: Vec<Member>,
}
impl Open {
    pub fn validate(&self) -> Result<()> {
        if self.op_id.get_version_num() != 7
            || self.choices.is_empty()
            || self.choices.len() > 8
            || self.push_review.get_version_num() != 7
        {
            return Err("Choose one to eight pushed folders for PR review.".into());
        }
        let mut folders = std::collections::BTreeSet::new();
        for choice in &self.choices {
            if !folders.insert(&choice.folder_id)
                || choice.destination.len() != 64
                || !choice
                    .destination
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err("Invalid PR folder or Git destination.".into());
            }
            intercat_git::validate_branch_reference(&format!("refs/heads/{}", choice.base))?;
        }
        Ok(())
    }
}
impl Job {
    pub fn validate(&self) -> Result<()> {
        self.request.validate()?;
        self.push.validate()?;
        if self.id != self.request.op_id
            || self.push.request.op_id != self.request.push_review
            || self.members.len() != self.request.choices.len()
        {
            return Err("The PR job has a different review or set of folders.".into());
        }
        if self.confirmed.is_some_and(|id| id.get_version_num() != 7) {
            return Err("Invalid PR confirmation.".into());
        }
        if self.account.as_ref().is_some_and(|account| {
            account.id == 0
                || account.login.is_empty()
                || account.login.len() > 256
                || account.login.chars().any(char::is_control)
        }) {
            return Err("Invalid PR account.".into());
        }
        for (member, choice) in self.members.iter().zip(&self.request.choices) {
            member.target.validate()?;
            if member.folder_id != choice.folder_id
                || member.destination != choice.destination
                || member.target.base != choice.base
            {
                return Err("The PR member does not match its reviewed choice.".into());
            }
            let source = self
                .push
                .members
                .iter()
                .find(|source| source.folder.id == member.folder_id)
                .ok_or("Missing reviewed PR folder.")?;
            let destination = source
                .target
                .destinations
                .iter()
                .find(|destination| destination.fingerprint == member.destination)
                .ok_or("Missing reviewed PR destination.")?;
            if member.target.repository
                != crate::github_repository::Repository::from_git(&destination.display)?
                || member.target.sha != source.target.sha
                || format!("refs/heads/{}", member.target.head) != source.target.target_ref
            {
                return Err("The PR target differs from the pushed commit.".into());
            }
            if matches!(
                member.phase,
                Phase::Review
                    | Phase::Ready
                    | Phase::Creating
                    | Phase::Uncertain
                    | Phase::Checking
                    | Phase::Created
                    | Phase::Linking
                    | Phase::Done
            ) && (member.draft.is_none() || self.account.is_none())
            {
                return Err("PR text and account must be reviewed before continuing.".into());
            }
            if matches!(
                member.phase,
                Phase::Ready
                    | Phase::Creating
                    | Phase::Uncertain
                    | Phase::Checking
                    | Phase::Created
                    | Phase::Linking
                    | Phase::Done
            ) && self.confirmed.is_none()
            {
                return Err("PR creation needs an explicit saved confirmation.".into());
            }
            if let Some(draft) = &member.draft {
                draft.validate()?;
            }
            if member.phase == Phase::Done && member.result.is_none() {
                return Err("The PR result has not been verified.".into());
            }
        }
        Ok(())
    }
    pub fn running(&self) -> bool {
        !self.held && self.members.iter().any(|member| member.phase.running())
    }
    pub fn done(&self) -> bool {
        self.members
            .iter()
            .all(|member| member.phase == Phase::Done)
    }
    pub(crate) fn project(&self) -> Result<serde_json::Value> {
        let mut value = serde_json::to_value(self)?;
        for member in value["members"]
            .as_array_mut()
            .ok_or("Missing PR members.")?
        {
            let object = member.as_object_mut().ok_or("Invalid PR member.")?;
            object.remove("sources");
            // Raw remote text is retained only for concurrent edit detection.
            for key in ["existing", "result"] {
                if let Some(pull) = object
                    .get_mut(key)
                    .and_then(serde_json::Value::as_object_mut)
                {
                    pull.remove("title");
                    pull.remove("body");
                }
            }
        }
        Ok(value)
    }
}
