//! Own the local editor process independently of model calls and worker Stop.
use crate::{Result, screening::EditorRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, watch},
    task::JoinHandle,
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub python: PathBuf,
    pub model: PathBuf,
}

impl Config {
    pub fn validated(mut self) -> Result<Self> {
        // A venv's interpreter is often a symlink. Preserve that invocation
        // path so Python discovers pyvenv.cfg and its installed MLX packages.
        self.python = std::path::absolute(&self.python)?;
        self.model = self.model.canonicalize()?;
        if !self.python.is_file() || !self.model.is_dir() {
            return Err(std::io::Error::other(
                "choose a Python executable and a local model directory",
            )
            .into());
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub phase: &'static str,
    pub process: bool,
    pub accepting: bool,
    pub model_loaded: bool,
    pub generation: u64,
    pub restart: u8,
    pub pid: Option<u32>,
    pub detail: Option<String>,
    pub can_retry: bool,
    pub download: Option<Download>,
    #[serde(skip)]
    endpoint: Option<String>,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub received: u64,
    pub total: u64,
    pub phase: String,
    pub file: Option<String>,
}
impl Status {
    fn initial(generation: u64, phase: &'static str) -> Self {
        Self {
            phase,
            generation,
            process: false,
            accepting: false,
            model_loaded: false,
            restart: 0,
            pid: None,
            detail: None,
            can_retry: false,
            download: None,
            endpoint: None,
        }
    }
}
struct Control {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}
struct Inner {
    source: PathBuf,
    log: PathBuf,
    model_root: PathBuf,
    http: reqwest::Client,
    status: watch::Sender<Status>,
    control: Mutex<Option<Control>>,
}
#[derive(Clone)]
pub struct Manager(Arc<Inner>);

pub struct Inference {
    response: reqwest::Response,
    manager: Manager,
    generation: u64,
}

impl Inference {
    pub async fn finish(self, budget: Duration) -> Result<Value> {
        let body = tokio::time::timeout(budget, bounded_json(self.response))
            .await
            .map_err(|_| InferenceTimeout)??;
        let now = self.manager.status();
        if now.generation != self.generation || !now.model_loaded {
            return Err(std::io::Error::other("editor changed during inference").into());
        }
        Ok(body)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Input,
    Question,
    Park,
    Background,
}
impl Priority {
    fn header(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Question => "question",
            Self::Park => "park",
            Self::Background => "background",
        }
    }
}

pub(crate) struct ReadingCancellation {
    pub editor: Manager,
    pub request: crate::input_readings::Ticket,
}
impl Drop for ReadingCancellation {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let editor = self.editor.clone();
            let request = self.request.clone();
            runtime.spawn(async move {
                let _ = editor.cancel_reading(&request).await;
            });
        }
    }
}

