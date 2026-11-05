use intercat_daemon::{
    Result,
    fingerprint_recovery::Request,
    journal::Journal,
    project_secrets::{Entry, Load, Manifest, Prepare},
    secrets::{Key, Registration},
};
use serde_json::json;
use std::{path::Path, process::Stdio};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const VALUE: &str = "dummy-rebuilt-project-secret-猫";
const ONCE: &str = "dummy-missing-one-time-secret";
fn manifest(project: &str, revision: u64, values: &[(&str, &str)]) -> Manifest {
    Load {
        project_id: project.into(),
        revision,
        entries: values
            .iter()
            .map(|(name, value)| Entry {
                name: (*name).into(),
                value: (*value).into(),
            })
            .collect(),
    }
    .prepare(&Key::new([7; 32]))
    .unwrap()
    .manifest
}
async fn save(journal: &Journal, manifest: &Manifest) -> Result<()> {
    let catalog = journal
        .prepare_project_secrets(Prepare {
            op_id: Uuid::now_v7(),
            project_id: manifest.project_id.clone(),
            revision: manifest.revision,
            fingerprints: manifest.fingerprints.clone(),
            key_check: Key::new([7; 32])
                .check()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await
}
async fn fixture() -> Result<(tempfile::TempDir, Journal, String)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    let path = root.path().join("project");
    std::fs::create_dir(&path)?;
    assert!(
        std::process::Command::new("/usr/bin/git")
            .args(["init", "-q", "-b", "main"])
            .arg(&path)
            .status()?
            .success()
    );
    journal
        .configure_project(
            Uuid::now_v7(),
            intercat_daemon::project::inspect(path).await?,
        )
        .await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let project = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    save(&journal, &manifest(&project, 0, &[])).await?;
    Ok((root, journal, project))
}
fn backup(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    rusqlite::Connection::open(source)?.execute(
        "VACUUM INTO ?1",
        [target.join("events.sqlite").to_str().unwrap()],
    )?;
    Ok(())
}

#[tokio::test]
async fn restores_proven_project_fingerprints_but_keeps_missing_once_history_blocked() -> Result<()>
{
    let (root, journal, project) = fixture().await?;
    let restored = root.path().join("restored");
    backup(&root.path().join("events.sqlite"), &restored)?;
    let saved = manifest(&project, 1, &[("PROJECT_TOKEN", VALUE)]);
    save(&journal, &saved).await?;
    rusqlite::Connection::open(root.path().join("events.sqlite"))?.execute(
        "INSERT INTO msp_positions VALUES('session','track',NULL)",
        [],
    )?;
    let key = Key::new([7; 32]);
    let latest = journal
        .register_fingerprint(Registration {
            op_id: Uuid::now_v7(),
            track_id: "track".into(),
            session_id: "session".into(),
            name: "ONCE_TOKEN".into(),
            byte_len: ONCE.len(),
            tag: key
                .tag(ONCE.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            key_check: key.check().iter().map(|b| format!("{b:02x}")).collect(),
        })
        .await?;
    let receipt = intercat_daemon::fingerprint_receipt::Receipt {
        catalog: latest.current,
        proof: latest.proof,
    };
    let copy = Journal::open(restored.join("events.sqlite"), Key::new([7; 32])).await?;
    let id = Uuid::now_v7();
    let request = || Request {
        op_id: id,
        receipt: receipt.clone(),
        manifest: saved.clone(),
    };
    let result = copy.rebuild_fingerprints(request()).await?;
    assert_eq!(result["projects"], json!([]));
    assert_eq!(result["sessions"][0]["sessionId"], "session");
    assert_eq!(result["rebuiltProjects"], json!([project]));
    assert_eq!(result["historyReviewRequired"], true);
    assert_eq!(
        copy.session_mask("track", "session").await?.redact(VALUE),
        "<secret:PROJECT_TOKEN>"
    );
    let catalog = copy.fingerprint_catalog().await?;
    assert!(catalog.current.revision > receipt.catalog.revision);
    assert_eq!(copy.rebuild_fingerprints(request()).await?, result);
    assert_eq!(copy.fingerprint_catalog().await?.current, catalog.current);
    drop(copy);
    let copy = Journal::open(restored.join("events.sqlite"), Key::new([7; 32])).await?;
    assert_eq!(
        copy.fingerprint_recovery(Some(receipt.clone())).await?,
        Some(result)
    );
    assert!(
        copy.fingerprint_catalog()
            .await?
            .accept(Some(&receipt))
            .is_err()
    );
    let mut changed = request();
    changed.manifest = manifest(&project, 2, &[]);
    assert!(copy.rebuild_fingerprints(changed).await.is_err());
    for file in ["events.sqlite", "events.sqlite-wal"] {
        if let Ok(bytes) = std::fs::read(restored.join(file)) {
            assert!(
                !bytes
                    .windows(VALUE.len())
                    .any(|part| part == VALUE.as_bytes())
            );
            assert!(
                !bytes
                    .windows(ONCE.len())
                    .any(|part| part == ONCE.as_bytes())
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn wrong_or_incomplete_saved_values_roll_back_without_replacing_any_receipt() -> Result<()> {
    let (root, journal, project) = fixture().await?;
    let restored = root.path().join("restored");
    backup(&root.path().join("events.sqlite"), &restored)?;
    save(
        &journal,
        &manifest(&project, 1, &[("OLD_TOKEN", "retired-value")]),
    )
    .await?;
    let saved = manifest(&project, 2, &[("PROJECT_TOKEN", VALUE)]);
    save(&journal, &saved).await?;
    let latest = journal.fingerprint_catalog().await?;
    let receipt = intercat_daemon::fingerprint_receipt::Receipt {
        catalog: latest.current,
        proof: latest.proof,
    };
    let copy = Journal::open(restored.join("events.sqlite"), Key::new([7; 32])).await?;
    let before = copy.fingerprint_catalog().await?.current;
    for candidate in [
        saved,
        manifest(&project, 2, &[("PROJECT_TOKEN", "wrong-value")]),
    ] {
        assert!(
            copy.rebuild_fingerprints(Request {
                op_id: Uuid::now_v7(),
                receipt: receipt.clone(),
                manifest: candidate
            })
            .await
            .is_err()
        );
        assert_eq!(copy.fingerprint_catalog().await?.current, before);
        assert!(
            copy.fingerprint_recovery(Some(receipt.clone()))
                .await?
                .is_none()
        );
    }
    Ok(())
}

#[tokio::test]
async fn private_rebuild_command_never_opens_rpc_and_normal_boot_still_requires_review()
-> Result<()> {
    let (root, journal, project) = fixture().await?;
    let restored = root.path().join("restored");
    backup(&root.path().join("events.sqlite"), &restored)?;
    let saved = manifest(&project, 1, &[("PROJECT_TOKEN", VALUE)]);
    save(&journal, &saved).await?;
    let latest = journal.fingerprint_catalog().await?;
    let receipt = intercat_daemon::fingerprint_receipt::Receipt {
        catalog: latest.current,
        proof: latest.proof,
    };
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_intercatd"))
        .args(["rebuild-fingerprints", restored.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    child.stdin.take().unwrap().write_all(format!("{}\n",json!({"secretKey":"07".repeat(32),"request":Request {op_id:Uuid::now_v7(),receipt:receipt.clone(),manifest:saved}})).as_bytes()).await?;
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output()).await??;
    assert!(result.status.success());
    let result: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(result["recovery"]["historyReviewRequired"], true);
    assert!(!restored.join("run/rpc.sock").exists());
    assert!(!restored.join("hosts").exists());
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_intercatd"))
        .args([
            "serve",
            restored.to_str().unwrap(),
            restored.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!(
                "{}\n",
                json!({"token":"13".repeat(32),"secretKey":"07".repeat(32),"receipt":receipt})
            )
            .as_bytes(),
        )
        .await?;
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output()).await??;
    assert!(!result.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout)?["error"]["code"],
        "FINGERPRINT_RECEIPT_MISMATCH"
    );
    assert!(!restored.join("run/rpc.sock").exists());
    Ok(())
}
