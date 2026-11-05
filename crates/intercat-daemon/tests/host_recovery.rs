use intercat_daemon::{
    Result,
    host::{Boundary, Host},
    msp::Event,
    process::{self, SignalResult},
};
use std::{fs, os::unix::fs::PermissionsExt, process::Command, time::Duration};

#[tokio::test]
async fn failed_setup_and_spawn_leave_proof_that_no_host_was_started() -> Result<()> {
    let root = tempfile::tempdir()?;
    let hosts = root.path().join("hosts");
    let missing = root.path().join("missing");
    for (name, denied) in [("setup", vec![missing.clone()]), ("spawn", vec![])] {
        assert!(
            Host::spawn(
                &missing,
                &hosts.join(name),
                root.path(),
                std::ffi::OsStr::new("/usr/bin:/bin"),
                None,
                Boundary {
                    profile: intercat_daemon::host::Profile::Worker,
                    secrets: &[],
                    rpc: &root.path().join("rpc"),
                    denied: &denied
                }
            )
            .await
            .is_err()
        );
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(hosts.join(name).join("processes.json"))?)?;
        assert_eq!(record["launching"], false);
        assert_eq!(record["root"], serde_json::Value::Null);
    }
    let mut restored = process::Tracker::restore(&hosts, root.path(), None)?.unwrap();
    assert!(restored.refresh()?.is_empty());
    assert!(!restored.has_live_host());
    Ok(())
}

#[tokio::test]
async fn retiring_a_connection_preserves_children_and_next_host_inherits_their_ownership()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let program = root.path().join("host");
    fs::write(
        &program,
        r#"#!/bin/sh
/bin/sleep 30 &
echo 'dummy-historical-secret-from-stderr' >&2
echo "{\"jsonrpc\":\"2.0\",\"childPid\":$!}"
wait
"#,
    )?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755))?;
    let rpc = root.path().join("rpc");
    fs::create_dir(&rpc)?;
    let mut first = Some(
        Host::spawn(
            &program,
            &root.path().join("one"),
            root.path(),
            std::ffi::OsStr::new("/usr/bin:/bin"),
            None,
            Boundary {
                profile: intercat_daemon::host::Profile::Worker,
                secrets: &[],
                rpc: &rpc,
                denied: &[],
            },
        )
        .await?,
    );
    let mut second = None::<Host>;
    let old_tracker = first.as_ref().unwrap().tracker.clone();
    let mut unrelated = Command::new("/bin/sleep").arg("30").spawn()?;
    let result = async {
        let host = first.as_mut().unwrap();
        let Some(Event::Frame(frame)) =
            tokio::time::timeout(Duration::from_secs(5), host.connection.next_event()).await?
        else {
            return Err("Dummy host did not report its child".into());
        };
        let child = process::identity(frame.frame["childPid"].as_u64().unwrap() as u32)?;
        // No execution loop is consuming this mock host. Its independent
        // monitor must still record the child for a later daemon restart.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let record: serde_json::Value =
                    serde_json::from_slice(&fs::read(root.path().join("one/processes.json"))?)?;
                if record["processes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|process| process["identity"]["pid"] == child.pid)
                {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await??;
        assert_eq!(
            old_tracker.lock().unwrap().terminate(host.identity, true)?,
            SignalResult::Sent
        );
        first.take().unwrap().retire().await?;
        assert!(
            process::is_alive(child),
            "Retiring a connection must not imply its child stopped"
        );
        let new = Host::spawn(
            &program,
            &root.path().join("two"),
            root.path(),
            std::ffi::OsStr::new("/usr/bin:/bin"),
            None,
            Boundary {
                profile: intercat_daemon::host::Profile::Worker,
                secrets: &[],
                rpc: &rpc,
                denied: &[],
            },
        )
        .await?;
        new.tracker
            .lock()
            .unwrap()
            .inherit(&old_tracker.lock().unwrap())?;
        second = Some(new);
        let new = second.as_ref().unwrap();
        for directory in ["one", "two"] {
            assert!(
                !fs::read_to_string(root.path().join(directory).join("stderr.log"))?
                    .contains("dummy-historical-secret-from-stderr")
            );
        }
        let scan = new.tracker.lock().unwrap().refresh()?;
        assert!(scan.processes.iter().any(|p| p.identity == child));
        assert!(
            !scan
                .processes
                .iter()
                .any(|p| p.identity.pid == unrelated.id())
        );
        assert_eq!(
            new.tracker
                .lock()
                .unwrap()
                .terminate(process::identity(unrelated.id())?, true)?,
            SignalResult::NotOwned
        );
        second.take().unwrap().shutdown().await?;
        assert!(
            !process::is_alive(child),
            "Final shutdown must clean up inherited children"
        );
        assert!(process::is_alive(process::identity(unrelated.id())?));
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    if let Some(host) = first {
        let _ = host.shutdown().await;
    }
    if let Some(host) = second {
        let _ = host.shutdown().await;
    }
    let cleanup = old_tracker.lock().unwrap().refresh();
    if let Ok(scan) = cleanup {
        for child in scan.processes {
            let _ = old_tracker.lock().unwrap().terminate(child.identity, true);
        }
    }
    let _ = unrelated.kill();
    let _ = unrelated.wait();
    result
}
