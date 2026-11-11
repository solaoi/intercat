//! Prepare an owned, hash-pinned host. No user settings are changed.

use crate::{Result, msp::Connection, process};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::process::Command;

pub const BINARY_SHA256: &str = "1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03";
pub const BINARY_VERSION: &str = "Muse Code 1.4.14 (1.4.14-R7309.1)";
const REPORT: &str = "---\nname: intercat-report\ndescription: Report completed work and explicit waiting questions for Intercat.\nintercat.group: false\n---\nReport in the user's last language. Start with one line stating what changed. Then list each change with its reason, report actual test results, and state the next step. Do not claim a test ran unless it did. When the user denies an action, skip it, finish the remaining allowed work, and report it as not run. Do not ask for the same permission again in the final report. Write exactly one standalone result footer: `Result: done — reason`, `Result: partial — reason`, or `Result: failed — reason`. Use done only when the requested work and its completion criteria were met; partial means some requested work remains, and failed means the requested result was not achieved. Keep the reason in the user's language. Put this footer after the report and before any waiting questions. End with `Waiting for you: ...` only when an answer is needed before continuing. Do not invent a checkpoint SHA; Intercat attaches it.\n\n日本語で報告するときは、主語と述語を揃え、比喩の動詞を避け、太字と箇条書きを増やさない。\n";

// Native transport tests exercise the real worker lifecycle with synthetic
// credentials. A task-local scope keeps their offline provider out of other
// concurrent tests; it does not exist in the application build.
#[cfg(test)]
tokio::task_local! {
    pub(crate) static OFFLINE_ECHO: ();
}

