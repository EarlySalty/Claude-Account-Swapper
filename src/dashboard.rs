use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use tiny_http::{Header, Method, Response, Server, StatusCode};

use crate::App;
use crate::routing::{Rule, Strategy};

const PAGE: &str = include_str!("../web/index.html");
const SCRIPT: &str = include_str!("../web/app.js");
const STYLE: &str = include_str!("../web/style.css");

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Account {
        name: String,
        enabled: bool,
    },
    Project {
        path: std::path::PathBuf,
        rule: Option<Rule>,
    },
    Session {
        id: String,
        rule: Option<Rule>,
    },
    Settings {
        strategy: Strategy,
        auto_switch: bool,
        threshold: f64,
        weekly_threshold: f64,
        cooldown_seconds: u64,
        hysteresis: f64,
    },
}

fn apply(app: &App, action: Action) -> Result<()> {
    match action {
        Action::Account { name, enabled } => app.set_account_enabled(&name, enabled),
        Action::Project { path, rule } => app.map_project(&path, rule),
        Action::Session { id, rule } => app.assign_session(&id, rule),
        Action::Settings {
            strategy,
            auto_switch,
            threshold,
            weekly_threshold,
            cooldown_seconds,
            hysteresis,
        } => {
            app.update_config(|config| {
                config.threshold = threshold;
                config.routing.strategy = strategy;
                config.routing.auto_switch = auto_switch;
                config.routing.weekly_threshold = weekly_threshold;
                config.routing.cooldown_seconds = cooldown_seconds;
                config.routing.hysteresis = hysteresis;
            })?;
            Ok(())
        }
    }
}

fn valid_host(host: &str, allowed: &[String]) -> bool {
    allowed
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed))
}

pub fn serve(app: &App, port: u16, extra_hosts: Vec<String>) -> Result<()> {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let server = Server::http(address)
        .map_err(|error| anyhow::anyhow!("Dashboard konnte nicht starten: {error}"))?;
    let csrf = uuid::Uuid::new_v4().to_string();
    let mut allowed = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    for host in extra_hosts {
        ensure!(
            !host.is_empty()
                && !host.contains('/')
                && !host.contains('*')
                && !host.chars().any(char::is_whitespace),
            "Zusätzlicher Host ist ungültig"
        );
        allowed.push(host);
    }
    app.start_native_monitor();
    eprintln!("Account-Dashboard: http://{address}");
    for mut request in server.incoming_requests() {
        let host = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Host"))
            .map(|header| header.value.as_str())
            .unwrap_or("")
            .to_owned();
        let route = request.url().split('?').next().unwrap_or("/").to_owned();
        let response: Result<(u16, &str, String)> = (|| {
            ensure!(valid_host(&host, &allowed), "Host ist nicht freigegeben");
            match (request.method(), route.as_str()) {
                (&Method::Get, "/") => Ok((
                    200,
                    "text/html; charset=utf-8",
                    PAGE.replace("{{CSRF}}", &csrf),
                )),
                (&Method::Get, "/app.js") => {
                    Ok((200, "text/javascript; charset=utf-8", SCRIPT.into()))
                }
                (&Method::Get, "/style.css") => Ok((200, "text/css; charset=utf-8", STYLE.into())),
                (&Method::Get, "/api/state") => Ok((
                    200,
                    "application/json; charset=utf-8",
                    serde_json::to_string(&app.orchestration_state()?)?,
                )),
                (&Method::Get, "/health") => Ok((200, "application/json", "{\"ok\":true}".into())),
                (&Method::Post, "/api/action") => {
                    let token = request
                        .headers()
                        .iter()
                        .find(|header| header.field.equiv("X-Account-CSRF"))
                        .map(|header| header.value.as_str());
                    ensure!(token == Some(csrf.as_str()), "Ungültige Anfragekennung");
                    if let Some(origin) = request
                        .headers()
                        .iter()
                        .find(|header| header.field.equiv("Origin"))
                        .map(|header| header.value.as_str())
                    {
                        ensure!(
                            origin == format!("http://{host}")
                                || origin == format!("https://{host}"),
                            "Fremder Ursprung ist nicht erlaubt"
                        );
                    }
                    let content_type = request
                        .headers()
                        .iter()
                        .find(|header| header.field.equiv("Content-Type"))
                        .map(|header| header.value.as_str())
                        .unwrap_or("");
                    ensure!(
                        content_type.split(';').next() == Some("application/json"),
                        "JSON-Anfrage erforderlich"
                    );
                    let mut body = Vec::new();
                    request.as_reader().take(65_537).read_to_end(&mut body)?;
                    ensure!(body.len() <= 65_536, "Anfrage ist zu groß");
                    let action =
                        serde_json::from_slice(&body).context("Ungültige Dashboard-Aktion")?;
                    apply(app, action)?;
                    Ok((200, "application/json", "{\"ok\":true}".into()))
                }
                _ => Ok((
                    404,
                    "application/json",
                    "{\"error\":\"Nicht gefunden\"}".into(),
                )),
            }
        })();
        let (code, content_type, body) = response.unwrap_or_else(|error| {
            (
                400,
                "application/json",
                serde_json::json!({"error":format!("{error:#}")}).to_string(),
            )
        });
        let mut response = Response::from_string(body).with_status_code(StatusCode(code));
        for (name, value) in [
            ("Content-Type", content_type),
            ("Cache-Control", "no-store"),
            ("X-Content-Type-Options", "nosniff"),
            ("Referrer-Policy", "no-referrer"),
            (
                "Content-Security-Policy",
                "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
            ),
        ] {
            response.add_header(
                Header::from_bytes(name, value)
                    .map_err(|_| anyhow::anyhow!("HTTP-Header ist ungültig"))?,
            );
        }
        let _ = request.respond(response);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_allowlist_is_exact() {
        let allowed = vec!["localhost:3775".into()];
        assert!(valid_host("localhost:3775", &allowed));
        assert!(!valid_host("localhost.evil:3775", &allowed));
        assert!(!valid_host("evil.example", &allowed));
    }
    #[test]
    fn actions_reject_unknown_fields() {
        assert!(
            serde_json::from_str::<Action>(
                r#"{"action":"account","name":"a","enabled":true,"command":"ignored"}"#
            )
            .is_err()
        );
    }
}
