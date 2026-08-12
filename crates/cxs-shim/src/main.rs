use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cxs_core::{
    AGENT_MAGIC, AGENT_PROTOCOL_VERSION, AgentHandshake, SHIM_MAGIC, SHIM_PROTOCOL_VERSION,
    ShimHandshake, ShimTransport,
};
use cxs_mux::{ChannelKind, IncomingChannel};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone, Deserialize)]
struct ShimConfig {
    profile: String,
    codex_version: String,
    agent_socket: PathBuf,
    #[serde(default)]
    control_socket: Option<PathBuf>,
    token_file: PathBuf,
    exec_server: PathBuf,
    #[serde(default)]
    exec_server_args: Vec<String>,
    exec_server_port: u16,
    codex_home: PathBuf,
    #[serde(default)]
    original_codex: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let config = load_config()?;
    if matches!(arguments.as_slice(), [mode] if mode == "__cxs-agent") {
        return run_agent(&config, false).await;
    }
    if matches!(arguments.as_slice(), [mode, replace] if mode == "__cxs-agent" && replace == "--replace")
    {
        return run_agent(&config, true).await;
    }
    if matches!(arguments.as_slice(), [flag] if flag == "--version" || flag == "-V") {
        println!("{}", config.codex_version);
        return Ok(());
    }
    let Some(position) = arguments
        .iter()
        .position(|argument| argument == "app-server")
    else {
        return delegate_or_reject(&config, &arguments).await;
    };
    let mut app_arguments = arguments[..position].to_vec();
    app_arguments.extend_from_slice(&arguments[position + 1..]);
    match arguments.get(position + 1).map(String::as_str) {
        Some("proxy") => proxy_control_socket(&config).await,
        _ if requests_unix_listener(&arguments[position + 1..]) => {
            serve_control_socket(&config, &app_arguments).await
        }
        _ => relay_app_server(&config, &app_arguments).await,
    }
}

fn requests_unix_listener(arguments: &[String]) -> bool {
    arguments.windows(2).any(|pair| {
        pair[0] == "--listen" && (pair[1] == "unix://" || pair[1].starts_with("unix://"))
    }) || arguments
        .iter()
        .any(|argument| argument.starts_with("--listen=unix://"))
}

fn load_config() -> Result<ShimConfig> {
    let path = std::env::var_os("CXS_SHIM_CONFIG").map_or_else(
        || {
            dirs::home_dir()
                .context("could not determine remote home directory")
                .map(|home| home.join(".config/codex-shuttle/shim.json"))
        },
        |value| Ok(PathBuf::from(value)),
    )?;
    let bytes = fs::read(&path)
        .with_context(|| format!("could not read shim config {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("could not parse shim config {}", path.display()))
}

async fn relay_app_server(config: &ShimConfig, arguments: &[String]) -> Result<()> {
    let stream = open_bridge_session(config, arguments, ShimTransport::Jsonl).await?;

    let (mut bridge_read, mut bridge_write) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let relay_result = tokio::select! {
        result = tokio::io::copy(&mut stdin, &mut bridge_write) => {
            result.map(|_| ()).context("could not relay App input")
        }
        result = tokio::io::copy(&mut bridge_read, &mut stdout) => {
            result.map(|_| ()).context("could not relay App Server output")
        }
    };
    relay_result
}

async fn serve_control_socket(config: &ShimConfig, arguments: &[String]) -> Result<()> {
    let socket_path = app_server_control_socket(config);
    serve_control_socket_at(config, arguments, socket_path).await
}

async fn serve_control_socket_at(
    config: &ShimConfig,
    arguments: &[String],
    socket_path: PathBuf,
) -> Result<()> {
    if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
        return Ok(());
    }
    prepare_control_socket(&socket_path)?;
    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(error)
            if error.kind() == std::io::ErrorKind::AddrInUse
                && std::os::unix::net::UnixStream::connect(&socket_path).is_ok() =>
        {
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("could not bind {}", socket_path.display()));
        }
    };
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    let _guard = SocketGuard(socket_path);
    let (completed_tx, mut completed_rx) = tokio::sync::mpsc::channel::<()>(32);
    let mut active = 0_usize;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (client, _) = accepted?;
                active = active.saturating_add(1);
                let config = config.clone();
                let arguments = arguments.to_vec();
                let completed = completed_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = relay_control_client(&config, &arguments, client).await {
                        eprintln!("cxs-shim: App control client failed: {error:#}");
                    }
                    let _ = completed.send(()).await;
                });
            }
            completed = completed_rx.recv(), if active > 0 => {
                if completed.is_some() {
                    active = active.saturating_sub(1);
                }
            }
            () = tokio::time::sleep(Duration::from_secs(30)), if active == 0 => return Ok(()),
        }
    }
}

