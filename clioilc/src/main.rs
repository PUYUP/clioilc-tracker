mod cloud;
mod commands;
mod readers;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use clioilc_core::{home_dir, EventFilter, Store};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "clioilc",
    about = "Track what your AI agents do, locally and on your clioilc dashboard"
)]
struct Cli {
    /// Directory where events are stored (default: ~/.clioilc)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Watch a reader's directory for changes and record events
    Watch {
        /// Reader name (default: first registered reader)
        #[arg(long)]
        reader: Option<String>,
        /// Override the watch directory
        #[arg(long)]
        path: Option<PathBuf>,
        /// Also print appended lines to stdout as `<path>\t<line>`
        #[arg(long)]
        follow: bool,
        /// Also upload new data in the background (requires `clioilc login`)
        #[arg(long)]
        sync: bool,
        /// Seconds between background syncs
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(5..))]
        sync_every: u64,
    },
    /// Connect this machine to your clioilc account (approve in the browser)
    Login {
        /// Backend URL, e.g. https://api.clioilc.com (default: $CLIOILC_SERVER
        /// or the server of the previous login)
        #[arg(long)]
        server: Option<String>,
        /// Device name shown in the dashboard (default: the Computer Name on macOS, else the hostname)
        #[arg(long)]
        name: Option<String>,
        /// How private data is encrypted, asked on the account's first login.
        /// Cannot be changed later
        #[arg(long, value_enum)]
        mode: Option<commands::login::Mode>,
        /// Do not ask before rebuilding usage.db with the account's salt
        #[arg(long)]
        yes: bool,
        /// Do not open the browser automatically
        #[arg(long)]
        no_browser: bool,
    },
    /// Revoke this machine's token and forget the login (local data is kept)
    Logout,
    /// Share this folder's agent data with a team project (workspace/project)
    Link {
        /// Team project, e.g. acme/api (default: the project whose git remote
        /// matches this folder's, matched locally)
        target: Option<String>,
        /// Folder to link (default: the current folder)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Also share the folder's history from before today
        #[arg(long, conflicts_with = "no_history")]
        history: bool,
        /// Share only from now on (the default when not asked)
        #[arg(long)]
        no_history: bool,
        /// Also link recorded subfolder projects without asking
        #[arg(long)]
        subfolders: bool,
    },
    /// List folders linked to team projects
    Links,
    /// End-to-end team workspaces: your fingerprint, verify teammates who gave
    /// you keys, then share linked folders now
    Team {
        #[command(subcommand)]
        command: Option<TeamCommands>,
    },
    /// Stop sharing this folder with its team project
    Unlink {
        /// Folder to unlink (default: the current folder)
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Upload data not yet on the server
    Sync {
        /// Send the whole local history again, not only what is new (safe:
        /// the server stores rows by id, nothing is counted twice)
        #[arg(long)]
        resend: bool,
    },
    /// Run `watch --sync` in the background at login (launchd / systemd --user)
    Service {
        #[command(subcommand)]
        command: ServiceCommands,
    },
    /// A standup of the last day: printed, or sent to Slack, Discord or Teams
    Standup {
        #[command(subcommand)]
        command: Option<StandupCommands>,
        /// How many days back (by last activity)
        #[arg(long, default_value_t = 1)]
        days: i64,
        /// Send it to the webhook set with `clioilc standup webhook`
        #[arg(long)]
        send: bool,
    },
    /// Signed record of which commits of a release AI agents helped with (for audits)
    #[command(args_conflicts_with_subcommands = true)]
    Attest {
        #[command(subcommand)]
        command: Option<AttestCommands>,
        /// The release: a tag (or any commit) in this repository
        release: Option<String>,
        /// Compare with this tag or commit (default: the tag before the release)
        #[arg(long)]
        base: Option<String>,
        /// Attest with the whole team's work in this workspace (default: yours)
        #[arg(long)]
        workspace: Option<String>,
        /// Where to save the signed envelope (default: attestation-<release>.json)
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// The repository's folder (default: the current folder)
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Work items of the last days: what was done, what is pending (private)
    Journal {
        /// How many days back (by last activity)
        #[arg(long, default_value_t = 7)]
        days: i64,
    },
    /// What the agent did on a day: prompts, replies, tool actions (private)
    Activity {
        /// Day to show, YYYY-MM-DD in local time (default: today)
        #[arg(long)]
        date: Option<chrono::NaiveDate>,
        /// Only projects whose name contains this text
        #[arg(long)]
        project: Option<String>,
        /// Include the agent's thinking
        #[arg(long)]
        thinking: bool,
        /// Show full text instead of one line per entry
        #[arg(long)]
        full: bool,
    },
    /// Token usage parsed from logs (stored in usage.db)
    Usage {
        #[command(subcommand)]
        command: UsageCommands,
    },
    /// List recorded events
    History {
        #[command(flatten)]
        filter: FilterArgs,
        /// Max number of most recent events to show (default: all)
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Show summary: event counts per reader/project
    Summary {
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Manage reader plugins
    Add {
        #[command(subcommand)]
        command: AddCommands,
    },
}

#[derive(Subcommand)]
enum TeamCommands {
    /// Confirm on this machine whether to share prompts, replies and thinking
    /// with a team project (no arguments: list; no on/off: show)
    Share {
        /// Team project, e.g. acme/api
        target: Option<String>,
        #[arg(value_enum, requires = "target")]
        state: Option<Toggle>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Toggle {
    On,
    Off,
}

#[derive(Subcommand)]
enum UsageCommands {
    /// Parse existing log files from the beginning into usage.db
    /// (safe to repeat: records are deduplicated)
    Import {
        /// Reader name (default: first registered reader)
        #[arg(long)]
        reader: Option<String>,
        /// Override the directory to import from
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Show token totals and error counts
    Report {
        /// Group rows by
        #[arg(long, value_enum, default_value_t = commands::usage::By::Model)]
        by: commands::usage::By,
    },
}

#[derive(Subcommand, Debug)]
enum AttestCommands {
    /// Check an attestation's signature with the server's public key
    Verify {
        /// The attestation file (the signed envelope)
        file: PathBuf,
        /// Server whose key signed it (default: the one you are logged in to)
        #[arg(long)]
        server: Option<String>,
    },
}

#[derive(Subcommand)]
enum StandupCommands {
    /// Show, set or remove the chat webhook (kept only on this machine)
    Webhook {
        /// Incoming webhook URL (Slack, Discord, Teams Workflows, or any that takes {"text": …})
        url: Option<String>,
        #[arg(long, conflicts_with = "url")]
        remove: bool,
    },
    /// Send the standup every weekday at a local time while `clioilc watch` runs
    Schedule {
        /// Local time, HH:MM
        at: Option<String>,
        /// Also on Saturday and Sunday
        #[arg(long)]
        weekends: bool,
        /// Stop sending it
        #[arg(long, conflicts_with_all = ["at", "weekends"])]
        off: bool,
    },
}

#[derive(Subcommand)]
enum ServiceCommands {
    /// Install and start the background watcher (requires `clioilc login`)
    Install,
    /// Stop and remove the background watcher
    Uninstall,
    /// Show whether the background watcher is running, and its last log lines
    Status,
}

#[derive(Subcommand)]
enum AddCommands {
    /// Scaffold a new reader plugin package under readers/<name>
    Mod { name: String },
}

#[derive(Args)]
struct FilterArgs {
    /// Filter by reader name
    #[arg(long)]
    reader: Option<String>,
    /// Filter by project slug
    #[arg(long)]
    project: Option<String>,
    /// Filter by path substring
    #[arg(long)]
    path: Option<String>,
}

impl From<FilterArgs> for EventFilter {
    fn from(a: FilterArgs) -> Self {
        Self {
            reader: a.reader,
            project: a.project,
            path: a.path,
        }
    }
}

fn open_store(data_dir: Option<PathBuf>) -> Result<Store> {
    let dir = match data_dir {
        Some(dir) => dir,
        None => home_dir()?.join(".clioilc"),
    };
    Ok(Store::new(&dir))
}

fn main() -> Result<()> {
    let Cli { data_dir, command } = Cli::parse();

    match command {
        Commands::Watch {
            reader,
            path,
            follow,
            sync,
            sync_every,
        } => commands::watch::run(
            &open_store(data_dir)?,
            reader.as_deref(),
            path,
            follow,
            sync.then(|| std::time::Duration::from_secs(sync_every)),
        ),
        Commands::Login {
            server,
            name,
            mode,
            yes,
            no_browser,
        } => commands::login::login(
            &open_store(data_dir)?,
            commands::login::Options {
                server,
                name,
                mode,
                yes,
                no_browser,
            },
        ),
        Commands::Logout => commands::login::logout(&open_store(data_dir)?),
        Commands::Link {
            target,
            path,
            history,
            no_history,
            subfolders,
        } => commands::link::link(
            &open_store(data_dir)?,
            commands::link::Options {
                target,
                path,
                history: if history {
                    Some(true)
                } else if no_history {
                    Some(false)
                } else {
                    None
                },
                subfolders,
            },
        ),
        Commands::Links => commands::link::links(&open_store(data_dir)?),
        Commands::Journal { days } => commands::journal::run(&open_store(data_dir)?, days),
        Commands::Attest {
            command: Some(AttestCommands::Verify { file, server }),
            ..
        } => commands::attest::verify(&open_store(data_dir)?, &file, server),
        Commands::Attest {
            command: None,
            release,
            base,
            workspace,
            output,
            path,
        } => {
            let release =
                release.ok_or_else(|| anyhow::anyhow!("name the release: clioilc attest <tag>"))?;
            commands::attest::run(
                &open_store(data_dir)?,
                &release,
                base,
                workspace,
                output,
                path,
            )
        }
        Commands::Standup {
            command: None,
            days,
            send,
        } => commands::standup::run(&open_store(data_dir)?, days, send),
        Commands::Standup {
            command: Some(StandupCommands::Webhook { url, remove }),
            ..
        } => commands::standup::webhook(&open_store(data_dir)?, url, remove),
        Commands::Standup {
            command: Some(StandupCommands::Schedule { at, weekends, off }),
            ..
        } => commands::standup::schedule(&open_store(data_dir)?, at, off, weekends),
        Commands::Team { command: None } => {
            commands::sync::Syncer::open(&open_store(data_dir)?)?.team()
        }
        Commands::Team {
            command: Some(TeamCommands::Share { target, state }),
        } => commands::team::share(
            &open_store(data_dir)?,
            target.as_deref(),
            state.map(|t| matches!(t, Toggle::On)),
        ),
        Commands::Unlink { path } => commands::link::unlink(&open_store(data_dir)?, path),
        Commands::Sync { resend } => commands::sync::run(&open_store(data_dir)?, resend),
        Commands::Service { command } => {
            let store = open_store(data_dir)?;
            match command {
                ServiceCommands::Install => commands::service::install(&store),
                ServiceCommands::Uninstall => commands::service::uninstall(&store),
                ServiceCommands::Status => commands::service::status(&store),
            }
        }
        Commands::Activity {
            date,
            project,
            thinking,
            full,
        } => commands::activity::run(
            &open_store(data_dir)?,
            commands::activity::Options {
                date,
                project,
                thinking,
                full,
            },
        ),
        Commands::Usage { command } => match command {
            UsageCommands::Import { reader, path } => {
                commands::usage::import(&open_store(data_dir)?, reader.as_deref(), path)
            }
            UsageCommands::Report { by } => commands::usage::report(&open_store(data_dir)?, by),
        },
        Commands::History { filter, limit } => {
            commands::history::run(&open_store(data_dir)?, &filter.into(), limit)
        }
        Commands::Summary { filter } => {
            commands::summary::run(&open_store(data_dir)?, &filter.into())
        }
        Commands::Add {
            command: AddCommands::Mod { name },
        } => commands::add::add_mod(&name),
    }
}
