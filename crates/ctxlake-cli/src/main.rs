//! `ctxlake` — the entire user-facing surface of the project. See docs/reference.md.
//!
//! Adoption is the product: a coordination tool nobody can install without reading
//! source code coordinates nothing. Every subcommand here is built to be run by a
//! human once and then forgotten, or wired into a runtime's hook config and never
//! thought about again.

mod briefing;
mod claims;
mod config;
mod config_cmd;
mod doctor;
mod hooks;
mod import;
mod init;
mod maint_cmd;
mod nudge;
mod paths;
mod repo;
mod sanitize;
mod service;
mod status;
mod store_ctx;
mod sync_cmd;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use hooks::Runtime as HookRuntime;

/// `EX_CONFIG` from `sysexits.h`: the config is missing or unusable.
///
/// Exiting 78 rather than 1 for this one case is what makes the systemd unit's
/// `RestartPreventExitStatus=78` mean something. A daemon whose `ctxlake.toml` has
/// gone missing cannot be fixed by restarting it, and without this the supervisor
/// would relaunch it every five seconds forever, burying the real error under
/// thousands of identical journal lines. `service.rs` has a test pinning the two
/// halves together.
pub(crate) const EX_CONFIG: i32 = 78;

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
    /// Backfill the history a runtime already has on disk. See docs/adding-it.md.
    Import(ImportCmd),
    /// Remove exactly what `install` added.
    Uninstall(InstallCmd),
    /// Show who is active, on what branch, doing what.
    Status,
    /// Print the resolved config.
    Config,
    /// Run the daemon — hook spool -> store, store -> local cache, and this
    /// agent's own presence — or install it as a service so it survives a reboot.
    Sync(SyncCmd),
    /// Compact, digest, and publish the snapshot. Safe to run on every host at once.
    Maint(MaintCmd),
    /// Review candidate, contested, and promoted claims.
    Claims(ClaimsCmd),
    /// Stop one agent's claims from promoting; its already-promoted claims move
    /// to contested. Capture continues.
    Quarantine(QuarantineCmd),
    /// Serve the MCP tool surface over stdio.
    ///
    /// `install` writes this command into each runtime's MCP config, so it has to be
    /// reachable as `ctxlake mcp` and not only as a separate binary.
    Mcp,
    /// Render the session briefing, or write it to the local cache for the hook to read.
    ///
    /// The hook cannot render this itself — rendering needs the lake, and the hook may
    /// not touch the object store (AGENTS.md invariant 1). So the expensive half runs
    /// here and the hook only reads the result.
    Briefing(BriefingCmd),
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
    /// Also install the sync daemon as a service (systemd user unit on Linux, a
    /// LaunchAgent on macOS) and start it, so it comes back after a reboot.
    ///
    /// Without this, `ctxlake sync start` runs a daemon that dies with the machine —
    /// and because capture keeps working regardless (the hook only writes locally),
    /// nothing tells you it is gone until the briefings quietly go stale.
    #[arg(long)]
    daemon: bool,
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
struct ImportCmd {
    /// `claude-code`, `cursor`, or `hermes`. Only `hermes` has a reader today —
    /// the other two are refused by name rather than accepted and quietly
    /// importing nothing (see `import/mod.rs`).
    #[arg(long)]
    runtime: Option<HookRuntime>,
    /// Every runtime, skipping the ones with no history on this host.
    #[arg(long)]
    all: bool,
    /// Only sessions whose last activity is at or after this point: a window
    /// (`90d`, `36h`, `45m`, `30s`) or a date (`2026-01-01`, or full RFC 3339).
    #[arg(long)]
    since: Option<String>,
    /// Count and classify, write nothing — neither the spool nor the dedup ledger.
    #[arg(long)]
    dry_run: bool,
    /// Read this file instead of the runtime's default state location.
    #[arg(long)]
    source: Option<PathBuf>,
}

#[derive(Args)]
struct SyncCmd {
    #[command(subcommand)]
    action: Option<SyncAction>,
    /// Which runtime this daemon's own presence heartbeat declares itself as. A
    /// sync daemon uploads every runtime's spool regardless of this value — it
    /// only labels this daemon's own live intent.
    #[arg(long, global = true)]
    runtime: Option<HookRuntime>,
}

/// The daemon's lifecycle. Bare `ctxlake sync` is [`SyncAction::Run`], which is what
/// it did before these subcommands existed.
///
/// `start`/`stop`/`restart`/`status` deliberately mean the same thing whether or not
/// a service is installed: with one, they drive systemd or launchd; without one, they
/// drive the re-exec'd background process directly. A user who never runs `install`
/// sees no behaviour change at all.
#[derive(Subcommand)]
enum SyncAction {
    /// Run the daemon in this process. `--foreground` stays attached (what a
    /// systemd/launchd unit invokes, and what tests drive); without it, re-exec into
    /// a detached background process.
    Run {
        #[arg(long)]
        foreground: bool,
    },
    /// Start the daemon — via the installed service if there is one.
    Start,
    /// Stop the daemon. A supervised daemon stays stopped; it is not restarted.
    Stop,
    /// Stop and start again.
    Restart,
    /// Whether the service is installed, whether it survives a reboot, and whether
    /// the daemon is actually running.
    Status,
    /// Install a systemd user unit (Linux) or LaunchAgent (macOS) so the daemon
    /// comes back after a reboot. See docs/reference.md.
    Install {
        /// Install the unit without starting it.
        #[arg(long)]
        no_start: bool,
    },
    /// Remove exactly what `install` added.
    #[command(alias = "uninstall")]
    Delete,
}

