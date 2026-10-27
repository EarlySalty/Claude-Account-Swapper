use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket};

const REQUEST_TIMEOUT_SECONDS: u64 = 15;
const RPC_TIMEOUT_SECONDS: u64 = 30;
const PROTOCOL_VERSION: &str = "2";
const BUSY_STATUSES: [&str; 5] = ["preparing", "queued", "starting", "running", "waiting"];

pub struct Client {
    base: String,
    token_path: PathBuf,
    t3_bin: PathBuf,
    agent: ureq::Agent,
}

#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub id: String,
    pub busy: bool,
    pub last_run_completed_at: Option<DateTime<Utc>>,
    pub last_user_message_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadDetail {
    pub native_session_ids: Vec<String>,
    pub used_tokens: Option<u64>,
    pub last_item_is_compaction: bool,
    pub last_compaction_at: Option<DateTime<Utc>>,
}

pub struct Rpc {
    socket: WebSocket<TcpStream>,
    next_id: u64,
}

impl Client {
    pub fn new(base: &str, token_path: PathBuf, t3_bin: PathBuf) -> Self {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(REQUEST_TIMEOUT_SECONDS)))
                .http_status_as_error(false)
                .build(),
        );
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token_path,
            t3_bin,
            agent,
        }
    }

    pub fn claude_threads(&self) -> Result<Vec<ThreadSummary>> {
        let body = self.authorized(|token| {
            let mut response = self
                .agent
                .get(&format!("{}/api/orchestration/shell", self.base))
                .header("Authorization", &format!("Bearer {token}"))
                .header("x-t3-orchestration-protocol", PROTOCOL_VERSION)
                .call()
                .map_err(|error| anyhow::anyhow!("T3 nicht erreichbar: {error}"))?;
            let status = response.status().as_u16();
            let body = response
                .body_mut()
                .with_config()
                .limit(256 * 1024 * 1024)
                .read_to_string()
                .context("T3-Antwort nicht lesbar")?;
            Ok((status, body))
        })?;
        let value: Value = serde_json::from_str(&body).context("T3 lieferte kein JSON")?;
        Ok(parse_shell(&value))
    }

    pub fn connect(&self) -> Result<Rpc> {
        let ticket_body = self.authorized(|token| {
            let mut response = self
                .agent
                .post(&format!("{}/api/auth/websocket-ticket", self.base))
                .header("Authorization", &format!("Bearer {token}"))
                .send_empty()
                .map_err(|error| anyhow::anyhow!("T3 nicht erreichbar: {error}"))?;
            let status = response.status().as_u16();
            let body = response
                .body_mut()
                .read_to_string()
                .context("T3-Antwort nicht lesbar")?;
            Ok((status, body))
        })?;
        let ticket: Value =
            serde_json::from_str(&ticket_body).context("T3-Ticket ist kein JSON")?;
        let ticket = ticket
            .get("ticket")
            .and_then(Value::as_str)
            .context("T3 lieferte kein Ticket")?;
        Rpc::open(&self.base, ticket)
    }

    fn authorized(&self, request: impl Fn(&str) -> Result<(u16, String)>) -> Result<String> {
        let token = match self.read_token() {
            Some(token) => token,
            None => self.issue_token()?,
        };
        let (status, body) = request(&token)?;
        let (status, body) = if status == 401 || status == 403 {
            let token = self.issue_token()?;
            request(&token)?
        } else {
            (status, body)
        };
        if status != 200 {
            bail!("T3 antwortete mit HTTP {status}");
        }
        Ok(body)
    }

    fn read_token(&self) -> Option<String> {
        let token = std::fs::read_to_string(&self.token_path).ok()?;
        let token = token.trim();
        (!token.is_empty()).then(|| token.to_owned())
    }

    fn issue_token(&self) -> Result<String> {
        let output = Command::new(&self.t3_bin)
            .args([
                "auth",
                "session",
                "issue",
                "--ttl",
                "30d",
                "--label",
                "claude-account-limit-compact",
                "--scope",
                "orchestration:read",
                "--scope",
                "orchestration:operate",
                "--token-only",
            ])
            .output()
            .with_context(|| format!("{} nicht startbar", self.t3_bin.display()))?;
        if !output.status.success() {
            bail!("T3-Zugang konnte nicht erneuert werden ({})", output.status);
        }
        let token = String::from_utf8(output.stdout)
            .context("T3-Zugang ist kein Text")?
            .trim()
            .to_owned();
        anyhow::ensure!(!token.is_empty(), "T3 lieferte keinen Zugang");
        crate::atomic_write(&self.token_path, token.as_bytes(), 0o600)?;
        Ok(token)
    }
}

