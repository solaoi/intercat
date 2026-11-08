use intercat_daemon::{
    Result,
    journal::Journal,
    project_secrets::{Descriptor, Entry, Load, Prepare},
    secrets::Key,
};
use sha2::Digest;
use uuid::Uuid;
const VALUE: &str = "dummy-project-secret-猫-value";
fn hex(value: &[u8]) -> String {
    value.iter().map(|b| format!("{b:02x}")).collect()
}
async fn fixture() -> Result<(tempfile::TempDir, Journal, String)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let path = root.path().join("project");
    std::fs::create_dir(&path)?;
    assert!(
        std::process::Command::new("/usr/bin/git")
            .args(["init", "-q", "-b", "main"])
            .arg(&path)
            .status()?
            .success()
    );
    let project =
        intercat_daemon::project::inspect(uuid::Uuid::now_v7(), "Dummy project".into(), vec![path])
            .await?;
    let project_id = hex(&sha2::Sha256::digest(project.id.as_bytes()));
    journal.configure_project(Uuid::now_v7(), project).await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let project = project_id;
    assert_eq!(
        journal.snapshot().await?["projectSecrets"]["projectId"],
        project
    );
    Ok((root, journal, project))
}
fn registration(project: &str, revision: u64, entries: &[(&str, &str)]) -> Prepare {
    let key = Key::new([7; 32]);
    Prepare {
        op_id: Uuid::now_v7(),
        project_id: project.into(),
        revision,
        key_check: hex(&key.check()),
        fingerprints: entries
            .iter()
            .map(|(name, value)| Descriptor {
                name: (*name).into(),
                byte_len: value.len(),
                tag: hex(&key.tag(value.as_bytes())),
            })
            .collect(),
    }
}
fn load(project: &str, revision: u64, entries: &[(&str, &str)]) -> Load {
    Load {
        project_id: project.into(),
        revision,
        entries: entries
            .iter()
            .map(|(name, value)| Entry {
                name: (*name).into(),
                value: (*value).into(),
            })
            .collect(),
    }
}
#[tokio::test]
async fn project_witness_changes_on_removal_and_keeps_values_out_of_the_receipt() -> Result<()> {
    let (_root, journal, project) = fixture().await?;
    let before = journal
        .prepare_project_secrets(registration(&project, 1, &[("TOKEN", VALUE)]))
        .await?;
    journal.confirm_fingerprints(before.current.clone()).await?;
    let removed = journal
        .prepare_project_secrets(registration(&project, 2, &[]))
        .await?;
    let difference = serde_json::to_value(removed.proof.difference(&before.proof))?;
    assert_eq!(difference["sessions"], serde_json::json!([]));
    assert_eq!(
        difference["projects"],
        serde_json::json!([{ "projectId":project,"reason":"changed" }])
    );
    let receipt = removed.accept(Some(&intercat_daemon::fingerprint_receipt::Receipt {
        catalog: before.current,
        proof: before.proof,
    }))?;
    let encoded = serde_json::to_string(&receipt)?;
    assert!(!encoded.contains(VALUE));
    assert!(!encoded.contains("TOKEN"));
    Ok(())
}
#[tokio::test]
async fn project_values_require_a_sealed_manifest_and_are_never_written_to_sqlite() -> Result<()> {
    let (root, journal, project) = fixture().await?;
    let entries = [("NPM_TOKEN", VALUE)];
    let catalog = journal
        .prepare_project_secrets(registration(&project, 1, &entries))
        .await?;
    assert!(
        !journal.snapshot().await?["projectSecrets"]["ready"]
            .as_bool()
            .unwrap()
    );
    assert!(
        journal
            .activate_project_secrets(load(&project, 1, &entries))
            .await
            .is_err()
    );
    journal.confirm_fingerprints(catalog.current).await?;
    assert!(
        journal
            .activate_project_secrets(load(&project, 1, &[("NPM_TOKEN", "wrong dummy value")]))
            .await
            .is_err()
    );
    let config = journal
        .activate_project_secrets(load(&project, 1, &entries))
        .await?;
    assert_eq!(config.values.len(), 1);
    assert!(
        journal.snapshot().await?["projectSecrets"]["ready"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        journal
            .session_mask("future-track", "future-session")
            .await?
            .redact(VALUE),
        "<secret:NPM_TOKEN>"
    );
    drop(config);
    drop(journal);
    for entry in std::fs::read_dir(root.path())? {
        let path = entry?.path();
        if path.is_file() {
            assert!(
                !std::fs::read(path)?
                    .windows(VALUE.len())
                    .any(|bytes| bytes == VALUE.as_bytes())
            );
        }
    }
    Ok(())
}
#[tokio::test]
async fn deleting_values_retains_history_masks_and_restart_requires_the_keychain_values_again()
-> Result<()> {
    let (root, journal, project) = fixture().await?;
    let catalog = journal
        .prepare_project_secrets(registration(&project, 1, &[("TOKEN", VALUE)]))
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    journal
        .activate_project_secrets(load(&project, 1, &[("TOKEN", VALUE)]))
        .await?;
    let catalog = journal
        .prepare_project_secrets(registration(&project, 2, &[]))
        .await?;
    assert!(
        journal
            .activate_project_secrets(load(&project, 1, &[("TOKEN", VALUE)]))
            .await
            .is_err()
    );
    journal.confirm_fingerprints(catalog.current).await?;
    journal
        .activate_project_secrets(load(&project, 2, &[]))
        .await?;
    let state = journal.snapshot().await?;
    assert_eq!(state["projectSecrets"]["names"], serde_json::json!([]));
    assert_eq!(
        state["projectSecrets"]["allNames"],
        serde_json::json!(["TOKEN"])
    );
    assert_eq!(
        journal.session_mask("any", "retained").await?.redact(VALUE),
        "<secret:TOKEN>"
    );
    drop(journal);
    let reopened = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(reopened.snapshot().await?["projectSecrets"]["ready"], false);
    reopened
        .activate_project_secrets(load(&project, 2, &[]))
        .await?;
    assert!(
        reopened
            .prepare_project_secrets(registration(&project, 1, &[("TOKEN", VALUE)]))
            .await
            .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn project_registration_rejects_wrong_owners_reserved_names_order_and_reused_operations()
-> Result<()> {
    let (_root, journal, project) = fixture().await?;
    for entries in [
        vec![("PATH", VALUE)],
        vec![("Z", VALUE), ("A", VALUE)],
        vec![("A", VALUE), ("A", VALUE)],
    ] {
        assert!(
            journal
                .prepare_project_secrets(registration(&project, 1, &entries))
                .await
                .is_err()
        );
    }
    assert!(
        journal
            .prepare_project_secrets(registration(&"f".repeat(64), 1, &[("TOKEN", VALUE)]))
            .await
            .is_err()
    );
    let first = registration(&project, 1, &[("TOKEN", VALUE)]);
    let id = first.op_id;
    let catalog = journal.prepare_project_secrets(first).await?;
    let mut retry = registration(&project, 1, &[("TOKEN", VALUE)]);
    retry.op_id = id;
    assert_eq!(
        journal.prepare_project_secrets(retry).await?.current,
        catalog.current
    );
    journal.confirm_fingerprints(catalog.current).await?;
    let mut changed = registration(&project, 2, &[]);
    changed.op_id = id;
    assert!(journal.prepare_project_secrets(changed).await.is_err());
    Ok(())
}
#[tokio::test]
async fn the_external_catalog_detects_project_manifest_tampering() -> Result<()> {
    let (root, journal, project) = fixture().await?;
    let catalog = journal
        .prepare_project_secrets(registration(&project, 1, &[("TOKEN", VALUE)]))
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    drop(journal);
    rusqlite::Connection::open(root.path().join("db"))?.execute(
        "UPDATE project_secret_state SET manifest=replace(manifest,'TOKEN','OTHER')",
        [],
    )?;
    assert!(
        Journal::open(root.path().join("db"), Key::new([7; 32]))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn dispatch_waits_until_the_host_has_the_current_confirmed_revision() -> Result<()> {
    let (root, journal, project) = fixture().await?;
    let catalog = journal
        .prepare_project_secrets(registration(&project, 1, &[("TOKEN", VALUE)]))
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    journal
        .activate_project_secrets(load(&project, 1, &[("TOKEN", VALUE)]))
        .await?;
    let mut db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute(
        "INSERT INTO tracks VALUES('track','A','Fixture','waiting')",
        [],
    )?;
    db.execute(
        "INSERT INTO workers VALUES('track',?1)",
        [r#"{"secretRevision":0}"#],
    )?;
    assert!(
        intercat_daemon::project_secrets::validate_dispatch(&db.transaction()?, "track").is_err()
    );
    db.execute("UPDATE workers SET record=?1", [r#"{"secretRevision":1}"#])?;
    assert!(
        intercat_daemon::project_secrets::validate_dispatch(&db.transaction()?, "track").is_ok()
    );
    let catalog = journal
        .prepare_project_secrets(registration(&project, 2, &[]))
        .await?;
    db.execute("UPDATE workers SET record=?1", [r#"{"secretRevision":2}"#])?;
    assert!(
        intercat_daemon::project_secrets::validate_dispatch(&db.transaction()?, "track").is_err()
    );
    journal.confirm_fingerprints(catalog.current).await?;
    journal
        .activate_project_secrets(load(&project, 2, &[]))
        .await?;
    assert!(
        intercat_daemon::project_secrets::validate_dispatch(&db.transaction()?, "track").is_ok()
    );
    Ok(())
}

#[tokio::test]
async fn preflight_does_not_register_fingerprints_or_consume_an_operation() -> Result<()> {
    let (_root, journal, project) = fixture().await?;
    let before = journal.snapshot().await?;
    let catalog = journal.fingerprint_catalog().await?;
    let request = registration(&project, 1, &[("TOKEN", VALUE)]);
    let operation = request.op_id;
    journal.check_project_secrets(request).await?;
    let mut repeat = registration(&project, 1, &[("TOKEN", VALUE)]);
    repeat.op_id = operation;
    journal.check_project_secrets(repeat).await?;
    assert_eq!(journal.snapshot().await?["sequence"], before["sequence"]);
    assert_eq!(
        journal.fingerprint_catalog().await?.current,
        catalog.current
    );
    assert_eq!(
        journal
            .session_mask("track", "session")
            .await?
            .redact(VALUE),
        VALUE
    );
    let mut commit = registration(&project, 1, &[("TOKEN", VALUE)]);
    commit.op_id = operation;
    let next = journal.prepare_project_secrets(commit).await?;
    journal.confirm_fingerprints(next.current.clone()).await?;
    journal
        .activate_project_secrets(load(&project, 1, &[("TOKEN", VALUE)]))
        .await?;
    let active = journal.snapshot().await?;
    journal.hold_project_secrets().await?;
    let held = journal.snapshot().await?;
    assert_eq!(held["projectSecrets"]["ready"], false);
    assert!(held["sequence"].as_i64() > active["sequence"].as_i64());
    journal.hold_project_secrets().await?;
    assert_eq!(journal.snapshot().await?["sequence"], held["sequence"]);
    assert_eq!(journal.fingerprint_catalog().await?.current, next.current);
    journal
        .activate_project_secrets(load(&project, 1, &[("TOKEN", VALUE)]))
        .await?;
    assert_eq!(journal.snapshot().await?["projectSecrets"]["ready"], true);
    Ok(())
}

#[tokio::test]
async fn full_fingerprint_history_rejects_a_new_value_before_save_but_allows_removal() -> Result<()>
{
    let (_root, journal, project) = fixture().await?;
    for batch in 0..8 {
        let entries: Vec<_> = (0..32)
            .map(|index| {
                (
                    format!("TOKEN_{index:02}"),
                    format!("dummy-capacity-{batch}-{index}"),
                )
            })
            .collect();
        let pairs: Vec<_> = entries
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let catalog = journal
            .prepare_project_secrets(registration(&project, batch + 1, &pairs))
            .await?;
        journal
            .confirm_fingerprints(catalog.current.clone())
            .await?;
    }
    let catalog = journal
        .prepare_project_secrets(registration(&project, 9, &[]))
        .await?;
    journal
        .confirm_fingerprints(catalog.current.clone())
        .await?;
    journal
        .activate_project_secrets(load(&project, 9, &[]))
        .await?;
    assert!(
        journal
            .check_project_secrets(registration(&project, 10, &[("EXTRA", "new-dummy-value")]))
            .await
            .is_err()
    );
    assert_eq!(
        journal.fingerprint_catalog().await?.current,
        catalog.current
    );
    assert_eq!(journal.snapshot().await?["projectSecrets"]["ready"], true);
    journal
        .check_project_secrets(registration(&project, 11, &[]))
        .await?;
    let newer = journal
        .prepare_project_secrets(registration(&project, 11, &[]))
        .await?;
    journal.confirm_fingerprints(newer.current).await?;
    journal
        .activate_project_secrets(load(&project, 11, &[]))
        .await?;
    assert_eq!(journal.snapshot().await?["projectSecrets"]["revision"], 11);
    Ok(())
}
