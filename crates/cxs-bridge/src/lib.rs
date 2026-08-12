use std::collections::{HashMap, VecDeque};
use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use cxs_core::{
    AGENT_MAGIC, AGENT_PROTOCOL_VERSION, AgentHandshake, Profile, ProfileStore, SHIM_MAGIC,
    SHIM_PROTOCOL_VERSION, ShimHandshake, ShimTransport, routing,
};
use cxs_mux::{ChannelKind, MuxHandle, MuxSession};
use futures_util::{SinkExt, StreamExt};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Map, Value, json};
use subtle::ConstantTimeEq;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

const MAX_HANDSHAKE_BYTES: usize = 8 * 1024;
const MAX_JSON_LINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PENDING_CLIENT_BYTES: usize = 32 * 1024 * 1024;
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(2);
const PROJECT_METADATA_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_INSTRUCTION_SOURCES: usize = 32;
const MAX_INSTRUCTION_SOURCE_BYTES: usize = 256 * 1024;
const MAX_INSTRUCTION_TOTAL_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy)]
enum RequestRoute {
    Local,
    Host,
    ThreadStart,
}

struct PendingThreadMetadata {
    client_id: Value,
    local_request: Value,
    host_response: Option<Value>,
    pending_instruction_reads: HashMap<String, String>,
    instruction_paths: Vec<String>,
    instruction_contents: HashMap<String, String>,
    started_at: tokio::time::Instant,
    local_sent: bool,
}

pub async fn serve(profile: Profile, store: ProfileStore, codex: PathBuf) -> Result<()> {
    let expected_token = store.read_token(&profile)?;
    let (mux, mut agent) = start_agent(&profile, &expected_token).await?;
    let agent_pid = agent
        .id()
        .and_then(|value| i32::try_from(value).ok())
        .map(Pid::from_raw);
    let result = serve_multiplexed(profile, store, codex, mux).await;
    stop_child(&mut agent, agent_pid).await;
    result
}

