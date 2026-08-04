use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use wait_timeout::ChildExt;

pub mod config;
pub mod jobs;
pub mod usage;

use config::Config;
use jobs::{Job, JobKind, Readiness};
use usage::{CachedUsage, Candidate, Decision, Stops, Usage};

const MAX_CREDENTIAL_SIZE: u64 = 1024 * 1024;
/// Ab dieser Restlaufzeit des Refresh-Tokens warnt `list`.
const REFRESH_TOKEN_WARN_SECONDS: i64 = 7 * 24 * 60 * 60;
pub const DEFAULT_WATCH_INTERVAL_SECONDS: u64 = 5;
/// Nach so vielen Tagen ohne Nutzung frischt `keepalive` ein Profil auf. Der Refresh-Token
/// selbst gilt rund 30 Tage; sieben Tage lassen genug Luft fuer ausgefallene Durchlaeufe.
pub const DEFAULT_KEEPALIVE_MAX_AGE_DAYS: u64 = 7;
/// Abstand zwischen zwei Auffrischungen im Dauerbetrieb.
const KEEPALIVE_INTERVAL_SECONDS: u64 = 12 * 60 * 60;
/// Abstand zwischen zwei Limitpruefungen im Dauerbetrieb. Die Abfrage kostet kein Kontingent,
/// eine Minute reicht aber: zwischen 98% und dem harten Limit liegt mehr als ein Request.
const USAGE_CHECK_INTERVAL_SECONDS: u64 = 60;
/// Bis zu diesem Alter gilt eine gemerkte Auslastung als aktuell und wird ohne neue Anfrage
/// benutzt. Knapp unter dem Pruefintervall, damit der Dienst nicht seinen eigenen Wert
/// wiederverwendet, ein zusaetzlicher Aufruf von Hand aber ohne Anfrage auskommt.
const USAGE_CACHE_FRESH_SECONDS: u64 = 45;
/// Darueber hinaus ist ein gemerkter Stand nur noch Notnagel, wenn die Anfrage scheitert.
/// Ein Fuenf-Stunden-Fenster bewegt sich in 30 Minuten nicht so weit, dass die Entscheidung
/// dadurch unbrauchbar wuerde - eine ausgefallene Bewertung dagegen schon.
const USAGE_CACHE_MAX_AGE_SECONDS: u64 = 30 * 60;
/// Ein haengender Request wuerde den Dienst sonst dauerhaft anhalten.
const KEEPALIVE_REQUEST_TIMEOUT_SECONDS: u64 = 120;
/// Restlaufzeit, ab der ein Access-Token vor einem Wechsel als sicher gilt. Darunter muesste
/// Claude Code refreshen, und genau dabei kann ein verbrauchter Login auffliegen.
const ACCESS_TOKEN_MIN_REMAINING_SECONDS: i64 = 5 * 60;
/// Genug fuer einen ganzen Satz Fehlermeldung, zu wenig fuer eine gekippte Ausgabe.
const STDERR_REASON_MAX_CHARS: usize = 300;
/// Ab dieser Laenge ist ein zusammenhaengendes Wort kein Satzteil mehr, sondern ein Token.
const SECRET_WORD_MIN_CHARS: usize = 40;
/// So viele Zeilen einer Sitzungsdatei reichen, um Arbeitsverzeichnis und erste Frage zu finden.
/// Die Dateien werden megabytegross; sie fuer eine Auswahlliste ganz zu lesen waere Verschwendung.
const SESSION_SCAN_LINES: usize = 60;
/// So viele Sitzungen zeigt die Auswahl an.
const SESSION_LIST_LIMIT: usize = 15;

#[derive(Debug, Clone)]
pub struct Paths {
    pub credentials: PathBuf,
    /// Claude Codes Konfigurationsdatei; enthaelt den Identitaets-Cache zum aktiven Login.
    pub claude_json: PathBuf,
    pub store: PathBuf,
    pub claude_bin: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .context("HOME ist nicht gesetzt")?;
        let config_dir = env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        // Ohne CLAUDE_CONFIG_DIR liegt `.credentials.json` in `~/.claude`, `.claude.json`
        // dagegen direkt im Home; mit gesetzter Variable liegen beide in diesem Verzeichnis.
        let claude_json = config_dir
            .clone()
            .unwrap_or_else(|| home.clone())
            .join(".claude.json");
        let claude_config = config_dir.unwrap_or_else(|| home.join(".claude"));
        let store = env::var_os("CLAUDE_ACCOUNT_SWITCHER_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude-account-switcher"));
        let claude_bin = env::var_os("CLAUDE_ACCOUNT_SWITCHER_CLAUDE_BIN")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("claude"));

        Ok(Self {
            credentials: claude_config.join(".credentials.json"),
            claude_json,
            store,
            claude_bin,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Profile {
    name: String,
    email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    org_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    org_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subscription_type: Option<String>,
    saved_at: u64,
    /// sha256 des zuletzt gesicherten Refresh-Tokens. Nie der Token selbst: der Hash reicht,
    /// um eine Rotation zu erkennen, und laesst sich nicht zurueckrechnen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_fingerprint: Option<String>,
    /// Unix-Sekunden des letzten Snapshot-Schreibens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credentials_synced_at: Option<u64>,
    /// Unix-Sekunden des letzten Versuchs, den Login zu erneuern, bei dem Claude ihn geleert hat.
    /// Der Ablauf im Snapshot sagt darueber nichts: ein Refresh-Token kann laut Datei noch
    /// wochenlang gelten und trotzdem verbraucht sein. Nur der gescheiterte Versuch beweist es.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    login_failed_at: Option<u64>,
    /// Eigene Nutzungsgrenzen dieses Accounts. Fehlen sie, gilt die globale Schwelle.
    #[serde(default)]
    limits: Stops,
}

/// Woher die Zuordnung "aktiver Login gehoert zu Profil X" stammen darf.
///
/// Ein Mensch, der `sync` tippt, weiss was er gerade gewechselt hat; fuer ihn genuegt der
/// Switcher-Status als Rueckfallebene. Der Hintergrunddienst bekommt diese Rueckfallebene
/// bewusst nicht: er liefe sonst genau in den Fall, den er verhindern soll, naemlich eine
/// fremde Session, die die Live-Datei ueberschreibt, waehrend Claudes Identitaets-Cache
/// direkt nach einem Wechsel noch leer ist. Er wartet lieber, bis Claude die Identitaet kennt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentitySource {
    AnyIncludingSwitcherState,
    ClaudeOnly,
}

/// Ergebnis einer Auffrischung eines untaetigen Profils.
#[derive(Debug)]
enum RenewOutcome {
    Renewed,
    StillValid,
    Expired,
    /// Der Request ist gescheitert und hat nichts erneuert. Das Profil bleibt unangetastet -
    /// vor allem gilt es nicht als geprueft, sonst laeuft seine echte Frist still weiter.
    RequestFailed(String),
}

/// Ergebnis einer Auffrischung samt der Credentials, die danach gelten. `RenewOutcome` sagt nur,
/// was passiert ist; der Wechsel braucht zusaetzlich den Stand selbst, denn nach einer Rotation
/// ist genau dieser Stand der einzig gueltige.
enum RenewResult {
    Renewed(Vec<u8>),
    StillValid(Vec<u8>),
    Expired,
    RequestFailed(String),
}

/// Ob eine Hilfsfunktion das Switcher-Lock selbst nehmen muss oder der Aufrufer es schon haelt.
/// Das Lock ist nicht reentrant: ein zweiter Versuch aus demselben Prozess blockiert fuer immer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Locking {
    Acquire,
    AlreadyHeld,
}

/// Was ein Auffrischungsversuch hinterlassen hat.
struct RenewalAttempt {
    /// `None` heisst: Claude hat den Login geleert, er ist nicht mehr zu erneuern.
    credentials: Option<Vec<u8>>,
    /// Gefuellt, wenn der Aufruf selbst schiefging.
    failure: Option<String>,
}

/// Ergebnis eines Sync-Durchlaufs. Auch der Fall "nichts zu tun" ist ein Ergebnis und
/// wird protokolliert - sonst sieht ein stiller Fehlschlag wie Erfolg aus.
#[derive(Debug)]
pub enum SyncOutcome {
    Updated { profile: String, email: String },
    Unchanged { profile: String },
}

impl std::fmt::Display for SyncOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Updated { profile, email } => {
                write!(formatter, "Aktualisiert: {profile} ({email})")
            }
            Self::Unchanged { profile } => write!(formatter, "Bereits aktuell: {profile}"),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    current: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthStatus {
    logged_in: bool,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    org_id: Option<String>,
    #[serde(default)]
    org_name: Option<String>,
    #[serde(default)]
    subscription_type: Option<String>,
}

impl AuthStatus {
    /// Claude kennt die Kontodaten zum aktiven Login. Direkt nach einem Wechsel ist das
    /// erst wieder der Fall, sobald Claude Code einmal gelaufen ist.
    fn has_identity(&self) -> bool {
        self.email
            .as_deref()
            .is_some_and(|email| !email.trim().is_empty())
    }

    fn email(&self) -> Result<&str> {
        if !self.logged_in {
            bail!("Claude Code ist nicht eingeloggt; nutze `claude-account login <name>`");
        }
        self.email
            .as_deref()
            .filter(|email| !email.trim().is_empty())
            .context("Claude kennt die Kontodaten zum aktiven Login noch nicht; starte einmal Claude Code")
    }

    fn matches(&self, profile: &Profile) -> bool {
        if self.email.as_deref() != Some(profile.email.as_str()) {
            return false;
        }
        match (&self.org_id, &profile.org_id) {
            (Some(active), Some(saved)) => active == saved,
            _ => true,
        }
    }
}

pub struct App {
    paths: Paths,
}

impl App {
    pub fn discover() -> Result<Self> {
        Ok(Self {
            paths: Paths::discover()?,
        })
    }

    pub fn save(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        self.ensure_no_auth_override()?;
        let _lock = self.lock()?;
        let profile = self.save_live_as(name)?;
        println!("Gespeichert: {} ({})", profile.name, profile.email);
        Ok(())
    }

    pub fn login(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        self.ensure_no_auth_override()?;
        let _lock = self.lock()?;

        let previous_credentials = if self.paths.credentials.is_file() {
            let status = self.auth_status()?;
            let credentials = read_credential_bytes(&self.paths.credentials)?;
            if status.logged_in {
                validate_credentials(&credentials)?;
                self.sync_known_active_with_credentials(&status, &credentials)?;
            }
            Some(credentials)
        } else {
            None
        };
        let previous_state = self.load_state()?;

        let login_status = Command::new(&self.paths.claude_bin)
            .args(["auth", "login", "--claudeai"])
            .status()
            .with_context(|| {
                format!(
                    "Claude Login konnte nicht gestartet werden: {}",
                    self.paths.claude_bin.display()
                )
            })?;
        if !login_status.success() {
            self.restore_live(previous_credentials.as_deref(), &previous_state)?;
            bail!("Claude Login wurde abgebrochen oder ist fehlgeschlagen");
        }

        match self.save_live_as(name) {
            Ok(profile) => {
                println!(
                    "Gespeichert und aktiv: {} ({})",
                    profile.name, profile.email
                );
                Ok(())
            }
            Err(error) => {
                self.restore_live(previous_credentials.as_deref(), &previous_state)?;
                Err(error).context(
                    "neuer Login wurde nicht gespeichert; vorheriger Login ist wieder aktiv",
                )
            }
        }
    }

    pub fn switch(&self, name: &str) -> Result<()> {
        self.switch_checked(name, true)
    }

    /// Wechselt zum gespeicherten Account.
    ///
    /// Jede laufende Claude-Session liest `.credentials.json` bei ihrer naechsten Anfrage neu.
    /// Das ist der Sinn des Wechsels - und zugleich seine Gefahr: liegt dort ein verbrauchter
    /// Login, laufen alle offenen Sessions und IDE-Integrationen sofort in `401 OAuth access
    /// token has expired`. Ein Snapshot, dessen Access-Token noch gilt, traegt garantiert; einer,
    /// der erst refreshen muss, kann serverseitig laengst tot sein. Genau dieser Fall wird vorher
    /// in einem eigenen Konfigurationsverzeichnis ausprobiert, wo ein Fehlschlag niemanden trifft.
    pub fn switch_checked(&self, name: &str, check: bool) -> Result<()> {
        validate_name(name)?;
        self.ensure_no_auth_override()?;
        let _lock = self.lock()?;

        let target = self.load_profile(name)?;
        let mut target_credentials = read_valid_credentials(&self.profile_credentials(name))
            .with_context(|| {
                format!("gespeicherte Credentials fuer Profil `{name}` sind ungueltig")
            })?;
        let active_status = self.auth_status()?;
        let mut outgoing_credentials = read_valid_credentials(&self.paths.credentials)?;
        let mut outgoing =
            self.sync_known_active_with_credentials(&active_status, &outgoing_credentials)?;

        if active_status.matches(&target) {
            self.write_state(&State {
                current: Some(target.name.clone()),
            })?;
            println!("Bereits aktiv: {} ({})", target.name, target.email);
            return Ok(());
        }

        let latest_outgoing = read_valid_credentials(&self.paths.credentials)?;
        if latest_outgoing != outgoing_credentials {
            outgoing = self.sync_known_active_with_credentials(&active_status, &latest_outgoing)?;
            outgoing_credentials = latest_outgoing;
        }

        // Erst hier steht fest, dass wirklich gewechselt wird. Frueher geprueft, wuerde ein
        // `switch <aktives Profil>` den Login rotieren, der gerade live benutzt wird, und der
        // "Bereits aktiv"-Zweig kaeme zurueck, ohne den erneuerten Stand einzusetzen - alle
        // offenen Sessions liefen dann in genau den 401, den die Pruefung verhindern soll.
        // Der Byte-Vergleich haelt denselben Fall auch dann ab, wenn Claudes Identitaet und
        // der Switcher-Status auseinanderlaufen: identische Bytes heissen, der Login ist der
        // live benutzte, und der beweist seine Gueltigkeit gerade selbst.
        if check && target_credentials != outgoing_credentials && needs_refresh(&target_credentials)
        {
            target_credentials = self.verify_target_login(&target)?;
        }

        atomic_write(&self.paths.credentials, &target_credentials, 0o600)
            .context("aktive Claude-Credentials konnten nicht ersetzt werden")?;
        let switched = self.clear_cached_identity().and_then(|()| {
            self.write_state(&State {
                current: Some(target.name.clone()),
            })
        });
        if let Err(error) = switched {
            atomic_write(&self.paths.credentials, &outgoing_credentials, 0o600)
                .context("Rollback der aktiven Claude-Credentials ist fehlgeschlagen")?;
            return Err(error).context("Wechsel wurde zurueckgerollt");
        }

        println!(
            "Gewechselt: {} ({}) -> {} ({})",
            outgoing.name, outgoing.email, target.name, target.email
        );
        Ok(())
    }

