use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tempfile::TempDir;
use tiny_http::{Response, Server};

struct Fixture {
    root: TempDir,
    usage: Arc<Mutex<BTreeMap<String, f64>>>,
    url: String,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        fs::create_dir_all(root.path().join("project/src")).unwrap();
        fs::write(home.join(".claude/.credentials.json"), "global-unchanged").unwrap();
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            + 86_400_000;
        for name in ["a", "b"] {
            let profile = root.path().join("store/accounts").join(name);
            let native = root.path().join(name);
            fs::create_dir_all(&profile).unwrap();
            fs::create_dir_all(&native).unwrap();
            let credentials = json!({"claudeAiOauth":{"accessToken":format!("fixture-{name}"),"refreshToken":format!("fixture-refresh-{name}"),"expiresAt":expiry,"scopes":["user:inference","user:profile"]}});
            fs::write(profile.join("credentials.json"), credentials.to_string()).unwrap();
            fs::write(native.join(".credentials.json"), credentials.to_string()).unwrap();
            fs::write(
                profile.join("profile.json"),
                json!({"name":name,"email":format!("{name}@example.invalid"),"saved_at":1})
                    .to_string(),
            )
            .unwrap();
            symlink(home.join(".claude/projects"), native.join("projects")).unwrap();
        }
        fs::write(root.path().join("store/config.json"),json!({"threshold":98,"routing":{"homes":{"a":root.path().join("a"),"b":root.path().join("b")},"strategy":"balanced","idle_release_seconds":5}}).to_string()).unwrap();
        let mock = root.path().join("mock-claude");
        fs::write(&mock, r##"#!/bin/sh
name=$(basename "$CLAUDE_CONFIG_DIR")
sid=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) echo 'fixture-claude'; exit 0 ;;
    --session-id|--resume) sid="$2"; shift ;;
    --hold-account) echo '{"type":"ready"}'; while [ ! -f "$CLAUDE_CONFIG_DIR/release" ]; do sleep 0.05; done; exit 0 ;;
    auth) printf '{"loggedIn":true,"email":"%s@example.invalid"}\n' "$name"; exit 0 ;;
  esac
  shift
done
while IFS= read -r line; do
  case "$line" in
    *'"control_request"'*)
      request=$(printf '%s' "$line" | sed -n 's/.*"request_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s"}}\n' "$request"
      ;;
    *'"user"'*)
      printf '{"type":"system","subtype":"init","session_id":"%s","account":"%s"}\n' "$sid" "$name"
      case "$line" in *'hold'*) while [ ! -f "$CLAUDE_CONFIG_DIR/release" ]; do sleep 0.05; done ;; esac
      printf '{"type":"assistant","session_id":"%s","account":"%s"}\n' "$sid" "$name"
      printf '{"type":"result","subtype":"success","session_id":"%s","result":"%s"}\n' "$sid" "$name"
      ;;
  esac