impl Manager {
    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn new(source: &Path, log: &Path) -> Result<Self> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(2))
            .build()?;
        let (status, _) = watch::channel(Status::initial(0, "not_configured"));
        Ok(Self(Arc::new(Inner {
            source: source.canonicalize()?,
            log: log.to_owned(),
            model_root: log
                .parent()
                .ok_or("Editor log has no parent directory")?
                .canonicalize()?
                .join("editor-model"),
            http,
            status,
            control: Mutex::new(None),
        })))
    }
    pub fn status(&self) -> Status {
        self.0.status.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.0.status.subscribe()
    }
    pub async fn restore(&self, saved: Value) {
        let result = match serde_json::from_value::<Config>(saved) {
            Ok(config) if config.model == self.model_path() && !self.download_complete() => {
                self.stop().await;
                let mut state = Status::initial(self.status().generation + 1, "download_paused");
                state.detail =
                    Some("The Editor download was interrupted. Resume to continue.".into());
                self.0.status.send_replace(state);
                Ok(())
            }
            Ok(config) => self.configure(config).await,
            Err(error) => Err(error.into()),
        };
        if let Err(error) = result {
            self.stop().await;
            let mut state = Status::initial(self.status().generation + 1, "unavailable");
            state.detail = Some(error.to_string());
            state.can_retry = true;
            self.0.status.send_replace(state);
        }
    }
    pub fn model_path(&self) -> PathBuf {
        self.0.model_root.join("model")
    }
    pub fn download_config(&self, python: PathBuf) -> Result<Config> {
        use std::os::unix::fs::PermissionsExt;
        for path in [&self.0.model_root, &self.model_path()] {
            std::fs::create_dir_all(path)?;
            if !std::fs::symlink_metadata(path)?.is_dir() {
                return Err("Editor storage must not be a symbolic link".into());
            }
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        Config {
            python,
            model: self.model_path(),
        }
        .validated()
    }
    fn download_complete(&self) -> bool {
        let manifest: Value = serde_json::from_str(include_str!(
            "../../../services/editor/src/intercat_editor/model_manifest.json"
        ))
        .expect("bundled model manifest");
        std::fs::read(self.0.model_root.join("complete.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|value| value["revision"] == manifest["revision"])
    }
    pub async fn download(&self, config: Config) -> Result<()> {
        let config = tokio::task::spawn_blocking(move || config.validated()).await??;
        if config.model != self.model_path() {
            return Err("Invalid Editor download destination".into());
        }
        let mut control = self.0.control.lock().await;
        if let Some(previous) = control.take() {
            let _ = previous.stop.send(true);
            let _ = previous.task.await;
        }
        let generation = self.status().generation + 1;
        let state = Status::initial(generation, "downloading");
        self.0.status.send_replace(state.clone());
        let (stop, mut stopping) = watch::channel(false);
        let inner = self.0.clone();
        let task = tokio::spawn(async move {
            let mut state = state;
            match download_model(&inner, &config, &mut state, &mut stopping).await {
                Ok(true) if !*stopping.borrow() => {
                    supervise(inner, config, generation, stopping).await
                }
                result => {
                    state.phase = "download_paused";
                    state.process = false;
                    state.pid = None;
                    state.detail = result.err().map(|error| error.to_string());
                    inner.status.send_replace(state);
                }
            }
        });
        *control = Some(Control { stop, task });
        Ok(())
    }
    pub async fn pause_download(&self) -> Result<()> {
        let mut control = self.0.control.lock().await;
        if self.status().phase == "download_paused" {
            return Ok(());
        }
        if self.status().phase != "downloading" {
            return Err("No Editor download is running".into());
        }
        if let Some(previous) = control.take() {
            let _ = previous.stop.send(true);
            let _ = previous.task.await;
        }
        Ok(())
    }
    pub async fn configure(&self, config: Config) -> Result<()> {
        let config = tokio::task::spawn_blocking(move || config.validated()).await??;
        let mut control = self.0.control.lock().await;
        if let Some(previous) = control.take() {
            let _ = previous.stop.send(true);
            let _ = previous.task.await;
        }
        let generation = self.status().generation + 1;
        self.0
            .status
            .send_replace(Status::initial(generation, "starting"));
        let (stop, stopping) = watch::channel(false);
        let inner = self.0.clone();
        let task = tokio::spawn(async move {
            supervise(inner, config, generation, stopping).await;
        });
        *control = Some(Control { stop, task });
        Ok(())
    }
    pub async fn stop(&self) {
        let mut control = self.0.control.lock().await;
        if let Some(previous) = control.take() {
            let _ = previous.stop.send(true);
            let _ = previous.task.await;
        }
        self.0
            .status
            .send_replace(Status::initial(self.status().generation + 1, "stopped"));
    }
    pub async fn decide(
        &self,
        request: EditorRequest,
        priority: Priority,
        reading: Option<&crate::input_readings::Ticket>,
    ) -> Result<Inference> {
        self.admit(
            "decide",
            request,
            priority,
            Duration::from_secs(120),
            reading,
        )
        .await
    }
    pub async fn analyze_report(
        &self,
        request: EditorRequest,
        priority: Priority,
        remaining: Duration,
        reading: Option<&crate::input_readings::Ticket>,
    ) -> Result<Value> {
        self.request("decide", request, remaining, priority, remaining, reading)
            .await
    }
    pub(crate) fn privacy_ready(&self) -> bool {
        let status = self.status();
        status.model_loaded
            && status.accepting
            && status
                .endpoint
                .and_then(|value| reqwest::Url::parse(&value).ok())
                .is_some_and(|url| {
                    url.scheme() == "http"
                        && url.host_str() == Some("127.0.0.1")
                        && url.port().is_some()
                })
    }
    pub(crate) async fn inspect_privacy(
        &self,
        request: EditorRequest,
        priority: Priority,
    ) -> Result<Value> {
        if !self.privacy_ready() {
            return Err("Full privacy is waiting for the local Editor.".into());
        }
        let reading = crate::input_readings::Ticket {
            input: uuid::Uuid::now_v7().to_string(),
            revision: 1,
            index: 1,
        };
        let _cancel = ReadingCancellation {
            editor: self.clone(),
            request: reading.clone(),
        };
        let prompt = format!(
            "{}{}\nFindings:\n",
            request.params["prefix"]
                .as_str()
                .ok_or("Missing privacy prefix.")?,
            request.params["tail"]
                .as_str()
                .ok_or("Missing privacy sources.")?
        );
        let counter = EditorRequest {
            params: serde_json::json!({"text":prompt}),
            seal: request.seal.clone(),
            permit: request.permit.clone(),
        };
        let tokens = self
            .request(
                "tokens",
                counter,
                Duration::from_secs(5),
                priority,
                Duration::from_secs(30),
                Some(&reading),
            )
            .await?;
        if tokens["tokens"].as_u64().is_none_or(|count| count > 8000) {
            return Err("Full privacy context exceeds the local model limit.".into());
        }
        self.request(
            "privacy",
            request,
            Duration::from_secs(30),
            priority,
            Duration::from_secs(30),
            Some(&reading),
        )
        .await
    }
    pub async fn generate(
        &self,
        request: EditorRequest,
        reading: Option<&crate::input_readings::Ticket>,
    ) -> Result<Value> {
        self.request(
            "generate",
            request,
            Duration::from_secs(30),
            Priority::Background,
            Duration::from_secs(30),
            reading,
        )
        .await
    }
    pub async fn count_tokens(
        &self,
        request: &EditorRequest,
        priority: Priority,
        reading: Option<&crate::input_readings::Ticket>,
    ) -> Result<usize> {
        self.count_prepared_tokens(request.token_count()?, priority, reading)
            .await
    }
    pub(crate) async fn count_generation_tokens(
        &self,
        request: &EditorRequest,
        reading: Option<&crate::input_readings::Ticket>,
    ) -> Result<usize> {
        self.count_prepared_tokens(
            request.generation_token_count()?,
            Priority::Background,
            reading,
        )
        .await
    }
    async fn count_prepared_tokens(
        &self,
        request: EditorRequest,
        priority: Priority,
        reading: Option<&crate::input_readings::Ticket>,
    ) -> Result<usize> {
        let response = self
            .request(
                "tokens",
                request,
                Duration::from_secs(5),
                priority,
                Duration::from_secs(120),
                reading,
            )
            .await?;
        response["tokens"]
            .as_u64()
            .and_then(|count| usize::try_from(count).ok())
            .ok_or_else(|| "The Editor returned an invalid token count.".into())
    }
    async fn request(
        &self,
        path: &str,
        request: EditorRequest,
        budget: Duration,
        priority: Priority,
        total_budget: Duration,
        reading: Option<&crate::input_readings::Ticket>,
    ) -> Result<Value> {
        self.admit(path, request, priority, total_budget, reading)
            .await?
            .finish(budget)
            .await
    }

    async fn admit(
        &self,
        path: &str,
        request: EditorRequest,
        priority: Priority,
        total_budget: Duration,
        reading: Option<&crate::input_readings::Ticket>,
    ) -> Result<Inference> {
        let status = self.status();
        let endpoint = status
            .endpoint
            .filter(|_| status.model_loaded)
            .ok_or_else(|| std::io::Error::other("editor unavailable"))?;
        let mut sending = self
            .0
            .http
            .post(format!("{endpoint}/{path}"))
            .header("X-Intercat-Priority", priority.header())
            .header(
                "X-Intercat-Timeout-Ms",
                total_budget.as_millis().clamp(1, 120_000).to_string(),
            )
            .json(request.params())
            .timeout(total_budget);
        if let Some(reading) = reading {
            sending = sending
                .header("X-Intercat-Reading", &reading.input)
                .header("X-Intercat-Reading-Index", reading.index.to_string());
        }
        let response = tokio::spawn(async move {
            let response = sending.send().await;
            // The Editor reads the request body before returning headers. Keep
            // its lease through this point even if the waiting caller cancels.
            drop(request);
            response
        })
        .await??;
        if response
            .headers()
            .get("X-Intercat-Inference")
            .and_then(|value| value.to_str().ok())
            != Some("started")
        {
            return Err(std::io::Error::other("editor did not report inference admission").into());
        }
        Ok(Inference {
            response,
            manager: self.clone(),
            generation: status.generation,
        })
    }

    pub async fn cancel_reading(&self, reading: &crate::input_readings::Ticket) -> Result<()> {
        if let Some(endpoint) = self.status().endpoint {
            self.0
                .http
                .post(format!("{endpoint}/cancel-reading"))
                .json(&serde_json::json!({"input":reading.input,"index":reading.index}))
                .timeout(Duration::from_secs(2))
                .send()
                .await?
                .error_for_status()?;
        }
        Ok(())
    }
}

async fn download_model(
    inner: &Inner,
    config: &Config,
    state: &mut Status,
    stopping: &mut watch::Receiver<bool>,
) -> Result<bool> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&inner.log)?;
    let mut child = Command::new(&config.python)
        .args(["-m", "intercat_editor.model_store", "--root"])
        .arg(&inner.model_root)
        .current_dir(inner.log.parent().ok_or("Missing Editor data directory")?)
        .env_clear()
        .env("HOME", &inner.model_root)
        .env("PYTHONPATH", &inner.source)
        .env("PYTHONUNBUFFERED", "1")
        .env("HF_HUB_DISABLE_TELEMETRY", "1")
        .env("HF_HUB_DISABLE_IMPLICIT_TOKEN", "1")
        .env("HF_HOME", inner.model_root.join("hub-home"))
        .env("LANG", "en_US.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()?;
    let _lifetime = child.stdin.take();
    state.pid = child.id();
    state.process = true;
    inner.status.send_replace(state.clone());
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut completed = false;
    let result = async {
        loop {
            let mut line = Vec::new();
            let mut bounded = (&mut reader).take(4097);
            let count = tokio::select! {
                biased;
                _ = stopping.changed() => return Ok(false),
                value = bounded.read_until(b'\n', &mut line) => value?,
            };
            if count == 0 {
                break;
            }
            if line.len() > 4096 {
                return Err("Invalid Editor download progress".into());
            }
            let progress: Value = serde_json::from_slice(&line)?;
            let phase = progress["phase"]
                .as_str()
                .ok_or("Missing Editor download phase")?;
            if phase == "failed" {
                return Err(progress["detail"]
                    .as_str()
                    .unwrap_or("Editor download failed")
                    .to_owned()
                    .into());
            }
            if !["checking", "downloading", "verifying", "complete"].contains(&phase) {
                return Err("Invalid Editor download phase".into());
            }
            let total = progress["total"]
                .as_u64()
                .ok_or("Invalid Editor download size")?;
            let received = progress["received"]
                .as_u64()
                .filter(|value| *value <= total)
                .ok_or("Invalid Editor download progress")?;
            state.download = Some(Download {
                received: received.max(state.download.as_ref().map_or(0, |old| old.received)),
                total,
                phase: phase.into(),
                file: progress["file"].as_str().map(str::to_owned),
            });
            if phase == "complete" {
                if progress["model"].as_str().map(PathBuf::from).as_ref() != Some(&config.model) {
                    return Err("Editor download returned an unexpected directory".into());
                }
                completed = true;
            }
            inner.status.send_replace(state.clone());
        }
        let exited = tokio::select! {
            _ = stopping.changed() => return Ok(false),
            value = child.wait() => value?,
        };
        if !exited.success() || !completed {
            return Err("Editor download did not finish".into());
        }
        Ok(true)
    }
    .await;
    terminate(&mut child).await;
    result
}

#[derive(Debug)]
pub struct InferenceTimeout;
impl std::fmt::Display for InferenceTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("editor inference exceeded 1.2 seconds")
    }
}
impl std::error::Error for InferenceTimeout {}

