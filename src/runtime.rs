use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::routing::{Observation, Rule, SelectContext, Selection};
use crate::{App, Paths, atomic_write, unix_timestamp, validate_name};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedSession {
    pub id: String,
    pub pid: u32,
    pub process_start: String,
    pub kind: String,
    pub cwd: PathBuf,
    pub account: Option<String>,
    pub state: String,
    pub reason: String,
    pub updated_at: u64,
    pub last_switch: u64,
}

impl ManagedSession {
    pub fn new(id: String, kind: &str, cwd: PathBuf) -> Result<Self> {
        validate_name(&id)?;
        Ok(Self {
            id,
            pid: std::process::id(),
            process_start: process_start(std::process::id()).context("Prozesskennung fehlt")?,
            kind: kind.into(),
            cwd,
            account: None,
            state: "starting".into(),
            reason: String::new(),
            updated_at: unix_timestamp()?,
            last_switch: 0,
        })
    }

    pub fn alive(&self) -> bool {
        process_start(self.pid).as_ref() == Some(&self.process_start)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct PollState {
    observation: Option<Observation>,
    retry_at: u64,
    failures: u32,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct KeepaliveState {
    attempted_at: u64,
    succeeded_at: u64,
}

pub struct Lease {
    pub account: String,
    pub directory: PathBuf,
    pub file: std::fs::File,
}

impl App {
    pub fn routing_config(&self) -> Result<Config> {
        match fs::read_to_string(self.config_path()) {
            Ok(text) => Config::parse(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(error).context("Routing-Einstellungen sind nicht lesbar"),
        }
    }

    pub(crate) fn try_account_lock(&self, name: &str) -> Result<Option<std::fs::File>> {
        validate_name(name)?;
        self.try_lock_path(&format!("native-session-{name}.lock"))
    }

    fn native_app(&self, directory: &Path) -> Self {
        Self {
            paths: Paths {
                credentials: directory.join(".credentials.json"),
                claude_json: directory.join(".claude.json"),
                store: self.paths.store.clone(),
                claude_bin: self.paths.claude_bin.clone(),
            },
        }
    }

    fn ensure_native_environment(&self) -> Result<()> {
        self.ensure_no_auth_override()?;
        if let Ok(base) = std::env::var("ANTHROPIC_BASE_URL") {
            ensure!(
                base.is_empty() || base.trim_end_matches('/') == "https://api.anthropic.com",
                "Account-Verteilung ist nur für den offiziellen Claude-Endpunkt eingerichtet"
            );
        }
        for flag in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            ensure!(
                !std::env::var(flag)
                    .is_ok_and(|value| !value.is_empty() && value != "0" && value != "false"),
                "Native Account-Verteilung ist nicht mit {flag} kombinierbar"
            );
        }
        Ok(())
    }

    fn validate_native_directory(&self, name: &str, directory: &Path) -> Result<()> {
        let directory = directory
            .canonicalize()
            .context("Native Konfiguration fehlt")?;
        let global = self
            .paths
            .credentials
            .parent()
            .context("Claude-Verzeichnis fehlt")?
            .canonicalize()
            .ok();
        ensure!(
            global.as_ref() != Some(&directory),
            "Für parallele Accounts ein eigenes Konfigurationsverzeichnis verwenden; der globale Login bleibt beim bisherigen Swapper"
        );
        for (other, path) in &self.routing_config()?.routing.homes {
            if other != name {
                ensure!(
                    path.canonicalize().ok().as_ref() != Some(&directory),
                    "Zwei Account-Namen dürfen nicht dieselbe native Konfiguration verwenden"
                );
            }
        }
        Ok(())
    }

    pub fn bind_native(&self, name: &str, directory: &Path) -> Result<()> {
        self.ensure_native_environment()?;
        let _lease = self
            .try_account_lock(name)?
            .context("Account wird noch von einer Session benutzt")?;
        self.bind_native_locked(name, directory)
    }

    fn bind_native_locked(&self, name: &str, directory: &Path) -> Result<()> {
        validate_name(name)?;
        self.validate_native_directory(name, directory)?;
        let directory = directory
            .canonicalize()
            .context("Claude-Konfigurationsverzeichnis existiert nicht")?;
        ensure!(
            directory.is_dir(),
            "Konfigurationspfad ist kein Verzeichnis"
        );
        let profile = self.load_profile(name)?;
        let output = Command::new(&self.paths.claude_bin)
            .args(["auth", "status", "--json"])
            .env("CLAUDE_CONFIG_DIR", &directory)
            .output()
            .context("Claude-Login konnte nicht geprüft werden")?;
        ensure!(
            output.status.success(),
            "Claude meldet für das Verzeichnis keinen gültigen Login"
        );
        let status: crate::AuthStatus =
            serde_json::from_slice(&output.stdout).context("Claude-Loginstatus ist unlesbar")?;
        ensure!(
            status.logged_in && status.matches(&profile),
            "Dieser Claude-Login gehört nicht zum gewählten gespeicherten Account"
        );
        let config = self.routing_config()?;
        ensure!(
            !config
                .routing
                .homes
                .iter()
                .any(|(other, path)| other != name && path == &directory),
            "Dieses Verzeichnis ist bereits einem anderen Account zugeordnet"
        );
        self.update_config(|config| {
            config.routing.homes.insert(name.into(), directory);
        })?;
        atomic_write(
            &self.poll_path(name),
            &serde_json::to_vec(&PollState::default())?,
            0o600,
        )?;
        Ok(())
    }

    pub fn native_login(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        self.load_profile(name)?;
        self.ensure_native_environment()?;
        let _lease = self.try_account_lock(name)?.context(
            "Account wird noch von einer Session benutzt; Anmeldung erst nach Abschluss starten",
        )?;
        let directory = self.paths.store.join("native").join(name);
        crate::create_private_dir(&directory)?;
        let status = Command::new(&self.paths.claude_bin)
            .args(["auth", "login", "--claudeai"])
            .env("CLAUDE_CONFIG_DIR", &directory)
            .status()
            .context("Offizieller Claude-Login konnte nicht gestartet werden")?;
        ensure!(status.success(), "Claude-Login wurde nicht abgeschlossen");
        self.bind_native_locked(name, &directory)
    }

    pub fn acquire_account(&self, name: &str) -> Result<Option<Lease>> {
        self.ensure_native_environment()?;
        validate_name(name)?;
        let config = self.routing_config()?;
        let directory = config.routing.homes.get(name)
            .with_context(|| format!("Account `{name}` hat noch keine native Session-Konfiguration. `claude-account session-login {name}` richtet sie mit dem offiziellen Login ein"))?.clone();
        self.validate_native_directory(name, &directory)?;
        let Some(file) = self.try_account_lock(name)? else {
            return Ok(None);
        };
        Ok(Some(Lease {
            account: name.into(),
            directory,
            file,
        }))
    }

    pub(crate) fn native_command(&self, lease: &Lease) -> Result<Command> {
        ensure!(
            self.paths
                .claude_bin
                .file_name()
                .is_none_or(|name| name != "claude-account-t3" && name != "claude-account"),
            "Claude-Binary darf nicht auf den Swapper selbst zeigen"
        );
        let mut command = Command::new(&self.paths.claude_bin);
        command.env("CLAUDE_CONFIG_DIR", &lease.directory);
        Ok(command)
    }

    pub fn share_native_setup(&self, name: &str) -> Result<()> {
        let lease = self
            .acquire_account(name)?
            .context("Account wird noch von einer Session benutzt")?;
        let source = self
            .paths
            .credentials
            .parent()
            .context("Claude-Verzeichnis fehlt")?;
        for item in [
            "projects",
            "skills",
            "agents",
            "commands",
            "plugins",
            "settings.json",
            "CLAUDE.md",
        ] {
            let target = source.join(item);
            if !target.exists() {
                continue;
            }
            let link = lease.directory.join(item);
            match fs::symlink_metadata(&link) {
                Ok(metadata) => ensure!(
                    metadata.file_type().is_symlink() && fs::read_link(&link)? == target,
                    "Vorhandener Inhalt bleibt erhalten: {}. Vor dem Teilen manuell zusammenführen",
                    link.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::os::unix::fs::symlink(target, link)?
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn poll_path(&self, name: &str) -> PathBuf {
        self.profile_dir(name).join("routing-usage.json")
    }

    fn read_poll(&self, name: &str) -> PollState {
        fs::read(self.poll_path(name))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn poll_accounts(&self) -> Result<()> {
        self.ensure_no_auth_override()?;
        let Some(_lock) = self.try_lock_file("routing-poll")? else {
            return Ok(());
        };
        let now = unix_timestamp()?;
        let config = self.routing_config()?;
        for (name, directory) in &config.routing.homes {
            let profile = self.load_profile(name)?;
            let mut state = self.read_poll(name);
            if state.retry_at > now {
                continue;
            }
            let result =
                (|| -> Result<(crate::usage::Usage, BTreeMap<String, crate::usage::Bucket>)> {
                    let credentials = crate::read_valid_credentials(&directory.join(".credentials.json"))
                    .context("Native Anmeldung fehlt oder ist unlesbar; kein Rückgriff auf einen anderen Login")?;
                    ensure!(
                        !crate::needs_refresh(&credentials),
                        "Native Anmeldung wartet auf sichere Auffrischung"
                    );
                    let document =
                        crate::usage::fetch_document(&crate::read_access_token(&credentials)?)?;
                    Ok((
                        crate::usage::parse(&document)?,
                        crate::usage::parse_model_windows(&document)?,
                    ))
                })();
            match result {
                Ok((usage, models)) => {
                    let exhausted = usage.five_hour.utilization >= config.threshold
                        || usage.seven_day.utilization >= config.routing.weekly_threshold;
                    state.retry_at = now + if exhausted { 60 } else { 30 };
                    state.failures = 0;
                    state.observation = Some(Observation {
                        name: name.clone(),
                        usage: Some(usage),
                        models,
                        fetched_at: now,
                        stops: profile.limits,
                        busy: false,
                        sessions: 0,
                        healthy: true,
                        problem: None,
                    });
                }
                Err(error) => {
                    state.failures = state.failures.saturating_add(1);
                    state.retry_at =
                        now + (15u64.saturating_mul(1u64 << state.failures.min(5))).min(300);
                    let observation = state.observation.get_or_insert_with(|| Observation {
                        name: name.clone(),
                        usage: None,
                        models: BTreeMap::new(),
                        fetched_at: 0,
                        stops: profile.limits,
                        busy: false,
                        sessions: 0,
                        healthy: false,
                        problem: None,
                    });
                    if error
                        .downcast_ref::<crate::usage::HttpFailure>()
                        .is_some_and(|failure| matches!(failure.0, 401 | 403))
                    {
                        observation.healthy = false;
                    }
                    observation.problem = Some(format!("{error:#}"));
                }
            }
            atomic_write(&self.poll_path(name), &serde_json::to_vec(&state)?, 0o600)?;
        }
        Ok(())
    }

    pub fn observations(&self, own: Option<&str>) -> Result<Vec<Observation>> {
        let sessions = self.managed_sessions()?;
        let config = self.routing_config()?;
        let mut result = Vec::new();
        for profile in self.load_profiles()? {
            let bound = config.routing.homes.contains_key(&profile.name);
            let mut observation =
                self.read_poll(&profile.name)
                    .observation
                    .unwrap_or(Observation {
                        name: profile.name.clone(),
                        usage: None,
                        models: BTreeMap::new(),
                        fetched_at: 0,
                        stops: profile.limits,
                        busy: false,
                        sessions: 0,
                        healthy: false,
                        problem: Some(
                            if bound {
                                "Noch keine Auslastung gelesen"
                            } else {
                                "Native Session-Konfiguration fehlt"
                            }
                            .into(),
                        ),
                    });
            observation.stops = profile.limits;
            observation.sessions = sessions
                .iter()
                .filter(|session| {
                    session.alive() && session.account.as_deref() == Some(&profile.name)
                })
                .count();
            if own == Some(profile.name.as_str()) {
                observation.sessions = observation.sessions.saturating_sub(1);
            }
            observation.busy = own != Some(profile.name.as_str())
                && self.try_account_lock(&profile.name)?.is_none();
            observation.healthy &= bound;
            result.push(observation);
        }
        Ok(result)
    }

    pub fn choose(
        &self,
        session: &ManagedSession,
        explicit: Option<&Rule>,
        own: Option<&str>,
    ) -> Result<Selection> {
        self.poll_accounts()?;
        let config = self.routing_config()?;
        let rule = explicit
            .cloned()
            .unwrap_or_else(|| config.routing.rule(&session.cwd, Some(&session.id)));
        Ok(crate::routing::select(
            &self.observations(own)?,
            &rule,
            &config.routing,
            SelectContext {
                current: session.account.as_deref(),
                last_switch: session.last_switch,
                now: unix_timestamp()?,
                threshold: config.threshold,
            },
        ))
    }

    fn session_path(&self, id: &str) -> PathBuf {
        self.paths
            .store
            .join("runtime")
            .join("sessions")
            .join(format!("{id}.json"))
    }

    pub fn write_managed_session(&self, session: &mut ManagedSession) -> Result<()> {
        validate_name(&session.id)?;
        session.updated_at = unix_timestamp()?;
        atomic_write(
            &self.session_path(&session.id),
            &serde_json::to_vec(session)?,
            0o600,
        )
    }

    pub fn managed_sessions(&self) -> Result<Vec<ManagedSession>> {
        let entries = match fs::read_dir(self.paths.store.join("runtime").join("sessions")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut result = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || entry
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "json")
            {
                continue;
            }
            let mut session: ManagedSession = serde_json::from_slice(&fs::read(entry.path())?)
                .context("Session-Zustand ist beschädigt")?;
            if !session.alive() {
                session.state = "stopped".into();
            }
            result.push(session);
        }
        result.sort_by_key(|entry| std::cmp::Reverse(entry.updated_at));
        result.truncate(200);
        Ok(result)
    }

    pub fn map_project(&self, directory: &Path, rule: Option<Rule>) -> Result<()> {
        let path = directory
            .canonicalize()
            .context("Projektverzeichnis existiert nicht")?;
        ensure!(path.is_dir(), "Projektpfad ist kein Verzeichnis");
        if let Some(rule) = &rule {
            self.validate_accounts(rule)?;
        }
        self.update_config(|config| {
            if let Some(rule) = rule {
                config.routing.projects.insert(path, rule);
            } else {
                config.routing.projects.remove(&path);
            }
        })?;
        Ok(())
    }

    pub fn assign_session(&self, id: &str, rule: Option<Rule>) -> Result<()> {
        validate_name(id)?;
        let sessions = self.managed_sessions()?;
        let session = sessions
            .iter()
            .find(|session| session.id == id)
            .context("Session ist unbekannt")?;
        ensure!(
            session.kind == "t3",
            "Interaktive Terminal-Sessions behalten ihren Start-Account; für einen anderen Account eine neue Terminal-Session starten"
        );
        if let Some(rule) = &rule {
            self.validate_accounts(rule)?;
        }
        self.update_config(|config| {
            if let Some(rule) = rule {
                config.routing.sessions.insert(id.into(), rule);
            } else {
                config.routing.sessions.remove(id);
            }
        })?;
        Ok(())
    }

    fn validate_accounts(&self, rule: &Rule) -> Result<()> {
        crate::routing::validate_rule(rule)?;
        for name in rule.account.iter().chain(rule.pool.iter()) {
            self.load_profile(name)?;
        }
        Ok(())
    }

    pub fn set_account_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        self.load_profile(name)?;
        self.update_config(|config| {
            if enabled {
                config.routing.disabled.remove(name);
            } else {
                config.routing.disabled.insert(name.into());
            }
        })?;
        Ok(())
    }

    pub fn orchestration_state(&self) -> Result<Value> {
        let config = self.routing_config()?;
        Ok(
            serde_json::json!({"now": unix_timestamp()?, "threshold":config.threshold, "routing":config.routing, "accounts":self.observations(None)?, "sessions":self.managed_sessions()?}),
        )
    }

    pub fn run_account(&self, explicit: Option<Rule>, arguments: Vec<String>) -> Result<i32> {
        let cwd = std::env::current_dir()?.canonicalize()?;
        let mut session = ManagedSession::new(uuid::Uuid::new_v4().to_string(), "terminal", cwd)?;
        let rule = explicit.unwrap_or(self.routing_config()?.routing.rule(&session.cwd, None));
        let selection = if let Some(account) = &rule.account {
            Selection::Use {
                account: account.clone(),
                reason: "Explizit gewählter Terminal-Account".into(),
            }
        } else {
            self.choose(&session, Some(&rule), None)?
        };
        let Selection::Use { account, reason } = selection else {
            anyhow::bail!("Kein freier Account; das Dashboard zeigt den Grund");
        };
        let lease = self
            .acquire_account(&account)?
            .context("Account ist belegt; anderen Account wählen oder später erneut starten")?;
        session.account = Some(account.clone());
        session.state = "running".into();
        session.reason = reason;
        session.last_switch = unix_timestamp()?;
        self.write_managed_session(&mut session)?;
        eprintln!("Account: {account} | Session: {}", session.id);
        let mut command = self.native_command(&lease)?;
        command.args(arguments).current_dir(&session.cwd);
        crate::bridge::parent_death_signal(&mut command);
        let result = command
            .status()
            .context("Claude Code konnte nicht gestartet werden");
        drop(lease);
        session.state = "stopped".into();
        self.write_managed_session(&mut session)?;
        Ok(result?.code().unwrap_or(1))
    }
}

impl App {
    pub fn maintain_native(&self) -> Result<()> {
        use std::process::Stdio;
        use std::time::Duration;
        use wait_timeout::ChildExt;
        let Some(_maintenance) = self.try_lock_file("native-maintenance")? else {
            return Ok(());
        };
        let config = self.routing_config()?;
        for (name, directory) in &config.routing.homes {
            let stamp_path = directory.join(".account-keepalive-state.json");
            let mut stamp: KeepaliveState = fs::read(&stamp_path)
                .ok()
                .and_then(|data| serde_json::from_slice(&data).ok())
                .unwrap_or_default();
            let now = unix_timestamp()?;
            if now.saturating_sub(stamp.attempted_at) < 900 {
                continue;
            }
            let native = self.native_app(directory);
            let credentials = match crate::read_valid_credentials(&native.paths.credentials) {
                Ok(credentials) => credentials,
                Err(_) => continue,
            };
            let age = fs::metadata(&native.paths.credentials)?
                .modified()?
                .elapsed()
                .unwrap_or_default();
            if !crate::needs_refresh(&credentials)
                && (age.as_secs() < 6 * 3600 || now.saturating_sub(stamp.succeeded_at) < 6 * 3600)
            {
                continue;
            }
            let Some(lease) = self.acquire_account(name)? else {
                continue;
            };
            stamp.attempted_at = now;
            atomic_write(&stamp_path, &serde_json::to_vec(&stamp)?, 0o600)?;
            let mut command = self.native_command(&lease)?;
            command
                .args([
                    "-p",
                    &config.ping.prompt,
                    "--model",
                    &config.ping.model,
                    "--strict-mcp-config",
                    "--tools",
                    "",
                ])
                .current_dir(&lease.directory)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            crate::bridge::parent_death_signal(&mut command);
            let mut child = command
                .spawn()
                .context("Native Login-Auffrischung konnte nicht gestartet werden")?;
            let status = match child.wait_timeout(Duration::from_secs(
                config.ping.timeout_minutes.saturating_mul(60),
            )) {
                Ok(Some(status)) => status,
                Ok(None) => {
                    let _ = child.kill();
                    child.wait()?
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.into());
                }
            };
            let mut poll = self.read_poll(name);
            if status.success() {
                stamp.succeeded_at = unix_timestamp()?;
                atomic_write(&stamp_path, &serde_json::to_vec(&stamp)?, 0o600)?;
                poll.retry_at = 0;
                poll.failures = 0;
            } else if let Some(observation) = &mut poll.observation {
                observation.problem = Some(format!(
                    "Native Auffrischung fehlgeschlagen ({status}); offizielle Anmeldung prüfen"
                ));
            }
            atomic_write(&self.poll_path(name), &serde_json::to_vec(&poll)?, 0o600)?;
        }
        Ok(())
    }

    pub fn start_native_monitor(&self) {
        let app = App {
            paths: self.paths.clone(),
        };
        std::thread::spawn(move || {
            let mut previous = String::new();
            loop {
                let result = app.maintain_native().and_then(|()| app.poll_accounts());
                let message = result
                    .err()
                    .map(|error| format!("Native Account-Überwachung: {error:#}"))
                    .unwrap_or_default();
                if message != previous && !message.is_empty() {
                    eprintln!("{message}");
                }
                previous = message;
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        });
    }
}

pub fn process_start(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_identity_is_not_just_a_pid() {
        assert!(process_start(std::process::id()).is_some());
        assert!(process_start(u32::MAX).is_none());
    }
}
