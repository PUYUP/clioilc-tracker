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
        /// Backend URL, e.g. https://api.example.com (default: $CLIOILC_SERVER
        /// or the server of the previous login)
        #[arg(long)]
        server: Option<String>,
        /// Device name shown in the dashboard (default: hostname)
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
    /// Upload data not yet on the server
    Sync,
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
        Commands::Sync => commands::sync::run(&open_store(data_dir)?),
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
