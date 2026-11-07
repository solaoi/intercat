use intercat_daemon::{
    Result,
    decision::Target,
    edit_hold::{Action, Edit, Request},
    editor::{Config, Manager},
    input::Processor,
    journal::Journal,
    request_sources::{Role, Source, Submission},
    secrets::Key,
};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc, time::Duration};
use tokio::sync::Notify;
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    Ok((root, journal))
}
async fn paste(
    root: &Path,
    journal: &Journal,
    text: &str,
    role: Role,
    after: &str,
) -> Result<Source> {
    let id = Uuid::now_v7();
    let file = root.join(id.to_string());
    fs::write(&file, text)?;
    journal.import_pasted_text(id, file).await?;
    Ok(Source {
        attachment_id: id,
        role,
        after: after.into(),
    })
}
fn submission(id: Uuid, text: &str, sources: Vec<Source>) -> Submission {
    Submission {
        op_id: id,
        text: text.into(),
        target: Target::New,
        selected_track_id: None,
        attachment_ids: vec![],
        sources: Some(sources),
    }
}
async fn terminal(journal: &Journal, id: Uuid) -> Value {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let snapshot = journal.snapshot().await.unwrap();
            let word = snapshot["utterances"]
                .as_array()
                .unwrap()
                .iter()
                .find(|word| word["id"] == id.to_string())
                .unwrap();
            if !["staged", "reading"].contains(&word["state"].as_str().unwrap()) {
                return word.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}
async fn editor(root: &Path) -> Result<Manager> {
    let source = root.join("source/intercat_editor");
    fs::create_dir_all(&source)?;
    fs::write(source.join("__init__.py"), "")?;
    fs::write(
        source.join("server.py"),
        r#"import json,math,os
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
capture=os.path.join(os.path.dirname(__file__),'capture.jsonl')
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
  with open(capture,'a') as file:file.write(json.dumps({'path':self.path,'request':request})+'\n')
  if self.path=='/tokens':
   self.respond({'tokens':501 if 'OVER_BUDGET' in request['text'] else 50});return
  selected={};stages={}
  for stage in request['stages']:
   if any(selected.get(key) not in values for key,values in stage.get('when',{}).items()):continue
   labels=stage['candidates'];choice=labels[0];selected[stage['name']]=choice;n=len(labels)
   stages[stage['name']]={'selected':choice,'logprobs':{x:-0.1 if x==choice else -2 for x in labels},'probabilities':{x:1 if n==1 else .9 if x==choice else .1/(n-1) for x in labels}}
  self.respond({'cachePrefix':False,'stages':stages,'inferenceMs':1})
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True);server.serve_forever()
"#,
    )?;
    fs::create_dir(root.join("model"))?;
    let editor = Manager::new(&root.join("source"), &root.join("editor.log"))?;
    editor
        .configure(Config {
            python: "/usr/bin/python3".into(),
            model: root.join("model"),
        })
        .await?;
    Ok(editor)
}
#[tokio::test]
async fn ordered_sources_reach_the_editor_without_reference_body_or_unmasked_original() -> Result<()>
{
    let (root, journal) = fixture().await?;
    let secret = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    let later = "later-request-secret";
    let request = paste(
        root.path(),
        &journal,
        &format!("Keep this request\ntoken={secret}\n{later}"),
        Role::Request,
        "\nMIDDLE\n",
    )
    .await?;
    let key = Key::new([7; 32]);
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    rusqlite::Connection::open(root.path().join("db"))?.execute(
        "INSERT INTO msp_positions VALUES('screen-session','screen-track',NULL)",
        [],
    )?;
    let catalog = journal
        .register_fingerprint(intercat_daemon::secrets::Registration {
            op_id: Uuid::now_v7(),
            track_id: "screen-track".into(),
            session_id: "screen-session".into(),
            name: "TOKEN".into(),
            byte_len: later.len(),
            tag: hex(&key.tag(later.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    journal
        .refresh_pasted_text(intercat_daemon::attachment_screening::Request {
            op_id: Uuid::now_v7(),
            attachment_id: request.attachment_id,
        })
        .await?;
    let reference = paste(
        root.path(),
        &journal,
        "REFERENCE_BODY_MUST_STAY_OUT",
        Role::Reference,
        "\nDo not modify the files.",
    )
    .await?;
    let id = Uuid::now_v7();
    journal
        .stage_document(submission(id, "BEGIN\n", vec![request, reference]))
        .await?;
    let editor = editor(root.path()).await?;
    let processor =
        Processor::start(journal.clone(), editor.clone(), Arc::new(Notify::new())).await?;
    let word = terminal(&journal, id).await;
    assert_eq!(word["state"], "placed", "{word}");
    let captures = fs::read_to_string(root.path().join("source/intercat_editor/capture.jsonl"))?;
    assert!(!captures.contains(secret));
    assert!(!captures.contains(later));
    assert!(!captures.contains("REFERENCE_BODY_MUST_STAY_OUT"));
    let decide: Value = captures
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["path"] == "/decide")
        .unwrap();
    let tail: Value = serde_json::from_str(decide["request"]["tail"].as_str().unwrap())?;
    let text = tail["request"].as_str().unwrap();
    assert!(text.starts_with("BEGIN\nKeep this request"));
    assert!(text.ends_with("\nMIDDLE\n\nDo not modify the files."));
    assert_eq!(tail["hasAttachments"], true);
    assert_eq!(word["sources"].as_array().unwrap().len(), 2);
    processor.shutdown().await?;
    editor.stop().await;
    Ok(())
}
#[tokio::test]
async fn source_only_titles_and_replays_use_the_sealed_original() -> Result<()> {
    let (root, journal) = fixture().await?;
    let source = paste(
        root.path(),
        &journal,
        "Complete pasted request\nLast constraint",
        Role::Request,
        "",
    )
    .await?;
    let id = Uuid::now_v7();
    let saved = journal
        .stage_document(submission(id, "", vec![source.clone()]))
        .await?;
    assert_eq!(saved["text"], "Complete pasted request");
    assert_eq!(saved["sourceText"], "");
    assert_eq!(
        journal.snapshot().await?["session"]["name"],
        "Complete pasted request"
    );
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        journal
            .stage_document(submission(id, "", vec![source.clone()]))
            .await?,
        saved
    );
    let mut changed = source.clone();
    changed.role = Role::Reference;
    assert!(
        journal
            .stage_document(submission(id, "", vec![changed]))
            .await
            .is_err()
    );
    assert!(
        journal
            .stage_document(submission(
                Uuid::now_v7(),
                "x",
                vec![source.clone(), source]
            ))
            .await
            .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn only_references_ask_for_an_instruction_without_running_inference() -> Result<()> {
    let (root, journal) = fixture().await?;
    let source = paste(
        root.path(),
        &journal,
        "Reference material",
        Role::Reference,
        "",
    )
    .await?;
    let saved = journal
        .stage_document(submission(Uuid::now_v7(), "", vec![source]))
        .await?;
    assert_eq!(saved["state"], "asked");
    assert_eq!(saved["reason"], "What should I do with this?");
    assert!(journal.claim_input().await?.is_none());
    Ok(())
}
#[tokio::test]
async fn over_budget_paste_is_kept_whole_and_can_be_edited_into_a_reference() -> Result<()> {
    let (root, journal) = fixture().await?;
    let source = paste(
        root.path(),
        &journal,
        "OVER_BUDGET\nLast constraint",
        Role::Request,
        "",
    )
    .await?;
    let id = Uuid::now_v7();
    journal
        .stage_document(submission(id, "", vec![source.clone()]))
        .await?;
    let editor = editor(root.path()).await?;
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone()).await?;
    let word = terminal(&journal, id).await;
    assert_eq!(
        word["reason"], "Too long for the Editor · give it a one-line request",
        "{word}"
    );
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let begin = journal
        .edit_input(Request {
            owner,
            action: Action::Begin {
                token,
                input: id.to_string(),
                revision: 1,
            },
        })
        .await?;
    let revision = begin["editHold"]["revision"].as_i64().unwrap();
    let mut reference = source;
    reference.role = Role::Reference;
    let edit = Request {
        owner,
        action: Action::End {
            token,
            revision,
            edit: Edit::Text {
                text: "Explain the reference without changing files".into(),
                sources: Some(vec![reference]),
            },
        },
    };
    let saved = journal.edit_input(edit.clone()).await?;
    assert_eq!(saved["sources"][0]["role"], "reference");
    let replayed = journal.edit_input(edit.clone()).await?;
    for field in ["id", "revision", "text", "sources", "attachments", "target"] {
        assert_eq!(replayed[field], saved[field], "{field}");
    }
    wake.notify_one();
    let word = terminal(&journal, id).await;
    assert_eq!(word["state"], "placed", "{word}");
    assert_eq!(word["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(journal.edit_input(edit).await?, word);
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    processor.shutdown().await?;
    editor.stop().await;
    Ok(())
}
#[tokio::test]
async fn source_edits_reject_other_references_and_atomically_remove_old_bindings() -> Result<()> {
    let (root, journal) = fixture().await?;
    let source = paste(root.path(), &journal, "Original", Role::Request, "").await?;
    let other = paste(root.path(), &journal, "Other", Role::Reference, "").await?;
    let id = Uuid::now_v7();
    journal
        .stage_document(submission(id, "", vec![source]))
        .await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let begin = journal
        .edit_input(Request {
            owner,
            action: Action::Begin {
                token,
                input: id.to_string(),
                revision: 1,
            },
        })
        .await?;
    let revision = begin["editHold"]["revision"].as_i64().unwrap();
    assert!(
        journal
            .edit_input(Request {
                owner,
                action: Action::End {
                    token,
                    revision,
                    edit: Edit::Text {
                        text: "changed".into(),
                        sources: Some(vec![other])
                    }
                }
            })
            .await
            .is_err()
    );
    let word = journal
        .edit_input(Request {
            owner,
            action: Action::End {
                token,
                revision,
                edit: Edit::Text {
                    text: "Plain replacement".into(),
                    sources: Some(vec![]),
                },
            },
        })
        .await?;
    assert_eq!(word["text"], "Plain replacement");
    assert!(word["sources"].is_null());
    assert_eq!(word["attachments"], json!([]));
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn a_full_database_cannot_partially_bind_pasted_sources_and_same_id_retries() -> Result<()> {
    use intercat_daemon::fault_injection::Request as Fault;
    let (root, journal) = fixture().await?;
    let source = paste(
        root.path(),
        &journal,
        "Immutable pasted request",
        Role::Request,
        "end",
    )
    .await?;
    let id = Uuid::now_v7();
    journal.test_fault(Fault::Fill).await?;
    assert!(
        journal
            .stage_document(submission(id, "prefix", vec![source.clone()]))
            .await
            .is_err()
    );
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    for table in ["utterances", "utterance_sources", "input_attachments"] {
        let count: i64 = db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
        assert_eq!(count, 0, "{table}");
    }
    journal.test_fault(Fault::Restore).await?;
    let saved = journal
        .stage_document(submission(id, "prefix", vec![source.clone()]))
        .await?;
    assert_eq!(
        journal
            .stage_document(submission(id, "prefix", vec![source]))
            .await?,
        saved
    );
    Ok(())
}

#[tokio::test]
async fn unfinished_paste_metadata_survives_draft_restart_without_storing_raw_body() -> Result<()> {
    let (root, journal) = fixture().await?;
    let initial = journal.read_drafts("s".into()).await?;
    let mut book = initial["book"].clone();
    let id = Uuid::now_v7();
    book["entries"]["work"]["text"] = "Before".into();
    book["entries"]["work"]["sources"] =
        json!([{ "id":id,"role":"reference","after":"After","words":2200,"attachment":null }]);
    let save = serde_json::from_value(
        json!({"session":"s","revision":initial["revision"],"parentOpId":initial["opId"],"opId":Uuid::now_v7(),"book":book}),
    )?;
    journal.save_drafts(save).await?;
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(journal.read_drafts("s".into()).await?["book"], book);
    Ok(())
}
