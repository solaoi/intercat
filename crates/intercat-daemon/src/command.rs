//! Bounded, cancellable child execution shared by isolated tools and checks.
use crate::{Result, process::Tracker};
use std::{ffi::OsStr, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::watch,
};
const OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub cancelled: bool,
    pub timed_out: bool,
}
impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.cancelled && !self.timed_out
    }
    pub(crate) fn stdout_text(&self) -> String {
        retained_text(&self.stdout, self.truncated)
    }
    pub(crate) fn stderr_text(&self) -> String {
        retained_text(&self.stderr, self.truncated)
    }
    pub(crate) fn require_success(self, step: &str) -> Result<Self> {
        if self.success() {
            return Ok(self);
        }
        Err(format!(
            "{step} failed (exit {:?}): {}",
            self.code,
            self.stderr_text()
        )
        .into())
    }
}

fn retained_text(bytes: &[u8], truncated: bool) -> String {
    if truncated {
        // A cutoff can split a multiline secret anywhere in the retained
        // bytes. No incomplete fragment is safe to redact by exact value.
        "[output withheld: truncated before secret scan]\n".into()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

async fn read_bounded(mut stream: impl AsyncRead + Unpin) -> std::io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok((kept, truncated));
        }
        let retain = count.min(OUTPUT_LIMIT - kept.len());
        kept.extend_from_slice(&buffer[..retain]);
        truncated |= retain < count;
    }
}

struct Drain(tokio::task::JoinHandle<std::io::Result<(Vec<u8>, bool)>>);
impl Drain {
    async fn finish(&mut self) -> Result<(Vec<u8>, bool)> {
        Ok(tokio::time::timeout(Duration::from_secs(2), &mut self.0)
            .await
            .map_err(|_| "A command left its output pipe open.")???)
    }
}
impl Drop for Drain {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Every child inherits a unique marker. On completion, timeout, cancellation,
/// or dropped futures, kill only descendants whose identities we revalidate.
struct Children(Tracker);
impl Drop for Children {
    fn drop(&mut self) {
        for _ in 0..2 {
            if let Ok(scan) = self.0.refresh() {
                for process in scan.processes {
                    let _ = self.0.terminate(process.identity, true);
                }
            }
        }
    }
}

/// A fixed, non-interactive launcher holds before exec until `run` has recorded
/// its PID/start time and process group. Even an instant command cannot exit
/// between spawn and registration. Input is a private file, or /dev/null.
pub(crate) fn gated_command(program: impl AsRef<OsStr>, input: Option<&Path>) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .env_clear()
        .arg("-c")
        .arg("IFS= read -r intercat_ready || exit 125\n[ \"$intercat_ready\" = start ] || exit 125\nintercat_input=$1\nshift\nexec \"$@\" < \"$intercat_input\"")
        .arg("intercat-command")
        .arg(input.unwrap_or(Path::new("/dev/null")))
        .arg(program);
    command
}

