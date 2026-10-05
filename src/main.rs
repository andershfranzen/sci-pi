mod acp;
mod agent;
mod anthropic_auth;
mod auth;
mod build_info;
mod config;
mod git;
mod hub;
mod model;
mod remote;
mod server;
mod session;
mod store;
mod tailscale;
mod terminal;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Remote-first coding agents: run them on your machines, drive them from anywhere.
#[derive(Parser)]
#[command(version = crate::build_info::VERSION)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon (normally via the systemd user unit `sci-pi add` installs).
    Serve,
    /// Open the multi-host UI on this machine.
    Ui {
        #[arg(long, default_value_t = config::HUB_PORT)]
        port: u16,
        /// Don't open a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Install sci-pi on a machine over SSH and remember it.
    Add {
        /// SSH destination: an alias from ~/.ssh/config or user@host.
        target: String,
        /// Name shown in the UI (defaults to the host part of the target).
        #[arg(long)]
        name: Option<String>,
        /// Reinstall even if agents are mid-turn (they'll be interrupted).
        #[arg(long)]
        force: bool,
    },
    /// Push this binary to every remembered host and restart their daemons.
    Update {
        #[arg(long)]
        force: bool,
    },
    /// Manage this machine's own daemon.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// List remembered hosts.
    Hosts,
    /// Print this machine's daemon token.
    Token,
    /// Pair a local browser with a separately revocable device credential.
    Pair {
        #[arg(long, default_value = "Browser")]
        name: String,
        #[arg(long)]
        no_open: bool,
    },
    /// Check agents, Tailscale and config on this machine.
    Doctor,
    /// Run sci-pi's own agent as an ACP server on stdio (the daemon does this; so can editors).
    Acp,
    /// Manage model provider credentials on this host.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    #[command(hide = true)]
    LocalInfo,
    #[command(hide = true)]
    Busy,
    #[command(hide = true)]
    DeploymentCheck,
    #[command(hide = true)]
    TailscaleAllow { login: String },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Install (or upgrade to) this binary as a systemd user service and (re)start it.
    Install {
        /// Reinstall even when turns are active (they will be interrupted).
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Store an API key (read from stdin), e.g. `sci-pi auth set anthropic`.
    Set { provider: String },
    /// Sign in to Anthropic using browser OAuth (supports remote/headless hosts).
    Login { provider: String },
    /// Remove sci-pi's saved OAuth login (does not remove API keys).
    Logout { provider: String },
    /// Show which providers have credentials.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "sci_pi=info".into()),
        )
        .init();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    match Cli::parse().cmd {
        Cmd::Serve => server::serve().await,
        Cmd::Ui { port, no_open } => hub::run(port, !no_open).await,
        Cmd::Add { target, name, force } => remote::add(&target, name, force).await,
        Cmd::Update { force } => remote::update_all(force).await,
        Cmd::Service { cmd: ServiceCmd::Install { force } } => remote::install_local(force).await,
        Cmd::Busy => remote::busy().await,
        Cmd::DeploymentCheck => remote::deployment_check().await,
        Cmd::Hosts => {
            for (name, h) in config::Hosts::load()?.hosts {
                let via = h.tailnet_url.as_deref().unwrap_or("ssh tunnel");
                println!("{name:16} ssh={:24} {via}", h.ssh);
            }
            Ok(())
        }
        Cmd::Token => {
            println!("{}", config::token()?);
            Ok(())
        }
        Cmd::Pair { name, no_open } => server::pair(&name, !no_open).await,
        Cmd::Doctor => doctor().await,
        Cmd::Acp => agent::serve_stdio().await,
        Cmd::Auth { cmd: AuthCmd::Login { provider } } => {
            anyhow::ensure!(provider == "anthropic", "browser login is supported for anthropic only");
            anthropic_auth::login().await
        }
        Cmd::Auth { cmd: AuthCmd::Logout { provider } } => {
            anyhow::ensure!(provider == "anthropic", "OAuth logout is supported for anthropic only");
            anthropic_auth::logout()?;
            println!("removed sci-pi's Anthropic OAuth login; API keys are unchanged");
            Ok(())
        }
        Cmd::Auth { cmd: AuthCmd::Set { provider } } => {
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                eprint!("{provider} API key: ");
            }
            let mut key = String::new();
            std::io::stdin().read_line(&mut key)?;
            let key = key.trim();
            anyhow::ensure!(!key.is_empty(), "no key given");
            config::set_credential(&provider, key)?;
            println!("saved {provider} key to {}", config::config_dir().join("credentials.toml").display());
            Ok(())
        }
        Cmd::Auth { cmd: AuthCmd::Status } => {
            for p in ["anthropic", "openai"] {
                if config::credential(p, None).is_some() {
                    println!("✓ {p} (API key)");
                } else if p == "anthropic" {
                    match anthropic_auth::status()? {
                        Some(status) => println!("✓ {p} ({status})"),
                        None => println!("✗ {p}"),
                    }
                } else {
                    println!("✗ {p}");
                }
            }
            for p in config::credential_providers().into_iter().filter(|p| p != "anthropic" && p != "openai") {
                println!("✓ {p}");
            }
            Ok(())
        }
        Cmd::LocalInfo => remote::local_info().await,
        Cmd::TailscaleAllow { login } => remote::tailscale_allow(&login),
    }
}

async fn doctor() -> Result<()> {
    let cfg = config::Config::load_or_init()?;
    println!("config:  {}", config::config_dir().join("config.toml").display());
    println!("data:    {}", config::data_dir().display());
    println!("agents:");
    for (id, a) in &cfg.agents {
        let found = which(&a.command[0]);
        let mark = if found.is_some() { "✓" } else { "✗" };
        println!("  {mark} {id:10} {}  ({})", a.command.join(" "), found.unwrap_or_else(|| "not on PATH".into()));
    }
    match tailscale::self_node().await {
        Some(n) => {
            println!("tailscale: {} {:?}", n.dns_name, n.ips);
            println!("  owner: {}", n.owner.as_deref().unwrap_or("(tagged node)"));
            let allow = if cfg.tailscale.allow.is_empty() { n.owner.clone().into_iter().collect() } else { cfg.tailscale.allow.clone() };
            println!("  token-free access for: {}", if allow.is_empty() { "nobody (set tailscale.allow)".into() } else { allow.join(", ") });
            match tailscale::cert_pair(&n.dns_name).await {
                Ok(_) => println!("  https: ✓ cert available"),
                Err(e) => println!("  https: ✗ {e:#}  (fix: sudo tailscale set --operator=$USER)"),
            }
        }
        None => println!("tailscale: not running (SSH tunnels only)"),
    }
    println!("ntfy:    {}", cfg.ntfy_url.as_deref().unwrap_or("not configured"));
    Ok(())
}

fn which(cmd: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(cmd)).find(|p| p.is_file()).map(|p| p.display().to_string())
}
