//! Public authenticated commands, without a worker configuration or API calls.
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
async fn paired_public_edit_and_control_require_authentication_and_preserve_replay_while_paused()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, word, initial) = pending(&f).await?;
    f.journal
        .two_ways(PairChange::Command(command(&initial, Action::Cancel)))
        .await?;
    let directory = tempfile::tempdir_in("/private/tmp")?;
    let _lock = crate::lock::DaemonLock::acquire(directory.path())?.unwrap();
    let editor =
        crate::editor::Manager::new(directory.path(), &directory.path().join("editor.log"))?;
    let token = "c8".repeat(32);
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
        let id=Uuid::now_v7();
        for (method,params) in [("two-ways.options",json!({"inputId":word["id"],"revision":word["revision"]})),("two-ways.read",json!({"id":id})),("two-ways.command",json!({"id":id,"opId":Uuid::now_v7(),"revision":1,"action":"cancel"}))]{
            let mut unauth=BufReader::new(UnixStream::connect(&socket).await?);
            assert_eq!(call(&mut unauth,method,params).await?["error"]["code"],-32001);
        }
        let mut stream=BufReader::new(UnixStream::connect(&socket).await?);
        assert!(call(&mut stream,"rpc.authenticate",json!({"token":token})).await?["error"].is_null());
        let owner=Uuid::now_v7();let lease=Uuid::now_v7();
        let begun=call(&mut stream,"utterance.edit",json!({"owner":owner,"action":{"kind":"begin","token":lease,"utteranceId":word["id"],"revision":word["revision"]}})).await?;
        assert!(begun["error"].is_null(),"{begun}");
        let end=json!({"owner":owner,"action":{"kind":"end","token":lease,"revision":begun["result"]["editHold"]["revision"],"edit":{"kind":"tryTwoWays","opId":id,"selection":initial.request.selection}}});
        let saved=call(&mut stream,"utterance.edit",end.clone()).await?;
        assert!(saved["error"].is_null(),"{saved}");
        let job=&saved["result"]["twoWays"];
        assert_eq!(job["phase"],"waiting");assert!(saved["result"]["editHold"].is_null());
        assert_eq!(call(&mut stream,"utterance.edit",end).await?["result"]["twoWays"],*job);
        assert_eq!(call(&mut stream,"two-ways.read",json!({"id":id})).await?["result"],*job);
        for field in ["workspace","path","summary","auth","credentials","files"]{assert!(job.get(field).is_none(),"{field}");}
        assert_eq!(call(&mut stream,"two-ways.read",json!({"id":id,"sourceTrack":"A"})).await?["error"]["code"],-32602);
        assert_eq!(call(&mut stream,"two-ways.options",json!({"inputId":word["id"],"revision":word["revision"],"modelId":"unreviewed"})).await?["error"]["code"],-32602);
        for action in ["publish","cancelled","capture","branch-ready"]{
            assert_eq!(call(&mut stream,"two-ways.command",json!({"id":id,"opId":Uuid::now_v7(),"revision":1,"action":action})).await?["error"]["code"],-32602);
        }
        assert_eq!(call(&mut stream,"two-ways.prepare",json!({"id":id})).await?["error"]["code"],-32601);
        assert_eq!(call(&mut stream,"system.pause",json!({"opId":Uuid::now_v7(),"paused":true})).await?["result"]["paused"],true);
        assert!(call(&mut stream,"two-ways.command",json!({"id":id,"opId":Uuid::now_v7(),"revision":1,"action":"retry"})).await?["error"].is_object());
        let change=json!({"id":id,"opId":Uuid::now_v7(),"revision":1,"action":"cancel"});
        let cancelled=call(&mut stream,"two-ways.command",change.clone()).await?;
        assert!(cancelled["error"].is_null(),"{cancelled}");assert_eq!(cancelled["result"]["phase"],"cancelled");
        assert_eq!(call(&mut stream,"two-ways.command",change).await?["result"],cancelled["result"]);
        assert_eq!(f.journal.snapshot().await?["dispatchPaused"],true);
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let _ = stop.send(());
    let served = task.await?;
    editor.stop().await;
    checks?;
    served?;
    Ok(())
}
