use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Target},
    edit_hold::{Action, Edit, Request},
    journal::Journal,
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn outcome(kind: Kind) -> Outcome {
    Outcome::Classified(Decision {
        kind,
        track_id: None,
        boundary: Some("now".into()),
        command: None,
        confidence: 1.0,
        inference_ms: 1.0,
        timed_out: false,
        scores: json!({}),
    })
}

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    Ok((root, journal))
}

async fn place(journal: &Journal, text: &str, kind: Kind) -> Result<String> {
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, text.into(), Target::None, None, vec![])
        .await?;
    let pending = journal.claim_input().await?.unwrap();
    assert_eq!(pending.id, id.to_string());
    journal
        .apply_input(pending.id.clone(), pending.revision, outcome(kind))
        .await?;
    Ok(pending.id)
}

async fn word(journal: &Journal, id: &str) -> Result<Value> {
    Ok(journal.snapshot().await?["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["id"] == id)
        .unwrap()
        .clone())
}

#[tokio::test]
async fn rereads_only_the_latest_three_parks_and_replaces_obsolete_readings() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let mut parks = Vec::new();
    for i in 0..5 {
        parks.push(place(&journal, &format!("idea {i}"), Kind::Park).await?);
    }
    let context = place(&journal, "new context", Kind::Fork).await?;
    let mut old = Vec::new();
    for id in parks.iter().rev().take(3) {
        let pending = journal.claim_reread().await?.unwrap();
        assert_eq!(&pending.ticket.input, id);
        assert_eq!(pending.context.as_ref().unwrap().id, context);
        assert_eq!(word(&journal, id).await?["state"], "placed");
        old.push(pending.ticket);
    }
    assert!(journal.claim_reread().await?.is_none());
    let next = Uuid::now_v7();
    journal
        .stage_utterance(next, "newest context".into(), Target::None, None, vec![])
        .await?;
    assert!(journal.claim_reread().await?.is_none());
    for ticket in &old {
        assert!(!journal.reread_current(ticket.clone()).await?);
        journal
            .apply_reread(ticket.clone(), outcome(Kind::Fork))
            .await?;
    }
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let pending = journal.claim_input().await?.unwrap();
    journal
        .apply_input(pending.id, pending.revision, outcome(Kind::Fork))
        .await?;
    let fresh = journal.claim_reread().await?.unwrap();
    assert_eq!(fresh.ticket.input, old[0].input);
    assert_eq!(fresh.ticket.revision, old[0].revision);
    assert!(fresh.ticket.index > old[0].index);
    journal
        .apply_reread(fresh.ticket.clone(), outcome(Kind::Fork))
        .await?;
    journal
        .apply_reread(fresh.ticket, outcome(Kind::Fork))
        .await?;
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn automatic_rereading_keeps_ambiguity_parked_without_a_question() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let id = place(&journal, "unclear idea", Kind::Park).await?;
    place(&journal, "more context", Kind::Fork).await?;
    let pending = journal.claim_reread().await?.unwrap();
    journal
        .apply_reread(
            pending.ticket,
            Outcome::Asked("Please choose a track".into()),
        )
        .await?;
    let value = word(&journal, &id).await?;
    assert_eq!(value["state"], "placed");
    assert_eq!(value["decision"]["kind"], "park");
    assert_eq!(value["reading"]["state"], "complete");
    assert!(journal.claim_reread().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn a_locked_park_has_no_dispatch_destination_and_survives_stop_all() -> Result<()> {
    let (_root, journal) = fixture().await?;
    place(&journal, "work", Kind::Fork).await?;
    let track = journal.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = Uuid::now_v7();
    journal
        .stage_utterance(
            id,
            "keep this idea".into(),
            Target::Lock(track.clone()),
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    let mut decision = match outcome(Kind::Park) {
        Outcome::Classified(decision) => decision,
        _ => unreachable!(),
    };
    decision.track_id = Some(track.clone());
    journal
        .apply_input(input.id, input.revision, Outcome::Classified(decision))
        .await?;
    assert!(word(&journal, &id.to_string()).await?["decision"]["trackId"].is_null());
    assert!(
        journal
            .reserve_turn(
                Uuid::now_v7(),
                id.to_string(),
                1,
                track,
                json!({"sessionId":"session"}),
                None,
            )
            .await
            .is_err()
    );
    journal.pause_dispatch(Uuid::now_v7(), true).await?;
    assert_eq!(word(&journal, &id.to_string()).await?["state"], "placed");
    assert_eq!(
        word(&journal, &id.to_string()).await?["decision"]["kind"],
        "park"
    );
    Ok(())
}

#[tokio::test]
async fn opening_and_closing_an_old_park_requests_a_bounded_reread() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let mut parks = Vec::new();
    for i in 0..5 {
        parks.push(place(&journal, &format!("idea {i}"), Kind::Park).await?);
    }
    place(&journal, "context", Kind::Fork).await?;
    let old = &parks[0];
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let opened = journal
        .edit_input(Request {
            owner,
            action: Action::Begin {
                token,
                input: old.clone(),
                revision: 1,
            },
        })
        .await?;
    assert!(opened["editHold"].is_object());
    journal
        .edit_input(Request {
            owner,
            action: Action::End {
                token,
                revision: 2,
                edit: Edit::Cancel,
            },
        })
        .await?;
    let pending = journal.claim_reread().await?.unwrap();
    assert_eq!(&pending.ticket.input, old);
    let mut count = 1;
    while journal.claim_reread().await?.is_some() {
        count += 1;
    }
    assert_eq!(count, 3);
    Ok(())
}

#[tokio::test]
async fn restart_changes_the_reading_identity_and_a_cut_cannot_be_revived() -> Result<()> {
    let (root, journal) = fixture().await?;
    let id = place(&journal, "idea", Kind::Park).await?;
    place(&journal, "context", Kind::Fork).await?;
    let old = journal.claim_reread().await?.unwrap().ticket;
    drop(journal);
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    let fresh = journal.claim_reread().await?.unwrap().ticket;
    assert!(fresh.index > old.index);
    journal.apply_reread(old, outcome(Kind::Fork)).await?;
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    journal.cut_input(Uuid::now_v7(), id.clone()).await?;
    journal.apply_reread(fresh, outcome(Kind::Fork)).await?;
    assert_eq!(word(&journal, &id).await?["state"], "cut");
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn a_failed_reread_transaction_can_retry_without_duplicating_a_track() -> Result<()> {
    use intercat_daemon::fault_injection::Request as Fault;
    let (_root, journal) = fixture().await?;
    let id = place(&journal, "idea", Kind::Park).await?;
    place(&journal, "context", Kind::Fork).await?;
    let ticket = journal.claim_reread().await?.unwrap().ticket;
    journal.test_fault(Fault::Fill).await?;
    assert!(
        journal
            .apply_reread(ticket.clone(), outcome(Kind::Fork))
            .await
            .is_err()
    );
    journal.test_fault(Fault::Restore).await?;
    assert_eq!(word(&journal, &id).await?["decision"]["kind"], "park");
    assert!(journal.reread_current(ticket.clone()).await?);
    journal
        .apply_reread(ticket.clone(), outcome(Kind::Fork))
        .await?;
    journal.apply_reread(ticket, outcome(Kind::Fork)).await?;
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    Ok(())
}
