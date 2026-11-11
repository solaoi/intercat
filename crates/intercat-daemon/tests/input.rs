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
    fs::write(source.join("server.py"),r#"import json,math,time
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def respond(self,value):
  if 'stages' in value:
   value['temperature']=self.temperature
   for stage in value['stages'].values():stage['logprobs']={label:math.log(max(probability,1e-300))*self.temperature for label,probability in stage['probabilities'].items()}
  body=json.dumps(value).encode();self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_GET(self):self.respond({'accepting':True,'modelLoaded':True})
 def do_POST(self):
  request=json.loads(self.rfile.read(int(self.headers['Content-Length'])));self.temperature=request.get('temperature',1)
  if self.path=='/tokens':self.respond({'tokens':10});return
  text=json.loads(request['tail'])['request'];time.sleep(0.3)
  selected={};stages={}
  for stage in request['stages']:
   if any(selected.get(key) not in values for key,values in stage.get('when',{}).items()):continue
   labels=stage['candidates'];choice=' E' if stage['name']=='decision' and text=='park' else ' D' if stage['name']=='boundary' else labels[0]
   selected[stage['name']]=choice;n=len(labels);stages[stage['name']]={'selected':choice,'logprobs':{x:-0.1 if x==choice else -2 for x in labels},'probabilities':{x:1 if n==1 else .9 if x==choice else .1/(n-1) for x in labels}}
  self.respond({'cachePrefix':False,'stages':stages,'inferenceMs':1300 if text=='slow' else 300})
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
    wait_state(&journal, &parked.to_string(), "placed").await;
    processor.shutdown().await.unwrap();
    editor.stop().await;
}