pub fn hash(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

pub fn prepare_binary(source: &Path, data: &Path) -> Result<PathBuf> {
    let directory = data.join("muse-1.4.14");
    fs::create_dir_all(&directory)?;
    let target = directory.join("muse");
    if !target.exists() {
        if hash(source)? != BINARY_SHA256 {
            return Err(std::io::Error::other("source Muse binary is not verified").into());
        }
        let source_c = CString::new(source.as_os_str().as_bytes())?;
        let target_c = CString::new(target.as_os_str().as_bytes())?;
        // SAFETY: both C strings live for the call; clonefile writes only the new target.
        if unsafe { libc::clonefile(source_c.as_ptr(), target_c.as_ptr(), 0) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    if hash(&target)? != BINARY_SHA256 {
        return Err(std::io::Error::other("owned Muse binary is damaged").into());
    }
    let result = std::process::Command::new(&target)
        .arg("--version")
        .output()?;
    if !result.status.success() || String::from_utf8_lossy(&result.stdout).trim() != BINARY_VERSION
    {
        return Err(std::io::Error::other("Muse binary version is not verified").into());
    }
    Ok(target)
}

pub struct Host {
    directory: PathBuf,
    pub connection: Connection,
    pub nonce: String,
    pub identity: process::Identity,
    pub tracker: Arc<Mutex<process::Tracker>>,
    exit: tokio::task::JoinHandle<std::io::Result<std::process::ExitStatus>>,
    output_monitor: Option<crate::host_monitor::OutputMonitor>,
    process_monitor: Option<crate::process_ledger::Monitor>,
    _auth_link: Option<AuthLink>,
    _step_capture: Option<crate::step_capture::Service>,
}

struct AuthLink(PathBuf);
impl Drop for AuthLink {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub struct Boundary<'a> {
    pub profile: Profile,
    pub secrets: &'a [crate::secret_input::Value],
    pub rpc: &'a Path,
    pub denied: &'a [PathBuf],
    pub steps: Option<crate::step_capture::Service>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Worker,
    Reader,
    Summary,
}
impl Profile {
    pub fn id(self) -> &'static str {
        match self {
            Self::Worker => "intercat-worker",
            Self::Reader => "intercat-reader",
            Self::Summary => ":read-only",
        }
    }
}

impl Host {
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    /// Untrusted project mode is deliberate: the pinned host ignores hook policy in user settings.
    pub async fn spawn(
        binary: &Path,
        host_dir: &Path,
        workspace: &Path,
        path: &std::ffi::OsStr,
        credentials: Option<&Path>,
        boundary: Boundary<'_>,
    ) -> Result<Self> {
        let workspace = workspace.canonicalize()?;
        fs::create_dir_all(host_dir)?;
        let host_dir = host_dir.canonicalize()?;
        fs::set_permissions(&host_dir, fs::Permissions::from_mode(0o700))?;
        let nonce = uuid::Uuid::now_v7().to_string();
        let mut tracker = process::Tracker::prepare(&host_dir, &workspace, nonce.clone())?;
        let setup = (|| -> Result<_> {
            let config = host_dir.join("config/muse");
            fs::create_dir_all(config.join("skills/intercat-report"))?;
            fs::write(config.join("skills/intercat-report/SKILL.md"), REPORT)?;
            let auth_link = if let Some(credentials) = credentials {
                let source = credentials.canonicalize()?;
                let link = config.join("auth.json");
                std::os::unix::fs::symlink(source, &link)?;
                Some(AuthLink(link))
            } else {
                None
            };
            let secret_denies = json!(crate::secret_paths::deny_globs());
            let mut filesystem = serde_json::Map::new();
            filesystem.insert(boundary.rpc.to_string_lossy().into_owned(), json!("deny"));
            filesystem.insert(":workspace_roots".into(), secret_denies);
            if let Some(steps) = &boundary.steps {
                if boundary.profile != Profile::Worker {
                    return Err("Only worker hosts capture step files.".into());
                }
                filesystem.insert(
                    steps.directory().to_string_lossy().into_owned(),
                    json!("deny"),
                );
                filesystem.insert(host_dir.to_string_lossy().into_owned(), json!("deny"));
            }
            for denied in boundary.denied {
                let denied = crate::physical_path::resolve(denied)?;
                if workspace.starts_with(&denied) {
                    return Err("The worker workspace overlaps a denied source path.".into());
                }
                filesystem.insert(denied.to_string_lossy().into_owned(), json!("deny"));
            }
            let mut settings = json!({"schema_version":1,"provider":"meta","model":"muse-spark-1.3-contributor","contributor_mode_enabled":false,
            "agents":{"execution_capacity":1},"run":{"toolset":["read_file","write_file","edit_file","bash","bash_input","search","web_fetch","web_search","request_user_input"]},
            "permissions":{"schema_version":1,"default_profile":"intercat-worker","profiles":{"intercat-worker":{"extends":":ask-me","filesystem":filesystem}}}});
            if let Some(steps) = &boundary.steps {
                settings["hooks"] = steps.hooks();
            }
            if matches!(boundary.profile, Profile::Reader | Profile::Summary) {
                if !boundary.secrets.is_empty() {
                    return Err("A planner must not receive environment secrets.".into());
                }
                settings["model"] = json!("muse-wonder-1.0");
                if boundary.profile == Profile::Summary {
                    settings["run"]["toolset"] = json!([]);
                    settings["permissions"] =
                        json!({"schema_version":1,"default_profile":":read-only"});
                } else {
                    settings["run"]["toolset"] = json!(["read_file", "search"]);
                    let permissions =
                        settings["permissions"]["profiles"]["intercat-worker"].clone();
                    settings["permissions"] = json!({"schema_version":1,"default_profile":"intercat-reader","profiles":{"intercat-reader":permissions}});
                }
            }
            #[cfg(test)]
            if OFFLINE_ECHO.try_with(|_| ()).is_ok() {
                settings["provider"] = json!("echo");
                settings.as_object_mut().unwrap().remove("model");
            }
            let mut file = fs::File::create(config.join("settings.json"))?;
            file.write_all(serde_json::to_string_pretty(&settings)?.as_bytes())?;
            // A resumed session can print an old secret even after its environment
            // value was removed. Only MSP output has a redaction boundary.
            fs::write(
                host_dir.join("stderr.log"),
                "Internal host diagnostics are not recorded because resumed history may contain secrets. Tool output is masked in the event log.\n",
            )?;
            let mut command = Command::new(binary);
            command
                .arg("serve")
                .current_dir(&workspace)
                .env_clear()
                .env(
                    "HOME",
                    std::env::var_os("HOME")
                        .ok_or_else(|| std::io::Error::other("HOME is missing"))?,
                )
                .env("PATH", path)
                .env("LANG", "en_US.UTF-8")
                .env("XDG_CONFIG_HOME", config.parent().unwrap())
                .env("INTERCAT_HOST", &nonce)
                .env("MUSE_NO_AUTO_UPDATE", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .process_group(0)
                .kill_on_drop(true);
            if boundary.profile == Profile::Reader {
                command.args([
                    "--trust-workspace",
                    "--disable-write",
                    "--disable-shell",
                    "--sandbox-network",
                    "restricted",
                ]);
            }
            for secret in boundary.secrets {
                secret.apply(&mut command);
            }
            Ok((command, auth_link))
        })();
        let (mut command, auth_link) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                tracker.cancel_launch()?;
                return Err(error);
            }
        };
        if let Some(steps) = &boundary.steps
            && let Err(error) = steps
                .register(&host_dir.join("config/muse/settings.json"))
                .await
        {
            tracker.cancel_launch()?;
            return Err(error);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracker.cancel_launch()?;
                return Err(error.into());
            }
        };
        let identity = process::identity(child.id().expect("child just started"))?;
        tracker.attach_process_group(identity)?;
        let tracker = Arc::new(Mutex::new(tracker));
        let process_monitor = Some(crate::process_ledger::Monitor::start(
            tracker.clone(),
            identity,
        ));
        let connection = Connection::new(child.stdout.take().unwrap(), child.stdin.take().unwrap());
        let output_monitor = Some(crate::host_monitor::OutputMonitor::start(
            connection.client.output_budget(),
            tracker.clone(),
            identity,
        ));
        // Reap independently of journal progress, including when the output
        // monitor terminates a host while its execution task awaits a write.
        let exit = tokio::spawn(async move { child.wait().await });
        Ok(Self {
            directory: host_dir,
            connection,
            exit,
            nonce,
            identity,
            tracker,
            output_monitor,
            process_monitor,
            _auth_link: auth_link,
            _step_capture: boundary.steps,
        })
    }
    /// Release an SDK session only after its matching completion receipt.
    pub async fn release_session(&mut self, session: &str, stop_turn: bool) -> Result<()> {
        let leave = uuid::Uuid::now_v7().to_string();
        let accepted = self
            .connection
            .client
            .request(
                "session/leave",
                json!({"commandId":leave,"sessionId":session,"stop":stop_turn}),
            )
            .await?;
        if accepted["status"] != "accepted" {
            return Err("The session could not be released.".into());
        }
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while let Some(event) = self.connection.next_event().await {
                if let crate::msp::Event::Frame(frame) = event {
                    let p = &frame.frame["params"];
                    if frame.frame["method"] == "session/leaveCompleted"
                        && p["commandId"] == leave
                        && p["sessionId"] == session
                    {
                        return if p["outcome"] == "released" {
                            Ok(())
                        } else {
                            Err("Session release failed.".into())
                        };
                    }
                } else {
                    break;
                }
            }
            Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                "The host closed before session release.".into(),
            )
        })
        .await
        .map_err(|_| "Session release timed out.")??;
        Ok(())
    }

    /// End one transport while retaining ownership of its surviving children.
    /// The next host inherits this tracker's identities and process groups.
    pub async fn retire(mut self) -> Result<Vec<crate::msp::Received>> {
        if self.connection.client.output_budget().needs_force() {
            return self.shutdown_received().await;
        }
        if let Some(monitor) = self.output_monitor.take() {
            monitor.stop().await?;
        }
        if let Some(monitor) = self.process_monitor.take() {
            monitor.stop().await?;
        }
        self.tracker.lock().unwrap().refresh()?;
        self.connection.close_input();
        match tokio::time::timeout(std::time::Duration::from_secs(2), &mut self.exit).await {
            Ok(status) => {
                status??;
            }
            Err(_) => {
                self.tracker
                    .lock()
                    .unwrap()
                    .terminate(self.identity, true)?;
                (&mut self.exit).await??;
            }
        }
        Ok(self.connection.finish_received().await)
    }

    pub async fn shutdown(self) -> Result<Vec<serde_json::Value>> {
        Ok(self
            .shutdown_received()
            .await?
            .into_iter()
            .map(|frame| frame.frame)
            .collect())
    }
    pub(crate) async fn shutdown_received(mut self) -> Result<Vec<crate::msp::Received>> {
        let monitor_error = match self.output_monitor.take() {
            Some(monitor) => monitor.stop().await.err(),
            None => None,
        };
        let process_error = match self.process_monitor.take() {
            Some(monitor) => monitor.stop().await.err(),
            None => None,
        };
        let tracker = self.tracker.clone();
        let before = tokio::task::spawn_blocking(move || tracker.lock().unwrap().refresh()).await?;
        self.connection.close_input();
        match tokio::time::timeout(std::time::Duration::from_secs(2), &mut self.exit).await {
            Ok(status) => {
                status??;
            }
            Err(_) => {
                self.tracker
                    .lock()
                    .unwrap()
                    .terminate(self.identity, true)?;
                (&mut self.exit).await??;
            }
        }
        let mut cleanup_error = before.err().or(monitor_error).or(process_error);
        for force in [false, true] {
            let tracker = self.tracker.clone();
            let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                let mut tracker = tracker.lock().unwrap();
                for process in tracker.refresh()?.processes {
                    tracker.terminate(process.identity, force)?;
                }
                Ok(())
            })
            .await?;
            if let Err(error) = result {
                cleanup_error = Some(error);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let frames = self.connection.finish_received().await;
        if let Some(error) = cleanup_error {
            return Err(error.into());
        }
        let tracker = self.tracker.clone();
        let remaining =
            tokio::task::spawn_blocking(move || tracker.lock().unwrap().refresh()).await??;
        if !remaining.is_empty() {
            return Err(std::io::Error::other(
                "host still has recorded live processes after shutdown",
            )
            .into());
        }
        Ok(frames)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.exit.abort();
    }
}
