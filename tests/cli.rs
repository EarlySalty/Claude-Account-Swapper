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
  -p:*)
    # Haelt fest, womit und wo aufgerufen wurde. Nur so laesst sich pruefen, dass eine Aufgabe
    # wirklich mit ihrem Auftrag und in ihrem Ordner startet - und nicht irgendwo anders.
    if [ -n "${FAKE_ARGS_FILE:-}" ]; then
      printf 'PWD=%s\n' "$PWD" >>"$FAKE_ARGS_FILE"
      printf 'ARG=%s\n' "$@" >>"$FAKE_ARGS_FILE"
    fi
    # Bildet einen echten Request nach: Claude Code erneuert dabei den Login in dem
    # Konfigurationsverzeichnis, das gerade gilt. Ein verbrauchter Token wird geleert.
    target="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/.credentials.json"
    if /bin/grep -q "refresh-dead" "$target" 2>/dev/null; then
      printf '{"claudeAiOauth":{"accessToken":"","refreshToken":"","expiresAt":0}}' >"$target"
      echo "Failed to authenticate: OAuth session expired and could not be refreshed"
      exit 0
    fi
    if [ "${FAKE_PROMPT_EXIT:-0}" -ne 0 ]; then
      if [ -n "${FAKE_PROMPT_STDERR:-}" ]; then
        echo "$FAKE_PROMPT_STDERR" >&2
      fi
      exit "$FAKE_PROMPT_EXIT"
    fi
    if [ -n "${FAKE_PROMPT_TOUCH_ACTIVE:-}" ]; then
      /bin/cp "$FAKE_PROMPT_TOUCH_ACTIVE" "$HOME/.claude/.credentials.json"
    fi
    if [ "${FAKE_PROMPT_NOOP:-0}" -eq 1 ]; then
      echo ok
      exit 0
    fi
    # Der Token rotiert, bevor die Antwort kommt - ab hier ist der alte serverseitig tot.
    /bin/sed -e 's/"accessToken":"[^"]*"/"accessToken":"access-renewed"/' \
      -e 's/"refreshToken":"[^"]*"/"refreshToken":"refresh-renewed"/' \
      "$target" >"$target.renewed"
    /bin/mv "$target.renewed" "$target"
    if [ -n "${FAKE_PROMPT_HANG:-}" ]; then
      sleep "$FAKE_PROMPT_HANG"
    fi
    echo ok
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
    let personal =
        String::from_utf8(fs::read(harness.saved_credentials("personal")).expect("read"))
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
        harness.store.join("accounts/personal/profile.json"),
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
fn watch_waits_instead_of_guessing_while_claude_has_no_identity() {
    let harness = Harness::new();
    harness.set_active(
        "personal@example.test",
        "access-personal",
        "refresh-personal",
    );
    assert_success(&harness.run(&["save", "personal"]));
    let saved_before = fs::read(harness.saved_credentials("personal")).expect("personal");

    // Genau die Luecke direkt nach einem Wechsel: Claude kennt die Identitaet noch nicht.
    // Eine fremde Session koennte die Live-Datei geschrieben haben - der Dienst darf dann
    // nicht auf den Switcher-Status zurueckfallen und fremde Tokens einsortieren.
    write_credentials(
        &harness.active_credentials(),
        "access-foreign",
        "refresh-foreign",
    );
    harness.set_status_without_identity();

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

    assert_eq!(
        fs::read(harness.saved_credentials("personal")).expect("personal"),
        saved_before,
        "ohne bestaetigte Identitaet darf nichts gesichert werden"
    );
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        log.contains("Kontodaten"),
        "Grund fuers Aussetzen muss im Log stehen: {log}"
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
fn keepalive_renews_an_idle_profile_without_touching_the_active_login() {
    let harness = Harness::new();
    harness.set_active("idle@example.test", "access-idle", "refresh-idle");
    assert_success(&harness.run(&["save", "idle"]));
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));
    let active_before = fs::read(harness.active_credentials()).expect("active");
    let aktiv_before = fs::read(harness.saved_credentials("aktiv")).expect("aktiv");

    let output = harness.run(&["keepalive", "--max-age-days", "0"]);

    assert_success(&output);
    let idle = String::from_utf8(fs::read(harness.saved_credentials("idle")).expect("idle"))
        .expect("utf8");
    assert!(
        idle.contains("refresh-renewed"),
        "das untaetige Profil haette erneuert werden muessen: {idle}"
    );
    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        active_before,
        "der aktive Login darf sich nicht aendern"
    );
    assert_eq!(
        fs::read(harness.saved_credentials("aktiv")).expect("aktiv"),
        aktiv_before,
        "das aktive Profil versorgt der Watcher, nicht die Auffrischung"
    );
    assert_eq!(
        fs::metadata(harness.saved_credentials("idle"))
            .expect("idle metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn keepalive_keeps_an_expired_profile_untouched_and_says_so() {
    let harness = Harness::new();
    harness.set_active("tot@example.test", "access-dead", "refresh-dead");
    assert_success(&harness.run(&["save", "tot"]));
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));
    let dead_before = fs::read(harness.saved_credentials("tot")).expect("tot");

    let output = harness.run(&["keepalive", "--max-age-days", "0"]);

    assert_success(&output);
    assert_eq!(
        fs::read(harness.saved_credentials("tot")).expect("tot"),
        dead_before,
        "ein abgelaufenes Profil darf nicht mit leeren Tokens ueberschrieben werden"
    );
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        log.contains("tot") && log.contains("neuen Login"),
        "der Fehlschlag muss benannt werden: {log}"
    );
}

#[test]
fn keepalive_skips_profiles_that_were_recently_synced() {
    let harness = Harness::new();
    harness.set_active("frisch@example.test", "access-frisch", "refresh-frisch");
    assert_success(&harness.run(&["save", "frisch"]));
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));
    let frisch_before = fs::read(harness.saved_credentials("frisch")).expect("frisch");

    let output = harness.run(&["keepalive"]);

    assert_success(&output);
    assert_eq!(
        fs::read(harness.saved_credentials("frisch")).expect("frisch"),
        frisch_before,
        "gerade gesicherte Profile brauchen keinen Request"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("frisch"),
        "auch das Ueberspringen gehoert ins Log"
    );
}

#[test]
fn keepalive_saves_the_renewed_login_even_if_the_active_one_changes_meanwhile() {
    let harness = Harness::new();
    harness.set_active("idle@example.test", "access-idle", "refresh-idle");
    assert_success(&harness.run(&["save", "idle"]));
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));

    // Eine beliebige andere Claude-Session verlaengert waehrenddessen ihren eigenen Login.
    // Das ist alle paar Stunden normal und hat mit der Auffrischung nichts zu tun. Der
    // erneuerte Token des Profils ist zu diesem Zeitpunkt serverseitig schon rotiert - wer
    // ihn jetzt verwirft, hat das Profil endgueltig getoetet.
    let interference = harness.home.join("interference.json");
    write_credentials(&interference, "access-other", "refresh-other");
    let output = harness
        .command()
        .args(["keepalive", "--max-age-days", "0"])
        .env("FAKE_PROMPT_TOUCH_ACTIVE", &interference)
        .output()
        .expect("keepalive");

    assert_success(&output);
    let idle = String::from_utf8(fs::read(harness.saved_credentials("idle")).expect("idle"))
        .expect("utf8");
    assert!(
        idle.contains("refresh-renewed"),
        "der erneuerte Login muss gesichert werden, egal was der aktive Login tut: {idle}"
    );
}

