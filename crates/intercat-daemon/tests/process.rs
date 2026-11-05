use intercat_daemon::{
    Result,
    process::{self, SignalResult, Tracker},
};
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn tracks_unmarked_system_child_and_rejects_unowned_process() -> Result<()> {
    let nonce = uuid::Uuid::now_v7().to_string();
    let mut root = Command::new("python3").args(["-c", "import subprocess,time,threading; p=subprocess.Popen(['/bin/sleep','30']); threading.Thread(target=p.wait,daemon=True).start(); print(p.pid,flush=True); time.sleep(30)"])
        .env("INTERCAT_HOST", &nonce).stdout(Stdio::piped()).spawn()?;
    let mut line = String::new();
    BufReader::new(root.stdout.take().unwrap()).read_line(&mut line)?;
    let child_pid: u32 = line.trim().parse()?;
    let mut other = Command::new("/bin/sleep").arg("30").spawn()?;
    let child_identity = process::identity(child_pid)?;
    let result = (|| -> Result<()> {
        let root_id = process::identity(root.id())?;
        assert!(process::has_host_marker(root.id(), &nonce)?);
        let mut tracker = Tracker::new(nonce, root_id)?;
        let scan = tracker.refresh()?;
        assert!(scan.processes.iter().any(|p| p.identity == child_identity));
        assert!(!scan.processes.iter().any(|p| p.identity.pid == other.id()));
        assert_eq!(
            tracker.terminate(process::identity(other.id())?, false)?,
            SignalResult::NotOwned
        );
        let mut stale = child_identity;
        stale.started_micros += 1;
        assert_eq!(tracker.terminate(stale, false)?, SignalResult::NotOwned);
        assert_eq!(
            tracker.terminate(child_identity, false)?,
            SignalResult::Sent
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while process::is_alive(child_identity) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!process::is_alive(child_identity));
        assert!(process::is_alive(root_id));
        assert!(process::is_alive(process::identity(other.id())?));
        Ok(())
    })();
    let _ = root.kill();
    let _ = root.wait();
    let _ = other.kill();
    let _ = other.wait();
    if process::is_alive(child_identity) {
        // SAFETY: this exact test-created PID and start time were just revalidated.
        unsafe {
            libc::kill(child_pid as i32, libc::SIGKILL);
        }
    }
    result
}