    /// Probiert den Login eines Profils aus, bevor er live gesetzt wird, und liefert den Stand,
    /// der danach gilt. Ein dabei rotierter Token ist der einzig gueltige und wird zugleich im
    /// Profil gesichert; ohne das waere er nach dem naechsten Refresh unwiederbringlich weg.
    fn verify_target_login(&self, target: &Profile) -> Result<Vec<u8>> {
        println!("Pruefe gespeicherten Login von {} ...", target.name);
        match self.renew_snapshot(target, Locking::AlreadyHeld)? {
            RenewResult::Renewed(credentials) | RenewResult::StillValid(credentials) => {
                Ok(credentials)
            }
            RenewResult::Expired => bail!(
                "der gespeicherte Login von `{}` ({}) ist verbraucht; der Wechsel wurde \
                 abgebrochen, der aktive Account laeuft weiter. Neu anmelden mit \
                 `claude-account login {}`",
                target.name,
                target.email,
                target.name
            ),
            RenewResult::RequestFailed(reason) => bail!(
                "der gespeicherte Login von `{}` liess sich nicht pruefen: {reason}. \
                 Der Wechsel wurde abgebrochen, der aktive Account laeuft weiter; \
                 mit `claude-account switch {} --no-check` trotzdem wechseln",
                target.name,
                target.name
            ),
        }
    }

    pub fn list(&self) -> Result<()> {
        let profiles = self.load_profiles()?;
        if profiles.is_empty() {
            println!("Keine Accounts gespeichert. Starte mit: claude-account save <name>");
            return Ok(());
        }
        let current = self.load_state()?.current;
        for profile in profiles {
            let marker = if current.as_deref() == Some(profile.name.as_str()) {
                "*"
            } else {
                " "
            };
            let plan = profile
                .subscription_type
                .as_deref()
                .map(|value| format!(" [{value}]"))
                .unwrap_or_default();
            println!(
                "{marker} {} - {}{plan}{}",
                profile.name,
                profile.email,
                describe_stops(&profile.limits)
            );
            for line in self.snapshot_report(&profile) {
                println!("    {line}");
            }
        }
        Ok(())
    }

    /// Beschreibt den gespeicherten Snapshot eines Profils: wann er zuletzt nachgezogen wurde
    /// und wie lange sein Refresh-Token noch gilt. Ein unlesbarer Snapshot wird gemeldet, nicht
    /// verschluckt - sonst faellt er erst beim naechsten Wechsel auf.
    fn snapshot_report(&self, profile: &Profile) -> Vec<String> {
        let mut lines = Vec::new();
        // Ein gescheiterter Versuch schlaegt jeden Ablauf aus der Datei: der Refresh-Token kann
        // dort noch wochenlang gelten und trotzdem verbraucht sein.
        if let Some(failed_at) = profile.login_failed_at {
            lines.push(format!(
                "Snapshot: braucht einen neuen Login (Pruefung am {} fehlgeschlagen) - \
                 `claude-account login {}`",
                format_timestamp(failed_at as i64),
                profile.name
            ));
            return lines;
        }
        let synced = profile
            .credentials_synced_at
            .map(|value| format_timestamp(value as i64))
            .unwrap_or_else(|| {
                "unbekannt (vor der automatischen Sicherung gespeichert)".to_owned()
            });

        let expiry = match read_refresh_token_expiry(&self.profile_credentials(&profile.name)) {
            Ok(expiry) => expiry,
            Err(error) => {
                lines.push(format!("Snapshot: {synced}"));
                lines.push(format!("WARNUNG: Snapshot unlesbar: {error:#}"));
                return lines;
            }
        };

        match expiry {
            None => {
                lines.push(format!(
                    "Snapshot: {synced} | Refresh-Token: Ablauf unbekannt"
                ));
            }
            Some(expires_at) => {
                lines.push(format!(
                    "Snapshot: {synced} | Refresh-Token gueltig bis {}",
                    format_timestamp(expires_at)
                ));
                let remaining = expires_at - unix_timestamp().map(|now| now as i64).unwrap_or(0);
                if remaining <= 0 {
                    lines.push(
                        "WARNUNG: Refresh-Token ist abgelaufen - dieses Profil braucht einen neuen Login"
                            .to_owned(),
                    );
                } else if remaining < REFRESH_TOKEN_WARN_SECONDS {
                    lines.push(format!(
                        "WARNUNG: Refresh-Token laeuft in {} Tagen ab - Profil einmal aktivieren",
                        remaining / (24 * 60 * 60)
                    ));
                }
            }
        }
        lines
    }

    pub fn status(&self) -> Result<()> {
        self.ensure_no_auth_override()?;
        let status = self.auth_status()?;
        if !status.logged_in {
            bail!("Claude Code ist nicht eingeloggt; nutze `claude-account login <name>`");
        }
        let (profile_name, profile_email, hint) = match self.resolve_active_profile(&status) {
            Ok(profile) => (profile.name, Some(profile.email), None),
            Err(error) => (
                "nicht zuzuordnen".to_owned(),
                None,
                Some(format!("{error:#}")),
            ),
        };
        let email = status
            .email
            .as_deref()
            .or(profile_email.as_deref())
            .unwrap_or("von Claude noch nicht geladen");
        let plan = status.subscription_type.as_deref().unwrap_or("unbekannt");
        println!("Aktiver Account: {email}");
        println!("Profil: {profile_name}");
        println!("Abo: {plan}");
        if let Some(org_name) = status.org_name.as_deref() {
            println!("Organisation: {org_name}");
        }
        if let Some(hint) = hint {
            println!("Hinweis: {hint}");
        }
        Ok(())
    }

    pub fn interactive(&self) -> Result<()> {
        loop {
            let profiles = match self.load_profiles() {
                Ok(profiles) => profiles,
                Err(error) => {
                    eprintln!(
                        "Warnung: Gespeicherte Accounts konnten nicht gelesen werden: {error:#}"
                    );
                    Vec::new()
                }
            };
            let current = match self.load_state() {
                Ok(state) => state.current,
                Err(error) => {
                    eprintln!("Warnung: Letzter Account-Status ist beschaedigt: {error:#}");
                    None
                }
            };
            let active = self.menu_active_label(&profiles, current.as_deref());

            println!("\n================================");
            println!("  Claude Account Swapper");
            println!("================================");
            println!("Aktiv: {active}\n");
            println!("  [1] Account wechseln");
            println!("  [2] Aktuellen Account speichern");
            println!("  [3] Neuen Account anmelden");
            println!("  [4] Status anzeigen");
            println!("  [5] Auslastung aller Accounts");
            println!("  [6] Grenzen eines Accounts");
            println!("  [7] Automatik und Aufgaben");
            println!("  [8] Beenden\n");

            let Some(choice) = prompt_line("Auswahl: ")? else {
                return Ok(());
            };
            let action = match choice.as_str() {
                "1" => self.interactive_switch(),
                "2" => self.interactive_save(),
                "3" => self.interactive_login(),
                "4" => self.status(),
                "5" => self.usage(),
                "6" => self.interactive_limits(),
                "7" => self.interactive_automation(),
                "8" | "q" | "quit" | "exit" => return Ok(()),
                _ => {
                    eprintln!("Fehler: Bitte 1 bis 8 waehlen.");
                    if !pause()? {
                        return Ok(());
                    }
                    continue;
                }
            };
            if let Err(error) = action {
                eprintln!("Fehler: {error:#}");
            }
            if !pause()? {
                return Ok(());
            }
        }
    }

    fn menu_active_label(&self, profiles: &[Profile], fallback: Option<&str>) -> String {
        let live = self
            .ensure_no_auth_override()
            .and_then(|()| self.auth_status());
        match live {
            Ok(status) if !status.logged_in => "nicht eingeloggt".to_owned(),
            Ok(status) => match self.resolve_active_profile(&status) {
                Ok(profile) => format!("{} ({})", profile.name, profile.email),
                Err(_) => status
                    .email
                    .as_deref()
                    .map(|email| format!("{email} (nicht gespeichert)"))
                    .unwrap_or_else(|| "nicht ermittelbar".to_owned()),
            },
            Err(_) => fallback
                .and_then(|name| profiles.iter().find(|profile| profile.name == name))
                .map(|profile| {
                    format!(
                        "{} ({}, Status nicht pruefbar)",
                        profile.name, profile.email
                    )
                })
                .unwrap_or_else(|| "nicht ermittelbar".to_owned()),
        }
    }

    fn interactive_switch(&self) -> Result<()> {
        let profiles = self.load_profiles()?;
        if profiles.is_empty() {
            bail!("Noch keine Accounts gespeichert. Nutze zuerst Menuepunkt 2.");
        }
        let current = self.load_state()?.current;
        println!("\nGespeicherte Accounts:");
        for (index, profile) in profiles.iter().enumerate() {
            let marker = if current.as_deref() == Some(profile.name.as_str()) {
                " *"
            } else {
                ""
            };
            println!(
                "  [{}]{} {} ({})",
                index + 1,
                marker,
                profile.name,
                profile.email
            );
        }
        println!("  [0] Zurueck");
        let Some(choice) = prompt_line("Account: ")? else {
            return Ok(());
        };
        if choice == "0" {
            return Ok(());
        }
        let selected = choice
            .parse::<usize>()
            .context("bitte eine Account-Nummer eingeben")?;
        let profile = profiles
            .get(
                selected
                    .checked_sub(1)
                    .context("ungueltige Account-Nummer")?,
            )
            .context("ungueltige Account-Nummer")?;
        self.switch(&profile.name)
    }

    fn interactive_save(&self) -> Result<()> {
        let Some(name) = prompt_line("Profilname, z.B. privat: ")? else {
            return Ok(());
        };
        if name.is_empty() {
            bail!("Profilname darf nicht leer sein");
        }
        self.save(&name)
    }

    fn interactive_login(&self) -> Result<()> {
        let Some(name) = prompt_line("Profilname fuer den neuen Account: ")? else {
            return Ok(());
        };
        if name.is_empty() {
            bail!("Profilname darf nicht leer sein");
        }
        println!("\nDer offizielle Claude-Login wird jetzt geoeffnet ...\n");
        self.login(&name)
    }

    fn interactive_limits(&self) -> Result<()> {
        let profiles = self.load_profiles()?;
        if profiles.is_empty() {
            bail!("Noch keine Accounts gespeichert. Nutze zuerst Menuepunkt 2.");
        }
        println!("\nGespeicherte Accounts:");
        for (index, profile) in profiles.iter().enumerate() {
            println!(
                "  [{}] {} ({}){}",
                index + 1,
                profile.name,
                profile.email,
                describe_stops(&profile.limits)
            );
        }
        println!("  [0] Zurueck");
        let Some(choice) = prompt_line("Account: ")? else {
            return Ok(());
        };
        if choice == "0" {
            return Ok(());
        }
        let profile = profiles
            .get(
                choice
                    .parse::<usize>()
                    .context("bitte eine Account-Nummer eingeben")?
                    .checked_sub(1)
                    .context("ungueltige Account-Nummer")?,
            )
            .context("ungueltige Account-Nummer")?;

        println!("\nEine eigene Grenze verlaesst den Account frueher als die globale Schwelle.");
        println!("Leer laesst eine Grenze unveraendert, ein Minus (-) loescht alle Grenzen.");
        let five_hour = prompt_line("Grenze 5h in Prozent: ")?.unwrap_or_default();
        if five_hour.trim() == "-" {
            return self.set_limits(&profile.name, None, None, None, true);
        }
        let seven_day = prompt_line("Grenze 7d in Prozent: ")?.unwrap_or_default();
        let hard = match prompt_line("Grenze hart einhalten? [j/n, leer = unveraendert]: ")?
            .unwrap_or_default()
            .to_lowercase()
        {
            answer if answer.is_empty() => None,
            answer => Some(matches!(answer.as_str(), "j" | "ja" | "y" | "yes")),
        };
        self.set_limits(
            &profile.name,
            parse_percent(&five_hour)?,
            parse_percent(&seven_day)?,
            hard,
            false,
        )
    }

    fn interactive_automation(&self) -> Result<()> {
        loop {
            let config = self.load_config();
            let jobs = self.load_jobs().unwrap_or_default();
            println!("\n--------------------------------");
            println!("  Automatik und Aufgaben");
            println!("--------------------------------");
            println!(
                "Auto-Wechsel bei vollem Limit: {}",
                onoff(config.auto_switch)
            );
            println!(
                "Fenster-Ping nach dem Reset:   {}",
                onoff(config.ping.enabled)
            );
            println!(
                "Aufgaben in der Warteschlange: {}\n",
                jobs.iter().filter(|job| job.enabled).count()
            );
            println!("  [1] Auto-Wechsel umschalten");
            println!("  [2] Fenster-Ping umschalten");
            println!("  [3] Aufgabe anlegen");
            println!("  [4] Sitzung fortsetzen lassen");
            println!("  [5] Aufgaben anzeigen und loeschen");
            println!("  [0] Zurueck\n");

            let Some(choice) = prompt_line("Auswahl: ")? else {
                return Ok(());
            };
            let action = match choice.as_str() {
                "1" => self.toggle_auto_switch(),
                "2" => self.toggle_ping(),
                "3" => self.interactive_add_job(),
                "4" => self.interactive_resume_session(),
                "5" => self.interactive_job_list(),
                "0" | "q" | "quit" | "exit" => return Ok(()),
                _ => {
                    eprintln!("Fehler: Bitte 0 bis 5 waehlen.");
                    Ok(())
                }
            };
            if let Err(error) = action {
                eprintln!("Fehler: {error:#}");
            }
            if !pause()? {
                return Ok(());
            }
        }
    }

