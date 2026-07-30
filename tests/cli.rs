use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::json;
use tempfile::TempDir;

struct Harness {
    _temp: TempDir,
    home: PathBuf,
    store: PathBuf,
    status: PathBuf,
    fake_claude: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let store = temp.path().join("store");
        let status = temp.path().join("status.json");
        let fake_claude = temp.path().join("claude");

        fs::create_dir_all(home.join(".claude")).expect("claude dir");
        fs::write(
            &fake_claude,
            r#"#!/bin/sh
case "$1:$2" in
  auth:status)
    if [ -n "${FAKE_STATUS_CREDENTIALS:-}" ] && [ ! -e "${FAKE_STATUS_CREDENTIALS}.used" ]; then
      /bin/cp "$FAKE_STATUS_CREDENTIALS" "$HOME/.claude/.credentials.json"
      : > "${FAKE_STATUS_CREDENTIALS}.used"
    fi
    /bin/cat "$FAKE_CLAUDE_STATUS"
    ;;
  auth:login)
    if [ "${FAKE_REQUIRE_CLAUDEAI:-0}" -eq 1 ] && [ "$3" != "--claudeai" ]; then
      exit 65
    fi
    if [ "${FAKE_LOGIN_WRITE_BEFORE_EXIT:-0}" -eq 1 ]; then
      /bin/cp "$FAKE_LOGIN_CREDENTIALS" "$HOME/.claude/.credentials.json"
      /bin/cp "$FAKE_LOGIN_STATUS" "$FAKE_CLAUDE_STATUS"
    fi
    if [ "${FAKE_LOGIN_EXIT:-0}" -ne 0 ]; then
      exit "$FAKE_LOGIN_EXIT"
    fi
    if [ "${FAKE_LOGIN_WRITE_BEFORE_EXIT:-0}" -ne 1 ]; then
      /bin/cp "$FAKE_LOGIN_CREDENTIALS" "$HOME/.claude/.credentials.json"
      /bin/cp "$FAKE_LOGIN_STATUS" "$FAKE_CLAUDE_STATUS"
    fi
    ;;
  *)
    exit 64
    ;;
esac
"#,
        )
        .expect("fake claude");
        fs::set_permissions(&fake_claude, fs::Permissions::from_mode(0o700))
            .expect("fake claude mode");

        Self {
            _temp: temp,
            home,
            store,
            status,
            fake_claude,
        }
    }

    fn active_credentials(&self) -> PathBuf {
        self.home.join(".claude/.credentials.json")
    }

    fn saved_credentials(&self, name: &str) -> PathBuf {
        self.store
            .join("accounts")
            .join(name)
            .join("credentials.json")
    }

    fn set_active(&self, email: &str, access: &str, refresh: &str) {
        write_credentials(&self.active_credentials(), access, refresh);
        write_status(&self.status, email);
    }

    fn claude_json(&self) -> PathBuf {
        self.home.join(".claude.json")
    }

    /// Bildet den Identitaets-Cache nach, den Claude Code selbst in `.claude.json` haelt.
    fn write_claude_json(&self, email: &str) {
        let payload = json!({
            "numStartups": 7,
            "userID": format!("hash-of-{email}"),
            "oauthAccount": {
                "emailAddress": email,
                "accountUuid": format!("uuid-of-{email}"),
            }
        });
        fs::write(
            self.claude_json(),
            serde_json::to_vec(&payload).expect("claude json"),
        )
        .expect("write claude json");
    }

    fn read_claude_json(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.claude_json()).expect("read claude json"))
            .expect("parse claude json")
    }

    /// Claude kennt den Login, aber noch keine Identitaet - genau der Zustand direkt
    /// nach einem Wechsel, bevor Claude Code das Profil neu geladen hat.
    fn set_status_without_identity(&self) {
        let payload = json!({
            "loggedIn": true,
            "authMethod": "claude.ai",
            "email": null,
            "subscriptionType": "pro"
        });
        fs::write(
            &self.status,
            serde_json::to_vec(&payload).expect("status json"),
        )
        .expect("write status");
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_claude-account"));
        command
            .env("HOME", &self.home)
            .env("CLAUDE_ACCOUNT_SWITCHER_HOME", &self.store)
            .env("CLAUDE_ACCOUNT_SWITCHER_CLAUDE_BIN", &self.fake_claude)
            .env("FAKE_CLAUDE_STATUS", &self.status)
            .env_remove("CLAUDE_CONFIG_DIR");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("run command")
    }

    fn run_with_input(&self, args: &[&str], input: &str) -> Output {
        let mut command = self.command();
        command.args(args);
        run_command_with_input(command, input)
    }
}

