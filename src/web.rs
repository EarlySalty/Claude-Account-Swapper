use std::collections::{HashMap, VecDeque};
use std::fs::{self, DirBuilder};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::config::Config;
use crate::jobs::JobKind;
use crate::usage::Stops;
use crate::{App, JobOptions};

pub const DEFAULT_ALLOWED_HOST: &str = "v50671-kde";
const SOCKET_DIR_NAME: &str = ".claude-account-web";
const SOCKET_FILE_NAME: &str = "web.sock";
const SOCKET_MODE: u32 = 0o660;
const SOCKET_DIR_MODE: u32 = 0o750;
const LOCAL_LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const MAX_BODY_BYTES: usize = 64 * 1024;
const TASK_HISTORY: usize = 24;
const RUNNING: &str = "läuft";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WebTask {
    id: u64,
    action: String,
    status: String,
    message: String,
    auth_url: Option<String>,
}

#[derive(Default)]
struct TaskStore {
    next_id: AtomicU64,
    tasks: Mutex<VecDeque<WebTask>>,
    device_login_inputs: Mutex<HashMap<u64, Sender<Option<String>>>>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceStatus {
    installed: bool,
    active: bool,
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct NameRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LoginRequest {
    name: String,
    mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DeviceLoginInputRequest {
    task_id: u64,
    code: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DeviceLoginCancelRequest {
    task_id: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SwitchRequest {
    name: String,
    #[serde(default)]
    skip_check: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LimitsRequest {
    name: String,
    five_hour: Option<f64>,
    seven_day: Option<f64>,
    hard: Option<bool>,
    #[serde(default)]
    clear: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct KeepaliveRequest {
    #[serde(default = "default_keepalive_days")]
    max_age_days: u64,
}

fn default_keepalive_days() -> u64 {
    crate::DEFAULT_KEEPALIVE_MAX_AGE_DAYS
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct JobRequest {
    kind: String,
    text: String,
    session_id: Option<String>,
    cwd: String,
    title: Option<String>,
    settings: Option<String>,
    model: Option<String>,
    timeout_minutes: Option<u64>,
    #[serde(default)]
    allow_permissions: bool,
    #[serde(default)]
    repeat: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ServiceRequest {
    action: String,
}

pub fn serve(app: &App, socket: Option<PathBuf>, allowed_hosts: Vec<String>) -> Result<()> {
    let allowed_hosts: Vec<String> = allowed_hosts
        .into_iter()
        .map(|host| host.trim().to_ascii_lowercase())
        .filter(|host| !host.is_empty())
        .collect();
    if allowed_hosts.is_empty() {
        bail!("Mindestens ein erlaubter Host ist nötig.");
    }
    let socket = match socket {
        Some(path) => path,
        None => default_socket_path()?,
    };
    let server = bind_socket(&socket)?;
    println!("Claude Account Swapper Webmenü auf {}", socket.display());
    println!("Erlaubte Hosts: {}", allowed_hosts.join(", "));

    let app = Arc::new(app.clone());
    let tasks = Arc::new(TaskStore::default());
    let allowed_hosts = Arc::new(allowed_hosts);
    for request in server.incoming_requests() {
        let app = Arc::clone(&app);
        let tasks = Arc::clone(&tasks);
        let allowed_hosts = Arc::clone(&allowed_hosts);
        thread::spawn(move || handle_request(request, app, tasks, allowed_hosts));
    }
    Ok(())
}

fn default_socket_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .context("HOME ist nicht gesetzt")?;
    Ok(PathBuf::from(home)
        .join(SOCKET_DIR_NAME)
        .join(SOCKET_FILE_NAME))
}

fn bind_socket(path: &Path) -> Result<Server> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .context("Der Socket braucht ein eigenes Verzeichnis.")?;
    DirBuilder::new()
        .recursive(true)
        .mode(SOCKET_DIR_MODE)
        .create(dir)
        .with_context(|| format!("Verzeichnis {} fehlt", dir.display()))?;
    let mode = fs::metadata(dir)?.permissions().mode();
    if mode & 0o007 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(mode & 0o7770))?;
    }
    remove_stale_socket(path)?;
    let server = Server::http_unix(path)
        .map_err(|error| anyhow::anyhow!("Webmenü konnte nicht gestartet werden: {error}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE))?;
    Ok(server)
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.file_type().is_socket() {
        bail!("{} existiert und ist kein Socket.", path.display());
    }
    if UnixStream::connect(path).is_ok() {
        bail!("Das Webmenü läuft bereits auf {}.", path.display());
    }
    fs::remove_file(path).with_context(|| format!("{} ist nicht entfernbar", path.display()))
}

fn handle_request(
    mut request: Request,
    app: Arc<App>,
    tasks: Arc<TaskStore>,
    allowed_hosts: Arc<Vec<String>>,
) {
    let response = match route(&mut request, &app, &tasks, &allowed_hosts) {
        Ok(response) => response,
        Err((status, message)) => json_response(status, &serde_json::json!({"error": message})),
    };
    let _ = request.respond(response);
}

fn route(
    request: &mut Request,
    app: &App,
    tasks: &Arc<TaskStore>,
    allowed_hosts: &[String],
) -> std::result::Result<Response<std::io::Cursor<Vec<u8>>>, (StatusCode, String)> {
    let host = header_value(request, "Host");
    if !host_allowed(host, allowed_hosts) {
        return Err((
            StatusCode(403),
            "Zugriff nur über das private Tailscale-Menü.".into(),
        ));
    }
    let path = request.url().split('?').next().unwrap_or("/").to_owned();
    match (request.method(), path.as_str()) {
        (&Method::Get, "/") => Ok(asset(
            include_bytes!("../web/index.html"),
            "text/html; charset=utf-8",
        )),
        (&Method::Get, "/app.js") => Ok(asset(
            include_bytes!("../web/app.js"),
            "text/javascript; charset=utf-8",
        )),
        (&Method::Get, "/style.css") => Ok(asset(
            include_bytes!("../web/style.css"),
            "text/css; charset=utf-8",
        )),
        (&Method::Get, "/api/dashboard") => {
            let mut dashboard = app.web_snapshot().map_err(internal_error)?;
            if let Some(object) = dashboard.as_object_mut() {
                object.insert(
                    "service".into(),
                    serde_json::to_value(service_status()).unwrap_or_default(),
                );
                object.insert(
                    "tasks".into(),
                    serde_json::to_value(task_snapshot(tasks)).unwrap_or_default(),
                );
            }
            Ok(json_response(StatusCode(200), &dashboard))
        }
        (&Method::Get, "/api/automation/logs") => {
            let output = Command::new("journalctl")
                .args([
                    "--user",
                    "-u",
                    "claude-account-sync.service",
                    "-n",
                    "120",
                    "--no-pager",
                    "-o",
                    "short-iso",
                ])
                .output()
                .map_err(anyhow::Error::from)
                .map_err(internal_error)?;
            let text = if output.status.success() {
                String::from_utf8_lossy(&output.stdout).into_owned()
            } else {
                String::from_utf8_lossy(&output.stderr).trim().to_owned()
            };
            Ok(json_response(
                StatusCode(200),
                &serde_json::json!({"logs": text}),
            ))
        }
        (&Method::Get, path) if path.starts_with("/api/jobs/") && path.ends_with("/log") => {
            let id = path
                .trim_start_matches("/api/jobs/")
                .trim_end_matches("/log")
                .trim_end_matches('/');
            let text = app.web_job_log(id).map_err(internal_error)?;
            Ok(json_response(
                StatusCode(200),
                &serde_json::json!({"log": text}),
            ))
        }
        (&Method::Post, path) if path.starts_with("/api/") => {
            if !origin_matches(host, header_value(request, "Origin")) {
                return Err((
                    StatusCode(403),
                    "Die Anfrage stammt nicht von dieser Seite.".into(),
                ));
            }
            dispatch_post(request, app, tasks, path)
        }
        _ => Err((StatusCode(404), "Diese Seite wurde nicht gefunden.".into())),
    }
}

fn dispatch_post(
    request: &mut Request,
    app: &App,
    tasks: &Arc<TaskStore>,
    path: &str,
) -> std::result::Result<Response<std::io::Cursor<Vec<u8>>>, (StatusCode, String)> {
    match path {
        "/api/accounts/save" => {
            let input: NameRequest = read_json(request)?;
            app.save(&input.name).map_err(internal_error)?;
            Ok(message_response("Aktiver Account wurde gespeichert."))
        }
        "/api/accounts/login" => {
            let input: LoginRequest = read_json(request)?;
            match input.mode.as_str() {
                "local" => {
                    let name = input.name;
                    let task = start_task(tasks, "Login (lokal)", move |app| {
                        app.login_within(&name, Some(LOCAL_LOGIN_TIMEOUT))?;
                        Ok(format!("Login für {name} abgeschlossen."))
                    });
                    Ok(task_response(task))
                }
                "device" => Ok(task_response(start_device_login_task(tasks, input.name))),
                _ => Err((
                    StatusCode(400),
                    "Bitte Lokal oder Device Code als Anmeldemethode wählen.".into(),
                )),
            }
        }
        "/api/accounts/login/complete" => {
            let input: DeviceLoginInputRequest = read_json(request)?;
            let code = input.code.trim();
            if code.is_empty() || code.len() > 16 * 1024 || code.chars().any(char::is_control) {
                return Err((
                    StatusCode(400),
                    "Bitte den vollständigen Code oder die Weiterleitungs-URL einfügen.".into(),
                ));
            }
            let sender = tasks
                .device_login_inputs
                .lock()
                .ok()
                .and_then(|inputs| inputs.get(&input.task_id).cloned())
                .ok_or_else(|| {
                    (
                        StatusCode(404),
                        "Dieser Device-Code-Login läuft nicht mehr.".into(),
                    )
                })?;
            sender.send(Some(code.to_owned())).map_err(|_| {
                (
                    StatusCode(404),
                    "Dieser Device-Code-Login läuft nicht mehr.".into(),
                )
            })?;
            Ok(message_response("Code wurde an Claude Code übergeben."))
        }
        "/api/accounts/login/cancel" => {
            let input: DeviceLoginCancelRequest = read_json(request)?;
            let sender = tasks
                .device_login_inputs
                .lock()
                .ok()
                .and_then(|inputs| inputs.get(&input.task_id).cloned())
                .ok_or_else(|| {
                    (
                        StatusCode(404),
                        "Dieser Device-Code-Login läuft nicht mehr.".into(),
                    )
                })?;
            sender.send(None).map_err(|_| {
                (
                    StatusCode(404),
                    "Dieser Device-Code-Login läuft nicht mehr.".into(),
                )
            })?;
            Ok(message_response("Device-Code-Login wird abgebrochen."))
        }
        "/api/accounts/switch" => {
            let input: SwitchRequest = read_json(request)?;
            let name = input.name;
            let check = !input.skip_check;
            let task = start_task(tasks, "Account wechseln", move |app| {
                app.switch_checked(&name, check)?;
                Ok(format!("Aktiver Account ist jetzt {name}."))
            });
            Ok(task_response(task))
        }
        "/api/accounts/limits" => {
            let input: LimitsRequest = read_json(request)?;
            let limits = match input.clear {
                true => Stops::default(),
                false => Stops {
                    five_hour: input.five_hour,
                    seven_day: input.seven_day,
                    hard: input.hard.unwrap_or_default(),
                },
            };
            app.replace_limits(&input.name, limits)
                .map_err(internal_error)?;
            Ok(message_response("Account-Grenzen wurden gespeichert."))
        }
        "/api/config" => {
            let mut config: Config = read_json(request)?;
            config.limit_compact = app.load_config().limit_compact;
            app.write_config(&config).map_err(internal_error)?;
            Ok(message_response(
                "Automatik-Einstellungen wurden gespeichert.",
            ))
        }
        "/api/actions/auto" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields, rename_all = "camelCase")]
            struct Input {
                threshold: f64,
                #[serde(default)]
                dry_run: bool,
            }
            let input: Input = read_json(request)?;
            let task = start_task(
                tasks,
                if input.dry_run {
                    "Automatik prüfen"
                } else {
                    "Automatisch wechseln"
                },
                move |app| {
                    let decision = app.auto_switch(input.threshold, input.dry_run)?;
                    Ok(format!("{decision:?}"))
                },
            );
            Ok(task_response(task))
        }
        "/api/actions/sync" => {
            let task = start_task(tasks, "Account synchronisieren", |app| {
                Ok(app.sync()?.to_string())
            });
            Ok(task_response(task))
        }
        "/api/actions/usage" => {
            let task = start_task(tasks, "Auslastung aktualisieren", |app| {
                app.web_refresh_usage()
            });
            Ok(task_response(task))
        }
        "/api/actions/keepalive" => {
            let input: KeepaliveRequest = read_json(request)?;
            let task = start_task(tasks, "Inaktive Accounts auffrischen", move |app| {
                app.keepalive(input.max_age_days)?;
                Ok("Auffrischung abgeschlossen.".into())
            });
            Ok(task_response(task))
        }
        "/api/actions/ping" => {
            let task = start_task(tasks, "Fenster-Ping", |app| {
                app.ping_now()?;
                Ok("Fenster-Ping abgeschlossen.".into())
            });
            Ok(task_response(task))
        }
        "/api/jobs" => {
            let input: JobRequest = read_json(request)?;
            let cwd = PathBuf::from(input.cwd);
            let settings = input.settings.map(PathBuf::from);
            let kind = match input.kind.as_str() {
                "prompt" => JobKind::Prompt { text: input.text },
                "resume" => JobKind::Resume {
                    session_id: input
                        .session_id
                        .ok_or_else(|| (StatusCode(400), "Bitte eine Sitzung auswählen.".into()))?,
                    text: input.text,
                },
                _ => return Err((StatusCode(400), "Aufgabentyp ist ungültig.".into())),
            };
            let job = app
                .add_job(
                    kind,
                    cwd,
                    JobOptions {
                        title: input.title,
                        settings,
                        model: input.model,
                        timeout_minutes: input.timeout_minutes,
                        skip_permissions: !input.allow_permissions,
                        repeat: input.repeat,
                    },
                )
                .map_err(internal_error)?;
            Ok(json_response(
                StatusCode(201),
                &serde_json::json!({"message": format!("Aufgabe [{}] wurde angelegt.", job.id), "job": job}),
            ))
        }
        _ if path.starts_with("/api/jobs/") => job_action(request, app, tasks, path),
        "/api/service" => {
            let input: ServiceRequest = read_json(request)?;
            let verb = match input.action.as_str() {
                "start" | "stop" | "restart" | "enable" | "disable" => input.action,
                _ => return Err((StatusCode(400), "Unbekannte Dienstaktion.".into())),
            };
            let output = Command::new("systemctl")
                .args(["--user", verb.as_str(), "claude-account-sync.service"])
                .output()
                .map_err(anyhow::Error::from)
                .map_err(internal_error)?;
            if !output.status.success() {
                return Err((
                    StatusCode(400),
                    String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                ));
            }
            Ok(message_response(&format!(
                "Hintergrunddienst: {verb} ausgeführt."
            )))
        }
        _ => Err((StatusCode(404), "Diese Aktion wurde nicht gefunden.".into())),
    }
}

fn job_action(
    request: &mut Request,
    app: &App,
    tasks: &Arc<TaskStore>,
    path: &str,
) -> std::result::Result<Response<std::io::Cursor<Vec<u8>>>, (StatusCode, String)> {
    let suffix = path.strip_prefix("/api/jobs/").unwrap_or_default();
    let Some((id, action)) = suffix.split_once('/') else {
        return Err((StatusCode(404), "Aufgabenaktion fehlt.".into()));
    };
    if id.is_empty() || id.len() > 12 || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err((StatusCode(400), "Aufgaben-ID ist ungültig.".into()));
    }
    match (request.method(), action) {
        (&Method::Post, "enable") => {
            app.set_job_enabled(id, true).map_err(internal_error)?;
            Ok(message_response("Aufgabe wurde aktiviert."))
        }
        (&Method::Post, "disable") => {
            app.set_job_enabled(id, false).map_err(internal_error)?;
            Ok(message_response("Aufgabe wurde pausiert."))
        }
        (&Method::Post, "remove") => {
            app.remove_job(id).map_err(internal_error)?;
            Ok(message_response("Aufgabe wurde gelöscht."))
        }
        (&Method::Post, "run") => {
            let id = id.to_owned();
            let task = start_task(tasks, "Aufgabe sofort ausführen", move |app| {
                app.run_job_now(&id)?;
                Ok(format!("Aufgabe [{id}] ist abgeschlossen."))
            });
            Ok(task_response(task))
        }
        _ => Err((
            StatusCode(404),
            "Diese Aufgabenaktion wurde nicht gefunden.".into(),
        )),
    }
}

fn start_task(
    store: &Arc<TaskStore>,
    action: &str,
    run: impl FnOnce(&App) -> Result<String> + Send + 'static,
) -> WebTask {
    let task = register_task(store, action, String::new());
    let id = task.id;
    let store = Arc::clone(store);
    let app = App::discover();
    thread::spawn(move || {
        let result = match app {
            Ok(app) => run(&app),
            Err(error) => Err(error),
        };
        if let Ok(mut tasks) = store.tasks.lock()
            && let Some(task) = tasks.iter_mut().find(|task| task.id == id)
        {
            match result {
                Ok(message) => {
                    task.status = "fertig".into();
                    task.message = message;
                }
                Err(error) => {
                    task.status = "fehler".into();
                    task.message = format!("{error:#}");
                }
            }
        }
    });
    task
}

fn start_device_login_task(store: &Arc<TaskStore>, name: String) -> WebTask {
    let task = register_task(
        store,
        "Login mit Device Code",
        "Claude-Code-Login wird gestartet …".into(),
    );
    let id = task.id;

    let (input_tx, input_rx) = mpsc::channel();
    if let Ok(mut inputs) = store.device_login_inputs.lock() {
        inputs.insert(id, input_tx);
    }
    let store = Arc::clone(store);
    thread::spawn(move || {
        let result = match App::discover() {
            Ok(app) => app.login_device(&name, input_rx, |url| {
                if let Ok(mut tasks) = store.tasks.lock()
                    && let Some(task) = tasks.iter_mut().find(|task| task.id == id)
                {
                    task.auth_url = Some(url);
                    task.message = "Öffne die URL, melde dich an und füge danach den Code oder die Weiterleitungs-URL hier ein.".into();
                }
            }),
            Err(error) => Err(error),
        };

        if let Ok(mut inputs) = store.device_login_inputs.lock() {
            inputs.remove(&id);
        }
        if let Ok(mut tasks) = store.tasks.lock()
            && let Some(task) = tasks.iter_mut().find(|task| task.id == id)
        {
            task.auth_url = None;
            match result {
                Ok(()) => {
                    task.status = "fertig".into();
                    task.message = format!("Login für {name} abgeschlossen und gespeichert.");
                }
                Err(error) => {
                    task.status = "fehler".into();
                    task.message = format!("{error:#}");
                }
            }
        }
    });
    task
}

fn register_task(store: &TaskStore, action: &str, message: String) -> WebTask {
    let task = WebTask {
        id: store.next_id.fetch_add(1, Ordering::Relaxed) + 1,
        action: action.into(),
        status: RUNNING.into(),
        message,
        auth_url: None,
    };
    if let Ok(mut tasks) = store.tasks.lock() {
        tasks.push_back(task.clone());
        prune_finished(&mut tasks);
    }
    task
}

fn prune_finished(tasks: &mut VecDeque<WebTask>) {
    while tasks.len() > TASK_HISTORY {
        let Some(index) = tasks.iter().position(|task| task.status != RUNNING) else {
            return;
        };
        tasks.remove(index);
    }
}

fn task_snapshot(store: &TaskStore) -> Vec<WebTask> {
    store
        .tasks
        .lock()
        .map(|tasks| tasks.iter().cloned().collect())
        .unwrap_or_default()
}

fn service_status() -> ServiceStatus {
    let active = systemctl_state("is-active");
    let enabled = systemctl_state("is-enabled");
    ServiceStatus {
        installed: Command::new("systemctl")
            .args(["--user", "cat", "claude-account-sync.service"])
            .output()
            .is_ok_and(|output| output.status.success()),
        active: active == "active",
        enabled: enabled == "enabled",
    }
}

fn systemctl_state(action: &str) -> String {
    Command::new("systemctl")
        .args(["--user", action, "claude-account-sync.service"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn header_value<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

fn host_allowed(host: Option<&str>, allowed_hosts: &[String]) -> bool {
    host.is_some_and(|host| {
        allowed_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
    })
}

fn origin_matches(host: Option<&str>, origin: Option<&str>) -> bool {
    match (host, origin) {
        (Some(host), Some(origin)) => ["http://", "https://"].iter().any(|scheme| {
            origin
                .strip_prefix(scheme)
                .is_some_and(|rest| rest.eq_ignore_ascii_case(host))
        }),
        _ => false,
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(
    request: &mut Request,
) -> std::result::Result<T, (StatusCode, String)> {
    let mut body = Vec::new();
    request
        .as_reader()
        .take((MAX_BODY_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| {
            (
                StatusCode(400),
                format!("Anfrage konnte nicht gelesen werden: {error}"),
            )
        })?;
    if body.len() > MAX_BODY_BYTES {
        return Err((StatusCode(413), "Die Anfrage ist zu groß.".into()));
    }
    serde_json::from_slice(&body)
        .map_err(|error| (StatusCode(400), format!("Eingaben sind ungültig: {error}")))
}

fn asset(bytes: &'static [u8], content_type: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_data(bytes.to_vec());
    add_header(&mut response, "Content-Type", content_type);
    add_header(&mut response, "Cache-Control", "no-store");
    add_header(&mut response, "X-Content-Type-Options", "nosniff");
    add_header(&mut response, "X-Frame-Options", "DENY");
    add_header(
        &mut response,
        "Content-Security-Policy",
        "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
    );
    response
}

fn json_response(
    status: StatusCode,
    value: &serde_json::Value,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_string(value.to_string()).with_status_code(status);
    add_header(
        &mut response,
        "Content-Type",
        "application/json; charset=utf-8",
    );
    add_header(&mut response, "Cache-Control", "no-store");
    add_header(&mut response, "X-Content-Type-Options", "nosniff");
    response
}

fn message_response(message: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(StatusCode(200), &serde_json::json!({"message": message}))
}

fn task_response(task: WebTask) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(StatusCode(202), &serde_json::json!({"task": task}))
}

fn add_header(response: &mut Response<std::io::Cursor<Vec<u8>>>, name: &str, value: &str) {
    if let Ok(header) = Header::from_bytes(name, value) {
        response.add_header(header);
    }
}

fn internal_error(error: anyhow::Error) -> (StatusCode, String) {
    (StatusCode(400), format!("{error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: u64, status: &str) -> WebTask {
        WebTask {
            id,
            action: "Login mit Device Code".into(),
            status: status.into(),
            message: String::new(),
            auth_url: None,
        }
    }

    #[test]
    fn only_allowlisted_hosts_and_their_own_origin_pass() {
        let allowed = vec!["v50671-kde".to_owned()];
        assert!(host_allowed(Some("V50671-KDE"), &allowed));
        assert!(!host_allowed(Some("rebind.example"), &allowed));
        assert!(!host_allowed(Some("v50671-kde:8785"), &allowed));
        assert!(!host_allowed(None, &allowed));
        assert!(origin_matches(
            Some("v50671-kde"),
            Some("http://v50671-kde")
        ));
        assert!(!origin_matches(
            Some("v50671-kde"),
            Some("http://v50671-kde.evil.example")
        ));
        assert!(!origin_matches(Some("v50671-kde"), None));
    }

    #[test]
    fn pruning_never_drops_a_running_task() {
        let mut tasks: VecDeque<WebTask> = (0..TASK_HISTORY as u64)
            .map(|id| task(id, RUNNING))
            .collect();
        tasks.push_back(task(99, "fertig"));
        tasks.push_back(task(100, RUNNING));
        prune_finished(&mut tasks);
        assert!(tasks.iter().all(|task| task.status == RUNNING));
        assert_eq!(tasks.len(), TASK_HISTORY + 1);
    }
}