    fn toggle_auto_switch(&self) -> Result<()> {
        let config = self.update_config(|config| config.auto_switch = !config.auto_switch)?;
        println!(
            "Auto-Wechsel bei vollem Limit ist jetzt {}.",
            onoff(config.auto_switch)
        );
        if config.auto_switch {
            println!(
                "Der Dienst wechselt ab {:.0}% Auslastung auf den freiesten gespeicherten Account.",
                config.threshold
            );
        }
        Ok(())
    }

    fn toggle_ping(&self) -> Result<()> {
        let config = self.update_config(|config| config.ping.enabled = !config.ping.enabled)?;
        println!(
            "Fenster-Ping nach dem Reset ist jetzt {}.",
            onoff(config.ping.enabled)
        );
        if config.ping.enabled {
            println!(
                "Sobald das Fuenf-Stunden-Fenster zurueckgesetzt ist, schickt der Dienst von \
                 selbst \"{}\" los. Damit laufen die fuenf Stunden ab dem fruehestmoeglichen \
                 Zeitpunkt, ohne dass du etwas tippen musst.",
                config.ping.prompt
            );
        }
        Ok(())
    }

    fn interactive_add_job(&self) -> Result<()> {
        let Some(text) = prompt_line("\nAuftrag (was soll Claude tun?): ")? else {
            return Ok(());
        };
        if text.trim().is_empty() {
            bail!("Der Auftrag darf nicht leer sein");
        }
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_owned());
        let cwd = prompt_line(&format!("Arbeitsverzeichnis [{home}]: "))?.unwrap_or_default();
        let cwd = if cwd.trim().is_empty() { home } else { cwd };
        let repeat = matches!(
            prompt_line("In jedem neuen Fenster wiederholen? [j/N]: ")?
                .unwrap_or_default()
                .to_lowercase()
                .as_str(),
            "j" | "ja" | "y" | "yes"
        );
        let job = self.add_job(
            JobKind::Prompt { text },
            PathBuf::from(cwd),
            JobOptions {
                repeat,
                ..JobOptions::default()
            },
        )?;
        self.announce_job(&job);
        Ok(())
    }

    fn interactive_resume_session(&self) -> Result<()> {
        let sessions = self.recent_sessions(SESSION_LIST_LIMIT);
        if sessions.is_empty() {
            bail!(
                "Keine Sitzungen gefunden in {}",
                self.sessions_dir().display()
            );
        }
        println!("\nZuletzt benutzte Sitzungen:");
        for (index, session) in sessions.iter().enumerate() {
            println!(
                "  [{}] {} - {}",
                index + 1,
                format_timestamp(session.modified as i64),
                session.title
            );
            println!("      {}", session.cwd.display());
        }
        println!("  [0] Zurueck");
        let Some(choice) = prompt_line("Sitzung: ")? else {
            return Ok(());
        };
        if choice == "0" {
            return Ok(());
        }
        let session = sessions
            .get(
                choice
                    .parse::<usize>()
                    .context("bitte eine Sitzungsnummer eingeben")?
                    .checked_sub(1)
                    .context("ungueltige Sitzungsnummer")?,
            )
            .context("ungueltige Sitzungsnummer")?;

        let text = prompt_line(&format!(
            "Weiter-Auftrag [{}]: ",
            jobs::DEFAULT_RESUME_PROMPT
        ))?
        .unwrap_or_default();
        let text = if text.trim().is_empty() {
            jobs::DEFAULT_RESUME_PROMPT.to_owned()
        } else {
            text
        };
        let job = self.add_job(
            JobKind::Resume {
                session_id: session.id.clone(),
                text,
            },
            session.cwd.clone(),
            JobOptions::default(),
        )?;
        self.announce_job(&job);
        Ok(())
    }

    /// Sagt, wann die eben angelegte Aufgabe laufen wird - sonst bliebe offen, ob ueberhaupt
    /// etwas passiert.
    fn announce_job(&self, job: &Job) {
        println!("\nAngelegt: [{}] {}", job.id, job.title);
        match job.last_window {
            Some(resets_at) => println!(
                "Startet, sobald das laufende Fenster zurueckgesetzt ist (ab {}).",
                format_timestamp(resets_at.timestamp())
            ),
            None => {
                println!("Startet beim naechsten Durchlauf des Dienstes (innerhalb einer Minute).")
            }
        }
        if !self.load_config().ping.enabled {
            println!(
                "Hinweis: Der Dienst muss laufen (`systemctl --user status claude-account-sync`)."
            );
        }
    }

    fn interactive_job_list(&self) -> Result<()> {
        let jobs = self.load_jobs()?;
        if jobs.is_empty() {
            println!("\nKeine Aufgaben angelegt.");
            return Ok(());
        }
        println!("\nAufgaben:");
        for job in &jobs {
            println!("  {}", job.summary());
        }
        println!("  [0] Zurueck");
        let Some(choice) = prompt_line("Nummer zum Loeschen: ")? else {
            return Ok(());
        };
        if choice == "0" || choice.is_empty() {
            return Ok(());
        }
        self.remove_job(&choice)
    }

    fn save_live_as(&self, name: &str) -> Result<Profile> {
        let status = self.auth_status()?;
        let email = status.email()?.to_owned();
        let credentials = read_valid_credentials(&self.paths.credentials)?;

        let existing = self.load_profile(name).ok();
        if let Some(existing) = &existing
            && !status.matches(existing)
        {
            bail!(
                "Profil `{name}` gehoert zu {}; waehle einen anderen Namen",
                existing.email
            );
        }

        let profile = Profile {
            name: name.to_owned(),
            email,
            org_id: status.org_id,
            org_name: status.org_name,
            subscription_type: status.subscription_type,
            saved_at: unix_timestamp()?,
            credential_fingerprint: Some(credential_fingerprint(&credentials)?),
            credentials_synced_at: Some(unix_timestamp()?),
            login_failed_at: None,
            // Ein erneutes `save` sichert Tokens, es setzt keine Konfiguration zurueck.
            limits: existing.map(|existing| existing.limits).unwrap_or_default(),
        };
        let profile_dir = self.profile_dir(name);
        create_private_dir(&profile_dir)?;
        atomic_write(&self.profile_credentials(name), &credentials, 0o600)?;
        self.write_profile(&profile)?;
        self.write_state(&State {
            current: Some(name.to_owned()),
        })?;
        Ok(profile)
    }

    fn sync_known_active_with_credentials(
        &self,
        status: &AuthStatus,
        credentials: &[u8],
    ) -> Result<Profile> {
        let mut profile = self.resolve_active_profile(status)?;
        atomic_write(&self.profile_credentials(&profile.name), credentials, 0o600)?;
        profile.credential_fingerprint = Some(credential_fingerprint(credentials)?);
        profile.credentials_synced_at = Some(unix_timestamp()?);
        // Ein Login, der gerade live benutzt wird, ist nicht tot.
        profile.login_failed_at = None;
        self.write_profile(&profile)?;
        self.write_state(&State {
            current: Some(profile.name.clone()),
        })?;
        Ok(profile)
    }

    /// Spiegelt den Live-Login in das Profil, zu dem er gehoert.
    ///
    /// Claude Code rotiert den Refresh-Token bei jedem Refresh. Ohne diesen Abgleich bleibt der
    /// gespeicherte Snapshot auf einem verbrauchten Token stehen, und ein spaeterer Wechsel
    /// zurueck endet in `Login expired`.
    pub fn sync(&self) -> Result<SyncOutcome> {
        self.ensure_no_auth_override()?;
        let _lock = self.lock()?;
        self.sync_locked(IdentitySource::AnyIncludingSwitcherState)
    }

    fn sync_locked(&self, identity: IdentitySource) -> Result<SyncOutcome> {
        let status = self.auth_status()?;
        if !status.logged_in {
            bail!("Claude Code ist nicht eingeloggt; nutze `claude-account login <name>`");
        }
        if identity == IdentitySource::ClaudeOnly && !status.has_identity() {
            bail!(
                "Claude kennt die Kontodaten zum aktiven Login noch nicht; \
                 der Stand wird gesichert, sobald Claude Code einmal gelaufen ist"
            );
        }
        let credentials = read_valid_credentials(&self.paths.credentials)?;
        let profile = self.resolve_active_profile(&status)?;

        let stored = fs::read(self.profile_credentials(&profile.name)).unwrap_or_default();
        if stored == credentials {
            self.refresh_profile_metadata(profile.clone(), &credentials)?;
            // Auch ohne Byte-Aenderung ist damit belegt, wer gerade aktiv ist. Bliebe der
            // Status auf einem fremden Profil stehen, wuerde die Auffrischung das falsche
            // Profil fuer aktiv halten und `list` den falschen Account markieren.
            self.write_state(&State {
                current: Some(profile.name.clone()),
            })?;
            return Ok(SyncOutcome::Unchanged {
                profile: profile.name,
            });
        }

        let synced = self.sync_known_active_with_credentials(&status, &credentials)?;
        Ok(SyncOutcome::Updated {
            email: synced.email,
            profile: synced.name,
        })
    }

    /// Ergaenzt Fingerprint und Sync-Zeitpunkt in Profilen, die vor diesem Feature entstanden
    /// sind, ohne den unveraenderten Credential-Snapshot neu zu schreiben.
    fn refresh_profile_metadata(&self, mut profile: Profile, credentials: &[u8]) -> Result<()> {
        let fingerprint = credential_fingerprint(credentials)?;
        if profile.credential_fingerprint.as_deref() == Some(fingerprint.as_str())
            && profile.login_failed_at.is_none()
        {
            return Ok(());
        }
        profile.login_failed_at = None;
        profile.credential_fingerprint = Some(fingerprint);
        profile.credentials_synced_at = Some(unix_timestamp()?);
        self.write_profile(&profile)
    }

    /// Beobachtet die aktive Credential-Datei und sichert jede Rotation sofort ins Profil.
    ///
    /// Jeder Durchlauf wird protokolliert, auch die folgenlosen: eine stille Ablehnung sieht
    /// sonst wie ein erfolgreicher Sync aus.
    pub fn watch(
        &self,
        interval_seconds: u64,
        keepalive_max_age_days: Option<u64>,
        auto_switch_threshold: Option<f64>,
    ) -> Result<()> {
        let interval = Duration::from_secs(interval_seconds.clamp(1, 3600));
        log_event(&format!(
            "Beobachte {} alle {}s",
            self.paths.credentials.display(),
            interval.as_secs()
        ));
        match keepalive_max_age_days {
            Some(days) => log_event(&format!(
                "Untaetige Profile werden nach {days} Tagen aufgefrischt"
            )),
            None => log_event("Auffrischung untaetiger Profile ist abgeschaltet"),
        }
        match auto_switch_threshold {
            Some(threshold) => log_event(&format!(
                "Auslastung wird alle {USAGE_CHECK_INTERVAL_SECONDS}s geprueft; \
                 ab {threshold:.0}% wird der Account gewechselt (per Aufrufoption erzwungen)"
            )),
            None => log_event(&format!(
                "Auslastung wird alle {USAGE_CHECK_INTERVAL_SECONDS}s geprueft; \
                 Wechsel, Fenster-Ping und Aufgaben richten sich nach {}",
                self.config_path().display()
            )),
        }

        let mut synced_stamp: Option<CredentialStamp> = None;
        let mut last_problem: Option<String> = None;
        let mut spoken: HashMap<String, String> = HashMap::new();
        let mut next_keepalive = SystemTime::now();
        let mut next_usage_check = SystemTime::now();
        loop {
            if let Some(max_age_days) = keepalive_max_age_days
                && SystemTime::now() >= next_keepalive
            {
                next_keepalive =
                    SystemTime::now() + Duration::from_secs(KEEPALIVE_INTERVAL_SECONDS);
                match self.keepalive_tick(max_age_days) {
                    Ok(()) => {}
                    Err(error) => log_problem(&format!("Auffrischung fehlgeschlagen: {error:#}")),
                }
            }

            if SystemTime::now() >= next_usage_check {
                next_usage_check =
                    SystemTime::now() + Duration::from_secs(USAGE_CHECK_INTERVAL_SECONDS);
                // Die Einstellungen werden bei jeder Pruefung neu gelesen: eine Aenderung im
                // Menue soll sofort wirken, ohne dass jemand den Dienst neu startet.
                let mut config = self.load_config();
                if let Some(threshold) = auto_switch_threshold {
                    config.auto_switch = true;
                    config.threshold = threshold;
                }
                log_once(
                    &mut spoken,
                    "config",
                    &format!("Einstellungen: {}", config.describe()),
                );
                if let Err(error) = self.automation_tick(&config, &mut spoken) {
                    log_problem(&format!("Limitpruefung fehlgeschlagen: {error:#}"));
                }
            }

            let stamp = credential_stamp(&self.paths.credentials);
            if stamp != synced_stamp {
                match self.watch_tick() {
                    Ok(Some(outcome)) => {
                        synced_stamp = stamp;
                        last_problem = None;
                        log_event(&outcome.to_string());
                    }
                    // Lock belegt oder Sync abgelehnt: Stand nicht als erledigt merken, damit der
                    // naechste Durchlauf es erneut versucht. Wiederholungen nicht doppelt loggen.
                    Ok(None) => {
                        let message = "Uebersprungen: Switcher wird gerade benutzt".to_owned();
                        if last_problem.as_deref() != Some(message.as_str()) {
                            log_event(&message);
                            last_problem = Some(message);
                        }
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        if last_problem.as_deref() != Some(message.as_str()) {
                            log_problem(&message);
                            last_problem = Some(message);
                        }
                    }
                }
            }
            thread::sleep(interval);
        }
    }

    /// Haelt Profile am Leben, die gerade nicht aktiv sind.
    ///
    /// Der Refresh-Token laeuft rund 30 Tage nach seiner letzten Nutzung ab. Ein Profil, das
    /// so lange nicht dran war, ist danach nur noch per Browser-Login zu retten. Dagegen hilft
    /// nur, es zu benutzen: jedes faellige Profil bekommt einen minimalen Request in einem
    /// eigenen Konfigurationsverzeichnis, bei dem Claude Code den Login selbst erneuert. Der
    /// global aktive Login wird dabei nicht angefasst.
    pub fn keepalive(&self, max_age_days: u64) -> Result<()> {
        self.ensure_no_auth_override()?;
        self.keepalive_run(max_age_days)
    }

    /// Haelt das Lock bewusst nicht ueber den ganzen Durchlauf: ein Request pro Profil kann
    /// bis zu zwei Minuten dauern, und solange duerfen weder das Menue noch der Watcher warten.
    /// Gesperrt wird nur zum Lesen des Ausgangszustands und zum Schreiben eines Ergebnisses.
    fn keepalive_run(&self, max_age_days: u64) -> Result<()> {
        // Eigenes Lock, getrennt vom Menue-Lock: zwei gleichzeitige Auffrischungen wuerden
        // beide mit demselben Snapshot starten, und die zweite liefe in den bereits
        // verbrauchten Token - das saehe wie ein abgelaufenes Profil aus, ist aber keins.
        let Some(_keepalive_lock) = self.try_lock_file("keepalive")? else {
            log_event("Auffrischung laeuft bereits");
            return Ok(());
        };
        let (profiles, active) = {
            let _lock = self.lock()?;
            (self.load_profiles()?, self.load_state()?.current)
        };
        if profiles.is_empty() {
            log_event("Auffrischung: keine Profile gespeichert");
            return Ok(());
        }
        let max_age_seconds = max_age_days.saturating_mul(24 * 60 * 60);
        let now = unix_timestamp()?;

        for profile in profiles {
            if active.as_deref() == Some(profile.name.as_str()) {
                log_event(&format!(
                    "Auffrischung uebersprungen: {} ist aktiv",
                    profile.name
                ));
                continue;
            }
            // Der Switcher-Status kann veraltet sein. Traegt ein Profil genau die Tokens, die
            // gerade live sind, wuerde die Auffrischung den Login unter einer laufenden Session
            // wegrotieren - der Byte-Vergleich ist die verlaessliche Aussage, nicht `state.json`.
            // Beide Seiten werden pro Profil frisch gelesen: ein Durchlauf dauert lange genug,
            // dass der Nutzer zwischendurch auf ein spaeter drankommendes Profil wechseln kann.
            let live = fs::read(&self.paths.credentials).ok();
            let snapshot = fs::read(self.profile_credentials(&profile.name)).ok();
            if snapshot.is_some() && snapshot == live {
                log_event(&format!(
                    "Auffrischung uebersprungen: {} ist der aktuell benutzte Login",
                    profile.name
                ));
                continue;
            }
            let age = profile
                .credentials_synced_at
                .map_or(u64::MAX, |synced| now.saturating_sub(synced));
            if age < max_age_seconds {
                log_event(&format!(
                    "Auffrischung uebersprungen: {} ist {} Tage alt",
                    profile.name,
                    age / (24 * 60 * 60)
                ));
                continue;
            }
            match self.renew_profile(&profile) {
                Ok(RenewOutcome::Renewed) => log_event(&format!(
                    "Aufgefrischt: {} ({})",
                    profile.name, profile.email
                )),
                Ok(RenewOutcome::StillValid) => log_event(&format!(
                    "Auffrischung nicht noetig: {} ist noch gueltig",
                    profile.name
                )),
                Ok(RenewOutcome::Expired) => log_problem(&format!(
                    "Abgelaufen: {} ({}) braucht einen neuen Login - \
                     `claude-account login {}`",
                    profile.name, profile.email, profile.name
                )),
                Ok(RenewOutcome::RequestFailed(reason)) => log_problem(&format!(
                    "Auffrischung von {} ohne Wirkung: {reason} - \
                     naechster Versuch beim naechsten Durchlauf",
                    profile.name
                )),
                Err(error) => log_problem(&format!(
                    "Auffrischung fehlgeschlagen: {} - {error:#}",
                    profile.name
                )),
            }
        }
        Ok(())
    }

    /// Zeigt die Auslastung jedes gespeicherten Accounts.
    ///
    /// Ein Profil, dessen Auslastung nicht zu holen ist, wird als solches gemeldet und nicht
    /// stillschweigend uebergangen - sonst sieht eine Luecke aus wie ein leerer Account.
    pub fn usage(&self) -> Result<()> {
        let profiles = self.load_profiles()?;
        if profiles.is_empty() {
            println!("Keine Accounts gespeichert. Starte mit: claude-account save <name>");
            return Ok(());
        }
        let active = self.active_profile_name().ok();
        for profile in &profiles {
            let marker = if active.as_deref() == Some(profile.name.as_str()) {
                "*"
            } else {
                " "
            };
            println!(
                "{marker} {} - {}{}",
                profile.name,
                profile.email,
                describe_stops(&profile.limits)
            );
            match self.profile_usage(profile, active.as_deref(), true) {
                Ok(usage) => {
                    println!(
                        "    5h:  {:>5.1}%  {}",
                        usage.five_hour.utilization,
                        describe_reset(usage.five_hour.resets_at)
                    );
                    println!(
                        "    7d:  {:>5.1}%  {}",
                        usage.seven_day.utilization,
                        describe_reset(usage.seven_day.resets_at)
                    );
                }
                Err(error) => println!("    Auslastung nicht abrufbar: {error:#}"),
            }
        }
        Ok(())
    }

    /// Setzt oder loescht die eigenen Nutzungsgrenzen eines Accounts.
    ///
    /// `None` bei beiden Werten und `clear` heisst loeschen; ansonsten wird nur ueberschrieben,
    /// was angegeben ist - sonst wuerde ein Aufruf fuer das Wochenfenster die Grenze fuer das
    /// Fuenf-Stunden-Fenster stillschweigend mitnehmen.
    pub fn set_limits(
        &self,
        name: &str,
        five_hour: Option<f64>,
        seven_day: Option<f64>,
        hard: Option<bool>,
        clear: bool,
    ) -> Result<()> {
        validate_name(name)?;
        for value in [five_hour, seven_day].into_iter().flatten() {
            if !(0.0..=100.0).contains(&value) {
                bail!("Grenzen muessen zwischen 0 und 100 liegen");
            }
        }
        let _lock = self.lock()?;
        let mut profile = self.load_profile(name)?;
        if clear {
            profile.limits = Stops::default();
        } else {
            if let Some(five_hour) = five_hour {
                profile.limits.five_hour = Some(five_hour);
            }
            if let Some(seven_day) = seven_day {
                profile.limits.seven_day = Some(seven_day);
            }
            if let Some(hard) = hard {
                profile.limits.hard = hard;
            }
        }
        self.write_profile(&profile)?;
        match profile.limits.is_set() {
            true => println!(
                "Grenzen fuer {}: {}",
                profile.name,
                describe_stops(&profile.limits).trim_start_matches(", ")
            ),
            false => println!(
                "{} hat keine eigenen Grenzen mehr; es gilt die globale Schwelle",
                profile.name
            ),
        }
        Ok(())
    }

    /// Wechselt den Account, wenn das Limit des aktiven Logins erreicht ist.
    ///
    /// `dry_run` entscheidet nur ueber die Ausfuehrung, nicht ueber die Auskunft: die
    /// Entscheidung samt Zahlen wird in jedem Fall protokolliert.
    pub fn auto_switch(&self, threshold: f64, dry_run: bool) -> Result<Decision> {
        self.ensure_no_auth_override()?;
        if !(0.0..=100.0).contains(&threshold) {
            bail!("Schwelle muss zwischen 0 und 100 liegen");
        }
        let profiles = self.load_profiles()?;
        if profiles.is_empty() {
            bail!("Keine Accounts gespeichert; nutze `claude-account save <name>`");
        }
        let active = self.active_profile_name()?;

        let Some(active_profile) = profiles.iter().find(|profile| profile.name == active) else {
            bail!("aktives Profil `{active}` wurde nicht gefunden");
        };
        let mut usages = Vec::new();
        if let Some(candidate) = self.candidate(active_profile, Some(active.as_str())) {
            // Solange der aktive Account unter seiner Grenze liegt, aendert keine weitere Zahl
            // das Ergebnis. Die Abfrage ist selbst ratenbegrenzt; jede vermeidbare Anfrage
            // erhoeht die Wahrscheinlichkeit, im entscheidenden Moment keine Antwort zu haben.
            if candidate.within_stops(threshold) {
                let decision = usage::pick_target(&active, &[candidate], threshold);
                log_event(&format!("Kein Wechsel: {}", decision.reason()));
                return Ok(decision);
            }
            usages.push(candidate);
        }
        for profile in profiles.iter().filter(|profile| profile.name != active) {
            if let Some(candidate) = self.candidate(profile, Some(active.as_str())) {
                usages.push(candidate);
            }
        }

        let decision = usage::pick_target(&active, &usages, threshold);
        match &decision {
            Decision::Stay { reason } => log_event(&format!("Kein Wechsel: {reason}")),
            Decision::NoCandidate { reason } => log_problem(&format!("Kein Wechsel: {reason}")),
            Decision::SwitchTo { name, reason } if dry_run => {
                log_event(&format!("Wuerde wechseln zu {name}: {reason}"));
            }
            Decision::SwitchTo { name, reason } => {
                log_event(&format!("Wechsel zu {name}: {reason}"));
                self.switch_checked(name, true)?;
            }
        }
        Ok(decision)
    }

    /// Baut den Bewertungskandidaten zu einem Profil und protokolliert dabei, worauf die
    /// spaetere Entscheidung beruht. `None` heisst: dieser Account faellt als Ziel aus - ein
    /// Wechsel auf gut Glueck kann in einem ebenfalls vollen Limit landen.
    fn candidate(&self, profile: &Profile, active: Option<&str>) -> Option<Candidate> {
        let (usage, age) = match self.profile_usage_cached(profile, active) {
            Ok(value) => value,
            Err(error) => {
                log_problem(&format!(
                    "Auslastung von {} nicht abrufbar, faellt als Ziel aus: {error:#}",
                    profile.name
                ));
                return None;
            }
        };
        log_event(&format!(
            "Auslastung {}: 5h {:.0}%, 7d {:.0}%{}{}",
            profile.name,
            usage.five_hour.utilization,
            usage.seven_day.utilization,
            describe_stops(&profile.limits),
            match age {
                0..=USAGE_CACHE_FRESH_SECONDS => String::new(),
                age => format!(" (Stand von vor {} min)", age / 60),
            }
        ));
        Some(Candidate {
            name: profile.name.clone(),
            usage,
            stops: profile.limits,
        })
    }

    /// Die Auslastung eines Profils samt Alter des gelieferten Standes in Sekunden.
    ///
    /// Ein junger gemerkter Stand wird ohne Anfrage benutzt, ein aelterer nur dann, wenn die
    /// Anfrage scheitert. Beides zusammen haelt die Bewertung auch dann am Leben, wenn der
    /// Endpunkt gerade mit `429` antwortet - und der Aufrufer erfaehrt, wie alt die Zahl ist.
    fn profile_usage_cached(
        &self,
        profile: &Profile,
        active: Option<&str>,
    ) -> Result<(Usage, u64)> {
        let now = unix_timestamp()?;
        let cached = self.read_cached_usage(&profile.name);
        let age = |cached: &CachedUsage| now.saturating_sub(cached.fetched_at);
        if let Some(cached) = cached.filter(|cached| age(cached) <= USAGE_CACHE_FRESH_SECONDS) {
            return Ok((cached.usage, age(&cached)));
        }

        match self.profile_usage(profile, active, true) {
            Ok(usage) => {
                self.write_cached_usage(&profile.name, usage, now);
                Ok((usage, 0))
            }
            Err(error) => {
                match cached.filter(|cached| age(cached) <= USAGE_CACHE_MAX_AGE_SECONDS) {
                    Some(cached) => {
                        log_problem(&format!(
                            "Auslastung von {} nicht abrufbar ({error:#}); \
                         der gemerkte Stand von vor {} min wird benutzt",
                            profile.name,
                            age(&cached) / 60
                        ));
                        Ok((cached.usage, age(&cached)))
                    }
                    None => Err(error),
                }
            }
        }
    }

    fn usage_cache_path(&self, name: &str) -> PathBuf {
        self.profile_dir(name).join("usage.json")
    }

    /// Ein unlesbarer oder veralteter Cache ist kein Fehler, sondern nur kein Cache.
    fn read_cached_usage(&self, name: &str) -> Option<CachedUsage> {
        let data = fs::read(self.usage_cache_path(name)).ok()?;
        serde_json::from_slice(&data).ok()
    }

    /// Der Cache haelt nur Prozentwerte und Zeitpunkte, keine Zugangsdaten.
    fn write_cached_usage(&self, name: &str, usage: Usage, fetched_at: u64) {
        let cached = CachedUsage { usage, fetched_at };
        let written = serde_json::to_vec(&cached)
            .map_err(anyhow::Error::from)
            .and_then(|payload| atomic_write(&self.usage_cache_path(name), &payload, 0o600));
        if let Err(error) = written {
            log_problem(&format!(
                "Auslastung von {name} konnte nicht gemerkt werden: {error:#}"
            ));
        }
    }

    /// Der Name des Profils, das gerade aktiv ist.
    fn active_profile_name(&self) -> Result<String> {
        let status = self.auth_status()?;
        Ok(self.resolve_active_profile(&status)?.name)
    }

    /// Holt die Auslastung eines Profils.
    ///
    /// Der Token stammt aus der Live-Datei, sobald das Profil der aktuell benutzte Login ist -
    /// sein Snapshot kann aelter sein, und ein abgelaufener Token aus dem Snapshot wuerde eine
    /// Auffrischung ausloesen, die den laufenden Login unter offenen Sessions wegrotiert.
    fn profile_usage(
        &self,
        profile: &Profile,
        active: Option<&str>,
        allow_renew: bool,
    ) -> Result<Usage> {
        let live = fs::read(&self.paths.credentials).ok();
        let snapshot = fs::read(self.profile_credentials(&profile.name)).ok();
        let is_live = active == Some(profile.name.as_str())
            || (snapshot.is_some() && snapshot == live && live.is_some());

        let credentials = if is_live {
            live.or(snapshot)
                .context("weder aktiver Login noch Snapshot lesbar")?
        } else {
            snapshot.context("kein gespeicherter Login vorhanden")?
        };

        let credentials = if needs_refresh(&credentials) {
            if is_live {
                bail!(
                    "Access-Token des aktiven Logins ist abgelaufen; \
                     Claude Code erneuert ihn beim naechsten Request"
                );
            }
            if !allow_renew {
                bail!("Access-Token ist abgelaufen");
            }
            match self.renew_snapshot(profile, Locking::Acquire)? {
                RenewResult::Renewed(credentials) | RenewResult::StillValid(credentials) => {
                    credentials
                }
                RenewResult::Expired => bail!(
                    "gespeicherter Login ist verbraucht - `claude-account login {}`",
                    profile.name
                ),
                RenewResult::RequestFailed(reason) => {
                    bail!("Login liess sich nicht auffrischen: {reason}")
                }
            }
        } else {
            credentials
        };

        usage::fetch(&read_access_token(&credentials)?)
    }

    fn auto_switch_tick(&self, threshold: f64) -> Result<()> {
        // Eigenes Lock: zwei ueberlappende Pruefungen wuerden dieselben Zahlen zweimal holen
        // und im schlimmsten Fall zweimal hintereinander wechseln.
        let Some(_lock) = self.try_lock_file("autoswitch")? else {
            log_event("Limitpruefung laeuft bereits");
            return Ok(());
        };
        self.auto_switch(threshold, false).map(|_| ())
    }

    // ---- Einstellungen ----

    fn config_path(&self) -> PathBuf {
        self.paths.store.join("config.json")
    }

    /// Die Einstellungen, so gut sie lesbar sind.
    ///
    /// Eine kaputte Datei darf den Dienst nicht anhalten: er faellt auf die Standardwerte
    /// zurueck - und weil die Standardwerte alle Automatik *aus* schalten, tut er dann lieber
    /// nichts, als das Falsche.
    pub fn load_config(&self) -> Config {
        match fs::read_to_string(self.config_path()) {
            Ok(text) => match Config::parse(&text) {
                Ok(config) => config,
                Err(error) => {
                    log_problem(&format!(
                        "Einstellungen sind unlesbar ({error:#}); es gelten die Standardwerte"
                    ));
                    Config::default()
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(error) => {
                log_problem(&format!(
                    "Einstellungen konnten nicht gelesen werden ({error}); \
                     es gelten die Standardwerte"
                ));
                Config::default()
            }
        }
    }

    pub fn write_config(&self, config: &Config) -> Result<()> {
        self.ensure_store()?;
        let payload = serde_json::to_vec_pretty(config)?;
        atomic_write(&self.config_path(), &payload, 0o600)
    }

    pub fn show_config(&self) -> Result<()> {
        let config = self.load_config();
        println!(
            "Auto-Wechsel bei vollem Limit: {}",
            onoff(config.auto_switch)
        );
        println!("Schwelle: {:.0}%", config.threshold);
        println!(
            "Fenster-Ping nach dem Reset: {}",
            onoff(config.ping.enabled)
        );
        println!("  Text: {}", config.ping.prompt);
        println!("  Modell: {}", config.ping.model);
        println!("  Zeitlimit: {} min", config.ping.timeout_minutes);
        let jobs = self.load_jobs()?;
        println!(
            "Aufgaben: {} in der Warteschlange, {} insgesamt",
            jobs.iter().filter(|job| job.enabled).count(),
            jobs.len()
        );
        Ok(())
    }

    /// Aendert einzelne Einstellungen und laesst den Rest, wie er war.
    pub fn update_config(&self, change: impl FnOnce(&mut Config)) -> Result<Config> {
        let mut config = self.load_config();
        change(&mut config);
        if !(0.0..=100.0).contains(&config.threshold) {
            bail!("Schwelle muss zwischen 0 und 100 liegen");
        }
        if config.ping.prompt.trim().is_empty() {
            bail!("Ping-Text darf nicht leer sein");
        }
        if config.ping.timeout_minutes == 0 {
            bail!("Zeitlimit des Pings muss groesser als 0 sein");
        }
        self.write_config(&config)?;
        Ok(config)
    }

    // ---- Aufgaben ----

    fn jobs_dir(&self) -> PathBuf {
        self.paths.store.join("jobs")
    }

    fn job_path(&self, id: &str) -> PathBuf {
        self.jobs_dir().join(format!("{id}.json"))
    }

    fn job_log_path(&self, id: &str) -> PathBuf {
        self.jobs_dir().join(format!("{id}.log"))
    }

    pub fn load_jobs(&self) -> Result<Vec<Job>> {
        let entries = match fs::read_dir(self.jobs_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).context("Aufgabenverzeichnis konnte nicht gelesen werden");
            }
        };
        let mut jobs = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let Some(id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            let data = fs::read(&path).with_context(|| {
                format!("Aufgabe konnte nicht gelesen werden: {}", path.display())
            })?;
            jobs.push(jobs::parse_job(&data, id)?);
        }
        jobs.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(jobs)
    }

    pub fn load_job(&self, id: &str) -> Result<Job> {
        let path = self.job_path(id);
        let data = fs::read(&path).with_context(|| format!("Aufgabe `{id}` gibt es nicht"))?;
        jobs::parse_job(&data, id)
    }

    fn write_job(&self, job: &Job) -> Result<()> {
        let payload = serde_json::to_vec_pretty(job)?;
        atomic_write(&self.job_path(&job.id), &payload, 0o600)
    }

    pub fn remove_job(&self, id: &str) -> Result<()> {
        let job = self.load_job(id)?;
        fs::remove_file(self.job_path(id))
            .with_context(|| format!("Aufgabe `{id}` konnte nicht geloescht werden"))?;
        println!("Geloescht: [{}] {}", job.id, job.title);
        Ok(())
    }

    pub fn set_job_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let mut job = self.load_job(id)?;
        job.enabled = enabled;
        self.write_job(&job)?;
        println!(
            "Aufgabe [{}] ist jetzt {}",
            job.id,
            if enabled { "aktiv" } else { "abgeschaltet" }
        );
        Ok(())
    }

    /// Legt eine Aufgabe an und merkt sich das *laufende* Fenster.
    ///
    /// Damit startet sie erst, wenn ein neues Fenster beginnt - wer gerade selbst arbeitet,
    /// bekommt sein Kontingent nicht von der eigenen Warteschlange weggenommen. Laeuft gerade
    /// kein Fenster, ist sie sofort faellig; genau dafuer ist sie ja da.
    pub fn add_job(&self, kind: JobKind, cwd: PathBuf, options: JobOptions) -> Result<Job> {
        if let JobKind::Resume { session_id, .. } = &kind {
            jobs::validate_session_id(session_id)?;
        }
        if kind.text_is_empty() {
            bail!("Der Auftrag darf nicht leer sein");
        }
        if !cwd.is_dir() {
            bail!("Arbeitsverzeichnis gibt es nicht: {}", cwd.display());
        }
        if options.timeout_minutes == Some(0) {
            bail!("Zeitlimit muss groesser als 0 sein");
        }
        create_private_dir(&self.jobs_dir())?;
        let existing = self.load_jobs()?;
        let mut job = Job::new(jobs::next_id(&existing), kind, cwd, unix_timestamp()?);
        job.repeat = options.repeat;
        job.skip_permissions = options.skip_permissions;
        job.settings = options.settings;
        job.model = options.model;
        if let Some(title) = options.title.filter(|title| !title.trim().is_empty()) {
            job.title = title;
        }
        if let Some(timeout_minutes) = options.timeout_minutes {
            job.timeout_minutes = timeout_minutes;
        }
        job.last_window = match self.active_usage() {
            Ok(usage) => usage.five_hour.resets_at,
            Err(error) => {
                log_problem(&format!(
                    "Auslastung ist gerade nicht abrufbar ({error:#}); \
                     die Aufgabe startet beim naechsten freien Fenster"
                ));
                None
            }
        };
        self.write_job(&job)?;
        Ok(job)
    }

    pub fn list_jobs(&self) -> Result<()> {
        let jobs = self.load_jobs()?;
        if jobs.is_empty() {
            println!("Keine Aufgaben. Anlegen mit: claude-account job add \"<Auftrag>\"");
            return Ok(());
        }
        for job in &jobs {
            println!("{}", job.summary());
        }
        Ok(())
    }

    /// Fuehrt eine Aufgabe sofort aus, unabhaengig vom Fenster. Fuer Diagnose und fuer den Fall,
    /// dass jemand nicht auf den naechsten Reset warten will.
    pub fn run_job_now(&self, id: &str) -> Result<()> {
        let job = self.load_job(id)?;
        let Some(_lock) = self.try_lock_file("jobs")? else {
            bail!("Es laeuft bereits eine Aufgabe");
        };
        log_event(&format!("Aufgabe [{}] wird von Hand gestartet", job.id));
        // Wer die Aufgabe selbst startet, will am Exitcode sehen, ob sie durchlief - der
        // Vermerk in der Aufgabendatei allein wird sonst leicht ueberlesen.
        if !self.execute_job(&job)? {
            let job = self.load_job(id)?;
            bail!(
                "Aufgabe [{}] ist nicht durchgelaufen: {}",
                job.id,
                job.last_status.as_deref().unwrap_or("Grund unbekannt")
            );
        }
        Ok(())
    }

    // ---- Ausfuehrung ----

    /// Startet Claude fuer eine Aufgabe und schreibt das Ergebnis in die Aufgabe zurueck.
    /// Der Rueckgabewert sagt, ob der Lauf selbst geglueckt ist.
    fn execute_job(&self, job: &Job) -> Result<bool> {
        let mut job = job.clone();
        if !job.cwd.is_dir() {
            job.enabled = false;
            job.last_status = Some(format!(
                "nicht gestartet: Arbeitsverzeichnis fehlt ({})",
                job.cwd.display()
            ));
            log_problem(&format!(
                "Aufgabe [{}] abgeschaltet: Arbeitsverzeichnis fehlt ({})",
                job.id,
                job.cwd.display()
            ));
            return self.write_job(&job).map(|()| false);
        }

        // Der Start wird vermerkt, bevor der Prozess laeuft: bricht der Dienst mitten im Lauf
        // ab, greift beim naechsten Start trotzdem die Sperre gegen einen Doppellauf.
        job.last_run_at = Some(unix_timestamp()?);
        job.last_status = Some("laeuft".to_owned());
        self.write_job(&job)?;

        let mut args = vec!["-p".to_owned(), job.text().to_owned()];
        let mut label = String::from("-p <Auftrag>");
        if let Some(session_id) = job.session_id() {
            args.push("--resume".to_owned());
            args.push(session_id.to_owned());
            label.push_str(&format!(" --resume {session_id}"));
        }
        if let Some(model) = &job.model {
            args.push("--model".to_owned());
            args.push(model.clone());
            label.push_str(&format!(" --model {model}"));
        }
        if let Some(settings) = &job.settings {
            args.push("--settings".to_owned());
            args.push(settings.display().to_string());
            label.push_str(&format!(" --settings {}", settings.display()));
        }
        if job.skip_permissions {
            args.push("--dangerously-skip-permissions".to_owned());
            label.push_str(" --dangerously-skip-permissions");
        }

        let outcome = self.run_claude_task(
            &self.job_log_path(&job.id),
            &job.cwd,
            args,
            &label,
            Duration::from_secs(job.timeout_minutes.saturating_mul(60)),
        );
        let success = match outcome {
            Ok(run) => {
                log_event(&format!("Aufgabe [{}] beendet: {}", job.id, run.status));
                job.last_status = Some(run.status);
                // Ein Fehlschlag darf die Aufgabe nicht stillschweigend verbrauchen: nur ein
                // erfolgreicher Lauf hakt eine einmalige Aufgabe ab.
                if run.success && !job.repeat {
                    job.enabled = false;
                }
                run.success
            }
            Err(error) => {
                log_problem(&format!(
                    "Aufgabe [{}] konnte nicht gestartet werden: {error:#}",
                    job.id
                ));
                job.last_status = Some(format!("nicht gestartet: {error:#}"));
                false
            }
        };
        job.last_window = self.confirm_window(&format!("Aufgabe [{}]", job.id));
        self.write_job(&job)?;
        Ok(success)
    }

    /// Holt nach einem Lauf den Fensterstand frisch und meldet ihn.
    ///
    /// Das ist zugleich der Beweis, dass der Lauf wirklich ein Fenster eroeffnet hat, und die
    /// Kennung, an der die Faelligkeit erkennt, dass dieses Fenster bedient ist. Scheitert die
    /// Abfrage, bleibt sie `None` - dann haelt allein die Sperre gegen Doppellauf.
    fn confirm_window(&self, subject: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        match self.active_usage_fresh() {
            Ok(usage) => {
                match usage.five_hour.resets_at {
                    Some(resets_at) => log_event(&format!(
                        "{subject}: Fenster laeuft jetzt bis {} ({:.0}% verbraucht)",
                        format_timestamp(resets_at.timestamp()),
                        usage.five_hour.utilization
                    )),
                    None => log_problem(&format!(
                        "{subject}: die API meldet weiterhin kein laufendes Fenster - \
                         der Lauf hat keines eroeffnet"
                    )),
                }
                usage.five_hour.resets_at
            }
            Err(error) => {
                log_problem(&format!(
                    "{subject}: Fensterstand nicht bestaetigt ({error:#}); \
                     ein sofortiger zweiter Lauf wird durch die Sperre verhindert"
                ));
                None
            }
        }
    }

    /// Ruft die Claude-CLI auf und protokolliert Lauf und Ausgang in eine eigene Logdatei.
    ///
    /// Der Auftragstext steht nur im Prozessargument, nicht in der Kopfzeile: eine Logdatei
    /// wird gelesen und weitergereicht, und was drinsteht, muss man verantworten koennen.
    fn run_claude_task(
        &self,
        log_path: &Path,
        cwd: &Path,
        args: Vec<String>,
        label: &str,
        timeout: Duration,
    ) -> Result<TaskRun> {
        create_private_dir(
            log_path
                .parent()
                .context("ungueltiger Pfad fuer die Logdatei")?,
        )?;
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(log_path)
            .with_context(|| format!("Logdatei nicht beschreibbar: {}", log_path.display()))?;
        let started = SystemTime::now();
        writeln!(
            log,
            "\n[{}] Start: {} {label} (Ordner {}, Zeitlimit {} min)",
            chrono::Local::now().format("%F %T"),
            self.paths.claude_bin.display(),
            cwd.display(),
            timeout.as_secs() / 60
        )
        .context("Logdatei konnte nicht geschrieben werden")?;

        let stdout = log
            .try_clone()
            .context("Logdatei konnte nicht geteilt werden")?;
        let stderr = log
            .try_clone()
            .context("Logdatei konnte nicht geteilt werden")?;
        let mut child = Command::new(&self.paths.claude_bin)
            .args(&args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(stdout))
            .stderr(std::process::Stdio::from(stderr))
            .spawn()
            .with_context(|| {
                format!(
                    "Claude konnte nicht gestartet werden: {}",
                    self.paths.claude_bin.display()
                )
            })?;

        // Testbar und im Notfall operativ anpassbar, ohne jede Aufgabe einzeln zu aendern.
        let timeout = env::var("CLAUDE_ACCOUNT_SWITCHER_JOB_TIMEOUT")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .map(Duration::from_secs)
            .unwrap_or(timeout);
        let (success, mut status) = match child.wait_timeout(timeout) {
            Ok(Some(exit)) if exit.success() => (true, "erfolgreich".to_owned()),
            Ok(Some(exit)) => (false, format!("Claude endete mit {exit}")),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                (
                    false,
                    format!(
                        "Abbruch nach Zeitlimit von {}",
                        describe_duration(timeout.as_secs())
                    ),
                )
            }
            Err(error) => (
                false,
                format!("Claude konnte nicht abgewartet werden: {error}"),
            ),
        };
        // Ohne Claudes eigene Worte sind Limit, Berechtigungsfrage und Netzfehler dieselbe Zeile.
        if !success && let Some(reason) = claude_reason(log_path) {
            status.push_str(&format!(": {reason}"));
        }
        let seconds = started
            .elapsed()
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let status = format!("{status} ({})", describe_duration(seconds));
        let _ = writeln!(
            log,
            "[{}] Ende: {status}",
            chrono::Local::now().format("%F %T")
        );
        Ok(TaskRun { success, status })
    }

    // ---- Fenster-Ping ----

    fn ping_state_path(&self) -> PathBuf {
        self.paths.store.join("ping.json")
    }

    fn load_ping_state(&self) -> PingState {
        fs::read(self.ping_state_path())
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    fn write_ping_state(&self, state: &PingState) -> Result<()> {
        self.ensure_store()?;
        let payload = serde_json::to_vec_pretty(state)?;
        atomic_write(&self.ping_state_path(), &payload, 0o600)
    }

    /// Eroeffnet das Fuenf-Stunden-Fenster mit einem winzigen Auftrag.
    ///
    /// Das Fenster startet nicht mit dem Reset, sondern mit der ersten Anfrage danach. Wer erst
    /// Stunden spaeter wieder etwas tippt, verschiebt seine ganzen fuenf Stunden nach hinten.
    pub fn ping_now(&self) -> Result<()> {
        let config = self.load_config();
        self.send_ping(&config)
    }

    fn send_ping(&self, config: &Config) -> Result<()> {
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .context("HOME ist nicht gesetzt")?;
        let mut state = self.load_ping_state();
        state.last_run_at = Some(unix_timestamp()?);
        self.write_ping_state(&state)?;

        let outcome = self.run_claude_task(
            &self.paths.store.join("ping.log"),
            &home,
            vec![
                "-p".to_owned(),
                config.ping.prompt.clone(),
                "--model".to_owned(),
                config.ping.model.clone(),
                "--strict-mcp-config".to_owned(),
            ],
            &format!("-p <Ping> --model {}", config.ping.model),
            Duration::from_secs(config.ping.timeout_minutes.saturating_mul(60)),
        );
        match outcome {
            Ok(run) if run.success => log_event(&format!("Fenster-Ping gesendet: {}", run.status)),
            Ok(run) => log_problem(&format!("Fenster-Ping fehlgeschlagen: {}", run.status)),
            Err(error) => log_problem(&format!("Fenster-Ping nicht gestartet: {error:#}")),
        }
        state.last_window = self.confirm_window("Fenster-Ping");
        self.write_ping_state(&state)
    }

    // ---- Durchlauf im Hintergrunddienst ----

    /// Ein Durchlauf der Automatik: Wechsel, Ping, faellige Aufgaben - in dieser Reihenfolge.
    ///
    /// `spoken` haelt die zuletzt gemeldete Begruendung je Gegenstand fest. Gemeldet wird jede
    /// Entscheidung, auch jedes "noch nicht" - aber erst wieder, wenn sie sich aendert. Sonst
    /// stuende dieselbe Zeile jede Minute im Journal, und niemand laese sie mehr.
    fn automation_tick(&self, config: &Config, spoken: &mut HashMap<String, String>) -> Result<()> {
        if config.auto_switch {
            self.auto_switch_tick(config.threshold)?;
        }
        let jobs = match self.load_jobs() {
            Ok(jobs) => jobs,
            Err(error) => {
                log_problem(&format!("Aufgaben nicht lesbar: {error:#}"));
                Vec::new()
            }
        };
        let waiting = jobs.iter().filter(|job| job.enabled).count();
        if !config.ping.enabled && waiting == 0 {
            return Ok(());
        }

        // Die Auslastung wird genau einmal geholt und von Ping und Aufgaben gemeinsam benutzt:
        // der Endpunkt ist ratenbegrenzt, und beide bewerten dasselbe Fenster.
        let usage = self.active_usage()?;
        let now = unix_timestamp()?;

        if config.ping.enabled {
            let state = self.load_ping_state();
            let verdict = jobs::window_readiness(
                &usage,
                config.threshold,
                state.last_window,
                state.last_run_at,
                now,
            );
            match &verdict {
                Readiness::Due(reason) => {
                    log_event(&format!("Fenster-Ping faellig: {reason}"));
                    spoken.remove("ping");
                    if let Err(error) = self.send_ping(config) {
                        log_problem(&format!("Fenster-Ping fehlgeschlagen: {error:#}"));
                    }
                }
                Readiness::Waiting(reason) => {
                    log_once(spoken, "ping", &format!("Fenster-Ping wartet: {reason}"))
                }
            }
        }

        if waiting == 0 {
            return Ok(());
        }
        // Nur eine Aufgabe gleichzeitig: zwei parallele Laeufe teilen sich dasselbe Kontingent
        // und stolpern im selben Arbeitsverzeichnis uebereinander.
        let Some(lock) = self.try_lock_file("jobs")? else {
            log_once(spoken, "jobs", "Aufgaben warten: es laeuft bereits eine");
            return Ok(());
        };
        let mut start: Option<Job> = None;
        for (job, verdict) in jobs::evaluate(&jobs, &usage, config.threshold, now) {
            let key = format!("job-{}", job.id);
            match (&verdict, start.is_none()) {
                (Readiness::Due(reason), true) => {
                    log_event(&format!("Aufgabe [{}] faellig: {reason}", job.id));
                    spoken.remove(&key);
                    start = Some(job.clone());
                }
                (Readiness::Due(_), false) => log_once(
                    spoken,
                    &key,
                    &format!("Aufgabe [{}] faellig, wartet auf den freien Platz", job.id),
                ),
                (Readiness::Waiting(reason), _) => {
                    log_once(spoken, &key, &format!("Aufgabe [{}]: {reason}", job.id))
                }
            }
        }
        match start {
            // Der Lauf gehoert in einen eigenen Faden: eine Aufgabe darf Stunden dauern, und
            // solange muessen rotierte Tokens weiter gesichert werden.
            Some(job) => self.spawn_job(job, lock),
            None => drop(lock),
        }
        Ok(())
    }

    fn spawn_job(&self, job: Job, lock: File) {
        let paths = self.paths.clone();
        thread::spawn(move || {
            let app = App { paths };
            if let Err(error) = app.execute_job(&job).map(|_| ()) {
                log_problem(&format!(
                    "Ergebnis der Aufgabe [{}] konnte nicht gespeichert werden: {error:#}",
                    job.id
                ));
            }
            drop(lock);
        });
    }

    /// Die Auslastung des aktiven Accounts; ein junger gemerkter Stand genuegt hier.
    fn active_usage(&self) -> Result<Usage> {
        let active = self.active_profile_name()?;
        let profile = self.load_profile(&active)?;
        self.profile_usage_cached(&profile, Some(active.as_str()))
            .map(|(usage, _age)| usage)
    }

    /// Die Auslastung ohne gemerkten Stand. Nach einem Lauf ist genau das noetig: der Cache
    /// waere wenige Sekunden alt und wuerde den Stand von *vor* dem Lauf zurueckgeben.
    fn active_usage_fresh(&self) -> Result<Usage> {
        let active = self.active_profile_name()?;
        let profile = self.load_profile(&active)?;
        let usage = self.profile_usage(&profile, Some(active.as_str()), true)?;
        self.write_cached_usage(&active, usage, unix_timestamp()?);
        Ok(usage)
    }

    // ---- Sitzungen ----

    fn sessions_dir(&self) -> PathBuf {
        self.paths
            .credentials
            .parent()
            .unwrap_or(Path::new("."))
            .join("projects")
    }

    /// Die zuletzt benutzten Claude-Sitzungen, neueste zuerst.
    ///
    /// Arbeitsverzeichnis und Titel kommen aus der Sitzungsdatei selbst und nicht aus ihrem
    /// Ordnernamen: der ist verlustbehaftet kodiert, und ein falsches Arbeitsverzeichnis waere
    /// beim Fortsetzen genau der Fehler, den niemand bemerkt.
    pub fn recent_sessions(&self, limit: usize) -> Vec<SessionEntry> {
        let mut files: Vec<(u64, PathBuf)> = Vec::new();
        let Ok(projects) = fs::read_dir(self.sessions_dir()) else {
            return Vec::new();
        };
        for project in projects.flatten() {
            let Ok(entries) = fs::read_dir(project.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                    continue;
                }
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|meta| meta.modified().ok())
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|value| value.as_secs())
                    .unwrap_or(0);
                files.push((modified, path));
            }
        }
        // Neueste zuerst: was zuletzt lief, will man am ehesten fortsetzen.
        files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        files
            .into_iter()
            .take(limit)
            .filter_map(|(modified, path)| read_session(&path, modified))
            .collect()
    }

    /// Sucht eine Sitzung anhand ihrer ID, damit ein Auftrag ohne Angabe eines Ordners im
    /// richtigen Verzeichnis fortsetzt statt irgendwo.
    pub fn find_session(&self, id: &str) -> Option<SessionEntry> {
        self.recent_sessions(usize::MAX)
            .into_iter()
            .find(|session| session.id == id)
    }

    pub fn list_sessions(&self) -> Result<()> {
        let sessions = self.recent_sessions(SESSION_LIST_LIMIT);
        if sessions.is_empty() {
            bail!(
                "Keine Sitzungen gefunden in {}",
                self.sessions_dir().display()
            );
        }
        for session in sessions {
            println!(
                "{}  {}  {}",
                session.id,
                format_timestamp(session.modified as i64),
                session.title
            );
            println!("    {}", session.cwd.display());
        }
        Ok(())
    }

    fn renew_profile(&self, profile: &Profile) -> Result<RenewOutcome> {
        Ok(match self.renew_snapshot(profile, Locking::Acquire)? {
            RenewResult::Renewed(_) => RenewOutcome::Renewed,
            RenewResult::StillValid(_) => RenewOutcome::StillValid,
            RenewResult::Expired => RenewOutcome::Expired,
            RenewResult::RequestFailed(reason) => RenewOutcome::RequestFailed(reason),
        })
    }

    /// Laesst Claude Code den Login eines Profils in einem eigenen Konfigurationsverzeichnis
    /// erneuern und liefert den Stand, der danach gilt. Geschrieben wird nur, wenn danach ein
    /// vollstaendiger Login dasteht: schlaegt der Refresh fehl, leert Claude Code die Datei, und
    /// ein leerer Stand darf den letzten bekannten niemals ueberschreiben.
    ///
    /// `locking` sagt, ob das Switcher-Lock hier genommen werden muss. Der Wechsel haelt es
    /// bereits; ein zweiter Versuch auf dieselbe Datei wuerde den eigenen Prozess blockieren.
    fn renew_snapshot(&self, profile: &Profile, locking: Locking) -> Result<RenewResult> {
        let snapshot = read_valid_credentials(&self.profile_credentials(&profile.name))?;
        let workspace = self.renew_workspace(&profile.name)?;
        let result = self.renew_in_workspace(&workspace, &snapshot);
        // Das Verzeichnis haelt eine Kopie aktiver Zugangsdaten; es verschwindet in jedem Fall.
        if let Err(error) = fs::remove_dir_all(&workspace) {
            log_problem(&format!(
                "Arbeitsverzeichnis konnte nicht entfernt werden: {} - {error}",
                workspace.display()
            ));
        }

        let attempt = result?;
        let credentials = match attempt.credentials {
            None => {
                // Der Beweis, dass dieses Profil einen Browser-Login braucht. Er gehoert ins
                // Profil, sonst zeigt die Liste weiter den Ablauf aus der Datei an - und der
                // behauptet Wochen an Gueltigkeit, die es nicht mehr gibt.
                let _lock = self.acquire(locking)?;
                let mut profile = profile.clone();
                profile.login_failed_at = Some(unix_timestamp()?);
                self.write_profile(&profile)?;
                return Ok(RenewResult::Expired);
            }
            // Unveraendert heisst: Claude hat nichts erneuert. Bei einem gescheiterten Aufruf
            // ist das kein Gesundheitszeugnis, sondern ein Fehlschlag - das Profil darf dann
            // nicht als geprueft gelten, sonst laeuft seine echte Frist still weiter, waehrend
            // das Journal gruen meldet.
            Some(credentials) if credentials == snapshot => {
                if let Some(failure) = attempt.failure {
                    return Ok(RenewResult::RequestFailed(failure));
                }
                let _lock = self.acquire(locking)?;
                let mut profile = profile.clone();
                profile.credentials_synced_at = Some(unix_timestamp()?);
                profile.login_failed_at = None;
                self.write_profile(&profile)?;
                return Ok(RenewResult::StillValid(snapshot));
            }
            Some(credentials) => credentials,
        };

        // Der Aufruf ging schief, hat aber trotzdem einen neuen Login hinterlassen: Claude
        // rotiert, bevor es antwortet. Der Vorfall gehoert ins Journal, gesichert wird er
        // trotzdem - sonst waere der Token unwiederbringlich weg.
        if let Some(failure) = attempt.failure {
            log_problem(&format!(
                "{failure}; der bereits erneuerte Login von `{}` wird trotzdem gesichert",
                profile.name
            ));
        }

        // Ab hier gilt: der neue Token existiert nur noch hier, der alte ist serverseitig tot.
        // Was jetzt nicht gespeichert wird, ist verloren - deshalb wird ab diesem Punkt nichts
        // mehr abgebrochen, was sich nicht auf dieses Profil bezieht.
        let _lock = self.acquire(locking)?;
        let unchanged = fs::read(self.profile_credentials(&profile.name))
            .is_ok_and(|current| current == snapshot);
        if !unchanged {
            bail!(
                "Profil `{}` wurde waehrend der Auffrischung veraendert; \
                 der erneuerte Login wurde verworfen",
                profile.name
            );
        }

        let mut profile = profile.clone();
        atomic_write(
            &self.profile_credentials(&profile.name),
            &credentials,
            0o600,
        )?;
        profile.credential_fingerprint = Some(credential_fingerprint(&credentials)?);
        profile.credentials_synced_at = Some(unix_timestamp()?);
        profile.login_failed_at = None;
        self.write_profile(&profile)?;
        Ok(RenewResult::Renewed(credentials))
    }

    fn acquire(&self, locking: Locking) -> Result<Option<File>> {
        match locking {
            Locking::Acquire => self.lock().map(Some),
            Locking::AlreadyHeld => Ok(None),
        }
    }

    /// `Ok(None)` heisst: Claude konnte den Login nicht erneuern.
    fn renew_in_workspace(&self, workspace: &Path, snapshot: &[u8]) -> Result<RenewalAttempt> {
        let credentials = workspace.join(".credentials.json");
        atomic_write(&credentials, snapshot, 0o600)?;

        // Claudes Begruendung wird in eine Datei geschrieben, nicht in eine Pipe: der Prozess
        // wird mit Timeout abgewartet, und eine volllaufende Pipe wuerde ihn genau dort haengen
        // lassen. Die Datei verschwindet mit dem Arbeitsverzeichnis.
        let stderr_log = workspace.join("claude-stderr.log");
        let stderr_sink = File::create(&stderr_log).with_context(|| {
            format!(
                "Fehlerausgabe konnte nicht angelegt werden: {}",
                stderr_log.display()
            )
        })?;

        let mut child = Command::new(&self.paths.claude_bin)
            .args(["-p", "ok", "--model", "haiku", "--strict-mcp-config"])
            .env("CLAUDE_CONFIG_DIR", workspace)
            .current_dir(workspace)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(stderr_sink))
            .spawn()
            .with_context(|| {
                format!(
                    "Claude konnte nicht gestartet werden: {}",
                    self.paths.claude_bin.display()
                )
            })?;
        // Ab dem Start des Prozesses darf nichts mehr ohne Rettungsversuch abbrechen: Claude
        // rotiert den Token, bevor es antwortet. Ein Timeout oder Fehler nach diesem Punkt
        // heisst nicht, dass kein neuer Login dasteht - er waere nur unwiederbringlich weg.
        let timeout = self.keepalive_timeout();
        let failure = match child.wait_timeout(timeout) {
            Ok(Some(status)) if status.success() => None,
            Ok(Some(status)) => Some(format!("Claude endete mit {status}")),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                Some(format!(
                    "Claude hat nach {} Sekunden nicht geantwortet",
                    timeout.as_secs()
                ))
            }
            Err(error) => Some(format!("Claude konnte nicht abgewartet werden: {error}")),
        };
        // Ohne Claudes eigene Worte sind Rate-Limit, toter Refresh-Token und Netzfehler
        // dieselbe Zeile - und damit nicht behebbar.
        let failure = failure.map(|failure| match claude_reason(&stderr_log) {
            Some(reason) => format!("{failure}: {reason}"),
            None => failure,
        });

        let renewed = fs::read(&credentials).with_context(|| {
            format!(
                "erneuerte Credentials konnten nicht gelesen werden: {}",
                credentials.display()
            )
        })?;
        Ok(RenewalAttempt {
            credentials: validate_credentials(&renewed).ok().map(|()| renewed),
            failure,
        })
    }

    /// Testbar und im Notfall operativ anpassbar; der Standard reicht fuer einen Prompt samt
    /// Retry-Backoff.
    fn keepalive_timeout(&self) -> Duration {
        let seconds = env::var("CLAUDE_ACCOUNT_SWITCHER_KEEPALIVE_TIMEOUT")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(KEEPALIVE_REQUEST_TIMEOUT_SECONDS);
        Duration::from_secs(seconds)
    }

    fn renew_workspace(&self, name: &str) -> Result<PathBuf> {
        let workspace = self
            .paths
            .store
            .join(format!(".renew-{name}-{}", std::process::id()));
        if workspace.exists() {
            fs::remove_dir_all(&workspace).with_context(|| {
                format!(
                    "altes Arbeitsverzeichnis konnte nicht entfernt werden: {}",
                    workspace.display()
                )
            })?;
        }
        create_private_dir(&workspace)?;
        Ok(workspace)
    }

    fn watch_tick(&self) -> Result<Option<SyncOutcome>> {
        self.ensure_no_auth_override()?;
        let Some(_lock) = self.try_lock()? else {
            return Ok(None);
        };
        self.sync_locked(IdentitySource::ClaudeOnly).map(Some)
    }

    fn keepalive_tick(&self, max_age_days: u64) -> Result<()> {
        self.ensure_no_auth_override()?;
        self.keepalive_run(max_age_days)
    }

    /// Ermittelt, zu welchem Profil der gerade aktive Login gehoert.
    ///
    /// Es gibt zwei Quellen: Claudes Identitaets-Cache in `.claude.json` und den zuletzt vom
    /// Switcher gesetzten Account. Der Cache ist direkt nach einem Wechsel absichtlich leer und
    /// kann von einer laufenden Claude-Session mit veralteten Kontodaten neu geschrieben werden.
    /// Widersprechen sich beide Quellen, laesst sich der aktive Login nicht sicher zuordnen; dann
    /// wird abgebrochen statt geraten, sonst landen fremde Tokens in einem Profil.
    fn resolve_active_profile(&self, status: &AuthStatus) -> Result<Profile> {
        if !status.logged_in {
            bail!("Claude Code ist nicht eingeloggt; nutze `claude-account login <name>`");
        }
        let profiles = self.load_profiles()?;
        let last_active = self.load_state()?.current;
        if !status.has_identity() {
            let current = last_active.context(
                "aktiver Login ist keinem Profil zuzuordnen; starte einmal Claude Code oder speichere ihn mit `claude-account save <name>`",
            )?;
            return profiles
                .into_iter()
                .find(|profile| profile.name == current)
                .with_context(|| {
                    format!("zuletzt aktives Profil `{current}` existiert nicht mehr")
                });
        }

        let matches: Vec<Profile> = profiles
            .into_iter()
            .filter(|profile| status.matches(profile))
            .collect();
        let profile = match matches.as_slice() {
            [profile] => profile.clone(),
            [] => {
                let email = status.email()?;
                bail!(
                    "aktiver Login {email} ist nicht gespeichert; zuerst `claude-account save <name>` ausfuehren"
                )
            }
            _ => bail!("aktiver Login passt zu mehreren Profilen; doppelte Profile bereinigen"),
        };
        if let Some(last_active) = last_active
            && last_active != profile.name
        {
            bail!(
                "Claude meldet `{}` ({}), der Switcher hat zuletzt `{last_active}` aktiviert; \
                 beende offene Claude-Code-Sessions und starte Claude einmal neu, oder speichere \
                 den aktiven Login mit `claude-account save <name>`",
                profile.name,
                profile.email
            );
        }
        Ok(profile)
    }

    /// Entfernt Claude Codes Identitaets-Cache aus `.claude.json`.
    ///
    /// `.credentials.json` enthaelt nur Tokens, keine Kontodaten. Bleibt der Cache nach einem
    /// Credential-Tausch auf dem alten Konto stehen, meldet Claude Code `Login expired`, und der
    /// Switcher wuerde die Tokens des neuen Kontos in das alte Profil zurueckschreiben. Ohne die
    /// Felder laedt Claude Code beide beim naechsten Start selbst neu.
    fn clear_cached_identity(&self) -> Result<()> {
        let path = &self.paths.claude_json;
        let data = match fs::read(path) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Claude-Konfiguration konnte nicht gelesen werden: {}",
                        path.display()
                    )
                });
            }
        };
        let mut value: Value = serde_json::from_slice(&data).with_context(|| {
            format!(
                "Claude-Konfiguration ist kein gueltiges JSON: {}",
                path.display()
            )
        })?;
        let object = value.as_object_mut().with_context(|| {
            format!(
                "Claude-Konfiguration ist kein JSON-Objekt: {}",
                path.display()
            )
        })?;
        let mut changed = false;
        for field in ["oauthAccount", "userID"] {
            changed |= object.remove(field).is_some();
        }
        if !changed {
            return Ok(());
        }
        let payload = serde_json::to_vec(&value)?;
        atomic_write_into_existing_dir(path, &payload, 0o600).with_context(|| {
            format!(
                "Identitaets-Cache konnte nicht geleert werden: {}",
                path.display()
            )
        })
    }

    fn auth_status(&self) -> Result<AuthStatus> {
        let output = Command::new(&self.paths.claude_bin)
            .args(["auth", "status"])
            .output()
            .with_context(|| {
                format!(
                    "Claude Status konnte nicht gestartet werden: {}",
                    self.paths.claude_bin.display()
                )
            })?;
        if !output.status.success() {
            bail!("`claude auth status` ist fehlgeschlagen");
        }
        serde_json::from_slice(&output.stdout)
            .context("`claude auth status` hat kein gueltiges JSON geliefert")
    }

    fn ensure_no_auth_override(&self) -> Result<()> {
        const OVERRIDES: [&str; 3] = [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ];
        let active: Vec<&str> = OVERRIDES
            .into_iter()
            .filter(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
            .collect();
        if !active.is_empty() {
            bail!(
                "Auth-Umgebungsvariable(n) {} ueberschreiben den Claude-Login; vor dem Wechsel entfernen",
                active.join(", ")
            );
        }
        Ok(())
    }

    fn restore_live(&self, credentials: Option<&[u8]>, state: &State) -> Result<()> {
        if let Some(credentials) = credentials {
            atomic_write(&self.paths.credentials, credentials, 0o600)?;
        } else if let Err(error) = fs::remove_file(&self.paths.credentials)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error).context(
                "teilweise geschriebene Claude-Credentials konnten nicht entfernt werden",
            );
        }
        self.write_state(state)
    }

    fn lock(&self) -> Result<File> {
        let lock = self.open_lock_file(".lock")?;
        FileExt::lock_exclusive(&lock).context("Account-Switcher ist bereits in Benutzung")?;
        Ok(lock)
    }

    /// Wie `lock`, wartet aber nicht. Fuer den Watcher: er darf ein offenes Menue nicht blockieren.
    fn try_lock(&self) -> Result<Option<File>> {
        self.try_lock_path(".lock")
    }

    /// Eigenes, benanntes Lock fuer Ablaeufe, die sich nur untereinander ausschliessen.
    fn try_lock_file(&self, name: &str) -> Result<Option<File>> {
        self.try_lock_path(&format!(".{name}.lock"))
    }

    fn try_lock_path(&self, file_name: &str) -> Result<Option<File>> {
        let lock = self.open_lock_file(file_name)?;
        match FileExt::try_lock_exclusive(&lock) {
            Ok(()) => Ok(Some(lock)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error).context("Lock konnte nicht geprueft werden"),
        }
    }

    fn open_lock_file(&self, file_name: &str) -> Result<File> {
        self.ensure_store()?;
        let lock_path = self.paths.store.join(file_name);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| {
                format!(
                    "Lock konnte nicht geoeffnet werden: {}",
                    lock_path.display()
                )
            })?;
        lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(lock)
    }

    fn ensure_store(&self) -> Result<()> {
        create_private_dir(&self.paths.store)?;
        create_private_dir(&self.paths.store.join("accounts"))
    }

    fn profile_dir(&self, name: &str) -> PathBuf {
        self.paths.store.join("accounts").join(name)
    }

    fn profile_credentials(&self, name: &str) -> PathBuf {
        self.profile_dir(name).join("credentials.json")
    }

    fn profile_metadata(&self, name: &str) -> PathBuf {
        self.profile_dir(name).join("profile.json")
    }

    fn state_path(&self) -> PathBuf {
        self.paths.store.join("state.json")
    }

    fn load_profile(&self, name: &str) -> Result<Profile> {
        let path = self.profile_metadata(name);
        let data =
            fs::read(&path).with_context(|| format!("Profil `{name}` wurde nicht gefunden"))?;
        let profile: Profile = serde_json::from_slice(&data)
            .with_context(|| format!("Profil-Metadaten sind ungueltig: {}", path.display()))?;
        if profile.name != name {
            bail!("Profilname in {} stimmt nicht", path.display());
        }
        Ok(profile)
    }

    fn load_profiles(&self) -> Result<Vec<Profile>> {
        let accounts_dir = self.paths.store.join("accounts");
        let entries = match fs::read_dir(&accounts_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).context("Account-Verzeichnis konnte nicht gelesen werden");
            }
        };
        let mut profiles = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !entry.path().join("profile.json").is_file() {
                continue;
            }
            profiles.push(self.load_profile(name)?);
        }
        profiles.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(profiles)
    }

    fn write_profile(&self, profile: &Profile) -> Result<()> {
        let payload = serde_json::to_vec_pretty(profile)?;
        atomic_write(&self.profile_metadata(&profile.name), &payload, 0o600)
    }

    fn load_state(&self) -> Result<State> {
        let path = self.state_path();
        match fs::read(&path) {
            Ok(data) => serde_json::from_slice(&data)
                .with_context(|| format!("Statusdatei ist ungueltig: {}", path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(error).context("Statusdatei konnte nicht gelesen werden"),
        }
    }

    fn write_state(&self, state: &State) -> Result<()> {
        self.ensure_store()?;
        let payload = serde_json::to_vec_pretty(state)?;
        atomic_write(&self.state_path(), &payload, 0o600)
    }
}

/// Alles, was eine Aufgabe ausser Auftrag und Ordner haben kann. Die Standardwerte sind die
/// eines unbeaufsichtigten Laufs: keine Rueckfragen, eine Stunde Zeit, einmalig.
#[derive(Debug, Clone)]
pub struct JobOptions {
    pub title: Option<String>,
    pub settings: Option<PathBuf>,
    pub model: Option<String>,
    pub timeout_minutes: Option<u64>,
    pub skip_permissions: bool,
    pub repeat: bool,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            title: None,
            settings: None,
            model: None,
            timeout_minutes: None,
            skip_permissions: true,
            repeat: false,
        }
    }
}

