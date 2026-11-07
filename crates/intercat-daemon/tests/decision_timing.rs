use intercat_daemon::{
    Result,
    decision::Target,
    editor::{Config, Manager},
    input::Processor,
    journal::Journal,
    secrets::Key,
};
use serde_json::{Value, json};
use std::{fs, sync::Arc, time::Duration};
use tokio::sync::Notify;
use uuid::Uuid;

#[tokio::test]
async fn request_time_includes_queueing_and_persists_success_reported_late_invalid_and_elapsed_deadline()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let module = root.path().join("source/intercat_editor");
    fs::create_dir_all(&module)?;
    fs::write(module.join("__init__.py"), "")?;
    fs::write(
        module.join("server.py"),
        r#"import json,math,time
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def respond(self,value):
  body=json.dumps(value).encode();self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_GET(self):self.respond({'accepting':True,'modelLoaded':True})
 def do_POST(self):
  request=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  if self.path=='/tokens':
   self.send_response(200);self.send_header('X-Intercat-Inference','started');self.end_headers();self.wfile.write(b'{"tokens":10}');return
  text=json.loads(request['tail'])['request']
  if text=='queued':time.sleep(1.4)
  self.send_response(200);self.send_header('X-Intercat-Inference','started');self.end_headers();self.wfile.flush()
  time.sleep(1.5 if text=='timeout' else .05)
  stages={stage['name']:{'selected':' D','logprobs':{label:0 if label==' D' else -5 for label in stage['candidates']},'probabilities':{label:1 if label==' D' else 0 for label in stage['candidates']}} for stage in request['stages']}
  for stage in stages.values():stage['logprobs']={label:math.log(max(probability,1e-300))*request['temperature'] for label,probability in stage['probabilities'].items()}
  body={'cachePrefix':False,'temperature':request['temperature'],'stages':{} if text=='invalid' else stages,'inferenceMs':1300 if text=='late' else 50,'roundTripMs':-99}
  try:self.wfile.write(json.dumps(body).encode())
  except (BrokenPipeError,ConnectionResetError):pass
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True);server.serve_forever()
"#,
    )?;
    let model = root.path().join("model");
    fs::create_dir(&model)?;
    let editor = Manager::new(&root.path().join("source"), &root.path().join("editor.log"))?;
    editor
        .configure(Config {
            python: "/usr/bin/python3".into(),
            model,
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !editor.status().model_loaded {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let path = root.path().join("events.sqlite");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone()).await?;
    let mut accepted = None;
    let mut measurements = Vec::new();
    for (text, state, outcome) in [
        ("queued", "placed", "complete"),
        ("late", "asked", "late"),
        ("invalid", "asked", "failed"),
        ("timeout", "asked", "late"),
    ] {
        let id = Uuid::now_v7();
        journal
            .stage_utterance(id, text.into(), Target::New, None, vec![])
            .await?;
        wake.notify_one();
        let word = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let snapshot = journal.snapshot().await?;
                let word = snapshot["utterances"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|word| word["id"] == id.to_string())
                    .unwrap();
                if word["state"] == state && word["reading"]["state"] == "complete" {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(word.clone());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        let db = rusqlite::Connection::open(&path)?;
        let rows: Vec<String> = db.prepare("SELECT payload FROM events WHERE kind='input.inference' AND json_extract(payload,'$.utteranceId')=?1 ORDER BY seq")?
            .query_map([id.to_string()], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
        assert_eq!(rows.len(), 1);
        let timing: Value = serde_json::from_str(&rows[0])?;
        measurements.push(json!({"case":text,"timing":timing}));
        assert_eq!(timing["outcome"], outcome);
        assert_eq!(timing["pass"], 1);
        assert!(timing["roundTripMs"].as_f64().unwrap() >= 0.0);
        assert!(timing.get("text").is_none());
        if text == "queued" {
            assert!(timing["roundTripMs"].as_f64().unwrap() >= 1400.0);
            assert_eq!(timing["inferenceMs"], 50.0);
            assert_eq!(word["decision"]["timedOut"], false);
            assert_eq!(
                word["decision"]["timings"][0]["roundTripMs"],
                timing["roundTripMs"]
            );
            accepted = Some((id, word["decision"].clone()));
        } else if text == "late" || text == "timeout" {
            assert_eq!(
                timing["inferenceMs"],
                if text == "late" { 1300.0 } else { 50.0 }
            );
            assert!(word["decision"].is_null());
            assert!(word["deadline"]["recommendation"].is_object());
        } else {
            assert!(timing["inferenceMs"].is_null());
        }
    }
    processor.shutdown().await?;
    editor.stop().await;
    drop(journal);
    let reopened = Journal::open(&path, Key::new([7; 32])).await?;
    let (id, decision) = accepted.unwrap();
    assert!(
        reopened.snapshot().await?["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|word| word["id"] == id.to_string() && word["decision"] == decision)
    );
    assert!(decision["timings"][0]["roundTripMs"].as_f64().unwrap() >= 1400.0);
    assert_ne!(decision["timings"][0]["roundTripMs"], json!(-99));
    assert!(decision["scores"].is_null());
    if let Some(path) = std::env::var_os("INTERCAT_TIMING_OUTPUT") {
        fs::write(path, serde_json::to_vec_pretty(&measurements)?)?;
    }
    Ok(())
}
