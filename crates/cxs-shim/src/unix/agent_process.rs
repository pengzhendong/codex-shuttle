use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use nix::sys::signal::kill;
use nix::unistd::Pid;

#[cfg(target_os = "linux")]
pub(super) fn existing_agent_processes() -> Result<Vec<Pid>> {
    let current_process_id =
        i32::try_from(std::process::id()).context("current PID exceeded i32")?;
    let current_user_id = fs::metadata("/proc/self")?.uid();
    let mut agents = Vec::new();
    for process in fs::read_dir("/proc").context("could not inspect Linux processes")? {
        let Ok(process) = process else { continue };
        let Some(raw_pid) = process
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if raw_pid <= 1 || raw_pid == current_process_id {
            continue;
        }
        let Ok(metadata) = process.metadata() else {
            continue;
        };
        if metadata.uid() != current_user_id {
            continue;
        }
        let Ok(command_line) = fs::read(process.path().join("cmdline")) else {
            continue;
        };
        if is_agent_command_line(&command_line) {
            agents.push(Pid::from_raw(raw_pid));
        }
    }
    agents.sort_unstable_by_key(|pid| pid.as_raw());
    Ok(agents)
}

#[cfg(target_os = "linux")]
pub(super) fn active_agent_pid(socket_path: &Path, _pid_path: &Path) -> Result<Pid> {
    find_socket_owner(socket_path)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn active_agent_pid(_socket_path: &Path, pid_path: &Path) -> Result<Pid> {
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
    Ok(Pid::from_raw(raw_pid))
}

#[cfg(target_os = "linux")]
pub(super) fn find_socket_owner(path: &Path) -> Result<Pid> {
    let sockets =
        fs::read_to_string("/proc/net/unix").context("could not inspect Linux Unix sockets")?;
    let socket_path = path.to_string_lossy();
    let inode = find_unix_socket_inode(&sockets, &socket_path)
        .with_context(|| format!("could not find active agent socket {}", path.display()))?;
    let expected = format!("socket:[{inode}]");
    for process in fs::read_dir("/proc").context("could not inspect Linux processes")? {
        let Ok(process) = process else { continue };
        let Some(raw_pid) = process
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        if raw_pid <= 1 {
            continue;
        }
        let Ok(descriptors) = fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        for descriptor in descriptors {
            let Ok(descriptor) = descriptor else {
                continue;
            };
            if fs::read_link(descriptor.path())
                .is_ok_and(|target| target.as_os_str() == expected.as_str())
            {
                return Ok(Pid::from_raw(raw_pid));
            }
        }
    }
    bail!(
        "could not identify the cxs-agent listening on {}",
        path.display()
    )
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn find_unix_socket_inode<'a>(table: &'a str, path: &str) -> Option<&'a str> {
    table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let inode = fields.nth(6)?;
        (fields.next()? == path).then_some(inode)
    })
}

#[cfg(target_os = "linux")]
pub(super) fn process_exists(pid: Pid) -> bool {
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

#[cfg(target_os = "macos")]
pub(super) fn process_exists(pid: Pid) -> bool {
    if kill(pid, None).is_err() {
        return false;
    }
    let Ok(output) = std::process::Command::new("/bin/ps")
        .args(["-p", &pid.as_raw().to_string(), "-o", "stat="])
        .output()
    else {
        return true;
    };
    output.status.success()
        && !String::from_utf8_lossy(&output.stdout)
            .trim_start()
            .starts_with('Z')
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn process_exists(pid: Pid) -> bool {
    kill(pid, None).is_ok()
}

#[cfg(target_os = "linux")]
pub(super) fn validate_agent_process(pid: Pid) -> Result<()> {
    let command_line = fs::read(format!("/proc/{}/cmdline", pid.as_raw()))
        .context("could not inspect the active cxs-agent process")?;
    if !is_agent_command_line(&command_line) {
        bail!(
            "refusing to replace PID {} because it is not cxs-agent",
            pid.as_raw()
        );
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn is_agent_command_line(command_line: &[u8]) -> bool {
    command_line
        .split(|byte| *byte == 0)
        .any(|argument| argument == b"__cxs-agent")
}

#[cfg(target_os = "macos")]
pub(super) fn validate_agent_process(pid: Pid) -> Result<()> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-ww", "-p", &pid.as_raw().to_string(), "-o", "args="])
        .output()
        .context("could not inspect the active cxs-agent process")?;
    let command = String::from_utf8_lossy(&output.stdout);
    let is_agent = command
        .split_ascii_whitespace()
        .any(|argument| argument == "__cxs-agent");
    if !output.status.success() || !is_agent {
        bail!(
            "refusing to replace PID {} because it is not cxs-agent",
            pid.as_raw()
        );
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[allow(clippy::unnecessary_wraps)]
pub(super) fn validate_agent_process(_pid: Pid) -> Result<()> {
    Ok(())
}