#[tokio::test]
async fn other_waits_for_expanded_candidates_and_a_cut_wins_during_the_second_pass() {
    use intercat_daemon::decision::{Decision, Kind, Outcome};
    use serde_json::{Value, json};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source/intercat_editor");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("__init__.py"), "").unwrap();
    fs::write(source.join("server.py"), r#"import json,math,time
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
calls={}
log=Path(__file__).with_name('calls.jsonl')
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def respond(self,value):
  if 'stages' in value:
   value['temperature']=self.temperature
   for stage in value['stages'].values():stage['logprobs']={label:math.log(max(probability,1e-300))*self.temperature for label,probability in stage['probabilities'].items()}
  body=json.dumps(value).encode();self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_GET(self):
  body=b'{"accepting":true,"modelLoaded":true}';self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_POST(self):
  request=json.loads(self.rfile.read(int(self.headers['Content-Length'])));self.temperature=request.get('temperature',1)
  if self.path=='/tokens':self.respond({'tokens':10});return
  tail=json.loads(request['tail']);text=tail['request'];calls[text]=calls.get(text,0)+1;attempt=calls[text]
  with log.open('a') as f:f.write(json.dumps({'text':text,'attempt':attempt,'tracks':tail['tracks']})+'\n')
  selected={};stages={}
  for stage in request['stages']:
   if any(selected.get(key) not in values for key,values in stage.get('when',{}).items()):continue
   labels=stage['candidates'];choice=' A' if stage['name']=='decision' else ('9' if attempt==1 or text=='again-other' else '1') if stage['name']=='lane' else labels[0]
   selected[stage['name']]=choice;n=len(labels);stages[stage['name']]={'selected':choice,'logprobs':{x:-0.1 if x==choice else -2 for x in labels},'probabilities':{x:1 if n==1 else .9 if x==choice else .1/(n-1) for x in labels}}
  for stage in stages.values():stage['logprobs']={label:math.log(max(probability,1e-300))*self.temperature for label,probability in stage['probabilities'].items()}
  body=json.dumps({'cachePrefix':False,'temperature':self.temperature,'stages':stages,'inferenceMs':700}).encode();self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();time.sleep(.7)
  try:self.wfile.write(body)
  except BrokenPipeError:pass
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True);server.serve_forever()
"#).unwrap();
    let model = root.path().join("model");
    fs::create_dir(&model).unwrap();
    let editor =
        Manager::new(&root.path().join("source"), &root.path().join("editor.log")).unwrap();
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    for index in 0..12 {
        journal
            .stage_utterance(
                uuid::Uuid::now_v7(),
                format!("Task {index}"),
                Target::New,
                None,
                vec![],
            )
            .await
            .unwrap();
        let input = journal.claim_input().await.unwrap().unwrap();
        journal
            .apply_input(
                input.id,
                input.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Fork,
                    track_id: None,
                    boundary: Some("when-told".into()),
                    command: None,
                    confidence: 1.0,
                    inference_ms: 0.0,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await
            .unwrap();
    }
    editor
        .configure(Config {
            python: "/usr/bin/python3".into(),
            model,
        })
        .await
        .unwrap();
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone())
        .await
        .unwrap();
    for text in ["second-pass", "again-other", "cut-second"] {
        let id = uuid::Uuid::now_v7();
        let started = std::time::Instant::now();
        journal
            .stage_utterance(id, text.into(), Target::None, None, vec![])
            .await
            .unwrap();
        wake.notify_one();
        let second_pass = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let calls = fs::read_to_string(source.join("calls.jsonl")).unwrap_or_default();
                if calls
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .any(|call| call["text"] == text && call["attempt"] == 2)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if second_pass.is_err() {
            let snapshot = journal.snapshot().await.unwrap();
            let word = snapshot["utterances"]
                .as_array()
                .unwrap()
                .iter()
                .find(|word| word["id"] == id.to_string());
            panic!(
                "No second pass for {text}: Editor={:?}, request={word:?}, calls={}",
                editor.status(),
                fs::read_to_string(source.join("calls.jsonl")).unwrap_or_default()
            );
        }
        let snapshot = journal.snapshot().await.unwrap();
        let word = snapshot["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|word| word["id"] == id.to_string())
            .unwrap();
        assert_eq!(word["state"], "reading");
        assert!(word["decision"].is_null());
        if text == "cut-second" {
            journal
                .cut_input(uuid::Uuid::now_v7(), id.to_string())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(850)).await;
            wait_state(&journal, &id.to_string(), "cut").await;
        } else {
            wait_state(
                &journal,
                &id.to_string(),
                if text == "again-other" {
                    "asked"
                } else {
                    "placed"
                },
            )
            .await;
            assert!(started.elapsed() > Duration::from_millis(1200));
            let snapshot = journal.snapshot().await.unwrap();
            let word = snapshot["utterances"]
                .as_array()
                .unwrap()
                .iter()
                .find(|word| word["id"] == id.to_string())
                .unwrap();
            if text == "again-other" {
                assert!(word["reason"].as_str().unwrap().contains("Not started"));
            } else {
                let db = rusqlite::Connection::open(root.path().join("events.sqlite")).unwrap();
                let pass: i64 = db.query_row(
                    "SELECT json_extract(decision,'$.scores.routing.pass') FROM utterances WHERE id=?1",
                    [id.to_string()], |row| row.get(0),
                ).unwrap();
                assert_eq!(pass, 2);
                let timings = word["decision"]["timings"].as_array().unwrap();
                assert_eq!(timings.len(), 2);
                assert!(
                    timings
                        .iter()
                        .all(|timing| timing["roundTripMs"].as_f64().unwrap() >= 650.0)
                );
                assert_eq!(timings[0]["inferenceMs"], 700.0);
            }
        }
        let db = rusqlite::Connection::open(root.path().join("events.sqlite")).unwrap();
        let attempts: i64 = db.query_row("SELECT count(*) FROM events WHERE kind='input.inference' AND json_extract(payload,'$.utteranceId')=?1", [id.to_string()], |row| row.get(0)).unwrap();
        assert_eq!(attempts, 2);
        let calls: Vec<Value> = fs::read_to_string(source.join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|call: &Value| call["text"] == text)
            .collect();
        assert_eq!(calls.len(), 2);
        let first = &calls[0]["tracks"].as_array().unwrap();
        let second = &calls[1]["tracks"].as_array().unwrap();
        assert!(
            second
                .iter()
                .any(|track| !first.iter().any(|old| old["title"] == track["title"]))
        );
    }
    processor.shutdown().await.unwrap();
    editor.stop().await;
}

