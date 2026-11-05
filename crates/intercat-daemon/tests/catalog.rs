use intercat_daemon::{
    Result,
    fingerprint_receipt::Receipt,
    journal::Journal,
    secrets::{Key, Registration},
};
use serde_json::json;
use std::{path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn registration(value: &str) -> Registration {
    let key = Key::new([7; 32]);
    Registration {
        op_id: Uuid::now_v7(),
        track_id: "track".into(),
        session_id: "session".into(),
        name: "TOKEN".into(),
        byte_len: value.len(),
        tag: hex(&key.tag(value.as_bytes())),
        key_check: hex(&key.check()),
    }
}
async fn fixture(root: &Path) -> Result<Journal> {
    std::fs::create_dir_all(root)?;
    let journal = Journal::open(root.join("events.sqlite"), Key::new([7; 32])).await?;
    rusqlite::Connection::open(root.join("events.sqlite"))?.execute(
        "INSERT INTO msp_positions VALUES('session','track',NULL)",
        [],
    )?;
    Ok(journal)
}
fn backup(from: &Path, to: &Path) -> Result<()> {
    rusqlite::Connection::open(from)?.execute("VACUUM INTO ?1", [to.to_str().unwrap()])?;
    Ok(())
}

#[tokio::test]
async fn external_receipt_rejects_rollback_and_resumes_either_side_of_an_interrupted_seal()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = fixture(root.path()).await?;
    let initial = journal.fingerprint_catalog().await?;
    let zero = initial.accept(None)?;
    journal.confirm_fingerprints(zero.catalog.clone()).await?;
    assert!(journal.fingerprint_catalog().await?.accept(None).is_err());
    backup(
        &root.path().join("events.sqlite"),
        &root.path().join("older.sqlite"),
    )?;
    let first = journal
        .register_fingerprint(registration("first-value"))
        .await?;
    let one = first.accept(Some(&zero))?;
    assert_eq!(one.catalog.revision, 1);
    // Keychain can still contain the old receipt, or the new receipt with its
    // final daemon acknowledgement lost. Both retain the additional masks.
    assert_eq!(first.accept(Some(&one))?, one);
    journal.confirm_fingerprints(one.catalog.clone()).await?;
    let second = journal
        .register_fingerprint(registration("second-value"))
        .await?;
    let two = second.accept(Some(&one))?;
    assert_eq!(two.catalog.revision, 2);
    assert!(
        journal
            .confirm_fingerprints(one.catalog.clone())
            .await
            .is_err()
    );
    assert!(
        journal
            .fingerprint_catalog()
            .await?
            .accept(Some(&zero))
            .is_err()
    );
    journal.confirm_fingerprints(two.catalog.clone()).await?;
    assert_eq!(
        journal.fingerprint_catalog().await?.accept(Some(&two))?,
        two
    );
    let old = Journal::open(root.path().join("older.sqlite"), Key::new([7; 32])).await?;
    assert!(old.fingerprint_catalog().await?.accept(Some(&one)).is_err());
    assert!(old.fingerprint_catalog().await?.accept(Some(&two)).is_err());
    Ok(())
}

#[tokio::test]
async fn removing_fingerprint_rows_does_not_leave_a_valid_catalog() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = fixture(root.path()).await?;
    journal
        .register_fingerprint(registration("kept-value"))
        .await?;
    rusqlite::Connection::open(root.path().join("events.sqlite"))?
        .execute("DELETE FROM secret_fingerprints", [])?;
    assert!(journal.fingerprint_catalog().await.is_err());
    drop(journal);
    assert!(
        Journal::open(root.path().join("events.sqlite"), Key::new([7; 32]))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn backup_witnesses_identify_only_changed_or_missing_sessions() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = fixture(root.path()).await?;
    let db = root.path().join("events.sqlite");
    rusqlite::Connection::open(&db)?.execute("INSERT INTO msp_positions VALUES('unchanged','track-2',NULL),('after-backup','track-3',NULL)", [])?;
    let first = journal
        .register_fingerprint(registration("before-value"))
        .await?;
    journal.confirm_fingerprints(first.current).await?;
    let mut unchanged = registration("unchanged-value");
    unchanged.session_id = "unchanged".into();
    unchanged.track_id = "track-2".into();
    let before = journal.register_fingerprint(unchanged).await?;
    journal.confirm_fingerprints(before.current.clone()).await?;
    backup(&db, &root.path().join("backup.sqlite"))?;
    let changed = journal
        .register_fingerprint(registration("after-value"))
        .await?;
    journal.confirm_fingerprints(changed.current).await?;
    let mut added = registration("new-session-value");
    added.session_id = "after-backup".into();
    added.track_id = "track-3".into();
    let latest = journal.register_fingerprint(added).await?;
    let receipt = latest.accept(Some(&intercat_daemon::fingerprint_receipt::Receipt {
        catalog: latest.sealed.clone().unwrap(),
        proof: changed.proof,
    }))?;
    let restored = Journal::open(root.path().join("backup.sqlite"), Key::new([7; 32])).await?;
    let old = restored.fingerprint_catalog().await?;
    assert!(old.accept(Some(&receipt)).is_err());
    let difference = serde_json::to_value(old.proof.difference(&receipt.proof))?;
    assert_eq!(
        difference["sessions"],
        json!([
            {"sessionId":"after-backup","trackId":"track-3","reason":"missing"},
            {"sessionId":"session","trackId":"track","reason":"changed"}
        ])
    );
    assert_eq!(difference["projects"], json!([]));
    assert_eq!(
        serde_json::to_value(latest.proof.difference(&old.proof))?["sessions"][0]["reason"],
        "unexpected"
    );
    let encoded = serde_json::to_string(&receipt)?;
    for value in [
        "before-value",
        "after-value",
        "new-session-value",
        "unchanged-value",
        "TOKEN",
    ] {
        assert!(!encoded.contains(value));
    }
    let mut forged = receipt.clone();
    forged.proof.sessions[0].track_id = "another-track".into();
    assert!(latest.accept(Some(&forged)).is_err());
    Ok(())
}