/// Ausgang eines Claude-Aufrufs, wie er in der Aufgabe vermerkt wird.
struct TaskRun {
    success: bool,
    status: String,
}

/// Zustand des Fenster-Pings. Er ist keine Aufgabe und taucht deshalb auch nicht in der
/// Aufgabenliste auf - er braucht aber dieselben zwei Merkposten.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PingState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_run_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_window: Option<chrono::DateTime<chrono::Utc>>,
}

/// Eine Claude-Sitzung, wie sie in der Auswahl erscheint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub modified: u64,
}

/// Liest Arbeitsverzeichnis und erste Frage aus einer Sitzungsdatei.
///
/// Gelesen werden nur die ersten Zeilen: die Dateien werden megabytegross, und beides steht
/// am Anfang. Eine unlesbare Datei wird uebersprungen und nicht zum Abbruch.
fn read_session(path: &Path, modified: u64) -> Option<SessionEntry> {
    let id = path.file_stem()?.to_str()?.to_owned();
    if jobs::validate_session_id(&id).is_err() {
        return None;
    }
    let file = File::open(path).ok()?;
    let mut cwd: Option<PathBuf> = None;
    let mut title: Option<String> = None;
    for line in BufReader::new(file).lines().take(SESSION_SCAN_LINES) {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if cwd.is_none()
            && let Some(found) = value.get("cwd").and_then(Value::as_str)
        {
            cwd = Some(PathBuf::from(found));
        }
        if title.is_none()
            && value.get("type").and_then(Value::as_str) == Some("user")
            && let Some(text) = session_text(&value)
        {
            title = Some(text);
        }
        if cwd.is_some() && title.is_some() {
            break;
        }
    }
    Some(SessionEntry {
        id,
        cwd: cwd?,
        title: title.unwrap_or_else(|| "(ohne Text)".to_owned()),
        modified,
    })
}

