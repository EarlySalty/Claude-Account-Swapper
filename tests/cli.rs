use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
