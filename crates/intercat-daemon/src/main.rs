use intercat_daemon::{
    Result,
    host::{self, Host},
    instructions::Instructions,
    journal::Journal,
    lock::DaemonLock,
    msp::Event,
    recording, worker, worktree,
};
use serde_json::json;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("intercatd: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|action| action == "inspect-processes" || action == "stop-processes")
    {
        if arguments.len() != 2 {
            return Err("usage: intercatd inspect-processes <data-directory>".into());
        }
        let audit = if arguments[0] == "stop-processes" {
            intercat_daemon::process_audit::stop(&PathBuf::from(&arguments[1]))?
        } else {
            intercat_daemon::process_audit::inspect(&PathBuf::from(&arguments[1]))?
        };
        println!("{}", serde_json::to_string(&audit)?);
        return Ok(());
    }
    if arguments
        .first()
        .is_some_and(|action| action == "review-backup")
    {
        return review_backup(&arguments).await;
    }
    if arguments
        .first()
        .is_some_and(|action| action == "rebuild-fingerprints")
    {
        return rebuild_fingerprints(&arguments).await;
    }
    if arguments.first().is_some_and(|action| action == "serve") {
        if arguments.len() != 3 {
            return Err(std::io::Error::other(
                "usage: intercatd serve <data-directory> <editor-module-directory>",
            )
            .into());
        }
        let directory = PathBuf::from(&arguments[1]);
        let Some(_lock) = DaemonLock::acquire(&directory)? else {
            return Err(std::io::Error::other("daemon already running").into());
        };
        // Both keys arrive only through the inherited pipe, never argv or env.
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let mut bootstrap_pipe = BufReader::new(tokio::io::stdin());
        let mut input = Vec::new();
        (&mut bootstrap_pipe)
            .take(512 * 1024 + 1)
            .read_until(b'\n', &mut input)
            .await?;
        if input.len() > 512 * 1024 || !input.ends_with(b"\n") {
            return Err("Invalid private bootstrap frame.".into());
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Bootstrap {
            token: String,
            secret_key: String,
            receipt: Option<intercat_daemon::fingerprint_receipt::Receipt>,
        }
        let parsed = (|| -> Result<_> {
            let bootstrap: Bootstrap =
                serde_json::from_slice(&input).map_err(|_| "Invalid private bootstrap.")?;
            let result = intercat_daemon::rpc::Token::parse(&bootstrap.token).and_then(|token| {
                intercat_daemon::secrets::Key::parse(&bootstrap.secret_key)
                    .map(|key| (token, key, bootstrap.receipt))
            });
            // These strings are private copies; do not retain them after decoding.
            let mut token_bytes = bootstrap.token.into_bytes();
            let mut key_bytes = bootstrap.secret_key.into_bytes();
            token_bytes.fill(0);
            key_bytes.fill(0);
            result
        })();
        input.fill(0);
        let (token, secret_key, receipt) = parsed?;
        if let Some(receipt) = &receipt {
            receipt.validate()?;
        }
        let journal = Journal::open(directory.join("events.sqlite"), secret_key).await?;
        let mut output = tokio::io::stdout();
        let catalog = journal.fingerprint_catalog().await?;
        if let Some(recovery) = journal.backup_apply_recovery(receipt.clone()).await? {
            output
                .write_all(
                    format!(
                        "{}\n",
                        json!({"error":{"code":"FINGERPRINT_RECEIPT_MISMATCH"},"recovery":recovery})
                    )
                    .as_bytes(),
                )
                .await?;
            output.flush().await?;
            return Err("Finish the prepared backup recovery before opening history.".into());
        }
        if let Some(recovery) = journal.fingerprint_recovery(receipt.clone()).await? {
            output
                .write_all(
                    format!(
                        "{}\n",
                        json!({"error":{"code":"FINGERPRINT_RECEIPT_MISMATCH"},"recovery":recovery})
                    )
                    .as_bytes(),
                )
                .await?;
            output.flush().await?;
            return Err("History review is required after rebuilding fingerprints.".into());
        }
        let required = match catalog.accept(receipt.as_ref()) {
            Ok(required) => required,
            Err(error) => {
                output
                    .write_all(
                        format!(
                            "{}\n",
                            serde_json::json!({"error":{"code":"FINGERPRINT_RECEIPT_MISMATCH"},"recovery":receipt.as_ref().map(|r| catalog.proof.difference(&r.proof))})
                        )
                        .as_bytes(),
                    )
                    .await?;
                output.flush().await?;
                return Err(error);
            }
        };
        output
            .write_all(format!("{}\n", serde_json::json!({"fingerprints":required})).as_bytes())
            .await?;
        output.flush().await?;
        input.clear();
        tokio::time::timeout(
            std::time::Duration::from_secs(90),
            (&mut bootstrap_pipe)
                .take(1025)
                .read_until(b'\n', &mut input),
        )
        .await??;
        if input.len() > 1024 || !input.ends_with(b"\n") {
            return Err("Fingerprint confirmation was not received.".into());
        }
        let confirmed: intercat_daemon::secrets::Seal =
            serde_json::from_slice(&input).map_err(|_| "Invalid fingerprint confirmation.")?;
        if confirmed != required.catalog {
            return Err("The fingerprint confirmation changed.".into());
        }
        journal.confirm_fingerprints(confirmed).await?;
        let editor = intercat_daemon::editor::Manager::new(
            &PathBuf::from(&arguments[2]),
            &directory.join("editor.log"),
        )?;
        if let Some(config) = journal.editor_config().await? {
            editor.restore(config).await;
        }
        let socket_directory = directory.join("run");
        std::fs::create_dir_all(&socket_directory)?;
        if !std::fs::symlink_metadata(&socket_directory)?.is_dir() {
            return Err(std::io::Error::other("RPC directory must not be a symlink").into());
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_directory, std::fs::Permissions::from_mode(0o700))?;
        let server = intercat_daemon::rpc::Server::bind(
            &socket_directory,
            token,
            journal,
            editor.clone(),
            &directory.join("workspaces"),
        )?;
        let result = server
            .serve(async {
                let mut terminate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            })
            .await;
        editor.stop().await;
        return result;
    }
    let diagnostic = arguments
        .first()
        .is_some_and(|action| action == "diagnose-turn");
    if (diagnostic && arguments.len() != 4)
        || (!diagnostic
            && (arguments.len() != 3
                || (arguments[0] != "probe-host" && arguments[0] != "probe-session")))
    {
        return Err(std::io::Error::other(
            "usage: intercatd <probe-host|probe-session> <muse-source> <data-directory> OR diagnose-turn <muse-source> <data-directory> <request.json>",
        )
        .into());
    }
    let source = PathBuf::from(&arguments[1]).canonicalize()?;
    let data = PathBuf::from(&arguments[2]);
    let Some(_lock) = DaemonLock::acquire(&data)? else {
        return Err(
            std::io::Error::other("daemon already running; connect to its RPC socket").into(),
        );
    };
    let data = data.canonicalize()?;
    let mut request = if diagnostic {
        let contents = std::fs::read(&arguments[3])?;
        if contents.len() > 64 * 1024 {
            return Err(std::io::Error::other("diagnostic request exceeds 64 KiB").into());
        }
        Some(serde_json::from_slice::<worker::ProbeRequest>(&contents)?)
    } else {
        None
    };
    let credentials = if diagnostic {
        let file = PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| std::io::Error::other("HOME is missing"))?,
        )
        .join(".config/muse/auth.json");
        if !file.is_file() {
            return Err(std::io::Error::other(
                "Muse sign-in is required before the diagnostic turn",
            )
            .into());
        }
        Some(file)
    } else {
        None
    };
    let journal = Journal::open(
        data.join("events.sqlite"),
        intercat_daemon::secrets::Key::generate()?,
    )
    .await?;
    let source_data = data.clone();
    let binary =
        tokio::task::spawn_blocking(move || host::prepare_binary(&source, &source_data)).await??;
    let host_dir = data.join(format!("hosts/{}", uuid::Uuid::now_v7()));
    let workspace = data.join("probe-workspace");
    if let Some(request) = request.as_mut() {
        let repository = request.repository.canonicalize()?;
        if !repository.starts_with(&data) {
            return Err(std::io::Error::other(
                "diagnostics only accept a disposable repository inside their data directory",
            )
            .into());
        }
        let track_ref = format!("refs/intercat/diagnostic-{}/tracks/A", uuid::Uuid::now_v7());
        let source = worktree::inspect(&repository).await?;
        worktree::prepare(
            &source,
            &workspace,
            &track_ref,
            uuid::Uuid::now_v7(),
            &journal,
        )
        .await?;
        let capture_path = workspace.clone();
        let instructions =
            tokio::task::spawn_blocking(move || Instructions::capture(&capture_path)).await??;
        journal
            .append(
                "instructions.snapshot",
                serde_json::to_value(&instructions)?,
            )
            .await?;
        request.instruction = instructions.with_request(&request.instruction)?;
        journal.append("workspace.prepared",json!({"path":workspace,"repository":source.root,"head":source.head.to_string(),"trackRef":track_ref})).await?;
    } else {
        std::fs::create_dir_all(&workspace)?;
    }
    let path = std::env::var_os("PATH").ok_or_else(|| std::io::Error::other("PATH missing"))?;
    let mut host = Host::spawn(
        &binary,
        &host_dir,
        &workspace,
        &path,
        credentials.as_deref(),
        intercat_daemon::host::Boundary {
            profile: crate::host::Profile::Worker,
            secrets: &[],
            rpc: &data.join("run"),
            denied: &[],
        },
    )
    .await?;
    let initialized = host.connection.client.initialize().await;
    let mut session = None;
    if initialized.is_ok() && (arguments[0] == "probe-session" || diagnostic) {
        let command = uuid::Uuid::now_v7().to_string();
        let params = json!({"commandId":command,"workspaceRoot":workspace,"approvalMode":"promptUnmatched","providerId":"meta","modelId":"muse-spark-1.3-contributor"});
        journal
            .intent(&command, "session.start", params.clone())
            .await?;
        let result = host
            .connection
            .client
            .request("session/start", params)
            .await;
        match result {
            Ok(result) => {
                journal.append("host.session", result.clone()).await?;
                journal.finish(&command, "dispatched").await?;
                if result
                    .pointer("/session/permissionState/profileId")
                    .and_then(serde_json::Value::as_str)
                    != Some("intercat-worker")
                {
                    host.shutdown().await?;
                    return Err(std::io::Error::other(
                        "host did not apply the intercat-worker profile",
                    )
                    .into());
                }
                let sid = &result["session"]["sessionId"];
                let skills = host
                    .connection
                    .client
                    .request("skill/list", json!({"sessionId":sid}))
                    .await?;
                let has_report = skills["skills"].as_array().is_some_and(|skills| {
                    skills.iter().any(|s| s["selector"] == "intercat-report")
                });
                if !has_report {
                    host.shutdown().await?;
                    return Err(std::io::Error::other(
                        "intercat-report is not in the host catalog",
                    )
                    .into());
                }
                journal.finish(&command, "complete").await?;
                session = Some(result["session"].clone());
            }
            Err(error) => {
                host.shutdown().await?;
                return Err(error);
            }
        }
    }
    let diagnostic_result = if let Some(request) = request {
        let sid = session
            .as_ref()
            .and_then(|s| s["sessionId"].as_str())
            .ok_or_else(|| std::io::Error::other("session was not initialized"))?;
        Some(worker::execute_once(&mut host, &journal, &workspace, sid, request).await)
    } else {
        None
    };
    let stop_verification = if let Some(Ok(report)) = diagnostic_result.as_ref() {
        Some(worker::verify_isolated_stop(&host, report).await)
    } else {
        None
    };
    if initialized.is_ok() {
        // Record the diagnostic frames while the reader independently services RPC replies.
        while let Ok(event) = host.connection.try_next_event() {
            match event {
                Event::Frame(received) => {
                    journal
                        .append("msp.received", recording::sanitize(received.frame, &[]))
                        .await?;
                }
                Event::Closed(reason) => {
                    host.shutdown().await?;
                    return Err(std::io::Error::other(reason).into());
                }
            }
        }
    }
    for frame in host.shutdown().await? {
        journal
            .append("msp.received", recording::sanitize(frame, &[]))
            .await?;
    }
    let initialized = initialized?;
    let report = diagnostic_result.transpose()?;
    let stop_verification = stop_verification.transpose()?.flatten();
    journal.append("host.verified", json!({"sha256":host::BINARY_SHA256,"version":host::BINARY_VERSION,"fingerprint":initialized["schema"]["fingerprint"]})).await?;
    println!(
        "{}",
        json!({"verified":true,"schema":initialized["schema"],"session":session,"requestedTurns":usize::from(diagnostic),"report":report,"stopVerification":stop_verification,"database":data.join("events.sqlite")})
    );
    Ok(())
}