fn run_command_with_input(mut command: Command, input: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn interactive command");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(input.as_bytes())
        .expect("write interactive input");
    child.wait_with_output().expect("interactive output")
}

fn write_credentials(path: &Path, access: &str, refresh: &str) {
    let payload = json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": 4_102_444_800_000_u64,
            "subscriptionType": "pro"
        }
    });
    fs::write(path, serde_json::to_vec(&payload).expect("credential json"))
        .expect("write credentials");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("credential mode");
}

/// Wie `write_credentials`, setzt zusaetzlich den Ablauf des Refresh-Tokens.
fn write_credentials_expiring_in(path: &Path, access: &str, refresh: &str, in_days: i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64;
    let payload = json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": 4_102_444_800_000_u64,
            "refreshTokenExpiresAt": now + in_days * 24 * 60 * 60 * 1000,
            "subscriptionType": "pro"
        }
    });
    fs::write(path, serde_json::to_vec(&payload).expect("credential json"))
        .expect("write credentials");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("credential mode");
}

fn read_profile(harness: &Harness, name: &str) -> serde_json::Value {
    let path = harness
        .store
        .join("accounts")
        .join(name)
        .join("profile.json");
    serde_json::from_slice(&fs::read(path).expect("read profile")).expect("parse profile")
}

fn write_status(path: &Path, email: &str) {
    let payload = json!({
        "loggedIn": true,
        "authMethod": "claude.ai",
        "email": email,
        "orgName": format!("{email} org"),
        "subscriptionType": "pro"
    });
    fs::write(path, serde_json::to_vec(&payload).expect("status json")).expect("write status");
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn save_and_switch_syncs_the_latest_outgoing_credentials() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));

    harness.set_active("work@example.test", "access-work", "refresh-work");
    assert_success(&harness.run(&["save", "work"]));

    harness.set_active(
        "work@example.test",
        "access-work-refreshed",
        "refresh-work-refreshed",
    );
    assert_success(&harness.run(&["switch", "personal"]));

    let active = fs::read(harness.active_credentials()).expect("active credentials");
    let personal = fs::read(harness.saved_credentials("personal")).expect("personal profile");
    let work = fs::read(harness.saved_credentials("work")).expect("work profile");

    assert_eq!(active, personal);
    assert!(
        String::from_utf8(work)
            .expect("utf8")
            .contains("access-work-refreshed")
    );
    assert_eq!(
        fs::metadata(harness.active_credentials())
            .expect("active metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(harness.saved_credentials("work"))
            .expect("saved metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn switch_clears_the_stale_account_identity_cache() {
    let harness = Harness::new();
    harness.set_active("personal@example.test", "access-p", "refresh-p");
    harness.write_claude_json("personal@example.test");
    assert_success(&harness.run(&["save", "personal"]));

    harness.set_active("work@example.test", "access-w", "refresh-w");
    harness.write_claude_json("work@example.test");
    assert_success(&harness.run(&["save", "work"]));

    assert_success(&harness.run(&["switch", "personal"]));

    let claude_json = harness.read_claude_json();
    assert!(
        claude_json.get("oauthAccount").is_none(),
        "oauthAccount haette geleert werden muessen: {claude_json}"
    );
    assert!(
        claude_json.get("userID").is_none(),
        "userID haette geleert werden muessen: {claude_json}"
    );
    assert_eq!(
        claude_json.get("numStartups").and_then(|v| v.as_u64()),
        Some(7),
        "uebrige Claude-Einstellungen duerfen nicht verloren gehen"
    );
    assert_eq!(
        fs::metadata(harness.claude_json())
            .expect("claude json metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn switch_without_a_known_identity_syncs_the_last_active_profile() {
    let harness = Harness::new();
    harness.set_active("personal@example.test", "access-p", "refresh-p");
    assert_success(&harness.run(&["save", "personal"]));
    harness.set_active("work@example.test", "access-w", "refresh-w");
    assert_success(&harness.run(&["save", "work"]));

    // Wechsel hat den Identitaets-Cache geleert; Claude wurde seitdem nicht gestartet,
    // waehrend der aktive Login (work) seine Tokens rotiert hat.
    write_credentials(
        &harness.active_credentials(),
        "access-w-rotated",
        "refresh-w-rotated",
    );
    harness.set_status_without_identity();

    assert_success(&harness.run(&["switch", "personal"]));

    let work =
        String::from_utf8(fs::read(harness.saved_credentials("work")).expect("work profile"))
            .expect("utf8");
    assert!(
        work.contains("refresh-w-rotated"),
        "rotierte Tokens muessen im ausgehenden Profil landen, nicht verloren gehen: {work}"
    );
    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        fs::read(harness.saved_credentials("personal")).expect("personal profile")
    );
}

#[test]
fn switch_refuses_to_sync_when_claude_reports_a_stale_identity() {
    let harness = Harness::new();
    harness.set_active("personal@example.test", "access-p", "refresh-p");
    assert_success(&harness.run(&["save", "personal"]));
    harness.set_active("work@example.test", "access-w", "refresh-w");
    assert_success(&harness.run(&["save", "work"]));

    // Aktiv sind die work-Tokens, Claude meldet aber noch personal - so sieht ein
    // Identitaets-Cache aus, den eine laufende Session zurueckgeschrieben hat.
    write_status(&harness.status, "personal@example.test");
    let personal_before = fs::read(harness.saved_credentials("personal")).expect("personal");
    let work_before = fs::read(harness.saved_credentials("work")).expect("work");
    let active_before = fs::read(harness.active_credentials()).expect("active");

    let output = harness.run(&["switch", "personal"]);

    assert!(
        !output.status.success(),
        "stale Identitaet darf keinen Sync ausloesen: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        fs::read(harness.saved_credentials("personal")).expect("personal"),
        personal_before,
        "fremde Tokens duerfen nicht in ein Profil geschrieben werden"
    );
    assert_eq!(
        fs::read(harness.saved_credentials("work")).expect("work"),
        work_before
    );
    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        active_before
    );
}

#[test]
fn switch_refuses_to_discard_an_unsaved_active_login() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));

    harness.set_active("unknown@example.test", "access-unknown", "refresh-unknown");
    let before = fs::read(harness.active_credentials()).expect("before");
    let output = harness.run(&["switch", "personal"]);

    assert!(!output.status.success());
    assert_eq!(
        fs::read(harness.active_credentials()).expect("after"),
        before
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("save"));
}

