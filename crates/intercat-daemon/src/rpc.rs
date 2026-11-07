//! Private local JSON-RPC. Credentials are accepted only over the bootstrap
//! pipe and the first frame of an owner-checked Unix connection, never logged.
use crate::{
    Result, editor,
    journal::{Admission, Journal},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs, io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Notify, Semaphore},
    task::JoinSet,
};

const MAX_FRAME: usize = 512 * 1024;

pub struct Token([u8; 32]);
impl Token {
    pub fn parse(value: &str) -> Result<Self> {
        if value.len() != 64 {
            return Err(io::Error::other("invalid bootstrap token length").into());
        }
        let mut bytes = [0; 32];
        for (i, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let nibble = |b| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                _ => None,
            };
            bytes[i] = nibble(pair[0])
                .zip(nibble(pair[1]))
                .map(|(a, b)| a * 16 + b)
                .ok_or_else(|| io::Error::other("invalid bootstrap token encoding"))?;
        }
        Ok(Self(bytes))
    }
    fn matches(&self, value: &str) -> bool {
        Self::parse(value).is_ok_and(|other| bool::from(self.0.ct_eq(&other.0)))
    }
}
impl Drop for Token {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Clone)]
struct Context {
    external: crate::external_content::Service,
    journal: Journal,
    editor: editor::Manager,
    wake: Arc<Notify>,
    input_status: tokio::sync::watch::Receiver<crate::input::Status>,
    preparation_status: tokio::sync::watch::Receiver<crate::preparation::Status>,
    runtime: crate::runtime::Service,
    integration: crate::integration::Service,
    repair: crate::repair_service::Service,
    paused: Arc<crate::dispatch_control::Gate>,
    pause_lock: Arc<tokio::sync::Mutex<()>>,
    shutdown_signal: tokio::sync::watch::Sender<bool>,
    pause_receipts: Arc<std::sync::Mutex<std::collections::HashMap<uuid::Uuid, PauseReceipt>>>,
}
#[derive(Clone, Copy)]
struct PauseReceipt {
    paused: bool,
    generation: u64,
    committed: bool,
}

pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    token: Arc<Token>,
    journal: Journal,
    editor: editor::Manager,
    wake: Arc<Notify>,
    workspace_root: PathBuf,
}
impl Server {
    /// Caller must hold DaemonLock for this directory, including stale socket removal.
    pub fn bind(
        directory: &Path,
        token: Token,
        journal: Journal,
        editor: editor::Manager,
        workspace_root: &Path,
    ) -> Result<Self> {
        let metadata = fs::symlink_metadata(directory)?;
        // SAFETY: geteuid has no pointer arguments.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o700
        {
            return Err(
                io::Error::other("RPC directory must be owned by this user and mode 0700").into(),
            );
        }
        journal.attach_privacy_editor(editor.clone())?;
        let path = directory.join("rpc.sock");
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.file_type().is_socket()
                    && metadata.uid() == unsafe { libc::geteuid() } =>
            {
                fs::remove_file(&path)?
            }
            Ok(_) => {
                return Err(io::Error::other("RPC socket path is occupied by another file").into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            path,
            token: Arc::new(token),
            journal,
            editor,
            wake: Arc::new(Notify::new()),
            workspace_root: workspace_root.to_path_buf(),
        })
    }
    pub async fn serve(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let processor = crate::input::Processor::start(
            self.journal.clone(),
            self.editor.clone(),
            self.wake.clone(),
        )
        .await?;
        let descriptions =
            crate::descriptions::Service::start(self.journal.clone(), self.editor.clone());
        let report_analysis =
            crate::report_analysis::Service::start(self.journal.clone(), self.editor.clone());
        let assumptions =
            crate::assumptions::Service::start(self.journal.clone(), self.editor.clone());
        let inferred_corrections = crate::input_inference::Service::start(self.journal.clone());
        let quiet_proposals = crate::quiet_proposals::Service::start(self.journal.clone());
        let clip_files = crate::clip_files::Service::start(self.journal.clone());
        let preparation =
            crate::preparation::Preparation::start(&self.workspace_root, self.journal.clone())?;
        let preparation_status = preparation.status.clone();
        let capacity = Arc::new(Semaphore::new(4));
        let paused = Arc::new(crate::dispatch_control::Gate::new(
            self.journal.snapshot().await?["dispatchPaused"]
                .as_bool()
                .unwrap(),
        ));
        let (shutdown_signal, mut requested_shutdown) = tokio::sync::watch::channel(false);
        let runtime = crate::runtime::Runtime::start(
            self.workspace_root
                .parent()
                .ok_or_else(|| io::Error::other("Workspace root needs a parent."))?
                .to_path_buf(),
            self.path.parent().unwrap().to_path_buf(),
            self.journal.clone(),
            self.editor.clone(),
            capacity.clone(),
            paused.clone(),
        );
        let integration = crate::integration::Service::new(
            self.journal.clone(),
            self.workspace_root.parent().unwrap().join("integration"),
        );
        let repairs = crate::repair_service::Runtime::start(
            self.journal.clone(),
            self.workspace_root.parent().unwrap().join("integration"),
            self.workspace_root.parent().unwrap().to_path_buf(),
            self.path.parent().unwrap().to_path_buf(),
            capacity,
            integration.clone(),
            runtime.service.project_secrets(),
            paused.clone(),
        );
        tokio::pin!(shutdown);
        let context = Context {
            external: crate::external_content::Service::new()?,
            journal: self.journal.clone(),
            editor: self.editor.clone(),
            wake: self.wake.clone(),
            input_status: processor.status.clone(),
            preparation_status,
            runtime: runtime.service.clone(),
            repair: repairs.service.clone(),
            integration,
            paused,
            pause_lock: Arc::new(tokio::sync::Mutex::new(())),
            shutdown_signal,
            pause_receipts: Arc::new(std::sync::Mutex::new(
                self.journal
                    .pause_history()
                    .await?
                    .into_iter()
                    .map(|(id, paused)| {
                        (
                            id,
                            PauseReceipt {
                                paused,
                                generation: 0,
                                committed: true,
                            },
                        )
                    })
                    .collect(),
            )),
        };
        context.integration.recover().await?;
        let slots = Arc::new(Semaphore::new(32));
        let mut clients = JoinSet::new();
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = requested_shutdown.changed() => break,
                _ = clients.join_next(), if !clients.is_empty() => {},
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted?;
                    if !same_owner(&stream)? { continue; }
                    let Ok(slot) = slots.clone().try_acquire_owned() else { continue; };
                    let token = self.token.clone(); let context = context.clone();
                    clients.spawn(async move { let _slot = slot; let _ = connection(stream, token, context).await; });
                }
            }
        }
        clients.shutdown().await;
        tokio::join!(
            context.integration.shutdown(),
            runtime.shutdown(),
            repairs.shutdown(),
            quiet_proposals.stop(),
            clip_files.stop()
        );
        processor.shutdown().await?;
        report_analysis.shutdown().await;
        descriptions.shutdown().await;
        assumptions.shutdown().await;
        inferred_corrections.shutdown().await;
        preparation.shutdown().await;
        Ok(())
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn same_owner(stream: &UnixStream) -> io::Result<bool> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: both output pointers refer to initialized storage for this call.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid == unsafe { libc::geteuid() })
}

