use intercat_daemon::{
    Result,
    confirmation::{Accept, Action as PolicyAction, Change, Policy},
    decision::{Decision, Kind, Outcome, Resolution, Target},
    edit_hold::{Action, Edit, Request},
    journal::Journal,
    secrets::Key,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    Ok((root, journal))
}

async fn command(journal: &Journal, name: &str) -> Result<Value> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, format!("Open {name}"), Target::None, None, vec![])
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal.apply_input(input.id, input.revision, Outcome::Classified(Decision {
        kind: Kind::Command, command: Some(name.into()), track_id: None, boundary: None,
        confidence: 1.0, inference_ms: 1.0, timed_out: false,
        scores: json!({"presentation":{"command":"settings","operationId":Uuid::nil(),"receiptId":Uuid::nil(),"trackId":null}}),
    })).await?;
    journal.read_utterance(id.to_string()).await
}

async fn shown(journal: &Journal, word: &Value) -> Result<Value> {
    journal
        .resolve_input(
            word["presentation"]["receiptId"]
                .as_str()
                .unwrap()
                .parse()?,
            word["id"].as_str().unwrap().into(),
            word["revision"].as_i64().unwrap(),
            Resolution::Presented,
        )
        .await
}

#[tokio::test]
async fn application_commands_wait_for_display_and_keep_the_same_receipt_after_restart()
-> Result<()> {
    for name in [
        "commit",
        "push",
        "open-pr",
        "settings",
        "stage-mode",
        "branch-from",
        "restore",
    ] {
        let (root, journal) = fixture().await?;
        let word = command(&journal, name).await?;
        assert_eq!(word["state"], "placed");
        assert_eq!(word["presentation"]["command"], name);
        for field in ["operationId", "receiptId"] {
            assert_eq!(
                Uuid::parse_str(word["presentation"][field].as_str().unwrap())?.get_version_num(),
                7
            );
        }
        assert!(
            journal.snapshot().await?["tracks"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            journal.snapshot().await?["pushes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        drop(journal);
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let restored = journal
            .read_utterance(word["id"].as_str().unwrap().into())
            .await?;
        assert_eq!(restored["presentation"], word["presentation"]);
        let landed = shown(&journal, &restored).await?;
        assert_eq!(landed["state"], "landed");
        assert!(landed["presentation"].is_null());
        assert_eq!(shown(&journal, &restored).await?, landed);
    }
    Ok(())
}

#[tokio::test]
async fn a_foreign_receipt_or_cut_request_cannot_be_completed() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let word = command(&journal, "settings").await?;
    assert!(
        journal
            .resolve_input(
                Uuid::now_v7(),
                word["id"].as_str().unwrap().into(),
                1,
                Resolution::Presented
            )
            .await
            .is_err()
    );
    journal
        .cut_input(Uuid::now_v7(), word["id"].as_str().unwrap().into())
        .await?;
    assert!(shown(&journal, &word).await.is_err());
    assert_eq!(
        journal
            .read_utterance(word["id"].as_str().unwrap().into())
            .await?["state"],
        "cut"
    );
    Ok(())
}

#[tokio::test]
async fn editing_rejects_old_receipts_and_cancel_preserves_the_display_operation() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let word = command(&journal, "settings").await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let opened = journal
        .edit_input(Request {
            owner,
            action: Action::Begin {
                token,
                input: word["id"].as_str().unwrap().into(),
                revision: 1,
            },
        })
        .await?;
    assert!(shown(&journal, &word).await.is_err());
    assert!(shown(&journal, &opened).await.is_err());
    let closed = journal
        .edit_input(Request {
            owner,
            action: Action::End {
                token,
                revision: opened["revision"].as_i64().unwrap(),
                edit: Edit::Cancel,
            },
        })
        .await?;
    assert_eq!(closed["presentation"], word["presentation"]);
    assert_eq!(shown(&journal, &closed).await?["state"], "landed");
    Ok(())
}

#[tokio::test]
async fn a_failed_display_receipt_can_retry_without_reopening_work() -> Result<()> {
    let (root, journal) = fixture().await?;
    let word = command(&journal, "commit").await?;
    let db = Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER fail_shown BEFORE UPDATE ON utterances WHEN NEW.state='landed' BEGIN SELECT RAISE(ABORT,'simulated disk failure'); END;")?;
    assert!(shown(&journal, &word).await.is_err());
    let unchanged = journal
        .read_utterance(word["id"].as_str().unwrap().into())
        .await?;
    assert_eq!(unchanged["revision"], 1);
    assert_eq!(unchanged["presentation"], word["presentation"]);
    db.execute_batch("DROP TRIGGER fail_shown;")?;
    assert_eq!(shown(&journal, &word).await?["state"], "landed");
    Ok(())
}

#[tokio::test]
async fn confirmation_precedes_display_and_stopped_turns_allow_settings() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let unconfirmed = command(&journal, "settings").await?;
    assert_eq!(unconfirmed["state"], "placed");
    assert!(unconfirmed["presentation"].is_object());
    journal.pause_dispatch(Uuid::now_v7(), true).await?;
    assert_eq!(shown(&journal, &unconfirmed).await?["state"], "landed");
    journal.pause_dispatch(Uuid::now_v7(), false).await?;
    journal
        .change_confirmation(Change {
            op_id: Uuid::now_v7(),
            revision: 0,
            action: PolicyAction::Policy {
                policy: Policy::Always,
            },
        })
        .await?;
    let waiting = command(&journal, "settings").await?;
    assert_eq!(waiting["state"], "asked");
    assert!(waiting["presentation"].is_null());
    let accepted = journal
        .confirm_input(Accept {
            op_id: Uuid::now_v7(),
            utterance_id: waiting["id"].as_str().unwrap().parse()?,
            revision: 1,
            decision_id: waiting["confirmation"]["decisionId"]
                .as_str()
                .unwrap()
                .parse()?,
        })
        .await?;
    assert_eq!(accepted["state"], "placed");
    assert_eq!(accepted["presentation"]["command"], "settings");
    Ok(())
}

#[tokio::test]
async fn classification_save_failure_does_not_publish_a_display_request() -> Result<()> {
    let (root, journal) = fixture().await?;
    let db = Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER fail_classification BEFORE UPDATE ON utterances WHEN NEW.state='placed' BEGIN SELECT RAISE(ABORT,'simulated disk failure'); END;")?;
    assert!(command(&journal, "commit").await.is_err());
    let status = journal.snapshot().await?;
    let word = &status["utterances"][0];
    assert_eq!(word["state"], "reading");
    assert!(word["presentation"].is_null());
    Ok(())
}

#[tokio::test]
async fn held_display_actions_resume_only_by_button_with_their_original_operation() -> Result<()> {
    for name in ["settings", "branch-from", "restore"] {
        let (_root, journal) = fixture().await?;
        let word = command(&journal, name).await?;
        let held = journal
            .resolve_input(
                Uuid::now_v7(),
                word["id"].as_str().unwrap().into(),
                1,
                Resolution::Hold,
            )
            .await?;
        assert!(held["presentation"].is_null());
        let revision = journal
            .read_utterance(word["id"].as_str().unwrap().into())
            .await?["revision"]
            .as_i64()
            .unwrap();
        let resumed = journal
            .start_held_input(
                Uuid::now_v7(),
                word["id"].as_str().unwrap().into(),
                revision,
            )
            .await?;
        assert_eq!(resumed["presentation"], word["presentation"]);
        assert!(resumed["decision"]["boundary"].is_null());
        assert_eq!(shown(&journal, &resumed).await?["state"], "landed");
    }
    Ok(())
}