/// Der erste echte Nutzertext einer Sitzung.
///
/// Der Inhalt ist mal ein String, mal eine Liste von Bloecken. Eingeklammerte Blocke wie
/// `<command-name>` sind Werkzeugrauschen und taugen nicht als Titel.
fn session_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.is_empty() || single_line.starts_with('<') {
        return None;
    }
    Some(shorten(&single_line, 60))
}

fn shorten(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(max_chars).collect();
    short.push_str(" ...");
    short
}

/// Eine Prozentangabe aus dem Menue. Leer heisst "nicht angegeben", nicht "null".
fn parse_percent(input: &str) -> Result<Option<f64>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let value: f64 = trimmed
        .replace(',', ".")
        .trim_end_matches('%')
        .trim()
        .parse()
        .with_context(|| format!("`{trimmed}` ist keine Prozentzahl"))?;
    Ok(Some(value))
}

fn onoff(value: bool) -> &'static str {
    if value { "an" } else { "aus" }
}

fn describe_duration(seconds: u64) -> String {
    match seconds {
        0..=90 => format!("{seconds} s"),
        _ => format!("{} min", seconds / 60),
    }
}

/// Schreibt eine Zeile nur, wenn sie sich seit der letzten zu diesem Gegenstand geaendert hat.
///
/// Gemeldet wird jede Entscheidung der Automatik, auch jedes Warten - aber eine unveraenderte
/// Begruendung jede Minute erneut zu schreiben, macht das Journal unlesbar und damit nutzlos.
fn log_once(spoken: &mut HashMap<String, String>, key: &str, message: &str) {
    if spoken.get(key).map(String::as_str) == Some(message) {
        return;
    }
    log_event(message);
    spoken.insert(key.to_owned(), message.to_owned());
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("Profilname muss 1 bis 64 Zeichen lang sein");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || name == "."
        || name == ".."
    {
        bail!("Profilname darf nur A-Z, a-z, 0-9, Punkt, Minus und Unterstrich enthalten");
    }
    Ok(())
}

