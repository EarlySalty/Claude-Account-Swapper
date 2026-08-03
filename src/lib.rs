use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
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

pub mod usage;

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
            println!("  [5] Beenden\n");

            let Some(choice) = prompt_line("Auswahl: ")? else {
                return Ok(());
            };
            let action = match choice.as_str() {
                "1" => self.interactive_switch(),
                "2" => self.interactive_save(),
                "3" => self.interactive_login(),
                "4" => self.status(),
                "5" | "q" | "quit" | "exit" => return Ok(()),
                _ => {
                    eprintln!("Fehler: Bitte 1 bis 5 waehlen.");
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
                 ab {threshold:.0}% wird der Account gewechselt"
            )),
            None => log_event("Automatischer Wechsel bei vollem Limit ist abgeschaltet"),
        }

        let mut synced_stamp: Option<CredentialStamp> = None;
        let mut last_problem: Option<String> = None;
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

            if let Some(threshold) = auto_switch_threshold
                && SystemTime::now() >= next_usage_check
            {
                next_usage_check =
                    SystemTime::now() + Duration::from_secs(USAGE_CHECK_INTERVAL_SECONDS);
                if let Err(error) = self.auto_switch_tick(threshold) {
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
