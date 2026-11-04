use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::config::LimitCompact;
use crate::t3::{self, ThreadSummary};
use crate::{App, log_event, log_once, unix_millis};

const STATE_FILE: &str = "subagent-compact.json";
const LOCK_FILE: &str = ".subagent-compact.lock";
const TICK_LOCK_FILE: &str = ".subagent-compact-tick.lock";
const COMPACT_COMMAND: &str = "/compact";
const MARK_MAX_AGE_MS: u64 = 3 * 60 * 60 * 1000;
const MAPPING_SLACK_MS: i64 = 10 * 60 * 1000;
const SPAWN_TOOLS: [&str; 3] = ["agent", "task", "spawn_subagent"];
const T3_ENTRYPOINT: &str = "sdk-ts";
const ENTRYPOINT_ENV: &str = "CLAUDE_CODE_ENTRYPOINT";
const SPOKEN_KEY: &str = "subagent-compact";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Marks {
    pub sessions: BTreeMap<String, Mark>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mark {
    pub marked_at_ms: u64,
    pub thread_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SpawnInput {
    hook_event_name: String,
    session_id: String,
    tool_name: String,
    agent_id: Option<String>,
    model: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Step {
    Wait,
    Drop,
    Inspect,
}

fn is_spawn_tool(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    SPAWN_TOOLS.contains(&name.as_str()) || name.ends_with("__delegate_task")
}

fn is_parent_spawn(input: &SpawnInput) -> bool {
    input.hook_event_name == "PostToolUse"
        && input.agent_id.is_none()
        && !input.session_id.is_empty()
        && is_spawn_tool(&input.tool_name)
}

fn in_scope(session: &str, limit: &LimitCompact) -> bool {
    limit.scope_sessions.is_empty() || limit.scope_sessions.iter().any(|id| id == session)
}

fn millis(time: DateTime<Utc>) -> u64 {
    u64::try_from(time.timestamp_millis()).unwrap_or(0)
}

fn minutes_ms(value: u64) -> u64 {
    value.saturating_mul(60_000)
}

fn step(mark: &Mark, thread: Option<&ThreadSummary>, now_ms: u64, limit: &LimitCompact) -> Step {
    let Some(thread) = thread else {
        return Step::Drop;
    };
    if thread.settled {
        return Step::Drop;
    }
    if thread.busy {
        return Step::Wait;
    }
    let Some(completed) = thread.last_run_completed_at.map(millis) else {
        return Step::Wait;
    };
    if completed < mark.marked_at_ms {
        return Step::Wait;
    }
    let idle = now_ms.saturating_sub(completed);
    if idle >= minutes_ms(limit.cache_ttl_minutes) {
        return Step::Drop;
    }
    if idle < minutes_ms(limit.idle_minutes) {
        return Step::Wait;
    }
    Step::Inspect
}

fn expired(mark: &Mark, now_ms: u64) -> bool {
    now_ms.saturating_sub(mark.marked_at_ms) > MARK_MAX_AGE_MS
}

fn drop_expired(marks: &mut Marks, now_ms: u64) {
    marks.sessions.retain(|_, mark| !expired(mark, now_ms));
}

fn remove_if_unchanged(marks: &mut Marks, session: &str, seen: &Mark) {
    if marks
        .sessions
        .get(session)
        .is_some_and(|mark| mark.marked_at_ms == seen.marked_at_ms)
    {
        marks.sessions.remove(session);
    }
}

impl App {
    fn marks_path(&self) -> PathBuf {
        self.paths.store.join(STATE_FILE)
    }

    fn load_marks(&self) -> Result<Marks> {
        match fs::read(self.marks_path()) {
            Ok(data) => serde_json::from_slice(&data)
                .context("Zustand des Subagent-Kompaktierens ist kein gueltiges JSON"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Marks::default()),
            Err(error) => Err(error).context("Zustand des Subagent-Kompaktierens nicht lesbar"),
        }
    }

    fn update_marks<T>(&self, change: impl FnOnce(&mut Marks) -> T) -> Result<T> {
        let lock = self.open_lock(LOCK_FILE)?;
        FileExt::lock_exclusive(&lock).context("Subagent-Lock nicht zu setzen")?;
        let mut marks = self.load_marks()?;
        let before = marks.clone();
        let result = change(&mut marks);
        if marks != before {
            let payload = serde_json::to_vec_pretty(&marks)?;
            crate::atomic_write(&self.marks_path(), &payload, 0o600)?;
        }
        Ok(result)
    }

    pub fn note_subagent_spawn(&self, input: &str) -> Result<()> {
        let Ok(input) = serde_json::from_str::<SpawnInput>(input) else {
            return Ok(());
        };
        if !is_parent_spawn(&input) {
            return Ok(());
        }
        if std::env::var(ENTRYPOINT_ENV).ok().as_deref() != Some(T3_ENTRYPOINT) {
            return Ok(());
        }
        self.check_config_file()?;
        let limit = self.load_config().limit_compact;
        if !limit.after_subagent || !in_scope(&input.session_id, &limit) {
            return Ok(());
        }
        if !crate::limit::claude_session(input.model.as_deref()) {
            return Ok(());
        }
        let now = unix_millis()?;
        self.update_marks(|marks| {
            marks.sessions.insert(
                input.session_id.clone(),
                Mark {
                    marked_at_ms: now,
                    thread_id: None,
                },
            );
        })
    }

    pub(crate) fn subagent_compact_tick(&self, spoken: &mut HashMap<String, String>) {
        let result = self.subagent_compact_pass(spoken);
        match result {
            Ok(()) => {
                spoken.remove(SPOKEN_KEY);
            }
            Err(error) => log_once(
                spoken,
                SPOKEN_KEY,
                &format!("Subagent-Kompaktieren fehlgeschlagen: {error:#}"),
            ),
        }
    }

    fn subagent_compact_pass(&self, spoken: &mut HashMap<String, String>) -> Result<()> {
        let tick_lock = self.open_lock(TICK_LOCK_FILE)?;
        if FileExt::try_lock_exclusive(&tick_lock).is_err() {
            return Ok(());
        }
        let now = unix_millis()?;
        let marks = self.update_marks(|marks| {
            drop_expired(marks, now);
            marks.clone()
        })?;
        if marks.sessions.is_empty() {
            return Ok(());
        }
        self.check_config_file()?;
        let limit = self.load_config().limit_compact;
        if !limit.after_subagent || self.limit_pause_holds() {
            return self.update_marks(|marks| marks.sessions.clear());
        }
        let client = self.t3_client(&limit);
        let threads = client.claude_threads()?;
        let mut rpc: Option<t3::Rpc> = None;
        for (session, mark) in marks.sessions {
            let key = format!("{SPOKEN_KEY}:{session}");
            match self.handle_mark(&client, &mut rpc, &threads, &limit, &session, &mark, now) {
                Ok(()) => {
                    spoken.remove(&key);
                }
                Err(error) => {
                    rpc = None;
                    log_once(
                        spoken,
                        &key,
                        &format!(
                            "Subagent-Kompaktieren fuer Sitzung {session} fehlgeschlagen: {error:#}"
                        ),
                    );
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_mark(
        &self,
        client: &t3::Client,
        rpc: &mut Option<t3::Rpc>,
        threads: &[ThreadSummary],
        limit: &LimitCompact,
        session: &str,
        mark: &Mark,
        now: u64,
    ) -> Result<()> {
        let thread_id = match mark.thread_id.clone() {
            Some(id) => Some(id),
            None => self.map_mark(client, rpc, threads, session, mark)?,
        };
        let Some(thread_id) = thread_id else {
            return Ok(());
        };
        let summary = threads.iter().find(|thread| thread.id == thread_id);
        match step(mark, summary, now, limit) {
            Step::Wait => Ok(()),
            Step::Drop => self.forget_mark(session, mark),
            Step::Inspect => {
                self.forget_mark(session, mark)?;
                let rpc = connected(client, rpc)?;
                let detail = rpc.thread_detail(&thread_id)?;
                if !crate::limit::worth_compacting(&detail, limit) {
                    return Ok(());
                }
                self.note_compacting(&detail.native_session_ids)?;
                let message_id = uuid::Uuid::new_v4().to_string();
                rpc.send_message(&thread_id, COMPACT_COMMAND, &message_id)?;
                log_event(&format!(
                    "Subagent-Kompaktieren: Thread {thread_id} kompaktiert ({} Tokens im Kontext)",
                    detail.used_tokens.unwrap_or(0)
                ));
                Ok(())
            }
        }
    }

    fn forget_mark(&self, session: &str, seen: &Mark) -> Result<()> {
        self.update_marks(|marks| remove_if_unchanged(marks, session, seen))
    }

    fn map_mark(
        &self,
        client: &t3::Client,
        rpc: &mut Option<t3::Rpc>,
        threads: &[ThreadSummary],
        session: &str,
        mark: &Mark,
    ) -> Result<Option<String>> {
        let since = i64::try_from(mark.marked_at_ms)
            .unwrap_or(i64::MAX)
            .saturating_sub(MAPPING_SLACK_MS);
        let mut recent: Vec<&ThreadSummary> = threads
            .iter()
            .filter(|thread| {
                thread
                    .updated_at
                    .is_some_and(|updated| updated.timestamp_millis() >= since)
            })
            .collect();
        if recent.is_empty() {
            return Ok(None);
        }
        recent.sort_by_key(|thread| std::cmp::Reverse(thread.updated_at));
        let rpc = connected(client, rpc)?;
        for thread in recent {
            let detail = rpc.thread_detail(&thread.id)?;
            if detail.native_session_ids.iter().any(|id| id == session) {
                let id = thread.id.clone();
                self.update_marks(|marks| {
                    if let Some(mark) = marks.sessions.get_mut(session) {
                        mark.thread_id = Some(id.clone());
                    }
                })?;
                return Ok(Some(id));
            }
        }
        Ok(None)
    }
}

fn connected<'a>(client: &t3::Client, rpc: &'a mut Option<t3::Rpc>) -> Result<&'a mut t3::Rpc> {
    if rpc.is_none() {
        *rpc = Some(client.connect()?);
    }
    rpc.as_mut().context("T3-Verbindung fehlt")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(busy: bool, completed_ms: Option<i64>) -> ThreadSummary {
        ThreadSummary {
            id: "t".to_owned(),
            busy,
            settled: false,
            last_run_completed_at: completed_ms.and_then(DateTime::from_timestamp_millis),
            last_user_message_at: None,
            updated_at: None,
        }
    }

    fn spawn(event: &str, tool: &str, agent: Option<&str>) -> SpawnInput {
        SpawnInput {
            hook_event_name: event.to_owned(),
            session_id: "s".to_owned(),
            tool_name: tool.to_owned(),
            agent_id: agent.map(str::to_owned),
            model: None,
        }
    }

    fn mark(at: u64) -> Mark {
        Mark {
            marked_at_ms: at,
            thread_id: Some("t".to_owned()),
        }
    }

    #[test]
    fn only_parent_spawn_tools_mark_the_session() {
        assert!(is_parent_spawn(&spawn("PostToolUse", "Agent", None)));
        assert!(is_parent_spawn(&spawn("PostToolUse", "Task", None)));
        assert!(is_parent_spawn(&spawn(
            "PostToolUse",
            "mcp__t3-code__delegate_task",
            None
        )));
        assert!(!is_parent_spawn(&spawn("PostToolUse", "Bash", None)));
        assert!(!is_parent_spawn(&spawn("PostToolUse", "Agent", Some("a"))));
        assert!(!is_parent_spawn(&spawn("Stop", "Agent", None)));
        let mut empty = spawn("PostToolUse", "Agent", None);
        empty.session_id.clear();
        assert!(!is_parent_spawn(&empty));
    }

    #[test]
    fn scope_limits_marked_sessions() {
        let mut limit = LimitCompact::default();
        assert!(in_scope("s", &limit));
        limit.scope_sessions = vec!["other".to_owned()];
        assert!(!in_scope("s", &limit));
        limit.scope_sessions.push("s".to_owned());
        assert!(in_scope("s", &limit));
    }

    #[test]
    fn compacts_only_while_the_settled_thread_is_still_warm() {
        let limit = LimitCompact::default();
        let mark = mark(1_000_000);
        let done: u64 = 1_100_000;
        let settle = minutes_ms(limit.idle_minutes);
        let ttl = minutes_ms(limit.cache_ttl_minutes);
        assert_eq!(step(&mark, None, 2_000_000, &limit), Step::Drop);
        assert_eq!(
            step(
                &mark,
                Some(&thread(true, Some(done as i64))),
                2_000_000,
                &limit
            ),
            Step::Wait
        );
        assert_eq!(
            step(&mark, Some(&thread(false, None)), 2_000_000, &limit),
            Step::Wait
        );
        assert_eq!(
            step(
                &mark,
                Some(&thread(false, Some(900_000))),
                2_000_000,
                &limit
            ),
            Step::Wait
        );
        let idle = |at: u64| step(&mark, Some(&thread(false, Some(done as i64))), at, &limit);
        assert_eq!(idle(done + settle - 1), Step::Wait);
        assert_eq!(idle(done + settle), Step::Inspect);
        assert_eq!(idle(done + ttl - 1), Step::Inspect);
        assert_eq!(idle(done + ttl), Step::Drop);
        let mut settled = thread(false, Some(done as i64));
        settled.settled = true;
        assert_eq!(
            step(&mark, Some(&settled), done + settle, &limit),
            Step::Drop
        );
    }

    #[test]
    fn expiry_and_removal_keep_fresh_marks() {
        let mut marks = Marks::default();
        marks.sessions.insert("old".to_owned(), mark(0));
        marks
            .sessions
            .insert("new".to_owned(), mark(MARK_MAX_AGE_MS));
        drop_expired(&mut marks, MARK_MAX_AGE_MS + 1);
        assert!(!marks.sessions.contains_key("old"));
        assert!(marks.sessions.contains_key("new"));
        remove_if_unchanged(&mut marks, "new", &mark(5));
        assert!(marks.sessions.contains_key("new"));
        remove_if_unchanged(&mut marks, "new", &mark(MARK_MAX_AGE_MS));
        assert!(marks.sessions.is_empty());
        remove_if_unchanged(&mut marks, "missing", &mark(0));
    }
}
