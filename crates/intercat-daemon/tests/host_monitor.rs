use intercat_daemon::{
    Result,
    host::{Boundary, Host},
    host_monitor::OutputMonitor,
    msp::Event,
    output_flow::Flow,
    process::{self, Tracker},
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    os::unix::process::CommandExt,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

#[tokio::test]
async fn output_overflow_forces_the_host_and_its_children_without_stopping_another_host()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let program = directory.path().join("output-host");
    fs::write(
        &program,
        r#"#!/usr/bin/env python3
import json, signal, subprocess, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
child = subprocess.Popen([sys.executable, '-c', 'import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(60)'])
print(json.dumps({'jsonrpc':'2.0','method':'turn/started','params':{'sessionId':'overflow-fixture','turnId':'01900000-0000-7000-8000-000000000001','childPid':child.pid}}), flush=True)
for _ in range(10):
    print(json.dumps({'jsonrpc':'2.0','method':'diagnostic','params':{'text':'x'*1000000}}), flush=True)
time.sleep(60)
"#,
    )?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
    let mut host = Host::spawn(
        &program,
        &directory.path().join("host"),
        directory.path(),
        &std::env::var_os("PATH").ok_or("PATH is missing")?,
        None,
        Boundary {
            steps: None,
            profile: intercat_daemon::host::Profile::Worker,
            secrets: &[],
            rpc: &directory.path().join("rpc"),
            denied: &[],
        },
    )
    .await?;
    let identity = host.identity;
    let tracker = host.tracker.clone();
    let mut other = Command::new("/bin/sleep").arg("60").spawn()?;
    let other_identity = process::identity(other.id())?;
    let result = async {
        let first =
            tokio::time::timeout(Duration::from_secs(10), host.connection.next_event()).await?;
        let Some(Event::Frame(started)) = first else {
            return Err("Missing fixture start".into());
        };
        let child = process::identity(
            started.frame["params"]["childPid"]
                .as_u64()
                .ok_or("Missing child PID")? as u32,
        )?;
        drop(started);
        tokio::time::timeout(Duration::from_secs(15), async {
            while process::is_alive(identity) || process::is_alive(child) {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await?;
        if !host.connection.client.output_flow().interrupted || !process::is_alive(other_identity) {
            return Err("Host isolation failed".into());
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    if let Err(error) = &result {
        eprintln!(
            "{error}; root={identity:?}; own={:?}",
            tracker.lock().unwrap().refresh()?.processes
        );
    }
    {
        let mut guard = tracker.lock().unwrap();
        for process in guard.refresh()?.processes {
            let _ = guard.terminate(process.identity, true);
        }
    }
    let _ = other.kill();
    let _ = other.wait();
    host.shutdown().await?;
    result
}

#[tokio::test]
async fn acknowledged_interrupt_and_normal_shutdown_do_not_force_the_host() -> Result<()> {
    for acknowledged in [false, true] {
        let mut root = Command::new("/bin/sleep")
            .arg("60")
            .process_group(0)
            .spawn()?;
        let identity = process::identity(root.id())?;
        let tracker = Arc::new(Mutex::new(Tracker::new_process_group(
            uuid::Uuid::now_v7().to_string(),
            identity,
        )?));
        let flow = Flow::default();
        let monitor = OutputMonitor::start(flow.clone(), tracker, identity);
        let result = async {
            if acknowledged {
                flow.interrupt();
                flow.settle_interrupt();
                tokio::time::timeout(Duration::from_secs(1), async {
                    while !monitor.is_finished() {
                        tokio::task::yield_now().await;
                    }
                })
                .await?;
            }
            monitor.stop().await?;
            if !process::is_alive(identity) {
                return Err("An acknowledged or inactive host was forced to stop".into());
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }
        .await;
        let _ = root.kill();
        root.wait()?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn abandoning_an_overflowed_host_keeps_its_stop_deadline() -> Result<()> {
    let nonce = uuid::Uuid::now_v7().to_string();
    let mut root = Command::new("/bin/sleep")
        .arg("60")
        .env("INTERCAT_HOST", &nonce)
        .process_group(0)
        .spawn()?;
    let identity = process::identity(root.id())?;
    let reaped = tokio::task::spawn_blocking(move || root.wait());
    let tracker = Arc::new(Mutex::new(Tracker::new_process_group(nonce, identity)?));
    let flow = Flow::default();
    let monitor = OutputMonitor::start(flow.clone(), tracker.clone(), identity);
    flow.interrupt();
    drop(monitor);
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        while process::is_alive(identity) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    // Keep failure cleanup independent of the assertion under test.
    tracker.lock().unwrap().terminate(identity, true)?;
    tokio::time::timeout(Duration::from_secs(2), reaped).await???;
    result?;
    Ok(())
}