fn prompt_line(label: &str) -> Result<Option<String>> {
    print!("{label}");
    io::stdout()
        .flush()
        .context("Ausgabe konnte nicht geschrieben werden")?;
    let mut input = String::new();
    let bytes = io::stdin()
        .read_line(&mut input)
        .context("Eingabe konnte nicht gelesen werden")?;
    if bytes == 0 {
        return Ok(None);
    }
    Ok(Some(input.trim().to_owned()))
}

fn pause() -> Result<bool> {
    Ok(prompt_line("\nEnter druecken, um zum Menue zurueckzukehren ...")?.is_some())
}

fn read_valid_credentials(path: &Path) -> Result<Vec<u8>> {
    let data = read_credential_bytes(path)?;
    validate_credentials(&data)?;
    Ok(data)
}

fn read_credential_bytes(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("Claude-Credentials nicht gefunden: {}", path.display()))?;
    if !metadata.is_file() {
        bail!(
            "Claude-Credentials sind keine regulaere Datei: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_CREDENTIAL_SIZE {
        bail!("Claude-Credentials sind unerwartet gross");
    }
    let data = fs::read(path).with_context(|| {
        format!(
            "Claude-Credentials konnten nicht gelesen werden: {}",
            path.display()
        )
    })?;
    Ok(data)
}

