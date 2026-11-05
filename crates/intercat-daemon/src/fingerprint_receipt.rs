//! Non-secret witnesses retained outside SQLite to locate lost fingerprints.
use crate::{Result, secrets::Seal};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Session {
    pub session_id: String,
    pub track_id: String,
    pub digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Project {
    pub project_id: String,
    pub digest: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Proof {
    pub sessions: Vec<Session>,
    pub projects: Vec<Project>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Receipt {
    pub catalog: Seal,
    pub proof: Proof,
}
impl Receipt {
    pub fn validate(&self) -> Result<()> {
        if serde_json::to_vec(self)?.len() > 480 * 1024 {
            return Err("Fingerprint receipt capacity exceeded.".into());
        }
        self.catalog.validate()?;
        let mut previous = "";
        for session in &self.proof.sessions {
            if session.session_id.as_str() <= previous
                || session.session_id.len() > 128
                || session.track_id.is_empty()
                || session.track_id.len() > 128
            {
                return Err("Invalid fingerprint session witness.".into());
            }
            crate::secrets::decode(&session.digest)?;
            previous = &session.session_id;
        }
        previous = "";
        for project in &self.proof.projects {
            if project.project_id.as_str() <= previous {
                return Err("Invalid fingerprint project witness.".into());
            }
            crate::secrets::decode(&project.project_id)?;
            crate::secrets::decode(&project.digest)?;
            previous = &project.project_id;
        }
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Difference<'a> {
    pub sessions: Vec<SessionDifference<'a>>,
    pub projects: Vec<ProjectDifference<'a>>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDifference<'a> {
    pub session_id: &'a str,
    pub track_id: &'a str,
    pub reason: &'static str,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDifference<'a> {
    pub project_id: &'a str,
    pub reason: &'static str,
}
impl Proof {
    pub fn difference<'a>(&'a self, saved: &'a Self) -> Difference<'a> {
        let mut sessions = BTreeMap::new();
        for session in &saved.sessions {
            sessions.insert(&session.session_id, (Some(session), None));
        }
        for session in &self.sessions {
            sessions.entry(&session.session_id).or_default().1 = Some(session);
        }
        let sessions = sessions
            .into_values()
            .filter_map(|pair| match pair {
                (Some(expected), Some(actual)) if expected != actual => Some(SessionDifference {
                    session_id: &expected.session_id,
                    track_id: &expected.track_id,
                    reason: "changed",
                }),
                (Some(expected), None) => Some(SessionDifference {
                    session_id: &expected.session_id,
                    track_id: &expected.track_id,
                    reason: "missing",
                }),
                (None, Some(actual)) => Some(SessionDifference {
                    session_id: &actual.session_id,
                    track_id: &actual.track_id,
                    reason: "unexpected",
                }),
                _ => None,
            })
            .collect();
        let mut projects = BTreeMap::new();
        for project in &saved.projects {
            projects.insert(&project.project_id, (Some(project), None));
        }
        for project in &self.projects {
            projects.entry(&project.project_id).or_default().1 = Some(project);
        }
        let projects = projects
            .into_values()
            .filter_map(|pair| match pair {
                (Some(expected), Some(actual)) if expected != actual => Some(ProjectDifference {
                    project_id: &expected.project_id,
                    reason: "changed",
                }),
                (Some(expected), None) => Some(ProjectDifference {
                    project_id: &expected.project_id,
                    reason: "missing",
                }),
                (None, Some(actual)) => Some(ProjectDifference {
                    project_id: &actual.project_id,
                    reason: "unexpected",
                }),
                _ => None,
            })
            .collect();
        Difference { sessions, projects }
    }
}
