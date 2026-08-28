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
        .stderr(Stdio::inherit());
    if replace {
        command.arg("--replace");
    }
    command.spawn().map(ChildGuard)
}

fn wait_for_pid(child: &mut ChildGuard, path: &Path, expected: u32) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if fs::read_to_string(path)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            == Some(expected)
        {
            return Ok(());
        }
        if let Some(status) = child.0.try_wait()? {
            return Err(io::Error::other(format!(
                "cxs-agent {expected} exited before publishing its PID: {status}"
            )));
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
    // macOS limits Unix-domain socket paths to 104 bytes. GitHub's runner
    // temp directory is deeply nested, so keep this fixture directly in /tmp.
    let directory = tempfile::tempdir_in("/tmp")?;
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
    wait_for_pid(&mut first, &pid_file, first_pid)?;

    let mut replacement = start_agent(&config, true)?;
    let replacement_pid = replacement.0.id();
    wait_for_pid(&mut replacement, &pid_file, replacement_pid)?;
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
    // Depending on whether the Tokio signal listener is installed before the
    // replacement arrives, SIGTERM either terminates the process directly or
    // is handled as a graceful successful shutdown.
    assert!(
        first_status.success() || first_status.signal() == Some(Signal::SIGTERM as i32),
        "original agent exited unexpectedly: {first_status}"
    );
    assert!(replacement.0.try_wait()?.is_none());
    Ok(())
}
