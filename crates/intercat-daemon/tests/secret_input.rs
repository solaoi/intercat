use intercat_daemon::{
    Result,
    journal::Journal,
    secret_input::Request,
    secrets::{Key, Registration},
};
use uuid::Uuid;

const SECRET: &str = "dummy-only-猫-secret-value";
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn request(id: Uuid) -> Request {
    Request {
        owner: intercat_daemon::secret_input::Owner::Work,
        op_id: id,
        track_id: "track".into(),
        session_id: "session".into(),
        turn_id: Uuid::now_v7(),
        question: intercat_daemon::secret_input::Question::Msp {
            id: "prompt".into(),
            revision: "a".repeat(64),
        },
        name: "NPM_TOKEN".into(),
        value: SECRET.into(),
    }
}
async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    rusqlite::Connection::open(root.path().join("db"))?.execute(
        "INSERT INTO msp_positions VALUES('session','track',NULL)",
        [],
    )?;
    let key = Key::new([7; 32]);
    journal
        .register_fingerprint(Registration {
            op_id: Uuid::now_v7(),
            track_id: "track".into(),
            session_id: "session".into(),
            name: "NPM_TOKEN".into(),
            byte_len: SECRET.len(),
            tag: hex(&key.tag(SECRET.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    Ok((root, journal))
}
async fn seal(journal: &Journal) -> Result<()> {
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await
}

#[tokio::test]
async fn value_requires_a_confirmed_matching_fingerprint_and_a_valid_destination() -> Result<()> {
    let (_root, journal) = fixture().await?;
    assert!(
        journal
            .prepare_secret(request(Uuid::now_v7()))
            .await
            .is_err()
    );
    seal(&journal).await?;
    for field in ["value", "name", "track", "session", "revision", "op"] {
        let mut invalid = request(Uuid::now_v7());
        match field {
            "value" => invalid.value = "different dummy secret".into(),
            "name" => invalid.name = "PATH".into(),
            "track" => invalid.track_id = "other".into(),
            "session" => invalid.session_id = "other".into(),
            "revision" => {
                invalid.question = intercat_daemon::secret_input::Question::Msp {
                    id: "prompt".into(),
                    revision: "z".repeat(64),
                }
            }
            "op" => invalid.op_id = Uuid::nil(),
            _ => unreachable!(),
        }
        assert!(journal.prepare_secret(invalid).await.is_err(), "{field}");
    }
    assert!(
        journal
            .prepare_secret(request(Uuid::now_v7()))
            .await?
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn accepted_secret_resumes_once_and_never_persists_the_value() -> Result<()> {
    let (root, journal) = fixture().await?;
    seal(&journal).await?;
    let id = Uuid::now_v7();
    let mut input = request(id);
    let turn = input.turn_id;
    let install = journal.prepare_secret(input).await?.unwrap();
    input = request(id);
    input.turn_id = turn;
    assert!(journal.prepare_secret(input).await?.is_some());
    journal.secret_accepted(id).await?;
    input = request(id);
    input.turn_id = turn;
    assert!(journal.prepare_secret(input).await?.is_none());
    assert!(
        journal
            .prepare_secret(request(Uuid::now_v7()))
            .await
            .is_err()
    );
    journal.secret_restarting(id).await?;
    journal
        .secret_ready(install.target.clone(), "host-one".into())
        .await?;
    journal
        .secret_ready(install.target, "host-one".into())
        .await?;
    let snapshot = journal.snapshot().await?;
    assert_eq!(snapshot["secretInputs"][0]["state"], "ready");
    assert!(!snapshot.to_string().contains(SECRET));
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let words: Vec<String> = db
        .prepare("SELECT text FROM utterances")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(words, ["The token is in $NPM_TOKEN. Run it again."]);
    let events: String =
        db.query_row("SELECT group_concat(payload) FROM events", [], |r| r.get(0))?;
    assert!(!events.contains(SECRET));
    assert!(!events.contains(&hex(&Key::new([7; 32]).tag(SECRET.as_bytes()))));
    journal.secret_expired("host-other".into()).await?;
    assert_eq!(
        journal.snapshot().await?["secretInputs"][0]["state"],
        "ready"
    );
    journal.secret_expired("host-one".into()).await?;
    assert_eq!(
        journal.snapshot().await?["secretInputs"][0]["state"],
        "expired"
    );
    assert_eq!(
        db.query_row("SELECT state FROM utterances", [], |r| r
            .get::<_, String>(0))?,
        "held"
    );
    drop(db);
    drop(install.value);
    drop(journal);
    for entry in std::fs::read_dir(root.path())? {
        let path = entry?.path();
        if path.is_file() {
            assert!(
                !std::fs::read(path)?
                    .windows(SECRET.len())
                    .any(|w| w == SECRET.as_bytes())
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn daemon_restart_never_dispatches_an_unsent_secret_resume_instruction() -> Result<()> {
    let (root, journal) = fixture().await?;
    seal(&journal).await?;
    let id = Uuid::now_v7();
    let install = journal.prepare_secret(request(id)).await?.unwrap();
    journal.secret_accepted(id).await?;
    journal.secret_restarting(id).await?;
    journal.secret_ready(install.target, "host".into()).await?;
    drop(journal);
    let reopened = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        reopened.snapshot().await?["secretInputs"][0]["state"],
        "expired"
    );
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    assert_eq!(
        db.query_row("SELECT state FROM utterances", [], |r| r
            .get::<_, String>(0))?,
        "held"
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_holds_the_resume_instruction_and_restart_requires_reentry() -> Result<()> {
    let (root, journal) = fixture().await?;
    seal(&journal).await?;
    let id = Uuid::now_v7();
    let install = journal.prepare_secret(request(id)).await?.unwrap();
    journal.secret_accepted(id).await?;
    journal.secret_restarting(id).await?;
    journal.secret_ready(install.target, "host".into()).await?;
    journal.secret_failed(id).await?;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    assert_eq!(
        db.query_row("SELECT state FROM utterances", [], |r| r
            .get::<_, String>(0))?,
        "held"
    );
    let id = Uuid::now_v7();
    let input = request(id);
    let turn = input.turn_id;
    journal.prepare_secret(input).await?.unwrap();
    journal.secret_accepted(id).await?;
    drop(journal);
    let reopened = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        reopened.snapshot().await?["secretInputs"][0]["state"],
        "failed"
    );
    let mut retry = request(id);
    retry.turn_id = turn;
    assert!(reopened.prepare_secret(retry).await.is_err());
    Ok(())
}

#[tokio::test]
async fn host_receives_value_only_in_its_environment_and_masks_it_before_delivery() -> Result<()> {
    use intercat_daemon::{
        host::{Boundary, Host},
        msp::Event,
        secret_input::Value,
    };
    use std::{fs, os::unix::fs::PermissionsExt};
    let (root, journal) = fixture().await?;
    let program = root.path().join("fake-host");
    fs::write(
        &program,
        r#"#!/usr/bin/env python3
import json, os, sys
sys.stdin.readline()
value = os.environ.get('NPM_TOKEN', 'absent')
print(value, file=sys.stderr, flush=True)
print(json.dumps({'jsonrpc':'2.0','method':'diagnostic','params':{'text':value}}), flush=True)
sys.stdin.read()
"#,
    )?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
    for supplied in [true, false] {
        let values = if supplied {
            vec![Value::new("NPM_TOKEN".into(), SECRET.into())?]
        } else {
            vec![]
        };
        let directory = root.path().join(if supplied { "one" } else { "two" });
        let mut host = Host::spawn(
            &program,
            &directory,
            root.path(),
            &std::env::var_os("PATH").unwrap(),
            None,
            Boundary {
                steps: None,
                profile: intercat_daemon::host::Profile::Worker,
                secrets: &values,
                rpc: &root.path().join("rpc"),
                denied: &[],
            },
        )
        .await?;
        host.connection
            .client
            .set_redaction(journal.session_mask("track", "session").await?);
        host.connection
            .client
            .notify_now("release", serde_json::json!({}))?;
        let received = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            host.connection.next_event(),
        )
        .await?;
        host.shutdown().await?;
        let Some(Event::Frame(received)) = received else {
            return Err("No diagnostic frame".into());
        };
        assert_eq!(
            received.frame["params"]["text"],
            if supplied {
                "<secret:NPM_TOKEN>"
            } else {
                "absent"
            }
        );
        assert!(!fs::read_to_string(directory.join("stderr.log"))?.contains(SECRET));
    }
    Ok(())
}

async fn report_request(journal: &Journal) -> Result<Request> {
    let turn = Uuid::now_v7();
    journal
        .observe_report(intercat_daemon::reports::Report::new(
            "track".into(),
            "session".into(),
            turn,
            "report".into(),
            "Waiting for you: Provide NPM_TOKEN to continue.".into(),
        ))
        .await?;
    let snapshot = journal.snapshot().await?;
    let question = &snapshot["reportQuestions"][0];
    let mut request = request(Uuid::now_v7());
    request.turn_id = turn;
    request.question = intercat_daemon::secret_input::Question::Report {
        id: question["id"].as_str().unwrap().into(),
        revision: question["revision"].as_i64().unwrap(),
    };
    Ok(request)
}

#[tokio::test]
async fn report_secret_reserves_its_question_and_reopens_it_if_the_host_stops() -> Result<()> {
    let (_root, journal) = fixture().await?;
    seal(&journal).await?;
    let request = report_request(&journal).await?;
    let intercat_daemon::secret_input::Question::Report { id, revision } = &request.question else {
        unreachable!()
    };
    let (question, revision) = (id.clone(), *revision);
    let install = journal.prepare_secret(request).await?.unwrap();
    assert!(
        journal
            .respond_report(
                Uuid::now_v7(),
                question.clone(),
                revision,
                intercat_daemon::reports::Answer::Reply {
                    text: "A different reply".into()
                }
            )
            .await
            .is_err()
    );
    journal.secret_accepted(install.target.op_id).await?;
    journal.secret_restarting(install.target.op_id).await?;
    journal
        .secret_ready(install.target, "report-host".into())
        .await?;
    let snapshot = journal.snapshot().await?;
    assert_eq!(snapshot["reportQuestions"][0]["state"], "queued");
    assert_eq!(
        snapshot["utterances"][0]["text"],
        "The token is in $NPM_TOKEN. Run it again."
    );
    assert_eq!(snapshot["utterances"][0]["replyTo"]["id"], question);
    assert!(!snapshot.to_string().contains(SECRET));
    journal.secret_expired("report-host".into()).await?;
    let snapshot = journal.snapshot().await?;
    assert_eq!(snapshot["reportQuestions"][0]["state"], "open");
    assert!(snapshot["reportQuestions"][0]["replyUtteranceId"].is_null());
    assert_eq!(snapshot["utterances"][0]["state"], "held");
    Ok(())
}

#[tokio::test]
async fn changed_report_rejects_a_secret_before_acceptance_or_activation() -> Result<()> {
    for accepted in [false, true] {
        let (_root, journal) = fixture().await?;
        seal(&journal).await?;
        let request = report_request(&journal).await?;
        let turn = request.turn_id;
        let install = journal.prepare_secret(request).await?.unwrap();
        if accepted {
            journal.secret_accepted(install.target.op_id).await?;
            journal.secret_restarting(install.target.op_id).await?;
        }
        journal
            .observe_report(intercat_daemon::reports::Report::new(
                "track".into(),
                "session".into(),
                turn,
                "report".into(),
                "Waiting for you: A changed question.".into(),
            ))
            .await?;
        if accepted {
            assert!(
                journal
                    .secret_ready(install.target, "host".into())
                    .await
                    .is_err()
            );
        } else {
            assert!(journal.secret_accepted(install.target.op_id).await.is_err());
        }
        assert!(
            journal.snapshot().await?["utterances"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    Ok(())
}
