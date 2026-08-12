use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use cxs_core::routing;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

pub fn codex_version(codex: &Path) -> Result<String> {
    let output = Command::new(codex)
        .arg("--version")
        .output()
        .with_context(|| format!("could not run {} --version", codex.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "{} --version exited with {}",
            codex.display(),
            output.status
        );
    }
    let version = String::from_utf8(output.stdout).context("Codex version output was not UTF-8")?;
    Ok(version.trim().to_owned())
}

#[must_use]
pub fn local_codex_checks(codex: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    match codex_version(codex) {
        Ok(version) => checks.push(Check {
            name: "codex-version".to_owned(),
            passed: true,
            detail: version,
        }),
        Err(error) => {
            checks.push(Check {
                name: "codex-version".to_owned(),
                passed: false,
                detail: error.to_string(),
            });
            return checks;
        }
    }

    match Command::new(codex).args(["exec-server", "--help"]).output() {
        Ok(output) => {
            let help = String::from_utf8_lossy(&output.stdout);
            let supports_stdio = output.status.success()
                && help.contains("--listen")
                && help.contains("stdio")
                && help.contains("--exit-on-stdin-close");
            checks.push(Check {
                name: "exec-server-stdio".to_owned(),
                passed: supports_stdio,
                detail: if supports_stdio {
                    "exec-server exposes stdio and exit-on-stdin-close".to_owned()
                } else {
                    "required experimental exec-server flags were not found".to_owned()
                },
            });
        }
        Err(error) => checks.push(Check {
            name: "exec-server-stdio".to_owned(),
            passed: false,
            detail: error.to_string(),
        }),
    }

    checks.push(match app_server_environment_contract(codex) {
        Ok(()) => Check {
            name: "app-exec-adapter".to_owned(),
            passed: true,
            detail: "execution-environment and remote host RPC contracts are present".to_owned(),
        },
        Err(error) => Check {
            name: "app-exec-adapter".to_owned(),
            passed: false,
            detail: error.to_string(),
        },
    });
    checks
}

fn app_server_environment_contract(codex: &Path) -> Result<()> {
    let directory = tempfile::tempdir().context("could not create schema probe directory")?;
    let output = Command::new(codex)
        .args([
            "app-server",
            "generate-json-schema",
            "--experimental",
            "--out",
        ])
        .arg(directory.path())
        .output()
        .context("could not generate the experimental App Server schema")?;
    if !output.status.success() {
        anyhow::bail!(
            "App Server schema generation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let schema_path = directory.path().join("ClientRequest.json");
    let schema: serde_json::Value = serde_json::from_slice(&std::fs::read(&schema_path)?)?;
    let definitions = schema
        .get("definitions")
        .and_then(serde_json::Value::as_object)
        .context("schema did not contain definitions")?;
    let has_exec_url = definitions
        .get("EnvironmentAddParams")
        .and_then(|value| value.pointer("/properties/execServerUrl"))
        .is_some();
    let has_environment_id = definitions
        .get("TurnEnvironmentParams")
        .and_then(|value| value.pointer("/properties/environmentId"))
        .is_some();
    let has_thread_environments = definitions
        .get("ThreadStartParams")
        .and_then(|value| value.pointer("/properties/environments"))
        .is_some();
    let methods = schema
        .get("oneOf")
        .and_then(serde_json::Value::as_array)
        .context("schema did not contain client request variants")?;
    let has_method = |expected: &str| {
        methods.iter().any(|request| {
            request
                .pointer("/properties/method/enum/0")
                .and_then(serde_json::Value::as_str)
                == Some(expected)
        })
    };
    let has_host_rpcs = routing::HOST_REQUEST_METHODS
        .iter()
        .copied()
        .all(has_method);
    if !(has_exec_url && has_environment_id && has_thread_environments && has_host_rpcs) {
        anyhow::bail!("required execution-environment or remote host RPC contracts are missing");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_codex_is_a_failed_check() {
        let checks = local_codex_checks(Path::new("/definitely/missing/codex"));
        assert_eq!(checks.len(), 1);
        assert!(!checks[0].passed);
    }
}
