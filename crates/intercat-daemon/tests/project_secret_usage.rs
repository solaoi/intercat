use intercat_daemon::{
    Result,
    host::{Boundary, Host},
    msp::Event,
    project_secret_usage::{self, HostUse, Record},
    project_secrets::Descriptor,
};
use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

#[tokio::test]
async fn retired_host_children_are_visible_and_only_attributed_children_can_be_stopped()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let program = root.path().join("host");
    fs::write(
        &program,
        r#"#!/usr/bin/env python3
import subprocess,sys,json
child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
print(json.dumps({'jsonrpc':'2.0','childPid':child.pid}),flush=True)
sys.stdin.read()
"#,
    )?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
    let mut host = Host::spawn(
        &program,
        &root.path().join("data"),
        root.path(),
        &std::env::var_os("PATH").unwrap(),
        None,
        Boundary {
            profile: intercat_daemon::host::Profile::Worker,
            secrets: &[],
            rpc: &root.path().join("rpc"),
            denied: &[],
        },
    )
    .await?;
    let Some(Event::Frame(frame)) =
        tokio::time::timeout(Duration::from_secs(5), host.connection.next_event()).await?
    else {
        return Err("Missing child identity".into());
    };
    let child =
        intercat_daemon::process::identity(frame.frame["childPid"].as_u64().unwrap() as u32)?;
    let record = Record {
        host: HostUse {
            project_id: "a".repeat(64),
            nonce: host.nonce.clone(),
            track_id: "track".into(),
            repair_run_id: None,
            revision: 1,
            fingerprints: vec![Descriptor {
                name: "TOKEN".into(),
                byte_len: 5,
                tag: "b".repeat(64),
            }],
            workspace: root.path().canonicalize()?,
            identity: host.identity,
        },
        kept: vec![],
    };
    let mut outsider = std::process::Command::new("/bin/sleep")
        .arg("60")
        .current_dir(root.path())
        .spawn()?;
    let result = async {
        let usage = project_secret_usage::scan(std::slice::from_ref(&record))?;
        assert!(
            usage[0]
                .processes
                .iter()
                .any(|p| p.identity == child && p.can_stop)
        );
        assert!(
            !usage[0]
                .processes
                .iter()
                .any(|p| p.identity.pid == outsider.id())
        );
        host.retire().await?;
        assert!(intercat_daemon::process::is_alive(child));
        let usage = project_secret_usage::scan(std::slice::from_ref(&record))?;
        assert_eq!(usage[0].processes.len(), 1);
        assert!(!usage[0].processes[0].host);
        let journal = intercat_daemon::journal::Journal::open(
            root.path().join("events.sqlite"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        journal.project_host_use(record.host.clone()).await?;
        journal
            .project_use_kept(project_secret_usage::Decision {
                nonce: record.host.nonce.clone(),
                identity: child,
                keep: true,
            })
            .await?;
        drop(journal);
        let journal = intercat_daemon::journal::Journal::open(
            root.path().join("events.sqlite"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        let retained = project_secret_usage::scan(&journal.project_uses().await?)?;
        assert!(retained[0].processes[0].kept);
        assert_eq!(retained[0].processes[0].identity, child);
        project_secret_usage::stop(&record, child).await?;
        assert!(!intercat_daemon::process::is_alive(child));
        assert!(
            project_secret_usage::stop(&record, intercat_daemon::process::identity(outsider.id())?)
                .await
                .is_err()
        );
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let _ = outsider.kill();
    let _ = outsider.wait();
    if intercat_daemon::process::is_alive(child) {
        let _ = project_secret_usage::stop(&record, child).await;
    }
    result
}