async fn read_frame(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Result<Request> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "RPC stream closed").into());
        }
        let count = available
            .iter()
            .position(|&b| b == b'\n')
            .map_or(available.len(), |n| n + 1);
        if bytes.len() + count > MAX_FRAME {
            return Err(io::Error::other("RPC frame exceeds limit").into());
        }
        let finished = available[count - 1] == b'\n';
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if finished {
            break;
        }
    }
    let request: Request = serde_json::from_slice(&bytes)?;
    if request.jsonrpc != "2.0" || !(request.id.is_u64() || request.id.is_string()) {
        return Err(io::Error::other("invalid RPC envelope").into());
    }
    Ok(request)
}

async fn change_pause(context: &Context, id: uuid::Uuid, pause: bool) -> Result<()> {
    let generation = {
        let mut receipts = context.pause_receipts.lock().unwrap();
        let generation = if let Some(receipt) = receipts.get(&id) {
            if receipt.paused != pause {
                return Err("Operation ID reused.".into());
            }
            if receipt.committed {
                return Ok(());
            }
            if receipt.generation != context.paused.epoch() {
                return Err("A newer control superseded this request.".into());
            }
            receipt.generation
        } else {
            let generation = context.paused.begin(pause);
            receipts.insert(
                id,
                PauseReceipt {
                    paused: pause,
                    generation,
                    committed: false,
                },
            );
            generation
        };
        if pause {
            context.runtime.interrupt_all(id)?;
        }
        generation
    };
    let _guard = context.pause_lock.lock().await;
    if generation != context.paused.epoch() {
        return Err("A newer control keeps this request from applying.".into());
    }
    if pause {
        let (saved, (_, repairs)) = tokio::join!(context.journal.pause_dispatch(id, true), async {
            tokio::join!(context.integration.shutdown(), context.repair.stop_all(id))
        });
        saved?;
        repairs?;
    } else {
        context.runtime.ready_to_resume().await?;
        context.journal.pause_dispatch(id, false).await?;
    }
    // Serialize the final in-memory release with begin/interrupt_all above.
    // Otherwise an old Resume could drain the ports captured by a newer Stop.
    let mut receipts = context.pause_receipts.lock().unwrap();
    receipts.get_mut(&id).unwrap().committed = true;
    if !pause {
        if generation != context.paused.epoch() {
            return Err("A newer Stop keeps work paused.".into());
        }
        context.runtime.resume_all();
        if !context.paused.resume(generation) {
            return Err("A newer Stop keeps work paused.".into());
        }
    }
    Ok(())
}

async fn connection(stream: UnixStream, token: Arc<Token>, context: Context) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let hello = tokio::time::timeout(Duration::from_secs(3), read_frame(&mut reader)).await??;
    if hello.method != "rpc.authenticate"
        || !hello
            .params
            .get("token")
            .and_then(Value::as_str)
            .is_some_and(|value| token.matches(value))
    {
        write.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":hello.id,"error":{"code":-32001,"message":"authentication required"}})).as_bytes()).await?;
        return Ok(());
    }
    write
        .write_all(
            format!(
                "{}\n",
                json!({"jsonrpc":"2.0","id":hello.id,"result":{"protocol":1}})
            )
            .as_bytes(),
        )
        .await?;
    let write = Arc::new(tokio::sync::Mutex::new(write));
    let permits = Arc::new(Semaphore::new(32));
    let mut requests = JoinSet::new();
    loop {
        tokio::select! {
            result=requests.join_next(),if !requests.is_empty()=>{if let Some(result)=result { result??; }},
            request=read_frame(&mut reader)=>{
                let request=request?;
                let slot=if matches!(request.method.as_str(), "worker.stop" | "utterance.stop" | "repair.stop" | "system.pause" | "system.shutdown" | "system.outputFlows") || (matches!(request.method.as_str(), "worker.respond" | "repair.respond") && request.params["response"]["action"]=="stop") {None}else{Some(permits.clone().try_acquire_owned().map_err(|_|io::Error::other("Too many concurrent requests."))?)};
                let context=context.clone();let write=write.clone();
                requests.spawn(async move {
                    let _slot=slot;
                    let response=match dispatch(&context,&request).await {
                        Ok(result)=>json!({"jsonrpc":"2.0","id":request.id,"result":result}),
                        Err((code,message))=>json!({"jsonrpc":"2.0","id":request.id,"error":{"code":code,"message":message}}),
                    };
                    let mut write=write.lock().await;
                    tokio::time::timeout(Duration::from_secs(10),write.write_all(format!("{response}\n").as_bytes())).await??;
                    if request.method=="system.shutdown" && response.get("result").is_some() {
                        let _=context.shutdown_signal.send(true);
                    }
                    Ok::<(),Box<dyn std::error::Error+Send+Sync>>(())
                });
            }
        }
    }
}