#[derive(Args)]
struct MaintCmd {
    /// Run the chain once and exit, instead of looping on an interval. What a
    /// cron entry or systemd timer should pass — see docs/reference.md on why that
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
struct BriefingCmd {
    /// Write the rendered briefing to the local cache instead of printing it.
    ///
    /// This is what the daemon calls. Without it, the command shows exactly what a
    /// session would be told — the first thing to check when a briefing looks wrong.
    #[arg(long)]
    write: bool,
}

#[derive(Args)]
struct QuarantineCmd {
    agent_id: String,
}

/// Marker attached to a config-load failure so [`main`] can exit [`EX_CONFIG`]
/// instead of 1. It carries no message of its own — the real explanation is the
/// context `load_config` already attaches — and exists purely to be downcast to.
#[derive(Debug)]
struct ConfigUnusable;

impl std::fmt::Display for ConfigUnusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the ctxlake config is missing or unusable")
    }
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("Error: {err:#}");
        // Only a config failure gets the distinguished code; everything else stays 1
        // so a supervisor still retries transient problems like a store that is
        // briefly unreachable.
        let code = if err.downcast_ref::<ConfigUnusable>().is_some() {
            EX_CONFIG
        } else {
            1
        };
        std::process::exit(code);
    }
}

async fn run() -> Result<()> {
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
            if args.daemon {
                println!();
                // After the config is written, so the unit's --config points at a
                // file that exists — install refuses otherwise, since the unit would
                // otherwise start a daemon that exits EX_CONFIG immediately.
                service::install(&cfg, &config_path, true)?;
            }
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
        // Synchronous on purpose: import reads a local SQLite file and appends to
        // the local spool. `ctxlake sync` is what carries those events to the
        // store, so nothing here touches the network (AGENTS.md invariant 1's
        // separation, applied to the import path too).
        Command::Import(args) => {
            let cfg = load_config(&config_path)?;
            import::run(
                &cfg,
                import::ImportRequest {
                    runtime: args.runtime,
                    all: args.all,
                    since: args.since,
                    dry_run: args.dry_run,
                    source: args.source,
                },
            )
        }
        Command::Uninstall(args) => run_install(&config_path, args, false).await,
        Command::Status => {
            let cfg = load_config(&config_path)?;
            status::run(&cfg).await
        }
        Command::Config => {
            let cfg = load_config(&config_path)?;
            config_cmd::run(&cfg)
        }
        Command::Sync(args) => {
            let cfg = load_config(&config_path)?;
            let runtime = args.runtime;
            match args.action.unwrap_or(SyncAction::Run { foreground: false }) {
                SyncAction::Run { foreground: true } => {
                    sync_cmd::run_foreground(&cfg, runtime).await
                }
                SyncAction::Run { foreground: false } => {
                    sync_cmd::run_background(&cfg, &config_path, runtime)
                }
                SyncAction::Start => service::start(&cfg, &config_path, runtime),
                SyncAction::Stop => service::stop(&cfg),
                SyncAction::Restart => service::restart(&cfg, &config_path, runtime),
                SyncAction::Status => service::status(&cfg),
                SyncAction::Install { no_start } => service::install(&cfg, &config_path, !no_start),
                SyncAction::Delete => service::uninstall(),
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
        // Blocking, and deliberately so: the MCP server owns stdio for the life of the
        // process and speaks a synchronous JSON-RPC loop. Nothing after it runs.
        Command::Mcp => {
            ctxlake_mcp::run_stdio();
            Ok(())
        }
        Command::Briefing(args) => {
            let cfg = load_config(&config_path)?;
            let text = briefing::render_briefing_for_fleet(&cfg.fleet_id);
            if args.write {
                let path = briefing::write_to_cache(&cfg.fleet_id, &text)?;
                println!("wrote {}", path.display());
            } else {
                print!("{text}");
            }
            Ok(())
        }
    }
}

fn load_config(path: &std::path::Path) -> Result<config::Config> {
    config::load(path)
        .with_context(|| {
            format!(
                "no usable config at {} — run `ctxlake init --store <url> --fleet <id>` first",
                path.display()
            )
        })
        // Downcast target for main's exit code. A supervised daemon must not be
        // restarted for this: no number of restarts will write the file.
        .map_err(|e| e.context(ConfigUnusable))
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