#[tokio::test]
async fn token_budget_reduces_candidates_keeps_the_lock_and_holds_an_oversized_request() {
    use intercat_daemon::decision::{Decision, Kind, Outcome};
    use serde_json::{Value, json};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source/intercat_editor");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("__init__.py"), "").unwrap();
    fs::write(source.join("server.py"), r#"import json,math
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
log=Path(__file__).with_name('requests.jsonl')
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def respond(self,value):
  if 'stages' in value:
   value['temperature']=self.temperature
   for stage in value['stages'].values():stage['logprobs']={label:math.log(max(probability,1e-300))*self.temperature for label,probability in stage['probabilities'].items()}
  body=json.dumps(value).encode();self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_GET(self):self.respond({'accepting':True,'modelLoaded':True})
 def do_POST(self):
  request=json.loads(self.rfile.read(int(self.headers['Content-Length'])));self.temperature=request.get('temperature',1)
  tail=json.loads(request['text'] if self.path=='/tokens' else request['tail'])
  with log.open('a') as output:output.write(json.dumps({'path':self.path,'tail':tail})+'\n')
  if self.path=='/tokens':
   self.respond({'tokens':600 if len(tail['tracks'])>4 or tail['request'].startswith('too-long') else 400});return
  selected={};stages={}
  for stage in request['stages']:
   if any(selected.get(key) not in values for key,values in stage.get('when',{}).items()):continue
   labels=stage['candidates'];choice=labels[0];selected[stage['name']]=choice;n=len(labels)
   stages[stage['name']]={'selected':choice,'logprobs':{x:-.1 if x==choice else -3 for x in labels},'probabilities':{x:.95 if x==choice else .05/(n-1) for x in labels}}
  self.respond({'cachePrefix':False,'stages':stages,'inferenceMs':10})
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True);server.serve_forever()
"#).unwrap();
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    for index in 0..8 {
        journal
            .stage_utterance(
                uuid::Uuid::now_v7(),
                format!("Task {index}"),
                Target::New,
                None,
                vec![],
            )
            .await
            .unwrap();
        let input = journal.claim_input().await.unwrap().unwrap();
        journal
            .apply_input(
                input.id,
                input.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Fork,
                    track_id: None,
                    boundary: Some("when-told".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await
            .unwrap();
    }
    let snapshot = journal.snapshot().await.unwrap();
    let locked = snapshot["tracks"][7]["id"].as_str().unwrap().to_owned();
    let selected = snapshot["tracks"][0]["id"].as_str().unwrap().to_owned();
    let editor =
        Manager::new(&root.path().join("source"), &root.path().join("editor.log")).unwrap();
    let model = root.path().join("model");
    fs::create_dir(&model).unwrap();
    editor
        .configure(Config {
            python: "/usr/bin/python3".into(),
            model,
        })
        .await
        .unwrap();
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone())
        .await
        .unwrap();
    let short = uuid::Uuid::now_v7();
    journal
        .stage_utterance(
            short,
            "Continue".into(),
            Target::Lock(locked.clone()),
            Some(selected),
            vec![],
        )
        .await
        .unwrap();
    wake.notify_one();
    wait_state(&journal, &short.to_string(), "placed").await;
    let too_long = uuid::Uuid::now_v7();
    journal
        .stage_utterance(
            too_long,
            "too-long: keep every word".into(),
            Target::New,
            None,
            vec![],
        )
        .await
        .unwrap();
    wake.notify_one();
    wait_state(&journal, &too_long.to_string(), "asked").await;
    let snapshot = journal.snapshot().await.unwrap();
    let word = snapshot["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["id"] == too_long.to_string())
        .unwrap();
    assert_eq!(word["text"], "too-long: keep every word");
    assert!(
        word["reason"]
            .as_str()
            .unwrap()
            .contains("Too long for the Editor")
    );
    let requests: Vec<Value> = fs::read_to_string(source.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let short_calls: Vec<_> = requests
        .iter()
        .filter(|request| request["tail"]["request"] == "Continue")
        .collect();
    assert!(short_calls.len() >= 3);
    assert!(short_calls[0]["tail"]["tracks"].as_array().unwrap().len() > 4);
    assert_eq!(
        short_calls[short_calls.len() - 2]["tail"]["tracks"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(short_calls.last().unwrap()["path"], "/decide");
    assert_eq!(
        short_calls.last().unwrap()["tail"]["target"]["trackId"],
        locked
    );
    assert!(!requests.iter().any(|request| request["path"] == "/decide"
        && request["tail"]["request"] == "too-long: keep every word"));
    processor.shutdown().await.unwrap();
    editor.stop().await;
}
