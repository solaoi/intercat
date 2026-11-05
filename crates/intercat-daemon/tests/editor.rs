use intercat_daemon::editor::{Config, Manager};
use serde_json::json;
use std::{fs, time::Duration};

fn fixture() -> (tempfile::TempDir, Manager, Config) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source/intercat_editor");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("__init__.py"), "").unwrap();
    fs::write(source.join("server.py"), r#"import json, time
from pathlib import Path
assert Path.cwd() == Path(__file__).resolve().parent.parent.parent
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
started=time.monotonic()
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_GET(self):
  body=json.dumps({'accepting':True,'modelLoaded':time.monotonic()-started>0.2}).encode()
  self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_POST(self):
  value=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  time.sleep(value.get('queueDelay',0))
  self.send_response(200);self.send_header('X-Intercat-Inference','started');self.end_headers();self.wfile.flush()
  time.sleep(value.get('delay',11 if json.loads(value.get('tail','{}')).get('report')=='slow report' else 0))
  body=json.dumps({'stages':{'decision':{'selected':' A'}},'inferenceMs':1}).encode()
  try: self.wfile.write(body)
  except (BrokenPipeError,ConnectionResetError): pass
server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True)
server.serve_forever()
"#).unwrap();
    let model = root.path().join("model");
    fs::create_dir(&model).unwrap();
    let manager =
        Manager::new(&root.path().join("source"), &root.path().join("editor.log")).unwrap();
    let config = Config {
        python: "/usr/bin/python3".into(),
        model,
    };
    (root, manager, config)
}

async fn ready(manager: &Manager) {
    let mut state = manager.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.borrow_and_update().phase != "ready" {
            state.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn inference_deadline_excludes_queue_time_but_stops_waiting_for_late_results() {
    let (_root, manager, config) = fixture();
    manager.configure(config).await.unwrap();
    ready(&manager).await;
    let queued = std::time::Instant::now();
    manager
        .decide(&json!({"queueDelay":1.4,"delay":0.05}))
        .await
        .unwrap();
    assert!(queued.elapsed() >= Duration::from_millis(1400));
    let started = std::time::Instant::now();
    let error = manager.decide(&json!({"delay":2.5})).await.unwrap_err();
    assert!(error.is::<intercat_daemon::editor::InferenceTimeout>());
    assert!(started.elapsed() < Duration::from_secs(2));
    manager.stop().await;
}

#[tokio::test]
async fn report_question_gate_waits_beyond_input_deadline_but_holds_after_ten_seconds() {
    let (_root, manager, config) = fixture();
    manager.configure(config).await.unwrap();
    ready(&manager).await;
    let start = std::time::Instant::now();
    manager.analyze_report(&json!({"delay":1.4})).await.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(1400));
    let report = intercat_daemon::reports::Report::new(
        "track".into(),
        "session".into(),
        uuid::Uuid::now_v7(),
        "item".into(),
        "slow report".into(),
    );
    let start = std::time::Instant::now();
    assert!(matches!(
        intercat_daemon::reports::analyze(&report, &manager).await,
        intercat_daemon::reports::Analysis::Review
    ));
    assert!(start.elapsed() >= Duration::from_secs(9));
    assert!(start.elapsed() < Duration::from_secs(11));
    manager.stop().await;
}

#[tokio::test]
async fn starts_loads_restarts_after_crash_and_stops_without_leaking_the_process() {
    let (_root, manager, config) = fixture();
    assert!(manager.decide(&json!({})).await.is_err());
    manager.configure(config).await.unwrap();
    ready(&manager).await;
    let first = manager.status();
    assert_eq!(
        manager.decide(&json!({})).await.unwrap()["stages"]["decision"]["selected"],
        " A"
    );
    unsafe {
        libc::kill(first.pid.unwrap() as i32, libc::SIGKILL);
    }
    let mut changes = manager.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            changes.changed().await.unwrap();
            let current = changes.borrow_and_update();
            if current.phase == "ready" && current.restart == 1 {
                break;
            }
        }
    })
    .await
    .unwrap();
    let last_pid = manager.status().pid.unwrap();
    assert_ne!(Some(last_pid), first.pid);
    manager.stop().await;
    assert_eq!(manager.status().phase, "stopped");
    assert_eq!(unsafe { libc::kill(last_pid as i32, 0) }, -1);
}

