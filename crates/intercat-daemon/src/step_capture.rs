//! A synchronous owned hook takes snapshots before the next tool batch can run.
//! The endpoint accepts only a private capability and never trusts a supplied path.
use crate::{Result, journal::Journal, preparation::Workspace, step_sources::Source};
use intercat_git::{Repository, SavedFile};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::Path,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    task::JoinHandle,
};

const LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Deserialize)]
struct Call {
    tool_use_id: String,
}
#[derive(Deserialize)]
struct Request {
    hook_event_name: String,
    session_id: String,
    turn_id: String,
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_calls: Vec<Call>,
}
struct Before {
    sha: String,
    workspace: Workspace,
    files: BTreeMap<String, SavedFile>,
    calls: BTreeSet<String>,
}

pub struct Service {
    directory: tempfile::TempDir,
    command: String,
    task: JoinHandle<()>,
    journal: Journal,
    track: String,
}
impl Service {
    pub fn start(journal: Journal, track: String, workspace: &Path) -> Result<Self> {
        let root = workspace.canonicalize()?;
        let directory = tempfile::Builder::new()
            .prefix("ic-step-")
            .tempdir_in("/private/tmp")?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let socket = directory.path().join("capture.sock");
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let mut bytes = [0u8; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "Could not create a step capability.")?;
        let token = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let token_file = directory.path().join("capability");
        fs::write(&token_file, format!("{token}\n"))?;
        fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600))?;
        let quote = |path: &Path| -> Result<String> {
            Ok(format!(
                "'{}'",
                path.to_str()
                    .ok_or("Invalid hook path.")?
                    .replace('\'', "'\\''")
            ))
        };
        let command = format!(
            "{{ /bin/cat {}; /bin/cat; }} | /usr/bin/nc -w 25 -U {}",
            quote(&token_file)?,
            quote(&socket)?
        );
        let writer = journal.clone();
        let owner = track.clone();
        let task = tokio::spawn(async move {
            let mut pending = BTreeMap::new();
            while let Ok((stream, _)) = listener.accept().await {
                let result = serve(stream, &token, &journal, &track, &root, &mut pending).await;
                // A hook failure is returned to the host, never written with its input body.
                if result.is_err() {
                    continue;
                }
            }
        });
        Ok(Self {
            directory,
            command,
            task,
            journal: writer,
            track: owner,
        })
    }
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }
    pub async fn register(&self, file: &Path) -> Result<()> {
        self.journal
            .register_step_hook(self.track.clone(), file.canonicalize()?)
            .await
    }
    pub fn hooks(&self) -> serde_json::Value {
        let handler = json!([{"hooks":[{"type":"command","command":self.command,"timeout":30}]}]);
        json!({"PreToolUse":handler,"PostToolBatch":handler})
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn receive(stream: UnixStream, token: String) -> Result<(UnixStream, Request)> {
    if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
        return Err("Foreign hook connection.".into());
    }
    let stream = stream.into_std()?;
    let (stream, request) = tokio::task::spawn_blocking(move || -> Result<_> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut reader = std::io::BufReader::new(stream);
        let mut header = [0u8; 65];
        reader.read_exact(&mut header)?;
        if header[64] != b'\n'
            || !bool::from(subtle::ConstantTimeEq::ct_eq(
                &header[..64],
                token.as_bytes(),
            ))
        {
            return Err("Invalid step capability.".into());
        }
        let mut limited = reader.by_ref().take(LIMIT);
        let request =
            Request::deserialize(&mut serde_json::Deserializer::from_reader(&mut limited))?;
        let stream = reader.into_inner();
        stream.set_nonblocking(true)?;
        Ok((stream, request))
    })
    .await??;
    Ok((UnixStream::from_std(stream)?, request))
}

async fn files(
    workspace: &Workspace,
    journal: &Journal,
) -> Result<(String, BTreeMap<String, SavedFile>)> {
    let captured = crate::checkpoint::capture(
        workspace,
        &workspace.base,
        "Intercat step file observation\n",
        journal,
    )
    .await?;
    let root = workspace.path.clone();
    tokio::task::spawn_blocking(move || -> Result<_> {
        Ok((
            captured.sha.clone(),
            Repository::open(root)?.saved_files(captured.sha.parse()?)?,
        ))
    })
    .await?
}

async fn restore_before(
    journal: &Journal,
    track: &str,
    key: &(String, String),
) -> Result<Option<Before>> {
    let Some(active) = journal
        .active_step_capture(track.to_owned(), key.0.clone(), key.1.clone())
        .await?
    else {
        return Ok(None);
    };
    let path = active.workspace.path.clone();
    let sha = active.before.clone();
    let files =
        tokio::task::spawn_blocking(move || Repository::open(path)?.saved_files(sha.parse()?))
            .await??;
    Ok(Some(Before {
        sha: active.before,
        workspace: active.workspace,
        files,
        calls: active.calls.into_iter().collect(),
    }))
}

