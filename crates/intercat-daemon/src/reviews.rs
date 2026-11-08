//! Versioned review targets and notes. A review never owns a writable worktree.
use crate::{Result, checkpoint::Checkpoint, preparation::Workspace};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Issue,
    Suggestion,
    Question,
    Nitpick,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftNote {
    pub kind: Kind,
    pub blocking: bool,
    pub clip_id: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Draft {
    pub summary: String,
    pub notes: Vec<DraftNote>,
    pub resolutions: Vec<Resolution>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Resolution {
    pub note_id: Uuid,
    pub explanation: String,
    /// None means resolved; otherwise this is the finding in the new saved tree.
    pub remaining: Option<DraftNote>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingFix {
    pub fix_id: Uuid,
    pub note: Note,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    pub id: String,
    pub ordinal: i64,
    pub paths: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub track_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub checkpoint: Checkpoint,
    pub workspace: Workspace,
    pub clips: Vec<Clip>,
    pub report_id: String,
    pub report_revision: i64,
    pub pending_fixes: Vec<PendingFix>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Queued,
    Running,
    Complete,
    Failed,
    Held,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: Uuid,
    pub generation: Uuid,
    pub revision: u64,
    pub state: State,
    pub target: Target,
    pub summary: Option<String>,
    pub detail: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    pub id: Uuid,
    pub review_id: Uuid,
    pub target_sha: String,
    pub checked_sha: String,
    pub outdated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<NoteFix>,
    #[serde(flatten)]
    pub draft: DraftNote,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteFix {
    pub state: crate::review_fixes::State,
    pub detail: Option<String>,
    pub input_id: Option<Uuid>,
    pub input_revision: Option<i64>,
}
pub fn identity(target: &Target) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!(
            "review/{}/{}/{}",
            target.session_id, target.turn_id, target.checkpoint.sha
        )
        .as_bytes(),
    )
}
pub fn validate(draft: &Draft, target: &Target) -> Result<()> {
    if draft.summary.trim().is_empty() || draft.summary.len() > 8000 || draft.notes.len() > 128 {
        return Err("The Reviewer returned an invalid summary or too many notes.".into());
    }
    let mut resolved = std::collections::HashSet::new();
    for resolution in &draft.resolutions {
        if !target
            .pending_fixes
            .iter()
            .any(|fix| fix.note.id == resolution.note_id)
            || !resolved.insert(resolution.note_id)
            || resolution.explanation.trim().is_empty()
            || resolution.explanation.len() > 8000
            || resolution
                .remaining
                .as_ref()
                .is_some_and(|note| !note.blocking)
        {
            return Err("The Reviewer returned an invalid fix resolution.".into());
        }
    }
    if resolved.len() != target.pending_fixes.len() {
        return Err("The Reviewer must resolve every prior finding explicitly.".into());
    }
    for note in draft.notes.iter().chain(
        draft
            .resolutions
            .iter()
            .filter_map(|r| r.remaining.as_ref()),
    ) {
        if !target.clips.iter().any(|clip|clip.id==note.clip_id && clip.paths.contains(&note.path))
            || note.text.trim().is_empty() || note.text.len()>8000
            || note.start_line==0 || note.end_line<note.start_line
            || note.path.is_empty() || note.path.chars().any(char::is_control)
            || !Path::new(&note.path).components().all(|part|matches!(part,Component::Normal(name) if name!=".git" && !intercat_git::secret_path(&name.to_string_lossy())))
        {return Err("The Reviewer returned an invalid note target.".into())}
    }
    Ok(())
}
pub enum Finished {
    Complete {
        draft: Draft,
        checked_sha: String,
        outdated: Vec<bool>,
    },
    Failed(String),
    Held,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Retry {
    pub op_id: Uuid,
    pub review_id: Uuid,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Stop {
    pub op_id: Uuid,
    pub review_id: Uuid,
    pub revision: u64,
}
