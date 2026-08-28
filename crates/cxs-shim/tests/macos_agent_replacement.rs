#![cfg(target_os = "macos")]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::Signal;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_agent(config: &Path, replace: bool) -> io::Result<ChildGuard> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cxs-shim"));
    command
        .arg("__cxs-agent")
        .env("CXS_SHIM_CONFIG", config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if replace {
        command.arg("--replace");
    }
    command.spawn().map(ChildGuard)
}

fn wait_for_pid(path: &Path, expected: u32) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if fs::read_to_string(path)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            == Some(expected)
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "agent PID file {} did not contain {expected}",
            path.display()
        ),
    ))
}

#[test]
fn live_agent_is_replaced_end_to_end() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let socket = directory.path().join("agent.sock");
    let pid_file = socket.with_extension("pid");
    let token_file = directory.path().join("token");
    let config = directory.path().join("shim.json");
    let exec_server = directory.path().join("fake-exec-server");
    let codex_home = directory.path().join("codex-home");
    fs::create_dir(&codex_home)?;
    fs::write(&token_file, "test-token\n")?;
    fs::write(&exec_server, "#!/bin/sh\ncat >/dev/null\n")?;
    fs::set_permissions(&exec_server, fs::Permissions::from_mode(0o700))?;
    fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "profile": "macos-replacement-test",
            "codex_version": "codex-cli test",
            "agent_socket": socket,
            "token_file": token_file,
            "exec_server": exec_server,
            "exec_server_args": [],
            "exec_server_port": 0,
            "codex_home": codex_home,
            "original_codex": null
        }))?,
    )?;

    let mut first = start_agent(&config, false)?;
    let first_pid = first.0.id();
    wait_for_pid(&pid_file, first_pid)?;

    let mut replacement = start_agent(&config, true)?;
    wait_for_pid(&pid_file, replacement.0.id())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let first_status = loop {
        if let Some(status) = first.0.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            return Err("original agent did not exit".into());
        }
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(first_status.signal(), Some(Signal::SIGTERM as i32));
    assert!(replacement.0.try_wait()?.is_none());
    Ok(())
}
