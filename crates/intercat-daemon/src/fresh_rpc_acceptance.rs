//! Own saved Git boundary over the actual authenticated Unix socket. No API calls.
use super::*;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

async fn call(stream: &mut BufReader<UnixStream>, method: &str, params: Value) -> Result<Value> {
    stream
        .get_mut()
        .write_all(
            format!(
                "{}\n",
                json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
            )
            .as_bytes(),
        )
        .await?;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_line(&mut line)).await??;
    Ok(serde_json::from_str(&line)?)
}

#[tokio::test]
async fn fresh_socket_authentication_replay_pause_and_private_projection() -> Result<()> {
    let f = Fixture::new(1).await?;
    let point = f.save(0, None).await?;
    let mut source = worker(&f, point).await?;
    source.phase = "working".into();
    let active = Uuid::now_v7();
    source.active_turn = Some(active);
    f.journal.worker_state(source.clone()).await?;
    let directory = tempfile::tempdir_in("/private/tmp")?;
    let _lock = crate::lock::DaemonLock::acquire(directory.path())?.unwrap();
    let editor =
        crate::editor::Manager::new(directory.path(), &directory.path().join("editor.log"))?;
    let token = "c7".repeat(32);
    let server = crate::rpc::Server::bind(
        directory.path(),
        crate::rpc::Token::parse(&token)?,
        f.journal.clone(),
        editor.clone(),
        &f.root.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let checks=async{
        let socket=directory.path().join("rpc.sock");
        let input=serde_json::to_value(request(&source))?;
        for (method,params) in [("worker.fresh.prepare",input.clone()),("worker.fresh.read",json!({"id":input["opId"]})),("worker.fresh.command",json!({"opId":Uuid::now_v7(),"id":input["opId"],"revision":1,"action":{"kind":"cancel"}}))]{
            let mut unauth=BufReader::new(UnixStream::connect(&socket).await?);
            assert_eq!(call(&mut unauth,method,params).await?["error"]["code"],-32001);
        }
        let mut stream=BufReader::new(UnixStream::connect(&socket).await?);
        assert!(call(&mut stream,"rpc.authenticate",json!({"token":token})).await?["error"].is_null());
        let mut extra=input.clone();extra["summaryInput"]=json!({"private":"DO_NOT_ACCEPT"});
        assert_eq!(call(&mut stream,"worker.fresh.prepare",extra).await?["error"]["code"],-32602);
        let job=call(&mut stream,"worker.fresh.prepare",input.clone()).await?;
        assert!(job["error"].is_null(),"{job}");
        assert_eq!(job["result"]["phase"],"waiting-for-turn");
        assert_eq!(call(&mut stream,"worker.fresh.prepare",input.clone()).await?["result"],job["result"]);
        let id=input["opId"].as_str().unwrap();
        assert_eq!(call(&mut stream,"worker.fresh.read",json!({"id":id})).await?["result"],job["result"]);
        for field in ["summaryInput","summary","workspace","folder","boundary"]{assert!(job["result"].get(field).is_none(),"{field}");}
        for kind in ["cancelled","transfer","summarized","published"]{
            assert_eq!(call(&mut stream,"worker.fresh.command",json!({"opId":Uuid::now_v7(),"id":id,"revision":1,"action":{"kind":kind}})).await?["error"]["code"],-32602);
        }
        assert_eq!(call(&mut stream,"system.pause",json!({"opId":Uuid::now_v7(),"paused":true})).await?["result"]["paused"],true);
        let blocked=call(&mut stream,"worker.fresh.command",json!({"opId":Uuid::now_v7(),"id":id,"revision":1,"action":{"kind":"retry"}})).await?;
        assert!(blocked["error"].is_object());
        let change=json!({"opId":Uuid::now_v7(),"id":id,"revision":job["result"]["revision"],"action":{"kind":"cancel"}});
        let cancelled=call(&mut stream,"worker.fresh.command",change.clone()).await?;
        assert!(cancelled["error"].is_null(),"{cancelled}");assert_eq!(cancelled["result"]["phase"],"cancelled");
        assert_eq!(call(&mut stream,"worker.fresh.command",change).await?["result"],cancelled["result"]);
        assert_eq!(f.journal.snapshot().await?["dispatchPaused"],true);
        assert_eq!(f.journal.snapshot().await?["workers"][0]["activeTurn"],active.to_string());
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let _ = stop.send(());
    let served = task.await?;
    editor.stop().await;
    checks?;
    served?;
    Ok(())
}