pub(crate) async fn run(
    command: &mut Command,
    mut cancel: watch::Receiver<bool>,
    timeout: Duration,
) -> Result<Output> {
    if *cancel.borrow() {
        return Ok(Output {
            code: None,
            stdout: vec![],
            stderr: vec![],
            truncated: false,
            cancelled: true,
            timed_out: false,
        });
    }
    let marker = uuid::Uuid::now_v7().to_string();
    let mut child = command
        .env("INTERCAT_HOST", &marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let identity = crate::process::identity(child.id().ok_or("Missing command process ID.")?)?;
    let mut children = Children(Tracker::new_process_group(marker, identity)?);
    let mut stdout = Drain(tokio::spawn(read_bounded(child.stdout.take().unwrap())));
    let mut stderr = Drain(tokio::spawn(read_bounded(child.stderr.take().unwrap())));
    let mut cancelled = *cancel.borrow();
    let mut gate = child.stdin.take().ok_or("Missing command startup gate.")?;
    if !cancelled {
        gate.write_all(b"start\n").await?;
    }
    drop(gate);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    let mut timed_out = false;
    let mut cancellation_open = true;
    let status = loop {
        tokio::select! {
            result=child.wait()=>break Some(result?),
            _=&mut deadline=>{timed_out=true;break None;},
            result=cancel.changed(),if cancellation_open=>{if result.is_err() {cancellation_open=false;} else if *cancel.borrow() {cancelled=true;break None;}},
            _=interval.tick()=>{let _=children.0.refresh();},
        }
    };
    // Includes detached marked descendants, not only the direct child.
    if status.is_none() {
        let _ = child.start_kill();
    }
    let _ = children.0.refresh();
    drop(children);
    let _ = child.wait().await;
    let (stdout, so) = stdout.finish().await?;
    let (stderr, se) = stderr.finish().await?;
    Ok(Output {
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        truncated: so || se,
        cancelled,
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_cannot_execute_before_tracking_and_keeps_arguments_literal() -> Result<()> {
        let root = tempfile::tempdir()?;
        let payload = root.path().join("literal; $(touch not-executed)");
        let mut command = gated_command("/usr/bin/touch", None);
        command
            .arg(&payload)
            .current_dir(root.path())
            .stdin(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let identity = crate::process::identity(child.id().unwrap())?;
        let tracker = Tracker::new_process_group("gate-test".into(), identity)?;
        assert!(crate::process::is_alive(identity));
        assert!(!payload.exists());
        child.stdin.take().unwrap().write_all(b"start\n").await?;
        assert!(child.wait().await?.success());
        assert!(payload.exists());
        assert!(!root.path().join("not-executed").exists());
        assert_eq!(
            tracker.terminate(identity, true)?,
            crate::process::SignalResult::Gone
        );
        Ok(())
    }

    #[tokio::test]
    async fn instant_commands_are_registered_before_they_can_exit() -> Result<()> {
        for code in [0, 7, 0, 23].into_iter().cycle().take(64) {
            let mut command = gated_command("/bin/sh", None);
            command.args(["-c", &format!("exit {code}")]);
            let output = run(
                &mut command,
                watch::channel(false).1,
                Duration::from_secs(5),
            )
            .await?;
            assert_eq!(output.code, Some(code));
            assert!(!output.cancelled);
            assert!(output.stdout.is_empty());
        }
        Ok(())
    }

    #[tokio::test]
    async fn input_file_reaches_payload_without_becoming_gate_commands() -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = root.path().join("input; $(touch not-executed)");
        let bytes = b"start\n$(touch not-executed)\nsecond\0line\n";
        std::fs::write(&input, bytes)?;
        let mut command = gated_command("/bin/cat", Some(&input));
        command.current_dir(root.path());
        let result = run(
            &mut command,
            watch::channel(false).1,
            Duration::from_secs(5),
        )
        .await?;
        assert!(result.success());
        assert_eq!(result.stdout, bytes);
        assert!(!root.path().join("not-executed").exists());
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_or_dropping_a_command_stops_its_children_and_keeps_other_hosts_alive()
    -> Result<()> {
        for abort in [false, true] {
            let root = tempfile::tempdir()?;
            let other = crate::test_host::IdleTestHost::new()?;
            let record = root.path().join("own-processes.json");
            let written = record.clone();
            let (stop, stopping) = watch::channel(false);
            let task = tokio::spawn(async move {
                let mut command = gated_command("/usr/bin/python3", None);
                command.args([
                    "-c",
                    "import json,os,subprocess,sys,time\np=subprocess.Popen(['/bin/sleep','30'])\nwith open(sys.argv[1],'w') as f: json.dump({'host':os.getpid(),'child':p.pid},f)\ntime.sleep(30)\n",
                ]).arg(written);
                run(&mut command, stopping, Duration::from_secs(30)).await
            });
            let identities = tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if let Ok(bytes) = std::fs::read(&record)
                        && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                        && let (Some(host), Some(child)) =
                            (value["host"].as_u64(), value["child"].as_u64())
                    {
                        break Ok::<_, Box<dyn std::error::Error + Send + Sync>>([
                            crate::process::identity(host.try_into()?)?,
                            crate::process::identity(child.try_into()?)?,
                        ]);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await??;
            if abort {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                stop.send(true)?;
                assert!(task.await??.cancelled);
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                while identities.into_iter().any(crate::process::is_alive) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            assert!(crate::process::is_alive(other.identity()?));
        }
        Ok(())
    }

    #[test]
    fn truncated_output_cannot_expose_a_partial_multiline_secret() {
        let partial = b"before\nfirst line of a private value\npartial second";
        let rendered = super::retained_text(partial, true);
        assert!(!rendered.contains("private value"));
        assert!(!rendered.contains("partial second"));
        assert!(rendered.contains("withheld"));
    }
}
