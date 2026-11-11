//! Named world sequences use product review, file and journal effects. The
//! worker boundary is already idle; native SDK Stop is accepted separately.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn db(f: &Fixture) -> Result<rusqlite::Connection> {
    Ok(rusqlite::Connection::open(
        f.root.path().join("events.sqlite"),
    )?)
}
pub(super) fn records(f: &Fixture, track: &str) -> Result<Vec<crate::integration_record::Record>> {
    db(f)?
        .prepare("SELECT record FROM integration_records WHERE track=?1 ORDER BY ordinal")?
        .query_map([track], |row| row.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str(&raw?)?))
        .collect()
}
pub(super) fn cancelled(f: &Fixture) -> Result<BTreeSet<String>> {
    Ok(db(f)?
        .prepare("SELECT record FROM integration_cancelled ORDER BY record")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}
pub(super) fn files(f: &Fixture, job: &Job) -> Result<BTreeMap<String, String>> {
    let head = &job.candidate.as_ref().unwrap().head;
    git(&f.source, &["ls-tree", "-r", "--name-only", head])?
        .lines()
        .map(|path| {
            Ok((
                path.to_owned(),
                git(&f.source, &["show", &format!("{head}:{path}")])?,
            ))
        })
        .collect()
}
pub(super) async fn checked_choice(f: &Fixture, index: usize) -> Result<Job> {
    let opened = f
        .action(Action::Open {
            tracks: Some(vec![f.workspaces[index].track_id.clone()]),
        })
        .await?;
    assert_eq!(opened.status, Status::Choosing);
    let chosen = f
        .action(Action::ChoosePath {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[index].track_id.clone(),
        })
        .await?;
    assert!(
        matches!(chosen.status, Status::Open | Status::MergeConflict),
        "Own world choice {index}: status {:?}, state {:?}, error {:?}",
        chosen.status,
        chosen.path_state,
        chosen.error
    );
    f.action(Action::Prepare {
        id: chosen.id,
        revision: chosen.revision,
    })
    .await?;
    let checked = f.settled(chosen.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    Ok(checked)
}
pub(super) async fn publish_idle(f: &Fixture, checked: &Job) -> Result<Job> {
    let committing = f
        .journal
        .integration_command(ReviewCommand {
            op_id: Uuid::now_v7(),
            action: commit(checked),
        })
        .await?;
    assert_eq!(committing.status, Status::Committing);
    let repo = intercat_git::Repository::open(&f.source)?;
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    for entry in entries
        .into_iter()
        .filter(|entry| entry.job == committing.id)
    {
        let snapshot = f.journal.snapshot().await?;
        let mut worker = snapshot["workers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|worker| worker["trackId"] == entry.track)
            .cloned()
            .map(serde_json::from_value::<WorkerState>)
            .transpose()?;
        if let Some(worker) = &mut worker {
            assert!(
                worker.active_turn.is_none()
                    && !worker.checkpoint_pending
                    && worker.approvals.is_empty()
                    && worker.prompts.is_empty()
            );
            worker.phase = "held".into();
            f.journal.worker_state(worker.clone()).await?;
        }
        // No process Stop is issued against the fixture's synthetic host PID.
        // These are the same durable/ref steps after the confirmed idle stop.
        f.journal
            .change_path_retirement(crate::path_retirement::Change::Freeze {
                id: entry.id,
                session: worker.as_ref().and_then(|worker| worker.session_id.clone()),
                checkpoint: worker.and_then(|worker| worker.checkpoint),
            })
            .await?;
        let entry = crate::path_retirement::read(&db(f)?, entry.id)?;
        let saved = entry.saved.as_ref().unwrap();
        repo.compare_and_swap(&saved.reference, None, saved.sha.parse()?)?;
        repo.sync_ref(&saved.reference)?;
        f.journal
            .change_path_retirement(crate::path_retirement::Change::Ready(entry.id))
            .await?;
    }
    f.review.recover().await?;
    let done = f.settled(checked.id).await?;
    assert_eq!(done.status, Status::Done, "{:?}", done.error);
    Ok(done)
}
pub(super) async fn realign(
    f: &Fixture,
    index: usize,
    point: Checkpoint,
    target: &Job,
) -> Result<Checkpoint> {
    let mut worker = f.worker(index, point).await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        track_id: f.workspaces[index].track_id.clone(),
        source: Source::Commit {
            job_id: target.id,
            revision: target.revision,
        },
    };
    let applied = f
        .apply_files(index, &request)
        .await?
        .ok_or("Missing own world realignment")?;
    worker.checkpoint = Some(applied.checkpoint.clone());
    f.journal.worker_state(worker).await?;
    f.journal
        .change_realign(crate::realign_store::Change::Complete(applied.operation))
        .await?;
    assert_eq!(
        crate::lineage_store::basis(&db(f)?, &f.workspaces[index].track_id)?.base,
        target.candidate.as_ref().unwrap().head
    );
    Ok(applied.checkpoint)
}
pub(super) async fn restore(f: &Fixture, index: usize) -> Result<crate::path_restore::Job> {
    use crate::path_restore_store::Change;
    let track = &f.workspaces[index].track_id;
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    let entry = entries
        .into_iter()
        .find(|entry| entry.track == *track && entry.phase == crate::path_retirement::Phase::Aside)
        .ok_or("Missing own set-aside world path")?;
    let words = f.journal.snapshot().await?["utterances"].clone();
    let request = crate::path_restore::Request {
        op_id: Uuid::now_v7(),
        retirement_id: entry.id,
    };
    f.journal.restore_path(request.clone()).await?;
    let identity = f._host.identity()?;
    let tracker = Arc::new(Mutex::new(crate::process::Tracker::new(
        format!("own-world-restore-{}", entry.id),
        identity,
    )?));
    let gate = Arc::new(crate::dispatch_control::Gate::new(false));
    let mut applied = None;
    for attempt in 0..4 {
        let boundary =
            crate::path_restore_apply::Boundary::Live(crate::inheritance_apply::Boundary {
                guard: crate::inheritance_apply::Guard::Recovery {
                    generation: Arc::new(AtomicU64::new(0)),
                    expected: 0,
                    epoch: gate.epoch(),
                    gate: gate.clone(),
                },
                tracker: tracker.clone(),
                identity,
            });
        match Box::pin(crate::path_restore_apply::apply(
            &f.journal,
            request.clone(),
            boundary,
        ))
        .await
        {
            Ok(result) => {
                applied = result;
                break;
            }
            Err(error) => {
                if let Some(required) =
                    error.downcast_ref::<crate::workspace_access::ConfirmationRequired>()
                {
                    assert!(attempt < 3);
                    f.journal
                        .approve_path_restore_access(crate::path_restore::AccessConsent {
                            op_id: Uuid::now_v7(),
                            request: request.clone(),
                            fingerprint: required.inspection.fingerprint.clone(),
                        })
                        .await?;
                } else {
                    return Err(error);
                }
            }
        }
    }
    if let Some(applied) = applied {
        let snapshot = f.journal.snapshot().await?;
        if let Some(raw) = snapshot["workers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|worker| worker["trackId"] == *track)
        {
            let mut worker: WorkerState = serde_json::from_value(raw.clone())?;
            worker.checkpoint = Some(applied.checkpoint);
            f.journal.worker_state(worker).await?;
        }
        f.journal
            .change_path_restore(Change::Complete(applied.operation))
            .await?;
    }
    let done = f.journal.restore_path(request.clone()).await?;
    assert_eq!(done.phase, crate::path_restore::Phase::Done);
    assert_eq!(words, f.journal.snapshot().await?["utterances"]);
    assert!(!crate::path_retirement::holds(&db(f)?, track)?);
    let replay = f.journal.restore_path(request).await?;
    assert_eq!(replay.revision, done.revision);
    Ok(done)
}
async fn named_world(fork_before_first_publication: bool) -> Result<()> {
    let f = Fixture::new(3).await?;
    fs::write(
        f.workspaces[0].path.join("shared-a.txt"),
        "Own shared A at the fork\n",
    )?;
    let fork = f.save(0, None).await?;
    if fork_before_first_publication {
        f.branch(&fork)?;
    }
    fs::write(
        f.workspaces[0].path.join("later-a.txt"),
        "Own later A to replace\n",
    )?;
    let point = f.save(0, Some(fork.clone())).await?;
    if !fork_before_first_publication {
        f.publish(vec![f.workspaces[0].track_id.clone()]).await?;
        f.branch(&fork)?;
    }
    fs::write(
        f.workspaces[1].path.join("alt.txt"),
        "Own alternate change\n",
    )?;
    f.save(1, None).await?;
    fs::write(f.workspaces[2].path.join("x.txt"), "Own independent X\n")?;
    f.save(2, None).await?;
    let x = f.publish(vec![f.workspaces[2].track_id.clone()]).await?;
    let aligned = realign(&f, 0, point, &x).await?;
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("x.txt"))?,
        "Own independent X\n"
    );
    fs::write(
        f.workspaces[0].path.join("later-after-align.txt"),
        "Own later A after realignment\n",
    )?;
    let point = f.save(0, Some(aligned)).await?;
    f.worker(0, point).await?;
    let checked = checked_choice(&f, 0).await?;
    assert!(checked.undo.is_none());
    let original = publish_idle(&f, &checked).await?;
    let original_records = records(&f, &f.workspaces[0].track_id)?;
    assert_eq!(
        original_records.len(),
        if fork_before_first_publication { 1 } else { 2 }
    );
    for record in &original_records {
        assert!(
            !git(
                &f.source,
                &[
                    "diff-tree",
                    "--no-commit-id",
                    "--name-only",
                    "-r",
                    &record.sha
                ]
            )?
            .lines()
            .any(|path| path == "x.txt")
        );
    }
    let restored = restore(&f, 1).await?;
    assert!(restored.restored.as_ref().unwrap().suggest_realign);
    assert!(restored.restored.as_ref().unwrap().checkpoint.is_none());
    let checked = checked_choice(&f, 1).await?;
    let expected: BTreeSet<_> = original_records.iter().map(|record| record.id).collect();
    let actual: BTreeSet<_> = checked
        .undo
        .as_ref()
        .unwrap()
        .records
        .iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(actual, expected);
    let replaced = publish_idle(&f, &checked).await?;
    assert_eq!(
        files(&f, &replaced)?,
        BTreeMap::from([
            ("parent.txt".into(), "Own initial base".into()),
            ("shared-a.txt".into(), "Own shared A at the fork".into()),
            ("x.txt".into(), "Own independent X".into()),
            ("alt.txt".into(), "Own alternate change".into())
        ])
    );
    assert_eq!(
        cancelled(&f)?,
        expected.iter().map(ToString::to_string).collect()
    );
    assert_eq!(
        files(&f, &original)?.get("x.txt").unwrap(),
        "Own independent X"
    );
    assert!(f.workspaces[0].path.join("later-a.txt").exists());
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    Ok(())
}
#[tokio::test]
async fn a_fork_before_publication_survives_world_realignment_original_commit_and_restore()
-> Result<()> {
    named_world(true).await
}
#[tokio::test]
async fn a_fork_after_publication_survives_world_realignment_original_commit_and_restore()
-> Result<()> {
    named_world(false).await
}

fn undo_ids(job: &Job) -> BTreeSet<String> {
    job.undo
        .as_ref()
        .into_iter()
        .flat_map(|undo| undo.records.iter())
        .map(|record| record.id.to_string())
        .collect()
}
fn changed_files(f: &Fixture, record: &crate::integration_record::Record) -> Result<Vec<String>> {
    Ok(git(
        &f.source,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            &record.sha,
        ],
    )?
    .lines()
    .map(str::to_owned)
    .collect())
}
async fn repeated_replacement(independent_equal_addition: bool, shared_files: bool) -> Result<()> {
    let f = Fixture::new(3).await?;
    let a = &f.workspaces[0].track_id;
    let alt = &f.workspaces[1].track_id;
    if shared_files {
        fs::write(f.workspaces[0].path.join("shared.txt"), "Own shared fork\n")?;
    }
    // An empty fork also covers an independent addition making the original's
    // whole contribution empty rather than only part of it.
    let fork = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("contribution.txt"),
        "Own same content\n",
    )?;
    f.save(0, Some(fork.clone())).await?;
    f.publish(vec![a.clone()]).await?;
    let first_a = records(&f, a)?;
    assert_eq!(first_a.len(), 1);
    f.branch(&fork)?;
    fs::write(
        f.workspaces[1].path.join("alternative.txt"),
        "Own alternate content\n",
    )?;
    f.save(1, None).await?;
    let selected = checked_choice(&f, 1).await?;
    assert_eq!(
        undo_ids(&selected),
        first_a.iter().map(|record| record.id.to_string()).collect()
    );
    let first_alt = publish_idle(&f, &selected).await?;
    assert!(!files(&f, &first_alt)?.contains_key("contribution.txt"));
    let first_cancelled = cancelled(&f)?;
    let restored = restore(&f, 0).await?;
    assert!(restored.restored.as_ref().unwrap().checkpoint.is_some());
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("contribution.txt"))?,
        "Own same content\n"
    );
    assert_eq!(records(&f, a)?, first_a);
    assert_eq!(cancelled(&f)?, first_cancelled);
    let independent = if independent_equal_addition {
        fs::write(
            f.workspaces[2].path.join("contribution.txt"),
            "Own same content\n",
        )?;
        f.save(2, None).await?;
        let published = f.publish(vec![f.workspaces[2].track_id.clone()]).await?;
        assert_eq!(
            files(&f, &published)?.get("contribution.txt").unwrap(),
            "Own same content"
        );
        Some(records(&f, &f.workspaces[2].track_id)?[0].clone())
    } else {
        None
    };
    let selected = checked_choice(&f, 0).await?;
    assert!(undo_ids(&selected).is_disjoint(&first_cancelled));
    let original = publish_idle(&f, &selected).await?;
    let mut original_files = BTreeMap::from([
        ("parent.txt".into(), "Own initial base".into()),
        ("contribution.txt".into(), "Own same content".into()),
    ]);
    if shared_files {
        original_files.insert("shared.txt".into(), "Own shared fork".into());
    }
    assert_eq!(files(&f, &original)?, original_files);
    let a_records = records(&f, a)?;
    assert_eq!(&a_records[..first_a.len()], first_a.as_slice());
    let new_a = &a_records[first_a.len()..];
    if independent_equal_addition {
        for record in new_a {
            assert!(
                changed_files(&f, record)?.is_empty(),
                "A must contribute no new file change"
            );
        }
        for commit in original
            .candidate
            .as_ref()
            .unwrap()
            .commits
            .iter()
            .filter(|commit| commit.track == *a)
        {
            assert!(
                git(
                    &f.source,
                    &[
                        "diff-tree",
                        "--no-commit-id",
                        "--name-only",
                        "-r",
                        &commit.sha
                    ]
                )?
                .is_empty()
            );
        }
    } else {
        assert!(!new_a.is_empty());
    }
    let cancellation_before = cancelled(&f)?;
    let restored_alt = restore(&f, 1).await?;
    assert!(restored_alt.restored.as_ref().unwrap().checkpoint.is_some());
    assert_eq!(cancelled(&f)?, cancellation_before);
    let selected = checked_choice(&f, 1).await?;
    let to_undo = undo_ids(&selected);
    assert!(
        to_undo.is_disjoint(&first_cancelled),
        "Never reverse old cancelled A receipts twice"
    );
    assert_eq!(
        to_undo,
        new_a.iter().map(|record| record.id.to_string()).collect()
    );
    if let Some(independent) = &independent {
        assert!(!to_undo.contains(&independent.id.to_string()));
    }
    let final_alt = publish_idle(&f, &selected).await?;
    let mut expected = BTreeMap::from([
        ("parent.txt".into(), "Own initial base".into()),
        ("alternative.txt".into(), "Own alternate content".into()),
    ]);
    if independent_equal_addition {
        expected.insert("contribution.txt".into(), "Own same content".into());
    }
    if shared_files {
        expected.insert("shared.txt".into(), "Own shared fork".into());
    }
    assert_eq!(files(&f, &final_alt)?, expected);
    assert_eq!(
        cancelled(&f)?,
        cancellation_before.union(&to_undo).cloned().collect()
    );
    assert_eq!(records(&f, a)?, a_records);
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    let current = entries.iter().find(|entry| entry.track == *a).unwrap();
    let source = f.journal.path_restore_source(current.id).await?;
    assert_eq!(
        source
            .cancelled
            .iter()
            .map(|record| record.id.to_string())
            .collect::<BTreeSet<_>>(),
        to_undo
    );
    let history = cancelled(&f)?;
    let restored_again = restore(&f, 0).await?;
    assert_eq!(
        restored_again
            .restored
            .as_ref()
            .unwrap()
            .checkpoint
            .is_some(),
        !to_undo.is_empty()
    );
    assert_eq!(cancelled(&f)?, history);
    assert_eq!(records(&f, a)?, a_records);
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("contribution.txt"))?,
        "Own same content\n"
    );
    let alt_records = records(&f, alt)?;
    for (index, record) in alt_records.iter().enumerate() {
        assert_eq!(record.ordinal, index as u64 + 1);
    }
    if let Some(independent) = independent {
        assert_eq!(
            records(&f, &f.workspaces[2].track_id)?.as_slice(),
            std::slice::from_ref(&independent)
        );
        assert!(!cancelled(&f)?.contains(&independent.id.to_string()));
        assert_eq!(
            git(
                &f.source,
                &[
                    "rev-parse",
                    &format!("{}:contribution.txt", independent.sha)
                ]
            )?,
            git(
                &f.source,
                &[
                    "rev-parse",
                    &format!(
                        "{}:contribution.txt",
                        final_alt.candidate.as_ref().unwrap().head
                    )
                ]
            )?,
            "The independent file survives byte for byte"
        );
    }
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    Ok(())
}
#[tokio::test]
async fn repeated_restore_and_replacement_keep_old_receipts_and_cancel_only_the_latest_publication()
-> Result<()> {
    repeated_replacement(false, false).await
}
#[tokio::test]
async fn a_zero_delta_republication_never_cancels_an_independent_equal_addition() -> Result<()> {
    repeated_replacement(true, false).await
}
#[tokio::test]
async fn repeated_replacement_retains_the_shared_fork_after_restoring_both_paths() -> Result<()> {
    repeated_replacement(false, true).await
}

