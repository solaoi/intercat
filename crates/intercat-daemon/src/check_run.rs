//! Execute a Check against a fixed tree in a private, fresh checkout generation.
//! Commit and path comparison share this boundary; worker worktrees are never
//! Check locations. Outcome records bind the tree and captured settings hash.
use crate::{Result, checkout::Checkout, command::Output};
use intercat_git::{ObjectId, Repository};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    content = "command",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum CheckCommand {
    #[default]
    Auto,
    Disabled,
    Run(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub tree: String,
    pub settings_hash: String,
    pub command: Option<String>,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}
pub(crate) fn resolve_command(
    repo: &Repository,
    tree: ObjectId,
    choice: &CheckCommand,
) -> Result<Option<String>> {
    match choice {
        CheckCommand::Disabled => Ok(None),
        CheckCommand::Run(command) => Ok(Some(command.clone())),
        CheckCommand::Auto => {
            let files = repo.tree_files(tree)?;
            if let Some(file) = files.get("package.json") {
                let manifest: serde_json::Value = serde_json::from_slice(&repo.saved_bytes(file)?)?;
                if manifest["scripts"]["test"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
                {
                    return Ok(Some("npm test".into()));
                }
            }
            Ok(None)
        }
    }
}

fn output_record(tree: &str, hash: &str, command: Option<String>, output: Output) -> Record {
    Record {
        tree: tree.into(),
        settings_hash: hash.into(),
        command,
        code: output.code,
        stdout: output.stdout_text(),
        stderr: output.stderr_text(),
        truncated: output.truncated,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Passed,
    EnvFailed,
    DirtyBeforeCheck,
    Cancelled,
    HistoryRequired,
    Failed,
    Mutated,
}

pub struct Snapshot<'a> {
    pub head: ObjectId,
    pub tree: ObjectId,
    pub settings_hash: &'a str,
}
pub struct Settings<'a> {
    pub command: &'a CheckCommand,
    pub history: bool,
    pub npm: Option<crate::dependencies::NpmTool>,
    pub registry_origins: &'a [String],
    pub timeout_ms: u64,
}
#[derive(Clone, Copy, Debug)]
pub enum Deployment {
    /// Keep hook output in a prepared candidate; mismatches must be reported.
    KeepPrepared,
    /// Materialize the selected tree in the new generation before Check.
    RestoreSnapshot,
}
/// Execution output is unredacted. The owner screens it before saving or
/// exposing it, just as it screens hook and dependency output.
pub struct Report {
    pub state: State,
    pub workspace: PathBuf,
    pub check: Option<Record>,
    pub dependency_runs: Vec<crate::dependencies::Run>,
    pub dependency_error: Option<crate::dependencies::Failure>,
    pub reason: Option<String>,
}
impl Report {
    fn fail(mut self, state: State, reason: impl Into<String>) -> Self {
        self.state = state;
        self.reason = Some(reason.into());
        self
    }
}

pub async fn execute(
    checkout: Checkout,
    snapshot: Snapshot<'_>,
    cache_root: &Path,
    settings: Settings<'_>,
    environment: &BTreeMap<String, String>,
    cancel: watch::Receiver<bool>,
    deployment: Deployment,
) -> Result<Report> {
    let mut report = Report {
        state: State::Passed,
        workspace: checkout.root.clone(),
        check: None,
        dependency_runs: vec![],
        dependency_error: None,
        reason: None,
    };
    let cache =
        crate::dependencies::Cache::new(cache_root.to_path_buf(), settings.registry_origins)?;
    let mut dependencies = crate::dependencies::Preparer::new(cache, settings.npm);
    let command = resolve_command(
        &Repository::open(&checkout.workdir)?,
        snapshot.tree,
        settings.command,
    )?;
    let checkout = match checkout.next_check(snapshot.head, cancel.clone()).await {
        Ok(checkout) => checkout,
        Err(error) => {
            return Ok(report.fail(State::EnvFailed, crate::checkout::failure(error.as_ref())));
        }
    };
    report.workspace = checkout.root.clone();
    if let Err(error) = checkout
        .set_history(settings.history, snapshot.head, cancel.clone())
        .await
    {
        return Ok(report.fail(State::EnvFailed, crate::checkout::failure(error.as_ref())));
    }
    if matches!(deployment, Deployment::RestoreSnapshot) {
        checkout.deploy(snapshot.tree, cancel.clone()).await?;
    }
    checkout
        .git(&["clean".into(), "-ffdx".into()], true, cancel.clone())
        .await?
        .require_success("Clean generated files before Check")?;
    checkout.ensure_attachments()?;
    match dependencies
        .prepare(
            &checkout,
            &Repository::open(&checkout.workdir)?,
            snapshot.tree,
            true,
            cancel.clone(),
        )
        .await
    {
        Ok(runs) => report.dependency_runs = runs,
        Err(error) => {
            let state = if error.kind == crate::dependencies::FailureKind::Cancelled {
                State::Cancelled
            } else {
                State::EnvFailed
            };
            let message = error.message.clone();
            report.dependency_error = Some(error);
            return Ok(report.fail(state, message));
        }
    }
    if checkout.tree_on_disk(snapshot.tree, cancel.clone()).await? != snapshot.tree {
        return Ok(report.fail(
            State::DirtyBeforeCheck,
            "The checkout differs from the candidate before Check.",
        ));
    }
    let tree = snapshot.tree.to_string();
    let (success, cancelled) = if let Some(command) = command {
        let output = checkout
            .run_check(
                &command,
                environment,
                cancel.clone(),
                Duration::from_millis(settings.timeout_ms),
            )
            .await?;
        let result = (output.success(), output.cancelled);
        report.check = Some(output_record(
            &tree,
            snapshot.settings_hash,
            Some(command),
            output,
        ));
        result
    } else {
        report.check = Some(Record {
            tree: tree.clone(),
            settings_hash: snapshot.settings_hash.into(),
            command: None,
            code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        });
        (true, false)
    };
    if cancelled {
        return Ok(report.fail(State::Cancelled, "Check cancelled."));
    }
    if checkout.tree_on_disk(snapshot.tree, cancel).await? != snapshot.tree {
        return Ok(report.fail(State::Mutated, "Check changed files in the candidate."));
    }
    if !success {
        if !settings.history
            && report.check.as_ref().is_some_and(|check| {
                [
                    "No names found, cannot describe anything",
                    "No tags can describe",
                    "fatal: shallow",
                ]
                .iter()
                .any(|message| check.stderr.contains(message))
            })
        {
            return Ok(report.fail(State::HistoryRequired, "This check needs Git history."));
        }
        return Ok(report.fail(State::Failed, "The Check failed."));
    }
    Ok(report)
}