#[test]
fn keepalive_rescues_the_renewed_login_when_the_request_runs_into_the_timeout() {
    let harness = Harness::new();
    harness.set_active("idle@example.test", "access-idle", "refresh-idle");
    assert_success(&harness.run(&["save", "idle"]));
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));

    // Claude rotiert den Token, bevor es antwortet, und haengt danach - etwa in einem
    // Retry-Backoff. Der alte Token ist zu dem Zeitpunkt schon tot; wer den neuen wegen des
    // Timeouts wegwirft, macht das Profil unrettbar.
    let output = harness
        .command()
        .args(["keepalive", "--max-age-days", "0"])
        .env("FAKE_PROMPT_HANG", "30")
        .env("CLAUDE_ACCOUNT_SWITCHER_KEEPALIVE_TIMEOUT", "1")
        .output()
        .expect("keepalive");

    assert_success(&output);
    let idle = String::from_utf8(fs::read(harness.saved_credentials("idle")).expect("idle"))
        .expect("utf8");
    assert!(
        idle.contains("refresh-renewed"),
        "der rotierte Token muss trotz Timeout gesichert werden: {idle}"
    );
}

#[test]
fn keepalive_reports_a_failed_request_instead_of_calling_it_valid() {
    let harness = Harness::new();
    harness.set_active("idle@example.test", "access-idle", "refresh-idle");
    assert_success(&harness.run(&["save", "idle"]));
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));

    // Ein alter Sicherungszeitpunkt, wie ihn ein laenger untaetiges Profil traegt.
    let profile_path = harness.store.join("accounts/idle/profile.json");
    let mut profile: serde_json::Value =
        serde_json::from_slice(&fs::read(&profile_path).expect("profile")).expect("json");
    profile["credentials_synced_at"] = json!(1_700_000_000_u64);
    fs::write(&profile_path, serde_json::to_vec(&profile).expect("json")).expect("write profile");

    // Claude bricht ab, ohne etwas zu erneuern - Netzfehler, Rate-Limit, was auch immer.
    // Der Snapshot ist unveraendert, aber die 30-Tage-Uhr laeuft weiter. Wer das als
    // "noch gueltig" verbucht, laesst den Account still sterben und meldet dabei gruen.
    let output = harness
        .command()
        .args(["keepalive", "--max-age-days", "0"])
        .env("FAKE_PROMPT_EXIT", "1")
        .output()
        .expect("keepalive");

    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        log.contains("idle") && !log.contains("noch gueltig"),
        "ein gescheiterter Request darf nicht als gueltig gemeldet werden: {log}"
    );
    assert_eq!(
        read_profile(&harness, "idle")
            .get("credentials_synced_at")
            .and_then(|value| value.as_u64()),
        Some(1_700_000_000),
        "nach einem Fehlschlag darf die Altersuhr nicht zurueckgesetzt werden"
    );
}

#[test]
fn a_failed_request_reports_what_claude_wrote_to_stderr() {
    let harness = Harness::new();
    harness.set_active("idle@example.test", "access-idle", "refresh-idle");
    assert_success(&harness.run(&["save", "idle"]));
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));

    // Ohne Claudes eigene Begruendung ist "endete mit exit status: 1" nicht diagnostizierbar:
    // Rate-Limit, abgelaufener Login und Netzfehler sehen identisch aus. Der Grund steht auf
    // stderr und muss bis in die Meldung durchkommen.
    let output = harness
        .command()
        .args(["keepalive", "--max-age-days", "0"])
        .env("FAKE_PROMPT_EXIT", "1")
        .env("FAKE_PROMPT_STDERR", "Claude AI usage limit reached")
        .output()
        .expect("keepalive");

    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        log.contains("Claude AI usage limit reached"),
        "der Grund von Claude muss in der Meldung stehen: {log}"
    );
}

#[test]
fn a_reported_reason_never_carries_a_token_into_the_log() {
    let harness = Harness::new();
    harness.set_active("idle@example.test", "access-idle", "refresh-idle");
    assert_success(&harness.run(&["save", "idle"]));
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));

    // Fehlermeldungen zitieren gelegentlich den Wert, an dem sie gescheitert sind. Im Journal
    // stuende der dann dauerhaft - und das Journal liest jeder, der Logs anschaut.
    let secret = "sk-ant-oat01-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let output = harness
        .command()
        .args(["keepalive", "--max-age-days", "0"])
        .env("FAKE_PROMPT_EXIT", "1")
        .env("FAKE_PROMPT_STDERR", format!("invalid bearer {secret}"))
        .output()
        .expect("keepalive");

    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !log.contains(secret) && log.contains("invalid bearer"),
        "der Grund gehoert ins Log, der Token nicht: {log}"
    );
}

#[test]
fn keepalive_does_not_hunt_a_profile_that_needs_no_renewal() {
    let harness = Harness::new();
    harness.set_active("frisch@example.test", "access-frisch", "refresh-frisch");
    assert_success(&harness.run(&["save", "frisch"]));
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));

    // Erster Lauf: faellig, aber Claude sieht keinen Grund zu erneuern (Token unveraendert).
    let mut command = harness.command();
    command.args(["keepalive", "--max-age-days", "0"]);
    command.env("FAKE_PROMPT_NOOP", "1");
    assert_success(&command.output().expect("keepalive"));

    // Zweiter Lauf mit normaler Altersgrenze: das Profil gilt jetzt als frisch geprueft.
    let output = harness.run(&["keepalive"]);
    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Tage alt"),
        "ein gerade geprueftes Profil darf nicht erneut beschossen werden: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn keepalive_never_renews_the_login_that_is_currently_live() {
    let harness = Harness::new();
    harness.set_active("live@example.test", "access-live", "refresh-live");
    assert_success(&harness.run(&["save", "live"]));
    let live_before = fs::read(harness.saved_credentials("live")).expect("live");

    // Ein veralteter Switcher-Status zeigt auf ein anderes Profil, obwohl die Tokens von
    // `live` aktiv sind. Ohne Byte-Vergleich wuerde die Auffrischung den Token rotieren,
    // den gerade eine laufende Session benutzt.
    fs::write(
        harness.store.join("state.json"),
        br#"{"current":"gibt-es-nicht"}"#,
    )
    .expect("state");

    let output = harness.run(&["keepalive", "--max-age-days", "0"]);

    assert_success(&output);
    assert_eq!(
        fs::read(harness.saved_credentials("live")).expect("live"),
        live_before,
        "der aktuell benutzte Login darf nicht aufgefrischt werden"
    );
    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        live_before,
        "und der aktive Login schon gar nicht"
    );
}