done
"##).unwrap();
        fs::set_permissions(&mock, fs::Permissions::from_mode(0o700)).unwrap();
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let usage = Arc::new(Mutex::new(BTreeMap::from([
            ("a".into(), 20.0),
            ("b".into(), 60.0),
        ])));
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let values = usage.clone();
        let server = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                if let Some(request) = server.recv_timeout(Duration::from_millis(100)).unwrap() {
                    let name = request
                        .headers()
                        .iter()
                        .find(|header| header.field.equiv("Authorization"))
                        .unwrap()
                        .value
                        .as_str()
                        .rsplit('-')
                        .next()
                        .unwrap()
                        .to_owned();
                    let five = *values.lock().unwrap().get(&name).unwrap_or(&100.0);
                    let body = json!({"five_hour":{"utilization":five,"resets_at":null},"seven_day":{"utilization":10,"resets_at":null},"seven_day_test":{"display_name":"Test Model","utilization":12,"resets_at":null}});
                    request
                        .respond(Response::from_string(body.to_string()))
                        .unwrap();
                }
            }
        });
        Self {
            root,
            usage,
            url,
            stop,
            server: Some(server),
        }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_claude-account"));
        command
            .args(args)
            .env_clear()
            .env("HOME", self.root.path().join("home"))
            .env("PATH", "/usr/bin:/bin")
            .env(
                "CLAUDE_ACCOUNT_SWITCHER_HOME",
                self.root.path().join("store"),
            )
            .env(
                "CLAUDE_ACCOUNT_SWITCHER_CLAUDE_BIN",
                self.root.path().join("mock-claude"),
            )
            .env("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL", &self.url)
            .current_dir(self.root.path().join("project"));
        command
    }
    fn bridge(&self) -> Running {
        Running::spawn(self.command(&[
            "t3",
            "--",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ]))
    }
    fn quota(&self, name: &str, value: f64) {
        self.usage.lock().unwrap().insert(name.into(), value);
        for name in ["a", "b"] {
            let _ = fs::remove_file(
                self.root
                    .path()
                    .join("store/accounts")
                    .join(name)
                    .join("routing-usage.json"),
            );
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

struct Running {
    child: Child,
    input: Option<ChildStdin>,
    events: mpsc::Receiver<Value>,
}
impl Running {
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = child.stdout.take().unwrap();
        let (sender, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                match line {
                    Ok(line) => {
                        if let Ok(value) = serde_json::from_str(&line)
                            && sender.send(value).is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            input,
            events,
        }
    }
    fn send(&mut self, value: Value) {
        writeln!(self.input.as_mut().unwrap(), "{value}").unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
    }
    fn event(&self, kind: &str) -> Value {
        loop {
            let value = self
                .events
                .recv_timeout(Duration::from_secs(15))
                .expect("Protocol event timeout");
            if value["type"] == kind {
                return value;
            }
        }
    }
    fn initialize(&mut self) {
        self.send(json!({"type":"control_request","request_id":"outer-init","request":{"subtype":"initialize"}}));
        assert_eq!(
            self.event("control_response")["response"]["request_id"],
            "outer-init"
        );
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn terminal_accounts_are_isolated_and_parallel() {
    let fixture = Fixture::new();
    let held = Running::spawn(fixture.command(&["run", "a", "--", "--hold-account"]));
    held.event("ready");
    let other = fixture
        .command(&["run", "b", "--", "--version"])
        .output()
        .unwrap();
    assert!(
        other.status.success(),
        "{}",
        String::from_utf8_lossy(&other.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&other.stdout).trim(),
        "fixture-claude"
    );
    let busy = fixture
        .command(&["run", "a", "--", "--version"])
        .output()
        .unwrap();
    assert!(!busy.status.success());
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("home/.claude/.credentials.json")).unwrap(),
        "global-unchanged"
    );
}

#[test]
fn t3_switches_after_result_and_preserves_session_id() {
    let fixture = Fixture::new();
    let mut process = fixture.bridge();
    process.initialize();
    process.send(json!({"type":"user","message":{"role":"user","content":"first"}}));
    let first = process.event("result");
    let initial = first["result"].as_str().unwrap();
    assert!(["a", "b"].contains(&initial));
    let target = if initial == "a" { "b" } else { "a" };
    fixture.quota(initial, 98.0);
    fixture.quota(target, 10.0);
    process.send(json!({"type":"user","message":{"role":"user","content":"second"}}));
    let second = process.event("result");
    assert_eq!(second["result"], target);
    assert_eq!(first["session_id"], second["session_id"]);
}

#[test]
fn t3_never_swaps_an_inflight_turn() {
    let fixture = Fixture::new();
    let mut process = fixture.bridge();
    process.initialize();
    process.send(json!({"type":"user","message":{"role":"user","content":"hold"}}));
    let started = process.event("system");
    let initial = started["account"].as_str().unwrap();
    assert!(["a", "b"].contains(&initial));
    let target = if initial == "a" { "b" } else { "a" };
    fixture.quota(initial, 98.0);
    fixture.quota(target, 10.0);
    process.send(json!({"type":"user","message":{"role":"user","content":"queued"}}));
    std::thread::sleep(Duration::from_millis(300));
    fs::write(fixture.root.path().join(initial).join("release"), "").unwrap();
    let completed = process.event("result");
    assert_eq!(completed["result"], initial);
    assert_eq!(completed["session_id"], started["session_id"]);
    let queued = process.event("result");
    assert!([initial, target].contains(&queued["result"].as_str().unwrap()));
    assert_eq!(queued["session_id"], started["session_id"]);
    process.send(json!({"type":"user","message":{"role":"user","content":"after"}}));
    assert_eq!(process.event("result")["result"], target);
}

#[test]
fn waiting_requests_can_be_cancelled_without_being_sent() {
    use wait_timeout::ChildExt;
    let fixture = Fixture::new();
    fixture.quota("a", 100.0);
    fixture.quota("b", 100.0);
    let mut process = fixture.bridge();
    process.initialize();
    process.send(json!({"type":"user","message":{"role":"user","content":"must-not-run"}}));
    std::thread::sleep(Duration::from_millis(500));
    process.send(json!({"type":"control_request","request_id":"outer-stop","request":{"subtype":"interrupt"}}));
    assert_eq!(
        process.event("control_response")["response"]["request_id"],
        "outer-stop"
    );
    assert!(
        process
            .child
            .wait_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(
        !process
            .events
            .try_iter()
            .any(|message| message["type"] == "result")
    );
}

#[test]
fn buffered_input_is_forwarded_before_eof() {
    let fixture = Fixture::new();
    let mut process = fixture.bridge();
    process.send(json!({"type":"control_request","request_id":"outer-init","request":{"subtype":"initialize"}}));
    process.send(json!({"type":"user","message":{"role":"user","content":"first"}}));
    process.input.take();
    let result = process.event("result");
    assert!(["a", "b"].contains(&result["result"].as_str().unwrap()));
}

#[test]
fn dashboard_rejects_cross_origin_writes_and_never_returns_tokens() {
    let fixture = Fixture::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let _server = Running::spawn(fixture.command(&["dashboard", "--port", &port.to_string()]));
    let client = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(3)))
            .build(),
    );
    let url = format!("http://127.0.0.1:{port}");
    let mut ready = false;
    for _ in 0..50 {
        if client.get(format!("{url}/health")).call().is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready);
    let mut page = client.get(&url).call().unwrap();
    assert!(
        page.headers()
            .get("Content-Security-Policy")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    let html = page.body_mut().read_to_string().unwrap();
    let csrf = html
        .split("name=\"account-csrf\" content=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let payload = json!({"action":"account","name":"a","enabled":false});
    let denied = client
        .post(format!("{url}/api/action"))
        .send_json(&payload)
        .unwrap();
    assert_eq!(denied.status().as_u16(), 400);
    let denied = client
        .post(format!("{url}/api/action"))
        .header("X-Account-CSRF", csrf)
        .header("Origin", "https://untrusted.example")
        .send_json(&payload)
        .unwrap();
    assert_eq!(denied.status().as_u16(), 400);
    let denied = client
        .get(format!("{url}/api/state"))
        .header("Host", "rebinding.example")
        .call()
        .unwrap();
    assert_eq!(denied.status().as_u16(), 400);
    let saved = client
        .post(format!("{url}/api/action"))
        .header("X-Account-CSRF", csrf)
        .header("Origin", &url)
        .send_json(&payload)
        .unwrap();
    assert_eq!(saved.status().as_u16(), 200);
    let mut response = client.get(format!("{url}/api/state")).call().unwrap();
    let body = response.body_mut().read_to_string().unwrap();
    let state: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(state["routing"]["disabled"], json!(["a"]));
    assert!(!body.contains("fixture-refresh"));
    assert!(!body.contains("accessToken"));
}

#[test]
fn project_mapping_and_model_windows_are_available() {
    let fixture = Fixture::new();
    let directory = fixture.root.path().join("project");
    let output = fixture
        .command(&["map", "b", directory.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = fixture.command(&["native-refresh"]).output().unwrap();
    assert!(output.status.success());
    let output = fixture.command(&["monitor"]).output().unwrap();
    assert!(output.status.success());
    let state: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        state["accounts"][0]["models"]["Test Model"]["utilization"],
        12.0
    );
    assert_eq!(
        state["routing"]["projects"][directory.to_str().unwrap()]["account"],
        "b"
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("fixture-refresh"));
    assert!(!text.contains("accessToken"));
}
