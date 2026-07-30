use anyhow::Result;
use clap::{Parser, Subcommand};

use claude_account_swapper::{App, DEFAULT_KEEPALIVE_MAX_AGE_DAYS, DEFAULT_WATCH_INTERVAL_SECONDS};

#[derive(Debug, Parser)]
#[command(
    name = "claude-account",
    version,
    about = "Claude Code Accounts speichern und ohne neuen Browser-Login wechseln"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<AccountCommand>,
}

#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// Aktuell autorisierten Claude-Login unter einem Namen speichern
    Save { name: String },
    /// Einmalig im Browser einloggen und den neuen Account speichern
    Login { name: String },
    /// Zu einem gespeicherten Account wechseln
    #[command(alias = "use")]
    Switch { name: String },
    /// Gespeicherte Accounts anzeigen
    List,
    /// Aktiven Claude-Login anzeigen
    Status,
    /// Von Claude rotierte Tokens einmalig ins aktive Profil sichern
    Sync,
    /// Rotierte Tokens dauerhaft ins aktive Profil sichern (Hintergrunddienst)
    Watch {
        /// Pruefintervall in Sekunden
        #[arg(long, default_value_t = DEFAULT_WATCH_INTERVAL_SECONDS)]
        interval: u64,
        /// Untaetige Profile nach so vielen Tagen auffrischen
        #[arg(long, default_value_t = DEFAULT_KEEPALIVE_MAX_AGE_DAYS)]
        keepalive_max_age_days: u64,
        /// Untaetige Profile gar nicht auffrischen
        #[arg(long)]
        no_keepalive: bool,
    },
    /// Untaetige Profile auffrischen, damit ihr Login nicht ablaeuft
    Keepalive {
        /// Erst Profile auffrischen, die so lange nicht gesichert wurden
        #[arg(long, default_value_t = DEFAULT_KEEPALIVE_MAX_AGE_DAYS)]
        max_age_days: u64,
    },
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let app = App::discover()?;
    match cli.command {
        Some(AccountCommand::Save { name }) => app.save(&name),
        Some(AccountCommand::Login { name }) => app.login(&name),
        Some(AccountCommand::Switch { name }) => app.switch(&name),
        Some(AccountCommand::List) => app.list(),
        Some(AccountCommand::Status) => app.status(),
        Some(AccountCommand::Sync) => app.sync().map(|outcome| println!("{outcome}")),
        Some(AccountCommand::Watch {
            interval,
            keepalive_max_age_days,
            no_keepalive,
        }) => app.watch(interval, (!no_keepalive).then_some(keepalive_max_age_days)),
        Some(AccountCommand::Keepalive { max_age_days }) => app.keepalive(max_age_days),
        None => app.interactive(),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Fehler: {error:#}");
        std::process::exit(1);
    }
}