fn validate_credentials(data: &[u8]) -> Result<()> {
    let value: Value =
        serde_json::from_slice(data).context("Claude-Credentials sind kein gueltiges JSON")?;
    let oauth = value
        .get("claudeAiOauth")
        .and_then(Value::as_object)
        .context("Claude-Credentials enthalten keinen claudeAiOauth-Login")?;
    for field in ["accessToken", "refreshToken"] {
        if oauth
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(|token| token.is_empty())
        {
            bail!("Claude-Credentials enthalten keinen vollstaendigen OAuth-Login");
        }
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| {
        format!(
            "Verzeichnis konnte nicht erstellt werden: {}",
            path.display()
        )
    })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Verzeichnisrechte konnten nicht gesetzt werden: {}",
            path.display()
        )
    })
}

fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("ungueltiger Zielpfad: {}", path.display()))?;
    create_private_dir(parent)?;
    atomic_write_into_existing_dir(path, data, mode)
}

/// Wie `atomic_write`, ohne das Zielverzeichnis anzulegen oder dessen Rechte zu aendern.
/// Fuer Pfade wie `~/.claude.json`, deren Verzeichnis nicht dem Switcher gehoert.
fn atomic_write_into_existing_dir(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("ungueltiger Zielpfad: {}", path.display()))?;
    let stamp = unix_timestamp()?;
    let mut temporary = None;
    for attempt in 0..100_u32 {
        let candidate = parent.join(format!(
            ".{}.{}.{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("credential"),
            std::process::id(),
            stamp + u64::from(attempt)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).context("temporaere Datei konnte nicht erstellt werden");
            }
        }
    }
    let (temporary_path, mut file) = temporary.context("kein freier temporaerer Dateiname")?;
    let result = (|| -> Result<()> {
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary_path, path)?;
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result.with_context(|| {
        format!(
            "Datei konnte nicht atomar geschrieben werden: {}",
            path.display()
        )
    })
}

