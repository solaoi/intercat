use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Target},
    drafts,
    editor::Priority,
    editor_log::Query,
    input_readings::Ticket,
    journal::Journal,
    reports::{Analysis, Report},
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn journal() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    Ok((root, journal))
}
async fn reading(journal: &Journal, text: &str) -> Result<Ticket> {
    journal
        .stage_utterance(Uuid::now_v7(), text.into(), Target::None, None, vec![])
        .await?;
    let input = journal.claim_input().await?.unwrap();
    Ok(Ticket {
        input: input.id,
        revision: input.revision,
        index: input.reading_index,
    })
}
fn wire(request: &str, title: &str) -> Value {
    json!({"prefix":"Classify the input.","tail":json!({"request":request,"tracks":[{"title":title}]}).to_string()})
}
async fn scenes(journal: &Journal, track: &str, scene: &str) -> Result<Vec<Value>> {
    let page = journal
        .editor_log(Query {
            track_id: track.into(),
            before: None,
        })
        .await?;
    Ok(page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["scene"] == scene)
        .cloned()
        .collect())
}

#[tokio::test]
async fn only_masked_request_text_announces_and_repeated_screening_does_not_repeat() -> Result<()> {
    let (_root, journal) = journal().await?;
    let ticket = reading(&journal, "Contact dummy@example.invalid").await?;
    let params = wire("Contact dummy@example.invalid", "Ordinary work");
    for _ in 0..2 {
        let screened = journal
            .screen_input(ticket.clone(), params.clone(), Priority::Input)
            .await?;
        assert!(
            !screened.params()["tail"]
                .as_str()
                .unwrap()
                .contains("dummy@example.invalid")
        );
        assert_eq!(scenes(&journal, "any-track", "masked").await?.len(), 1);
    }
    let log = scenes(&journal, "any-track", "masked").await?;
    assert_eq!(log[0]["inputId"], ticket.input);
    assert!(log[0]["line"].as_str().unwrap().contains("masked"));
    assert!(!serde_json::to_string(&log)?.contains("dummy@example.invalid"));
    Ok(())
}