impl Rpc {
    fn open(base: &str, ticket: &str) -> Result<Self> {
        let authority = base
            .strip_prefix("http://")
            .context("T3-Adresse muss mit http:// beginnen")?;
        let address = authority
            .to_socket_addrs()
            .context("T3-Adresse nicht aufloesbar")?
            .next()
            .context("T3-Adresse nicht aufloesbar")?;
        let stream =
            TcpStream::connect_timeout(&address, Duration::from_secs(REQUEST_TIMEOUT_SECONDS))
                .context("T3-WebSocket nicht erreichbar")?;
        stream.set_read_timeout(Some(Duration::from_secs(RPC_TIMEOUT_SECONDS)))?;
        stream.set_write_timeout(Some(Duration::from_secs(REQUEST_TIMEOUT_SECONDS)))?;
        let url = format!(
            "ws://{authority}/ws?orchestrationProtocol={PROTOCOL_VERSION}&wsTicket={ticket}"
        );
        let (socket, _) = tungstenite::client(url.as_str(), stream)
            .map_err(|error| anyhow::anyhow!("T3-WebSocket-Handshake fehlgeschlagen: {error}"))?;
        Ok(Self { socket, next_id: 1 })
    }

    pub fn call(&mut self, method: &str, payload: Value) -> Result<Value> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        let request = json!({
            "_tag": "Request",
            "id": id,
            "tag": method,
            "payload": payload,
            "headers": [],
        });
        self.socket
            .send(Message::text(request.to_string()))
            .context("T3-Anfrage nicht gesendet")?;
        loop {
            let message = match self.socket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    bail!("T3 antwortete nicht auf {method}");
                }
                Err(error) => bail!("T3-WebSocket abgebrochen: {error}"),
            };
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            if let Some(value) = exit_value(&frame, &id)? {
                return Ok(value);
            }
        }
    }

    pub fn thread_detail(&mut self, thread_id: &str) -> Result<ThreadDetail> {
        let value = self.call(
            "orchestration.getThreadProjection",
            json!({ "threadId": thread_id }),
        )?;
        Ok(parse_projection(&value))
    }

    pub fn send_message(&mut self, thread_id: &str, text: &str) -> Result<()> {
        self.call(
            "orchestration.dispatchCommand",
            json!({
                "type": "message.dispatch",
                "createdBy": "system",
                "creationSource": "server",
                "commandId": uuid::Uuid::new_v4().to_string(),
                "threadId": thread_id,
                "messageId": uuid::Uuid::new_v4().to_string(),
                "text": text,
                "attachments": [],
                "dispatchMode": { "type": "start_immediately" },
            }),
        )
        .map(|_| ())
    }
}

fn exit_value(frame: &Value, id: &str) -> Result<Option<Value>> {
    if frame.get("_tag").and_then(Value::as_str) != Some("Exit")
        || frame.get("requestId").and_then(Value::as_str) != Some(id)
    {
        return Ok(None);
    }
    let exit = frame.get("exit").context("T3-Antwort ohne Ergebnis")?;
    match exit.get("_tag").and_then(Value::as_str) {
        Some("Success") => Ok(Some(exit.get("value").cloned().unwrap_or(Value::Null))),
        _ => {
            let cause: String = exit.to_string().chars().take(400).collect();
            bail!("T3 lehnte die Anfrage ab: {cause}")
        }
    }
}

fn timestamp(value: &Value, field: &str) -> Option<DateTime<Utc>> {
    value
        .get(field)
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.with_timezone(&Utc))
}

fn is_busy(thread: &Value) -> bool {
    let active_run = thread
        .get("activeRunId")
        .is_some_and(|value| !value.is_null());
    let pending = thread
        .get("pendingRuntimeRequest")
        .is_some_and(|value| !value.is_null());
    let status = thread
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| BUSY_STATUSES.contains(&status));
    active_run || pending || status
}

