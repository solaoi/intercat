use intercat_daemon::{
    editor::{Config, Manager},
    journal::Journal,
    secrets::Key,
};
use serde_json::json;
use std::{fs, time::Duration};

async fn fixture() -> (tempfile::TempDir, Manager, Config, Journal) {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source/intercat_editor");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("__init__.py"), "").unwrap();
    fs::write(source.join("server.py"), r#"import json, math, time
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
  report=json.loads(value.get('tail','{}')).get('report')
  with open('requests.jsonl','a') as log:
   log.write(json.dumps({'priority':self.headers.get('X-Intercat-Priority'),'queueMs':int(self.headers.get('X-Intercat-Timeout-Ms','0'))})+'\n')
  if self.path=='/tokens':
   body=json.dumps({'tokens':23}).encode();self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body);return
  time.sleep(value.get('queueDelay',0))
  self.send_response(200);self.send_header('X-Intercat-Inference','started');self.end_headers();self.wfile.flush()
  time.sleep(value.get('delay',11 if json.loads(value.get('tail','{}')).get('report')=='slow report' else 0))
  body=json.dumps({'cachePrefix':False,'stages':{'decision':{'selected':' A'}},'inferenceMs':1}).encode()
  if report:
   self.server.reports[report]=self.server.reports.get(report,0)+1
   probabilities={'0':0.98,'1':0.01,'2':0.01}
   body=json.dumps({'cachePrefix':False,'stages':{'question':{'selected':'0','probabilities':probabilities,'logprobs':{key:math.log(p) for key,p in probabilities.items()}}}}).encode()
   if report=='retry report' and self.server.reports[report]==1:
    time.sleep(0.05)
    body=json.dumps({'preempted':True}).encode()
  try: self.wfile.write(body)
  except (BrokenPipeError,ConnectionResetError): pass
server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
server.reports={}
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
    let journal = Journal::open(&root.path().join("events.sqlite"), Key::new([7; 32]))
        .await
        .unwrap();
    (root, manager, config, journal)
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
    let (_root, manager, config, journal) = fixture().await;
    manager.configure(config).await.unwrap();
    ready(&manager).await;
    let request = journal
        .screen_editor(
            json!({"queueDelay":1.4,"delay":0.05}),
            intercat_daemon::editor::Priority::Input,
        )
        .await
        .unwrap();
    let queued = std::time::Instant::now();
    manager
        .decide(request, intercat_daemon::editor::Priority::Input, None)
        .await
        .unwrap()
        .finish(Duration::from_millis(1200))
        .await
        .unwrap();
    assert!(queued.elapsed() >= Duration::from_millis(1400));
    let request = journal
        .screen_editor(
            json!({"delay":2.5}),
            intercat_daemon::editor::Priority::Input,
        )
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let error = manager
        .decide(request, intercat_daemon::editor::Priority::Input, None)
        .await
        .unwrap()
        .finish(Duration::from_millis(1200))
        .await
        .unwrap_err();
    assert!(error.is::<intercat_daemon::editor::InferenceTimeout>());
    assert!(started.elapsed() < Duration::from_secs(2));
    manager.stop().await;
}

#[tokio::test]
async fn report_question_gate_waits_beyond_input_deadline_but_holds_after_ten_seconds() {
    let (_root, manager, config, journal) = fixture().await;
    manager.configure(config).await.unwrap();
    ready(&manager).await;
    let request = journal
        .screen_editor(
            json!({"delay":1.4}),
            intercat_daemon::editor::Priority::Input,
        )
        .await
        .unwrap();
    let start = std::time::Instant::now();
    manager
        .analyze_report(
            request,
            intercat_daemon::editor::Priority::Question,
            Duration::from_secs(10),
            None,
        )
        .await
        .unwrap();
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
        intercat_daemon::reports::analyze_urgent(&report, &manager, &journal).await,
        intercat_daemon::reports::Analysis::Review
    ));
    assert!(start.elapsed() >= Duration::from_secs(9));
    assert!(start.elapsed() < Duration::from_secs(11));
    manager.stop().await;
}

#[tokio::test]
async fn reports_retry_preemption_with_the_remaining_budget_and_keep_question_priority() {
    use intercat_daemon::{
        decision::{Decision, Kind, Outcome, Target},
        reports::{self, Analysis, Report},
    };
    let (root, manager, config, journal) = fixture().await;
    manager.configure(config).await.unwrap();
    ready(&manager).await;
    let mut report = Report::new(
        "unused".into(),
        "session".into(),
        uuid::Uuid::now_v7(),
        "item".into(),
        "retry report".into(),
    );
    assert!(matches!(
        reports::analyze_urgent(&report, &manager, &journal).await,
        Analysis::Clear
    ));
    journal
        .stage_utterance(
            uuid::Uuid::now_v7(),
            "Do the next task".into(),
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
                boundary: Some("now".into()),
                command: None,
                confidence: 1.0,
                inference_ms: 1.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await
        .unwrap();
    report.track_id = journal.snapshot().await.unwrap()["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    report.text = "clear report".into();
    assert!(matches!(
        reports::analyze_urgent(&report, &manager, &journal).await,
        Analysis::Clear
    ));
    let calls: Vec<serde_json::Value> = fs::read_to_string(root.path().join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        calls
            .iter()
            .map(|call| call["priority"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["question", "question", "question"]
    );
    assert!(calls[1]["queueMs"].as_u64().unwrap() < calls[0]["queueMs"].as_u64().unwrap());
    assert!(
        calls
            .iter()
            .all(|call| (1..=10000).contains(&call["queueMs"].as_u64().unwrap()))
    );
    manager.stop().await;
}

#[tokio::test]
async fn starts_loads_restarts_after_crash_and_stops_without_leaking_the_process() {
    let (_root, manager, config, journal) = fixture().await;
    assert!(
        manager
            .decide(
                journal
                    .screen_editor(json!({}), intercat_daemon::editor::Priority::Input)
                    .await
                    .unwrap(),
                intercat_daemon::editor::Priority::Input,
                None
            )
            .await
            .is_err()
    );
    manager.configure(config).await.unwrap();
    ready(&manager).await;
    let first = manager.status();
    assert_eq!(
        manager
            .decide(
                journal
                    .screen_editor(json!({}), intercat_daemon::editor::Priority::Input)
                    .await
                    .unwrap(),
                intercat_daemon::editor::Priority::Input,
                None
            )
            .await
            .unwrap()
            .finish(Duration::from_millis(1200))
            .await
            .unwrap()["stages"]["decision"]["selected"],
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
    let (_root, manager, config, journal) = fixture().await;
    manager.configure(config.clone()).await.unwrap();
    ready(&manager).await;
    let running = manager.clone();
    let screened = journal
        .screen_editor(
            json!({"delay":0.5}),
            intercat_daemon::editor::Priority::Input,
        )
        .await
        .unwrap();
    let request = tokio::spawn(async move {
        running
            .decide(screened, intercat_daemon::editor::Priority::Input, None)
            .await?
            .finish(Duration::from_millis(1200))
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    manager.configure(config).await.unwrap();
    ready(&manager).await;
    assert!(request.await.unwrap().is_err());
    manager.stop().await;
}

#[tokio::test]
async fn missing_saved_model_does_not_prevent_the_daemon_from_serving() {
    let (_root, manager, _, _journal) = fixture().await;
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
    let (root, manager, config, _journal) = fixture().await;
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
    let (root, manager, _, _journal) = fixture().await;
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
