use intercat_daemon::{
    decision::Target,
    editor::{Config, Manager},
    input::Processor,
    journal::Journal,
};
use std::{fs, sync::Arc, time::Duration};
use tokio::sync::Notify;

async fn wait_state(journal: &Journal, id: &str, state: &str) {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let value = journal.snapshot().await.unwrap();
            if value["utterances"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["id"] == id && w["state"] == state)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn inference_cannot_revive_a_cut_request_and_restarts_resume_waiting_words() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source/intercat_editor");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("__init__.py"), "").unwrap();
    fs::write(source.join("server.py"),r#"import json,time
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def respond(self,value):
  body=json.dumps(value).encode();self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_GET(self):self.respond({'accepting':True,'modelLoaded':True})
 def do_POST(self):
  request=json.loads(self.rfile.read(int(self.headers['Content-Length'])));text=json.loads(request['tail'])['request'];time.sleep(0.3)
  selected={};stages={}
  for stage in request['stages']:
   if any(selected.get(key) not in values for key,values in stage.get('when',{}).items()):continue
   labels=stage['candidates'];choice=' E' if stage['name']=='decision' and text=='park' else ' D' if stage['name']=='boundary' else labels[0]
   selected[stage['name']]=choice;n=len(labels);stages[stage['name']]={'selected':choice,'logprobs':{x:-0.1 if x==choice else -2 for x in labels},'probabilities':{x:1 if n==1 else .9 if x==choice else .1/(n-1) for x in labels}}
  self.respond({'stages':stages,'inferenceMs':1300 if text=='slow' else 300})
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True);server.serve_forever()
"#).unwrap();
    let model = root.path().join("model");
    fs::create_dir(&model).unwrap();
    let config = Config {
        python: "/usr/bin/python3".into(),
        model,
    };
    let editor =
        Manager::new(&root.path().join("source"), &root.path().join("editor.log")).unwrap();
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone())
        .await
        .unwrap();
    let first = uuid::Uuid::now_v7();
    journal
        .stage_utterance(first, "first".into(), Target::New, None, Vec::new())
        .await
        .unwrap();
    wake.notify_one();
    assert_eq!(
        journal.snapshot().await.unwrap()["utterances"][0]["state"],
        "staged"
    );
    editor.configure(config.clone()).await.unwrap();
    wait_state(&journal, &first.to_string(), "reading").await;
    journal
        .cut_input(uuid::Uuid::now_v7(), first.to_string())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(
        journal.snapshot().await.unwrap()["tracks"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let second = uuid::Uuid::now_v7();
    journal
        .stage_utterance(second, "second".into(), Target::New, None, Vec::new())
        .await
        .unwrap();
    wake.notify_one();
    wait_state(&journal, &second.to_string(), "placed").await;
    let slow = uuid::Uuid::now_v7();
    journal
        .stage_utterance(slow, "slow".into(), Target::New, None, Vec::new())
        .await
        .unwrap();
    wake.notify_one();
    wait_state(&journal, &slow.to_string(), "asked").await;
    assert_eq!(
        journal.snapshot().await.unwrap()["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    editor.stop().await;
    let parked = uuid::Uuid::now_v7();
    journal
        .stage_utterance(parked, "park".into(), Target::None, None, Vec::new())
        .await
        .unwrap();
    wake.notify_one();
    editor.configure(config).await.unwrap();
    wait_state(&journal, &parked.to_string(), "parked").await;
    processor.shutdown().await.unwrap();
    editor.stop().await;
}
