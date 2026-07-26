use anyhow::Result;
use clap::{Parser, Subcommand};

use claude_account_swapper::App;

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
        None => app.interactive(),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Fehler: {error:#}");
        std::process::exit(1);
    }
}