async fn rebuild_fingerprints(arguments: &[std::ffi::OsString]) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    if arguments.len() != 2 {
        return Err("usage: intercatd rebuild-fingerprints <data-directory>".into());
    }
    let directory = PathBuf::from(&arguments[1]);
    let Some(_lock) = DaemonLock::acquire(&directory)? else {
        return Err("The daemon must be stopped before fingerprint recovery.".into());
    };
    let mut bytes = Vec::new();
    BufReader::new(tokio::io::stdin())
        .take(512 * 1024 + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    if bytes.len() > 512 * 1024 || !bytes.ends_with(b"\n") {
        return Err("Invalid fingerprint recovery frame.".into());
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Input {
        secret_key: String,
        request: intercat_daemon::fingerprint_recovery::Request,
    }
    let input: Input =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid fingerprint recovery request.")?;
    bytes.fill(0);
    let key = intercat_daemon::secrets::Key::parse(&input.secret_key);
    let mut raw_key = input.secret_key.into_bytes();
    raw_key.fill(0);
    let journal = Journal::open(directory.join("events.sqlite"), key?).await?;
    let response = match journal.rebuild_fingerprints(input.request).await {
        Ok(recovery) => json!({"recovery":recovery}),
        Err(error) => json!({"error":{"message":error.to_string()}}),
    };
    let mut output = tokio::io::stdout();
    output.write_all(format!("{response}\n").as_bytes()).await?;
    output.flush().await?;
    Ok(())
}

async fn review_backup(arguments: &[std::ffi::OsString]) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    if arguments.len() != 2 {
        return Err("usage: intercatd review-backup <data-directory>".into());
    }
    let directory = PathBuf::from(&arguments[1]);
    let Some(_lock) = DaemonLock::acquire(&directory)? else {
        return Err("Stop the daemon before reviewing a restored database.".into());
    };
    let mut bytes = Vec::new();
    BufReader::new(tokio::io::stdin())
        .take(512 * 1024 + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    if bytes.len() > 512 * 1024 || !bytes.ends_with(b"\n") {
        return Err("Invalid backup review frame.".into());
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Input {
        secret_key: String,
        receipt: intercat_daemon::fingerprint_receipt::Receipt,
        request: intercat_daemon::backup_review::Request,
    }
    let input: Input =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid backup review request.")?;
    bytes.fill(0);
    let key = intercat_daemon::secrets::Key::parse(&input.secret_key);
    let mut raw_key = input.secret_key.into_bytes();
    raw_key.fill(0);
    let journal = Journal::open(directory.join("events.sqlite"), key?).await?;
    let response = match intercat_daemon::backup_review::run(
        &journal,
        &directory,
        input.receipt,
        input.request,
    )
    .await
    {
        Ok(review) => json!({"review":review}),
        Err(error) => json!({"error":{"message":error.to_string()}}),
    };
    let mut output = tokio::io::stdout();
    output.write_all(format!("{response}\n").as_bytes()).await?;
    output.flush().await?;
    Ok(())
}
