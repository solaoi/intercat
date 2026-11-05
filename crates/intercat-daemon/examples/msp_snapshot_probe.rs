//! Native snapshot/subscription and cold-resume probe; optional single dummy turn.
use intercat_daemon::{
    Result,
    clock::Stamp,
    history::{self, Context},
    host::{self, Boundary, Host, Profile},
    journal::Journal,
    msp::Event,
    reconcile::{self, Expected},
    secrets::Key,
    session_snapshot::Snapshot,
};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use uuid::Uuid;

async fn exchange(
    host: &Host,
    calls: &mut Vec<Value>,
    method: &str,
    params: Value,
) -> Result<Value> {
    let result = host
        .connection
        .client
        .request(method, params.clone())
        .await?;
    calls.push(json!({"method":method,"params":params,"result":result}));
    Ok(result)
}
async fn drain(host: &mut Host, session: &str) -> Result<Vec<Value>> {
    let mut frames = vec![];
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(150), host.connection.next_event()).await
    {
        match event {
            Event::Frame(received) if received.frame["params"]["sessionId"] == session => {
                frames.push(received.frame)
            }
            Event::Closed(reason) => return Err(reason.into()),
            _ => {}
        }
    }
    Ok(frames)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let binary = PathBuf::from(args.next().ok_or("binary")?).canonicalize()?;
    let auth = PathBuf::from(args.next().ok_or("authentication file")?);
    let output = PathBuf::from(args.next().ok_or("output directory")?);
    let with_turn = match args.next().as_deref() {
        None => false,
        Some(arg) if arg == "--with-turn" => true,
        _ => return Err("Expected --with-turn or no additional argument".into()),
    };
    std::fs::create_dir_all(&output)?;
    if host::hash(&binary)? != host::BINARY_SHA256 {
        return Err("Unverified Muse binary".into());
    }
    let temporary = tempfile::tempdir()?;
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    std::fs::write(
        workspace.join("README.txt"),
        "Disposable snapshot subscription fixture. No model work.\n",
    )?;
    let rpc = temporary.path().join("rpc");
    std::fs::create_dir(&rpc)?;
    let denied = vec![std::env::current_dir()?];
    let mut host = Host::spawn(
        &binary,
        &temporary.path().join("host"),
        &workspace,
        &std::env::var_os("PATH").ok_or("PATH")?,
        Some(&auth),
        Boundary {
            profile: Profile::Worker,
            secrets: &[],
            rpc: &rpc,
            denied: &denied,
        },
    )
    .await?;
    let journal_path = temporary.path().join("journal.sqlite");
    let journal = Journal::open(&journal_path, Key::new([7; 32])).await?;
    let result=async {
        let initialized=host.connection.client.initialize().await?;
        let mut calls=vec![];
        let opened=exchange(&host,&mut calls,"session/start",json!({"commandId":Uuid::now_v7(),"workspaceRoot":workspace,
            "approvalMode":"promptUnmatched","providerId":"meta","modelId":"muse-spark-1.3-contributor","recoveryMode":"on"})).await?;
        let session=opened["session"]["sessionId"].as_str().ok_or("session identity")?.to_owned();
        let mut before=drain(&mut host,&session).await?;
        let submitted = if with_turn {
            let command=Uuid::now_v7();
            let params=json!({"commandId":command,"sessionId":session,"input":[{"type":"text","text":"This is a disposable protocol test. Do not use tools or inspect any files. Reply exactly: snapshot recovery probe."}]});
            journal.intent(&command.to_string(),"turn.start",params.clone()).await?;
            std::fs::write(output.join("model-turn.json"),serde_json::to_vec_pretty(&json!({"attempted":1,"params":params}))?)?;
            let ack=exchange(&host,&mut calls,"turn/start",params).await?;
            let turn=ack["turnId"].as_str().ok_or("Missing submitted turn")?.to_owned();
            tokio::time::timeout(Duration::from_secs(120),async {
                loop {
                    match host.connection.next_event().await {
                        Some(Event::Frame(received)) if received.frame["params"]["sessionId"]==session => {
                            let done=received.frame["method"]=="turn/completed" && received.frame["params"]["turnId"]==turn;
                            before.push(received.frame);
                            if done {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(());}
                        }
                        Some(Event::Closed(reason)) => return Err(reason.into()),
                        None => return Err("Host ended before the dummy turn completed".into()),
                        _ => {}
                    }
                }
            }).await??;
            Some((turn,command))
        } else {None};
        exchange(&host,&mut calls,"view/unsubscribe",json!({"sessionId":session})).await?;
        let read=exchange(&host,&mut calls,"session/read",json!({"sessionId":session,"excludeItems":false})).await?;
        std::fs::write(output.join("read.json"),serde_json::to_vec_pretty(&read)?)?;
        let command=Uuid::now_v7();
        let params=json!({"commandId":command,"sessionId":session,"excludeItems":false,"history":"snapshot"});
        journal.intent(&command.to_string(),"session/resume",params.clone()).await?;
        let resumed=history::resume(&mut host.connection,Context{journal:&journal,track:"track",session:&session,scanner:None},command).await?;
        calls.push(json!({"method":"session/resume","params":params,"result":resumed}));
        std::fs::write(output.join("resume.json"),serde_json::to_vec_pretty(&resumed)?)?;
        let snapshot=Snapshot::from_read(&resumed)?.ok_or("The native resume did not supply a snapshot")?;
        exchange(&host,&mut calls,"view/unsubscribe",json!({"sessionId":session})).await?;
        let page=exchange(&host,&mut calls,"view/page",json!({"sessionId":session,"cursor":snapshot.cursor,"limit":200,"direction":"forward"})).await?;
        let renamed=exchange(&host,&mut calls,"session/rename",json!({"sessionId":session,"commandId":Uuid::now_v7(),"name":"Intercat snapshot-only fixture"})).await?;
        let while_unsubscribed=drain(&mut host,&session).await?;
        let subscribed=exchange(&host,&mut calls,"view/subscribe",json!({"sessionId":session,"after":snapshot.cursor})).await?;
        let suffix=drain(&mut host,&session).await?;
        for frame in &suffix {
            journal.try_msp("track".into(),session.clone(),Stamp::now(),frame.clone(),None,None)?.await??;
        }
        let local=journal.reconcile_local(session.clone()).await?;
        let stored=local.view.snapshot_state().ok_or("The native snapshot was not installed")?;
        std::fs::write(output.join("local.json"),serde_json::to_vec_pretty(&json!({"snapshot":stored,"suffix":suffix,"renamed":renamed}))?)?;
        if stored["name"]!=renamed["name"] {return Err("Subscription suffix did not update the installed snapshot".into());}
        let after=exchange(&host,&mut calls,"session/read",json!({"sessionId":session,"excludeItems":false})).await?;
        let result=json!({"binarySha256":host::BINARY_SHA256,"schema":initialized["schema"],"sessionId":session,
            "modelTurns":u8::from(with_turn),"submitted":submitted,"snapshotCursor":snapshot.cursor,"readHead":read["viewCursor"],"readMode":read["history"]["mode"],"resumeMode":resumed["history"]["mode"],
            "localSnapshot":stored,"localCursor":journal.view_cursor("track".into(),session.clone()).await?,
            "localItems":local.view.items,"localSessionState":local.view.session_state,
            "pageFromSnapshot":page,"subscribeHead":subscribed["viewCursor"],"afterHead":after["viewCursor"],
            "before":before,"whileUnsubscribed":while_unsubscribed,"suffix":suffix,"calls":calls});
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&result)?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(result)
    }.await;
    host.shutdown().await?;
    let mut result = result?;
    drop(journal);
    let journal = Journal::open(&journal_path, Key::new([7; 32])).await?;
    let session = result["sessionId"].as_str().ok_or("session")?.to_owned();
    let restored = journal.reconcile_local(session.clone()).await?;
    if json!(restored.view.items) != result["localItems"]
        || json!(restored.view.session_state) != result["localSessionState"]
    {
        return Err("Journal reopen changed the snapshot state".into());
    }
    let mut host = Host::spawn(
        &binary,
        &temporary.path().join("restarted-host"),
        &workspace,
        &std::env::var_os("PATH").ok_or("PATH")?,
        Some(&auth),
        Boundary {
            profile: Profile::Worker,
            secrets: &[],
            rpc: &rpc,
            denied: &denied,
        },
    )
    .await?;
    let cold=async {
        host.connection.client.initialize().await?;
        let command=Uuid::now_v7();
        let resumed=history::resume(&mut host.connection,Context{journal:&journal,track:"track",session:&session,scanner:None},command).await?;
        let head=resumed["viewCursor"].as_str().ok_or("cold head")?;
        history::recover(&mut host.connection,Context{journal:&journal,track:"track",session:&session,scanner:None},head,None).await?;
        let turn=result["submitted"][0].as_str().map(str::parse).transpose()?;
        let expected=Expected{turn,checkpoint:None,checkpoint_pending:with_turn,..Expected::default()};
        let recovered=reconcile::prepare(&mut host.connection,Context{journal:&journal,track:"track",session:&session,scanner:None},expected).await?;
        if recovered.resume_turn!=turn || recovered.active_turn.is_some() {return Err("Cold resume recovered a different turn".into());}
        let local=journal.reconcile_local(session.clone()).await?;
        if json!(local.view.items)!=result["localItems"] {return Err("Cold resume changed transcript items".into());}
        if local.view.snapshot_state().as_ref()!=resumed.pointer("/history/snapshot/state") {
            return Err("Reconciliation changed the authoritative cold snapshot state".into());
        }
        let replay=if let Some(turn)=turn {journal.reconcile_replay(session.clone(),turn).await?} else {vec![]};
        if with_turn && (recovered.command.map(|c|c.to_string()).as_deref()!=result["submitted"][1].as_str() || replay.len()!=2) {
            return Err("Cold resume lost submit identity or completed report".into());
        }
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(json!({"resume":resumed,"items":local.view.items,"sessionState":local.view.session_state,"fullState":local.view.snapshot_state(),"replay":replay,"command":recovered.command,"resumeTurn":recovered.resume_turn,"activeTurn":recovered.active_turn,"journalReopenVerified":true}))
    }.await;
    host.shutdown().await?;
    result["cold"] = cold?;
    std::fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    println!(
        "Native snapshot/subscription and cold-resume probe completed (model turns: {}).",
        u8::from(with_turn)
    );
    Ok(())
}
