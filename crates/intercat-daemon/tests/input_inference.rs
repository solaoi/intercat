use intercat_daemon::{
    Result,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    input_inference::{Evaluation, Match, Service},
    journal::Journal,
    secrets::Key,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    Ok((root, journal))
}

async fn place(
    journal: &Journal,
    text: &str,
    target: Target,
    track: Option<String>,
) -> Result<(String, String)> {
    let track = match &target {
        Target::Lock(track) => Some(track.clone()),
        _ => track,
    };
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, text.into(), target, None, vec![])
        .await?;
    classify(journal, id.to_string(), track).await
}

async fn classify(
    journal: &Journal,
    id: String,
    track: Option<String>,
) -> Result<(String, String)> {
    let pending = journal.claim_input().await?.unwrap();
    assert_eq!(pending.id, id);
    journal
        .apply_input(
            id.clone(),
            pending.revision,
            Outcome::Classified(Decision {
                kind: if track.is_some() {
                    Kind::Extend
                } else {
                    Kind::Fork
                },
                track_id: track,
                boundary: Some("now".into()),
                command: None,
                confidence: 1.0,
                inference_ms: 1.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let word = journal.read_utterance(id.clone()).await?;
    Ok((id, word["decision"]["trackId"].as_str().unwrap().into()))
}

async fn arrive(journal: &Journal, id: &str, track: &str) -> Result<()> {
    let revision = journal.read_utterance(id.into()).await?["revision"]
        .as_i64()
        .unwrap();
    let command = Uuid::now_v7();
    journal
        .reserve_turn(
            command,
            id.into(),
            revision,
            track.into(),
            json!({"sessionId":format!("session-{track}"),"commandId":command}),
        )
        .await?;
    journal.try_msp(track.into(),format!("session-{track}"),Stamp::now(),json!({"method":"item/updated","params":{"sessionId":format!("session-{track}"),"item":{"itemId":format!("user-{command}"),"commandId":command,"turnId":command,"kind":"userMessage","text":"Recorded test request","revision":1,"status":"completed"}}}),None,None)?.await??;
    assert_eq!(journal.read_utterance(id.into()).await?["state"], "landed");
    Ok(())
}

async fn settle(journal: &Journal) -> Result<()> {
    let service = Service::start(journal.clone());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while journal.next_inferred_correction().await?.is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await??;
    service.shutdown().await;
    Ok(())
}

fn inferred(root: &tempfile::TempDir) -> Result<Vec<Value>> {
    let db = Connection::open(root.path().join("db"))?;
    db.prepare("SELECT payload FROM events WHERE kind='utterance.corrected' AND json_extract(payload,'$.kind')='inferred' ORDER BY seq")?
        .query_map([],|row|row.get::<_,String>(0))?.map(|row|Ok(serde_json::from_str(&row?)?)).collect()
}

#[tokio::test]
async fn a_confirmed_resubmission_is_inferred_once_and_carried_into_next_context() -> Result<()> {
    let (root, journal) = fixture().await?;
    let (old, _) = place(&journal, "Ｃａｆé の表示を調整する", Target::None, None).await?;
    journal.cut_input(Uuid::now_v7(), old.clone()).await?;
    let (new, track) = place(&journal, "cafe\u{301} の表示を調整する", Target::None, None).await?;
    settle(&journal).await?;
    assert!(inferred(&root)?.is_empty());
    arrive(&journal, &new, &track).await?;
    settle(&journal).await?;
    let records = inferred(&root)?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["inputId"], old);
    assert_eq!(records[0]["replacementInputId"], new);
    assert_eq!(records[0]["distance"]["edits"], 0);
    assert_eq!(
        journal.read_utterance(old).await?["correction"],
        json!({"explicit":0,"inferred":1,"misread":false})
    );
    let next = Uuid::now_v7();
    journal
        .stage_utterance(
            next,
            "Continue the corrected request".into(),
            Target::None,
            None,
            vec![],
        )
        .await?;
    let pending = journal.claim_input().await?.unwrap();
    let context = pending
        .history
        .decisions
        .iter()
        .find(|row| row["title"] == "cafe\u{301} の表示を調整する")
        .unwrap();
    assert_eq!(context["correctionKind"], "inferred");
    assert_eq!(context["decision"]["worker"], "B");
    assert_eq!(context["previous"]["worker"], "A");
    settle(&journal).await?;
    assert_eq!(inferred(&root)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn same_destination_explicit_destination_and_different_text_do_not_count() -> Result<()> {
    for case in [
        "same",
        "explicit-new",
        "explicit-lock",
        "different",
        "asked",
    ] {
        let (root, journal) = fixture().await?;
        let (seed, seed_track) = place(&journal, "Seed worker", Target::None, None).await?;
        let target = match case {
            "explicit-new" => Target::New,
            "explicit-lock" => Target::Lock(seed_track),
            _ => Target::None,
        };
        let (old, track) = place(&journal, "abcdefghij", target, None).await?;
        if case == "asked" {
            let db = Connection::open(root.path().join("db"))?;
            intercat_daemon::clock::register(&db)?;
            db.execute("UPDATE utterances SET state='asked' WHERE id=?1", [&old])?;
        }
        journal.cut_input(Uuid::now_v7(), old.clone()).await?;
        let (new, new_track) = place(
            &journal,
            if case == "different" {
                "different request"
            } else {
                "abcdefghij"
            },
            Target::None,
            if case == "same" { Some(track) } else { None },
        )
        .await?;
        arrive(&journal, &new, &new_track).await?;
        settle(&journal).await?;
        assert!(inferred(&root)?.is_empty(), "{case}");
        assert!(journal.read_utterance(seed).await?["correction"].is_null());
    }
    Ok(())
}

#[tokio::test]
async fn the_thirty_second_limit_uses_put_time_and_not_delivery_time() -> Result<()> {
    for (elapsed, delivery_delay, expected) in [(30_000, 0, 1), (30_001, 0, 0), (10_000, 60_000, 1)]
    {
        let (root, journal) = fixture().await?;
        let (old, _) = place(&journal, "abcdefghij", Target::None, None).await?;
        journal.cut_input(Uuid::now_v7(), old).await?;
        let (new, track) = place(&journal, "abcdefghxy", Target::None, None).await?;
        let db = Connection::open(root.path().join("db"))?;
        let (mono,at):(i64,i64)=db.query_row("SELECT mono,ts FROM events WHERE kind='utterance.staged' AND json_extract(payload,'$.id')=?1",[&new],|row|Ok((row.get(0)?,row.get(1)?)))?;
        db.execute("UPDATE events SET mono=?1,ts=?2 WHERE kind='utterance.staged' AND json_extract(payload,'$.id')=?3", params![mono-delivery_delay,at-delivery_delay,new])?;
        db.execute(
            "UPDATE events SET mono=?1,ts=?2 WHERE kind='utterance.taken-back'",
            params![
                mono - delivery_delay - elapsed,
                at - delivery_delay - elapsed
            ],
        )?;
        arrive(&journal, &new, &track).await?;
        settle(&journal).await?;
        assert_eq!(inferred(&root)?.len(), expected, "{elapsed}");
    }
    Ok(())
}

#[tokio::test]
async fn restoring_a_taken_back_word_invalidates_an_in_flight_inference() -> Result<()> {
    let (root, journal) = fixture().await?;
    let (old, _) = place(&journal, "abcdefghij", Target::None, None).await?;
    journal.cut_input(Uuid::now_v7(), old.clone()).await?;
    let entry = journal.snapshot().await?["editHistory"]["undo"]["entryId"]
        .as_str()
        .unwrap()
        .to_owned();
    let (new, track) = place(&journal, "abcdefghij", Target::None, None).await?;
    arrive(&journal, &new, &track).await?;
    let ticket = journal.next_inferred_correction().await?.unwrap();
    assert_eq!(ticket.candidates.len(), 1);
    journal
        .edit_history(intercat_daemon::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: intercat_daemon::timeline_history::Action::Undo { entry },
        })
        .await?;
    assert!(
        !journal
            .finish_inferred_correction(
                ticket.clone(),
                Evaluation::Matched(Match {
                    sequence: ticket.candidates[0].sequence,
                    edits: 0,
                    characters: 10
                })
            )
            .await?
    );
    settle(&journal).await?;
    assert!(inferred(&root)?.is_empty());
    assert_ne!(journal.read_utterance(old).await?["state"], "cut");
    Ok(())
}

#[tokio::test]
async fn a_failed_save_and_duplicate_finish_do_not_consume_or_duplicate_the_source() -> Result<()> {
    let (root, journal) = fixture().await?;
    let (old, _) = place(&journal, "abcdefghij", Target::None, None).await?;
    journal.cut_input(Uuid::now_v7(), old).await?;
    let (new, track) = place(&journal, "abcdefghij", Target::None, None).await?;
    arrive(&journal, &new, &track).await?;
    let ticket = journal.next_inferred_correction().await?.unwrap();
    let result = Evaluation::Matched(Match {
        sequence: ticket.candidates[0].sequence,
        edits: 0,
        characters: 10,
    });
    let db = Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER refuse_inference BEFORE UPDATE OF done ON input_inference_jobs BEGIN SELECT RAISE(ABORT,'injected inference failure'); END;")?;
    assert!(
        journal
            .finish_inferred_correction(ticket.clone(), result.clone())
            .await
            .is_err()
    );
    assert!(inferred(&root)?.is_empty());
    assert!(journal.next_inferred_correction().await?.is_some());
    db.execute_batch("DROP TRIGGER refuse_inference;")?;
    let (a, b) = tokio::join!(
        journal.finish_inferred_correction(ticket.clone(), result.clone()),
        journal.finish_inferred_correction(ticket, result)
    );
    assert_ne!(a?, b?);
    assert_eq!(inferred(&root)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_pending_inference_survives_restart_and_a_second_resubmission_cannot_reuse_it()
-> Result<()> {
    let (root, journal) = fixture().await?;
    let (old, _) = place(&journal, "abcdefghij", Target::None, None).await?;
    journal.cut_input(Uuid::now_v7(), old.clone()).await?;
    let (new, track) = place(&journal, "abcdefghij", Target::None, None).await?;
    arrive(&journal, &new, &track).await?;
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    settle(&journal).await?;
    let (another, track) = place(&journal, "abcdefghij", Target::None, None).await?;
    arrive(&journal, &another, &track).await?;
    settle(&journal).await?;
    assert_eq!(inferred(&root)?.len(), 1);
    assert_eq!(
        journal.read_utterance(old).await?["correction"]["explicit"],
        0
    );
    Ok(())
}

#[tokio::test]
async fn text_edits_restart_the_submission_window_but_an_unchanged_edit_does_not() -> Result<()> {
    use intercat_daemon::edit_hold::{Action, Edit, Request};
    let (root, journal) = fixture().await?;
    let (old, _) = place(&journal, "abcdefghij", Target::None, None).await?;
    journal.cut_input(Uuid::now_v7(), old).await?;
    let (new, _) = place(&journal, "different text", Target::None, None).await?;
    let db = Connection::open(root.path().join("db"))?;
    for _ in 0..2 {
        let revision = journal.read_utterance(new.clone()).await?["revision"]
            .as_i64()
            .unwrap();
        let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
        let opened = journal
            .edit_input(Request {
                owner,
                action: Action::Begin {
                    token,
                    input: new.clone(),
                    revision,
                },
            })
            .await?;
        journal
            .edit_input(Request {
                owner,
                action: Action::End {
                    token,
                    revision: opened["revision"].as_i64().unwrap(),
                    edit: Edit::Text {
                        text: "abcdefghij".into(),
                        sources: None,
                    },
                },
            })
            .await?;
    }
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='utterance.reworded'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    let (mono,at):(i64,i64)=db.query_row("SELECT mono,ts FROM events WHERE kind='utterance.reworded' AND json_extract(payload,'$.id')=?1",[&new],|row|Ok((row.get(0)?,row.get(1)?)))?;
    db.execute(
        "UPDATE events SET mono=?1,ts=?2 WHERE kind='utterance.taken-back'",
        params![mono - 30_001, at - 30_001],
    )?;
    let (_, track) = classify(&journal, new.clone(), None).await?;
    arrive(&journal, &new, &track).await?;
    settle(&journal).await?;
    assert!(inferred(&root)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn pasted_request_text_is_compared_in_full_and_reference_text_is_excluded() -> Result<()> {
    use intercat_daemon::request_sources::{Role, Source, Submission};
    let (root, journal) = fixture().await?;
    let body = "同じ依頼の本文です。".repeat(300);
    let mut words = Vec::new();
    for index in 0..2 {
        let file = root.path().join(format!("request-{index}.txt"));
        std::fs::write(&file, &body)?;
        let attachment = journal.import_pasted_text(Uuid::now_v7(), file).await?;
        let reference_file = root.path().join(format!("reference-{index}.txt"));
        std::fs::write(&reference_file, format!("Unrelated reference {index}"))?;
        let reference = journal
            .import_pasted_text(Uuid::now_v7(), reference_file)
            .await?;
        let id = Uuid::now_v7();
        journal
            .stage_document(Submission {
                execute_allowed: true,
                plan_item: None,
                op_id: id,
                text: String::new(),
                target: Target::None,
                selected_track_id: None,
                attachment_ids: vec![],
                sources: Some(vec![
                    Source {
                        attachment_id: attachment.id,
                        role: Role::Request,
                        after: String::new(),
                    },
                    Source {
                        attachment_id: reference.id,
                        role: Role::Reference,
                        after: String::new(),
                    },
                ]),
            })
            .await?;
        let word = classify(&journal, id.to_string(), None).await?;
        if index == 0 {
            journal.cut_input(Uuid::now_v7(), word.0.clone()).await?;
        }
        words.push(word);
    }
    arrive(&journal, &words[1].0, &words[1].1).await?;
    settle(&journal).await?;
    let rows = inferred(&root)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["distance"]["edits"], 0);
    assert_eq!(rows[0]["distance"]["characters"], body.chars().count());
    Ok(())
}

#[tokio::test]
async fn another_daemon_boot_uses_recorded_wall_time_and_other_sessions_are_excluded() -> Result<()>
{
    for (elapsed, same_session, expected) in [(29_999, true, 1), (30_001, true, 0), (10, false, 0)]
    {
        let (root, journal) = fixture().await?;
        let (old, _) = place(&journal, "abcdefghij", Target::None, None).await?;
        journal.cut_input(Uuid::now_v7(), old).await?;
        let (new, track) = place(&journal, "abcdefghij", Target::None, None).await?;
        let db = Connection::open(root.path().join("db"))?;
        let at:i64=db.query_row("SELECT ts FROM events WHERE kind='utterance.staged' AND json_extract(payload,'$.id')=?1",[&new],|row|row.get(0))?;
        db.execute(
            "UPDATE events SET boot='previous-daemon',ts=?1 WHERE kind='utterance.taken-back'",
            [at - elapsed],
        )?;
        if !same_session {
            db.execute("UPDATE events SET payload=json_set(payload,'$.sessionId','other') WHERE kind='utterance.taken-back'",[])?;
        }
        arrive(&journal, &new, &track).await?;
        settle(&journal).await?;
        assert_eq!(inferred(&root)?.len(), expected);
    }
    Ok(())
}

#[tokio::test]
async fn redoing_take_back_records_its_new_revision_and_time() -> Result<()> {
    use intercat_daemon::timeline_history::{Action, Request};
    let (root, journal) = fixture().await?;
    let (old, _) = place(&journal, "abcdefghij", Target::None, None).await?;
    journal.cut_input(Uuid::now_v7(), old.clone()).await?;
    let snapshot = journal.snapshot().await?;
    let entry = snapshot["editHistory"]["undo"]["entryId"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .edit_history(Request {
            op_id: Uuid::now_v7(),
            action: Action::Undo {
                entry: entry.clone(),
            },
        })
        .await?;
    journal
        .edit_history(Request {
            op_id: Uuid::now_v7(),
            action: Action::Redo { entry },
        })
        .await?;
    let revision = journal.read_utterance(old.clone()).await?["revision"]
        .as_i64()
        .unwrap();
    let (new, track) = place(&journal, "abcdefghij", Target::None, None).await?;
    arrive(&journal, &new, &track).await?;
    settle(&journal).await?;
    let rows = inferred(&root)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["revision"], revision);
    Ok(())
}

#[tokio::test]
async fn unavailable_text_retries_without_a_false_inference_or_blocking_later_work() -> Result<()> {
    let (root, journal) = fixture().await?;
    let (old, _) = place(&journal, "abcdefghij", Target::None, None).await?;
    journal.cut_input(Uuid::now_v7(), old).await?;
    let (new, track) = place(&journal, "abcdefghij", Target::None, None).await?;
    arrive(&journal, &new, &track).await?;
    let ticket = journal.next_inferred_correction().await?.unwrap();
    assert!(
        journal
            .finish_inferred_correction(ticket.clone(), Evaluation::Unavailable)
            .await?
    );
    assert!(journal.next_inferred_correction().await?.is_none());
    assert!(inferred(&root)?.is_empty());
    let db = Connection::open(root.path().join("db"))?;
    assert_eq!(
        db.query_row(
            "SELECT done FROM input_inference_jobs WHERE input=?1",
            [&new],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    journal
        .finish_inferred_correction(ticket, Evaluation::Unavailable)
        .await?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='utterance.correction-unavailable'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    db.execute(
        "UPDATE input_inference_jobs SET retry_at=retry_at+60000 WHERE input=?1",
        [&new],
    )?;
    let (other, track) =
        place(&journal, "A separate unrelated request", Target::None, None).await?;
    arrive(&journal, &other, &track).await?;
    settle(&journal).await?;
    assert_eq!(
        db.query_row(
            "SELECT done FROM input_inference_jobs WHERE input=?1",
            [&other],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT done FROM input_inference_jobs WHERE input=?1",
            [&new],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    db.execute(
        "UPDATE input_inference_jobs SET retry_at=0 WHERE input=?1",
        [&new],
    )?;
    settle(&journal).await?;
    assert_eq!(inferred(&root)?.len(), 1);
    let (next, track) = place(&journal, "abcdefghij", Target::None, None).await?;
    arrive(&journal, &next, &track).await?;
    settle(&journal).await?;
    assert_eq!(inferred(&root)?.len(), 1);
    Ok(())
}
