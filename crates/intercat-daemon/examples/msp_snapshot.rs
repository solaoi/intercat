//! Exercise persisted snapshot seeds against the pinned SDK store oracle.
use intercat_daemon::{
    Result, clock::Stamp, journal::Journal, secrets::Key, session_snapshot::Snapshot,
};
use serde_json::{Value, json};

#[path = "support/msp_transport_types.rs"]
mod wire_types;

async fn put(journal: &Journal, session: &str, frame: &Value) -> Result<()> {
    journal
        .try_msp(
            "track".into(),
            session.into(),
            Stamp::now(),
            frame.clone(),
            None,
            None,
        )?
        .await??;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let input = std::env::args_os().nth(1).ok_or("fixture")?;
    let output = std::env::args_os().nth(2).ok_or("output")?;
    let fixture: Value = serde_json::from_slice(&std::fs::read(input)?)?;
    let root = tempfile::tempdir()?;
    let mut cases = vec![];
    let mut validated = 0;
    for (index, c) in fixture["cases"]
        .as_array()
        .ok_or("cases")?
        .iter()
        .enumerate()
    {
        let session = c["sessionId"].as_str().ok_or("session")?;
        Snapshot::from_read(&c["read"])?.ok_or("missing snapshot")?;
        for frame in c["before"]
            .as_array()
            .unwrap()
            .iter()
            .chain(c["after"].as_array().unwrap())
            .chain(c["suffix"].as_array().unwrap())
        {
            let name = match frame["method"].as_str() {
                Some("item/completed") => "ItemCompletedParams",
                Some("session/nameChanged") => "SessionNameChangedParams",
                Some("session/goalChanged") => "SessionGoalChangedParams",
                _ => return Err("unknown fixture method".into()),
            };
            wire_types::validate(&wire_types::Validation {
                name: name.into(),
                value: frame["params"].clone(),
            })?;
            validated += 1;
        }
        let path = root.path().join(format!("{index}.sqlite"));
        let journal = Journal::open(&path, Key::new([7; 32])).await?;
        for frame in c["before"]
            .as_array()
            .unwrap()
            .iter()
            .chain(c["after"].as_array().unwrap())
        {
            put(&journal, session, frame).await?;
        }
        let source = journal
            .msp_raw(
                "track".into(),
                session.into(),
                Stamp::now(),
                json!({"jsonrpc":"2.0","id":1,"result":c["read"]}).into(),
                None,
            )
            .await?;
        drop(journal);
        let journal = Journal::open(&path, Key::new([7; 32])).await?;
        let before = journal.reconcile_local(session.into()).await?;
        if before.view.snapshot_state().is_some() {
            return Err("Uncommitted seed was applied".into());
        }
        if c["opening"] == true {
            if !journal
                .opening_snapshot("track".into(), session.into(), source, None)
                .await?
            {
                return Err("Opening snapshot was not installed".into());
            }
        } else {
            journal
                .view_snapshot("track".into(), session.into(), source)
                .await?;
        }
        for frame in c["suffix"].as_array().unwrap() {
            put(&journal, session, frame).await?;
        }
        let view = journal.reconcile_local(session.into()).await?.view;
        let timeline = journal
            .timeline_window(intercat_daemon::timeline::Window {
                tracks: vec!["track".into()],
                start: 0,
                end: i64::MAX,
                since: None,
            })
            .await?;
        let mut details = Vec::new();
        for clip in timeline["clips"].as_array().unwrap() {
            details.push(
                journal
                    .clip_detail(clip["id"].as_str().unwrap().into())
                    .await?,
            );
        }
        let result = json!({"id":c["id"],"items":view.items,"sessionState":view.session_state,"fullState":view.snapshot_state(),"timeline":timeline,"details":details});
        drop(journal);
        let journal = Journal::open(&path, Key::new([7; 32])).await?;
        let restored = journal.reconcile_local(session.into()).await?.view;
        if json!(restored.items) != result["items"]
            || json!(restored.session_state) != result["sessionState"]
            || restored.snapshot_state() != view.snapshot_state()
        {
            return Err("Restart changed the snapshot view".into());
        }
        let after = journal
            .timeline_window(intercat_daemon::timeline::Window {
                tracks: vec!["track".into()],
                start: 0,
                end: i64::MAX,
                since: None,
            })
            .await?;
        if after != result["timeline"] {
            return Err("Restart changed the timeline projection".into());
        }
        let mut result = result;
        result["restartVerified"] = json!(true);
        result["rawOnlyRestartVerified"] = json!(true);
        cases.push(result);
    }
    std::fs::write(
        output,
        serde_json::to_vec(
            &json!({"cases":cases,"validatedFrames":validated,"schemaVersion":intercat_daemon::journal::SCHEMA_VERSION}),
        )?,
    )?;
    Ok(())
}