#[tokio::test]
async fn previews_context_fields_and_literal_replacement_labels_stay_silent() -> Result<()> {
    let (_root, journal) = journal().await?;
    journal
        .preview_privacy(vec!["Contact dummy@example.invalid".into()])
        .await?;
    let ticket = reading(&journal, "Use the saved <redacted:email#1> label.").await?;
    journal
        .screen_input(
            ticket,
            wire(
                "Use the saved <redacted:email#1> label.",
                "Context dummy@example.invalid",
            ),
            Priority::Input,
        )
        .await?;
    assert!(scenes(&journal, "any-track", "masked").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn stale_readings_do_not_announce_but_a_soft_deadline_keeps_the_same_request() -> Result<()> {
    let (_root, journal) = journal().await?;
    let old = reading(&journal, "Contact old@example.invalid").await?;
    journal.cut_input(Uuid::now_v7(), old.input.clone()).await?;
    journal
        .screen_input(
            old,
            wire("Contact old@example.invalid", ""),
            Priority::Input,
        )
        .await?;
    assert!(scenes(&journal, "any-track", "masked").await?.is_empty());
    let late = reading(&journal, "Contact new@example.invalid").await?;
    journal.expire_input(late.clone()).await?;
    journal
        .screen_input(
            late.clone(),
            wire("Contact new@example.invalid", ""),
            Priority::Input,
        )
        .await?;
    assert_eq!(
        scenes(&journal, "any-track", "masked").await?[0]["inputId"],
        late.input
    );
    Ok(())
}

#[tokio::test]
async fn pasted_redaction_is_saved_once_and_ordinary_file_attachments_do_not_announce() -> Result<()>
{
    let (root, journal) = journal().await?;
    let path = root.path().join("dummy.txt");
    std::fs::write(&path, "Contact dummy@example.invalid")?;
    journal
        .import_attachment(Uuid::now_v7(), path.clone())
        .await?;
    assert!(scenes(&journal, "any-track", "masked").await?.is_empty());
    let id = Uuid::now_v7();
    journal.import_pasted_text(id, path.clone()).await?;
    journal.import_pasted_text(id, path).await?;
    assert_eq!(
        journal.read_pasted_text(id).await?,
        "Contact dummy@example.invalid"
    );
    let log = scenes(&journal, "any-track", "masked").await?;
    assert_eq!(log.len(), 1);
    assert!(log[0]["line"].as_str().unwrap().contains("pasted text"));
    assert!(!serde_json::to_string(&log)?.contains("dummy@example.invalid"));
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(scenes(&journal, "any-track", "masked").await?, log);
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn failed_paste_save_does_not_announce_and_retry_records_once() -> Result<()> {
    use intercat_daemon::fault_injection::Request as Fault;
    let (root, journal) = journal().await?;
    journal
        .preview_privacy(vec!["dummy@example.invalid".into()])
        .await?;
    let path = root.path().join("dummy.txt");
    std::fs::write(&path, "dummy@example.invalid")?;
    journal.test_fault(Fault::Fill).await?;
    let id = Uuid::now_v7();
    assert!(journal.import_pasted_text(id, path.clone()).await.is_err());
    assert!(scenes(&journal, "any-track", "masked").await?.is_empty());
    journal.test_fault(Fault::Restore).await?;
    journal.import_pasted_text(id, path).await?;
    assert_eq!(scenes(&journal, "any-track", "masked").await?.len(), 1);
    Ok(())
}

async fn question(journal: &Journal) -> Result<(String, String, Value)> {
    let first = reading(journal, "Make a dummy file").await?;
    journal
        .apply_input(
            first.input,
            first.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: None,
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let state = journal.snapshot().await?;
    let track = state["tracks"][0]["id"].as_str().unwrap().to_owned();
    let report = Report::new(
        track.clone(),
        "dummy-worker".into(),
        Uuid::now_v7(),
        "report".into(),
        "Choose the file format.".into(),
    );
    journal.observe_report(report.clone()).await?;
    journal
        .analyze_report(report.id, 1, Analysis::Questions(vec![report.text]))
        .await?;
    let state = journal.snapshot().await?;
    Ok((
        state["session"]["id"].as_str().unwrap().into(),
        track,
        state["reportQuestions"][0].clone(),
    ))
}
fn reply_book(saved: &Value, question: &Value) -> Value {
    let mut book = saved["book"].clone();
    book["active"] = "reply".into();
    book["stack"] = json!(["work"]);
    book["entries"]["reply"] = json!({"text":"","target":{"mode":"none"},"attachments":[],"context":{"kind":"reply","question":question},"pending":null});
    book
}
fn save(session: &str, current: &Value, book: Value, op: Uuid) -> drafts::Save {
    serde_json::from_value(json!({"session":session,"revision":current["revision"],"parentOpId":current["opId"],"opId":op,"book":book})).unwrap()
}

#[tokio::test]
async fn opening_a_saved_reply_announces_once_and_typing_or_loading_does_not_repeat() -> Result<()>
{
    let (_root, journal) = journal().await?;
    let (session, track, q) = question(&journal).await?;
    let initial = journal.read_drafts(session.clone()).await?;
    let book = reply_book(&initial, &q);
    let op = Uuid::now_v7();
    let saved = journal
        .save_drafts(save(&session, &initial, book.clone(), op))
        .await?;
    assert_eq!(scenes(&journal, &track, "reply").await?.len(), 1);
    journal
        .save_drafts(save(&session, &initial, book.clone(), op))
        .await?;
    journal.read_drafts(session.clone()).await?;
    let mut book = book;
    book["entries"]["reply"]["text"] = "Plain text".into();
    journal
        .save_drafts(save(&session, &saved, book, Uuid::now_v7()))
        .await?;
    let log = scenes(&journal, &track, "reply").await?;
    assert_eq!(log.len(), 1);
    assert!(
        log[0]["detail"]
            .as_str()
            .unwrap()
            .contains("Nothing has been sent")
    );
    assert_eq!(
        journal.snapshot().await?["reportQuestions"][0]["state"],
        "open"
    );
    Ok(())
}

#[tokio::test]
async fn rejected_or_stale_reply_drafts_do_not_announce() -> Result<()> {
    let (_root, journal) = journal().await?;
    let (session, track, q) = question(&journal).await?;
    let initial = journal.read_drafts(session.clone()).await?;
    let mut edited = initial["book"].clone();
    edited["entries"]["work"]["text"] = "Other window".into();
    journal
        .save_drafts(save(&session, &initial, edited, Uuid::now_v7()))
        .await?;
    let result = journal
        .save_drafts(save(
            &session,
            &initial,
            reply_book(&initial, &q),
            Uuid::now_v7(),
        ))
        .await?;
    assert_eq!(result["status"], "conflict");
    assert!(scenes(&journal, &track, "reply").await?.is_empty());
    let current = journal.read_drafts(session.clone()).await?;
    let mut stale = q;
    stale["revision"] = 999.into();
    journal
        .save_drafts(save(
            &session,
            &current,
            reply_book(&current, &stale),
            Uuid::now_v7(),
        ))
        .await?;
    assert!(scenes(&journal, &track, "reply").await?.is_empty());
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn failed_reply_save_rolls_back_its_notice_and_retry_keeps_the_question_open() -> Result<()> {
    use intercat_daemon::fault_injection::Request as Fault;
    let (_root, journal) = journal().await?;
    let (session, track, q) = question(&journal).await?;
    let initial = journal.read_drafts(session.clone()).await?;
    let book = reply_book(&initial, &q);
    let op = Uuid::now_v7();
    journal.test_fault(Fault::Fill).await?;
    assert!(
        journal
            .save_drafts(save(&session, &initial, book.clone(), op))
            .await
            .is_err()
    );
    assert!(scenes(&journal, &track, "reply").await?.is_empty());
    journal.test_fault(Fault::Restore).await?;
    journal
        .save_drafts(save(&session, &initial, book, op))
        .await?;
    assert_eq!(scenes(&journal, &track, "reply").await?.len(), 1);
    assert_eq!(
        journal.snapshot().await?["reportQuestions"][0]["state"],
        "open"
    );
    Ok(())
}