#[test]
fn invalid_saved_credentials_never_replace_the_live_login() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));
    harness.set_active("work@example.test", "access-work", "refresh-work");
    assert_success(&harness.run(&["save", "work"]));

    fs::write(harness.saved_credentials("personal"), b"{}\n").expect("corrupt profile");
    let before = fs::read(harness.active_credentials()).expect("before");
    let output = harness.run(&["switch", "personal"]);

    assert!(!output.status.success());
    assert_eq!(
        fs::read(harness.active_credentials()).expect("after"),
        before
    );
}

#[test]
fn login_preserves_the_previous_account_and_saves_the_new_one() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));

    let login_credentials = harness.home.join("login-credentials.json");
    let login_status = harness.home.join("login-status.json");
    write_credentials(&login_credentials, "access-work", "refresh-work");
    write_status(&login_status, "work@example.test");

    let output = harness
        .command()
        .env("FAKE_LOGIN_CREDENTIALS", &login_credentials)
        .env("FAKE_LOGIN_STATUS", &login_status)
        .args(["login", "work"])
        .output()
        .expect("login command");
    assert_success(&output);

    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        fs::read(harness.saved_credentials("work")).expect("saved work")
    );
    assert!(harness.saved_credentials("personal").is_file());
}

#[test]
fn profile_names_cannot_escape_the_account_store() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );

    let output = harness.run(&["save", "../outside"]);

    assert!(!output.status.success());
    assert!(!harness.store.join("outside").exists());
}

#[test]
fn failed_first_login_removes_partially_written_credentials() {
    let harness = Harness::new();
    let login_credentials = harness.home.join("login-credentials.json");
    let login_status = harness.home.join("login-status.json");
    write_credentials(&login_credentials, "partial-access", "partial-refresh");
    write_status(&login_status, "partial@example.test");

    let output = harness
        .command()
        .env("FAKE_LOGIN_CREDENTIALS", &login_credentials)
        .env("FAKE_LOGIN_STATUS", &login_status)
        .env("FAKE_LOGIN_WRITE_BEFORE_EXIT", "1")
        .env("FAKE_LOGIN_EXIT", "1")
        .args(["login", "partial"])
        .output()
        .expect("failed login command");

    assert!(!output.status.success());
    assert!(!harness.active_credentials().exists());
    assert!(!harness.saved_credentials("partial").exists());
}

