//! `ctxlake` — the entire user-facing surface of the project. See docs/cli.md.
//!
//! Adoption is the product: a coordination tool nobody can install without reading
//! source code coordinates nothing. Every subcommand here is built to be run by a
//! human once and then forgotten, or wired into a runtime's hook config and never
//! thought about again.

mod claim;
mod claims;
mod config;
mod config_cmd;
mod doctor;
mod hooks;
mod init;
mod maint_cmd;
mod nudge;
mod paths;
mod sanitize;
mod status;
mod store_ctx;
mod sync_cmd;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use hooks::Runtime as HookRuntime;

#[derive(Parser)]
#[command(
    name = "ctxlake",
    version,
    about = "A zero-compute coordination layer for fleets of coding agents."
)]
struct Cli {
    /// Path to `ctxlake.toml`. Defaults to `$XDG_CONFIG_HOME/ctxlake/ctxlake.toml`
    /// (or `~/.config/ctxlake/ctxlake.toml`).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Point ctxlake at a store and write `ctxlake.toml`.
    Init(InitCmd),
    /// Run the backend's CAS matrix and detect installed runtimes.
    Doctor,
    /// Merge ctxlake's hooks into a runtime's config.
    Install(InstallCmd),
    /// Remove exactly what `install` added.
    Uninstall(InstallCmd),
    /// Show who is active, on what branch, holding what.
    Status,
    /// Acquire a lease on one or more resources.
    Claim(ClaimCmd),
    /// Release a lease this agent holds.
    Release(ReleaseCmd),
    /// Print the resolved config.
    Config,
    /// Run (or check, or stop) the daemon: hook spool -> store, store -> local
    /// cache, and this agent's own presence.
    Sync(SyncCmd),
    /// Run the maintenance chain under the fleet-wide maintenance lease.
    Maint(MaintCmd),
    /// Review candidate, contested, and promoted claims.
    Claims(ClaimsCmd),
    /// Stop one agent's claims from promoting; its already-promoted claims move
    /// to contested. Capture continues.
    Quarantine(QuarantineCmd),
}

#[derive(Args)]
struct InitCmd {
    #[arg(long)]
    store: String,
    #[arg(long)]
    fleet: String,
    #[arg(long = "agent-id")]
    agent_id: Option<String>,
    /// Overwrite an existing config rather than refusing (AGENTS.md invariant 8's
    /// same "never clobber silently" reasoning applies to this file too).
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct InstallCmd {
    /// `claude-code`, `cursor`, or `hermes`.
    runtime: Option<HookRuntime>,
    /// Every runtime `doctor` would detect as installed.
    #[arg(long)]
    all: bool,
    /// Compute and print the change without writing anything.
    #[arg(long)]
    dry_run: bool,
}

impl clap::ValueEnum for HookRuntime {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            HookRuntime::ClaudeCode,
            HookRuntime::Cursor,
            HookRuntime::Hermes,
        ]
    }
    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.name()))
    }
}

#[derive(Args)]
struct ClaimCmd {
    /// One or more resource identifiers — a path glob, a package name, anything a
    /// human would type to describe what they're about to touch. Each is hashed via
    /// `ctxlake_core::hash::resource_key` into its own independent lease.
    #[arg(required = true)]
    resources: Vec<String>,
    #[arg(long)]
    reason: Option<String>,
    /// Seconds. Defaults to the fleet-wide 5-minute lease TTL (AGENTS.md's knob
    /// table) if omitted.
    #[arg(long)]
    ttl: Option<u64>,
    /// Require every requested resource to be claimable, or acquire none of them —
    /// see `claim::run`'s doc for why this is the only thing "exclusive" can mean
    /// for a primitive that is already single-holder by construction.
    #[arg(long)]
    exclusive: bool,
}

#[derive(Args)]
struct ReleaseCmd {
    resources: Vec<String>,
    /// Release every lease this agent currently holds, not just the ones named.
    #[arg(long)]
    all: bool,
}

#[derive(Args)]
struct SyncCmd {
    /// Run attached to this process instead of daemonizing. What tests exercise
    /// directly, and what a systemd/launchd unit should invoke.
    #[arg(long)]
    foreground: bool,
    /// Report whether the daemon is running, then exit. Mutually exclusive with
    /// every other flag here.
    #[arg(long)]
    status: bool,
    /// Stop a running daemon (SIGTERM, then wait briefly), then exit.
    #[arg(long)]
    stop: bool,
    /// Which runtime this daemon's own presence heartbeat declares itself as. A
    /// sync daemon uploads every runtime's spool regardless of this value — it
    /// only labels this daemon's own live intent.
    #[arg(long)]
    runtime: Option<HookRuntime>,
}

#[derive(Args)]
struct MaintCmd {
    /// Run the chain once and exit, instead of looping on an interval. What a
    /// cron entry or systemd timer should pass — see docs/cli.md on why that
    /// timer is optional.
    #[arg(long)]
    once: bool,
}