async fn dispatch(
    context: &Context,
    request: &Request,
) -> std::result::Result<Value, (i32, String)> {
    let Context {
        external,
        journal,
        editor,
        wake,
        input_status,
        preparation_status,
        runtime,
        integration,
        repair,
        paused,
        ..
    } = context;
    let invalid = || (-32602, "invalid parameters".to_owned());
    if paused.paused()
        && (matches!(
            request.method.as_str(),
            "worker.start"
                | "worker.answer"
                | "utterance.start"
                | "repair.start"
                | "repair.followup"
                | "repair.answer"
                | "external.confirm"
                | "project.configure"
        ) || request.method == "utterance.resolve"
            && request.params["resolution"]["action"] == "retry"
            || request.method == "repair.control" && request.params["action"] == "resume"
            || request.method == "integration.command"
                && !matches!(request.params["action"]["action"].as_str(), Some("discard")))
    {
        return Err((
            -32005,
            "Turns are stopped. Resume before starting new work.".into(),
        ));
    }
    match request.method.as_str() {
        "check-environment.get" => {
            let selection = journal
                .check_environment()
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            let snapshot = journal
                .snapshot()
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            let project =
                serde_json::from_value::<crate::project::Project>(snapshot["project"].clone()).ok();
            let available = project.is_some();
            let candidates = tokio::task::spawn_blocking(move || {
                project
                    .map(|project| {
                        [".env.test", ".env.ci"]
                            .into_iter()
                            .filter_map(|name| {
                                crate::check_environment::validate_file(
                                    &project.root.join(name),
                                    &project,
                                )
                                .ok()
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .await
            .map_err(|_| (-32603, "Check environment unavailable".into()))?;
            Ok(json!({"selection":selection,"candidates":candidates,"available":available}))
        }
        "check-environment.change" => {
            let params = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            let selection = journal
                .change_check_environment(params)
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            Ok(serde_json::to_value(selection).unwrap())
        }
        #[cfg(all(feature = "fault-injection", debug_assertions))]
        "test.writer" => {
            let request = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .test_fault(request)
                .await
                .map_err(|error| (-32005, error.to_string()))
        }
        "system.pause" | "system.shutdown" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                paused: bool,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7
                || request.method == "system.shutdown" && !params.paused
            {
                return Err(invalid());
            }
            change_pause(context, params.op_id, params.paused)
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            Ok(json!({"paused":paused.paused()}))
        }
        "external.review" => {
            let params = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            external
                .review(params)
                .await
                .and_then(|review| Ok(serde_json::to_value(review)?))
                .map_err(|error| (-32004, error.to_string()))
        }
        "external.image" | "external.link" | "external.cancel" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: uuid::Uuid,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.id.get_version_num() != 7 {
                return Err(invalid());
            }
            match request.method.as_str() {
                "external.image" => external
                    .image(params.id)
                    .await
                    .and_then(|image| Ok(serde_json::to_value(image)?)),
                "external.link" => external.link(params.id).await.map(|url| json!({"url":url})),
                _ => {
                    external.cancel(params.id).await;
                    Ok(Value::Null)
                }
            }
            .map_err(|error| (-32004, error.to_string()))
        }
        "repair.start" => {
            let params: crate::repair_run::Request =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 || uuid::Uuid::parse_str(&params.track).is_err()
            {
                return Err(invalid());
            }
            match repair.start(params).await {
                Ok(run) => Ok(json!({"status":"accepted","run":run})),
                Err(error) if error.is::<crate::repair_run::Rejected>() => {
                    Ok(json!({"status":"rejected","reason":error.to_string()}))
                }
                Err(error) => Err((-32005, error.to_string())),
            }
        }
        "repair.refresh" => {
            let params: crate::repair_run::RefreshRequest =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            match repair.refresh(params).await {
                Ok(run) => Ok(json!({"status":"accepted","run":run})),
                Err(error) if error.is::<crate::repair_run::Rejected>() => {
                    Ok(json!({"status":"rejected","reason":error.to_string()}))
                }
                Err(error) => Err((-32005, error.to_string())),
            }
        }
        "repair.followup" => {
            let params: crate::repair_run::FollowUp =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            match repair.followup(params).await {
                Ok(run) => Ok(json!({"status":"accepted","run":run})),
                Err(error) if error.is::<crate::repair_run::Rejected>() => {
                    Ok(json!({"status":"rejected","reason":error.to_string()}))
                }
                Err(error) => Err((-32005, error.to_string())),
            }
        }
        "repair.control" => {
            let params: crate::repair_run::Command =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            repair
                .control(params)
                .await
                .and_then(|run| Ok(serde_json::to_value(run)?))
                .map_err(|e| (-32005, e.to_string()))
        }
        "repair.stop" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                id: uuid::Uuid,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            repair
                .stop(params.op_id, params.id)
                .await
                .map_err(|e| (-32005, e.to_string()))?;
            Ok(Value::Null)
        }
        "repair.answer" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                id: uuid::Uuid,
                approval_id: String,
                requirement: Value,
                response: crate::approval::Response,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 || params.approval_id.len() > 256 {
                return Err(invalid());
            }
            repair
                .answer(
                    params.op_id,
                    params.id,
                    params.approval_id,
                    params.requirement,
                    params.response,
                )
                .await
                .map_err(|e| (-32005, e.to_string()))?;
            Ok(Value::Null)
        }
        "repair.respond" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                id: uuid::Uuid,
                prompt_id: String,
                revision: String,
                response: crate::questions::Response,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7
                || params.prompt_id.len() > 128
                || params.revision.len() != 64
            {
                return Err(invalid());
            }
            repair
                .respond(
                    params.op_id,
                    params.id,
                    params.prompt_id,
                    params.revision,
                    params.response,
                )
                .await
                .map_err(|e| (-32005, e.to_string()))?;
            Ok(Value::Null)
        }
        "integration.diff" => {
            let params: crate::integration::DiffRequest =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            integration
                .diff(params)
                .await
                .and_then(|value| Ok(serde_json::to_value(value)?))
                .map_err(|error| (-32000, error.to_string()))
        }
        "integration.command" => {
            let params: crate::integration::Command =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            match &params.action {
                crate::integration::Action::Open {
                    tracks: Some(tracks),
                }
                | crate::integration::Action::Order { tracks, .. }
                    if tracks.len() > 256
                        || tracks.iter().any(|t| uuid::Uuid::parse_str(t).is_err()) =>
                {
                    return Err(invalid());
                }
                crate::integration::Action::Select { track, .. }
                | crate::integration::Action::EditMessage { track, .. }
                    if uuid::Uuid::parse_str(track).is_err() =>
                {
                    return Err(invalid());
                }
                _ => {}
            }
            integration
                .command(params)
                .await
                .and_then(|job| crate::integration::project(&job))
                .map_err(|e| (-32000, e.to_string()))
        }
        "deletion.restore" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                track_id: String,
                snapshot_id: uuid::Uuid,
                repair_run_id: Option<uuid::Uuid>,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 || params.track_id.len() > 100 {
                return Err(invalid());
            }
            match params.repair_run_id {
                Some(id) => {
                    repair
                        .restore(params.op_id, id, params.track_id, params.snapshot_id)
                        .await
                }
                None => {
                    runtime
                        .restore(params.op_id, &params.track_id, params.snapshot_id)
                        .await
                }
            }
            .map(|result| serde_json::to_value(result).unwrap())
            .map_err(|e| (-32005, e.to_string()))
        }
        "confirmation.change" => {
            let params: crate::confirmation::Change =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .change_confirmation(params)
                .await
                .and_then(|status| serde_json::to_value(status).map_err(Into::into))
                .map_err(|error| (-32005, error.to_string()))
        }
        "utterance.confirm" => {
            let params: crate::confirmation::Accept =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            let before = journal
                .read_utterance(params.utterance_id.to_string())
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            let track = before["decision"]["trackId"].as_str().map(str::to_owned);
            let generation = track
                .as_ref()
                .and_then(|track| runtime.start_generation(track).ok());
            let result = journal
                .confirm_input(params.clone())
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            if result["decision"]["kind"] == "trim" && result["state"] == "redirecting" {
                let resumed = match (track, generation) {
                    (Some(track), Some(generation)) => {
                        runtime
                            .start_correction(
                                uuid::Uuid::new_v5(&params.op_id, b"confirmation.resume"),
                                &track,
                                generation,
                            )
                            .await
                    }
                    _ => Err("The correction's worker is not connected.".into()),
                };
                if let Err(error) = resumed {
                    let _ = journal
                        .resolve_input(
                            uuid::Uuid::new_v5(&params.op_id, b"confirmation.hold"),
                            params.utterance_id.to_string(),
                            result["revision"].as_i64().unwrap(),
                            crate::decision::Resolution::Hold,
                        )
                        .await;
                    return Err((-32005, error.to_string()));
                }
            }
            wake.notify_one();
            Ok(result)
        }
        "policy.stops" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                stops: crate::policy::Stops,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            journal
                .set_stops(params.op_id, params.stops)
                .await
                .map(|stops| serde_json::json!({"stops":stops}))
                .map_err(|e| (-32005, e.to_string()))
        }
        "utterance.origin" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                utterance_id: uuid::Uuid,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            let origin = journal
                .read_input_origin(params.utterance_id)
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            serde_json::to_value(origin).map_err(|error| (-32005, error.to_string()))
        }
        "utterance.changes" => {
            let identity = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            let changes = crate::input_origin::changes(journal, identity)
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            serde_json::to_value(changes).map_err(|error| (-32005, error.to_string()))
        }
        "utterance.diff" => {
            let params = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            let diff = crate::input_origin::diff(journal, params)
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            serde_json::to_value(diff).map_err(|error| (-32005, error.to_string()))
        }
        "utterance.stop" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                track_id: String,
                identity: crate::input_origin::Identity,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            runtime
                .stop_input_origin(params.op_id, params.track_id, params.identity)
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            Ok(Value::Null)
        }
        "utterance.misread" => {
            let params = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .mark_misread(params)
                .await
                .map_err(|e| (-32005, e.to_string()))
        }
        "utterance.read" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                utterance_id: uuid::Uuid,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .read_utterance(params.utterance_id.to_string())
                .await
                .map_err(|e| (-32005, e.to_string()))
        }
        "timeline.detail" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                clip_id: uuid::Uuid,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .clip_detail(params.clip_id.to_string())
                .await
                .map_err(|e| (-32005, e.to_string()))
        }
        "timeline.observe" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                owner: uuid::Uuid,
                revision: i64,
                view: Option<crate::clip_summaries::View>,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.owner.get_version_num() != 7
                || params.revision < 1
                || params.view.as_ref().is_some_and(|view| {
                    uuid::Uuid::parse_str(&view.track_id).is_err()
                        || view
                            .repair_run_id
                            .as_ref()
                            .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
                })
            {
                return Err(invalid());
            }
            journal
                .observe_timeline(params.owner, params.revision, params.view)
                .await
                .map(|()| Value::Null)
                .map_err(|error| (-32005, error.to_string()))
        }
        "editor.log" => {
            let query: crate::editor_log::Query =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .editor_log(query)
                .await
                .map_err(|error| (-32005, error.to_string()))
        }
        "editor.away" => {
            let query: crate::editor_away::Query =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .editor_away(query)
                .await
                .map_err(|error| (-32005, error.to_string()))
        }
        "timeline.window" => {
            let params: crate::timeline::Window =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .timeline_window(params)
                .await
                .map_err(|e| (-32005, e.to_string()))
        }
        "proposal.respond" => {
            let command: crate::proposals::Command =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if command.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            let result = journal
                .answer_proposal(command)
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            wake.notify_one();
            Ok(result)
        }
        "assumption.respond" => {
            let command: crate::assumptions::Command =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if command.op_id.get_version_num() != 7 || command.revision < 1 {
                return Err(invalid());
            }
            journal
                .answer_assumption(command)
                .await
                .map_err(|error| (-32005, error.to_string()))
        }
        "report.respond" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                question_id: String,
                revision: i64,
                answer: crate::reports::Answer,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7
                || params.question_id.len() > 64
                || params.revision < 1
            {
                return Err(invalid());
            }
            journal
                .respond_report(
                    params.op_id,
                    params.question_id,
                    params.revision,
                    params.answer,
                )
                .await
                .map_err(|e| (-32005, e.to_string()))
        }
        "worker.respond" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                track_id: String,
                prompt_id: String,
                revision: String,
                response: crate::questions::Response,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7
                || params.prompt_id.len() > 128
                || params.revision.len() != 64
            {
                return Err(invalid());
            }
            runtime
                .respond(
                    params.op_id,
                    &params.track_id,
                    params.prompt_id,
                    params.revision,
                    params.response,
                )
                .await
                .map_err(|e| (-32005, e.to_string()))?;
            Ok(Value::Null)
        }
        "worker.stop" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                track_id: String,
                turn_id: Value,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            let turn = if params.turn_id.is_null() {
                None
            } else {
                Some(
                    params
                        .turn_id
                        .as_str()
                        .ok_or_else(invalid)?
                        .parse::<uuid::Uuid>()
                        .map_err(|_| invalid())?,
                )
            };
            runtime
                .stop_captured(params.op_id, &params.track_id, turn)
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            Ok(Value::Null)
        }
        "worker.start" | "worker.hold" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                track_id: String,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            if request.method == "worker.hold" {
                runtime
                    .hold(params.op_id, &params.track_id)
                    .await
                    .map_err(|error| (-32005, error.to_string()))?;
            } else {
                runtime
                    .start_turn(params.op_id, &params.track_id)
                    .await
                    .map_err(|error| (-32005, error.to_string()))?;
            }
            Ok(Value::Null)
        }
        "worker.answer" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                track_id: String,
                approval_id: String,
                requirement: Value,
                response: crate::approval::Response,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            runtime
                .answer(
                    params.op_id,
                    &params.track_id,
                    params.approval_id,
                    params.requirement,
                    params.response,
                )
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            Ok(Value::Null)
        }
        "worker.retry" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                track_id: String,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            runtime
                .retry(params.op_id, params.track_id)
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            Ok(Value::Null)
        }
        "worker.configure" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                source: PathBuf,
                credentials: PathBuf,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7
                || !params.source.is_absolute()
                || !params.credentials.is_absolute()
            {
                return Err(invalid());
            }
            runtime
                .configure(params.op_id, params.source, params.credentials)
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            Ok(json!({"configured":true}))
        }
        "preparation.retry" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            journal
                .retry_preparation(params.op_id)
                .await
                .map_err(|_| (-32002, "Retry could not be saved.".into()))?;
            Ok(Value::Null)
        }
        "project.configure" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                path: PathBuf,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 || !params.path.is_absolute() {
                return Err(invalid());
            }
            if let Some(previous) = journal
                .operation(params.op_id)
                .await
                .map_err(|_| (-32002, "Operation could not be read.".into()))?
            {
                if previous["kind"] != "project.configure"
                    || previous["payload"]["path"] != serde_json::to_value(&params.path).unwrap()
                {
                    return Err(invalid());
                }
                let project =
                    serde_json::from_value(previous["payload"].clone()).map_err(|_| invalid())?;
                return journal
                    .configure_project(params.op_id, project)
                    .await
                    .map_err(|error| (-32002, error.to_string()));
            }
            let project = crate::project::inspect(params.path)
                .await
                .map_err(|error| (-32004, error.to_string()))?;
            journal
                .configure_project(params.op_id, project)
                .await
                .map_err(|error| (-32002, error.to_string()))
        }
        "secret.project.usage" => runtime
            .project_usage()
            .await
            .and_then(|v| Ok(serde_json::to_value(v)?))
            .map_err(|error| (-32002, error.to_string())),
        "secret.project.process" => {
            let decision = serde_json::from_value(request.params.clone())
                .map_err(|_| (-32602, "Invalid project process decision.".into()))?;
            runtime
                .project_process(decision)
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            Ok(json!({"accepted":true}))
        }
        "secret.project.apply" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Apply {
                revision: u64,
            }
            let apply: Apply = serde_json::from_value(request.params.clone())
                .map_err(|_| (-32602, "Invalid project revision.".into()))?;
            runtime
                .apply_project_secrets(apply.revision)
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            repair
                .apply_project_secrets(apply.revision)
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            Ok(json!({"accepted":true}))
        }
        "secret.project.check" => {
            let request = serde_json::from_value(request.params.clone())
                .map_err(|_| (-32602, "Invalid project secret manifest.".into()))?;
            journal
                .check_project_secrets(request)
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            Ok(json!({"valid":true}))
        }
        "secret.project.hold" => {
            journal
                .hold_project_secrets()
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            Ok(json!({"held":true}))
        }
        "secret.project.prepare" => {
            let request = serde_json::from_value(request.params.clone())
                .map_err(|_| (-32602, "Invalid project secret manifest.".into()))?;
            let catalog = journal
                .prepare_project_secrets(request)
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            let snapshot = journal
                .snapshot()
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            for worker in snapshot["workers"].as_array().into_iter().flatten() {
                if let (Some(track), Some(session)) =
                    (worker["trackId"].as_str(), worker["sessionId"].as_str())
                {
                    runtime
                        .reload_fingerprints(track, session)
                        .await
                        .map_err(|error| (-32002, error.to_string()))?;
                }
            }
            for run in snapshot["repairRuns"].as_array().into_iter().flatten() {
                if let (Some(track), Some(session)) =
                    (run["track"].as_str(), run["session"].as_str())
                {
                    repair
                        .reload_fingerprints(track, session)
                        .await
                        .map_err(|error| (-32002, error.to_string()))?;
                }
            }
            Ok(serde_json::to_value(catalog).unwrap())
        }
        "secret.project.activate" => {
            let request = serde_json::from_value(request.params.clone())
                .map_err(|_| (-32602, "Invalid project secret values.".into()))?;
            runtime
                .activate_project_secrets(request)
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            Ok(json!({"accepted":true}))
        }
        "worker.secret" => {
            let request: crate::secret_input::Request =
                serde_json::from_value(request.params.clone())
                    .map_err(|_| (-32602, "Invalid worker secret request.".into()))?;
            let result = match request.owner {
                crate::secret_input::Owner::Work => runtime.install_secret(request).await,
                crate::secret_input::Owner::Repair { .. } => repair.install_secret(request).await,
            };
            result.map_err(|error| (-32002, error.to_string()))?;
            Ok(json!({"accepted":true}))
        }
        "worker.pickup.prepare" => {
            let recovery = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            runtime
                .prepare_pickup(recovery)
                .await
                .map_err(|e| (-32002, e.to_string()))
        }
        "worker.pickup.cancel" => {
            let cancel = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            runtime
                .cancel_pickup(cancel)
                .await
                .map_err(|e| (-32002, e.to_string()))
        }
        "worker.pickup.activate" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Activate {
                op_id: uuid::Uuid,
            }
            let params: Activate =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            runtime
                .activate_pickup(params.op_id)
                .await
                .map(|r| r.status())
                .map_err(|e| (-32002, e.to_string()))
        }
        "branch.points" => {
            let query = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .branch_points(query)
                .await
                .map(|page| serde_json::to_value(page).unwrap())
                .map_err(|e| (-32002, e.to_string()))
        }
        "branch.prepare" => {
            let branch = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            runtime
                .prepare_branch(branch)
                .await
                .map_err(|e| (-32002, e.to_string()))
        }
        "branch.activate" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Activate {
                op_id: uuid::Uuid,
            }
            let branch: Activate =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            runtime
                .activate_branch(branch.op_id)
                .await
                .map(|v| serde_json::to_value(v).unwrap())
                .map_err(|e| (-32002, e.to_string()))
        }
        "secret.catalog" => journal
            .fingerprint_catalog()
            .await
            .map(|v| serde_json::to_value(v).unwrap())
            .map_err(|e| (-32002, e.to_string())),
        "secret.confirm" => {
            let seal = serde_json::from_value(request.params.clone())
                .map_err(|_| (-32602, "Invalid fingerprint confirmation.".into()))?;
            journal
                .confirm_fingerprints(seal)
                .await
                .map_err(|e| (-32002, e.to_string()))?;
            Ok(json!({"confirmed":true}))
        }
        "secret.registerFingerprint" => {
            let registration: crate::secrets::Registration =
                serde_json::from_value(request.params.clone())
                    .map_err(|_| (-32602, "Invalid secret fingerprint registration.".into()))?;
            let track = registration.track_id.clone();
            let session = registration.session_id.clone();
            let catalog = journal
                .register_fingerprint(registration)
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            runtime
                .reload_fingerprints(&track, &session)
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            repair
                .reload_fingerprints(&track, &session)
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            Ok(serde_json::to_value(catalog).unwrap())
        }
        "system.outputFlows" => {
            let mut flows = runtime.output_flows();
            flows.extend(repair.output_flows());
            Ok(serde_json::to_value(flows).unwrap())
        }
        "system.status" => {
            let mut snapshot = journal
                .snapshot()
                .await
                .map_err(|_| (-32603, "state unavailable".to_owned()))?;
            snapshot["dispatchPaused"] = json!(paused.paused());
            if paused.paused() {
                match runtime.process_audit().await {
                    Ok(audit) => {
                        snapshot["runningProcesses"] = json!(audit.processes);
                        snapshot["processIssues"] = json!(audit.issues);
                    }
                    Err(error) => {
                        snapshot["processIssues"] = json!([error.to_string()]);
                    }
                }
            }
            snapshot["editor"] = serde_json::to_value(editor.status()).unwrap();
            let mut flows = runtime.output_flows();
            flows.extend(repair.output_flows());
            snapshot["outputFlows"] = serde_json::to_value(flows).unwrap();
            snapshot["input"] = serde_json::to_value(input_status.borrow().clone()).unwrap();
            snapshot["preparation"] =
                serde_json::to_value(preparation_status.borrow().clone()).unwrap();
            Ok(snapshot)
        }
        "editor.retry" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                op_id: uuid::Uuid,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            let config = journal
                .editor_config()
                .await
                .map_err(|_| (-32603, "Editor settings unavailable".into()))?
                .ok_or_else(|| (-32003, "Configure the Editor first".into()))?;
            let accepted = journal
                .set_editor_config(params.op_id, config.clone())
                .await
                .map_err(|_| (-32002, "Editor restart could not be saved".into()))?;
            if accepted == Admission::New {
                editor.restore(config).await;
            }
            Ok(serde_json::to_value(editor.status()).unwrap())
        }
        "editor.download" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                op_id: uuid::Uuid,
                python: Option<PathBuf>,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            let python = match params.python {
                Some(path) => path,
                None => {
                    serde_json::from_value::<editor::Config>(
                        journal
                            .editor_config()
                            .await
                            .map_err(|_| (-32603, "Editor settings unavailable".into()))?
                            .ok_or_else(|| {
                                (-32003, "Choose the Editor Python environment first".into())
                            })?,
                    )
                    .map_err(|_| invalid())?
                    .python
                }
            };
            let manager = editor.clone();
            let config = tokio::task::spawn_blocking(move || manager.download_config(python))
                .await
                .map_err(|_| invalid())?
                .map_err(|_| {
                    (
                        -32003,
                        "Editor storage or Python environment is unavailable".into(),
                    )
                })?;
            let accepted = journal
                .set_editor_config(params.op_id, serde_json::to_value(&config).unwrap())
                .await
                .map_err(|_| (-32002, "Editor download could not be saved".into()))?;
            if accepted == Admission::New {
                editor
                    .download(config)
                    .await
                    .map_err(|_| (-32003, "Editor download could not start".into()))?;
            }
            Ok(serde_json::to_value(editor.status()).unwrap())
        }
        "editor.pauseDownload" => {
            editor
                .pause_download()
                .await
                .map_err(|_| (-32003, "No Editor download is running".into()))?;
            Ok(serde_json::to_value(editor.status()).unwrap())
        }
        "editor.calibration" => journal
            .editor_calibration_state()
            .await
            .map(|state| serde_json::to_value(state).unwrap())
            .map_err(|_| (-32603, "Calibration unavailable".into())),
        "editor.calibrate" => {
            let change = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .apply_editor_calibration(change)
                .await
                .map(|state| serde_json::to_value(state).unwrap())
                .map_err(|_| {
                    (
                        -32002,
                        "Calibration is invalid or changed. Reload before applying.".into(),
                    )
                })
        }
        "editor.configure" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                #[serde(rename = "opId")]
                op_id: uuid::Uuid,
                config: editor::Config,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            let config = tokio::task::spawn_blocking(move || params.config.validated())
                .await
                .map_err(|_| invalid())?
                .map_err(|_| invalid())?;
            let accepted = journal
                .set_editor_config(params.op_id, serde_json::to_value(&config).unwrap())
                .await
                .map_err(|_| (-32002, "configuration could not be saved".into()))?;
            if accepted == Admission::New {
                editor
                    .configure(config)
                    .await
                    .map_err(|_| (-32003, "editor could not start".into()))?;
            }
            Ok(serde_json::to_value(editor.status()).unwrap())
        }
        "privacy.change" => {
            let params: crate::privacy_policy::Change =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .change_privacy(params)
                .await
                .and_then(|state| serde_json::to_value(state).map_err(Into::into))
                .map_err(|error| (-32005, error.to_string()))
        }
        "privacy.preview" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: String,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .preview_privacy_file(params.id)
                .await
                .map_err(|error| (-32002, error.to_string()))
        }
        "privacy.original" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: i64,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.id <= 0 || params.id > 9_007_199_254_740_991 {
                return Err(invalid());
            }
            journal
                .privacy_original(params.id)
                .await
                .map(|text| json!({"textBase64":base64::Engine::encode(&base64::prelude::BASE64_STANDARD,text)}))
                .map_err(|error| (-32002, error.to_string()))
        }
        "attachment.import" | "attachment.paste" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                op_id: uuid::Uuid,
                path: PathBuf,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 || !params.path.is_absolute() {
                return Err(invalid());
            }
            let saved = if request.method == "attachment.paste" {
                journal.import_pasted_text(params.op_id, params.path).await
            } else {
                journal.import_attachment(params.op_id, params.path).await
            };
            saved
                .and_then(|value| serde_json::to_value(value).map_err(Into::into))
                .map_err(|_| (-32002, "reference file could not be saved".into()))
        }
        "attachment.paste.refresh" => {
            let params = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .refresh_pasted_text(params)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(Into::into))
                .map_err(|_| (-32002, "pasted reference could not be screened".into()))
        }
        "attachment.paste.list" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Params {
                track_id: String,
                after: Option<uuid::Uuid>,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.track_id.is_empty() || params.track_id.len() > 128 {
                return Err(invalid());
            }
            journal
                .pasted_references(params.track_id, params.after)
                .await
                .and_then(|value| serde_json::to_value(value).map_err(Into::into))
                .map_err(|_| (-32002, "pasted references could not be loaded".into()))
        }
        "attachment.paste.read" => {
            use base64::Engine as _;
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                id: uuid::Uuid,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            let original = journal
                .read_pasted_text(params.id)
                .await
                .map_err(|_| (-32002, "pasted text could not be read".into()))?;
            Ok(
                serde_json::json!({"textBase64":base64::engine::general_purpose::STANDARD.encode(original.as_bytes())}),
            )
        }
        "draft.read" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                session: String,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .read_drafts(params.session)
                .await
                .map_err(|error| (-32002, error.to_string()))
        }
        "draft.save" => {
            let params = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            journal
                .save_drafts(params)
                .await
                .map_err(|error| (-32002, error.to_string()))
        }
        "utterance.edit" => {
            let params: crate::edit_hold::Request =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            let result = journal
                .edit_input(params)
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            wake.notify_one();
            Ok(result)
        }
        "utterance.create" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                #[serde(rename = "opId")]
                op_id: uuid::Uuid,
                text: String,
                target: crate::decision::Target,
                #[serde(rename = "selectedTrackId")]
                selected: Option<String>,
                #[serde(rename = "attachmentIds")]
                attachment_ids: Vec<uuid::Uuid>,
                sources: Option<Vec<crate::request_sources::Source>>,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7
                || (params.text.trim().is_empty()
                    && params.sources.as_ref().is_none_or(Vec::is_empty))
                || params.text.len() > 32 * 1024
            {
                return Err(invalid());
            }
            let result = journal
                .stage_document(crate::request_sources::Submission {
                    op_id: params.op_id,
                    text: params.text,
                    target: params.target,
                    selected_track_id: params.selected,
                    attachment_ids: params.attachment_ids,
                    sources: params.sources,
                })
                .await
                .map_err(|_| (-32002, "operation could not be accepted".to_owned()))?;
            wake.notify_one();
            Ok(result)
        }
        "timeline.history" => {
            let request = serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            let result = journal
                .edit_history(request)
                .await
                .map_err(|error| (-32005, error.to_string()))?;
            wake.notify_one();
            Ok(result)
        }
        "utterance.start" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                utterance_id: String,
                revision: i64,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 || params.revision < 1 {
                return Err(invalid());
            }
            runtime
                .start_held_input(params.op_id, params.utterance_id, params.revision)
                .await
                .map_err(|error| (-32005, error.to_string()))
        }
        "utterance.rephrase" => {
            let params: crate::rephrases::Submission =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            let result = journal
                .submit_rephrase(params)
                .await
                .map_err(|error| (-32002, error.to_string()))?;
            wake.notify_one();
            Ok(result)
        }
        "utterance.resolve" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Params {
                op_id: uuid::Uuid,
                utterance_id: String,
                revision: i64,
                resolution: crate::decision::Resolution,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 || params.revision < 1 {
                return Err(invalid());
            }
            let start_guard = match &params.resolution {
                crate::decision::Resolution::Retry {
                    target: crate::decision::Target::Lock(track),
                } => Some((track.clone(), runtime.start_generation(track))),
                _ => None,
            };
            let result = journal
                .resolve_input(
                    params.op_id,
                    params.utterance_id,
                    params.revision,
                    params.resolution,
                )
                .await
                .map_err(|_| {
                    (
                        -32002,
                        "request changed or could not be saved; refresh before answering".into(),
                    )
                })?;
            if result["decision"]["kind"] == "trim"
                && matches!(result["state"].as_str(), Some("placed" | "redirecting"))
            {
                let resumed = match start_guard {
                    Some((track, Ok(generation))) => {
                        runtime
                            .start_correction(
                                uuid::Uuid::new_v5(&params.op_id, b"redirect.resume"),
                                &track,
                                generation,
                            )
                            .await
                    }
                    _ => Err("The correction's worker is not connected.".into()),
                };
                if let Err(error) = resumed {
                    let _ = journal
                        .resolve_input(
                            uuid::Uuid::new_v5(&params.op_id, b"redirect.hold"),
                            result["id"].as_str().unwrap().into(),
                            result["revision"].as_i64().unwrap(),
                            crate::decision::Resolution::Hold,
                        )
                        .await;
                    return Err((-32005, error.to_string()));
                }
            }
            wake.notify_one();
            Ok(result)
        }
        "utterance.cut" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                #[serde(rename = "opId")]
                op_id: uuid::Uuid,
                #[serde(rename = "utteranceId")]
                input: String,
            }
            let params: Params =
                serde_json::from_value(request.params.clone()).map_err(|_| invalid())?;
            if params.op_id.get_version_num() != 7 {
                return Err(invalid());
            }
            journal
                .cut_input(params.op_id, params.input)
                .await
                .map_err(|_| (-32002, "request could not be cut".into()))
        }
        _ => Err((-32601, "method not found".to_owned())),
    }
}