#[tokio::test]
async fn discards_an_inflight_answer_from_a_replaced_editor() {
    let (_root, manager, config) = fixture();
    manager.configure(config.clone()).await.unwrap();
    ready(&manager).await;
    let running = manager.clone();
    let request = tokio::spawn(async move { running.decide(&json!({"delay":0.5})).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    manager.configure(config).await.unwrap();
    ready(&manager).await;
    assert!(request.await.unwrap().is_err());
    manager.stop().await;
}

#[tokio::test]
async fn missing_saved_model_does_not_prevent_the_daemon_from_serving() {
    let (_root, manager, _) = fixture();
    manager
        .restore(json!({"python":"/not-installed/python","model":"/not-installed/model"}))
        .await;
    assert_eq!(manager.status().phase, "unavailable");
    assert!(!manager.status().process);
    assert!(manager.status().detail.is_some());
    manager.stop().await;
}

#[tokio::test]
async fn three_failed_restarts_stop_and_explicit_retry_loads_the_repaired_model() {
    let (root, manager, config) = fixture();
    let server = root.path().join("source/intercat_editor/server.py");
    let original = fs::read(&server).unwrap();
    fs::write(&server, "raise RuntimeError('damaged model')\n").unwrap();
    manager.configure(config.clone()).await.unwrap();
    let mut changes = manager.subscribe();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if changes.borrow_and_update().can_retry {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(manager.status().restart, 3);
    assert_eq!(manager.status().phase, "unavailable");
    assert!(!manager.status().process);
    let generation = manager.status().generation;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(manager.status().generation, generation);
    fs::write(server, original).unwrap();
    manager.restore(serde_json::to_value(config).unwrap()).await;
    ready(&manager).await;
    assert!(!manager.status().can_retry);
    manager.stop().await;
}

#[tokio::test]
async fn download_pause_survives_restart_and_completed_download_starts_the_editor() {
    let (root, manager, _) = fixture();
    fs::write(root.path().join("source/intercat_editor/model_store.py"), r#"import argparse,json,time
from pathlib import Path
assert Path.cwd() == Path(__file__).resolve().parent.parent.parent
p=argparse.ArgumentParser();p.add_argument('--root');root=Path(p.parse_args().root)
print(json.dumps({'phase':'downloading','received':5,'total':10,'file':'weights'}),flush=True)
while not (root/'finish').exists(): time.sleep(.02)
(root/'complete.json').write_text(json.dumps({'revision':'3e7677d7a40d348a3daba263a2b1c0aa41910710'}))
print(json.dumps({'phase':'complete','received':10,'total':10,'model':str(root/'model')}),flush=True)
"#).unwrap();
    let config = manager.download_config("/usr/bin/python3".into()).unwrap();
    manager.download(config.clone()).await.unwrap();
    let mut changes = manager.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        while changes.borrow_and_update().download.is_none() {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let pid = manager.status().pid.unwrap();
    manager.pause_download().await.unwrap();
    assert_eq!(manager.status().phase, "download_paused");
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    manager.stop().await;
    let restored =
        Manager::new(&root.path().join("source"), &root.path().join("editor.log")).unwrap();
    restored
        .restore(serde_json::to_value(&config).unwrap())
        .await;
    assert_eq!(restored.status().phase, "download_paused");
    fs::write(root.path().join("editor-model/finish"), "").unwrap();
    restored.download(config).await.unwrap();
    ready(&restored).await;
    restored.stop().await;
}