#[test]
fn login_with_the_wrong_account_restores_the_previous_login() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));
    let before = fs::read(harness.active_credentials()).expect("before");

    let login_credentials = harness.home.join("login-credentials.json");
    let login_status = harness.home.join("login-status.json");
    write_credentials(&login_credentials, "access-wrong", "refresh-wrong");
    write_status(&login_status, "wrong@example.test");
    let output = harness
        .command()
        .env("FAKE_LOGIN_CREDENTIALS", &login_credentials)
        .env("FAKE_LOGIN_STATUS", &login_status)
        .args(["login", "personal"])
        .output()
        .expect("wrong login command");

    assert!(!output.status.success());
    assert_eq!(
        fs::read(harness.active_credentials()).expect("after"),
        before
    );
}

#[test]
fn auth_environment_overrides_block_switching_without_mutation() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));
    let before = fs::read(harness.active_credentials()).expect("before");

    let output = harness
        .command()
        .env("ANTHROPIC_API_KEY", "test-only-placeholder")
        .args(["switch", "personal"])
        .output()
        .expect("override command");

    assert!(!output.status.success());
    assert_eq!(
        fs::read(harness.active_credentials()).expect("after"),
        before
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("ANTHROPIC_API_KEY"));
}

#[test]
fn login_recovers_from_a_logged_out_stale_credential_file() {
    let harness = Harness::new();
    fs::write(harness.active_credentials(), b"{}\n").expect("stale credentials");
    fs::write(&harness.status, br#"{"loggedIn":false}"#).expect("logged out status");

    let login_credentials = harness.home.join("login-credentials.json");
    let login_status = harness.home.join("login-status.json");
    write_credentials(&login_credentials, "access-new", "refresh-new");
    write_status(&login_status, "new@example.test");
    let output = harness
        .command()
        .env("FAKE_LOGIN_CREDENTIALS", &login_credentials)
        .env("FAKE_LOGIN_STATUS", &login_status)
        .env("FAKE_REQUIRE_CLAUDEAI", "1")
        .args(["login", "new"])
        .output()
        .expect("recovery login command");

    assert_success(&output);
    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        fs::read(harness.saved_credentials("new")).expect("saved")
    );
}

#[test]
fn login_preserves_credentials_rotated_by_the_status_check() {
    let harness = Harness::new();
    harness.set_active("personal@example.test", "access-old", "refresh-old");
    assert_success(&harness.run(&["save", "personal"]));

    let rotated_credentials = harness.home.join("rotated-credentials.json");
    let login_credentials = harness.home.join("login-credentials.json");
    let login_status = harness.home.join("login-status.json");
    write_credentials(&rotated_credentials, "access-rotated", "refresh-rotated");
    write_credentials(&login_credentials, "access-work", "refresh-work");
    write_status(&login_status, "work@example.test");

    let output = harness
        .command()
        .env("FAKE_STATUS_CREDENTIALS", &rotated_credentials)
        .env("FAKE_LOGIN_CREDENTIALS", &login_credentials)
        .env("FAKE_LOGIN_STATUS", &login_status)
        .args(["login", "work"])
        .output()
        .expect("rotating login command");

    assert_success(&output);
    let personal = fs::read_to_string(harness.saved_credentials("personal"))
        .expect("saved personal credentials");
    assert!(personal.contains("access-rotated"));
    assert!(personal.contains("refresh-rotated"));
}

#[test]
fn incomplete_profile_directories_do_not_block_valid_accounts() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));
    fs::create_dir_all(harness.store.join("accounts/incomplete")).expect("incomplete dir");
    fs::write(
        harness.store.join("accounts/incomplete/credentials.json"),
        b"{}\n",
    )
    .expect("incomplete credentials");

    let output = harness.run(&["list"]);

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("personal"));
}

#[test]
fn menu_can_save_the_current_account_without_cli_arguments() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );

    let output = harness.run_with_input(&[], "2\npersonal\n\n5\n");

    assert_success(&output);
    assert!(harness.saved_credentials("personal").is_file());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Account wechseln"));
    assert!(stdout.contains("Aktuellen Account speichern"));
    assert!(stdout.contains("Neuen Account anmelden"));
}

