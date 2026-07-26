use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_CREDENTIAL_SIZE: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Paths {
    pub credentials: PathBuf,
    pub store: PathBuf,
    pub claude_bin: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .context("HOME ist nicht gesetzt")?;
        let claude_config = env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
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
    fn email(&self) -> Result<&str> {
        if !self.logged_in {
            bail!("Claude Code ist nicht eingeloggt; nutze `claude-account login <name>`");
        }
        self.email
            .as_deref()
            .filter(|email| !email.trim().is_empty())
            .context("Claude meldet einen Login ohne E-Mail-Adresse")
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
        if let Err(error) = self.write_state(&State {
            current: Some(target.name.clone()),
        }) {
            atomic_write(&self.paths.credentials, &outgoing_credentials, 0o600)
                .context("Rollback der aktiven Claude-Credentials ist fehlgeschlagen")?;
            return Err(error).context(
                "Profilstatus konnte nicht gespeichert werden; Wechsel wurde zurueckgerollt",
            );
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
        }
        Ok(())
    }

    pub fn status(&self) -> Result<()> {
        self.ensure_no_auth_override()?;
        let status = self.auth_status()?;
        let email = status.email()?;
        let profile_name = self
            .load_profiles()?
            .into_iter()
            .find(|profile| status.matches(profile))
            .map(|profile| profile.name)
            .unwrap_or_else(|| "nicht gespeichert".to_owned());
        let plan = status.subscription_type.as_deref().unwrap_or("unbekannt");
        println!("Aktiver Account: {email}");
        println!("Profil: {profile_name}");
        println!("Abo: {plan}");
        if let Some(org_name) = status.org_name.as_deref() {
            println!("Organisation: {org_name}");
        }
        Ok(())
    }

    pub fn interactive(&self) -> Result<()> {
        let profiles = self.load_profiles()?;
        if profiles.is_empty() {
            println!("Keine Accounts gespeichert. Starte mit: claude-account save <name>");
            return Ok(());
        }
        let current = self.load_state()?.current;
        println!("\nClaude Accounts\n---------------");
        for (index, profile) in profiles.iter().enumerate() {
            let marker = if current.as_deref() == Some(profile.name.as_str()) {
                " *"
            } else {
                ""
            };
            println!(
                "{}.{} {} ({})",
                index + 1,
                marker,
                profile.name,
                profile.email
            );
        }
        print!("\nAccount waehlen: ");
        io::stdout()
            .flush()
            .context("Ausgabe konnte nicht geschrieben werden")?;
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Auswahl konnte nicht gelesen werden")?;
        let selected = input
            .trim()
            .parse::<usize>()
            .context("bitte eine Account-Nummer eingeben")?;
        let profile = profiles
            .get(selected.saturating_sub(1))
            .context("ungueltige Account-Nummer")?;
        self.switch(&profile.name)
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
        let matches: Vec<Profile> = self
            .load_profiles()?
            .into_iter()
            .filter(|profile| status.matches(profile))
            .collect();
        let profile = match matches.as_slice() {
            [profile] => profile.clone(),
            [] => {
                let email = status.email()?;
                bail!(
                    "aktiver Login {email} ist nicht gespeichert; zuerst `claude-account save <name>` ausfuehren"
                );
            }
            _ => bail!("aktiver Login passt zu mehreren Profilen; doppelte Profile bereinigen"),
        };
        atomic_write(&self.profile_credentials(&profile.name), credentials, 0o600)?;
        self.write_state(&State {
            current: Some(profile.name.clone()),
        })?;
        Ok(profile)
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
        FileExt::lock_exclusive(&lock).context("Account-Switcher ist bereits in Benutzung")?;
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

fn unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("Systemzeit liegt vor 1970")?
        .as_secs())
}