async fn relay_control_client(
    config: &ShimConfig,
    arguments: &[String],
    mut client: UnixStream,
) -> Result<()> {
    let mut bridge = open_bridge_session(config, arguments, ShimTransport::WebSocket).await?;
    match tokio::io::copy_bidirectional(&mut client, &mut bridge).await {
        Ok(_) => Ok(()),
        Err(error) if normal_disconnect(&error) => Ok(()),
        Err(error) => Err(error).context("could not relay the App WebSocket stream"),
    }
}

fn normal_disconnect(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof
    )
}

async fn proxy_control_socket(config: &ShimConfig) -> Result<()> {
    let path = app_server_control_socket(config);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => break stream,
            Err(error) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
                drop(error);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not connect to {}", path.display()));
            }
        }
    };
    let (mut socket_read, mut socket_write) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        result = tokio::io::copy(&mut stdin, &mut socket_write) => {
            result.map(|_| ()).context("could not relay proxy input")
        }
        result = tokio::io::copy(&mut socket_read, &mut stdout) => {
            result.map(|_| ()).context("could not relay proxy output")
        }
    }
}

async fn open_bridge_session(
    config: &ShimConfig,
    arguments: &[String],
    transport: ShimTransport,
) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(&config.agent_socket)
        .await
        .with_context(|| {
            format!(
                "could not connect to Shuttle agent socket {}; run 'cxs up {}' on the Mac",
                config.agent_socket.display(),
                config.profile
            )
        })?;
    let token = fs::read_to_string(&config.token_file)
        .with_context(|| format!("could not read {}", config.token_file.display()))?;
    let handshake = ShimHandshake {
        magic: SHIM_MAGIC.to_owned(),
        protocol_version: SHIM_PROTOCOL_VERSION,
        profile: config.profile.clone(),
        token: token.trim().to_owned(),
        app_server_args: arguments.to_vec(),
        transport,
    };
    let mut encoded = serde_json::to_vec(&handshake)?;
    if encoded.len() > 8 * 1024 - 1 {
        bail!("app-server arguments are too large for the shim handshake");
    }
    encoded.push(b'\n');
    stream.write_all(&encoded).await?;
    Ok(stream)
}

async fn run_agent(config: &ShimConfig, replace: bool) -> Result<()> {
    prepare_agent_socket(&config.agent_socket, replace).await?;
    let listener = UnixListener::bind(&config.agent_socket)
        .with_context(|| format!("could not bind {}", config.agent_socket.display()))?;
    fs::set_permissions(&config.agent_socket, fs::Permissions::from_mode(0o600))?;
    let _guard = SocketGuard(config.agent_socket.clone());
    let pid_path = config.agent_socket.with_extension("pid");
    write_agent_pid(&pid_path)?;
    let _pid_guard = PidFileGuard(pid_path);

    let token = fs::read_to_string(&config.token_file)
        .with_context(|| format!("could not read {}", config.token_file.display()))?;
    let hello = AgentHandshake {
        magic: AGENT_MAGIC.to_owned(),
        protocol_version: AGENT_PROTOCOL_VERSION,
        profile: config.profile.clone(),
        token: token.trim().to_owned(),
    };
    let mut stdout = tokio::io::stdout();
    let mut encoded = serde_json::to_vec(&hello)?;
    encoded.push(b'\n');
    stdout.write_all(&encoded).await?;
    stdout.flush().await?;

    let mux = cxs_mux::start(tokio::io::stdin(), stdout, cxs_mux::Role::Agent);
    serve_agent_with_exec_server(config, listener, mux).await
}

