//! Compare real worker approval orchestration without a model or shell execution.
use intercat_daemon::{
    Result, execution, journal::Journal, msp::Connection, process, runtime::WorkerState,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[path = "../tests/support/approval_fixture.rs"]
mod approval_fixture;
use approval_fixture::Fixture;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    let fixture: Fixture = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let _: muse_code_sdk::msp::ApprovalRequestParams =
        serde_json::from_value(fixture.request.clone())?;
    let _: muse_code_sdk::msp::ApprovalUpdatedParams =
        serde_json::from_value(fixture.update.clone())?;
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Offline approval probe".into(),
            intercat_daemon::decision::Target::New,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.ok_or("Missing probe input")?;
    journal
        .apply_input(
            input.id,
            input.revision,
            intercat_daemon::decision::Outcome::Classified(intercat_daemon::decision::Decision {
                kind: intercat_daemon::decision::Kind::Fork,
                track_id: None,
                boundary: Some("when-told".into()),
                command: None,
                confidence: 1.0,
                inference_ms: 0.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let track = journal.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .ok_or("Missing probe track")?
        .to_owned();
    let (client_side, server_side) = tokio::io::duplex(1024 * 1024);
    let (read, write) = tokio::io::split(client_side);
    let mut connection = Connection::new(read, write);
    let (port, commands) = execution::Port::new(std::sync::Arc::new(
        intercat_daemon::dispatch_control::Gate::new(false),
    ));
    port.attach(connection.client.clone(), fixture.session.clone(), None);
    let mut context = execution::Context {
        project_secrets: Default::default(),
        port,
        commands,
        editor: intercat_daemon::editor::Manager::new(
            root.path(),
            &root.path().join("editor.log"),
        )?,
        capacity: Arc::new(intercat_daemon::scheduler::Capacity::new(1)),
    };
    let mut state = WorkerState {
        secret_revision: None,
        track_id: track,
        phase: "working".into(),
        session_id: Some(fixture.session.clone()),
        identity: None,
        detail: None,
        active_turn: Some(fixture.turn),
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    let identity = process::identity(std::process::id())?;
    let tracker = Arc::new(Mutex::new(process::Tracker::new(
        "approval-probe".into(),
        identity,
    )?));
    let (_stop, mut stopping) = tokio::sync::watch::channel(false);
    let mut continuity = execution::Continuity::default();
    // The fixture chooses Abort through the same public control used by UI.
    let controls = context.port.clone();
    let ui_journal = journal.clone();
    let ui = tokio::spawn(async move {
        let mut changes = ui_journal.subscribe();
        let mut answered = std::collections::BTreeSet::new();
        while changes.changed().await.is_ok() {
            let snapshot = ui_journal.snapshot().await?;
            for worker in snapshot["workers"].as_array().into_iter().flatten() {
                for card in worker["approvals"].as_array().into_iter().flatten() {
                    if card["canDeny"] == true
                        && card["canAllow"] == false
                        && answered.insert(format!("{}:{}", card["id"], card["requirement"]))
                    {
                        controls
                            .answer(
                                Uuid::now_v7(),
                                card["id"].as_str().unwrap().into(),
                                card["requirement"].clone(),
                                intercat_daemon::approval::Response::Deny,
                            )
                            .await?;
                    }
                }
            }
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let workspace = fixture.workspace.clone();
    let server = tokio::spawn(approval_fixture::serve(server_side, fixture, false));
    let result = execution::run(
        execution::WorkerIo {
            connection: &mut connection,
            tracker,
            identity,
            recovery: None,
            continuity: &mut continuity,
        },
        &journal,
        &workspace,
        &mut state,
        &mut context,
        &mut stopping,
    )
    .await;
    ui.abort();
    continuity.settle().await?;
    let mut output = server.await??;
    let mut operations = Vec::new();
    for frame in output["sent"].as_array().unwrap() {
        let id = frame["params"]["commandId"]
            .as_str()
            .ok_or("Missing sent command ID")?
            .parse()?;
        operations.push(
            journal
                .operation(id)
                .await?
                .ok_or("Sent decision was not recorded")?,
        );
    }
    output["operations"] = json!(operations);
    output["workerEnded"] = json!(result.is_err());
    output["workerError"] = json!(result.err().map(|error| error.to_string()));
    output["state"] = serde_json::to_value(&state)?;
    std::fs::write(&args[2], serde_json::to_vec(&output)?)?;
    println!(
        "{}",
        json!({"decisions":output["sent"].as_array().unwrap().len(),"complete":output["complete"]})
    );
    Ok(())
}
