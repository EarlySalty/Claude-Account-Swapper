use anyhow::Result;
use clap::{Parser, Subcommand};

use claude_account_swapper::usage::DEFAULT_SWITCH_THRESHOLD;
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
    Switch {
        name: String,
        /// Gespeicherten Login nicht vorher ausprobieren
        #[arg(long)]
        no_check: bool,
    },
    /// Gespeicherte Accounts anzeigen
    List,
    /// Nutzungslimits aller gespeicherten Accounts anzeigen
    Usage,
    /// Eigene Nutzungsgrenzen eines Accounts setzen oder loeschen
    Limit {
        name: String,
        /// Grenze fuer das Fuenf-Stunden-Fenster in Prozent
        #[arg(long)]
        five_hour: Option<f64>,
        /// Grenze fuer das Wochenfenster in Prozent
        #[arg(long)]
        seven_day: Option<f64>,
        /// Grenze auch dann einhalten, wenn sonst kein Account mehr frei ist
        #[arg(long, conflicts_with = "soft")]
        hard: bool,
        /// Grenze im Notfall anbrechen duerfen (Standard)
        #[arg(long)]
        soft: bool,
        /// Alle eigenen Grenzen dieses Accounts entfernen
        #[arg(long, conflicts_with_all = ["five_hour", "seven_day", "hard", "soft"])]
        clear: bool,
    },
    /// Bei vollem Limit auf den Account mit den meisten freien Kontingenten wechseln
    Auto {
        /// Ab dieser Auslastung in Prozent gilt ein Limit als verbraucht
        #[arg(long, default_value_t = DEFAULT_SWITCH_THRESHOLD)]
        threshold: f64,
        /// Entscheidung nur anzeigen, nicht wechseln
        #[arg(long)]
        dry_run: bool,
    },
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
        /// Bei vollem Limit selbst den Account wechseln (standardmaessig aus)
        #[arg(long)]
        auto_switch: bool,
        /// Ab dieser Auslastung in Prozent wird gewechselt; wirkt nur mit --auto-switch
        #[arg(long, default_value_t = DEFAULT_SWITCH_THRESHOLD, requires = "auto_switch")]
        auto_switch_threshold: f64,
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
        Some(AccountCommand::Switch { name, no_check }) => app.switch_checked(&name, !no_check),
        Some(AccountCommand::List) => app.list(),
        Some(AccountCommand::Status) => app.status(),
        Some(AccountCommand::Usage) => app.usage(),
        Some(AccountCommand::Limit {
            name,
            five_hour,
            seven_day,
            hard,
            soft,
            clear,
        }) => app.set_limits(
            &name,
            five_hour,
            seven_day,
            // Nur eine ausdrueckliche Angabe aendert die Haerte; sonst bliebe sie bei jedem
            // spaeteren Setzen einer Zahl stillschweigend auf weich zurueckfallen.
            hard.then_some(true).or(soft.then_some(false)),
            clear,
        ),
        Some(AccountCommand::Auto { threshold, dry_run }) => {
            app.auto_switch(threshold, dry_run).map(|_| ())
        }
        Some(AccountCommand::Sync) => app.sync().map(|outcome| println!("{outcome}")),
        Some(AccountCommand::Watch {
            interval,
            keepalive_max_age_days,
            no_keepalive,
            auto_switch,
            auto_switch_threshold,
        }) => app.watch(
            interval,
            (!no_keepalive).then_some(keepalive_max_age_days),
            auto_switch.then_some(auto_switch_threshold),
        ),
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
