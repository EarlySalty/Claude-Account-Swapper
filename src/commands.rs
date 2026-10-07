use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use crate::App;
use crate::routing::{Rule, Strategy};

#[derive(Debug, Subcommand)]
pub enum ManagedCommand {
    #[command(
        about = "Claude mit einem eigenen Account starten; andere Terminals bleiben unverändert"
    )]
    Run {
        account: Option<String>,
        #[arg(long)]
        strategy: Option<Strategy>,
        #[arg(long, value_delimiter = ',')]
        pool: Vec<String>,
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(
        about = "Account oder Pool einem Projekt zuordnen; ohne Argumente Zuordnungen anzeigen"
    )]
    Map {
        account: Option<String>,
        directory: Option<PathBuf>,
        #[arg(long)]
        strategy: Option<Strategy>,
        #[arg(long, value_delimiter = ',')]
        pool: Vec<String>,
        #[arg(long)]
        model: Option<String>,
    },
    #[command(about = "Projektzuordnung entfernen")]
    Unmap { directory: Option<PathBuf> },
    #[command(about = "Verwaltete Terminal- und T3-Sessions anzeigen")]
    Sessions {
        #[arg(long)]
        json: bool,
    },
    #[command(
        about = "Eine verwaltete Session zuordnen; ohne Account wieder die Projektregel nutzen"
    )]
    Session {
        id: String,
        account: Option<String>,
        #[arg(long)]
        strategy: Option<Strategy>,
        #[arg(long, value_delimiter = ',')]
        pool: Vec<String>,
        #[arg(long)]
        model: Option<String>,
    },
    #[command(about = "Account aus der automatischen Session-Verteilung nehmen")]
    Disable { name: String },
    #[command(about = "Account für die automatische Session-Verteilung freigeben")]
    Enable { name: String },
    #[command(about = "Eigenen offiziellen Claude-Login für parallele Sessions einrichten")]
    SessionLogin { name: String },
    #[command(
        about = "Vorhandene native Claude-Konfiguration mit einem gespeicherten Account verbinden"
    )]
    Bind { name: String, directory: PathBuf },
    #[command(about = "Skills, Einstellungen und Projektverlauf für einen nativen Account teilen")]
    ShareSetup { name: String },
    #[command(about = "Verteilungsstrategie und sichere Wechselgrenzen einstellen")]
    Routing {
        #[arg(long)]
        strategy: Option<Strategy>,
        #[arg(long)]
        auto_switch: Option<bool>,
        #[arg(long)]
        weekly_threshold: Option<f64>,
        #[arg(long)]
        cooldown_seconds: Option<u64>,
        #[arg(long)]
        hysteresis: Option<f64>,
    },
    #[command(about = "Kleine lokale HTML-Oberfläche starten")]
    Dashboard {
        #[arg(long, default_value_t = 3775)]
        port: u16,
        #[arg(long)]
        allow_host: Vec<String>,
    },
    #[command(about = "Account- und Session-Zustand als JSON ausgeben")]
    Monitor,
    #[command(about = "Native Logins sicher auffrischen und Auslastungen abfragen")]
    NativeRefresh,
    #[command(about = "Stream-JSON-Launcher für T3 Code")]
    T3 {
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

impl ManagedCommand {
    pub fn execute(self, app: &App) -> Result<()> {
        match self {
            Self::Run {
                account,
                strategy,
                pool,
                args,
            } => {
                let explicit = if account.is_some() || strategy.is_some() || !pool.is_empty() {
                    Some(Rule {
                        account: account.filter(|name| name != "auto"),
                        strategy,
                        pool,
                        model: None,
                    })
                } else {
                    None
                };
                if let Some(rule) = &explicit {
                    crate::routing::validate_rule(rule)?;
                }
                app.maintain_native()?;
                std::process::exit(app.run_account(explicit, args)?);
            }
            Self::Map {
                account,
                directory,
                strategy,
                pool,
                model,
            } => {
                if account.is_none()
                    && directory.is_none()
                    && strategy.is_none()
                    && pool.is_empty()
                    && model.is_none()
                {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&app.routing_config()?.routing.projects)?
                    );
                    return Ok(());
                }
                let directory = directory.unwrap_or(std::env::current_dir()?);
                app.map_project(
                    &directory,
                    Some(Rule {
                        account: account.filter(|name| name != "auto"),
                        strategy,
                        pool,
                        model,
                    }),
                )
            }
            Self::Unmap { directory } => {
                app.map_project(&directory.unwrap_or(std::env::current_dir()?), None)
            }
            Self::Sessions { json } => {
                let sessions = app.managed_sessions()?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&sessions)?);
                } else {
                    for session in sessions {
                        println!(
                            "{}  {:12} {:12} {}",
                            session.id,
                            session.state,
                            session.account.as_deref().unwrap_or("-"),
                            session.cwd.display()
                        );
                    }
                }
                Ok(())
            }
            Self::Session {
                id,
                account,
                strategy,
                pool,
                model,
            } => {
                let rule = if account.is_none()
                    && strategy.is_none()
                    && pool.is_empty()
                    && model.is_none()
                {
                    None
                } else {
                    Some(Rule {
                        account: account.filter(|name| name != "auto"),
                        strategy,
                        pool,
                        model,
                    })
                };
                app.assign_session(&id, rule)
            }
            Self::Disable { name } => app.set_account_enabled(&name, false),
            Self::Enable { name } => app.set_account_enabled(&name, true),
            Self::SessionLogin { name } => app.native_login(&name),
            Self::Bind { name, directory } => app.bind_native(&name, &directory),
            Self::ShareSetup { name } => app.share_native_setup(&name),
            Self::Routing {
                strategy,
                auto_switch,
                weekly_threshold,
                cooldown_seconds,
                hysteresis,
            } => {
                if strategy.is_some()
                    || auto_switch.is_some()
                    || weekly_threshold.is_some()
                    || cooldown_seconds.is_some()
                    || hysteresis.is_some()
                {
                    app.update_config(|config| {
                        if let Some(value) = strategy {
                            config.routing.strategy = value;
                        }
                        if let Some(value) = auto_switch {
                            config.routing.auto_switch = value;
                        }
                        if let Some(value) = weekly_threshold {
                            config.routing.weekly_threshold = value;
                        }
                        if let Some(value) = cooldown_seconds {
                            config.routing.cooldown_seconds = value;
                        }
                        if let Some(value) = hysteresis {
                            config.routing.hysteresis = value;
                        }
                    })?;
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&app.routing_config()?.routing)?
                );
                Ok(())
            }
            Self::Dashboard { port, allow_host } => crate::dashboard::serve(app, port, allow_host),
            Self::Monitor => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&app.orchestration_state()?)?
                );
                Ok(())
            }
            Self::NativeRefresh => app.maintain_native().and_then(|()| app.poll_accounts()),
            Self::T3 { args } => std::process::exit(crate::bridge::run(app, args)?),
        }
    }
}