async fn bounded_json(mut response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success() {
        return Err(std::io::Error::other(format!("editor HTTP {}", response.status())).into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 64 * 1024 {
            return Err(std::io::Error::other("editor response exceeds limit").into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

async fn supervise(
    inner: Arc<Inner>,
    config: Config,
    generation: u64,
    mut stopping: watch::Receiver<bool>,
) {
    for restart in 0..=3 {
        if *stopping.borrow() {
            return;
        }
        if restart > 0 {
            tokio::select! { _ = stopping.changed() => return, _ = tokio::time::sleep(Duration::from_secs(1 << (restart - 1))) => {} }
        }
        let mut state = Status::initial(generation, "starting");
        state.restart = restart;
        inner.status.send_replace(state.clone());
        let result = one_process(&inner, &config, &mut state, &mut stopping).await;
        if *stopping.borrow() {
            return;
        }
        state.phase = "unavailable";
        state.process = false;
        state.model_loaded = false;
        state.accepting = false;
        state.endpoint = None;
        state.pid = None;
        state.detail = Some(
            result
                .err()
                .map_or_else(|| "editor exited".into(), |error| error.to_string()),
        );
        state.can_retry = restart == 3;
        inner.status.send_replace(state);
    }
}

async fn terminate(child: &mut Child) {
    if let Some(pid) = child.id() {
        // The unreaped child is owned here; its pid cannot be reused yet.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

async fn one_process(
    inner: &Inner,
    config: &Config,
    state: &mut Status,
    stopping: &mut watch::Receiver<bool>,
) -> Result<()> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&inner.log)?;
    let mut child = Command::new(&config.python)
        .args(["-m", "intercat_editor.server", "--model"])
        .arg(&config.model)
        .current_dir(inner.log.parent().ok_or("Missing Editor data directory")?)
        .env_clear()
        .env(
            "HOME",
            std::env::var_os("HOME").ok_or_else(|| std::io::Error::other("HOME is missing"))?,
        )
        .env("PYTHONPATH", &inner.source)
        .env("PYTHONUNBUFFERED", "1")
        .env("LANG", "en_US.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()?;
    let _lifetime = child.stdin.take();
    state.pid = child.id();
    state.process = true;
    inner.status.send_replace(state.clone());
    let stdout = child.stdout.take().unwrap();
    let boot = async {
        let mut line = String::new();
        BufReader::new(stdout)
            .take(1025)
            .read_line(&mut line)
            .await?;
        if line.len() > 1024 {
            return Err(std::io::Error::other("editor handshake exceeds limit").into());
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Address {
            host: String,
            port: u16,
        }
        let address: Address = serde_json::from_str(&line)?;
        if address.host != "127.0.0.1" || address.port == 0 {
            return Err(std::io::Error::other("editor returned a non-loopback address").into());
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(format!(
            "http://127.0.0.1:{}",
            address.port
        ))
    };
    let endpoint = tokio::select! {
        _ = stopping.changed() => { terminate(&mut child).await; return Ok(()); },
        result = tokio::time::timeout(Duration::from_secs(10), boot) => match result {
            Ok(Ok(endpoint)) => endpoint,
            Ok(Err(error)) => { terminate(&mut child).await; return Err(error); },
            Err(error) => { terminate(&mut child).await; return Err(error.into()); },
        }
    };
    state.endpoint = Some(endpoint.clone());
    let mut failures = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let result = loop {
        let health = async {
            bounded_json(
                inner
                    .http
                    .get(format!("{endpoint}/health"))
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await?,
            )
            .await
        };
        tokio::select! {
            _ = stopping.changed() => break Ok(()),
            exited = child.wait() => break Err(std::io::Error::other(format!("editor process exited: {}", exited?)).into()),
            health = health => {
                match health {
                    Ok(value) => {
                        failures = 0;
                        state.accepting = value.get("accepting").and_then(Value::as_bool) == Some(true);
                        state.model_loaded = value.get("modelLoaded").and_then(Value::as_bool) == Some(true);
                        if !state.accepting {
                            break Err(std::io::Error::other(value.get("loadError").and_then(Value::as_str).unwrap_or("editor stopped accepting work")).into());
                        }
                        state.phase = if state.accepting && state.model_loaded { "ready" } else { "starting" };
                        inner.status.send_if_modified(|old| { if old == state { false } else { old.clone_from(state); true } });
                        if !state.model_loaded && tokio::time::Instant::now() >= deadline { break Err(std::io::Error::other("editor model loading timed out").into()); }
                    },
                    Err(error) => { failures += 1; if failures >= 3 { break Err(error); } }
                }
            }
        }
        tokio::select! { _ = stopping.changed() => break Ok(()), _ = tokio::time::sleep(Duration::from_millis(250)) => {} }
    };
    terminate(&mut child).await;
    result
}