pub fn parse_shell(value: &Value) -> Vec<ThreadSummary> {
    value
        .get("threads")
        .and_then(Value::as_array)
        .map(|threads| {
            threads
                .iter()
                .filter(|thread| {
                    thread.get("providerInstanceId").and_then(Value::as_str) == Some("claudeAgent")
                        && thread.get("archivedAt").is_none_or(Value::is_null)
                        && thread.get("deletedAt").is_none_or(Value::is_null)
                })
                .filter_map(|thread| {
                    Some(ThreadSummary {
                        id: thread.get("id")?.as_str()?.to_owned(),
                        busy: is_busy(thread),
                        last_run_completed_at: timestamp(thread, "latestRunCompletedAt"),
                        last_user_message_at: timestamp(thread, "latestUserMessageAt"),
                        updated_at: timestamp(thread, "updatedAt"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn by_ordinal(items: &[Value]) -> Vec<&Value> {
    let mut sorted: Vec<&Value> = items.iter().collect();
    sorted.sort_by_key(|item| item.get("ordinal").and_then(Value::as_u64).unwrap_or(0));
    sorted
}

pub fn parse_projection(value: &Value) -> ThreadDetail {
    let empty = Vec::new();
    let array = |field: &str| value.get(field).and_then(Value::as_array).unwrap_or(&empty);
    let native_session_ids = array("providerThreads")
        .iter()
        .filter_map(|thread| thread.pointer("/nativeThreadRef/nativeId")?.as_str())
        .map(str::to_owned)
        .collect();
    let used_tokens = by_ordinal(array("providerTurns"))
        .into_iter()
        .rev()
        .find_map(|turn| turn.pointer("/tokenUsage/usedTokens")?.as_u64());
    let items: Vec<&Value> = by_ordinal(array("turnItems"))
        .into_iter()
        .filter(|item| item.get("type").and_then(Value::as_str) != Some("checkpoint"))
        .collect();
    let is_compaction =
        |item: &Value| item.get("type").and_then(Value::as_str) == Some("compaction");
    let last_item_is_compaction = items.last().is_some_and(|item| is_compaction(item));
    let last_compaction_at = items
        .iter()
        .rev()
        .find(|item| is_compaction(item))
        .and_then(|item| timestamp(item, "startedAt"));
    ThreadDetail {
        native_session_ids,
        used_tokens,
        last_item_is_compaction,
        last_compaction_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_reads_latest_turn_and_compaction() {
        let value = json!({
            "providerThreads": [{ "nativeThreadRef": { "nativeId": "s-1" } }],
            "providerTurns": [
                { "ordinal": 2, "tokenUsage": { "usedTokens": 12351 } },
                { "ordinal": 1, "tokenUsage": { "usedTokens": 63759 } }
            ],
            "turnItems": [
                { "ordinal": 2000003, "type": "checkpoint" },
                { "ordinal": 1000001, "type": "user_message" },
                { "ordinal": 2000002, "type": "compaction", "startedAt": "2026-10-27T21:18:10Z" }
            ]
        });
        let detail = parse_projection(&value);
        assert_eq!(detail.native_session_ids, vec!["s-1".to_owned()]);
        assert_eq!(detail.used_tokens, Some(12351));
        assert!(detail.last_item_is_compaction);
        assert!(detail.last_compaction_at.is_some());
    }

    #[test]
    fn shell_keeps_live_claude_threads_and_flags_busy_ones() {
        let value = json!({ "threads": [
            { "id": "a", "providerInstanceId": "claudeAgent", "status": "running", "activeRunId": "r" },
            { "id": "b", "providerInstanceId": "claudeAgent", "status": "completed", "activeRunId": null,
              "latestRunCompletedAt": "2026-10-27T21:18:18.404Z" },
            { "id": "c", "providerInstanceId": "codex", "status": "completed" },
            { "id": "d", "providerInstanceId": "claudeAgent", "archivedAt": "2026-10-01T00:00:00Z" }
        ]});
        let threads = parse_shell(&value);
        assert_eq!(threads.len(), 2);
        assert!(threads[0].busy);
        assert!(!threads[1].busy);
        assert!(threads[1].last_run_completed_at.is_some());
    }
}
