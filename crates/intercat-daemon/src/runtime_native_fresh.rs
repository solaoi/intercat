//! A real offline source host, an owned late writer and an unowned file reader.
use super::*;
use crate::fresh::{Change, Job, Phase, Request};
use std::{fs, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn ready(child: &mut tokio::process::Child) -> Result<()> {
    let mut lines = BufReader::new(child.stdout.take().ok_or("Missing own dummy stdout")?);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), lines.read_line(&mut line)).await??;
    if line != "ready\n" {
        return Err("Own dummy child did not become ready.".into());
    }
    Ok(())
}
async fn phase(journal: &Journal, id: Uuid, wanted: Phase) -> Result<Job> {
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let job = journal
                .fresh_job(id)
                .await?
                .ok_or("Missing own Fresh request")?;
            if job.phase == wanted {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            if let Some(error) = job.error {
                return Err(format!("Own Fresh {:?}: {error}", job.phase).into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "pinned Muse offline echo; cancellation while closing the source host"]
async fn native_fresh_cancellation_during_release_closes_only_its_source_and_keeps_words_held()
-> Result<()> {
    let mut fixture = QueueFixture::new().await?;
    let output = PathBuf::from(std::env::var("INTERCAT_M4_FRESH_OUTPUT")?).join("cancel-release");
    fs::create_dir_all(&output)?;
    let result = async {
        fixture.launch(&[0, 1]).await?;
        fixture.start(0).await?;
        fixture.start(1).await?;
        fixture.wait_workers(&[0, 1], true).await?;
        let source = fixture.words[0].1.clone();
        let other = fixture.words[1].1.clone();
        let snapshot = fixture.journal.snapshot().await?;
        let worker: WorkerState = serde_json::from_value(
            snapshot["workers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|worker| worker["trackId"] == source)
                .unwrap()
                .clone(),
        )?;
        let other_identity: crate::process::Identity = serde_json::from_value(
            snapshot["workers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|worker| worker["trackId"] == other)
                .unwrap()["identity"]
                .clone(),
        )?;
        let old_identity = worker.identity.unwrap();
        let workspace = fixture.journal.workspace(source.clone()).await?;
        let head = native_git(&workspace.path, &["rev-parse", "HEAD"])?;
        let index = PathBuf::from(native_git(
            &workspace.path,
            &["rev-parse", "--absolute-git-dir"],
        )?)
        .join("index");
        let index_bytes = fs::read(&index)?;
        let mut job = fixture
            .service
            .prepare_fresh(Request {
                op_id: Uuid::now_v7(),
                track_id: source.clone(),
                session_id: worker.session_id.clone().unwrap(),
            })
            .await?;
        let word = Uuid::now_v7();
        fixture
            .journal
            .stage_utterance(
                word,
                "OWN_CANCEL_RELEASE_QUEUED".into(),
                Target::Lock(source.clone()),
                None,
                vec![],
            )
            .await?;
        let input = fixture.journal.claim_input().await?.unwrap();
        assert_eq!(input.id, word.to_string());
        fixture
            .journal
            .apply_input(
                input.id,
                input.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Extend,
                    track_id: Some(source.clone()),
                    boundary: Some("now".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        job = fixture.service.advance_fresh(job).await?;
        assert_eq!(job.phase, Phase::Captured);
        job = fixture.service.advance_fresh(job).await?;
        assert_eq!(job.phase, Phase::Releasing);
        let cancel = Uuid::now_v7();
        let pending = fixture
            .service
            .cancel_fresh(cancel, job.request.op_id, job.revision)
            .await?;
        assert_eq!(pending.phase, Phase::Cancelling);
        assert_eq!(
            fixture
                .service
                .cancel_fresh(cancel, job.request.op_id, job.revision)
                .await?
                .revision,
            pending.revision
        );
        let mut current = pending;
        let done = tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                match fixture.service.advance_fresh(current.clone()).await {
                    Ok(next) => {
                        current = next;
                        if current.phase == Phase::Cancelled {
                            return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current);
                        }
                    }
                    Err(error) if error.is::<crate::fresh::Waiting>() => {}
                    Err(error) => return Err(error),
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await??;
        assert!(!crate::process::is_alive(old_identity));
        assert!(crate::process::is_alive(other_identity));
        let held = fixture.journal.read_utterance(word.to_string()).await?;
        assert_eq!(held["state"], "held");
        assert_eq!(held["dispatchRunnable"], false);
        assert_eq!(fixture.capacity.snapshot().active, 0);
        assert_eq!(fixture.memory.snapshot().hosts, 1);
        assert_eq!(native_git(&workspace.path, &["rev-parse", "HEAD"])?, head);
        assert_eq!(fs::read(index)?, index_bytes);
        assert_eq!(
            native_git(&fixture.root.path().join("source"), &["rev-parse", "HEAD"])?,
            fixture.source_head
        );
        let db = rusqlite::Connection::open(fixture.root.path().join("db"))?;
        assert!(!crate::continuation::closed(&db, &source)?);
        assert_eq!(
            fixture.journal.operation(cancel).await?.unwrap()["state"],
            "complete"
        );
        fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(&json!({
                "provider":"echo","paidTurns":0,"sourceStopped":true,"otherHostAlive":true,
                "queuedWordHeld":true,"headAndIndexUnchanged":true,"sourceUnchanged":true,
                "noWriterTransferred":true,"cancelReplayedOnce":true,"job":done.status(),
            }))?,
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    fixture._service_stop.send_replace(true);
    let stopped = fixture.stop_hosts().await;
    fixture.editor.stop().await;
    if let Err(error) = &result {
        fs::write(
            output.join("failure.json"),
            serde_json::to_vec_pretty(
                &json!({"error":error.to_string(),"snapshot":fixture.journal.snapshot().await?}),
            )?,
        )?;
    }
    result?;
    stopped
}
async fn single_job(journal: &Journal, track: &str, text: &str) -> Result<crate::plans::Plan> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, text.into(), Target::Lock(track.into()), None, vec![])
        .await?;
    let input = journal
        .claim_input()
        .await?
        .ok_or("Missing own plan source")?;
    assert_eq!(input.id, id.to_string());
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Plan,
                track_id: Some(track.into()),
                boundary: None,
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let plan = journal
        .plans()
        .await?
        .into_iter()
        .find(|plan| plan.source_id == id.to_string())
        .ok_or("Missing own plan")?;
    assert!(
        journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?
    );
    assert!(
        journal
            .generated_plan(
                plan.id,
                plan.generation.id,
                plan.rev,
                crate::plan_store::Generated::TimedOut,
                vec![]
            )
            .await?
    );
    journal
        .plan_command(crate::plan_store::Command {
            op_id: Uuid::now_v7(),
            plan_id: plan.id,
            revision: plan.rev,
            action: crate::plan_store::Action::RunAsOne,
        })
        .await?;
    let input = journal
        .claim_input()
        .await?
        .ok_or("Missing own single job")?;
    assert_eq!(input.id, id.to_string());
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(track.into()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    journal
        .plans()
        .await?
        .into_iter()
        .find(|value| value.id == plan.id)
        .ok_or_else(|| "Missing own single job plan".into())
}
fn native_saved_word(db: &rusqlite::Connection, id: &str) -> Result<String> {
    Ok(db.query_row("SELECT json_object('text',text,'target',target,'state',state,'revision',revision,'decision',decision,'command',dispatch_command) FROM utterances WHERE id=?1",[id],|row|row.get(0))?)
}

async fn finish_native_fresh(fixture: &QueueFixture, mut job: Job, purpose: &str) -> Result<Job> {
    job = tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            if job.phase == Phase::SnapshotReady {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            if job.phase == Phase::NeedsAccess {
                let inspection = job.inspection.as_ref().ok_or("Missing own inspection")?;
                if !inspection.processes.is_empty() {
                    return Err("Unexpected external process in own native fixture".into());
                }
                job = fixture
                    .journal
                    .fresh(Change::ApproveAccess {
                        op_id: Uuid::now_v7(),
                        id: job.request.op_id,
                        revision: job.revision,
                        fingerprint: inspection.fingerprint.clone(),
                    })
                    .await?;
            }
            match fixture.service.advance_fresh(job.clone()).await {
                Ok(next) => job = next,
                Err(error) if error.is::<crate::fresh::Waiting>() => {}
                Err(error) => return Err(error),
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    // The summary engine's actual-provider quality is covered separately.
    // Here a typed dummy document isolates the real session/ownership path.
    let changes = crate::fresh_summary::changes(&job).await?;
    job = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match fixture
                .journal
                .fresh(Change::PrepareSummary {
                    id: job.request.op_id,
                    revision: job.revision,
                    changes: changes.clone(),
                })
                .await
            {
                Ok(next) => return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(next),
                Err(error) if error.is::<crate::fresh::Waiting>() => {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await??;
    let document = crate::handoff::Document {
        fields: crate::handoff::Fields {
            purpose: purpose.into(),
            constraints: vec!["GREEN_ONLY".into()],
            unresolved: vec![],
            changed_files: vec![],
            completed: vec!["Own source turn completed".into()],
            unverified: vec!["No actual code changes requested".into()],
            next_checks: vec!["Read existing own files".into()],
        },
        source_addresses: crate::handoff::addresses(
            job.summary_input.as_ref().unwrap(),
            &job.request.track_id,
        ),
    };
    job = fixture
        .journal
        .fresh(Change::Summarized {
            id: job.request.op_id,
            revision: job.revision,
            document,
        })
        .await?;
    job = fixture.service.advance_fresh(job).await?;
    assert_eq!(job.phase, Phase::PublishingOwner);
    job = fixture.service.advance_fresh(job).await?;
    assert_eq!(job.phase, Phase::Transferred);
    Ok(job)
}

#[tokio::test]
#[ignore = "native offline echo: release source/owned writer, current file-access consent, final private snapshot and memory cap1"]
async fn native_fresh_release_preserves_late_files_and_never_stops_an_unowned_reader() -> Result<()>
{
    let mut fixture = QueueFixture::new().await?;
    fixture.memory = crate::host_memory::Pool::fixed(crate::host_memory::Sample {
        free_bytes: 500 * 1024 * 1024,
        pressured: false,
    });
    fixture.service.host_memory = fixture.memory.clone();
    let output = PathBuf::from(std::env::var("INTERCAT_M4_FRESH_OUTPUT")?).join("release");
    fs::create_dir_all(&output)?;
    let reports =
        crate::report_analysis::Service::start(fixture.journal.clone(), fixture.editor.clone());
    let (external, stopping) = watch::channel(false);
    let mut driver = None;
    let mut writer = None;
    let mut reader = None;
    let result=async {
        fixture.launch(&[0]).await?;
        fixture.start(0).await?;
        fixture.wait_workers(&[0],true).await?;
        let track=fixture.words[0].1.clone();
        let workspace=fixture.journal.workspace(track.clone()).await?;
        let before=fixture.journal.snapshot().await?;
        let worker:WorkerState=serde_json::from_value(before["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==track).unwrap().clone())?;
        let source_identity=worker.identity.unwrap();
        let point=worker.checkpoint.clone().unwrap();
        let directory=fixture.root.path().join("hosts").join(&track);
        let mut nonce=None;
        for entry in fs::read_dir(&directory)? {
            let entry=entry?;
            let record=crate::process_ledger::read(&entry.path().join(crate::process_ledger::NAME))?;
            if record.root==Some(source_identity) {nonce=record.nonces.first().cloned();}
        }
        let nonce=nonce.ok_or("Missing own source host marker")?;
        let index=PathBuf::from(native_git(&workspace.path,&["rev-parse","--absolute-git-dir"])?).join("index");
        let before_index=fs::read(&index)?;
        let before_head=native_git(&workspace.path,&["rev-parse","HEAD"])?;
        let late=workspace.path.join("late-source.txt");
        let mut owned=tokio::process::Command::new("/usr/bin/python3")
            .args(["-I","-u","-c","import signal,sys,time\ndef stop(*args):\n with open(sys.argv[1],'w') as f: f.write('Own final bytes written while stopping\\n')\n sys.exit(0)\nsignal.signal(signal.SIGTERM,stop)\nprint('ready',flush=True)\nwhile True: time.sleep(.05)"])
            .arg(&late).current_dir(&workspace.path).env_clear().env("PATH","/usr/bin:/bin").env("INTERCAT_HOST",nonce)
            .process_group(0).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
        ready(&mut owned).await?;
        let writer_identity=crate::process::identity(owned.id().unwrap())?;
        writer=Some(owned);
        let tracker=fixture.service.port(&track)?.process_tracker().ok_or("Missing own source tracker")?;
        assert!(tracker.lock().unwrap().refresh()?.processes.iter().any(|process|process.identity==writer_identity));
        let dummy=workspace.path.join("reader-dummy.txt");
        fs::write(&dummy,"Own dummy reference for an external reader\n")?;
        let mut unowned=tokio::process::Command::new("/usr/bin/python3")
            .args(["-I","-u","-c","import sys\nf=open(sys.argv[1],'rb')\nprint('ready',flush=True)\nsys.stdin.read()"])
            .arg(&dummy).current_dir(fixture.root.path()).env_clear().env("PATH","/usr/bin:/bin")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
        ready(&mut unowned).await?;
        let reader_identity=crate::process::identity(unowned.id().unwrap())?;
        reader=Some(unowned);
        let mut next_memory=fixture.memory.request("own-next-summary-host".into());
        assert!(next_memory.try_acquire().is_none());
        let job=fixture.service.prepare_fresh(Request{op_id:Uuid::now_v7(),track_id:track.clone(),session_id:worker.session_id.clone().unwrap()}).await?;
        let word=Uuid::now_v7();
        fixture.journal.stage_utterance(word,"OWN_FRESH_QUEUED_AFTER_HANDOFF".into(),Target::Lock(track.clone()),None,vec![]).await?;
        let input=fixture.journal.claim_input().await?.unwrap();
        fixture.journal.apply_input(input.id,input.revision,Outcome::Classified(Decision{kind:Kind::Extend,track_id:Some(track.clone()),boundary:Some("now".into()),command:None,confidence:1.,inference_ms:1.,timed_out:false,scores:json!({})})).await?;
        let queued=fixture.journal.read_utterance(word.to_string()).await?;
        assert_eq!(queued["state"],"placed");
        driver=Some(crate::fresh_driver::Driver::start(fixture.service.clone(),fixture.journal.clone(),stopping.clone()));
        let waiting=phase(&fixture.journal,job.request.op_id,Phase::NeedsAccess).await?;
        assert!(waiting.inspection.as_ref().unwrap().processes.iter().any(|process|process.identity==reader_identity));
        assert!(crate::process::is_alive(reader_identity));
        assert!(!crate::process::is_alive(source_identity));
        tokio::time::timeout(Duration::from_secs(5),writer.as_mut().unwrap().wait()).await??;
        assert_eq!(fs::read_to_string(&late)?,"Own final bytes written while stopping\n");
        assert_eq!(fixture.memory.snapshot().hosts,0);
        let memory=next_memory.try_acquire().ok_or("Source memory remained reserved after its stop")?;
        drop(memory);
        // A restarted driver resumes the same persisted request. It neither
        // starts another SDK session nor loses the pending access evidence.
        driver.take().unwrap().shutdown().await;
        driver=Some(crate::fresh_driver::Driver::start(fixture.service.clone(),fixture.journal.clone(),stopping.clone()));
        let mut approvals=Vec::new();
        let frozen=tokio::time::timeout(Duration::from_secs(35),async {
            loop {
                let current=fixture.journal.fresh_job(job.request.op_id).await?.ok_or("Missing own Fresh request")?;
                if current.phase==Phase::SnapshotReady {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(current);}
                if let Some(error)=current.error {return Err(format!("Own Fresh files: {error}").into());}
                if current.phase==Phase::NeedsAccess && !approvals.iter().any(|(_,revision,_)|*revision==current.revision) {
                    let op_id=Uuid::now_v7();let fingerprint=current.inspection.as_ref().unwrap().fingerprint.clone();
                    fixture.journal.fresh(Change::ApproveAccess{op_id,id:job.request.op_id,revision:current.revision,fingerprint:fingerprint.clone()}).await?;
                    approvals.push((op_id,current.revision,fingerprint));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        assert!(crate::process::is_alive(reader_identity));
        let files=frozen.files.as_ref().unwrap();
        assert_eq!(frozen.boundary.as_ref().unwrap().checkpoint.sha,point.sha);
        assert_ne!(files.checkpoint.sha,point.sha);
        let repo=intercat_git::Repository::open(&workspace.path)?;
        let saved=repo.saved_files(files.checkpoint.sha.parse()?)?;
        assert_eq!(repo.saved_bytes(&saved["late-source.txt"])?,b"Own final bytes written while stopping\n");
        assert_eq!(repo.reference_target(&files.checkpoint.reference)?,Some(files.checkpoint.sha.parse()?));
        assert_eq!(native_git(&workspace.path,&["rev-parse","HEAD"])?,before_head);
        assert_eq!(fs::read(&index)?,before_index);
        let after=fixture.journal.read_utterance(word.to_string()).await?;
        for field in ["state","revision","decision","text"] {assert_eq!(after[field],queued[field]);}
        let db=rusqlite::Connection::open(fixture.root.path().join("db"))?;
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|row|row.get::<_,i64>(0))?,1);
        assert_eq!(db.query_row("SELECT count(*) FROM events WHERE kind='worker.sessionReleased' AND json_extract(payload,'$.sessionId')=?1",[&job.request.session_id],|row|row.get::<_,i64>(0))?,1);
        assert_eq!(db.query_row("SELECT count(*) FROM timeline_turns WHERE turn=?1",[files.checkpoint.turn_id.to_string()],|row|row.get::<_,i64>(0))?,0);
        let echo:i64=db.query_row("SELECT count(*) FROM events WHERE kind='msp.received' AND json_extract(payload,'$.result.session.providerId')='echo'",[],|row|row.get(0))?;
        assert_eq!(echo,1);
        assert_eq!(native_git(&fixture.root.path().join("source"),&["rev-parse","HEAD"])?,fixture.source_head);
        fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"schema":crate::journal::SCHEMA_VERSION,"provider":"echo","paidTurns":0,"echoSessionReplies":echo,"sourceReleased":true,"ownedWriterStopped":true,"unownedReaderStillAlive":true,"lateFilesProtected":true,"headAndIndexUnchanged":true,"memoryBefore":1,"memoryAfter":0,"nextHostCanAcquire":true,"driverRestarted":true,"sameRequestId":job.request.op_id,"accessConfirmations":approvals.len(),"futureWordUnchanged":true,"noSyntheticSdkTurn":true,"sourceUnchanged":true,"jobStatus":frozen.status(),"before":before}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    external.send_replace(true);
    if let Some(driver) = driver {
        driver.shutdown().await;
    }
    reports.shutdown().await;
    let stopped = fixture.stop_hosts().await;
    if let Some(mut child) = reader {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.shutdown().await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
    if let Some(mut child) = writer {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    fixture.editor.stop().await;
    if let Err(error) = &result {
        let snapshot = fixture.journal.snapshot().await?;
        fs::write(
            output.join("failure.json"),
            serde_json::to_vec_pretty(&json!({"error":error.to_string(),"snapshot":snapshot}))?,
        )?;
    }
    result?;
    stopped
}

#[tokio::test]
#[ignore = "explicit pinned Muse offline echo; own dummy workspace only"]
async fn native_fresh_new_session_receives_summary_before_moved_words_on_the_same_worktree()
-> Result<()> {
    let mut fixture = QueueFixture::new()
        .await
        .map_err(|error| format!("Fresh fixture initialization: {error}"))?;
    let output = PathBuf::from(std::env::var("INTERCAT_M4_FRESH_OUTPUT")?).join("continuation");
    fs::create_dir_all(&output)?;
    let reports =
        crate::report_analysis::Service::start(fixture.journal.clone(), fixture.editor.clone());
    let result=async {
        fixture.launch(&[0]).await?;
        fixture.start(0).await?;
        fixture.wait_workers(&[0],true).await?;
        let source=fixture.words[0].1.clone();
        let completed=single_job(&fixture.journal,&source,"OWN_NATIVE_COMPLETED_SINGLE_JOB").await?;
        tokio::time::timeout(Duration::from_secs(35),async {
            loop {
                let snapshot=fixture.journal.snapshot().await?;
                let worker=snapshot["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==source).unwrap();
                if fixture.journal.read_utterance(completed.source_id.clone()).await?["state"]=="landed" && worker["activeTurn"].is_null() && worker["checkpointPending"]==false
                    && fixture.journal.read_input_origin(completed.source_id.parse()?).await?.is_some_and(|origin|origin.checkpoint.is_some()) {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(());}
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        let db=rusqlite::Connection::open(fixture.root.path().join("db"))?;
        let completed_word=native_saved_word(&db,&completed.source_id)?;
        let completed_plan:String=db.query_row("SELECT record FROM plans WHERE id=?1",[completed.id.to_string()],|row|row.get(0))?;
        let completed_origin=fixture.journal.read_input_origin(completed.source_id.parse()?).await?.ok_or("Missing completed own native turn")?;
        assert!(completed_origin.checkpoint.is_some());
        let source_workspace=fixture.journal.workspace(source.clone()).await?;
        let before=fixture.journal.snapshot().await?;
        let old:WorkerState=serde_json::from_value(before["workers"].as_array().unwrap().iter().find(|value|value["trackId"]==source).unwrap().clone())?;
        let old_host=old.identity.unwrap();
        let head=native_git(&source_workspace.path,&["rev-parse","HEAD"])?;
        let index=PathBuf::from(native_git(&source_workspace.path,&["rev-parse","--absolute-git-dir"])?).join("index");
        let index_bytes=fs::read(&index)?;
        let mut job=fixture.service.prepare_fresh(Request{op_id:Uuid::now_v7(),track_id:source.clone(),session_id:old.session_id.clone().unwrap()}).await?;
        let pending=single_job(&fixture.journal,&source,"OWN_NATIVE_PENDING_SINGLE_JOB").await?;
        assert_eq!(fixture.journal.read_utterance(pending.source_id.clone()).await?["state"],"placed");
        let word=Uuid::now_v7();
        fixture.journal.stage_utterance(word,"OWN_FOLLOWUP_AFTER_FRESH_SUMMARY".into(),Target::Lock(source.clone()),None,vec![]).await?;
        let input=fixture.journal.claim_input().await?.unwrap();
        assert_eq!(input.id,word.to_string());
        fixture.journal.apply_input(input.id,input.revision,Outcome::Classified(Decision{kind:Kind::Extend,track_id:Some(source.clone()),boundary:Some("now".into()),command:None,confidence:1.,inference_ms:0.,timed_out:false,scores:json!({})})).await?;
        job=finish_native_fresh(&fixture,job,"OWN_NATIVE_FRESH_SUMMARY_FIRST").await?;
        assert!(!crate::process::is_alive(old_host));
        let child=fixture.journal.workspace(job.successor_track.clone()).await?;
        assert_eq!(child.path,source_workspace.path);
        assert_eq!(native_git(&child.path,&["rev-parse","HEAD"])?,head);
        assert_eq!(fs::read(&index)?,index_bytes);
        let db=rusqlite::Connection::open(fixture.root.path().join("db"))?;
        let owner=crate::continuation::start(&db,&job.successor_track)?.unwrap();
        assert_eq!(fixture.journal.read_utterance(word.to_string()).await?["freshRunnable"],false);
        fixture.tasks.push(crate::runtime::tests::fixture_echo_worker(&fixture.service,fixture.config.clone(),fixture.editor.clone(),child).await?);
        let final_state=tokio::time::timeout(Duration::from_secs(45),async{
            loop{
                let snapshot=fixture.journal.snapshot().await?;
                let worker=snapshot["workers"].as_array().unwrap().iter().find(|value|value["trackId"]==job.successor_track);
                if let Some(worker)=worker {
                    if matches!(worker["phase"].as_str(),Some("failed"|"lost"|"needs-recovery")){return Err(format!("Fresh successor failed: {worker}").into());}
                    if worker["checkpointPending"]==false && worker["activeTurn"].is_null() && !worker["checkpoint"].is_null() && fixture.journal.read_utterance(word.to_string()).await?["state"]=="landed" && fixture.journal.read_utterance(pending.source_id.clone()).await?["state"]=="landed" && crate::continuation::start(&db,&job.successor_track)?.unwrap().ready {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(worker.clone());}
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(final_state["sessionId"],job.successor_session);
        let review=fixture.journal.integration_command(crate::integration::Command{op_id:Uuid::now_v7(),action:crate::integration::Action::Open{tracks:Some(vec![job.successor_track.clone()])}}).await?;
        assert_eq!(review.path_review.families.len(),1);
        assert!(!review.path_review.families[0].has_alternatives());
        assert_eq!(review.path_review.choices.values().next().unwrap().kept,job.successor_track);
        assert!(review.path_state.as_ref().unwrap().pending.is_empty());
        assert!(crate::integration::project(&review)?["pathReview"]["families"].as_array().unwrap().is_empty());
        assert!(!review.tracks.iter().find(|track|track.id==source).unwrap().included);
        let starts=db.prepare("SELECT u.id FROM operations o JOIN utterances u ON u.dispatch_command=o.id WHERE o.kind='turn.start' AND json_extract(o.payload,'$.sessionId')=?1 ORDER BY o.rowid")?.query_map([&job.successor_session],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(starts,vec![owner.first_input.clone(),pending.source_id.clone(),word.to_string()]);
        assert_eq!(native_saved_word(&db,&completed.source_id)?,completed_word);
        assert_eq!(db.query_row("SELECT record FROM plans WHERE id=?1",[completed.id.to_string()],|row|row.get::<_,String>(0))?,completed_plan);
        let origin=fixture.journal.read_input_origin(completed.source_id.parse()?).await?.unwrap();
        assert_eq!(origin.identity,completed_origin.identity);assert_eq!(origin.checkpoint,completed_origin.checkpoint);
        let moved=fixture.journal.plans().await?.into_iter().find(|plan|plan.id==pending.id).unwrap();
        assert_eq!(moved.locked_track.as_deref(),Some(job.successor_track.as_str()));
        assert_eq!(moved.source_revision,fixture.journal.read_utterance(pending.source_id.clone()).await?["revision"].as_i64().unwrap());
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='session/start' AND json_extract(payload,'$.sessionId')=?1",[&job.successor_session],|row|row.get::<_,i64>(0))?,1);
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='session/fork' OR (kind='session/resume' AND json_extract(payload,'$.sessionId')=?1)",[&job.request.session_id],|row|row.get::<_,i64>(0))?,0);
        assert!(crate::continuation::ensure_writer(&db,&source).is_err());
        assert_eq!(native_git(&source_workspace.path,&["rev-parse","HEAD"])?,head);
        assert_eq!(fs::read(index)?,index_bytes);
        assert_eq!(native_git(&fixture.root.path().join("source"),&["rev-parse","HEAD"])?,fixture.source_head);
        fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"schema":crate::journal::SCHEMA_VERSION,"provider":"echo","paidTurns":0,"sourceHostStopped":true,"sameWorktree":true,"oldWriterPermanentlyClosed":true,"newSessionStartedOnce":true,"noParentHistoryImported":true,"summaryFirst":true,"movedSingleJobSecond":true,"ordinaryFollowupThird":true,"completedSingleJobReceiptUnchanged":true,"completedSingleJobPlanUnchanged":true,"pendingSingleJobSourceAndPlanMatch":true,"headAndIndexUnchanged":true,"sourceUnchanged":true,"job":job.status(),"successor":final_state}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    reports.shutdown().await;
    fixture._service_stop.send_replace(true);
    let stopped = fixture.stop_hosts().await;
    fixture.editor.stop().await;
    if let Err(error) = &result {
        fs::write(
            output.join("failure.json"),
            serde_json::to_vec_pretty(
                &json!({"error":error.to_string(),"snapshot":fixture.journal.snapshot().await?}),
            )?,
        )?;
    }
    result?;
    stopped
}

#[tokio::test]
#[ignore = "explicit pinned Muse offline echo; cancellation after source host closure"]
async fn native_fresh_cold_cancel_resumes_only_new_words_and_explicitly_started_old_words()
-> Result<()> {
    let mut fixture = QueueFixture::new().await?;
    let output = PathBuf::from(std::env::var("INTERCAT_M4_FRESH_OUTPUT")?).join("cold-cancel");
    fs::create_dir_all(&output)?;
    let reports =
        crate::report_analysis::Service::start(fixture.journal.clone(), fixture.editor.clone());
    let result=async {
        fixture.launch(&[0]).await?;fixture.start(0).await?;fixture.wait_workers(&[0],true).await?;
        let track=fixture.words[0].1.clone();
        let workspace=fixture.journal.workspace(track.clone()).await?;
        let before=fixture.journal.snapshot().await?;
        let old:WorkerState=serde_json::from_value(before["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==track).unwrap().clone())?;
        let old_host=old.identity.unwrap();let session=old.session_id.clone().unwrap();
        let head=native_git(&workspace.path,&["rev-parse","HEAD"])?;
        let index=PathBuf::from(native_git(&workspace.path,&["rev-parse","--absolute-git-dir"])?).join("index");let index_bytes=fs::read(&index)?;
        let mut job=fixture.service.prepare_fresh(Request{op_id:Uuid::now_v7(),track_id:track.clone(),session_id:session.clone()}).await?;
        let mut old_words=Vec::new();
        for marker in ["OWN_COLD_QUEUED_FIRST","OWN_COLD_QUEUED_SECOND"]{
            let id=Uuid::now_v7();fixture.journal.stage_utterance(id,marker.into(),Target::Lock(track.clone()),None,vec![]).await?;
            let input=fixture.journal.claim_input().await?.unwrap();assert_eq!(input.id,id.to_string());
            fixture.journal.apply_input(input.id,input.revision,Outcome::Classified(Decision{kind:Kind::Extend,track_id:Some(track.clone()),boundary:Some("now".into()),command:None,confidence:1.,inference_ms:0.,timed_out:false,scores:json!({})})).await?;
            old_words.push(id);
        }
        job=tokio::time::timeout(Duration::from_secs(40),async {
            loop{
                if job.phase==Phase::Inspecting{return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(job);}
                match fixture.service.advance_fresh(job.clone()).await{Ok(next)=>job=next,Err(error) if error.is::<crate::fresh::Waiting>()=>{},Err(error)=>return Err(error)}
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        assert!(!crate::process::is_alive(old_host));assert_eq!(fixture.memory.snapshot().hosts,0);
        let cancelled=fixture.service.cancel_fresh(Uuid::now_v7(),job.request.op_id,job.revision).await?;
        assert_eq!(cancelled.phase,Phase::Cancelled);
        for id in &old_words {let held=fixture.journal.read_utterance(id.to_string()).await?;assert_eq!(held["state"],"held");assert_eq!(held["dispatchRunnable"],false);}
        // A new explicit word is allowed without reviving the old queued work.
        let new_word=Uuid::now_v7();fixture.journal.stage_utterance(new_word,"OWN_COLD_NEW_WORD_ONLY".into(),Target::Lock(track.clone()),None,vec![]).await?;
        let input=fixture.journal.claim_input().await?.unwrap();assert_eq!(input.id,new_word.to_string());
        fixture.journal.apply_input(input.id,input.revision,Outcome::Classified(Decision{kind:Kind::Extend,track_id:Some(track.clone()),boundary:Some("now".into()),command:None,confidence:1.,inference_ms:0.,timed_out:false,scores:json!({})})).await?;
        assert_eq!(fixture.journal.read_utterance(new_word.to_string()).await?["dispatchRunnable"],true);
        fixture.launch(&[0]).await?;
        let first=tokio::time::timeout(Duration::from_secs(30),async {
            loop{
                let snapshot=fixture.journal.snapshot().await?;
                let worker=snapshot["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==track).unwrap();
                if matches!(worker["phase"].as_str(),Some("failed"|"lost"|"needs-recovery")){return Err(format!("Own cold resume failed: {worker}").into());}
                if worker["checkpointPending"]==false && worker["activeTurn"].is_null() && worker["checkpoint"]["turnId"]!=old.checkpoint.as_ref().unwrap().turn_id.to_string() && fixture.journal.read_utterance(new_word.to_string()).await?["state"]=="landed"{return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(worker.clone());}
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(first["sessionId"],session);assert_ne!(first["identity"],serde_json::to_value(old_host)?);
        for id in &old_words{assert_eq!(fixture.journal.read_utterance(id.to_string()).await?["state"],"held");}
        let held=fixture.journal.read_utterance(old_words[0].to_string()).await?;
        fixture.service.start_held_input(Uuid::now_v7(),old_words[0].to_string(),held["revision"].as_i64().unwrap()).await?;
        tokio::time::timeout(Duration::from_secs(30),async {
            loop{
                let snapshot=fixture.journal.snapshot().await?;
                let worker=snapshot["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==track).unwrap();
                if worker["checkpointPending"]==false && worker["activeTurn"].is_null() && worker["checkpoint"]["turnId"]!=first["checkpoint"]["turnId"] && fixture.journal.read_utterance(old_words[0].to_string()).await?["state"]=="landed"{assert_eq!(worker["sessionId"],session);return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(());}
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(fixture.journal.read_utterance(old_words[1].to_string()).await?["state"],"held");
        let db=rusqlite::Connection::open(fixture.root.path().join("db"))?;
        let inputs=db.prepare("SELECT u.id FROM operations o JOIN utterances u ON u.dispatch_command=o.id WHERE o.kind='turn.start' AND json_extract(o.payload,'$.sessionId')=?1 ORDER BY o.rowid")?.query_map([&session],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(inputs,vec![fixture.words[0].0.clone(),new_word.to_string(),old_words[0].to_string()]);
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='session/resume' AND json_extract(payload,'$.sessionId')=?1",[&session],|row|row.get::<_,i64>(0))?,1);
        assert_eq!(native_git(&workspace.path,&["rev-parse","HEAD"])?,head);assert_eq!(fs::read(&index)?,index_bytes);
        assert_eq!(native_git(&fixture.root.path().join("source"),&["rev-parse","HEAD"])?,fixture.source_head);
        // A later Fresh carries the old cutoff into its new SDK conversation.
        let continued=fixture.service.prepare_fresh(Request {op_id:Uuid::now_v7(),track_id:track.clone(),session_id:session.clone()}).await?;
        let later=Uuid::now_v7();fixture.journal.stage_utterance(later,"OWN_COLD_LATER_FRESH_WORD".into(),Target::Lock(track.clone()),None,vec![]).await?;
        let input=fixture.journal.claim_input().await?.unwrap();assert_eq!(input.id,later.to_string());
        fixture.journal.apply_input(input.id,input.revision,Outcome::Classified(Decision {kind:Kind::Extend,track_id:Some(track.clone()),boundary:Some("now".into()),command:None,confidence:1.,inference_ms:0.,timed_out:false,scores:json!({})})).await?;
        let continued=finish_native_fresh(&fixture,continued,"OWN_COLD_LATER_FRESH_SUMMARY").await?;
        let owner=crate::continuation::start(&db,&continued.successor_track)?.unwrap();
        let old_hold=crate::worker_hold::current(&db,&track)?.unwrap();
        assert_eq!(crate::worker_hold::current(&db,&continued.successor_track)?,Some(old_hold));
        assert_eq!(fixture.journal.read_utterance(old_words[1].to_string()).await?["state"],"held");
        let successor=fixture.journal.workspace(continued.successor_track.clone()).await?;
        fixture.tasks.push(crate::runtime::tests::fixture_echo_worker(&fixture.service,fixture.config.clone(),fixture.editor.clone(),successor).await?);
        tokio::time::timeout(Duration::from_secs(40),async {
            loop {
                if let Some(origin)=fixture.journal.read_input_origin(later).await?
                    && origin.checkpoint.is_some() && crate::continuation::start(&db,&continued.successor_track)?.unwrap().ready {
                    assert_eq!(origin.identity.session_id,continued.successor_session);return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        let still_held=fixture.journal.read_utterance(old_words[1].to_string()).await?;
        assert_eq!(still_held["decision"]["trackId"],continued.successor_track);assert_eq!(still_held["state"],"held");assert_eq!(still_held["dispatchRunnable"],false);
        let new_starts=db.prepare("SELECT u.id FROM operations o JOIN utterances u ON u.dispatch_command=o.id WHERE o.kind='turn.start' AND json_extract(o.payload,'$.sessionId')=?1 ORDER BY o.rowid")?.query_map([&continued.successor_session],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(new_starts,vec![owner.first_input.clone(),later.to_string()]);
        fixture.service.start_held_input(Uuid::now_v7(),old_words[1].to_string(),still_held["revision"].as_i64().unwrap()).await?;
        let last=tokio::time::timeout(Duration::from_secs(30),async {
            loop {
                if let Some(origin)=fixture.journal.read_input_origin(old_words[1]).await? && origin.checkpoint.is_some() {
                    assert_eq!(origin.identity.session_id,continued.successor_session);return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(origin);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(crate::worker_hold::current(&db,&continued.successor_track)?,Some(old_hold));
        assert!(crate::continuation::closed(&db,&track)?);
        assert_eq!(native_git(&workspace.path,&["rev-parse","HEAD"])?,head);assert_eq!(fs::read(&index)?,index_bytes);
        assert_eq!(native_git(&fixture.root.path().join("source"),&["rev-parse","HEAD"])?,fixture.source_head);
        fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"schema":crate::journal::SCHEMA_VERSION,"provider":"echo","paidTurns":0,"sourceHostClosedBeforeCancel":true,"sameSessionResumedOnce":true,"newWordRanAlone":true,"oldWordNeededExplicitStart":true,"otherOldWordStayedHeld":true,"headAndIndexUnchanged":true,"sourceUnchanged":true,"inputs":inputs,"cancelled":cancelled.status(),"laterFreshSummaryThenNewWord":true,"oldWordStillNeededStartAfterTransfer":true,"holdKeptOriginalCutoff":true,"continued":continued.status(),"lastExplicitOrigin":last}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    reports.shutdown().await;
    fixture._service_stop.send_replace(true);
    let stopped = fixture.stop_hosts().await;
    fixture.editor.stop().await;
    if let Err(error) = &result {
        fs::write(
            output.join("failure.json"),
            serde_json::to_vec_pretty(
                &json!({"error":error.to_string(),"snapshot":fixture.journal.snapshot().await?}),
            )?,
        )?;
    }
    result?;
    stopped
}

#[tokio::test]
#[ignore = "pinned Muse offline echo: retry an actually received failed plan item after Fresh; dummy auth only"]
async fn native_fresh_retry_keeps_the_failed_clip_and_receives_only_the_retry_on_the_new_session()
-> Result<()> {
    let mut fixture = QueueFixture::new().await?;
    let output = PathBuf::from(std::env::var("INTERCAT_M4_FRESH_OUTPUT")?).join("retry");
    fs::create_dir_all(&output)?;
    let reports =
        crate::report_analysis::Service::start(fixture.journal.clone(), fixture.editor.clone());
    let result=async {
        use crate::{plans::{Draft,DraftItem,ItemState},plan_store::{Action,Command,Generated}};
        fixture.launch(&[0]).await?;fixture.start(0).await?;fixture.wait_workers(&[0],true).await?;
        let source=fixture.words[0].1.clone();
        let id=Uuid::now_v7();
        fixture.journal.stage_utterance(id,"OWN_NATIVE_PLAN_WITH_FAILED_ITEM".into(),Target::Lock(source.clone()),None,vec![]).await?;
        let input=fixture.journal.claim_input().await?.unwrap();assert_eq!(input.id,id.to_string());
        fixture.journal.apply_input(input.id,input.revision,Outcome::Classified(Decision {kind:Kind::Plan,track_id:Some(source.clone()),boundary:None,command:None,confidence:1.,inference_ms:0.,timed_out:false,scores:json!({})})).await?;
        let initial=fixture.journal.plans().await?.into_iter().find(|plan|plan.source_id==id.to_string()).unwrap();
        let folder=fixture.journal.plan_context(initial.id).await?.folders.into_iter().next().unwrap();
        assert!(fixture.journal.begin_plan(initial.id,initial.generation.id,initial.rev).await?);
        assert!(fixture.journal.generated_plan(initial.id,initial.generation.id,initial.rev,Generated::Draft(Draft {assumptions:vec![],conflicts:vec![],summary:"Own two dummy file inspections".into(),items:(0..2).map(|i|DraftItem {id:format!("own-{i}"),track_ref:source.clone(),folder_id:folder.clone(),title:format!("Own inspection {i}"),instruction:format!("OWN_NATIVE_MISSING_FILE_{i}"),constraints:vec![],done_when:"Own missing file is inspected".into(),after:vec![]}).collect()}),vec![]).await?);
        let ready=fixture.journal.plans().await?.into_iter().find(|plan|plan.id==initial.id).unwrap();
        fixture.journal.plan_command(Command {op_id:Uuid::now_v7(),plan_id:ready.id,revision:ready.rev,action:Action::Review}).await?;
        fixture.journal.plan_command(Command {op_id:Uuid::now_v7(),plan_id:ready.id,revision:ready.rev,action:Action::Start}).await?;
        let failed=tokio::time::timeout(Duration::from_secs(40),async {
            loop {
                fixture.journal.advance_plans().await?;
                let plan=fixture.journal.plans().await?.into_iter().find(|plan|plan.id==initial.id).unwrap();
                if plan.items[0].state==ItemState::Failed {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(plan);}
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        let held=fixture.journal.plan_command(Command {op_id:Uuid::now_v7(),plan_id:failed.id,revision:failed.rev,action:Action::Hold}).await?;
        let old=held.items[0].utterance_id.unwrap();
        let origin=fixture.journal.read_input_origin(old).await?.unwrap();assert!(origin.checkpoint.is_some());
        let db=rusqlite::Connection::open(fixture.root.path().join("db"))?;
        let old_word=native_saved_word(&db,&old.to_string())?;
        let old_clip:String=db.query_row("SELECT c.record FROM timeline_turns t JOIN timeline_clips c ON c.id=json_extract(t.record,'$.current') WHERE t.session=?1 AND t.turn=?2",rusqlite::params![origin.identity.session_id,origin.identity.turn_id.to_string()],|row|row.get(0))?;
        let worker:WorkerState=serde_json::from_value(fixture.journal.snapshot().await?["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==source).unwrap().clone())?;
        let job=fixture.service.prepare_fresh(Request {op_id:Uuid::now_v7(),track_id:source.clone(),session_id:worker.session_id.clone().unwrap()}).await?;
        let job=finish_native_fresh(&fixture,job,"OWN_NATIVE_RETRY_SUMMARY_FIRST").await?;
        let moved=fixture.journal.plans().await?.into_iter().find(|plan|plan.id==initial.id).unwrap();
        assert_eq!(moved.items[0].utterance_id,Some(old));assert_eq!(moved.items[0].track_ref,job.successor_track);
        let request=Command {op_id:Uuid::now_v7(),plan_id:moved.id,revision:moved.rev,action:Action::RetryItem {item_id:moved.items[0].plan_item_id}};
        let retry=Uuid::new_v5(&request.op_id,b"retry-plan-item");
        let retried=fixture.journal.plan_command(request.clone()).await?;
        assert_eq!(fixture.journal.plan_command(request).await?.rev,retried.rev);
        assert_eq!(fixture.journal.read_utterance(retry.to_string()).await?["freshRunnable"],false);
        let owner=crate::continuation::start(&db,&job.successor_track)?.unwrap();
        let child=fixture.journal.workspace(job.successor_track.clone()).await?;
        fixture.tasks.push(crate::runtime::tests::fixture_echo_worker(&fixture.service,fixture.config.clone(),fixture.editor.clone(),child).await?);
        let retry_origin=tokio::time::timeout(Duration::from_secs(45),async {
            loop {
                if let Some(origin)=fixture.journal.read_input_origin(retry).await?
                    && origin.checkpoint.is_some() && fixture.journal.read_utterance(retry.to_string()).await?["state"]=="landed" {
                    return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(origin);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(retry_origin.identity.session_id,job.successor_session);
        assert_ne!(retry_origin.identity.session_id,origin.identity.session_id);
        fixture.journal.advance_plans().await?;
        let after=fixture.journal.plans().await?.into_iter().find(|plan|plan.id==initial.id).unwrap();
        assert_eq!(after.items[0].utterance_id,Some(retry));assert_eq!(after.items[1].state,ItemState::Planned);assert!(!after.items[1].start_requested);assert_eq!(after.state,crate::plans::State::Held);
        assert_eq!(native_saved_word(&db,&old.to_string())?,old_word);
        assert_eq!(db.query_row("SELECT c.record FROM timeline_turns t JOIN timeline_clips c ON c.id=json_extract(t.record,'$.current') WHERE t.session=?1 AND t.turn=?2",rusqlite::params![origin.identity.session_id,origin.identity.turn_id.to_string()],|row|row.get::<_,String>(0))?,old_clip);
        let old_origin=fixture.journal.read_input_origin(old).await?.unwrap();assert_eq!(old_origin.identity,origin.identity);assert_eq!(old_origin.checkpoint,origin.checkpoint);
        let starts=db.prepare("SELECT u.id FROM operations o JOIN utterances u ON u.dispatch_command=o.id WHERE o.kind='turn.start' AND json_extract(o.payload,'$.sessionId')=?1 ORDER BY o.rowid")?.query_map([&job.successor_session],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(starts,vec![owner.first_input,retry.to_string()]);
        assert_eq!(native_git(&fixture.root.path().join("source"),&["rev-parse","HEAD"])?,fixture.source_head);
        fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"schema":crate::journal::SCHEMA_VERSION,"provider":"echo","paidTurns":0,"oldFailedOrigin":origin,"retryOrigin":retry_origin,"oldWordAndClipUnchanged":true,"oldCheckpointUnchanged":true,"newSession":true,"summaryThenRetry":true,"otherItemHeld":true,"retryInputReplayedOnce":true,"retryOutcome":after.items[0].state,"sourceUnchanged":true}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    reports.shutdown().await;
    fixture._service_stop.send_replace(true);
    let stopped = fixture.stop_hosts().await;
    fixture.editor.stop().await;
    if let Err(error) = &result {
        fs::write(
            output.join("failure.json"),
            serde_json::to_vec_pretty(
                &json!({"error":error.to_string(),"snapshot":fixture.journal.snapshot().await?}),
            )?,
        )?;
    }
    result?;
    stopped
}