#[derive(Args)]
struct ClaimsCmd {
    #[arg(long, value_parser = ["candidate", "contested", "promoted"])]
    status: String,
    /// For `--status candidate`: name which of the four gates rejected each
    /// claim, and why.
    #[arg(long)]
    explain: bool,
}

#[derive(Args)]
struct QuarantineCmd {
    agent_id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(paths::default_config_path);

    match cli.command {
        Command::Init(args) => {
            let cfg = init::run(
                init::InitArgs {
                    store: &args.store,
                    fleet_id: &args.fleet,
                    agent_id: args.agent_id.as_deref(),
                    force: args.force,
                },
                &config_path,
            )
            .await?;
            println!(
                "wrote {}\n  store:     {}\n  fleet_id:  {}\n  agent_id:  {}",
                config_path.display(),
                cfg.store,
                cfg.fleet_id,
                cfg.agent_id
            );
            println!("\nNext: ctxlake doctor");
            Ok(())
        }
        Command::Doctor => {
            let cfg = load_config(&config_path)?;
            let report = doctor::run(&cfg).await?;
            report.print();
            if report.breaks_capture() {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Install(args) => run_install(&config_path, args, true).await,
        Command::Uninstall(args) => run_install(&config_path, args, false).await,
        Command::Status => {
            let cfg = load_config(&config_path)?;
            status::run(&cfg).await
        }
        Command::Claim(args) => {
            let cfg = load_config(&config_path)?;
            claim::claim(
                &cfg,
                &args.resources,
                args.reason.as_deref(),
                args.ttl,
                args.exclusive,
            )
            .await
        }
        Command::Release(args) => {
            let cfg = load_config(&config_path)?;
            claim::release(&cfg, &args.resources, args.all).await
        }
        Command::Config => {
            let cfg = load_config(&config_path)?;
            config_cmd::run(&cfg)
        }
        Command::Sync(args) => {
            if args.status as u8 + args.stop as u8 > 1 {
                anyhow::bail!("pass at most one of --status / --stop");
            }
            let cfg = load_config(&config_path)?;
            if args.status {
                sync_cmd::status(&cfg)
            } else if args.stop {
                sync_cmd::stop(&cfg)
            } else if args.foreground {
                sync_cmd::run_foreground(&cfg, args.runtime).await
            } else {
                sync_cmd::run_background(&cfg, &config_path, args.runtime)
            }
        }
        Command::Maint(args) => {
            let cfg = load_config(&config_path)?;
            maint_cmd::run(&cfg, args.once).await
        }
        Command::Claims(args) => {
            let cfg = load_config(&config_path)?;
            claims::run_claims(&cfg, &args.status, args.explain).await
        }
        Command::Quarantine(args) => {
            let cfg = load_config(&config_path)?;
            claims::quarantine(&cfg, &args.agent_id).await
        }
    }
}

fn load_config(path: &std::path::Path) -> Result<config::Config> {
    config::load(path).with_context(|| {
        format!(
            "no usable config at {} — run `ctxlake init --store <url> --fleet <id>` first",
            path.display()
        )
    })
}

async fn run_install(
    config_path: &std::path::Path,
    args: InstallCmd,
    installing: bool,
) -> Result<()> {
    let cfg = load_config(config_path)?;
    let targets: Vec<HookRuntime> = match (args.runtime, args.all) {
        (Some(_), true) => anyhow::bail!("pass a runtime or --all, not both"),
        (Some(r), false) => vec![r],
        (None, true) => detected_runtimes(),
        (None, false) => anyhow::bail!("pass a runtime (claude-code, cursor, hermes) or --all"),
    };

    let mut any_error = false;
    for runtime in targets {
        let path = runtime.default_config_path();
        let plan = if installing {
            hooks::plan_install(runtime, &path, &cfg.fleet_id, &cfg.agent_id)
        } else {
            hooks::plan_uninstall(runtime, &path)
        };
        let plan = match plan {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{runtime}: {e:#}");
                any_error = true;
                continue;
            }
        };
        let verb = if installing { "install" } else { "uninstall" };
        if !plan.changed() {
            println!(
                "{runtime}: already up to date ({verb} is a no-op) — {}",
                path.display()
            );
            continue;
        }
        if args.dry_run {
            println!(
                "{runtime}: would change {}\n{}",
                path.display(),
                plan.diff()
            );
        } else {
            hooks::apply(&plan)?;
            println!("{runtime}: {verb}ed into {}", path.display());
            if installing {
                if let Some(caveat) = hooks::install_caveat(runtime) {
                    eprintln!("{runtime}: warning: {caveat}");
                }
            }
        }
    }
    if any_error {
        anyhow::bail!("one or more runtimes failed — see above");
    }
    Ok(())
}

fn detected_runtimes() -> Vec<HookRuntime> {
    HookRuntime::ALL
        .into_iter()
        .filter(|r| r.default_config_path().exists())
        .collect()
}