async fn serve(
    stream: UnixStream,
    token: &str,
    journal: &Journal,
    track: &str,
    root: &Path,
    pending: &mut BTreeMap<(String, String), Before>,
) -> Result<()> {
    let (mut stream, request) = receive(stream, token.to_owned()).await?;
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        capture(journal, track, root, pending, request),
    )
    .await;
    let response = match result {
        Ok(Ok(())) => json!({}),
        _ => {
            json!({"continue":false,"stopReason":"Intercat could not record the step's file changes."})
        }
    };
    stream
        .write_all(serde_json::to_string(&response)?.as_bytes())
        .await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    Ok(())
}

async fn capture(
    journal: &Journal,
    track: &str,
    root: &Path,
    pending: &mut BTreeMap<(String, String), Before>,
    request: Request,
) -> Result<()> {
    uuid::Uuid::parse_str(&request.session_id)?;
    uuid::Uuid::parse_str(&request.turn_id)?;
    let Some(workspace) = journal
        .step_source_owner(track.to_owned(), request.session_id.clone())
        .await?
    else {
        return Ok(());
    };
    if workspace.path.canonicalize()? != root {
        return Err("The step workspace changed.".into());
    }
    let key = (request.session_id, request.turn_id);
    match request.hook_event_name.as_str() {
        "PreToolUse" => {
            let call = request
                .tool_use_id
                .filter(|call| {
                    !call.is_empty() && call.len() <= 256 && !call.chars().any(char::is_control)
                })
                .ok_or("Missing tool identity.")?;
            if !pending.contains_key(&key) {
                // A host runs one turn at a time. An interrupted turn cannot
                // lend its before-state to the next turn or retain it forever.
                pending.clear();
                let before = if let Some(before) = restore_before(journal, track, &key).await? {
                    before
                } else {
                    let (sha, files) = files(&workspace, journal).await?;
                    Before {
                        sha,
                        files,
                        workspace,
                        calls: BTreeSet::new(),
                    }
                };
                pending.insert(key.clone(), before);
            }
            let before = pending.get_mut(&key).unwrap();
            if before.calls.len() >= 256 {
                return Err("Too many tools in one batch.".into());
            }
            before.calls.insert(call);
            if !journal
                .begin_step_capture(crate::step_sources::Active {
                    track: track.to_owned(),
                    session: key.0,
                    turn: key.1,
                    before: before.sha.clone(),
                    calls: before.calls.iter().cloned().collect(),
                    workspace: before.workspace.clone(),
                })
                .await?
            {
                return Err("The capture owner changed.".into());
            }
        }
        "PostToolBatch" => {
            let count = request.tool_calls.len();
            if count > 256 {
                return Err("Too many tools in one captured batch.".into());
            }
            let calls = request
                .tool_calls
                .into_iter()
                .map(|call| call.tool_use_id)
                .collect::<BTreeSet<_>>();
            if calls.len() != count {
                return Err("A captured batch repeated a tool identity.".into());
            }
            if calls.is_empty() {
                return Ok(());
            }
            let ordered = calls.iter().cloned().collect::<Vec<_>>();
            let id = crate::step_sources::identity(&key.0, &key.1, &ordered);
            if journal.step_source(id).await?.is_some() {
                return Ok(());
            }
            let before = match pending.remove(&key) {
                Some(before) => Some(before),
                None => restore_before(journal, track, &key).await?,
            };
            let paths = if let Some(before) = before {
                if before.calls != calls
                    || serde_json::to_value(&before.workspace)? != serde_json::to_value(&workspace)?
                {
                    return Err("The captured batch or workspace changed.".into());
                }
                match files(&workspace, journal).await {
                    Ok((_, after)) => Some(
                        before
                            .files
                            .keys()
                            .chain(after.keys())
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .filter(|path| before.files.get(*path) != after.get(*path))
                            .cloned()
                            .collect(),
                    ),
                    Err(_) => None,
                }
            } else {
                None
            };
            if !journal
                .save_step_source(Source {
                    id,
                    track: track.to_owned(),
                    session: key.0,
                    turn: key.1,
                    calls: ordered,
                    paths,
                    workspace,
                    at: crate::clock::Stamp::now(),
                })
                .await?
            {
                return Err("The step owner changed before saving.".into());
            }
        }
        _ => return Err("Unsupported step hook.".into()),
    }
    Ok(())
}
