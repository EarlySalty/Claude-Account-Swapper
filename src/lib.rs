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

const MAX_CREDENTIAL_SIZE: u64 = 1024 * 1024;
/// Ab dieser Restlaufzeit des Refresh-Tokens warnt `list`.
const REFRESH_TOKEN_WARN_SECONDS: i64 = 7 * 24 * 60 * 60;
pub const DEFAULT_WATCH_INTERVAL_SECONDS: u64 = 5;

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
        validate_name(name)?;
        self.ensure_no_auth_override()?;
        let _lock = self.lock()?;

        let target = self.load_profile(name)?;
        let target_credentials = read_valid_credentials(&self.profile_credentials(name))
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
            println!("{marker} {} - {}{plan}", profile.name, profile.email);
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
        let synced = profile
            .credentials_synced_at
            .map(|value| format_timestamp(value as i64))
            .unwrap_or_else(|| "unbekannt (vor der automatischen Sicherung gespeichert)".to_owned());

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
                lines.push(format!("Snapshot: {synced} | Refresh-Token: Ablauf unbekannt"));
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

        if let Ok(existing) = self.load_profile(name)
            && !status.matches(&existing)
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
        self.sync_locked()
    }

    fn sync_locked(&self) -> Result<SyncOutcome> {
        let status = self.auth_status()?;
        if !status.logged_in {
            bail!("Claude Code ist nicht eingeloggt; nutze `claude-account login <name>`");
        }
        let credentials = read_valid_credentials(&self.paths.credentials)?;
        let profile = self.resolve_active_profile(&status)?;

        let stored = fs::read(self.profile_credentials(&profile.name)).unwrap_or_default();
        if stored == credentials {
            self.refresh_profile_metadata(profile.clone(), &credentials)?;
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
        if profile.credential_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            return Ok(());
        }
        profile.credential_fingerprint = Some(fingerprint);
        profile.credentials_synced_at = Some(unix_timestamp()?);
        self.write_profile(&profile)
    }

    /// Beobachtet die aktive Credential-Datei und sichert jede Rotation sofort ins Profil.
    ///
    /// Jeder Durchlauf wird protokolliert, auch die folgenlosen: eine stille Ablehnung sieht
    /// sonst wie ein erfolgreicher Sync aus.
    pub fn watch(&self, interval_seconds: u64) -> Result<()> {
        let interval = Duration::from_secs(interval_seconds.clamp(1, 3600));
        log_event(&format!(
            "Beobachte {} alle {}s",
            self.paths.credentials.display(),
            interval.as_secs()
        ));

        let mut synced_stamp: Option<CredentialStamp> = None;
        let mut last_problem: Option<String> = None;
        loop {
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

    fn watch_tick(&self) -> Result<Option<SyncOutcome>> {
        self.ensure_no_auth_override()?;
        let Some(_lock) = self.try_lock()? else {
            return Ok(None);
        };
        self.sync_locked().map(Some)
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
        let lock = self.open_lock_file()?;
        FileExt::lock_exclusive(&lock).context("Account-Switcher ist bereits in Benutzung")?;
        Ok(lock)
    }

    /// Wie `lock`, wartet aber nicht. Fuer den Watcher: er darf ein offenes Menue nicht blockieren.
    fn try_lock(&self) -> Result<Option<File>> {
        let lock = self.open_lock_file()?;
        match FileExt::try_lock_exclusive(&lock) {
            Ok(()) => Ok(Some(lock)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error).context("Lock konnte nicht geprueft werden"),
        }
    }

    fn open_lock_file(&self) -> Result<File> {
        self.ensure_store()?;
        let lock_path = self.paths.store.join(".lock");
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

fn unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("Systemzeit liegt vor 1970")?
        .as_secs())
}