async fn restore_words(f: &Fixture, track: Option<&str>) -> Result<serde_json::Value> {
    let id = Uuid::now_v7();
    f.journal
        .stage_utterance(
            id,
            "Restore the saved path".into(),
            crate::decision::Target::None,
            Some(f.workspaces[1].track_id.clone()),
            vec![],
        )
        .await?;
    let input = f.journal.claim_input().await?.unwrap();
    assert_eq!(input.id, id.to_string());
    f.journal.apply_input(input.id, input.revision, crate::decision::Outcome::Classified(crate::decision::Decision {
        kind:crate::decision::Kind::Command,command:Some("restore".into()),track_id:track.map(str::to_owned),boundary:None,
        confidence:1.,inference_ms:1.,timed_out:false,
        scores:serde_json::json!({"presentation":{"command":"restore","operationId":Uuid::nil(),"receiptId":Uuid::nil(),"restore":[{"id":Uuid::nil()}]}}),
    })).await?;
    f.journal.read_utterance(id.to_string()).await
}

#[tokio::test]
async fn restore_words_capture_the_retirement_and_never_restore_or_retarget_it_by_themselves()
-> Result<()> {
    let f = Fixture::new(2).await?;
    let a = &f.workspaces[0].track_id;
    let fork = f.save(0, None).await?;
    fs::write(f.workspaces[0].path.join("own.txt"), "Own original\n")?;
    f.save(0, Some(fork.clone())).await?;
    f.publish(vec![a.clone()]).await?;
    f.branch(&fork)?;
    fs::write(
        f.workspaces[1].path.join("alternate.txt"),
        "Own alternate\n",
    )?;
    let alternative = f.save(1, None).await?;
    let first = checked_choice(&f, 1).await?;
    publish_idle(&f, &first).await?;
    let before = f.journal.snapshot().await?;
    let retirement = before["pathRetirements"][0]["id"].clone();
    let word = restore_words(&f, None).await?;
    assert_eq!(word["state"], "placed");
    assert!(word["presentation"]["trackId"].is_null());
    assert_eq!(word["presentation"]["restore"][0]["id"], retirement);
    assert_eq!(word["presentation"]["restore"][0]["track"], *a);
    assert_eq!(word["presentation"]["restore"].as_array().unwrap().len(), 1);
    let pending_for_a = restore_words(&f, Some(a)).await?;
    assert_eq!(pending_for_a["state"], "placed");
    let frozen = word["presentation"].clone();
    f.journal
        .resolve_input(
            Uuid::now_v7(),
            word["id"].as_str().unwrap().into(),
            1,
            crate::decision::Resolution::Hold,
        )
        .await?;
    let after_reading = f.journal.snapshot().await?;
    assert_eq!(after_reading["pathRetirements"], before["pathRetirements"]);
    assert_eq!(after_reading["pathRestores"], before["pathRestores"]);
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("own.txt"))?,
        "Own original\n"
    );
    // This is a separate explicit Restore, as if another window confirmed it.
    restore(&f, 0).await?;
    fs::write(
        f.workspaces[1].path.join("second.txt"),
        "Own next alternative\n",
    )?;
    f.save(1, Some(alternative)).await?;
    let second = checked_choice(&f, 1).await?;
    publish_idle(&f, &second).await?;
    let current = f.journal.snapshot().await?;
    assert_ne!(current["pathRetirements"][0]["id"], retirement);
    assert_eq!(current["pathRestores"].as_array().unwrap().len(), 1);
    // A pending display is not worker input and must survive a later retirement.
    let pinned = f
        .journal
        .read_utterance(pending_for_a["id"].as_str().unwrap().into())
        .await?;
    assert_eq!(pinned["state"], "placed");
    assert_eq!(pinned["presentation"], pending_for_a["presentation"]);
    let restarted = Journal::open(
        f.root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    let held = restarted
        .read_utterance(word["id"].as_str().unwrap().into())
        .await?;
    let resumed = restarted
        .start_held_input(
            Uuid::now_v7(),
            word["id"].as_str().unwrap().into(),
            held["revision"].as_i64().unwrap(),
        )
        .await?;
    assert_eq!(resumed["presentation"], frozen);
    let displayed = restarted
        .resolve_input(
            frozen["receiptId"].as_str().unwrap().parse()?,
            word["id"].as_str().unwrap().into(),
            resumed["revision"].as_i64().unwrap(),
            crate::decision::Resolution::Presented,
        )
        .await?;
    assert_eq!(displayed["state"], "landed");
    assert_eq!(
        restarted.snapshot().await?["pathRestores"],
        current["pathRestores"]
    );
    let fresh = restore_words(&f, None).await?;
    assert_eq!(
        fresh["presentation"]["restore"][0]["id"],
        current["pathRetirements"][0]["id"]
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    Ok(())
}
