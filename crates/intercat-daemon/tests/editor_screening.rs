use intercat_daemon::{
    Result,
    decision::{Candidate, Decision, Kind, Outcome, Request, Target},
    editor::{Config, Manager},
    input::Processor,
    journal::Journal,
    reports::{Analysis, Report},
    secrets::{Key, Registration},
};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc, time::Duration};
use tokio::sync::Notify;
use uuid::Uuid;

const KNOWN: &str = "known-editor-secret";
const PROVIDER: &str = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");

async fn journal(root: &Path) -> Result<Journal> {
    let journal = Journal::open(root.join("events.sqlite"), Key::new([7; 32])).await?;
    rusqlite::Connection::open(root.join("events.sqlite"))?
        .execute("INSERT INTO msp_positions VALUES('s','t',NULL)", [])?;
    Ok(journal)
}

async fn register(journal: &Journal, value: &str) -> Result<()> {
    let key = Key::new([7; 32]);
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let catalog = journal
        .register_fingerprint(Registration {
            op_id: Uuid::now_v7(),
            track_id: "t".into(),
            session_id: "s".into(),
            name: "TOKEN".into(),
            byte_len: value.len(),
            tag: hex(&key.tag(value.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await
}

async fn wait_file(path: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

async fn fixture() -> Result<(tempfile::TempDir, Journal, Manager)> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("source/intercat_editor");
    fs::create_dir_all(&source)?;
    fs::write(source.join("__init__.py"), "")?;
    fs::write(
        source.join("server.py"),
        r#"import hashlib,json,math,threading,time
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
root=Path.cwd()
lock=threading.Lock()
decisions=0
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_GET(self):
  body=b'{"accepting":true,"modelLoaded":true}'
  self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_POST(self):
  global decisions
  request=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  with lock:
   with (root/'requests.jsonl').open('a') as f:f.write(json.dumps({'path':self.path,'request':request})+'\n')
   if self.path=='/decide':decisions+=1
   attempt=decisions
  if (root/'gated').exists():
   name='tokens' if self.path=='/tokens' else 'decide'
   (root/(name+'-received')).touch()
   while not (root/(name+'-headers')).exists():time.sleep(.01)
  self.send_response(200);self.send_header('X-Intercat-Inference','started');self.end_headers();self.wfile.flush()
  if self.path=='/tokens':
   tail=json.loads(request['text'])
   oversized=(root/'routing-budget').exists() and (bool(tail.get('recentDecisions')) or len(tail.get('tracks',[]))>4 or any('activity' in track for track in tail.get('tracks',[])) or any('text' in row or 'choices' in row or 'at' in row for key in ('previousUtterances','waitingCards') for row in tail.get(key,[])))
   body={'tokens':600 if oversized else 10}
  else:
   if (root/'gated').exists():
    while not (root/'decide-body').exists():time.sleep(.01)
   selected={};stages={}
   for stage in request['stages']:
    if any(selected.get(key) not in values for key,values in stage.get('when',{}).items()):continue
    labels=stage['candidates']
    choice=('9' if (root/'other-first').exists() and attempt==1 else '1') if stage['name']=='lane' else ' D' if stage['name']=='boundary' else '1' if stage['name']=='question' else labels[0]
    selected[stage['name']]=choice
    logs={label:0 if label==choice else -10 for label in labels};total=sum(math.exp(value) for value in logs.values())
    stages[stage['name']]={'selected':choice,'logprobs':logs,'probabilities':{label:math.exp(value)/total for label,value in logs.items()}}
   for stage in stages.values():stage['logprobs']={label:math.log(max(probability,1e-300))*request.get('temperature',1) for label,probability in stage['probabilities'].items()}
   body={'cachePrefix':False,'temperature':request.get('temperature',1),'stages':stages,'inferenceMs':1,'promptVersion':hashlib.sha256(request['prefix'].encode()).hexdigest(),'tailHash':hashlib.sha256(request['tail'].encode()).hexdigest()}
  try:self.wfile.write(json.dumps(body).encode())
  except (BrokenPipeError,ConnectionResetError):pass
server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True);server.serve_forever()
"#,
    )?;
    fs::create_dir(root.path().join("model"))?;
    let journal = journal(root.path()).await?;
    register(&journal, KNOWN).await?;
    let editor = Manager::new(&root.path().join("source"), &root.path().join("editor.log"))?;
    editor
        .configure(Config {
            python: "/usr/bin/python3".into(),
            model: root.path().join("model"),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !editor.status().model_loaded {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok((root, journal, editor))
}

fn calls(root: &Path) -> Result<Vec<Value>> {
    fs::read_to_string(root.join("requests.jsonl"))?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn assert_screened(value: &Value) {
    let text = value.to_string();
    assert!(!text.contains(KNOWN));
    assert!(!text.contains(PROVIDER));
}

#[tokio::test]
async fn complete_editor_request_preserves_json_and_checks_the_tail_beyond_one_megabyte()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = journal(root.path()).await?;
    register(&journal, KNOWN).await?;
    let original = json!({
        "prefix":format!("Header\n{KNOWN}\nKeep the final instruction."),
        "tail":json!({"request":format!("{}\n{KNOWN}\ntoken={PROVIDER}\nFinal instruction", "字".repeat(400_000)), "tracks":[{"title":format!("Review {KNOWN}"),"state":"waiting"}],"target":{"mode":"none"}}).to_string(),
        "stages":[{"name":"decision","prompt":"\nDecision:","candidates":[" A"," B"]}],
        "temperature":1.0,"cachePrefix":false
    });
    let screened = journal
        .screen_editor(original.clone(), intercat_daemon::editor::Priority::Input)
        .await?;
    assert_screened(screened.params());
    let tail: Value = serde_json::from_str(screened.params()["tail"].as_str().unwrap())?;
    assert!(
        tail["request"]
            .as_str()
            .unwrap()
            .starts_with(&"字".repeat(400_000))
    );
    assert!(
        tail["request"]
            .as_str()
            .unwrap()
            .ends_with("Final instruction")
    );
    assert_eq!(screened.params()["stages"], original["stages"]);
    assert_eq!(tail["target"]["mode"], "none");
    assert!(
        screened.params()["prefix"]
            .as_str()
            .unwrap()
            .ends_with("Keep the final instruction.")
    );
    drop(screened);
    let candidate = Candidate {
        id: "t".into(),
        label: "A".into(),
        title: format!("{}{KNOWN}", "x".repeat(65)),
        status: "waiting".into(),
        activity: None,
        running: None,
        planned: Vec::new(),
    };
    let request = Request::new("Continue", vec![candidate], Target::None, None)?;
    let screened = journal
        .screen_editor(request.wire, intercat_daemon::editor::Priority::Input)
        .await?;
    let tail: Value = serde_json::from_str(screened.params()["tail"].as_str().unwrap())?;
    let title = tail["tracks"][0]["title"].as_str().unwrap();
    assert!(!title.contains("known-editor"));
    assert!(title.chars().count() <= 80);
    drop(screened);
    for invalid in [
        json!({"tail":json!({"target":{"mode":"lock","trackId":KNOWN}}).to_string()}),
        json!({"stages":[{"name":KNOWN}]}),
        json!({(KNOWN):"a field"}),
        json!({"tail":"invalid JSON"}),
    ] {
        assert!(
            journal
                .screen_editor(invalid, intercat_daemon::editor::Priority::Input)
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn tokens_and_both_classification_passes_receive_only_screened_text() -> Result<()> {
    let (root, journal, editor) = fixture().await?;
    fs::write(root.path().join("other-first"), "")?;
    for index in 0..12 {
        let id = Uuid::now_v7();
        journal
            .stage_utterance(
                id,
                format!("Task {index} {} {KNOWN}", "x".repeat(20)),
                Target::New,
                None,
                vec![],
            )
            .await?;
        let input = journal.claim_input().await?.unwrap();
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
            .await?;
    }
    let selected = journal.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone()).await?;
    let id = Uuid::now_v7();
    let original = format!("Inspect\n{KNOWN}\ntoken={PROVIDER}\nKeep every file unchanged.");
    journal
        .stage_utterance(id, original.clone(), Target::None, Some(selected), vec![])
        .await?;
    wake.notify_one();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = journal.snapshot().await?;
            let word = snapshot["utterances"]
                .as_array()
                .unwrap()
                .iter()
                .find(|word| word["id"] == id.to_string())
                .unwrap();
            if word["state"] == "placed" {
                assert_eq!(word["text"], original);
                assert_eq!(word["decision"]["timings"].as_array().unwrap().len(), 2);
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    processor.shutdown().await?;
    editor.stop().await;
    let requests = calls(root.path())?;
    assert_eq!(requests.len(), 4);
    for call in &requests {
        assert_screened(call);
        let tail: Value = serde_json::from_str(if call["path"] == "/tokens" {
            call["request"]["text"].as_str().unwrap()
        } else {
            call["request"]["tail"].as_str().unwrap()
        })?;
        assert!(!tail["tracks"].as_array().unwrap().is_empty());
        assert!(
            tail["request"]
                .as_str()
                .unwrap()
                .ends_with("Keep every file unchanged.")
        );
    }
    Ok(())
}

#[tokio::test]
async fn summary_context_is_screened_and_reduced_after_candidate_count_without_losing_the_request()
-> Result<()> {
    use intercat_daemon::{
        clip_summaries::{Finish, Summary},
        clock::Stamp,
    };
    let (root, journal, editor) = fixture().await?;
    fs::write(root.path().join("routing-budget"), "")?;
    let mut tracks = Vec::new();
    for index in 0..12 {
        let id = Uuid::now_v7();
        journal
            .stage_utterance(
                id,
                format!("Work item {index}\n{KNOWN}\ntoken={PROVIDER}"),
                Target::New,
                None,
                vec![],
            )
            .await?;
        let pending = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                pending.id,
                pending.revision,
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
            .await?;
        let snapshot = journal.snapshot().await?;
        let word = snapshot["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|word| word["id"] == id.to_string())
            .unwrap();
        let track = word["decision"]["trackId"].as_str().unwrap().to_owned();
        let session = format!("session-{index}");
        for frame in [
            json!({"method":"item/updated","params":{"sessionId":session,"item":{
                "itemId":"answer","turnId":"turn","kind":"agentMessage","revision":1,
                "status":"completed","text":"raw-output-never-in-classification"}}}),
            json!({"method":"turn/completed","params":{"sessionId":session,"turnId":"turn","terminal":"completed"}}),
        ] {
            journal
                .try_msp(
                    track.clone(),
                    session.clone(),
                    Stamp::now(),
                    frame,
                    None,
                    None,
                )?
                .await??;
        }
        let ticket = journal.claim_clip_summary().await?.unwrap();
        journal
            .finish_clip_summary(
                ticket,
                Finish::Complete(Summary {
                    title: format!("Result {index}"),
                    summary: format!("Investigated retries. {KNOWN} token={PROVIDER}"),
                }),
            )
            .await?;
        tracks.push(track);
    }
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone()).await?;
    for (text, target, expected_track) in [
        (
            "Continue A, B and C with the existing work.",
            Target::None,
            tracks[0].clone(),
        ),
        (
            "Continue the explicitly selected destination.",
            Target::Lock(tracks[10].clone()),
            tracks[10].clone(),
        ),
    ] {
        let id = Uuid::now_v7();
        journal
            .stage_utterance(id, text.into(), target, Some(tracks[11].clone()), vec![])
            .await?;
        wake.notify_one();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let snapshot = journal.snapshot().await?;
                let word = snapshot["utterances"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|word| word["id"] == id.to_string())
                    .unwrap();
                if word["state"] == "placed" {
                    assert_eq!(word["text"], text);
                    assert_eq!(word["decision"]["trackId"], expected_track);
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
    }
    processor.shutdown().await?;
    editor.stop().await;
    let calls = calls(root.path())?;
    for call in &calls {
        assert_screened(call);
        let tail = call["request"][if call["path"] == "/tokens" {
            "text"
        } else {
            "tail"
        }]
        .as_str()
        .unwrap();
        assert!(tail.rfind("\"request\":").unwrap() > tail.rfind("\"tracks\":").unwrap());
        assert!(
            !call
                .to_string()
                .contains("raw-output-never-in-classification")
        );
    }
    for call in calls.iter().filter(|call| call["path"] == "/decide") {
        use sha2::{Digest, Sha256};
        let tail = call["request"]["tail"].as_str().unwrap();
        let text: Value = serde_json::from_str(tail)?;
        let stored: String = rusqlite::Connection::open(root.path().join("events.sqlite"))?
            .query_row(
                "SELECT decision FROM utterances WHERE text=?1",
                [text["request"].as_str().unwrap()],
                |row| row.get(0),
            )?;
        let stored: Value = serde_json::from_str(&stored)?;
        assert_eq!(
            stored["scores"]["tailHash"],
            Sha256::digest(tail.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
    }
    for text in [
        "Continue A, B and C with the existing work.",
        "Continue the explicitly selected destination.",
    ] {
        let tails = calls
            .iter()
            .map(|call| -> Result<_> {
                let tail = serde_json::from_str::<Value>(
                    call["request"][if call["path"] == "/tokens" {
                        "text"
                    } else {
                        "tail"
                    }]
                    .as_str()
                    .unwrap(),
                )?;
                Ok((call["path"].as_str().unwrap(), tail))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|(_, tail)| tail["request"] == text)
            .collect::<Vec<_>>();
        assert_eq!(tails.len(), 13);
        assert!(tails[..12].iter().all(|(path, _)| *path == "/tokens"));
        assert_eq!(tails[12].0, "/decide");
        let initial_count = if text.contains("explicitly") { 5 } else { 8 };
        for (index, (_, tail)) in tails[..9].iter().enumerate() {
            assert_eq!(
                tail["recentDecisions"].as_array().map_or(0, Vec::len),
                8 - index
            );
            assert_eq!(tail["tracks"].as_array().unwrap().len(), initial_count);
        }
        assert_eq!(tails[9].1["tracks"].as_array().unwrap().len(), 4);
        assert!(
            tails[9].1["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|track| track["activity"]["summary"].is_string())
        );
        assert!(
            tails[10].1["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|track| track.get("activity").is_none())
        );
        assert!(
            tails[10].1["previousUtterances"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row.get("text").is_none() && row.get("at").is_some())
        );
        assert!(
            tails[11].1["previousUtterances"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row.get("at").is_none())
        );
        assert_eq!(tails[11].1, tails[12].1);
        assert!(
            tails[0].1["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|track| {
                    track["activity"]["summary"]
                        == "Investigated retries. <secret:TOKEN> token=<redacted:api-key#1>"
                        && track["activity"]["title"]
                            .as_str()
                            .unwrap()
                            .starts_with("Result ")
                })
        );
        if text.contains("explicitly") {
            assert_eq!(tails[12].1["target"]["mode"], "lock");
            assert_eq!(tails[12].1["target"]["trackId"], tracks[10]);
        }
    }
    Ok(())
}

#[tokio::test]
async fn pasted_title_retains_its_full_source_for_fingerprints_registered_later() -> Result<()> {
    use intercat_daemon::request_sources::{Role, Source, Submission};
    let (root, journal, editor) = fixture().await?;
    let secret = "private-value-".repeat(12);
    let original = format!("{} {secret}", "x".repeat(20));
    let attachment = Uuid::now_v7();
    let file = root.path().join("source.txt");
    fs::write(&file, &original)?;
    journal.import_pasted_text(attachment, file).await?;
    let first = Uuid::now_v7();
    let saved = journal
        .stage_document(Submission {
            op_id: first,
            text: String::new(),
            target: Target::New,
            selected_track_id: None,
            attachment_ids: vec![],
            sources: Some(vec![Source {
                attachment_id: attachment,
                role: Role::Request,
                after: String::new(),
            }]),
        })
        .await?;
    assert_eq!(
        saved["text"],
        original.chars().take(120).collect::<String>()
    );
    let stored: String = rusqlite::Connection::open(root.path().join("events.sqlite"))?.query_row(
        "SELECT json_extract(record,'$.preview') FROM utterance_sources WHERE input=?1",
        [first.to_string()],
        |row| row.get(0),
    )?;
    assert_eq!(stored, original);
    register(&journal, &secret).await?;
    journal
        .refresh_pasted_text(intercat_daemon::attachment_screening::Request {
            op_id: Uuid::now_v7(),
            attachment_id: attachment,
        })
        .await?;
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone()).await?;
    for id in [first, Uuid::now_v7()] {
        if id != first {
            let snapshot = journal.snapshot().await?;
            assert_eq!(
                snapshot["tracks"][0]["title"],
                original.chars().take(80).collect::<String>()
            );
            let track = snapshot["tracks"][0]["id"].as_str().unwrap().to_owned();
            journal
                .stage_utterance(
                    id,
                    "Continue the task".into(),
                    Target::None,
                    Some(track),
                    vec![],
                )
                .await?;
            wake.notify_one();
        }
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let snapshot = journal.snapshot().await?;
                if snapshot["utterances"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|word| word["id"] == id.to_string() && word["state"] == "placed")
                {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
    }
    processor.shutdown().await?;
    editor.stop().await;
    let requests = calls(root.path())?;
    assert_eq!(requests.len(), 4);
    for request in requests {
        assert!(!request.to_string().contains("private-value-"));
    }
    drop(journal);
    let reopened = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    let stored: String = rusqlite::Connection::open(root.path().join("events.sqlite"))?.query_row(
        "SELECT title FROM tracks ORDER BY rowid LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(stored, original);
    assert_eq!(
        reopened.snapshot().await?["tracks"][0]["title"],
        original.chars().take(80).collect::<String>()
    );
    Ok(())
}

#[tokio::test]
async fn fingerprint_lease_spans_token_count_and_upload_but_not_inference() -> Result<()> {
    let (root, journal, editor) = fixture().await?;
    fs::write(root.path().join("gated"), "")?;
    let request = Request::new("Explain the fixture", vec![], Target::New, None)?;
    let prepared = journal
        .screen_editor(request.wire, intercat_daemon::editor::Priority::Input)
        .await?;
    let counting_editor = editor.clone();
    let counting = tokio::spawn(async move {
        let count = counting_editor
            .count_tokens(&prepared, intercat_daemon::editor::Priority::Input, None)
            .await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((count, prepared))
    });
    wait_file(&root.path().join("tokens-received")).await?;
    let writer = journal.clone();
    let mut registration =
        tokio::spawn(async move { register(&writer, "later-editor-secret").await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut registration)
            .await
            .is_err()
    );
    fs::write(root.path().join("tokens-headers"), "")?;
    let (count, prepared) = counting.await??;
    assert_eq!(count, 10);
    assert!(!registration.is_finished());
    let sending_editor = editor.clone();
    let sending = tokio::spawn(async move {
        sending_editor
            .decide(prepared, intercat_daemon::editor::Priority::Input, None)
            .await?
            .finish(Duration::from_millis(1200))
            .await
    });
    wait_file(&root.path().join("decide-received")).await?;
    assert!(!registration.is_finished());
    fs::write(root.path().join("decide-headers"), "")?;
    tokio::time::timeout(Duration::from_secs(2), registration).await???;
    assert!(!sending.is_finished());
    fs::write(root.path().join("decide-body"), "")?;
    sending.await??;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn report_analysis_screens_its_whole_request_before_http() -> Result<()> {
    let (root, journal, editor) = fixture().await?;
    let text = format!("Please answer\n{KNOWN}\ntoken={PROVIDER}\nWhich database should I use?");
    let report = Report::new(
        "t".into(),
        "s".into(),
        Uuid::now_v7(),
        "report".into(),
        text.clone(),
    );
    let analysis = intercat_daemon::reports::analyze_urgent(&report, &editor, &journal).await;
    assert!(matches!(analysis,Analysis::Questions(questions) if questions==vec![text]));
    editor.stop().await;
    let requests = calls(root.path())?;
    assert_eq!(requests.len(), 1);
    assert_screened(&requests[0]);
    Ok(())
}

#[tokio::test]
async fn cancelling_token_count_keeps_its_upload_lease_until_headers_arrive() -> Result<()> {
    let (root, journal, editor) = fixture().await?;
    fs::write(root.path().join("gated"), "")?;
    let request = Request::new("Explain the fixture", vec![], Target::New, None)?;
    let prepared = journal
        .screen_editor(request.wire, intercat_daemon::editor::Priority::Input)
        .await?;
    let counting_editor = editor.clone();
    let counting = tokio::spawn(async move {
        counting_editor
            .count_tokens(&prepared, intercat_daemon::editor::Priority::Input, None)
            .await
    });
    wait_file(&root.path().join("tokens-received")).await?;
    let writer = journal.clone();
    let mut registration =
        tokio::spawn(async move { register(&writer, "after-cancelled-token-count").await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut registration)
            .await
            .is_err()
    );
    counting.abort();
    assert!(counting.await.unwrap_err().is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut registration)
            .await
            .is_err()
    );
    fs::write(root.path().join("tokens-headers"), "")?;
    tokio::time::timeout(Duration::from_secs(2), registration).await???;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn original_timing_still_requires_review_when_the_sent_request_is_redacted() -> Result<()> {
    let (root, journal, editor) = fixture().await?;
    let create = Uuid::now_v7();
    journal
        .stage_utterance(create, "A task".into(), Target::New, None, vec![])
        .await?;
    let pending = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            pending.id,
            pending.revision,
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
        .await?;
    let processor =
        Processor::start(journal.clone(), editor.clone(), Arc::new(Notify::new())).await?;
    let id = Uuid::now_v7();
    let text = format!("Wait until token={PROVIDER} is available");
    journal
        .stage_utterance(id, text.clone(), Target::None, None, vec![])
        .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = journal.snapshot().await?;
            let word = snapshot["utterances"]
                .as_array()
                .unwrap()
                .iter()
                .find(|word| word["id"] == id.to_string())
                .unwrap();
            if word["state"] == "asked" {
                assert_eq!(word["review"]["stage"], "operation");
                assert_eq!(word["text"], text);
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    processor.shutdown().await?;
    editor.stop().await;
    let calls = calls(root.path())?;
    let sent = calls.iter().find(|call| call["path"] == "/decide").unwrap();
    let tail: Value = serde_json::from_str(sent["request"]["tail"].as_str().unwrap())?;
    assert_eq!(
        tail["request"],
        "Wait until token=<redacted:api-key#1> is available"
    );
    Ok(())
}
