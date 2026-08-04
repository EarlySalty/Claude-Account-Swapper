use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use claude_account_swapper::jobs::{DEFAULT_RESUME_PROMPT, Job, JobKind};
use claude_account_swapper::usage::DEFAULT_SWITCH_THRESHOLD;
use claude_account_swapper::{
    App, DEFAULT_KEEPALIVE_MAX_AGE_DAYS, DEFAULT_WATCH_INTERVAL_SECONDS, JobOptions,
};

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
    /// Einstellungen der Automatik anzeigen oder aendern
    Config {
        /// Bei vollem Limit selbst den Account wechseln
        #[arg(long)]
        auto_switch: Option<Switch>,
        /// Das Fuenf-Stunden-Fenster nach dem Reset von selbst eroeffnen
        #[arg(long)]
        ping: Option<Switch>,
        /// Text, mit dem das Fenster eroeffnet wird
        #[arg(long)]
        ping_prompt: Option<String>,
        /// Modell fuer den Fenster-Ping
        #[arg(long)]
        ping_model: Option<String>,
        /// Ab dieser Auslastung in Prozent gilt ein Limit als verbraucht
        #[arg(long)]
        threshold: Option<f64>,
    },
    /// Aufgaben anzeigen, die auf ein freies Fenster warten
    Jobs,
    /// Eine Aufgabe anlegen, aendern oder ausfuehren
    Job {
        #[command(subcommand)]
        command: JobCommand,
    },
    /// Das Fuenf-Stunden-Fenster sofort eroeffnen
    Ping,
}

/// Ein Schalter, den man auf der Kommandozeile ohne Nachdenken trifft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Switch {
    On,
    Off,
}

impl Switch {
    fn is_on(self) -> bool {
        self == Self::On
    }
}

#[derive(Debug, Subcommand)]
enum JobCommand {
    /// Auftrag anlegen, der beim naechsten freien Fuenf-Stunden-Fenster startet
    Add {
        /// Was Claude tun soll
        prompt: String,
        /// Arbeitsverzeichnis; ohne Angabe das aktuelle
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Eigene Bezeichnung fuer Listen
        #[arg(long)]
        title: Option<String>,
        /// In jedem neuen Fenster erneut ausfuehren
        #[arg(long)]
        repeat: bool,
        /// Eigene Einstellungsdatei fuer diesen Lauf
        #[arg(long)]
        settings: Option<PathBuf>,
        /// Modell fuer diesen Lauf
        #[arg(long)]
        model: Option<String>,
        /// Zeitlimit des Laufs in Minuten
        #[arg(long)]
        timeout_minutes: Option<u64>,
        /// Berechtigungsfragen zulassen; ohne das laeuft die Aufgabe ohne Rueckfragen
        #[arg(long)]
        allow_permissions: bool,
    },
    /// Eine bestehende Sitzung fortsetzen lassen, sobald wieder Kontingent da ist
    Resume {
        /// Sitzungs-ID; `claude-account job sessions` zeigt die letzten an
        session_id: String,
        /// Womit fortgesetzt wird
        #[arg(long)]
        prompt: Option<String>,
        /// Arbeitsverzeichnis der Sitzung; ohne Angabe aus der Sitzung selbst
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        settings: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        timeout_minutes: Option<u64>,
        #[arg(long)]
        allow_permissions: bool,
    },
    /// Die zuletzt benutzten Claude-Sitzungen anzeigen
    Sessions,
    /// Aufgabe loeschen
    Remove { id: String },
    /// Aufgabe wieder aktivieren
    Enable { id: String },
    /// Aufgabe abschalten, ohne sie zu loeschen
    Disable { id: String },
    /// Aufgabe sofort ausfuehren, unabhaengig vom Fenster
    Run { id: String },
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
        Some(AccountCommand::Config {
            auto_switch,
            ping,
            ping_prompt,
            ping_model,
            threshold,
        }) => {
            let unchanged = auto_switch.is_none()
                && ping.is_none()
                && ping_prompt.is_none()
                && ping_model.is_none()
                && threshold.is_none();
            if !unchanged {
                app.update_config(|config| {
                    if let Some(value) = auto_switch {
                        config.auto_switch = value.is_on();
                    }
                    if let Some(value) = ping {
                        config.ping.enabled = value.is_on();
                    }
                    if let Some(value) = ping_prompt {
                        config.ping.prompt = value;
                    }
                    if let Some(value) = ping_model {
                        config.ping.model = value;
                    }
                    if let Some(value) = threshold {
                        config.threshold = value;
                    }
                })?;
            }
            app.show_config()
        }
        Some(AccountCommand::Jobs) => app.list_jobs(),
        Some(AccountCommand::Job { command }) => run_job_command(&app, command),
        Some(AccountCommand::Ping) => app.ping_now(),
        None => app.interactive(),
    }
}

fn run_job_command(app: &App, command: JobCommand) -> Result<()> {
    match command {
        JobCommand::Add {
            prompt,
            cwd,
            title,
            repeat,
            settings,
            model,
            timeout_minutes,
            allow_permissions,
        } => {
            let cwd = match cwd {
                Some(cwd) => cwd,
                None => std::env::current_dir().context("aktuelles Verzeichnis unbekannt")?,
            };
            let job = app.add_job(
                JobKind::Prompt { text: prompt },
                cwd,
                JobOptions {
                    title,
                    settings,
                    model,
                    timeout_minutes,
                    skip_permissions: !allow_permissions,
                    repeat,
                },
            )?;
            announce(&job);
            Ok(())
        }
        JobCommand::Resume {
            session_id,
            prompt,
            cwd,
            settings,
            model,
            timeout_minutes,
            allow_permissions,
        } => {
            // Ohne Ordnerangabe zaehlt der Ordner der Sitzung selbst: eine Sitzung woanders
            // fortzusetzen waere ein Fehler, den man erst am Ergebnis merkt.
            let cwd = match cwd {
                Some(cwd) => cwd,
                None => app
                    .find_session(&session_id)
                    .map(|session| session.cwd)
                    .with_context(|| {
                        format!(
                            "Sitzung `{session_id}` wurde nicht gefunden; \
                             gib das Arbeitsverzeichnis mit --cwd an"
                        )
                    })?,
            };
            let job = app.add_job(
                JobKind::Resume {
                    session_id,
                    text: prompt.unwrap_or_else(|| DEFAULT_RESUME_PROMPT.to_owned()),
                },
                cwd,
                JobOptions {
                    settings,
                    model,
                    timeout_minutes,
                    skip_permissions: !allow_permissions,
                    ..JobOptions::default()
                },
            )?;
            announce(&job);
            Ok(())
        }
        JobCommand::Sessions => app.list_sessions(),
        JobCommand::Remove { id } => app.remove_job(&id),
        JobCommand::Enable { id } => app.set_job_enabled(&id, true),
        JobCommand::Disable { id } => app.set_job_enabled(&id, false),
        JobCommand::Run { id } => app.run_job_now(&id),
    }
}

fn announce(job: &Job) {
    println!("Angelegt: [{}] {}", job.id, job.title);
    println!("Ordner: {}", job.cwd.display());
    match job.last_window {
        Some(resets_at) => println!(
            "Startet, sobald das laufende Fenster zurueckgesetzt ist (ab {}).",
            resets_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
        ),
        None => println!("Startet beim naechsten Durchlauf des Dienstes."),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Fehler: {error:#}");
        std::process::exit(1);
    }
}