#[test]
fn keepalive_leaves_no_working_directory_behind() {
    let harness = Harness::new();
    harness.set_active("idle@example.test", "access-idle", "refresh-idle");
    assert_success(&harness.run(&["save", "idle"]));
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));

    assert_success(&harness.run(&["keepalive", "--max-age-days", "0"]));

    // Ein Arbeitsverzeichnis haelt eine Kopie aktiver Zugangsdaten; nur `accounts` darf bleiben.
    let leftovers: Vec<_> = fs::read_dir(&harness.store)
        .expect("store")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "accounts")
        .collect();
    assert!(
        leftovers.is_empty(),
        "Arbeitsverzeichnisse mit Credentials duerfen nicht liegen bleiben: {leftovers:?}"
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

/// Schreibt Credentials mit frei waehlbarem Ablauf des Access-Tokens. Ein abgelaufener
/// Access-Token zwingt Claude Code beim naechsten Request zu einem Refresh - genau dort
/// entscheidet sich, ob ein Snapshot noch traegt oder jede laufende Session in einen 401 reisst.
fn write_credentials_expiring_access(path: &Path, access: &str, refresh: &str, in_minutes: i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64;
    let payload = json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": now + in_minutes * 60 * 1000,
            "subscriptionType": "pro"
        }
    });
    fs::write(path, serde_json::to_vec(&payload).expect("credential json"))
        .expect("write credentials");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("credential mode");
}

/// Legt ein Profil an, dessen Access-Token bereits abgelaufen ist.
fn save_profile_with_stale_access(harness: &Harness, name: &str, email: &str, refresh: &str) {
    write_credentials_expiring_access(&harness.active_credentials(), "access-stale", refresh, -60);
    write_status(&harness.status, email);
    assert_success(&harness.run(&["save", name]));
}

#[test]
fn switch_refuses_a_dead_snapshot_instead_of_breaking_running_sessions() {
    let harness = Harness::new();
    save_profile_with_stale_access(&harness, "tot", "tot@example.test", "refresh-dead");
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));
    harness.write_claude_json("aktiv@example.test");
    let active_before = fs::read(harness.active_credentials()).expect("active");
    let identity_before = harness.read_claude_json();

    let output = harness.run(&["switch", "tot"]);

    assert!(
        !output.status.success(),
        "ein toter Snapshot darf nicht live gesetzt werden: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        active_before,
        "der laufende Login muss unangetastet bleiben"
    );
    assert_eq!(
        harness.read_claude_json(),
        identity_before,
        "ohne Wechsel darf auch der Identitaets-Cache nicht geleert werden"
    );
    let message = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        message.contains("claude-account login tot") && message.contains("laeuft weiter"),
        "die Meldung muss den Ausweg nennen und beruhigen: {message}"
    );
}

#[test]
fn switch_puts_the_renewed_login_live_when_the_snapshot_needed_a_refresh() {
    let harness = Harness::new();
    save_profile_with_stale_access(&harness, "alt", "alt@example.test", "refresh-alt");
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));

    let output = harness.run(&["switch", "alt"]);

    assert_success(&output);
    let live =
        String::from_utf8(fs::read(harness.active_credentials()).expect("active")).expect("utf8");
    assert!(
        live.contains("refresh-renewed"),
        "live gehoert der erneuerte Stand, nicht der verbrauchte: {live}"
    );
    let saved =
        String::from_utf8(fs::read(harness.saved_credentials("alt")).expect("alt")).expect("utf8");
    assert!(
        saved.contains("refresh-renewed"),
        "der rotierte Token muss auch im Profil stehen, sonst ist er beim naechsten Mal weg: {saved}"
    );
}

#[test]
fn switch_spends_no_request_while_the_snapshot_is_still_valid() {
    let harness = Harness::new();
    harness.set_active("erst@example.test", "access-erst", "refresh-erst");
    assert_success(&harness.run(&["save", "erst"]));
    harness.set_active("zweit@example.test", "access-zweit", "refresh-zweit");
    assert_success(&harness.run(&["save", "zweit"]));
    let saved_before = fs::read(harness.saved_credentials("erst")).expect("erst");

    assert_success(&harness.run(&["switch", "erst"]));

    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        saved_before,
        "ein gueltiger Snapshot wandert unveraendert live; ein Request waere nur verbrauchte Zeit"
    );
}

#[test]
fn switch_no_check_keeps_the_old_unverified_way() {
    let harness = Harness::new();
    save_profile_with_stale_access(&harness, "tot", "tot@example.test", "refresh-dead");
    let dead_snapshot = fs::read(harness.saved_credentials("tot")).expect("tot");
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));

    assert_success(&harness.run(&["switch", "tot", "--no-check"]));

    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        dead_snapshot,
        "mit --no-check bleibt es beim ungeprueften Tausch"
    );
}

#[test]
fn sync_writes_the_state_even_when_the_snapshot_already_matches() {
    let harness = Harness::new();
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));
    // Zustand nach dem allerersten Start des Dienstes: der Switcher hat noch nichts aktiviert.
    fs::write(harness.store.join("state.json"), b"{\"current\":null}").expect("state");

    assert_success(&harness.run(&["sync"]));

    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(harness.store.join("state.json")).expect("state"))
            .expect("state json");
    assert_eq!(
        state.get("current").and_then(serde_json::Value::as_str),
        Some("aktiv"),
        "auch ohne Byte-Aenderung muss der Switcher wissen, wer aktiv ist: {state}"
    );
}

#[test]
fn list_marks_a_profile_whose_renewal_failed() {
    let harness = Harness::new();
    save_profile_with_stale_access(&harness, "tot", "tot@example.test", "refresh-dead");
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));
    assert_success(&harness.run(&["keepalive", "--max-age-days", "0"]));

    let output = harness.run(&["list"]);

    assert_success(&output);
    let listing = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        listing.contains("braucht einen neuen Login"),
        "ein gescheiterter Versuch darf nicht als Gueltigkeit durchgehen: {listing}"
    );
}

#[test]
fn list_forgets_the_mark_after_a_successful_renewal() {
    let harness = Harness::new();
    save_profile_with_stale_access(&harness, "tot", "tot@example.test", "refresh-dead");
    harness.set_active("aktiv@example.test", "access-aktiv", "refresh-aktiv");
    assert_success(&harness.run(&["save", "aktiv"]));
    assert_success(&harness.run(&["keepalive", "--max-age-days", "0"]));

    // Der Nutzer hat sich neu eingeloggt; der Snapshot traegt wieder.
    write_credentials(
        &harness.saved_credentials("tot"),
        "access-neu",
        "refresh-neu",
    );
    assert_success(&harness.run(&["keepalive", "--max-age-days", "0"]));

    let output = harness.run(&["list"]);
    assert_success(&output);
    let listing = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        !listing.contains("braucht einen neuen Login"),
        "nach einem geglueckten Versuch muss die Markierung verschwinden: {listing}"
    );
}

#[test]
fn profiles_written_before_this_version_still_load() {
    let harness = Harness::new();
    harness.set_active("alt@example.test", "access-alt", "refresh-alt");
    assert_success(&harness.run(&["save", "alt"]));
    let path = harness
        .store
        .join("accounts")
        .join("alt")
        .join("profile.json");
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "name": "alt",
            "email": "alt@example.test",
            "saved_at": 1_700_000_000
        }))
        .expect("legacy profile"),
    )
    .expect("write legacy profile");

    let output = harness.run(&["list"]);

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("alt@example.test"));
}