async fn start_agent(profile: &Profile, expected_token: &str) -> Result<(MuxSession, Child)> {
    let mut child = Command::new("ssh");
    child
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            &profile.app_alias,
            "$HOME/.local/lib/codex-shuttle/current/cxs-shim __cxs-agent --replace",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = child.spawn().with_context(|| {
        format!(
            "could not start Shuttle agent through {}",
            profile.app_alias
        )
    })?;
    let stdin = child
        .stdin
        .take()
        .context("agent SSH stdin was unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("agent SSH stdout was unavailable")?;
    let mut reader = BufReader::new(stdout);
    let hello = timeout(Duration::from_secs(10), read_agent_handshake(&mut reader))
        .await
        .context("timed out waiting for the remote Shuttle agent")??;
    validate_agent_handshake(&hello, profile, expected_token)?;
    Ok((cxs_mux::start(reader, stdin, cxs_mux::Role::Bridge), child))
}

#[doc(hidden)]
pub async fn serve_multiplexed(
    profile: Profile,
    store: ProfileStore,
    codex: PathBuf,
    mut mux: MuxSession,
) -> Result<()> {
    prepare_socket(&profile.local_socket)?;
    let listener = UnixListener::bind(&profile.local_socket)
        .with_context(|| format!("could not bind {}", profile.local_socket.display()))?;
    fs::set_permissions(&profile.local_socket, fs::Permissions::from_mode(0o600))?;
    let _socket_guard = SocketGuard(profile.local_socket.clone());
    let exec_listener = TcpListener::bind(("127.0.0.1", profile.local_exec_port))
        .await
        .with_context(|| {
            format!(
                "could not bind multiplexed Exec endpoint 127.0.0.1:{}",
                profile.local_exec_port
            )
        })?;
    let expected_token = store.read_token(&profile)?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("could not listen for SIGTERM")?;

    info!(profile = %profile.name, socket = %profile.local_socket.display(), exec_port = profile.local_exec_port, "multiplexed bridge is listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let profile = profile.clone();
                let token = expected_token.clone();
                let codex = codex.clone();
                let mux = mux.handle.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, &profile, &token, &codex, mux).await {
                        warn!(profile = %profile.name, error = %error, "bridge connection failed");
                    }
                });
            }
            accepted = exec_listener.accept() => {
                let (mut socket, _) = accepted?;
                let handle = mux.handle.clone();
                let name = profile.name.clone();
                tokio::spawn(async move {
                    let result = async {
                        let mut channel = handle.open(ChannelKind::Exec).await?;
                        tokio::io::copy_bidirectional(&mut socket, &mut channel).await?;
                        Ok::<_, anyhow::Error>(())
                    }.await;
                    if let Err(error) = result {
                        warn!(profile = %name, error = %error, "Exec multiplexed channel failed");
                    }
                });
            }
            incoming = mux.incoming.recv() => {
                let Some(incoming) = incoming else {
                    bail!("remote Shuttle agent stopped accepting channels");
                };
                if incoming.kind != ChannelKind::App {
                    bail!("remote Shuttle agent opened an unsupported {:?} channel", incoming.kind);
                }
                let stream = incoming.stream;
                let profile = profile.clone();
                let token = expected_token.clone();
                let codex = codex.clone();
                let mux = mux.handle.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, &profile, &token, &codex, mux).await {
                        warn!(profile = %profile.name, error = %error, "multiplexed App channel failed");
                    }
                });
            }
            result = &mut mux.task => {
                return result.context("multiplexed session task failed")?;
            }
            signal = tokio::signal::ctrl_c() => {
                signal.context("could not listen for Ctrl-C")?;
                info!(profile = %profile.name, "bridge is shutting down");
                return Ok(());
            }
            signal = terminate.recv() => {
                if signal.is_some() {
                    info!(profile = %profile.name, "bridge received SIGTERM");
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_connection<S>(
    mut stream: S,
    profile: &Profile,
    expected_token: &str,
    codex: &Path,
    mux: MuxHandle,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let handshake = read_handshake(&mut stream).await?;
    validate_handshake(&handshake, profile, expected_token)?;
    let forwarded_arguments = app_server_arguments(&handshake.app_server_args)?;

    if handshake.transport == ShimTransport::WebSocket {
        return handle_websocket_connection(stream, profile, codex, &forwarded_arguments, mux)
            .await;
    }

    let mut command = Command::new(codex);
    command
        .args(&forwarded_arguments)
        .args([
            "--stdio",
            "--enable",
            "deferred_executor",
            "--enable",
            "executor_capability_discovery",
        ])
        .env("CODEX_HOME", &profile.codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start {} app-server", codex.display()))?;
    let pid = child
        .id()
        .and_then(|value| i32::try_from(value).ok())
        .map(Pid::from_raw);
    let child_stdin = child
        .stdin
        .take()
        .context("app-server stdin was unavailable")?;
    let child_stdout = child
        .stdout
        .take()
        .context("app-server stdout was unavailable")?;
    let (client_read, mut client_write) = tokio::io::split(stream);
    let mut client_read = BufReader::new(client_read);
    let mut child_stdout = BufReader::new(child_stdout);
    let mut child_stdin = child_stdin;

    info!(profile = %profile.name, ?pid, "app-server relay started");
    let relay_result = relay_jsonl(
        &mut client_read,
        &mut client_write,
        &mut child_stdout,
        &mut child_stdin,
        profile,
    )
    .await;
    let _ = child_stdin.shutdown().await;
    let _ = client_write.shutdown().await;
    stop_child(&mut child, pid).await;
    relay_result.context("app-server byte relay failed")?;
    info!(profile = %profile.name, "app-server relay stopped");
    Ok(())
}

async fn handle_websocket_connection<S>(
    stream: S,
    profile: &Profile,
    codex: &Path,
    forwarded_arguments: &[String],
    mux: MuxHandle,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let socket_directory = tempfile::Builder::new()
        .prefix("cxs-u-")
        .tempdir_in("/private/tmp")
        .context("could not create a private App Server socket directory")?;
    let socket_path = socket_directory.path().join("app.sock");
    let listen_url = format!("unix://{}", socket_path.display());
    let mut command = Command::new(codex);
    command
        .args(forwarded_arguments)
        .args([
            "--listen",
            &listen_url,
            "--enable",
            "deferred_executor",
            "--enable",
            "executor_capability_discovery",
        ])
        .env("CODEX_HOME", &profile.codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start {} app-server", codex.display()))?;
    let pid = child
        .id()
        .and_then(|value| i32::try_from(value).ok())
        .map(Pid::from_raw);
    wait_for_socket(&socket_path, &mut child).await?;
    let _socket_guard = SocketGuard(socket_path.clone());

    let client = tokio_tungstenite::accept_async(stream)
        .await
        .context("could not accept the App WebSocket")?;
    let upstream_stream = UnixStream::connect(&socket_path)
        .await
        .context("could not connect to the local App Server socket")?;
    let (upstream, _) =
        tokio_tungstenite::client_async("ws://codex-app-server/rpc", upstream_stream)
            .await
            .context("could not open the local App Server WebSocket")?;
    let host = mux
        .open(ChannelKind::Host)
        .await
        .context("could not open the remote Host App Server channel")?;

    let relay_result = relay_websocket(client, upstream, host, profile).await;
    stop_child(&mut child, pid).await;
    relay_result
}

async fn wait_for_socket(path: &Path, child: &mut Child) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(metadata) = fs::symlink_metadata(path)
            && metadata.file_type().is_socket()
        {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("App Server exited during startup with {status}");
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "App Server did not create {} within 5 seconds",
                path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[allow(clippy::too_many_lines)]
async fn relay_websocket<C, H>(
    client: tokio_tungstenite::WebSocketStream<C>,
    upstream: tokio_tungstenite::WebSocketStream<UnixStream>,
    host: H,
    profile: &Profile,
) -> Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
    H: AsyncRead + AsyncWrite + Unpin,
{
    let (mut client_write, mut client_read) = client.split();
    let (mut server_write, mut server_read) = upstream.split();
    let (host_read, mut host_write) = tokio::io::split(host);
    let mut host_read = BufReader::new(host_read);
    let internal_request_id = format!("cxs/{}/environment-add", profile.environment_id);
    let internal_status_id = format!("cxs/{}/environment-status", profile.environment_id);
    let host_initialize_id = format!("cxs/{}/host-initialize", profile.environment_id);
    let mut initialize_id: Option<Value> = None;
    let mut environment_request_sent = false;
    let mut environment_ready = false;
    let mut environment_status_attempts = 0_u16;
    let mut local_initialize_response: Option<Value> = None;
    let mut host_initialize_response: Option<Value> = None;
    let mut initialize_response_sent = false;
    let mut pending_client_messages: VecDeque<(String, RequestRoute)> = VecDeque::new();
    let mut pending_client_bytes = 0_usize;
    let mut metadata_sequence = 0_u64;
    let mut pending_thread_metadata: HashMap<String, PendingThreadMetadata> = HashMap::new();

    loop {
        tokio::select! {
            message = client_read.next() => {
                let Some(message) = message else { return Ok(()); };
                let message = message.context("invalid App WebSocket frame")?;
                if let Message::Text(text) = message {
                    let rewritten = rewrite_client_message(text.as_bytes(), profile, &mut initialize_id)?;
                    let value = serde_json::from_slice::<Value>(&rewritten)?;
                    let method = value
                        .get("method")
                        .and_then(Value::as_str);
                    let is_initialize = method == Some("initialize");
                    let route = request_route(method);
                    let rewritten = String::from_utf8(rewritten)?;
                    if is_initialize {
                        let mut host_initialize = value;
                        host_initialize["id"] = Value::String(host_initialize_id.clone());
                        write_line(&mut host_write, &serde_json::to_vec(&host_initialize)?).await?;
                        server_write.send(Message::Text(rewritten.into())).await?;
                    } else if initialize_id.is_some()
                        && (!environment_ready || !initialize_response_sent)
                    {
                        pending_client_bytes = pending_client_bytes.saturating_add(rewritten.len());
                        if pending_client_bytes > MAX_PENDING_CLIENT_BYTES {
                            bail!("client sent too much data before the remote host became ready");
                        }
                        pending_client_messages.push_back((rewritten, route));
                    } else {
                        forward_client_message(
                            rewritten,
                            route,
                            &mut server_write,
                            &mut host_write,
                            &mut pending_thread_metadata,
                            &mut metadata_sequence,
                            profile,
                        ).await?;
                    }
                } else {
                    server_write.send(message).await?;
                }
            }
            message = server_read.next() => {
                let Some(message) = message else { return Ok(()); };
                let message = message.context("invalid App Server WebSocket frame")?;
                if let Message::Text(text) = &message {
                    let value: Value = serde_json::from_str(text)
                        .context("app-server emitted invalid WebSocket JSON")?;
                    if value.get("id").and_then(Value::as_str) == Some(internal_request_id.as_str()) {
                        if let Some(error) = value.get("error") {
                            bail!("environment/add failed: {error}");
                        }
                        server_write
                            .send(Message::Text(
                                serde_json::to_string(&environment_status_request(
                                    &internal_status_id,
                                    profile,
                                ))?
                                .into(),
                            ))
                            .await?;
                        environment_status_attempts = 1;
                        continue;
                    }
                    if value.get("id").and_then(Value::as_str) == Some(internal_status_id.as_str()) {
                        if environment_status_ready(&value)? {
                            environment_ready = true;
                            if initialize_response_sent {
                                while let Some((pending, route)) = pending_client_messages.pop_front() {
                                    pending_client_bytes = pending_client_bytes.saturating_sub(pending.len());
                                    forward_client_message(
                                        pending,
                                        route,
                                        &mut server_write,
                                        &mut host_write,
                                        &mut pending_thread_metadata,
                                        &mut metadata_sequence,
                                        profile,
                                    ).await?;
                                }
                            }
                        } else {
                            environment_status_attempts += 1;
                            if environment_status_attempts > 200 {
                                bail!("execution environment did not become ready within 10 seconds");
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            server_write
                                .send(Message::Text(
                                    serde_json::to_string(&environment_status_request(
                                        &internal_status_id,
                                        profile,
                                    ))?
                                    .into(),
                                ))
                                .await?;
                        }
                        continue;
                    }
                    let is_initialize_response = !environment_request_sent
                        && initialize_id.as_ref().is_some_and(|id| value.get("id") == Some(id));
                    if is_initialize_response {
                        if let Some(error) = value.get("error") {
                            bail!("app-server initialize failed: {error}");
                        }
                        local_initialize_response = Some(value);
                        let add_environment = json!({
                            "id": internal_request_id,
                            "method": "environment/add",
                            "params": {
                                "environmentId": profile.environment_id,
                                "execServerUrl": format!("ws://127.0.0.1:{}", profile.local_exec_port),
                                "connectTimeoutMs": 10_000
                            }
                        });
                        server_write.send(Message::Text(serde_json::to_string(&add_environment)?.into())).await?;
                        environment_request_sent = true;
                    } else if let Some(metadata_id) = pending_thread_metadata
                        .iter()
                        .find(|(_, pending)| {
                            pending.local_sent && value.get("id") == Some(&pending.client_id)
                        })
                        .map(|(metadata_id, _)| metadata_id.clone())
                    {
                        let pending = pending_thread_metadata
                            .remove(&metadata_id)
                            .context("pending project metadata disappeared")?;
                        let host_response = pending.host_response;
                        if let Some(host_response) = host_response {
                            let merged = merge_thread_metadata(value, &host_response)?;
                            client_write
                                .send(Message::Text(serde_json::to_string(&merged)?.into()))
                                .await?;
                        } else {
                            client_write
                                .send(Message::Text(serde_json::to_string(&value)?.into()))
                                .await?;
                        }
                    } else {
                        client_write.send(message).await?;
                    }
                } else {
                    client_write.send(message).await?;
                }
            }
            line = read_bounded_line(&mut host_read, MAX_JSON_LINE_BYTES) => {
                let line = line?.context("remote Host App Server disconnected")?;
                let value: Value = serde_json::from_slice(&line)
                    .context("remote Host App Server emitted invalid JSONL")?;
                if value.get("id").and_then(Value::as_str) == Some(host_initialize_id.as_str()) {
                    if let Some(error) = value.get("error") {
                        bail!("remote Host App Server initialize failed: {error}");
                    }
                    host_initialize_response = Some(value);
                } else if let Some(metadata_id) = value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    && pending_thread_metadata.contains_key(&metadata_id)
                {
                    if value.get("error").is_some() {
                        send_local_thread_start(&metadata_id, &mut pending_thread_metadata, &mut server_write).await?;
                    } else if pending_thread_metadata
                        .get(&metadata_id)
                        .is_some_and(|pending| pending.local_sent)
                    {
                        pending_thread_metadata
                            .get_mut(&metadata_id)
                            .context("pending project metadata disappeared")?
                            .host_response = Some(value);
                    } else {
                        let instruction_paths = instruction_source_paths(&value);
                        let pending = pending_thread_metadata
                            .get_mut(&metadata_id)
                            .context("pending project metadata disappeared")?;
                        pending.host_response = Some(value);
                        pending
                            .instruction_paths
                            .clone_from(&instruction_paths);
                        if instruction_paths.is_empty() {
                            send_local_thread_start(&metadata_id, &mut pending_thread_metadata, &mut server_write).await?;
                        } else {
                            for (index, path) in instruction_paths.into_iter().enumerate() {
                                let read_id = format!("{metadata_id}/instruction/{index}");
                                pending_instruction_read(
                                    &metadata_id,
                                    &read_id,
                                    path.clone(),
                                    &mut pending_thread_metadata,
                                )?;
                                write_line(
                                    &mut host_write,
                                    &serde_json::to_vec(&json!({
                                        "id": read_id,
                                        "method": "fs/readFile",
                                        "params": {"path": path}
                                    }))?,
                                ).await?;
                            }
                        }
                    }
                } else if let Some(read_id) = value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    && let Some(metadata_id) = pending_thread_metadata
                        .iter()
                        .find(|(_, pending)| pending.pending_instruction_reads.contains_key(&read_id))
                        .map(|(metadata_id, _)| metadata_id.clone())
                {
                    record_instruction_read(
                        &metadata_id,
                        &read_id,
                        &value,
                        &mut pending_thread_metadata,
                    )?;
                    if pending_thread_metadata
                        .get(&metadata_id)
                        .is_some_and(|pending| pending.pending_instruction_reads.is_empty())
                    {
                        send_local_thread_start(
                            &metadata_id,
                            &mut pending_thread_metadata,
                            &mut server_write,
                        ).await?;
                    }
                } else if host_message_allowed(&value, profile) {
                    client_write
                        .send(Message::Text(String::from_utf8(line)?.into()))
                        .await?;
                }
            }
            () = tokio::time::sleep(Duration::from_millis(50)), if pending_thread_metadata
                .values()
                .any(|pending| !pending.local_sent) => {
                let expired = pending_thread_metadata
                    .iter()
                    .filter(|(_, pending)| {
                        !pending.local_sent && pending.started_at.elapsed() >= PROJECT_METADATA_TIMEOUT
                    })
                    .map(|(metadata_id, _)| metadata_id.clone())
                    .collect::<Vec<_>>();
                for metadata_id in expired {
                    send_local_thread_start(
                        &metadata_id,
                        &mut pending_thread_metadata,
                        &mut server_write,
                    ).await?;
                }
            }
        }

        if !initialize_response_sent
            && local_initialize_response.is_some()
            && host_initialize_response.is_some()
        {
            let local = local_initialize_response
                .take()
                .context("local initialize response disappeared")?;
            let host = host_initialize_response
                .as_ref()
                .context("host initialize response disappeared")?;
            let merged = merge_initialize_response(local, host, profile.remote_home.as_deref())?;
            client_write
                .send(Message::Text(serde_json::to_string(&merged)?.into()))
                .await?;
            initialize_response_sent = true;
        }

        if environment_ready && initialize_response_sent {
            while let Some((pending, route)) = pending_client_messages.pop_front() {
                pending_client_bytes = pending_client_bytes.saturating_sub(pending.len());
                forward_client_message(
                    pending,
                    route,
                    &mut server_write,
                    &mut host_write,
                    &mut pending_thread_metadata,
                    &mut metadata_sequence,
                    profile,
                )
                .await?;
            }
        }
    }
}

fn request_route(method: Option<&str>) -> RequestRoute {
    match method {
        Some("thread/start") => RequestRoute::ThreadStart,
        Some(method) if routing::is_host_request(method) => RequestRoute::Host,
        _ => RequestRoute::Local,
    }
}

async fn forward_client_message<W, S>(
    message: String,
    route: RequestRoute,
    server: &mut S,
    host: &mut W,
    pending_metadata: &mut HashMap<String, PendingThreadMetadata>,
    metadata_sequence: &mut u64,
    profile: &Profile,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    match route {
        RequestRoute::Local => server.send(Message::Text(message.into())).await?,
        RequestRoute::Host => write_line(host, message.as_bytes()).await?,
        RequestRoute::ThreadStart => {
            let local: Value = serde_json::from_str(&message)?;
            let client_id = local
                .get("id")
                .context("thread/start did not contain an id")?
                .clone();
            *metadata_sequence = metadata_sequence.saturating_add(1);
            let metadata_id = format!(
                "cxs/{}/thread-metadata/{}",
                profile.environment_id, metadata_sequence
            );
            let mut shadow = local.clone();
            shadow["id"] = Value::String(metadata_id.clone());
            let params = object_field(&mut shadow, "params")?;
            params.remove("environments");
            params.insert("ephemeral".to_owned(), Value::Bool(true));
            write_line(host, &serde_json::to_vec(&shadow)?).await?;
            pending_metadata.insert(
                metadata_id,
                PendingThreadMetadata {
                    client_id,
                    local_request: local,
                    host_response: None,
                    pending_instruction_reads: HashMap::new(),
                    instruction_paths: Vec::new(),
                    instruction_contents: HashMap::new(),
                    started_at: tokio::time::Instant::now(),
                    local_sent: false,
                },
            );
        }
    }
    Ok(())
}

fn instruction_source_paths(host_response: &Value) -> Vec<String> {
    host_response
        .pointer("/result/instructionSources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(MAX_INSTRUCTION_SOURCES)
        .map(str::to_owned)
        .collect()
}

fn pending_instruction_read(
    metadata_id: &str,
    read_id: &str,
    path: String,
    pending_metadata: &mut HashMap<String, PendingThreadMetadata>,
) -> Result<()> {
    pending_metadata
        .get_mut(metadata_id)
        .context("pending project metadata disappeared")?
        .pending_instruction_reads
        .insert(read_id.to_owned(), path);
    Ok(())
}

fn record_instruction_read(
    metadata_id: &str,
    read_id: &str,
    response: &Value,
    pending_metadata: &mut HashMap<String, PendingThreadMetadata>,
) -> Result<()> {
    let pending = pending_metadata
        .get_mut(metadata_id)
        .context("pending project metadata disappeared")?;
    let path = pending
        .pending_instruction_reads
        .remove(read_id)
        .context("pending instruction read disappeared")?;
    let Some(encoded) = response
        .pointer("/result/dataBase64")
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let bytes = BASE64
        .decode(encoded)
        .with_context(|| format!("Host App Server returned invalid base64 for {path}"))?;
    if bytes.len() > MAX_INSTRUCTION_SOURCE_BYTES {
        bail!("remote instruction source is too large: {path}");
    }
    let total_bytes = pending
        .instruction_contents
        .values()
        .map(String::len)
        .sum::<usize>()
        .saturating_add(bytes.len());
    if total_bytes > MAX_INSTRUCTION_TOTAL_BYTES {
        bail!("remote instruction sources exceed the aggregate size limit");
    }
    let contents = String::from_utf8(bytes)
        .with_context(|| format!("remote instruction source is not UTF-8: {path}"))?;
    pending.instruction_contents.insert(path, contents);
    Ok(())
}

async fn send_local_thread_start<S>(
    metadata_id: &str,
    pending_metadata: &mut HashMap<String, PendingThreadMetadata>,
    server: &mut S,
) -> Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let pending = pending_metadata
        .get_mut(metadata_id)
        .context("pending project metadata disappeared")?;
    if pending.local_sent {
        return Ok(());
    }
    let mut local = pending.local_request.clone();
    let instructions = pending
        .instruction_paths
        .iter()
        .filter_map(|path| {
            pending
                .instruction_contents
                .get(path)
                .map(|contents| (path, contents))
        })
        .map(|(path, contents)| format!("Instructions from {path}:\n{contents}"))
        .collect::<Vec<_>>();
    if !instructions.is_empty() {
        let params = object_field(&mut local, "params")?;
        let existing = params
            .get("developerInstructions")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let remote = instructions.join("\n\n");
        let combined = if existing.is_empty() {
            remote
        } else {
            format!("{existing}\n\n{remote}")
        };
        params.insert("developerInstructions".to_owned(), Value::String(combined));
    }
    server
        .send(Message::Text(serde_json::to_string(&local)?.into()))
        .await?;
    pending.local_sent = true;
    Ok(())
}

fn merge_thread_metadata(mut local: Value, host: &Value) -> Result<Value> {
    if local.get("error").is_some() || host.get("error").is_some() {
        return Ok(local);
    }
    let local_result = local
        .get_mut("result")
        .and_then(Value::as_object_mut)
        .context("Mac thread/start response did not contain a result object")?;
    let host_result = host
        .get("result")
        .and_then(Value::as_object)
        .context("Host thread/start response did not contain a result object")?;
    for field in ["cwd", "runtimeWorkspaceRoots", "instructionSources"] {
        if let Some(value) = host_result.get(field) {
            local_result.insert(field.to_owned(), value.clone());
        }
    }
    if let Some(host_thread) = host_result.get("thread").and_then(Value::as_object)
        && let Some(local_thread) = local_result
            .get_mut("thread")
            .and_then(Value::as_object_mut)
    {
        for field in ["cwd", "gitInfo"] {
            if let Some(value) = host_thread.get(field) {
                local_thread.insert(field.to_owned(), value.clone());
            }
        }
    }
    Ok(local)
}

fn host_message_allowed(message: &Value, profile: &Profile) -> bool {
    let internal_prefix = format!("cxs/{}/thread-metadata/", profile.environment_id);
    let internal_initialize = format!("cxs/{}/host-initialize", profile.environment_id);
    message.get("id").is_some_and(|id| {
        !id.as_str()
            .is_some_and(|id| id == internal_initialize || id.starts_with(&internal_prefix))
    }) || message
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(routing::is_host_notification)
}

fn merge_initialize_response(
    mut local: Value,
    host: &Value,
    remote_home: Option<&str>,
) -> Result<Value> {
    if let Some(error) = host.get("error") {
        bail!("remote Host App Server initialize failed: {error}");
    }
    let local_result = local
        .get_mut("result")
        .and_then(Value::as_object_mut)
        .context("local App Server initialize response did not contain a result object")?;
    let host_result = host
        .get("result")
        .and_then(Value::as_object)
        .context("remote Host App Server initialize response did not contain a result object")?;
    for &field in routing::REQUIRED_INITIALIZE_FIELDS {
        if let Some(value) = host_result.get(field) {
            local_result.insert(field.to_owned(), value.clone());
        }
    }
    if let Some(remote_home) = remote_home {
        local_result.insert(
            "codexHome".to_owned(),
            Value::String(format!("{}/.codex", remote_home.trim_end_matches('/'))),
        );
    }
    Ok(local)
}

fn app_server_arguments(arguments: &[String]) -> Result<Vec<String>> {
    let mut forwarded = vec!["app-server".to_owned()];
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--stdio" => index += 1,
            "--listen" => {
                if index + 1 >= arguments.len() {
                    bail!("app-server --listen did not include a value");
                }
                index += 2;
            }
            "-c" | "--config" | "--enable" | "--disable" | "--code-mode-host" => {
                if index + 1 >= arguments.len() {
                    bail!("app-server argument '{argument}' did not include a value");
                }
                forwarded.push(argument.clone());
                forwarded.push(arguments[index + 1].clone());
                index += 2;
            }
            "--strict-config" | "--analytics-default-enabled" => {
                forwarded.push(argument.clone());
                index += 1;
            }
            value
                if value.starts_with("--listen=")
                    || value.starts_with("--ws-")
                    || value == "daemon"
                    || value == "proxy"
                    || value == "generate-ts"
                    || value == "generate-json-schema"
                    || value == "help"
                    || value == "--help"
                    || value == "-h" =>
            {
                if value.starts_with("--listen=") {
                    index += 1;
                } else {
                    bail!("unsupported app-server argument '{value}' from the remote client");
                }
            }
            value
                if value.starts_with("--config=")
                    || value.starts_with("--enable=")
                    || value.starts_with("--disable=")
                    || value.starts_with("--code-mode-host=") =>
            {
                forwarded.push(argument.clone());
                index += 1;
            }
            value => bail!("unsupported app-server argument '{value}' from the remote client"),
        }
    }
    Ok(forwarded)
}

async fn relay_jsonl<CR, CW, SR, SW>(
    client_read: &mut CR,
    client_write: &mut CW,
    server_read: &mut SR,
    server_write: &mut SW,
    profile: &Profile,
) -> Result<()>
where
    CR: AsyncBufRead + Unpin,
    CW: AsyncWrite + Unpin,
    SR: AsyncBufRead + Unpin,
    SW: AsyncWrite + Unpin,
{
    let internal_request_id = format!("cxs/{}/environment-add", profile.environment_id);
    let internal_status_id = format!("cxs/{}/environment-status", profile.environment_id);
    let mut initialize_id: Option<Value> = None;
    let mut environment_request_sent = false;
    let mut environment_ready = false;
    let mut environment_status_attempts = 0_u16;
    let mut pending_client_lines: VecDeque<Vec<u8>> = VecDeque::new();
    let mut pending_client_bytes = 0_usize;

    loop {
        tokio::select! {
            line = read_bounded_line(client_read, MAX_JSON_LINE_BYTES) => {
                let Some(line) = line? else { return Ok(()); };
                let rewritten = rewrite_client_message(&line, profile, &mut initialize_id)?;
                let is_initialize = serde_json::from_slice::<Value>(&rewritten)?
                    .get("method")
                    .and_then(Value::as_str)
                    == Some("initialize");
                if initialize_id.is_some() && !environment_ready && !is_initialize {
                    pending_client_bytes = pending_client_bytes.saturating_add(rewritten.len());
                    if pending_client_bytes > MAX_PENDING_CLIENT_BYTES {
                        bail!("client sent too much data before the execution environment became ready");
                    }
                    pending_client_lines.push_back(rewritten);
                } else {
                    write_line(server_write, &rewritten).await?;
                }
            }
            line = read_bounded_line(server_read, MAX_JSON_LINE_BYTES) => {
                let Some(line) = line? else { return Ok(()); };
                let message: Value = serde_json::from_slice(&line)
                    .context("app-server emitted invalid JSONL")?;
                if message.get("id").and_then(Value::as_str) == Some(internal_request_id.as_str()) {
                    if let Some(error) = message.get("error") {
                        bail!("environment/add failed: {error}");
                    }
                    write_line(
                        server_write,
                        &serde_json::to_vec(&environment_status_request(
                            &internal_status_id,
                            profile,
                        ))?,
                    )
                    .await?;
                    environment_status_attempts = 1;
                    continue;
                }
                if message.get("id").and_then(Value::as_str) == Some(internal_status_id.as_str()) {
                    if environment_status_ready(&message)? {
                        environment_ready = true;
                        while let Some(pending) = pending_client_lines.pop_front() {
                            pending_client_bytes = pending_client_bytes.saturating_sub(pending.len());
                            write_line(server_write, &pending).await?;
                        }
                    } else {
                        environment_status_attempts += 1;
                        if environment_status_attempts > 200 {
                            bail!("execution environment did not become ready within 10 seconds");
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        write_line(
                            server_write,
                            &serde_json::to_vec(&environment_status_request(
                                &internal_status_id,
                                profile,
                            ))?,
                        )
                        .await?;
                    }
                    continue;
                }

                let is_initialize_response = !environment_request_sent
                    && initialize_id.as_ref().is_some_and(|id| message.get("id") == Some(id));
                write_line(client_write, &line).await?;
                if is_initialize_response {
                    if let Some(error) = message.get("error") {
                        bail!("app-server initialize failed: {error}");
                    }
                    let add_environment = json!({
                        "id": internal_request_id,
                        "method": "environment/add",
                        "params": {
                            "environmentId": profile.environment_id,
                            "execServerUrl": format!("ws://127.0.0.1:{}", profile.local_exec_port),
                            "connectTimeoutMs": 10_000
                        }
                    });
                    write_line(server_write, &serde_json::to_vec(&add_environment)?).await?;
                    environment_request_sent = true;
                }
            }
        }
    }
}

fn environment_status_request(id: &str, profile: &Profile) -> Value {
    json!({
        "id": id,
        "method": "environment/status",
        "params": {
            "environmentId": profile.environment_id
        }
    })
}

fn environment_status_ready(message: &Value) -> Result<bool> {
    if let Some(error) = message.get("error") {
        bail!("environment/status failed: {error}");
    }
    let status = message
        .pointer("/result/status")
        .and_then(Value::as_str)
        .context("environment/status response did not contain result.status")?;
    Ok(status == "ready")
}

fn rewrite_client_message(
    line: &[u8],
    profile: &Profile,
    initialize_id: &mut Option<Value>,
) -> Result<Vec<u8>> {
    let mut message: Value =
        serde_json::from_slice(line).context("client sent invalid App Server JSONL")?;
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match method.as_deref() {
        Some("initialize") => {
            *initialize_id = Some(
                message
                    .get("id")
                    .context("initialize request did not contain an id")?
                    .clone(),
            );
            let params = object_field(&mut message, "params")?;
            let capabilities = object_field_in(params, "capabilities")?;
            capabilities.insert("experimentalApi".to_owned(), Value::Bool(true));
        }
        Some("thread/start") => {
            let params = object_field(&mut message, "params")?;
            let cwd = params
                .get("cwd")
                .and_then(Value::as_str)
                .context("thread/start must contain a remote cwd")?
                .to_owned();
            set_environment(params, profile, &cwd);
        }
        Some("turn/start") => {
            let params = object_field(&mut message, "params")?;
            if let Some(cwd) = params.get("cwd").and_then(Value::as_str).map(str::to_owned) {
                set_environment(params, profile, &cwd);
            } else {
                params.remove("environments");
            }
        }
        _ => {}
    }
    serde_json::to_vec(&message).context("could not encode rewritten App Server message")
}

fn set_environment(params: &mut Map<String, Value>, profile: &Profile, cwd: &str) {
    params.insert(
        "environments".to_owned(),
        json!([{
            "environmentId": profile.environment_id,
            "cwd": cwd,
            "runtimeWorkspaceRoots": [cwd]
        }]),
    );
}

fn object_field<'a>(value: &'a mut Value, field: &str) -> Result<&'a mut Map<String, Value>> {
    let object = value
        .as_object_mut()
        .context("App Server message was not a JSON object")?;
    object_field_in(object, field)
}

fn object_field_in<'a>(
    object: &'a mut Map<String, Value>,
    field: &str,
) -> Result<&'a mut Map<String, Value>> {
    let value = object
        .entry(field.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if value.is_null() {
        *value = Value::Object(Map::new());
    }
    value
        .as_object_mut()
        .with_context(|| format!("App Server field '{field}' was not an object"))
}

async fn read_bounded_line<R>(reader: &mut R, maximum: usize) -> Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(buffer.len(), |position| position + 1);
        if line.len().saturating_add(take) > maximum {
            bail!("JSONL message exceeded {maximum} bytes");
        }
        line.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if newline.is_some() {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

async fn write_line<W>(writer: &mut W, line: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(line).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn read_agent_handshake<R>(reader: &mut R) -> Result<AgentHandshake>
where
    R: AsyncBufRead + Unpin,
{
    let bytes = read_bounded_line(reader, MAX_HANDSHAKE_BYTES)
        .await?
        .context("remote agent disconnected during handshake")?;
    serde_json::from_slice(&bytes).context("remote agent sent an invalid handshake")
}

fn validate_agent_handshake(
    handshake: &AgentHandshake,
    profile: &Profile,
    expected_token: &str,
) -> Result<()> {
    if handshake.magic != AGENT_MAGIC || handshake.protocol_version != AGENT_PROTOCOL_VERSION {
        bail!("remote agent protocol version mismatch");
    }
    if handshake.profile != profile.name {
        bail!("remote agent belongs to the wrong profile");
    }
    if handshake.token.len() != expected_token.len()
        || handshake
            .token
            .as_bytes()
            .ct_eq(expected_token.as_bytes())
            .unwrap_u8()
            != 1
    {
        bail!("remote agent authentication failed");
    }
    Ok(())
}

async fn read_handshake<R>(reader: &mut R) -> Result<ShimHandshake>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(512);
    loop {
        if bytes.len() >= MAX_HANDSHAKE_BYTES {
            bail!("shim handshake exceeded {MAX_HANDSHAKE_BYTES} bytes");
        }
        let byte = reader
            .read_u8()
            .await
            .context("shim disconnected during handshake")?;
        if byte == b'\n' {
            break;
        }
        bytes.push(byte);
    }
    serde_json::from_slice(&bytes).context("shim sent an invalid handshake")
}

fn validate_handshake(
    handshake: &ShimHandshake,
    profile: &Profile,
    expected_token: &str,
) -> Result<()> {
    if handshake.magic != SHIM_MAGIC || handshake.protocol_version != SHIM_PROTOCOL_VERSION {
        bail!("shim protocol version mismatch");
    }
    if handshake.profile != profile.name {
        bail!("shim requested the wrong profile");
    }
    if handshake.token.len() != expected_token.len()
        || handshake
            .token
            .as_bytes()
            .ct_eq(expected_token.as_bytes())
            .unwrap_u8()
            != 1
    {
        bail!("shim authentication failed");
    }
    Ok(())
}

async fn stop_child(child: &mut Child, pid: Option<Pid>) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Some(pid) = pid {
        let _ = killpg(pid, Signal::SIGTERM);
    } else {
        let _ = child.start_kill();
    }
    if timeout(CHILD_EXIT_GRACE, child.wait()).await.is_err() {
        if let Some(pid) = pid {
            let _ = killpg(pid, Signal::SIGKILL);
        }
        let _ = child.kill().await;
    }
}

fn prepare_socket(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        bail!("bridge socket path has no parent directory");
    };
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        bail!("refusing to replace non-socket path {}", path.display());
    }
    fs::remove_file(path)
        .with_context(|| format!("could not remove stale socket {}", path.display()))
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use cxs_core::{PROFILE_SCHEMA_VERSION, ProfileStatus};
    use tokio::io::AsyncWriteExt;

    use super::*;

    fn profile() -> Profile {
        Profile {
            schema_version: PROFILE_SCHEMA_VERSION,
            name: "gpu".to_owned(),
            source_host: "gpu-server".to_owned(),
            app_alias: "cxs-gpu".to_owned(),
            status: ProfileStatus::Prepared,
            codex_version: "codex-cli 0.147.0".to_owned(),
            local_socket: PathBuf::from("/tmp/cxs-test.sock"),
            remote_socket: "/tmp/cxs-remote.sock".to_owned(),
            environment_id: "cxs-gpu".to_owned(),
            local_exec_port: 49_444,
            remote_exec_port: 49_445,
            token_file: PathBuf::from("/tmp/cxs-token"),
            codex_home: PathBuf::from("/tmp/codex"),
            installed_target: None,
            remote_home: None,
            remote_release: None,
            package_sha256: None,
            shim_sha256: None,
            executor_source: None,
            executor_path: None,
        }
    }

    #[tokio::test]
    async fn reads_and_validates_handshake() -> Result<()> {
        let (mut writer, mut reader) = tokio::io::duplex(2048);
        let handshake = ShimHandshake {
            magic: SHIM_MAGIC.to_owned(),
            protocol_version: SHIM_PROTOCOL_VERSION,
            profile: "gpu".to_owned(),
            token: "a".repeat(64),
            app_server_args: Vec::new(),
            transport: ShimTransport::Jsonl,
        };
        writer
            .write_all(serde_json::to_string(&handshake)?.as_bytes())
            .await?;
        writer.write_all(b"\nremaining").await?;
        let decoded = read_handshake(&mut reader).await?;
        validate_handshake(&decoded, &profile(), &"a".repeat(64))?;
        let mut remaining = [0_u8; 9];
        reader.read_exact(&mut remaining).await?;
        assert_eq!(&remaining, b"remaining");
        Ok(())
    }

    #[test]
    fn enables_experimental_api_and_remote_thread_environment() -> Result<()> {
        let profile = profile();
        let mut initialize_id = None;
        let initialize = rewrite_client_message(
            br#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"app","version":"1"}}}"#,
            &profile,
            &mut initialize_id,
        )?;
        let initialize: Value = serde_json::from_slice(&initialize)?;
        assert_eq!(
            initialize.pointer("/params/capabilities/experimentalApi"),
            Some(&Value::Bool(true))
        );
        assert_eq!(initialize_id, Some(json!(1)));

        let thread = rewrite_client_message(
            br#"{"id":2,"method":"thread/start","params":{"cwd":"/srv/project"}}"#,
            &profile,
            &mut initialize_id,
        )?;
        let thread: Value = serde_json::from_slice(&thread)?;
        assert_eq!(
            thread
                .pointer("/params/environments/0/environmentId")
                .and_then(Value::as_str),
            Some("cxs-gpu")
        );
        assert_eq!(
            thread
                .pointer("/params/environments/0/cwd")
                .and_then(Value::as_str),
            Some("/srv/project")
        );
        Ok(())
    }

    #[test]
    fn forwards_client_probe_ids_but_hides_internal_host_ids() {
        let profile = profile();
        assert!(host_message_allowed(
            &json!({"id": "cxs/probe/read-directory", "result": {}}),
            &profile,
        ));
        assert!(!host_message_allowed(
            &json!({"id": "cxs/cxs-gpu/host-initialize", "result": {}}),
            &profile,
        ));
        assert!(!host_message_allowed(
            &json!({"id": "cxs/cxs-gpu/thread-metadata/7", "result": {}}),
            &profile,
        ));
    }

    #[test]
    fn filters_transport_arguments_and_preserves_safe_options() -> Result<()> {
        let arguments = vec![
            "--listen".to_owned(),
            "stdio://".to_owned(),
            "--analytics-default-enabled".to_owned(),
            "--enable".to_owned(),
            "example".to_owned(),
        ];
        assert_eq!(
            app_server_arguments(&arguments)?,
            [
                "app-server",
                "--analytics-default-enabled",
                "--enable",
                "example"
            ]
        );
        assert!(app_server_arguments(&["proxy".to_owned()]).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn registers_environment_before_forwarding_pipelined_thread_start() -> Result<()> {
        let (app_side, relay_client_side) = tokio::io::duplex(16 * 1024);
        let (relay_server_side, server_side) = tokio::io::duplex(16 * 1024);
        let (app_read, mut app_write) = tokio::io::split(app_side);
        let (relay_client_read, mut relay_client_write) = tokio::io::split(relay_client_side);
        let (relay_server_read, mut relay_server_write) = tokio::io::split(relay_server_side);
        let (server_read, mut server_write) = tokio::io::split(server_side);
        let mut app_read = BufReader::new(app_read);
        let mut relay_client_read = BufReader::new(relay_client_read);
        let mut relay_server_read = BufReader::new(relay_server_read);
        let mut server_read = BufReader::new(server_read);
        let profile = profile();

        let relay = tokio::spawn(async move {
            relay_jsonl(
                &mut relay_client_read,
                &mut relay_client_write,
                &mut relay_server_read,
                &mut relay_server_write,
                &profile,
            )
            .await
        });

        write_line(
            &mut app_write,
            br#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"app","version":"1"}}}"#,
        )
        .await?;
        write_line(
            &mut app_write,
            br#"{"id":2,"method":"thread/start","params":{"cwd":"/srv/project"}}"#,
        )
        .await?;

        let initialize = read_bounded_line(&mut server_read, MAX_JSON_LINE_BYTES)
            .await?
            .context("server did not receive initialize")?;
        let initialize: Value = serde_json::from_slice(&initialize)?;
        assert_eq!(
            initialize.pointer("/params/capabilities/experimentalApi"),
            Some(&Value::Bool(true))
        );
        write_line(&mut server_write, br#"{"id":1,"result":{}}"#).await?;

        let response = read_bounded_line(&mut app_read, MAX_JSON_LINE_BYTES)
            .await?
            .context("app did not receive initialize response")?;
        assert_eq!(serde_json::from_slice::<Value>(&response)?["id"], json!(1));

        let add = read_bounded_line(&mut server_read, MAX_JSON_LINE_BYTES)
            .await?
            .context("server did not receive environment/add")?;
        let add: Value = serde_json::from_slice(&add)?;
        assert_eq!(add["method"], json!("environment/add"));
        assert_eq!(
            add["params"]["execServerUrl"],
            json!("ws://127.0.0.1:49444")
        );
        let internal_id = add["id"].clone();
        write_line(
            &mut server_write,
            &serde_json::to_vec(&json!({"id": internal_id, "result": {}}))?,
        )
        .await?;

        let status = read_bounded_line(&mut server_read, MAX_JSON_LINE_BYTES)
            .await?
            .context("server did not receive environment/status")?;
        let status: Value = serde_json::from_slice(&status)?;
        assert_eq!(status["method"], json!("environment/status"));
        let status_id = status["id"].clone();
        write_line(
            &mut server_write,
            &serde_json::to_vec(&json!({
                "id": status_id,
                "result": {"status": "ready"}
            }))?,
        )
        .await?;

        let thread = read_bounded_line(&mut server_read, MAX_JSON_LINE_BYTES)
            .await?
            .context("server did not receive queued thread/start")?;
        let thread: Value = serde_json::from_slice(&thread)?;
        assert_eq!(
            thread["params"]["environments"][0]["environmentId"],
            json!("cxs-gpu")
        );

        app_write.shutdown().await?;
        server_write.shutdown().await?;
        drop(app_read);
        drop(server_read);
        timeout(Duration::from_secs(2), relay)
            .await
            .context("relay did not stop after both peers closed")???;
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn registers_environment_over_websocket_transport() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let app_path = directory.path().join("app.sock");
        let server_path = directory.path().join("server.sock");
        let app_listener = UnixListener::bind(&app_path)?;
        let server_listener = UnixListener::bind(&server_path)?;

        let app_connect = async {
            let stream = UnixStream::connect(&app_path).await?;
            tokio_tungstenite::client_async("ws://codex-app-server/rpc", stream).await
        };
        let bridge_accept = async {
            let (stream, _) = app_listener.accept().await?;
            tokio_tungstenite::accept_async(stream).await
        };
        let (app_result, bridge_client_result) = tokio::join!(app_connect, bridge_accept);
        let (mut app, _) = app_result?;
        let bridge_client = bridge_client_result?;

        let bridge_connect = async {
            let stream = UnixStream::connect(&server_path).await?;
            tokio_tungstenite::client_async("ws://codex-app-server/rpc", stream).await
        };
        let mock_accept = async {
            let (stream, _) = server_listener.accept().await?;
            tokio_tungstenite::accept_async(stream).await
        };
        let (bridge_server_result, mock_server_result) = tokio::join!(bridge_connect, mock_accept);
        let (bridge_server, _) = bridge_server_result?;
        let mut mock_server = mock_server_result?;
        let (relay_host, mock_host) = tokio::io::duplex(64 * 1024);
        let (mock_host_read, mut mock_host_write) = tokio::io::split(mock_host);
        let mut mock_host_read = BufReader::new(mock_host_read);
        let profile = profile();
        let relay_profile = profile.clone();
        let relay = tokio::spawn(async move {
            relay_websocket(bridge_client, bridge_server, relay_host, &relay_profile).await
        });

        app.send(Message::Text(
            r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"app","version":"1"}}}"#
                .into(),
        ))
        .await?;
        app.send(Message::Text(
            r#"{"id":2,"method":"environment/status","params":{"environmentId":"cxs-gpu"}}"#.into(),
        ))
        .await?;

        let initialize = mock_server.next().await.context("missing initialize")??;
        let initialize: Value = serde_json::from_str(initialize.to_text()?)?;
        assert_eq!(
            initialize.pointer("/params/capabilities/experimentalApi"),
            Some(&Value::Bool(true))
        );
        let host_initialize = read_bounded_line(&mut mock_host_read, MAX_JSON_LINE_BYTES)
            .await?
            .context("missing Host App Server initialize")?;
        let host_initialize: Value = serde_json::from_slice(&host_initialize)?;
        assert_eq!(host_initialize["method"], "initialize");
        assert_eq!(
            host_initialize.pointer("/params/capabilities/experimentalApi"),
            Some(&Value::Bool(true))
        );
        let host_initialize_id = host_initialize["id"].clone();
        write_line(
            &mut mock_host_write,
            &serde_json::to_vec(&json!({
                "id": host_initialize_id,
                "result": {
                    "userAgent": "codex/linux",
                    "codexHome": "/home/test/.codex",
                    "platformFamily": "unix",
                    "platformOs": "linux"
                }
            }))?,
        )
        .await?;
        mock_server
            .send(Message::Text(
                r#"{"id":1,"result":{"userAgent":"codex/macos","codexHome":"/Users/test/.codex","platformFamily":"unix","platformOs":"macos"}}"#.into(),
            ))
            .await?;
        let add = mock_server
            .next()
            .await
            .context("missing environment/add")??;
        let add: Value = serde_json::from_str(add.to_text()?)?;
        assert_eq!(add["method"], "environment/add");
        let add_id = add["id"].clone();
        mock_server
            .send(Message::Text(
                serde_json::to_string(&json!({"id": add_id, "result": {}}))?.into(),
            ))
            .await?;
        let internal_status = mock_server
            .next()
            .await
            .context("missing internal environment/status")??;
        let internal_status: Value = serde_json::from_str(internal_status.to_text()?)?;
        assert_eq!(internal_status["method"], "environment/status");
        let internal_status_id = internal_status["id"].clone();
        mock_server
            .send(Message::Text(
                serde_json::to_string(&json!({
                    "id": internal_status_id,
                    "result": {"status": "ready"}
                }))?
                .into(),
            ))
            .await?;
        let status = mock_server
            .next()
            .await
            .context("missing queued status")??;
        let status: Value = serde_json::from_str(status.to_text()?)?;
        assert_eq!(status["id"], 2);
        mock_server
            .send(Message::Text(
                r#"{"id":2,"result":{"status":"ready"}}"#.into(),
            ))
            .await?;

        let first = app.next().await.context("missing initialize response")??;
        let second = app.next().await.context("missing status response")??;
        let first: Value = serde_json::from_str(first.to_text()?)?;
        assert_eq!(first["id"], 1);
        assert_eq!(first["result"]["platformOs"], "linux");
        assert_eq!(first["result"]["codexHome"], "/home/test/.codex");
        assert_eq!(serde_json::from_str::<Value>(second.to_text()?)?["id"], 2);

        app.send(Message::Text(
            r#"{"id":4,"method":"thread/start","params":{"cwd":"/home/test/project"}}"#.into(),
        ))
        .await?;
        let host_thread = read_bounded_line(&mut mock_host_read, MAX_JSON_LINE_BYTES)
            .await?
            .context("missing remote project metadata thread/start")?;
        let host_thread: Value = serde_json::from_slice(&host_thread)?;
        assert_eq!(host_thread["method"], "thread/start");
        assert_eq!(host_thread["params"]["ephemeral"], true);
        assert!(host_thread["params"].get("environments").is_none());
        let metadata_id = host_thread["id"].clone();
        write_line(
            &mut mock_host_write,
            &serde_json::to_vec(&json!({
                "id": metadata_id,
                "result": {
                    "cwd": "/home/test/project",
                    "runtimeWorkspaceRoots": ["/home/test/project"],
                    "instructionSources": ["/home/test/project/AGENTS.md"],
                    "thread": {
                        "id": "host-shadow-thread",
                        "cwd": "/home/test/project",
                        "gitInfo": {"branch": "main", "sha": "abc", "originUrl": null}
                    }
                }
            }))?,
        )
        .await?;
        let instruction_read = read_bounded_line(&mut mock_host_read, MAX_JSON_LINE_BYTES)
            .await?
            .context("missing remote instruction file read")?;
        let instruction_read: Value = serde_json::from_slice(&instruction_read)?;
        assert_eq!(instruction_read["method"], "fs/readFile");
        assert_eq!(
            instruction_read["params"]["path"],
            "/home/test/project/AGENTS.md"
        );
        write_line(
            &mut mock_host_write,
            &serde_json::to_vec(&json!({
                "id": instruction_read["id"],
                "result": {"dataBase64": BASE64.encode("Always run remote tests.")}
            }))?,
        )
        .await?;
        let local_thread = mock_server
            .next()
            .await
            .context("missing Mac thread/start")??;
        let local_thread: Value = serde_json::from_str(local_thread.to_text()?)?;
        assert_eq!(local_thread["id"], 4);
        assert_eq!(
            local_thread.pointer("/params/environments/0/environmentId"),
            Some(&Value::String("cxs-gpu".to_owned()))
        );
        assert!(
            local_thread["params"]["developerInstructions"]
                .as_str()
                .is_some_and(|instructions| {
                    instructions.contains("/home/test/project/AGENTS.md")
                        && instructions.contains("Always run remote tests.")
                })
        );
        mock_server
            .send(Message::Text(
                serde_json::to_string(&json!({
                    "id": 4,
                    "result": {
                        "cwd": "/home/test/project",
                        "instructionSources": [],
                        "thread": {"id": "mac-thread", "cwd": "/home/test/project", "gitInfo": null}
                    }
                }))?
                .into(),
            ))
            .await?;
        let thread_response = app
            .next()
            .await
            .context("missing merged thread response")??;
        let thread_response: Value = serde_json::from_str(thread_response.to_text()?)?;
        assert_eq!(thread_response["result"]["thread"]["id"], "mac-thread");
        assert_eq!(
            thread_response["result"]["thread"]["gitInfo"]["branch"],
            "main"
        );
        assert_eq!(
            thread_response["result"]["instructionSources"][0],
            "/home/test/project/AGENTS.md"
        );

        app.send(Message::Text(
            r#"{"id":3,"method":"fs/readDirectory","params":{"path":"/home/test"}}"#.into(),
        ))
        .await?;
        let host_fs = read_bounded_line(&mut mock_host_read, MAX_JSON_LINE_BYTES)
            .await?
            .context("missing routed fs/readDirectory")?;
        let host_fs: Value = serde_json::from_slice(&host_fs)?;
        assert_eq!(host_fs["method"], "fs/readDirectory");
        write_line(
            &mut mock_host_write,
            br#"{"id":3,"result":{"entries":[{"fileName":"project","isDirectory":true,"isFile":false}]}}"#,
        )
        .await?;
        let fs_response = app.next().await.context("missing fs response")??;
        let fs_response: Value = serde_json::from_str(fs_response.to_text()?)?;
        assert_eq!(fs_response["result"]["entries"][0]["fileName"], "project");
        app.close(None).await?;
        relay.await??;
        Ok(())
    }
}
