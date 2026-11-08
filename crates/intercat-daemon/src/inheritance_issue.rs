//! Durable reasons an inherited update is waiting for the user.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum Blocker {
    Processes,
    Conflict { paths: Vec<String>, base: bool },
    Other { message: String },
}
impl std::fmt::Display for Blocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Processes => f.write_str(
                "A worker process is still running. Stop it before applying inherited files.",
            ),
            Self::Conflict { paths, base } => write!(
                f,
                "{}: {}",
                if *base {
                    "Inherited base conflicts"
                } else {
                    "Inherited files conflict"
                },
                paths.join(", ")
            ),
            Self::Other { message } => f.write_str(message),
        }
    }
}
impl std::error::Error for Blocker {}
impl Blocker {
    pub(crate) fn from_error(error: &(dyn std::error::Error + 'static)) -> Self {
        error
            .downcast_ref::<Self>()
            .cloned()
            .unwrap_or_else(|| Self::Other {
                message: error.to_string(),
            })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Issue {
    pub id: Uuid,
    pub input: String,
    pub track_id: String,
    pub session_id: Option<String>,
    pub revision: i64,
    pub parents: Vec<String>,
    pub application_id: Option<Uuid>,
    pub blocker: Blocker,
    pub kept: bool,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Keep,
    Stop,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub op_id: Uuid,
    pub issue_id: Uuid,
    pub action: Action,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS inheritance_issues(input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,record TEXT NOT NULL);")?;
    Ok(())
}
pub(crate) fn record(
    db: &Connection,
    input: &str,
    track: &str,
    application_id: Option<Uuid>,
    blocker: Blocker,
) -> Result<()> {
    let revision = db
        .query_row(
            "SELECT revision FROM utterances WHERE id=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    let Some(revision) = revision else {
        return Ok(());
    };
    let session_id = db
        .query_row(
            "SELECT json_extract(record,'$.sessionId') FROM workers WHERE track_id=?1",
            [track],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM input_inheritance WHERE input=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    let parents = raw
        .map(|raw| serde_json::from_str::<crate::inheritance::Dependencies>(&raw))
        .transpose()?
        .map(|source| source.points.into_iter().map(|point| point.track).collect())
        .unwrap_or_default();
    let issue = Issue {
        id: Uuid::now_v7(),
        input: input.into(),
        track_id: track.into(),
        session_id,
        revision,
        parents,
        application_id,
        blocker,
        kept: false,
    };
    db.execute("INSERT INTO inheritance_issues VALUES(?1,?2) ON CONFLICT(input) DO UPDATE SET record=excluded.record",params![input,serde_json::to_string(&issue)?])?;
    Ok(())
}
pub(crate) fn clear(db: &Connection, input: &str) -> Result<()> {
    db.execute("DELETE FROM inheritance_issues WHERE input=?1", [input])?;
    Ok(())
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Issue> {
    let raw: String = db.query_row(
        "SELECT record FROM inheritance_issues WHERE json_extract(record,'$.id')=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    let issue: Issue = serde_json::from_str(&raw)?;
    if !current(db, &issue)? {
        return Err("This request changed. Review its current state.".into());
    }
    Ok(issue)
}
fn current(db: &Connection, issue: &Issue) -> Result<bool> {
    if let Some(application) = issue.application_id {
        return Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND kind='workspace.apply-inheritance' AND state<>'complete')",[application.to_string()],|row|row.get(0))?);
    }
    let row: Option<(String, i64, Option<String>)> = db
        .query_row(
            "SELECT state,revision,json_extract(decision,'$.trackId') FROM utterances WHERE id=?1",
            [&issue.input],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    Ok(row.is_some_and(|(state, revision, track)| {
        state == "held" && revision == issue.revision && track.as_deref() == Some(&issue.track_id)
    }))
}
pub(crate) fn snapshot(db: &Connection) -> Result<Vec<serde_json::Value>> {
    let mut query = db.prepare("SELECT record FROM inheritance_issues ORDER BY rowid")?;
    let records = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut result = Vec::new();
    for raw in records {
        let issue: Issue = serde_json::from_str(&raw)?;
        if !current(db, &issue)? {
            continue;
        }
        let mut value = serde_json::to_value(&issue)?;
        if let Some((plan, item)) = crate::plan_execution::owner(db, &issue.input)? {
            value["planId"] = plan.to_string().into();
            value["itemId"] = item.to_string().into();
        }
        result.push(value);
    }
    Ok(result)
}
pub(crate) fn command(db: &mut Connection, command: Command, stopped: bool) -> Result<()> {
    let tx = db.transaction()?;
    let request = serde_json::to_value(&command)?;
    if let Some((kind, old)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [command.op_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        let old: serde_json::Value = serde_json::from_str(&old)?;
        if kind != "inheritance.issue" || old["command"] != request {
            return Err("Operation ID reused.".into());
        }
        return Ok(());
    }
    let mut issue = read(&tx, command.issue_id)?;
    let payload = serde_json::json!({"command":command,"issue":issue}).to_string();
    match command.action {
        Action::Keep => {
            issue.kept = true;
            tx.execute(
                "UPDATE inheritance_issues SET record=?2 WHERE input=?1",
                params![issue.input, serde_json::to_string(&issue)?],
            )?;
        }
        Action::Stop => {
            if !stopped || !matches!(issue.blocker, Blocker::Processes) {
                return Err("Confirm the worker processes have stopped first.".into());
            }
            clear(&tx, &issue.input)?;
        }
    }
    tx.execute(
        "INSERT INTO operations VALUES(?1,'inheritance.issue',?2,'complete')",
        params![command.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'inheritance.issue-resolved',?2)",
        params![crate::journal::timestamp(), payload],
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        decision::{Decision, Kind, Outcome, Target},
        inheritance_apply_store::Change,
        journal::Journal,
        secrets::Key,
    };
    #[tokio::test]
    async fn issues_survive_restart_keep_the_input_held_and_reject_stale_actions() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let input = Uuid::now_v7();
        journal
            .stage_utterance(
                input,
                "Dummy conflicting task".into(),
                Target::New,
                None,
                vec![],
            )
            .await?;
        let claimed = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                claimed.id,
                claimed.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Fork,
                    track_id: None,
                    boundary: None,
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: serde_json::json!({}),
                }),
            )
            .await?;
        let word = journal.read_utterance(input.to_string()).await?;
        journal
            .change_inheritance(Change::Failure {
                input: input.to_string(),
                revision: word["revision"].as_i64().unwrap(),
                blocker: Blocker::Conflict {
                    paths: vec!["dummy.txt".into()],
                    base: false,
                },
            })
            .await?;
        let issue: Issue =
            serde_json::from_value(journal.snapshot().await?["inheritanceIssues"][0].clone())?;
        let keep = Command {
            op_id: Uuid::now_v7(),
            issue_id: issue.id,
            action: Action::Keep,
        };
        journal
            .inheritance_issue_command(keep.clone(), false)
            .await?;
        journal
            .inheritance_issue_command(keep.clone(), false)
            .await?;
        drop(journal);
        journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        assert!(journal.inheritance_issue(issue.id).await?.kept);
        journal.advance_plans().await?;
        assert_eq!(
            journal.read_utterance(input.to_string()).await?["state"],
            "held"
        );
        assert!(
            journal
                .inheritance_issue_command(
                    Command {
                        op_id: Uuid::now_v7(),
                        issue_id: issue.id,
                        action: Action::Stop
                    },
                    false
                )
                .await
                .is_err()
        );
        let current = journal.read_utterance(input.to_string()).await?;
        journal
            .start_held_input(
                Uuid::now_v7(),
                input.to_string(),
                current["revision"].as_i64().unwrap(),
            )
            .await?;
        assert!(
            journal.snapshot().await?["inheritanceIssues"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            journal
                .inheritance_issue_command(
                    Command {
                        op_id: Uuid::now_v7(),
                        ..keep
                    },
                    false
                )
                .await
                .is_err()
        );
        Ok(())
    }
}