#[test]
fn switch_to_the_active_account_never_rotates_the_live_login() {
    let harness = Harness::new();
    // Der aktive Login muss demnaechst verlaengert werden - genau die Lage, in der ein
    // Pruef-Request ihn serverseitig rotieren wuerde. Passiert das ohne dass der erneuerte
    // Stand live geschrieben wird, sind alle offenen Sessions ausgesperrt.
    write_credentials_expiring_access(
        &harness.active_credentials(),
        "access-bald-abgelaufen",
        "refresh-aktiv",
        -60,
    );
    write_status(&harness.status, "aktiv@example.test");
    assert_success(&harness.run(&["save", "aktiv"]));
    let live_before = fs::read(harness.active_credentials()).expect("active");

    let output = harness.run(&["switch", "aktiv"]);

    assert_success(&output);
    assert_eq!(
        fs::read(harness.active_credentials()).expect("active"),
        live_before,
        "der live benutzte Login darf beim Wechsel auf sich selbst nicht rotiert werden"
    );
    assert_eq!(
        fs::read(harness.saved_credentials("aktiv")).expect("aktiv"),
        live_before,
        "und im Profil ebenso wenig"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Bereits aktiv"));
}

/// Ein lokaler Ersatz fuer die Auslastungs-API. Antwortet je nach Access-Token, damit ein Test
/// mehrere Accounts mit unterschiedlicher Auslastung nebeneinander stellen kann.
struct UsageServer {
    url: String,
    server: std::sync::Arc<tiny_http::Server>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl UsageServer {
    fn start(responses: Vec<(&str, u16, String)>) -> Self {
        let server = std::sync::Arc::new(
            tiny_http::Server::http("127.0.0.1:0").expect("usage server startet"),
        );
        let url = format!("http://{}/usage", server.server_addr());
        let responses: Vec<(String, u16, String)> = responses
            .into_iter()
            .map(|(token, status, body)| (token.to_owned(), status, body))
            .collect();
        let listener = std::sync::Arc::clone(&server);
        let worker = std::thread::spawn(move || {
            for request in listener.incoming_requests() {
                let token = request
                    .headers()
                    .iter()
                    .find(|header| header.field.equiv("Authorization"))
                    .map(|header| {
                        header
                            .value
                            .as_str()
                            .trim_start_matches("Bearer ")
                            .to_owned()
                    })
                    .unwrap_or_default();
                let (status, body) = responses
                    .iter()
                    .find(|(expected, _, _)| *expected == token)
                    .map(|(_, status, body)| (*status, body.clone()))
                    .unwrap_or_else(|| (401, r#"{"error":"unbekannter Token"}"#.to_owned()));
                let response = tiny_http::Response::from_string(body).with_status_code(status);
                let _ = request.respond(response);
            }
        });
        Self {
            url,
            server,
            worker: Some(worker),
        }
    }
}

impl UsageServer {
    /// Antwortet der Reihe nach; die letzte Antwort gilt danach weiter.
    ///
    /// Genau das braucht der Fenster-Ping: vor ihm meldet die API ein ungenutztes Fenster, nach
    /// ihm ein laufendes. Ein Server mit nur einer Antwort koennte den Unterschied nie zeigen.
    fn start_sequence(token: &str, bodies: Vec<String>) -> Self {
        let server = std::sync::Arc::new(
            tiny_http::Server::http("127.0.0.1:0").expect("usage server startet"),
        );
        let url = format!("http://{}/usage", server.server_addr());
        let token = token.to_owned();
        let listener = std::sync::Arc::clone(&server);
        let worker = std::thread::spawn(move || {
            let mut served = 0usize;
            for request in listener.incoming_requests() {
                let matches = request.headers().iter().any(|header| {
                    header.field.equiv("Authorization")
                        && header.value.as_str().trim_start_matches("Bearer ") == token
                });
                let response = if matches {
                    let body = bodies[served.min(bodies.len() - 1)].clone();
                    served += 1;
                    tiny_http::Response::from_string(body).with_status_code(200)
                } else {
                    tiny_http::Response::from_string(r#"{"error":"unbekannter Token"}"#)
                        .with_status_code(401)
                };
                let _ = request.respond(response);
            }
        });
        Self {
            url,
            server,
            worker: Some(worker),
        }
    }
}

impl Drop for UsageServer {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn usage_body(five: f64, seven: f64) -> String {
    usage_body_with_resets(five, seven, "2026-08-03T20:00:00Z", "2026-08-09T02:00:00Z")
}

fn usage_body_with_resets(five: f64, seven: f64, five_reset: &str, seven_reset: &str) -> String {
    json!({
        "five_hour": {"utilization": five, "resets_at": five_reset, "limit_dollars": null},
        "seven_day": {"utilization": seven, "resets_at": seven_reset},
        "seven_day_opus": null,
        "limits": [{"kind": "session", "percent": five, "severity": "normal"}]
    })
    .to_string()
}

/// Legt zwei gespeicherte Accounts an; `work` ist danach aktiv.
fn harness_with_two_accounts() -> Harness {
    let harness = Harness::new();
    harness.set_active("personal@example.test", "access-personal", "refresh-p");
    harness.write_claude_json("personal@example.test");
    assert_success(&harness.run(&["save", "personal"]));

    harness.set_active("work@example.test", "access-work", "refresh-w");
    harness.write_claude_json("work@example.test");
    assert_success(&harness.run(&["save", "work"]));
    harness
}

#[test]
fn auto_switches_to_the_account_with_free_limits() {
    let harness = harness_with_two_accounts();
    let server = UsageServer::start(vec![
        ("access-work", 200, usage_body(100.0, 40.0)),
        ("access-personal", 200, usage_body(12.0, 30.0)),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto"])
        .output()
        .expect("auto");
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Wechsel zu personal"), "{stdout}");
    assert!(
        stdout.contains("5h 100%"),
        "Begruendung ohne Zahlen: {stdout}"
    );

    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(harness.store.join("state.json")).expect("state"))
            .expect("state json");
    assert_eq!(
        state.get("current").and_then(|v| v.as_str()),
        Some("personal")
    );
    assert_eq!(
        fs::read(harness.active_credentials()).expect("live"),
        fs::read(harness.saved_credentials("personal")).expect("snapshot"),
        "der Login von personal muss live sein"
    );
}

#[test]
fn auto_keeps_the_active_account_below_the_threshold() {
    let harness = harness_with_two_accounts();
    let server = UsageServer::start(vec![
        ("access-work", 200, usage_body(97.0, 40.0)),
        ("access-personal", 200, usage_body(0.0, 0.0)),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto"])
        .output()
        .expect("auto");
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Kein Wechsel"), "{stdout}");
    assert!(stdout.contains("5h 97%"), "{stdout}");
    assert_eq!(
        fs::read(harness.active_credentials()).expect("live"),
        fs::read(harness.saved_credentials("work")).expect("snapshot"),
        "es darf nichts gewechselt worden sein"
    );
}

#[test]
fn auto_dry_run_reports_without_switching() {
    let harness = harness_with_two_accounts();
    let server = UsageServer::start(vec![
        ("access-work", 200, usage_body(100.0, 40.0)),
        ("access-personal", 200, usage_body(5.0, 5.0)),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto", "--dry-run"])
        .output()
        .expect("auto");
    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Wuerde wechseln zu personal"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        fs::read(harness.active_credentials()).expect("live"),
        fs::read(harness.saved_credentials("work")).expect("snapshot"),
        "dry-run darf nichts anfassen"
    );
}

#[test]
fn auto_ignores_an_account_whose_weekly_limit_is_full() {
    let harness = Harness::new();
    harness.set_active("personal@example.test", "access-personal", "refresh-p");
    harness.write_claude_json("personal@example.test");
    assert_success(&harness.run(&["save", "personal"]));
    harness.set_active("spare@example.test", "access-spare", "refresh-s");
    harness.write_claude_json("spare@example.test");
    assert_success(&harness.run(&["save", "spare"]));
    harness.set_active("work@example.test", "access-work", "refresh-w");
    harness.write_claude_json("work@example.test");
    assert_success(&harness.run(&["save", "work"]));

    let server = UsageServer::start(vec![
        ("access-work", 200, usage_body(100.0, 40.0)),
        // Frei im Fuenf-Stunden-Fenster, aber das Wochenlimit ist voll: kein Ziel.
        ("access-personal", 200, usage_body(1.0, 99.0)),
        ("access-spare", 200, usage_body(60.0, 60.0)),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto", "--dry-run"])
        .output()
        .expect("auto");
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Wuerde wechseln zu spare"), "{stdout}");
}

#[test]
fn auto_picks_the_earliest_reset_when_every_account_is_full() {
    let harness = harness_with_two_accounts();
    let server = UsageServer::start(vec![
        (
            "access-work",
            200,
            usage_body_with_resets(100.0, 40.0, "2026-08-03T23:00:00Z", "2026-08-09T02:00:00Z"),
        ),
        (
            "access-personal",
            200,
            usage_body_with_resets(100.0, 40.0, "2026-08-03T20:00:00Z", "2026-08-09T02:00:00Z"),
        ),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto", "--dry-run"])
        .output()
        .expect("auto");
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Wuerde wechseln zu personal"), "{stdout}");
    assert!(stdout.contains("alle Accounts sind voll"), "{stdout}");
}

#[test]
fn auto_reports_an_unreachable_usage_api_and_switches_nothing() {
    let harness = harness_with_two_accounts();
    let server = UsageServer::start(vec![
        ("access-work", 500, r#"{"error":"kaputt"}"#.to_owned()),
        ("access-personal", 200, usage_body(1.0, 1.0)),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto"])
        .output()
        .expect("auto");
    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HTTP 500"),
        "Fehler muss sichtbar sein: {stderr}"
    );
    assert!(
        stderr.contains("Kein Wechsel"),
        "die Nicht-Entscheidung muss protokolliert sein: {stderr}"
    );
    assert_eq!(
        fs::read(harness.active_credentials()).expect("live"),
        fs::read(harness.saved_credentials("work")).expect("snapshot"),
        "ohne Zahlen darf nicht gewechselt werden"
    );
}

#[test]
fn auto_never_leaks_the_access_token_into_its_output() {
    let harness = harness_with_two_accounts();
    let server = UsageServer::start(vec![("access-personal", 200, usage_body(1.0, 1.0))]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto"])
        .output()
        .expect("auto");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !text.contains("access-work") && !text.contains("access-personal"),
        "Access-Token darf nirgends auftauchen: {text}"
    );
}

#[test]
fn usage_lists_every_account_and_shows_failures() {
    let harness = harness_with_two_accounts();
    let server = UsageServer::start(vec![
        ("access-work", 200, usage_body(42.0, 17.0)),
        ("access-personal", 503, r#"{"error":"weg"}"#.to_owned()),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["usage"])
        .output()
        .expect("usage");
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("42.0%"), "{stdout}");
    assert!(stdout.contains("17.0%"), "{stdout}");
    assert!(
        stdout.contains("nicht abrufbar") && stdout.contains("HTTP 503"),
        "ein Ausfall muss sichtbar sein: {stdout}"
    );
}

#[test]
fn auto_rejects_an_impossible_threshold() {
    let harness = harness_with_two_accounts();
    let output = harness.run(&["auto", "--threshold", "140"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("zwischen 0 und 100"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn limit_stores_its_own_stops_in_the_profile() {
    let harness = harness_with_two_accounts();
    assert_success(&harness.run(&["limit", "personal", "--five-hour", "80"]));
    assert_success(&harness.run(&["limit", "personal", "--seven-day", "50", "--hard"]));

    let profile = read_profile(&harness, "personal");
    let limits = profile.get("limits").expect("limits");
    assert_eq!(limits.get("five_hour").and_then(|v| v.as_f64()), Some(80.0));
    assert_eq!(limits.get("seven_day").and_then(|v| v.as_f64()), Some(50.0));
    assert_eq!(limits.get("hard").and_then(|v| v.as_bool()), Some(true));

    assert_success(&harness.run(&["limit", "personal", "--clear"]));
    let profile = read_profile(&harness, "personal");
    let limits = profile.get("limits").expect("limits");
    assert!(limits.get("five_hour").is_none(), "{limits}");
    assert!(limits.get("seven_day").is_none(), "{limits}");
}

#[test]
fn saving_an_account_again_keeps_its_limits() {
    let harness = harness_with_two_accounts();
    assert_success(&harness.run(&["limit", "work", "--five-hour", "70"]));

    // Erneutes Speichern sichert Tokens; die Konfiguration darf es nicht zuruecksetzen.
    assert_success(&harness.run(&["save", "work"]));
    let profile = read_profile(&harness, "work");
    assert_eq!(
        profile
            .get("limits")
            .and_then(|limits| limits.get("five_hour"))
            .and_then(|v| v.as_f64()),
        Some(70.0)
    );
}

#[test]
fn auto_leaves_an_account_at_its_own_stop() {
    let harness = harness_with_two_accounts();
    assert_success(&harness.run(&["limit", "work", "--five-hour", "80"]));
    let server = UsageServer::start(vec![
        ("access-work", 200, usage_body(85.0, 20.0)),
        ("access-personal", 200, usage_body(90.0, 20.0)),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto", "--dry-run"])
        .output()
        .expect("auto");
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Ohne eigene Grenze bliebe `work` bei 85% stehen; mit ihr wechselt er auf den
    // absolut voelleren, aber unbegrenzten Account.
    assert!(stdout.contains("Wuerde wechseln zu personal"), "{stdout}");
    assert!(stdout.contains("5h 85%/80%"), "{stdout}");
}

#[test]
fn auto_never_switches_onto_an_account_beyond_its_own_stop() {
    let harness = Harness::new();
    harness.set_active("personal@example.test", "access-personal", "refresh-p");
    harness.write_claude_json("personal@example.test");
    assert_success(&harness.run(&["save", "personal"]));
    harness.set_active("spare@example.test", "access-spare", "refresh-s");
    harness.write_claude_json("spare@example.test");
    assert_success(&harness.run(&["save", "spare"]));
    harness.set_active("work@example.test", "access-work", "refresh-w");
    harness.write_claude_json("work@example.test");
    assert_success(&harness.run(&["save", "work"]));
    assert_success(&harness.run(&["limit", "personal", "--five-hour", "30"]));

    let server = UsageServer::start(vec![
        ("access-work", 200, usage_body(100.0, 20.0)),
        // Absolut der freieste, aber ueber seiner eigenen Grenze.
        ("access-personal", 200, usage_body(40.0, 20.0)),
        ("access-spare", 200, usage_body(70.0, 20.0)),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto", "--dry-run"])
        .output()
        .expect("auto");
    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Wuerde wechseln zu spare"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn auto_breaks_into_a_soft_reserve_when_nothing_else_is_left() {
    let harness = harness_with_two_accounts();
    assert_success(&harness.run(&["limit", "personal", "--five-hour", "30"]));
    let server = UsageServer::start(vec![
        ("access-work", 200, usage_body(100.0, 20.0)),
        ("access-personal", 200, usage_body(40.0, 20.0)),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto", "--dry-run"])
        .output()
        .expect("auto");
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Wuerde wechseln zu personal"), "{stdout}");
    assert!(stdout.contains("Reserve"), "{stdout}");
}

#[test]
fn auto_respects_a_hard_stop_even_with_nothing_else_left() {
    let harness = harness_with_two_accounts();
    assert_success(&harness.run(&["limit", "personal", "--five-hour", "30", "--hard"]));
    let server = UsageServer::start(vec![
        (
            "access-work",
            200,
            usage_body_with_resets(100.0, 20.0, "2026-08-03T20:00:00Z", "2026-08-09T02:00:00Z"),
        ),
        (
            "access-personal",
            200,
            usage_body_with_resets(40.0, 20.0, "2026-08-03T23:00:00Z", "2026-08-09T02:00:00Z"),
        ),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto", "--dry-run"])
        .output()
        .expect("auto");
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Kein Wechsel"), "{stdout}");
}

#[test]
fn limit_rejects_impossible_values_and_unknown_profiles() {
    let harness = harness_with_two_accounts();
    let output = harness.run(&["limit", "work", "--five-hour", "150"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("zwischen 0 und 100"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = harness.run(&["limit", "gibtsnicht", "--five-hour", "50"]);
    assert!(!output.status.success());
}

fn write_cached_usage(harness: &Harness, name: &str, five: f64, seven: f64, age_seconds: u64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let payload = json!({
        "five_hour": {"utilization": five, "resets_at": "2026-08-03T20:00:00Z"},
        "seven_day": {"utilization": seven, "resets_at": "2026-08-09T02:00:00Z"},
        "fetched_at": now - age_seconds
    });
    fs::write(
        harness.store.join("accounts").join(name).join("usage.json"),
        serde_json::to_vec(&payload).expect("usage json"),
    )
    .expect("write usage cache");
}

/// Der Endpunkt ist selbst ratenbegrenzt. Faellt er mit 429 aus, darf nicht ausgerechnet der
/// Account als Ziel wegfallen, der bewertet werden muss - dafuer gibt es den gemerkten Stand.
#[test]
fn auto_falls_back_to_the_remembered_usage_when_the_api_rate_limits() {
    let harness = harness_with_two_accounts();
    write_cached_usage(&harness, "work", 100.0, 20.0, 300);
    write_cached_usage(&harness, "personal", 5.0, 5.0, 300);
    let server = UsageServer::start(vec![
        (
            "access-work",
            429,
            r#"{"error":{"type":"rate_limit_error"}}"#.to_owned(),
        ),
        (
            "access-personal",
            429,
            r#"{"error":{"type":"rate_limit_error"}}"#.to_owned(),
        ),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto", "--dry-run"])
        .output()
        .expect("auto");
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("Wuerde wechseln zu personal"), "{stdout}");
    assert!(
        stdout.contains("Stand von vor 5 min"),
        "das Alter muss dranstehen: {stdout}"
    );
    assert!(
        stderr.contains("HTTP 429"),
        "der Ausfall muss sichtbar sein: {stderr}"
    );
}

/// Zu alte gemerkte Zahlen sind schlechter als keine: sie wuerden einen Wechsel auf einen
/// laengst vollen Account begruenden.
#[test]
fn auto_ignores_a_stale_remembered_usage() {
    let harness = harness_with_two_accounts();
    write_cached_usage(&harness, "work", 100.0, 20.0, 60 * 60 * 3);
    write_cached_usage(&harness, "personal", 5.0, 5.0, 60 * 60 * 3);
    let server = UsageServer::start(vec![
        ("access-work", 429, r#"{"error":{}}"#.to_owned()),
        ("access-personal", 429, r#"{"error":{}}"#.to_owned()),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto"])
        .output()
        .expect("auto");
    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Kein Wechsel"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(harness.active_credentials()).expect("live"),
        fs::read(harness.saved_credentials("work")).expect("snapshot"),
        "mit veralteten Zahlen darf nicht gewechselt werden"
    );
}

/// Ein junger gemerkter Stand spart die Anfrage komplett - der Endpunkt wird gar nicht erst
/// belastet. Der Test beweist es, indem der Server jede Anfrage mit 500 quittieren wuerde.
#[test]
fn auto_uses_a_fresh_remembered_usage_without_asking_again() {
    let harness = harness_with_two_accounts();
    write_cached_usage(&harness, "work", 10.0, 10.0, 5);
    let server = UsageServer::start(vec![("access-work", 500, r#"{"error":{}}"#.to_owned())]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto", "--dry-run"])
        .output()
        .expect("auto");
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Kein Wechsel"), "{stdout}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("HTTP 500"),
        "es haette gar keine Anfrage geben duerfen: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Solange der aktive Account unter seiner Grenze ist, werden die anderen gar nicht erst
/// abgefragt: jede vermeidbare Anfrage erhoeht das Risiko, spaeter keine Antwort zu bekommen.
#[test]
fn auto_asks_no_other_account_while_the_active_one_is_fine() {
    let harness = harness_with_two_accounts();
    let server = UsageServer::start(vec![
        ("access-work", 200, usage_body(10.0, 10.0)),
        ("access-personal", 500, r#"{"error":{}}"#.to_owned()),
    ]);

    let output = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["auto", "--dry-run"])
        .output()
        .expect("auto");
    assert_success(&output);
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("HTTP 500"),
        "personal haette nicht abgefragt werden duerfen: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("Auslastung personal"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Der automatische Wechsel greift in laufende Sitzungen ein und ist deshalb opt-in. Ohne das
/// Flag darf der Dienst die Auslastung nicht einmal abfragen.
#[test]
fn watch_switches_nothing_without_the_opt_in() {
    let harness = harness_with_two_accounts();
    let server = UsageServer::start(vec![
        ("access-work", 200, usage_body(100.0, 100.0)),
        ("access-personal", 200, usage_body(0.0, 0.0)),
    ]);

    let mut child = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
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
        log.contains("Auto-Wechsel aus"),
        "der Dienst muss seinen Zustand nennen: {log}"
    );
    assert!(
        !log.contains("Auslastung work"),
        "ohne Opt-in darf nichts abgefragt werden: {log}"
    );
    assert_eq!(
        fs::read(harness.active_credentials()).expect("live"),
        fs::read(harness.saved_credentials("work")).expect("snapshot"),
        "ohne Opt-in darf nichts gewechselt werden"
    );
}

/// Mit dem Flag wechselt derselbe Aufbau sehr wohl - sonst wuerde der Test oben auch dann
/// bestehen, wenn der automatische Wechsel gar nicht mehr funktioniert.
#[test]
fn watch_switches_with_the_opt_in() {
    let harness = harness_with_two_accounts();
    let server = UsageServer::start(vec![
        ("access-work", 200, usage_body(100.0, 100.0)),
        ("access-personal", 200, usage_body(0.0, 0.0)),
    ]);

    let mut child = harness
        .command()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["watch", "--interval", "1", "--auto-switch"])
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

    assert!(log.contains("Wechsel zu personal"), "{log}");
    assert_eq!(
        fs::read(harness.active_credentials()).expect("live"),
        fs::read(harness.saved_credentials("personal")).expect("snapshot"),
        "mit Opt-in muss gewechselt werden"
    );
}

/// Die Schwelle allein schaltet nichts ein - das waere eine stille Falle.
#[test]
fn watch_rejects_a_threshold_without_the_opt_in() {
    let harness = harness_with_two_accounts();
    let output = harness.run(&["watch", "--auto-switch-threshold", "90"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("auto-switch"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---- Aufgaben, Fenster-Ping und Menue ----

impl Harness {
    fn job_file(&self, id: &str) -> PathBuf {
        self.store.join("jobs").join(format!("{id}.json"))
    }

    fn read_job(&self, id: &str) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.job_file(id)).expect("Aufgabe lesen"))
            .expect("Aufgabe ist JSON")
    }

    fn args_file(&self) -> PathBuf {
        self.home.join("fake-args.txt")
    }

    /// Womit und wo die Claude-CLI wirklich aufgerufen wurde.
    fn recorded_calls(&self) -> String {
        fs::read_to_string(self.args_file()).unwrap_or_default()
    }

    /// Ein Aufruf, bei dem die gefakte CLI ihre Argumente mitschreibt und den Login in Ruhe
    /// laesst - hier interessiert der Aufruf selbst, nicht die Token-Rotation.
    fn command_recording(&self) -> Command {
        let mut command = self.command();
        command
            .env("FAKE_ARGS_FILE", self.args_file())
            .env("FAKE_PROMPT_NOOP", "1");
        command
    }
}

/// Ein Fenster, in dem noch nichts verbraucht wurde: die API nennt dann keinen Reset-Zeitpunkt.
fn unused_window_body() -> String {
    json!({
        "five_hour": {"utilization": 0.0, "resets_at": null},
        "seven_day": {"utilization": 30.0, "resets_at": "2026-08-09T02:00:00Z"}
    })
    .to_string()
}

#[test]
fn a_job_runs_with_its_own_prompt_in_its_own_directory() {
    let harness = harness_with_two_accounts();
    let workdir = harness.home.join("projekt");
    fs::create_dir_all(&workdir).expect("Arbeitsverzeichnis");

    assert_success(&harness.run(&[
        "job",
        "add",
        "Baue das Ding",
        "--cwd",
        workdir.to_str().expect("Pfad"),
    ]));
    assert_success(
        &harness
            .command_recording()
            .args(["job", "run", "0001"])
            .output()
            .expect("job run"),
    );

    let calls = harness.recorded_calls();
    assert!(
        calls.contains(&format!("PWD={}", workdir.display())),
        "die Aufgabe muss in ihrem Ordner laufen: {calls}"
    );
    assert!(calls.contains("ARG=-p"), "{calls}");
    assert!(calls.contains("ARG=Baue das Ding"), "{calls}");
    assert!(
        calls.contains("ARG=--dangerously-skip-permissions"),
        "eine unbeaufsichtigte Aufgabe darf nicht auf eine Rueckfrage warten: {calls}"
    );

    let log = fs::read_to_string(harness.store.join("jobs").join("0001.log")).expect("Logdatei");
    let head = log
        .lines()
        .find(|line| line.contains("Start:"))
        .expect("Kopfzeile");
    assert!(
        !head.contains("Baue das Ding"),
        "der Auftrag gehoert nicht in die Kopfzeile: {head}"
    );
    assert!(
        !log.contains("access-work") && !log.contains("refresh-w"),
        "im Log darf kein Token stehen: {log}"
    );
}

#[test]
fn a_resume_job_hands_the_session_id_to_claude() {
    let harness = harness_with_two_accounts();
    assert_success(&harness.run(&[
        "job",
        "resume",
        "abc-123",
        "--prompt",
        "Mach weiter",
        "--cwd",
        harness.home.to_str().expect("Pfad"),
    ]));
    assert_success(
        &harness
            .command_recording()
            .args(["job", "run", "0001"])
            .output()
            .expect("job run"),
    );

    let calls = harness.recorded_calls();
    assert!(calls.contains("ARG=--resume"), "{calls}");
    assert!(calls.contains("ARG=abc-123"), "{calls}");
    assert!(calls.contains("ARG=Mach weiter"), "{calls}");
}

#[test]
fn a_one_off_job_is_done_after_its_run_a_repeating_one_stays() {
    let harness = harness_with_two_accounts();
    let home = harness.home.to_str().expect("Pfad").to_owned();
    assert_success(&harness.run(&["job", "add", "einmal", "--cwd", &home]));
    assert_success(&harness.run(&["job", "add", "immer", "--cwd", &home, "--repeat"]));

    for id in ["0001", "0002"] {
        assert_success(
            &harness
                .command_recording()
                .args(["job", "run", id])
                .output()
                .expect("job run"),
        );
    }

    let einmal = harness.read_job("0001");
    assert_eq!(einmal["enabled"], json!(false), "{einmal}");
    assert!(
        einmal["last_status"]
            .as_str()
            .expect("Status")
            .contains("erfolgreich"),
        "{einmal}"
    );
    let immer = harness.read_job("0002");
    assert_eq!(immer["enabled"], json!(true), "{immer}");
}

/// Ein gescheiterter Lauf darf die Aufgabe nicht verbrauchen - sonst waere sie nach einem
/// Rate-Limit still weg, und niemand haette den Auftrag je erledigt.
#[test]
fn a_failed_run_keeps_the_job_and_records_the_reason() {
    let harness = harness_with_two_accounts();
    assert_success(&harness.run(&[
        "job",
        "add",
        "wird scheitern",
        "--cwd",
        harness.home.to_str().expect("Pfad"),
    ]));

    let output = harness
        .command_recording()
        .env("FAKE_PROMPT_EXIT", "3")
        .env("FAKE_PROMPT_STDERR", "Claude AI usage limit reached")
        .args(["job", "run", "0001"])
        .output()
        .expect("job run");
    assert!(
        !output.status.success(),
        "ein gescheiterter Lauf muss auch als Fehler zurueckkommen"
    );

    let job = harness.read_job("0001");
    assert_eq!(job["enabled"], json!(true), "{job}");
    let status = job["last_status"].as_str().expect("Status");
    assert!(status.contains("endete mit"), "{status}");
    assert!(status.contains("usage limit reached"), "{status}");
}

#[test]
fn a_job_without_its_directory_is_switched_off_with_a_reason() {
    let harness = harness_with_two_accounts();
    let workdir = harness.home.join("weg");
    fs::create_dir_all(&workdir).expect("Arbeitsverzeichnis");
    assert_success(&harness.run(&[
        "job",
        "add",
        "egal",
        "--cwd",
        workdir.to_str().expect("Pfad"),
    ]));
    fs::remove_dir_all(&workdir).expect("Arbeitsverzeichnis entfernen");

    let output = harness
        .command_recording()
        .args(["job", "run", "0001"])
        .output()
        .expect("job run");
    assert!(!output.status.success());
    assert!(
        harness.recorded_calls().is_empty(),
        "ohne Arbeitsverzeichnis darf Claude gar nicht erst starten"
    );

    let job = harness.read_job("0001");
    assert_eq!(job["enabled"], json!(false), "{job}");
    assert!(
        job["last_status"]
            .as_str()
            .expect("Status")
            .contains("Arbeitsverzeichnis fehlt"),
        "{job}"
    );
}

/// Eine haengende Aufgabe darf den Platz nicht auf Dauer belegen; das Zeitlimit ist die einzige
/// Bremse, die auch dann noch greift, wenn Claude selbst nicht mehr antwortet.
#[test]
fn a_hanging_job_is_stopped_by_its_time_limit() {
    let harness = harness_with_two_accounts();
    assert_success(&harness.run(&[
        "job",
        "add",
        "haengt",
        "--cwd",
        harness.home.to_str().expect("Pfad"),
    ]));

    let output = harness
        .command_recording()
        // Der kurze Weg der gefakten CLI endet sofort; hier ist gerade das Haengen der Punkt.
        .env("FAKE_PROMPT_NOOP", "0")
        .env("FAKE_PROMPT_HANG", "30")
        .env("CLAUDE_ACCOUNT_SWITCHER_JOB_TIMEOUT", "1")
        .args(["job", "run", "0001"])
        .output()
        .expect("job run");
    assert!(!output.status.success());

    let job = harness.read_job("0001");
    assert!(
        job["last_status"]
            .as_str()
            .expect("Status")
            .contains("Abbruch nach Zeitlimit"),
        "{job}"
    );
    assert_eq!(job["enabled"], json!(true), "{job}");
}

#[test]
fn settings_survive_a_restart_and_a_broken_file_falls_back_to_defaults() {
    let harness = Harness::new();
    assert_success(&harness.run(&["config", "--ping", "on", "--auto-switch", "on"]));

    let shown = harness.run(&["config"]);
    assert_success(&shown);
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(text.contains("Fenster-Ping nach dem Reset: an"), "{text}");
    assert!(text.contains("Auto-Wechsel bei vollem Limit: an"), "{text}");

    fs::write(harness.store.join("config.json"), "kein json").expect("Einstellungen kaputtmachen");
    let shown = harness.run(&["config"]);
    assert_success(&shown);
    assert!(
        String::from_utf8_lossy(&shown.stdout).contains("Fenster-Ping nach dem Reset: aus"),
        "eine kaputte Datei muss auf die Standardwerte fallen"
    );
    assert!(
        String::from_utf8_lossy(&shown.stderr).contains("unlesbar"),
        "und das muss gemeldet werden"
    );
}

/// Der eigentliche Zweck des Pings: das Fenster startet mit der ersten Anfrage. Der Dienst
/// schickt sie, sobald die API kein laufendes Fenster mehr meldet - und beweist danach am
/// frischen Reset-Zeitpunkt, dass wirklich eines geoeffnet wurde.
#[test]
fn the_service_opens_a_fresh_window_with_a_ping() {
    let harness = harness_with_two_accounts();
    assert_success(&harness.run(&["config", "--ping", "on"]));
    let server = UsageServer::start_sequence(
        "access-work",
        vec![
            unused_window_body(),
            usage_body_with_resets(4.0, 30.0, "2026-08-04T23:00:00Z", "2026-08-09T02:00:00Z"),
        ],
    );

    let mut child = harness
        .command_recording()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["watch", "--interval", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("watch starten");
    std::thread::sleep(std::time::Duration::from_millis(3000));
    child.kill().expect("watch beenden");
    let output = child.wait_with_output().expect("watch output");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(log.contains("Fenster-Ping faellig"), "{log}");
    assert!(
        log.contains("Fenster laeuft jetzt bis"),
        "der Beweis fehlt: {log}"
    );
    let calls = harness.recorded_calls();
    assert!(calls.contains("ARG=Bist du da?"), "{calls}");
    assert!(calls.contains("ARG=haiku"), "{calls}");
}

#[test]
fn the_service_starts_a_due_job_and_marks_it_done() {
    let harness = harness_with_two_accounts();
    assert_success(&harness.run(&[
        "job",
        "add",
        "starte von selbst",
        "--cwd",
        harness.home.to_str().expect("Pfad"),
    ]));
    let server = UsageServer::start_sequence("access-work", vec![usage_body(10.0, 30.0)]);

    let mut child = harness
        .command_recording()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["watch", "--interval", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("watch starten");
    std::thread::sleep(std::time::Duration::from_millis(3000));
    child.kill().expect("watch beenden");
    let output = child.wait_with_output().expect("watch output");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(log.contains("Aufgabe [0001] faellig"), "{log}");
    assert!(
        harness.recorded_calls().contains("ARG=starte von selbst"),
        "die Aufgabe muss wirklich gestartet worden sein: {log}"
    );
    let job = harness.read_job("0001");
    assert_eq!(job["enabled"], json!(false), "{job}");
}

/// Ohne Aufgaben und ohne Ping darf der Dienst die ratenbegrenzte Auslastungs-API gar nicht
/// erst anfassen.
#[test]
fn the_service_asks_for_usage_only_when_it_has_something_to_do() {
    let harness = harness_with_two_accounts();
    let server = UsageServer::start_sequence("access-work", vec![usage_body(10.0, 30.0)]);

    let mut child = harness
        .command_recording()
        .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &server.url)
        .args(["watch", "--interval", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("watch starten");
    std::thread::sleep(std::time::Duration::from_millis(2000));
    child.kill().expect("watch beenden");
    let output = child.wait_with_output().expect("watch output");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(log.contains("Auto-Wechsel aus"), "{log}");
    assert!(!log.contains("Auslastung work"), "{log}");
    assert!(harness.recorded_calls().is_empty(), "{log}");
}

#[test]
fn the_menu_offers_usage_limits_and_automation() {
    let harness = harness_with_two_accounts();
    let output = harness.run_with_input(&[], "8\n");
    assert_success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    for entry in [
        "[5] Auslastung aller Accounts",
        "[6] Grenzen eines Accounts",
        "[7] Automatik und Aufgaben",
        "[8] Beenden",
    ] {
        assert!(text.contains(entry), "{entry} fehlt im Menue: {text}");
    }
}

#[test]
fn the_menu_switches_the_window_ping_on() {
    let harness = harness_with_two_accounts();
    let output = harness.run_with_input(&[], "7\n2\n\n0\n\n8\n");
    assert_success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("Fenster-Ping nach dem Reset ist jetzt an."),
        "{text}"
    );

    let config = fs::read_to_string(harness.store.join("config.json")).expect("Einstellungen");
    assert!(config.contains("\"enabled\": true"), "{config}");
}

#[test]
fn the_menu_creates_a_job() {
    let harness = harness_with_two_accounts();
    let home = harness.home.display().to_string();
    let output = harness.run_with_input(
        &[],
        &format!("7\n3\nSchreib den Bericht\n{home}\nn\n\n0\n\n8\n"),
    );
    assert_success(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Angelegt: [0001]"), "{text}");

    let job = harness.read_job("0001");
    assert_eq!(job["text"], json!("Schreib den Bericht"), "{job}");
    assert_eq!(job["cwd"], json!(home), "{job}");
    assert_eq!(job["repeat"], json!(false), "{job}");
}
