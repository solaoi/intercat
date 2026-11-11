//! Request edits and timeline history keep the current writable destination.
use super::*;

#[tokio::test]
async fn a_taken_back_request_restores_on_the_current_writer_after_transfer() -> Result<()> {
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let word = future(&f).await?;
    let id = word["id"].as_str().unwrap().to_owned();
    let cut = f.journal.cut_input(Uuid::now_v7(), id.clone()).await?;
    assert_eq!(cut["state"], "cut");
    transfer(&f, &job).await?;
    f.journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Restore {
                input: id.clone(),
                revision: cut["revision"].as_i64().unwrap(),
            },
        })
        .await?;
    let restored = f.journal.read_utterance(id).await?;
    assert_eq!(restored["state"], "held");
    assert_eq!(restored["target"]["trackId"], job.successor_track);
    assert_eq!(restored["decision"]["trackId"], job.successor_track);
    assert_eq!(restored["freshRunnable"], false);
    assert_eq!(restored["text"], word["text"]);
    Ok(())
}

#[tokio::test]
async fn editing_a_future_request_during_transfer_keeps_the_draft_savable() -> Result<()> {
    use crate::edit_hold::{Action, Edit, Request};
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let word = future(&f).await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let editing = f
        .journal
        .edit_input(Request {
            owner,
            action: Action::Begin {
                token,
                input: word["id"].as_str().unwrap().into(),
                revision: word["revision"].as_i64().unwrap(),
            },
        })
        .await?;
    let error = crate::fresh_files::transfer(&f.journal, &job)
        .await
        .unwrap_err();
    assert!(error.is::<crate::fresh::Waiting>());
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert!(!crate::continuation::closed(&db, &job.request.track_id)?);
    assert!(crate::continuation::start(&db, &job.successor_track)?.is_none());
    assert_eq!(
        f.journal
            .read_utterance(word["id"].as_str().unwrap().into())
            .await?["revision"],
        editing["revision"]
    );
    let saved = f
        .journal
        .edit_input(Request {
            owner,
            action: Action::End {
                token,
                revision: editing["revision"].as_i64().unwrap(),
                edit: Edit::Text {
                    text: "Own revised future request".into(),
                    sources: None,
                },
            },
        })
        .await?;
    assert_eq!(saved["text"], "Own revised future request");
    assert!(saved["editHold"].is_null());
    transfer(&f, &job).await?;
    let moved = f
        .journal
        .read_utterance(word["id"].as_str().unwrap().into())
        .await?;
    assert_eq!(moved["target"]["trackId"], job.successor_track);
    assert_eq!(moved["text"], saved["text"]);
    Ok(())
}

#[tokio::test]
async fn a_held_request_keeps_undo_and_redo_on_the_current_writer() -> Result<()> {
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let word = future(&f).await?;
    let id = word["id"].as_str().unwrap().to_owned();
    f.journal
        .resolve_input(
            Uuid::now_v7(),
            id.clone(),
            word["revision"].as_i64().unwrap(),
            crate::decision::Resolution::Hold,
        )
        .await?;
    transfer(&f, &job).await?;
    let snapshot = f.journal.snapshot().await?;
    assert_eq!(snapshot["editHistory"]["undo"]["label"], "Hold");
    assert!(
        snapshot["editHistory"]["undo"]["blocked"].is_null(),
        "{:?}",
        snapshot["editHistory"]
    );
    let undo = f
        .journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Undo {
                entry: snapshot["editHistory"]["undo"]["entryId"]
                    .as_str()
                    .unwrap()
                    .into(),
            },
        })
        .await?;
    let restored = f.journal.read_utterance(id.clone()).await?;
    assert_eq!(restored["text"], word["text"]);
    assert_eq!(restored["state"], "placed");
    assert_eq!(restored["target"]["trackId"], job.successor_track);
    assert_eq!(restored["decision"]["trackId"], job.successor_track);
    assert_eq!(restored["freshRunnable"], false);
    assert!(undo["editHistory"]["redo"]["blocked"].is_null());
    f.journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Redo {
                entry: undo["editHistory"]["redo"]["entryId"]
                    .as_str()
                    .unwrap()
                    .into(),
            },
        })
        .await?;
    let redone = f.journal.read_utterance(id).await?;
    assert_eq!(redone["text"], word["text"]);
    assert_eq!(redone["state"], "held");
    assert_eq!(redone["target"]["trackId"], job.successor_track);
    assert_eq!(
        redone["revision"].as_i64().unwrap(),
        restored["revision"].as_i64().unwrap() + 1
    );
    Ok(())
}

#[tokio::test]
async fn a_single_job_source_keeps_its_current_version_when_a_restoration_is_undone_and_redone()
-> Result<()> {
    use super::plans::{current, plan};
    use crate::plan_store::{Action, Command, Generated};
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let initial = plan(&f, Some(&job.request.track_id)).await?;
    assert!(
        f.journal
            .begin_plan(initial.id, initial.generation.id, initial.rev)
            .await?
    );
    assert!(
        f.journal
            .generated_plan(
                initial.id,
                initial.generation.id,
                initial.rev,
                Generated::TimedOut,
                vec![]
            )
            .await?
    );
    let failed = current(&f, initial.id).await?;
    f.journal
        .plan_command(Command {
            op_id: Uuid::now_v7(),
            plan_id: failed.id,
            revision: failed.rev,
            action: Action::RunAsOne,
        })
        .await?;
    let input = f.journal.claim_input().await?.unwrap();
    f.journal
        .apply_input(
            input.id.clone(),
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(job.request.track_id.clone()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let cut = f
        .journal
        .cut_input(Uuid::now_v7(), input.id.clone())
        .await?;
    transfer(&f, &job).await?;
    let restored = f
        .journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Restore {
                input: input.id.clone(),
                revision: cut["revision"].as_i64().unwrap(),
            },
        })
        .await?;
    let word = f.journal.read_utterance(input.id.clone()).await?;
    assert_eq!(
        word["revision"],
        current(&f, initial.id).await?.source_revision
    );
    let undone = f
        .journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Undo {
                entry: restored["editHistory"]["undo"]["entryId"]
                    .as_str()
                    .unwrap()
                    .into(),
            },
        })
        .await?;
    assert_eq!(
        f.journal.read_utterance(input.id.clone()).await?["state"],
        "cut"
    );
    f.journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Redo {
                entry: undone["editHistory"]["redo"]["entryId"]
                    .as_str()
                    .unwrap()
                    .into(),
            },
        })
        .await?;
    let after = f.journal.read_utterance(input.id).await?;
    assert_eq!(after["state"], "held");
    assert_eq!(after["target"]["trackId"], job.successor_track);
    assert_eq!(after["freshRunnable"], false);
    assert_eq!(
        after["revision"],
        current(&f, initial.id).await?.source_revision
    );
    Ok(())
}