/// sha256 des Refresh-Tokens. Erkennt eine Rotation, ohne den Token selbst irgendwo abzulegen.
fn credential_fingerprint(credentials: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(credentials)
        .context("Claude-Credentials sind kein gueltiges JSON")?;
    let token = value
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("refreshToken"))
        .and_then(Value::as_str)
        .context("Claude-Credentials enthalten keinen Refresh-Token")?;
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    Ok(format!("{:x}", hasher.finalize()))
}

/// Muss Claude Code diesen Login beim naechsten Request erst erneuern?
///
/// Nur dann kann er ueberhaupt scheitern. Solange der Access-Token gilt, ist der Wechsel
/// risikolos - und ein Pruef-Request waere nur verbrauchte Zeit. Ein Login ohne lesbaren Ablauf
/// gilt als erneuerungsbeduerftig: lieber einmal zu viel geprueft als eine Session zerschossen.
fn needs_refresh(credentials: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(credentials) else {
        return true;
    };
    let Some(expires_at) = value
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("expiresAt"))
        .and_then(Value::as_i64)
    else {
        return true;
    };
    let Ok(now) = unix_timestamp() else {
        return true;
    };
    expires_at / 1000 <= now as i64 + ACCESS_TOKEN_MIN_REMAINING_SECONDS
}

/// Liest den Access-Token aus einem Credential-Stand.
///
/// Der Rueckgabewert ist ein Geheimnis: er darf nur in einen Authorization-Header, nie in ein
/// Log, ein Kommandozeilenargument oder eine Fehlermeldung.
fn read_access_token(credentials: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(credentials)
        .context("Claude-Credentials sind kein gueltiges JSON")?;
    value
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("accessToken"))
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .context("Claude-Credentials enthalten keinen Access-Token")
}

fn read_refresh_token_expiry(path: &Path) -> Result<Option<i64>> {
    let data = read_credential_bytes(path)?;
    let value: Value =
        serde_json::from_slice(&data).context("Claude-Credentials sind kein gueltiges JSON")?;
    Ok(value
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("refreshTokenExpiresAt"))
        .and_then(Value::as_i64)
        .map(|millis| millis / 1000))
}

type CredentialStamp = (i64, u64, u64);

/// Fingerabdruck der Datei selbst. Claude Code schreibt ueber `rename`, deshalb gehoert die
/// Inode dazu: sonst bliebe ein Tausch mit identischer Groesse und Zeit unbemerkt.
fn credential_stamp(path: &Path) -> Option<CredentialStamp> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.mtime(), metadata.len(), metadata.ino()))
}

/// Die eigenen Grenzen eines Accounts als Anhang fuer eine Logzeile. Leer, wenn keine gesetzt
/// sind - dann gilt die globale Schwelle, und die steht ohnehin in jeder Begruendung.
fn describe_stops(stops: &Stops) -> String {
    if !stops.is_set() {
        return String::new();
    }
    let mut text = String::from(", Grenze");
    if let Some(five_hour) = stops.five_hour {
        text.push_str(&format!(" 5h {five_hour:.0}%"));
    }
    if let Some(seven_day) = stops.seven_day {
        text.push_str(&format!(" 7d {seven_day:.0}%"));
    }
    if stops.hard {
        text.push_str(" (hart)");
    }
    text
}

/// Ein Fenster ohne Reset-Zeitpunkt wurde noch nicht angebrochen; das gehoert so in die
/// Ausgabe und nicht als leere Stelle.
fn describe_reset(resets_at: Option<chrono::DateTime<chrono::Utc>>) -> String {
    match resets_at {
        Some(resets_at) => format!("frei ab {}", format_timestamp(resets_at.timestamp())),
        None => "kein laufendes Fenster".to_owned(),
    }
}

fn format_timestamp(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| format!("ungueltiger Zeitstempel ({seconds})"))
}

fn log_event(message: &str) {
    println!("[{}] {message}", chrono::Local::now().format("%F %T"));
    let _ = io::stdout().flush();
}

fn log_problem(message: &str) {
    eprintln!("[{}] {message}", chrono::Local::now().format("%F %T"));
}

/// Holt aus Claudes Fehlerausgabe die Zeile, die den Abbruch erklaert.
///
/// Genommen wird die letzte nicht-leere Zeile: Fortschrittsrauschen und Stacktraces stehen
/// davor, die eigentliche Ursache zuletzt. Die Laenge ist begrenzt, damit eine Logzeile nicht
/// eine ganze Ausgabe ins Journal kippt.
fn claude_reason(path: &Path) -> Option<String> {
    let output = fs::read_to_string(path).ok()?;
    let line = output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let redacted = redact_secrets(line);
    let mut reason: String = redacted.chars().take(STDERR_REASON_MAX_CHARS).collect();
    if redacted.chars().count() > STDERR_REASON_MAX_CHARS {
        reason.push_str(" ...");
    }
    Some(reason)
}

/// Ersetzt alles, was wie ein Token aussieht. Fehlermeldungen zitieren gelegentlich den Wert,
/// der sie ausgeloest hat; im Journal steht er dann dauerhaft.
fn redact_secrets(line: &str) -> String {
    line.split_inclusive(char::is_whitespace)
        .map(|word| {
            let trimmed = word.trim_end();
            if trimmed.len() >= SECRET_WORD_MIN_CHARS
                && trimmed
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                word.replace(trimmed, "[entfernt]")
            } else {
                word.to_string()
            }
        })
        .collect()
}

fn unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("Systemzeit liegt vor 1970")?
        .as_secs())
}