async fn serve_agent_with_exec_server(
    config: &ShimConfig,
    listener: UnixListener,
    mux: cxs_mux::MuxSession,
) -> Result<()> {
    let exec_server_port = available_exec_server_port(config.exec_server_port)?;
    let mut exec_server = spawn_exec_server(config, exec_server_port)?;
    let exec_server_stdin = exec_server
        .stdin
        .take()
        .context("Exec Server stdin was unavailable")?;
    let exec_server_pid = exec_server
        .id()
        .and_then(|value| i32::try_from(value).ok())
        .map(Pid::from_raw);
    tokio::time::sleep(Duration::from_millis(100)).await;
    if let Some(status) = exec_server.try_wait()? {
        bail!("Exec Server exited during startup with {status}");
    }
    let result = serve_agent_mux(config, listener, mux, exec_server_port).await;
    drop(exec_server_stdin);
    stop_exec_server(&mut exec_server, exec_server_pid).await;
    result
}

async fn serve_agent_mux(
    config: &ShimConfig,
    listener: UnixListener,
    mut mux: cxs_mux::MuxSession,
    exec_server_port: u16,
) -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("could not listen for SIGTERM")?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                let handle = mux.handle.clone();
                tokio::spawn(async move {
                    if let Err(error) = relay_shim_to_mux(socket, handle).await {
                        eprintln!("cxs-agent: App channel failed: {error:#}");
                    }
                });
            }
            incoming = mux.incoming.recv() => {
                let Some(incoming) = incoming else {
                    bail!("multiplexed SSH session stopped accepting channels");
                };
                let config = config.clone();
                tokio::spawn(async move {
                    let kind = incoming.kind;
                    if let Err(error) = relay_incoming_channel(&config, incoming, exec_server_port).await {
                        eprintln!("cxs-agent: {kind:?} channel failed: {error:#}");
                    }
                });
            }
            result = &mut mux.task => {
                return result.context("multiplexed SSH task failed")?;
            }
            signal = terminate.recv() => {
                if signal.is_some() {
                    return Ok(());
                }
            }
        }
    }
}

async fn prepare_agent_socket(path: &Path, replace: bool) -> Result<()> {
    let parent = path.parent().context("agent socket has no parent")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() {
            bail!("refusing to replace non-socket path {}", path.display());
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            if !replace {
                bail!(
                    "another cxs-agent is already listening on {}",
                    path.display()
                );
            }
            replace_agent(&path.with_extension("pid")).await?;
        }
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    let _ = fs::remove_file(path.with_extension("pid"));
    Ok(())
}