#[tokio::test]
async fn another_registration_cannot_overtake_an_unconfirmed_receipt() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = fixture(root.path()).await?;
    let first = registration("first-value");
    let operation = first.op_id;
    let pending = journal.register_fingerprint(first).await?;
    let mut retry = registration("first-value");
    retry.op_id = operation;
    assert_eq!(
        journal.register_fingerprint(retry).await?.current,
        pending.current
    );
    assert!(
        journal
            .register_fingerprint(registration("second-value"))
            .await
            .is_err()
    );
    assert_eq!(
        journal.fingerprint_catalog().await?.current,
        pending.current
    );
    journal.confirm_fingerprints(pending.current).await?;
    assert_eq!(
        journal
            .register_fingerprint(registration("second-value"))
            .await?
            .current
            .revision,
        2
    );
    Ok(())
}

async fn boot(
    directory: &Path,
    receipt: Option<&Receipt>,
) -> Result<(
    tokio::process::Child,
    BufReader<tokio::process::ChildStdout>,
    tokio::process::ChildStdin,
)> {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_intercatd"))
        .arg("serve")
        .arg(directory)
        .arg(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(
            format!(
                "{}\n",
                json!({"token":"13".repeat(32),"secretKey":"07".repeat(32),"receipt":receipt})
            )
            .as_bytes(),
        )
        .await?;
    let output = BufReader::new(child.stdout.take().unwrap());
    Ok((child, output, input))
}

#[tokio::test]
async fn process_checks_receipts_and_waits_for_confirmation_before_binding_rpc() -> Result<()> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let journal = fixture(&source).await?;
    let zero = journal.fingerprint_catalog().await?.accept(None)?;
    journal.confirm_fingerprints(zero.catalog.clone()).await?;
    let old = root.path().join("old");
    std::fs::create_dir(&old)?;
    backup(&source.join("events.sqlite"), &old.join("events.sqlite"))?;
    let latest = journal
        .register_fingerprint(registration("latest-value"))
        .await?
        .accept(Some(&zero))?;
    journal.confirm_fingerprints(latest.catalog.clone()).await?;
    let current = root.path().join("current");
    std::fs::create_dir(&current)?;
    backup(
        &source.join("events.sqlite"),
        &current.join("events.sqlite"),
    )?;
    let (mut child, mut output, input) = boot(&old, Some(&latest)).await?;
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        output.read_line(&mut line),
    )
    .await??;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line)?["error"]["code"],
        "FINGERPRINT_RECEIPT_MISMATCH"
    );
    drop(input);
    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await??
            .success()
    );
    assert!(!old.join("run/rpc.sock").exists());
    assert!(!old.join("hosts").exists());
    let (mut child, mut output, input) = boot(&current, Some(&latest)).await?;
    line.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        output.read_line(&mut line),
    )
    .await??;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line)?["fingerprints"],
        json!(latest)
    );
    assert!(!current.join("run/rpc.sock").exists());
    drop(input); // A failed Keychain write must not be treated as confirmation.
    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await??
            .success()
    );
    assert!(!current.join("run/rpc.sock").exists());
    Ok(())
}

#[tokio::test]
async fn bootstrap_accepts_multi_session_witnesses_larger_than_a_single_seal() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = fixture(root.path()).await?;
    let mut receipt = journal.fingerprint_catalog().await?.accept(None)?;
    journal
        .confirm_fingerprints(receipt.catalog.clone())
        .await?;
    for index in 0..20 {
        let session = format!("session-{index:02}");
        rusqlite::Connection::open(root.path().join("events.sqlite"))?.execute(
            "INSERT INTO msp_positions VALUES(?1,'track',NULL)",
            [&session],
        )?;
        let mut request = registration("dummy-witness-value");
        request.session_id = session;
        receipt = journal
            .register_fingerprint(request)
            .await?
            .accept(Some(&receipt))?;
        journal
            .confirm_fingerprints(receipt.catalog.clone())
            .await?;
    }
    assert!(serde_json::to_vec(&receipt)?.len() > 1024);
    let (mut child, mut output, input) = boot(root.path(), Some(&receipt)).await?;
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        output.read_line(&mut line),
    )
    .await??;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line)?["fingerprints"],
        json!(receipt)
    );
    drop(input);
    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await??
            .success()
    );
    assert!(!root.path().join("run/rpc.sock").exists());
    Ok(())
}