#[test]
fn menu_can_switch_between_saved_accounts() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));
    harness.set_active("work@example.test", "access-work", "refresh-work");
    assert_success(&harness.run(&["save", "work"]));

    let output = harness.run_with_input(&[], "1\n1\n\n5\n");

    assert_success(&output);
    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        fs::read(harness.saved_credentials("personal")).expect("personal")
    );
}

#[test]
fn menu_returns_after_an_invalid_action_instead_of_closing() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );

    let output = harness.run_with_input(&[], "2\n../invalid\n\n5\n");

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Fehler:"));
    assert!(!harness.store.join("outside").exists());
}

#[test]
fn menu_can_run_the_one_time_login_flow() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));
    let login_credentials = harness.home.join("login-credentials.json");
    let login_status = harness.home.join("login-status.json");
    write_credentials(&login_credentials, "access-work", "refresh-work");
    write_status(&login_status, "work@example.test");

    let mut command = harness.command();
    command
        .env("FAKE_LOGIN_CREDENTIALS", &login_credentials)
        .env("FAKE_LOGIN_STATUS", &login_status);
    let output = run_command_with_input(command, "3\nwork\n\n5\n");

    assert_success(&output);
    assert!(harness.saved_credentials("work").is_file());
    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        fs::read(harness.saved_credentials("work")).expect("work")
    );
}

#[test]
fn menu_header_uses_the_real_claude_login_not_stale_switcher_state() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));
    harness.set_active(
        "external@example.test",
        "access-external",
        "refresh-external",
    );

    let output = harness.run_with_input(&[], "5\n");

    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Aktiv: external@example.test (nicht gespeichert)")
    );
}

#[test]
fn sync_writes_rotated_live_tokens_into_the_active_profile() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));

    // Claude Code hat die Tokens rotiert, ohne dass der Switcher lief.
    write_credentials(
        &harness.active_credentials(),
        "access-rotated",
        "refresh-rotated",
    );

    let output = harness.run(&["sync"]);

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Aktualisiert: personal"));
    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        fs::read(harness.saved_credentials("personal")).expect("personal profile")
    );
    assert_eq!(
        fs::metadata(harness.saved_credentials("personal"))
            .expect("saved metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn sync_reports_unchanged_credentials_without_rewriting() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));
    let before = fs::metadata(harness.saved_credentials("personal"))
        .expect("saved metadata")
        .modified()
        .expect("mtime");

    let output = harness.run(&["sync"]);

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Bereits aktuell: personal"));
    assert_eq!(
        fs::metadata(harness.saved_credentials("personal"))
            .expect("saved metadata")
            .modified()
            .expect("mtime"),
        before,
        "unveraenderte Credentials duerfen nicht neu geschrieben werden"
    );
}

#[test]
fn sync_refuses_when_claude_reports_a_different_identity() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));
    harness.set_active("work@example.test", "access-work", "refresh-work");
    assert_success(&harness.run(&["save", "work"]));

    // Aktiv ist laut Switcher `work`, Claude meldet aber `personal` - so sieht es aus,
    // wenn eine fremde Session die Live-Datei ueberschrieben hat.
    write_credentials(
        &harness.active_credentials(),
        "access-foreign",
        "refresh-foreign",
    );
    write_status(&harness.status, "personal@example.test");
    let personal_before = fs::read(harness.saved_credentials("personal")).expect("personal");
    let work_before = fs::read(harness.saved_credentials("work")).expect("work");

    let output = harness.run(&["sync"]);

    assert!(
        !output.status.success(),
        "widerspruechliche Identitaet darf nicht synchronisiert werden"
    );
    assert_eq!(
        fs::read(harness.saved_credentials("personal")).expect("personal"),
        personal_before
    );
    assert_eq!(
        fs::read(harness.saved_credentials("work")).expect("work"),
        work_before
    );
}

#[test]
fn sync_without_identity_updates_the_last_active_profile() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));

    // Direkt nach einem Wechsel ist der Identitaets-Cache leer, die Tokens rotieren trotzdem.
    write_credentials(
        &harness.active_credentials(),
        "access-rotated",
        "refresh-rotated",
    );
    harness.set_status_without_identity();

    let output = harness.run(&["sync"]);

    assert_success(&output);
    let personal = String::from_utf8(fs::read(harness.saved_credentials("personal")).expect("read"))
        .expect("utf8");
    assert!(personal.contains("refresh-rotated"), "{personal}");
}

