use intercat_daemon::{
    Result,
    process::{self, SignalResult, Tracker},
};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    process::{Command, Stdio},
};

fn host_directory(root: &std::path::Path) -> Result<std::path::PathBuf> {
    let host = root.join("hosts/track/one");
    fs::create_dir_all(&host)?;
    fs::set_permissions(&host, fs::Permissions::from_mode(0o700))?;
    Ok(host)
}

#[test]
fn persisted_group_finds_a_new_unmarked_child_after_both_parents_exit() -> Result<()> {
    let root = tempfile::tempdir()?;
    let directory = host_directory(root.path())?;
    let nonce = uuid::Uuid::now_v7().to_string();
    let mut tracker = Tracker::prepare(&directory, root.path(), nonce.clone())?;
    let code = r#"import subprocess,sys
child_code="import subprocess,sys; print('ready',flush=True); sys.stdin.readline(); p=subprocess.Popen(['/bin/sleep','60'],env={}); print(p.pid,flush=True)"
p=subprocess.Popen([sys.executable,'-c',child_code],env={},start_new_session=True,stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
assert p.stdout.readline().strip()=='ready'
print(p.pid,flush=True)
sys.stdin.readline()
p.stdin.write('\n'); p.stdin.flush()
print(p.stdout.readline().strip(),flush=True)
p.wait()
"#;
    let mut launcher = Command::new("python3")
        .args(["-c", code])
        .current_dir(root.path())
        .env("INTERCAT_HOST", &nonce)
        .env("PRIVATE_DUMMY_VALUE", "must-not-be-in-ledger")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let original = process::identity(launcher.id())?;
    tracker.attach_process_group(original)?;
    let mut output = BufReader::new(launcher.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line)?;
    let group = process::identity(line.trim().parse()?)?;
    assert!(!process::has_host_marker(group.pid, &nonce)?);
    assert!(tracker.refresh()?.groups.contains(&group));
    let bytes = fs::read(directory.join("processes.json"))?;
    assert!(!String::from_utf8_lossy(&bytes).contains("must-not-be-in-ledger"));
    launcher.stdin.as_mut().unwrap().write_all(b"\n")?;
    line.clear();
    output.read_line(&mut line)?;
    let orphan = process::identity(line.trim().parse()?)?;
    launcher.wait()?;
    drop(tracker);
    let mut unrelated = Command::new("/bin/sleep")
        .arg("60")
        .current_dir(root.path())
        .spawn()?;
    let result = (|| -> Result<()> {
        assert!(!process::has_host_marker(orphan.pid, &nonce)?);
        assert!(!process::is_alive(original));
        assert!(!process::is_alive(group));
        let mut recovered = Tracker::restore(
            &root.path().join("hosts/track"),
            root.path(),
            Some(original),
        )?
        .unwrap();
        let scan = recovered.refresh()?;
        assert!(
            scan.processes
                .iter()
                .any(|process| process.identity == orphan)
        );
        assert_eq!(
            recovered.terminate(process::identity(unrelated.id())?, true)?,
            SignalResult::NotOwned
        );
        let audit = intercat_daemon::process_audit::inspect(root.path())?;
        assert_eq!(audit.hosts, 0);
        assert!(audit.live.contains(&orphan));
        assert!(!audit.live.iter().any(|item| item.pid == unrelated.id()));
        assert!(audit.issues.is_empty());
        let stopped = intercat_daemon::process_audit::stop(root.path())?;
        assert!(stopped.live.is_empty());
        assert_eq!(
            (stopped.hosts, stopped.processes, stopped.groups),
            (0, 0, 0)
        );
        assert!(stopped.issues.is_empty());
        assert!(recovered.refresh()?.is_empty());
        assert!(!process::is_alive(orphan));
        assert!(process::is_alive(process::identity(unrelated.id())?));
        Ok(())
    })();
    // Test-owned cleanup, including an assertion failure before recovery.
    if process::is_alive(orphan) {
        unsafe {
            libc::kill(orphan.pid as i32, libc::SIGKILL);
        }
    }
    let _ = unrelated.kill();
    let _ = unrelated.wait();
    result
}

#[test]
fn recovery_rejects_missing_incomplete_public_or_wrong_workspace_evidence() -> Result<()> {
    let root = tempfile::tempdir()?;
    let directory = host_directory(root.path())?;
    let mut tracker = Tracker::prepare(&directory, root.path(), uuid::Uuid::now_v7().to_string())?;
    let hosts = root.path().join("hosts/track");
    assert!(Tracker::restore(&hosts, root.path(), None).is_err());
    tracker.cancel_launch()?;
    assert!(
        Tracker::restore(&hosts, root.path(), None)?
            .unwrap()
            .refresh()?
            .is_empty()
    );
    let different = tempfile::tempdir()?;
    assert!(Tracker::restore(&hosts, different.path(), None).is_err());
    let ledger = directory.join("processes.json");
    fs::set_permissions(&ledger, fs::Permissions::from_mode(0o644))?;
    assert!(Tracker::restore(&hosts, root.path(), None).is_err());
    fs::set_permissions(&ledger, fs::Permissions::from_mode(0o600))?;
    let external = root.path().join("external.json");
    fs::rename(&ledger, &external)?;
    std::os::unix::fs::symlink(&external, &ledger)?;
    assert!(Tracker::restore(&hosts, root.path(), None).is_err());
    fs::remove_file(&ledger)?;
    assert!(Tracker::restore(&hosts, root.path(), None).is_err());
    Ok(())
}

#[test]
fn a_reused_pid_cannot_inherit_a_saved_process_or_group() -> Result<()> {
    let root = tempfile::tempdir()?;
    let directory = host_directory(root.path())?;
    let mut unrelated = Command::new("/bin/sleep")
        .arg("60")
        .process_group(0)
        .spawn()?;
    let result = (|| -> Result<()> {
        let current = process::identity(unrelated.id())?;
        let mut tracker =
            Tracker::prepare(&directory, root.path(), uuid::Uuid::now_v7().to_string())?;
        tracker.attach_process_group(current)?;
        drop(tracker);
        let path = directory.join("processes.json");
        let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        let old = (current.started_micros + 1) % 1_000_000;
        record["root"]["started_micros"] = old.into();
        record["processes"][0]["identity"]["started_micros"] = old.into();
        record["groups"][0]["started_micros"] = old.into();
        fs::write(&path, serde_json::to_vec(&record)?)?;
        let mut restored =
            Tracker::restore(&root.path().join("hosts/track"), root.path(), None)?.unwrap();
        assert!(!restored.has_live_host());
        assert!(restored.refresh()?.is_empty());
        assert_eq!(restored.terminate(current, true)?, SignalResult::NotOwned);
        restored.stop_confirmed()?;
        assert!(process::is_alive(current));
        Ok(())
    })();
    let _ = unrelated.kill();
    let _ = unrelated.wait();
    result
}
