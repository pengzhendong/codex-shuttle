use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use codex_app_server::{
    AppServerCodeModeHostArgs, AppServerRuntimeOptions, AppServerTransport,
    AppServerWebsocketAuthSettings, PluginStartupTasks, run_main_with_transport_options,
};
use codex_arg0::{Arg0DispatchPaths, arg0_dispatch_or_else};
use codex_config::LoaderOverrides;
use codex_exec_server::ExecServerRuntimePaths;
use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use codex_protocol::protocol::SessionSource;
use codex_utils_cli::CliConfigOverrides;

#[derive(Debug, Parser)]
#[command(name = "cxs-runtime", disable_version_flag = true)]
struct Cli {
    #[arg(short = 'V', long = "version", global = true)]
    version: bool,

    #[command(flatten)]
    config: CliConfigOverrides,

    #[arg(long, action = clap::ArgAction::Append, global = true)]
    enable: Vec<String>,

    #[arg(long, action = clap::ArgAction::Append, global = true)]
    disable: Vec<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    AppServer(AppServerArgs),
    ExecServer(ExecServerArgs),
}

#[derive(Debug, Args)]
struct AppServerArgs {
    #[command(flatten)]
    code_mode_host: AppServerCodeModeHostArgs,

    #[arg(long, default_value = AppServerTransport::DEFAULT_LISTEN_URL)]
    listen: AppServerTransport,

    #[arg(long, conflicts_with = "listen")]
    stdio: bool,

    #[arg(long)]
    strict_config: bool,

    #[arg(long)]
    analytics_default_enabled: bool,
}

#[derive(Debug, Args)]
struct ExecServerArgs {
    #[arg(long)]
    listen: Option<String>,

    #[arg(long)]
    strict_config: bool,
}

fn main() -> Result<()> {
    arg0_dispatch_or_else(|paths| async move { run(paths).await })
}

async fn run(paths: Arg0DispatchPaths) -> Result<()> {
    let mut cli = Cli::parse();
    if cli.version {
        println!("codex-cli {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    for feature in cli.enable {
        cli.config
            .raw_overrides
            .push(format!("features.{feature}=true"));
    }
    for feature in cli.disable {
        cli.config
            .raw_overrides
            .push(format!("features.{feature}=false"));
    }

    match cli.command.context("expected app-server or exec-server")? {
        Command::AppServer(args) => run_app_server(paths, cli.config, args).await,
        Command::ExecServer(args) => run_exec_server(paths, args).await,
    }
}

async fn run_app_server(
    paths: Arg0DispatchPaths,
    config: CliConfigOverrides,
    args: AppServerArgs,
) -> Result<()> {
    let transport = if args.stdio {
        AppServerTransport::Stdio
    } else {
        args.listen
    };
    let options = AppServerRuntimeOptions {
        code_mode_host_transport: args.code_mode_host.into(),
        plugin_startup_tasks: PluginStartupTasks::Skip,
        ..Default::default()
    };
    run_main_with_transport_options(
        paths,
        config,
        LoaderOverrides::default(),
        args.strict_config,
        args.analytics_default_enabled,
        transport,
        SessionSource::VSCode,
        AppServerWebsocketAuthSettings::default(),
        options,
    )
    .await
    .map_err(Into::into)
}

async fn run_exec_server(paths: Arg0DispatchPaths, args: ExecServerArgs) -> Result<()> {
    let self_exe = paths
        .codex_self_exe
        .context("runtime executable path is unavailable")?;
    let runtime_paths = ExecServerRuntimePaths::new(self_exe, paths.codex_linux_sandbox_exe)?;
    let listen = args
        .listen
        .unwrap_or_else(|| codex_exec_server::DEFAULT_LISTEN_URL.to_owned());
    let _ = args.strict_config;
    codex_exec_server::run_main(
        &listen,
        runtime_paths,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
    .await
    .map_err(anyhow::Error::from_boxed)
}