#[test]
fn saved_profile_stores_a_fingerprint_instead_of_the_token() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));

    let profile = read_profile(&harness, "personal");
    let fingerprint = profile
        .get("credential_fingerprint")
        .and_then(|value| value.as_str())
        .expect("Fingerprint fehlt im Profil");
    assert_eq!(fingerprint.len(), 64, "sha256-Hex erwartet: {fingerprint}");
    assert!(
        !serde_json::to_string(&profile)
            .expect("profile json")
            .contains("refresh-personal"),
        "Profil-Metadaten duerfen keinen Klartext-Token enthalten: {profile}"
    );
    assert!(
        profile
            .get("credentials_synced_at")
            .and_then(|value| value.as_u64())
            .is_some_and(|value| value > 1_700_000_000),
        "Sync-Zeitpunkt fehlt: {profile}"
    );

    write_credentials(
        &harness.active_credentials(),
        "access-rotated",
        "refresh-rotated",
    );
    assert_success(&harness.run(&["sync"]));
    let after = read_profile(&harness, "personal");
    assert_ne!(
        after.get("credential_fingerprint"),
        profile.get("credential_fingerprint"),
        "Fingerprint muss der Rotation folgen"
    );
}

#[test]
fn legacy_profiles_without_sync_metadata_still_load() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));

    // Profil im alten Format, wie es vor dem Sync-Feature geschrieben wurde.
    fs::write(
        harness
            .store
            .join("accounts/personal/profile.json"),
        serde_json::to_vec(&json!({
            "name": "personal",
            "email": "personal@example.test",
            "subscription_type": "pro",
            "saved_at": 1_700_000_000_u64
        }))
        .expect("legacy profile"),
    )
    .expect("write legacy profile");

    let output = harness.run(&["list"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("personal"));

    harness.set_active("work@example.test", "access-work", "refresh-work");
    assert_success(&harness.run(&["save", "work"]));
    assert_success(&harness.run(&["switch", "personal"]));
}

#[test]
fn list_warns_about_a_refresh_token_that_expires_soon() {
    let harness = Harness::new();
    write_credentials_expiring_in(
        &harness.active_credentials(),
        "access-personal",
        "refresh-personal",
        3,
    );
    write_status(&harness.status, "personal@example.test");
    assert_success(&harness.run(&["save", "personal"]));

    let output = harness.run(&["list"]);

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("WARNUNG"),
        "bald ablaufender Refresh-Token muss gemeldet werden: {stdout}"
    );
}

#[test]
fn watch_keeps_the_active_profile_in_sync_with_rotated_tokens() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));

    let mut child = harness
        .command()
        .args(["watch", "--interval", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("watch starten");

    std::thread::sleep(std::time::Duration::from_millis(1500));
    write_credentials(
        &harness.active_credentials(),
        "access-rotated",
        "refresh-rotated",
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut synced = false;
    while std::time::Instant::now() < deadline {
        let saved = fs::read_to_string(harness.saved_credentials("personal")).unwrap_or_default();
        if saved.contains("refresh-rotated") {
            synced = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    child.kill().expect("watch beenden");
    let output = child.wait_with_output().expect("watch output");

    assert!(
        synced,
        "watch haette die Rotation sichern muessen\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn watch_logs_every_decision_including_the_ones_without_changes() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );

    // Kein Profil gespeichert: der aktive Login ist nicht zuzuordnen. Auch das muss im Log stehen.
    let mut child = harness
        .command()
        .args(["watch", "--interval", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("watch starten");
    std::thread::sleep(std::time::Duration::from_millis(2500));
    child.kill().expect("watch beenden");
    let output = child.wait_with_output().expect("watch output");

    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        log.contains("Beobachte"),
        "Startzeile mit beobachtetem Pfad fehlt: {log}"
    );
    assert!(
        log.contains("nicht zuzuordnen") || log.contains("nicht gespeichert"),
        "abgelehnte Zuordnung muss protokolliert werden: {log}"
    );
    assert!(
        !log.contains("refresh-personal"),
        "Log darf keine Token enthalten: {log}"
    );
}

#[test]
fn corrupt_state_does_not_close_the_desktop_menu() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));
    fs::write(harness.store.join("state.json"), b"{\n").expect("corrupt state");

    let output = harness.run_with_input(&[], "5\n");

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Warnung:"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Claude Account Swapper"));
}
