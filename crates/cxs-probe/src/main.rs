use anyhow::Result;
use clap::Parser;
use cxs_core::desktop_codex_path;
use cxs_probe::local_codex_checks;
use cxs_ssh::query_remote;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Probe Codex Shuttle contracts", version)]
struct Arguments {
    /// Optional existing SSH host alias to inspect.
    #[arg(long)]
    host: Option<String>,

    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct Report {
    local: Vec<cxs_probe::Check>,
    remote: Option<RemoteReport>,
}

#[derive(Serialize)]
struct RemoteReport {
    host: String,
    passed: bool,
    detail: String,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let local = local_codex_checks(desktop_codex_path());
    let remote = arguments.host.map(|host| match query_remote(&host) {
        Ok(facts) => RemoteReport {
            host,
            passed: facts.kernel == "Linux" && matches!(facts.arch.as_str(), "x86_64" | "aarch64"),
            detail: format!(
                "kernel={} arch={} home={}",
                facts.kernel, facts.arch, facts.home
            ),
        },
        Err(error) => RemoteReport {
            host,
            passed: false,
            detail: error.to_string(),
        },
    });
    let report = Report { local, remote };
    if arguments.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for check in &report.local {
            println!(
                "{:<5} {:<24} {}",
                mark(check.passed),
                check.name,
                check.detail
            );
        }
        if let Some(remote) = &report.remote {
            println!(
                "{:<5} {:<24} {}",
                mark(remote.passed),
                format!("ssh:{}", remote.host),
                remote.detail
            );
        }
    }
    Ok(())
}

const fn mark(passed: bool) -> &'static str {
    if passed { "PASS" } else { "FAIL" }
}