fn write_agent_pid(path: &Path) -> Result<()> {
    fs::write(path, format!("{}\n", std::process::id()))
        .with_context(|| format!("could not write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

async fn replace_agent(pid_path: &Path) -> Result<()> {
    let value = fs::read_to_string(pid_path).with_context(|| {
        format!(
            "could not read active agent PID from {}",
            pid_path.display()
        )
    })?;
    let raw_pid = value
        .trim()
        .parse::<i32>()
        .with_context(|| format!("invalid active agent PID in {}", pid_path.display()))?;
    if raw_pid <= 1 {
        bail!("refusing to replace invalid cxs-agent PID {raw_pid}");
    }
    let pid = Pid::from_raw(raw_pid);
    validate_agent_process(pid)?;
    let _ = kill(pid, Signal::SIGTERM);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while process_exists(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if process_exists(pid) {
        validate_agent_process(pid)?;
        let _ = kill(pid, Signal::SIGKILL);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while process_exists(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if process_exists(pid) {
        bail!("could not stop existing cxs-agent PID {raw_pid}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn process_exists(pid: Pid) -> bool {
    if kill(pid, None).is_err() {
        return false;
    }
    let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", pid.as_raw())) else {
        return true;
    };
    !stat
        .rsplit_once(") ")
        .is_some_and(|(_, status)| status.starts_with('Z'))
}

#[cfg(not(target_os = "linux"))]
fn process_exists(pid: Pid) -> bool {
    kill(pid, None).is_ok()
}

#[cfg(target_os = "linux")]
fn validate_agent_process(pid: Pid) -> Result<()> {
    let command_line = fs::read(format!("/proc/{}/cmdline", pid.as_raw()))
        .context("could not inspect the active cxs-agent process")?;
    if !command_line
        .split(|byte| *byte == 0)
        .any(|argument| argument == b"__cxs-agent")
    {
        bail!(
            "refusing to replace PID {} because it is not cxs-agent",
            pid.as_raw()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
fn validate_agent_process(_pid: Pid) -> Result<()> {
    Ok(())
}

async fn relay_shim_to_mux(mut socket: UnixStream, handle: cxs_mux::MuxHandle) -> Result<()> {
    let mut channel = handle.open(ChannelKind::App).await?;
    relay_bidirectional(&mut socket, &mut channel).await
}

async fn relay_incoming_channel(
    config: &ShimConfig,
    incoming: IncomingChannel,
    exec_server_port: u16,
) -> Result<()> {
    match incoming.kind {
        ChannelKind::Exec => {
            let mut socket = connect_exec_server(exec_server_port).await?;
            let mut channel = incoming.stream;
            relay_bidirectional(&mut socket, &mut channel).await
        }
        ChannelKind::Host => relay_host_app_server(config, incoming.stream).await,
        ChannelKind::App => bail!("Mac opened an unsupported App channel"),
    }
}

async fn relay_host_app_server(config: &ShimConfig, channel: cxs_mux::MuxStream) -> Result<()> {
    let host_codex_home = config.codex_home.join("host-app-server");
    fs::create_dir_all(&host_codex_home).with_context(|| {
        format!(
            "could not create Host App Server state {}",
            host_codex_home.display()
        )
    })?;
    fs::set_permissions(&host_codex_home, fs::Permissions::from_mode(0o700))?;
    let mut command = Command::new(&config.exec_server);
    command
        .args([
            "app-server",
            "--stdio",
            "--disable",
            "plugins",
            "--disable",
            "remote_plugin",
            "--disable",
            "plugin_sharing",
            "--disable",
            "apps",
        ])
        .env("CODEX_HOME", &host_codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().with_context(|| {
        format!(
            "could not start Host App Server {}",
            config.exec_server.display()
        )
    })?;
    let pid = child
        .id()
        .and_then(|value| i32::try_from(value).ok())
        .map(Pid::from_raw);
    let mut child_stdin = child
        .stdin
        .take()
        .context("Host App Server stdin was unavailable")?;
    let mut child_stdout = child
        .stdout
        .take()
        .context("Host App Server stdout was unavailable")?;
    let (mut channel_read, mut channel_write) = tokio::io::split(channel);
    let relay_result = tokio::select! {
        result = tokio::io::copy(&mut channel_read, &mut child_stdin) => {
            result.map(|_| ()).context("could not relay Host App Server input")
        }
        result = tokio::io::copy(&mut child_stdout, &mut channel_write) => {
            result.map(|_| ()).context("could not relay Host App Server output")
        }
    };
    drop(child_stdin);
    drop(channel_write);
    stop_exec_server(&mut child, pid).await;
    relay_result
}

async fn connect_exec_server(port: u16) -> Result<TcpStream> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => return Ok(stream),
            Err(error) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
                drop(error);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not connect to Exec Server port {port}"));
            }
        }
    }
}

async fn relay_bidirectional<A, B>(left: &mut A, right: &mut B) -> Result<()>
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match tokio::io::copy_bidirectional(left, right).await {
        Ok(_) => Ok(()),
        Err(error) if normal_disconnect(&error) => Ok(()),
        Err(error) => Err(error).context("multiplexed channel relay failed"),
    }
}

fn app_server_control_socket(config: &ShimConfig) -> PathBuf {
    std::env::var_os("CXS_CONTROL_SOCKET").map_or_else(
        || {
            config
                .control_socket
                .clone()
                .unwrap_or_else(|| config.agent_socket.with_file_name("app.sock"))
        },
        PathBuf::from,
    )
}

fn prepare_control_socket(path: &Path) -> Result<()> {
    let parent = path.parent().context("control socket has no parent")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() {
            bail!("refusing to replace non-socket path {}", path.display());
        }
        fs::remove_file(path)?;
    }
    Ok(())
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct PidFileGuard(PathBuf);

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn available_exec_server_port(preferred: u16) -> Result<u16> {
    if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", preferred)) {
        return Ok(listener.local_addr()?.port());
    }
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("could not reserve a loopback port for the Exec Server")?;
    Ok(listener.local_addr()?.port())
}

fn spawn_exec_server(config: &ShimConfig, port: u16) -> Result<tokio::process::Child> {
    let mut command = Command::new(&config.exec_server);
    let listen_url = format!("ws://127.0.0.1:{port}");
    command
        .args(&config.exec_server_args)
        .args(["--listen", &listen_url])
        .env("CODEX_HOME", &config.codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()
        .with_context(|| {
            format!(
                "could not start Exec Server {}",
                config.exec_server.display()
            )
        })
}

async fn stop_exec_server(child: &mut tokio::process::Child, pid: Option<Pid>) {
    if timeout(Duration::from_secs(3), child.wait()).await.is_ok() {
        return;
    }
    if let Some(pid) = pid {
        let _ = killpg(pid, Signal::SIGTERM);
    } else {
        let _ = child.start_kill();
    }
    if timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        if let Some(pid) = pid {
            let _ = killpg(pid, Signal::SIGKILL);
        }
        let _ = child.kill().await;
    }
}

async fn delegate_or_reject(config: &ShimConfig, arguments: &[String]) -> Result<()> {
    let Some(original) = &config.original_codex else {
        bail!("cxs-shim supports only 'codex --version' and 'codex app-server'");
    };
    let status = Command::new(original)
        .args(arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("could not delegate to {}", original.display()))?;
    if !status.success() {
        bail!("delegated Codex command exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    use cxs_core::{AppPaths, ProfileStatus, ProfileStore};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    #[test]
    fn detects_desktop_unix_listener_invocations() {
        assert!(requests_unix_listener(&[
            "--listen".to_owned(),
            "unix://".to_owned()
        ]));
        assert!(requests_unix_listener(&[
            "--listen=unix:///tmp/app.sock".to_owned()
        ]));
        assert!(!requests_unix_listener(&[
            "--listen".to_owned(),
            "stdio://".to_owned()
        ]));
    }

    #[tokio::test]
    async fn replacement_stops_the_previous_agent() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("agent.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket)?;
        let mut previous = std::process::Command::new("/bin/sleep");
        previous.arg0("__cxs-agent").arg("60");
        let mut previous = previous.spawn()?;
        fs::write(socket.with_extension("pid"), previous.id().to_string())?;
        let reaper = std::thread::spawn(move || previous.wait());

        prepare_agent_socket(&socket, true).await?;

        let status = reaper
            .join()
            .map_err(|_| anyhow::anyhow!("agent reaper thread panicked"))??;
        assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));
        assert!(!socket.exists());
        assert!(!socket.with_extension("pid").exists());
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires a compatible local Codex binary"]
    #[allow(clippy::too_many_lines)]
    async fn desktop_websocket_reaches_exec_server_end_to_end() -> Result<()> {
        let codex = std::env::var_os("CXS_TEST_CODEX")
            .map_or_else(|| PathBuf::from("/opt/homebrew/bin/codex"), PathBuf::from);
        if !codex.is_file() {
            bail!("set CXS_TEST_CODEX to a compatible Codex binary");
        }
        let directory = tempfile::tempdir()?;
        let paths = AppPaths {
            state_root: directory.path().join("state"),
            ssh_config: directory.path().join("ssh/config"),
            managed_ssh_config: directory.path().join("ssh/codex-shuttle.conf"),
            default_codex_home: directory.path().join("app-codex-home"),
        };
        fs::create_dir_all(&paths.default_codex_home)?;
        fs::create_dir_all(directory.path().join("exec-codex-home"))?;
        let remote_project = directory.path().join("remote-project");
        fs::create_dir_all(&remote_project)?;
        fs::write(
            remote_project.join("AGENTS.md"),
            "CXS_INTEGRATION_REMOTE_INSTRUCTION\n",
        )?;
        let store = ProfileStore::new(paths);
        let mut profile = store.create_prepared("integration", "unused", "codex-cli 0.147.0")?;
        profile.status = ProfileStatus::Installed;
        store.save(&profile)?;
        let token = store.read_token(&profile)?;

        let control_socket = directory.path().join("control.sock");
        let config = ShimConfig {
            profile: profile.name.clone(),
            codex_version: profile.codex_version.clone(),
            agent_socket: directory.path().join("agent.sock"),
            control_socket: Some(control_socket.clone()),
            token_file: profile.token_file.clone(),
            exec_server: codex.clone(),
            exec_server_args: vec![
                "exec-server".to_owned(),
                "--disable".to_owned(),
                "plugins".to_owned(),
                "--disable".to_owned(),
                "remote_plugin".to_owned(),
                "--disable".to_owned(),
                "plugin_sharing".to_owned(),
                "--disable".to_owned(),
                "apps".to_owned(),
            ],
            exec_server_port: profile.remote_exec_port,
            codex_home: directory.path().join("exec-codex-home"),
            original_codex: None,
        };
        assert_eq!(token.len(), 64);
        let (bridge_transport, agent_transport) = tokio::io::duplex(1024 * 1024);
        let (bridge_read, bridge_write) = tokio::io::split(bridge_transport);
        let (agent_read, agent_write) = tokio::io::split(agent_transport);
        let bridge_mux = cxs_mux::start(bridge_read, bridge_write, cxs_mux::Role::Bridge);
        let agent_mux = cxs_mux::start(agent_read, agent_write, cxs_mux::Role::Agent);
        prepare_agent_socket(&config.agent_socket, false).await?;
        let agent_listener = UnixListener::bind(&config.agent_socket)?;
        let agent_config = config.clone();
        let agent = tokio::spawn(async move {
            serve_agent_with_exec_server(&agent_config, agent_listener, agent_mux).await
        });
        let bridge_profile = profile.clone();
        let bridge = tokio::spawn(cxs_bridge::serve_multiplexed(
            bridge_profile,
            store,
            codex.clone(),
            bridge_mux,
        ));
        wait_for_path(&profile.local_socket).await?;

        let daemon_socket = control_socket.clone();
        let daemon_config = config.clone();
        let daemon = tokio::spawn(async move {
            serve_control_socket_at(
                &daemon_config,
                &["--listen".to_owned(), "unix://".to_owned()],
                daemon_socket,
            )
            .await
        });
        wait_for_path(&control_socket).await?;
        let stream = UnixStream::connect(&control_socket).await?;
        let (mut app, _) =
            tokio_tungstenite::client_async("ws://codex-app-server/rpc", stream).await?;
        app.send(Message::Text(
            serde_json::to_string(&json!({
                "id": "init",
                "method": "initialize",
                "params": {"clientInfo": {"name": "cxs-test", "version": "1"}}
            }))?
            .into(),
        ))
        .await?;
        app.send(Message::Text(
            serde_json::to_string(&json!({
                "id": "status",
                "method": "environment/status",
                "params": {"environmentId": profile.environment_id}
            }))?
            .into(),
        ))
        .await?;
        let (initialize_response, status_response) = timeout(Duration::from_secs(20), async {
            let mut initialize_response = None;
            while let Some(message) = app.next().await {
                let message = message?;
                if !message.is_text() {
                    continue;
                }
                let value: Value = serde_json::from_str(message.to_text()?)?;
                if value["id"] == "init" {
                    initialize_response = Some(value);
                    continue;
                }
                if value["id"] == "status" {
                    return Ok::<_, anyhow::Error>((
                        initialize_response.context("missing initialize response")?,
                        value,
                    ));
                }
            }
            bail!("connection closed before environment/status")
        })
        .await
        .context("timed out waiting for the bridged environment/status response")??;
        assert!(
            initialize_response
                .pointer("/result/codexHome")
                .and_then(Value::as_str)
                .is_some_and(|path| path.ends_with("/exec-codex-home/host-app-server")),
            "unexpected initialize response: {initialize_response}"
        );
        assert!(
            status_response
                .pointer("/result/status")
                .and_then(Value::as_str)
                == Some("ready")
                || status_response.get("result").and_then(Value::as_str) == Some("ready"),
            "unexpected status response: {status_response}"
        );
        app.send(Message::Text(
            serde_json::to_string(&json!({
                "id": "fs",
                "method": "fs/readDirectory",
                "params": {"path": directory.path()}
            }))?
            .into(),
        ))
        .await?;
        let fs_response = timeout(Duration::from_secs(10), async {
            while let Some(message) = app.next().await {
                let message = message?;
                if !message.is_text() {
                    continue;
                }
                let value: Value = serde_json::from_str(message.to_text()?)?;
                if value["id"] == "fs" {
                    return Ok::<_, anyhow::Error>(value);
                }
            }
            bail!("connection closed before fs/readDirectory")
        })
        .await
        .context("timed out waiting for remote fs/readDirectory")??;
        assert!(
            fs_response["result"]["entries"]
                .as_array()
                .is_some_and(|entries| entries
                    .iter()
                    .any(|entry| { entry["fileName"].as_str() == Some("exec-codex-home") })),
            "unexpected fs/readDirectory response: {fs_response}"
        );
        app.send(Message::Text(
            serde_json::to_string(&json!({
                "id": "thread",
                "method": "thread/start",
                "params": {"cwd": remote_project}
            }))?
            .into(),
        ))
        .await?;
        let thread_response = timeout(Duration::from_secs(20), async {
            while let Some(message) = app.next().await {
                let message = message?;
                if !message.is_text() {
                    continue;
                }
                let value: Value = serde_json::from_str(message.to_text()?)?;
                if value["id"] == "thread" {
                    return Ok::<_, anyhow::Error>(value);
                }
            }
            bail!("connection closed before thread/start")
        })
        .await
        .context("timed out waiting for thread/start")??;
        assert!(thread_response.get("error").is_none(), "{thread_response}");
        assert_eq!(
            thread_response.pointer("/result/instructionSources/0"),
            Some(&Value::String(
                remote_project.join("AGENTS.md").display().to_string()
            )),
            "unexpected thread/start response: {thread_response}"
        );
        let host_sessions = config.codex_home.join("host-app-server/sessions");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !thread_response
                .pointer("/result/thread/ephemeral")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            "Mac-owned real thread was unexpectedly ephemeral: {thread_response}"
        );
        assert!(
            !contains_regular_file(&host_sessions)?,
            "Host shadow thread was unexpectedly persisted under {}",
            host_sessions.display()
        );
        app.close(None).await?;
        drop(app);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !daemon.is_finished(),
            "control daemon stopped before its idle timeout"
        );
        daemon.abort();
        bridge.abort();
        agent.abort();
        Ok(())
    }

    async fn wait_for_path(path: &Path) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if path.exists() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        bail!("timed out waiting for {}", path.display())
    }

    fn contains_regular_file(path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_file() || contains_regular_file(&entry.path())? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
