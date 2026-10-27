use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, LimitCompact};
use crate::jobs::JobKind;
use crate::t3::{self, ThreadSummary};
use crate::{App, JobOptions, log_event, log_once, log_problem, unix_timestamp};

const STATE_FILE: &str = "limit-compact.json";
const LOCK_FILE: &str = ".limit-compact.lock";
const TOKEN_FILE: &str = "t3-bearer";
const COMPACT_COMMAND: &str = "/compact";
const MAPPING_SLACK_SECONDS: i64 = 10 * 60;
const MAX_PROJECTIONS_PER_TICK: usize = 60;
const USER_MESSAGE_SLACK_SECONDS: i64 = 5;
const TRANSCRIPT_SLACK_SECONDS: u64 = 120;
const TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;
const TODO_FILE: &str = "TODO.md";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitState {
    pub active: bool,
    pub since: Option<u64>,
    pub reason: String,
    pub scope: Vec<String>,
    pub sessions: BTreeMap<String, Parked>,
    pub compacted: BTreeMap<String, u64>,
    pub compacting_sessions: BTreeMap<String, u64>,
    pub inspected: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Parked {
    pub told_at: u64,
    pub cwd: Option<PathBuf>,
    pub transcript: Option<PathBuf>,
    pub stopped_at: Option<u64>,
    pub todo: Option<PathBuf>,
    pub thread_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HookInput {
    hook_event_name: String,
    session_id: String,
    cwd: Option<PathBuf>,
    transcript_path: Option<PathBuf>,
    stop_hook_active: bool,
    agent_id: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Pending,
    Done(String),
}

pub fn wrap_up_message(reason: &str) -> String {
    format!(
        "[claude-account] Usage limit reached: {reason}. Do not start new tool work. Write your \
         current state and the next steps into the task's TODO.md (.tasks/<date>-<slug>/TODO.md \
         in the domain repo), then end your turn. End your final reply with the line \
         `TODO: <absolute path to that TODO.md>`. This session is resumed automatically once \
         the 5-hour window has reset."
    )
}

pub fn resume_message(parked: &Parked) -> String {
    let pointer = match (&parked.todo, &parked.cwd) {
        (Some(todo), _) => format!("Read {} first", todo.display()),
        (None, Some(cwd)) => format!(
            "Read the TODO.md you wrote under .tasks/ (working directory {}) first",
            cwd.display()
        ),
        (None, None) => "Read the TODO.md you wrote under .tasks/ first".to_owned(),
    };
    format!(
        "[claude-account] The 5-hour usage limit has reset. You stopped earlier because every \
         account was at its limit. {pointer} and continue the task from there. If nothing is \
         left to do, say so in one line and stop."
    )
}

pub fn find_todo_path(text: &str) -> Option<PathBuf> {
    let separator =
        |c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | '<' | '>');
    text.split(separator)
        .map(|word| word.trim_end_matches(['.', ',', ':', ';']))
        .rfind(|word| word.starts_with('/') && word.ends_with(TODO_FILE))
        .map(PathBuf::from)
}

fn last_assistant_text(transcript: &Path) -> Option<String> {
    let mut file = fs::File::open(transcript).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(
        length.saturating_sub(TRANSCRIPT_TAIL_BYTES),
    ))
    .ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    tail.lines().rev().find_map(|line| {
        let value: Value = serde_json::from_str(line).ok()?;
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        let text: String = value
            .pointer("/message/content")?
            .as_array()?
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        (!text.trim().is_empty()).then_some(text)
    })
}

fn todo_from_transcript(transcript: Option<&Path>) -> Option<PathBuf> {
    transcript
        .and_then(last_assistant_text)
        .and_then(|text| find_todo_path(&text))
}

fn with_todo(parked: &Parked) -> Parked {
    let mut parked = parked.clone();
    if parked.todo.is_none() {
        parked.todo = todo_from_transcript(parked.transcript.as_deref());
    }
    parked
}

fn hook_reply(input: &HookInput, state: &mut LimitState, now: u64) -> Option<Value> {
    if !state.active || input.session_id.is_empty() || input.agent_id.is_some() {
        return None;
    }
    if !state.scope.is_empty() && !state.scope.contains(&input.session_id) {
        return None;
    }
    let message = wrap_up_message(&state.reason);
    match input.hook_event_name.as_str() {
        "PostToolUse" => {
            if state.sessions.contains_key(&input.session_id) {
                return None;
            }
            state
                .sessions
                .insert(input.session_id.clone(), parked_from(input, now));
            Some(json!({
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": message,
                }
            }))
        }
        "Stop" => {
            if state.compacting_sessions.contains_key(&input.session_id) {
                return None;
            }
            match state.sessions.get_mut(&input.session_id) {
                Some(parked) => {
                    parked.stopped_at = Some(now);
                    if let Some(todo) = todo_from_transcript(input.transcript_path.as_deref()) {
                        parked.todo = Some(todo);
                    }
                    None
                }
                None if input.stop_hook_active => None,
                None => {
                    state
                        .sessions
                        .insert(input.session_id.clone(), parked_from(input, now));
                    Some(json!({ "decision": "block", "reason": message }))
                }
            }
        }
        _ => None,
    }
}

fn parked_from(input: &HookInput, now: u64) -> Parked {
    Parked {
        told_at: now,
        cwd: input.cwd.clone(),
        transcript: input.transcript_path.clone(),
        ..Parked::default()
    }
}

fn warm_candidate(thread: &ThreadSummary, limit: &LimitCompact, now: DateTime<Utc>) -> bool {
    let Some(completed) = thread.last_run_completed_at else {
        return false;
    };
    let idle = now.signed_duration_since(completed).num_seconds();
    !thread.busy
        && idle >= minutes(limit.idle_minutes)
        && idle < minutes(limit.cache_ttl_minutes)
        && thread
            .last_user_message_at
            .is_none_or(|message| message <= completed)
}

fn minutes(value: u64) -> i64 {
    i64::try_from(value.saturating_mul(60)).unwrap_or(i64::MAX)
}

fn worth_compacting(detail: &t3::ThreadDetail, limit: &LimitCompact) -> bool {
    !detail.last_item_is_compaction
        && detail
            .used_tokens
            .is_some_and(|tokens| tokens >= limit.min_context_tokens)
}

fn to_utc(seconds: u64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(i64::try_from(seconds).ok()?, 0)
}

fn user_continued(thread: &ThreadSummary, since: u64) -> bool {
    let Some(since) = to_utc(since) else {
        return false;
    };
    thread.last_user_message_at.is_some_and(|message| {
        message.signed_duration_since(since).num_seconds() > USER_MESSAGE_SLACK_SECONDS
    })
}

impl App {
    fn limit_state_path(&self) -> PathBuf {
        self.paths.store.join(STATE_FILE)
    }

    pub fn load_limit_state(&self) -> LimitState {
        fs::read(self.limit_state_path())
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    fn update_limit_state<T>(&self, change: impl FnOnce(&mut LimitState) -> T) -> Result<T> {
        self.ensure_store()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.paths.store.join(LOCK_FILE))
            .context("Limit-Lock nicht zu oeffnen")?;
        FileExt::lock_exclusive(&lock).context("Limit-Lock nicht zu setzen")?;
        let mut state = self.load_limit_state();
        let before = state.clone();
        let result = change(&mut state);
        if state != before {
            let payload = serde_json::to_vec_pretty(&state)?;
            crate::atomic_write(&self.limit_state_path(), &payload, 0o600)?;
        }
        Ok(result)
    }

    pub fn hook(&self, input: &str) -> Result<Option<String>> {
        if !self.load_limit_state().active {
            return Ok(None);
        }
        let input: HookInput = serde_json::from_str(input).context("Hook-Eingabe ist kein JSON")?;
        let now = unix_timestamp()?;
        let reply = self.update_limit_state(|state| hook_reply(&input, state, now))?;
        Ok(reply.map(|value| value.to_string()))
    }

    fn t3_client(&self, limit: &LimitCompact) -> t3::Client {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        t3::Client::new(
            &limit.t3_url,
            self.paths.store.join(TOKEN_FILE),
            home.join(".local/bin/t3"),
        )
    }

    fn five_hour_levels(&self) -> Result<Option<Vec<f64>>> {
        let now = unix_timestamp()?;
        let profiles = self.load_profiles()?;
        if profiles.is_empty() {
            return Ok(None);
        }
        let levels: Option<Vec<f64>> = profiles
            .iter()
            .map(|profile| {
                self.read_cached_usage(&profile.name)
                    .filter(|cached| {
                        now.saturating_sub(cached.fetched_at) <= crate::USAGE_CACHE_MAX_AGE_SECONDS
                    })
                    .map(|cached| cached.usage.five_hour.utilization)
            })
            .collect();
        Ok(levels)
    }

    pub(crate) fn limit_tick(
        &self,
        config: &Config,
        spoken: &mut HashMap<String, String>,
    ) -> Result<()> {
        let limit = &config.limit_compact;
        let levels = self.five_hour_levels()?;
        let all_at_limit = levels
            .as_ref()
            .is_some_and(|levels| levels.iter().all(|level| *level >= limit.threshold));
        let any_free = levels
            .as_ref()
            .is_some_and(|levels| levels.iter().any(|level| *level < limit.resume_below));
        let at_limit = limit.enabled && (limit.force || all_at_limit);
        let released = !limit.enabled || (!limit.force && any_free);
        let state = self.load_limit_state();

        if at_limit && !state.active {
            let reason = if limit.force {
                "forced test mode of the account switcher".to_owned()
            } else {
                format!(
                    "every saved Claude account is at or above {:.0}% of its 5-hour window",
                    limit.threshold
                )
            };
            let now = unix_timestamp()?;
            self.update_limit_state(|state| {
                state.active = true;
                state.since = Some(now);
                state.reason = reason.clone();
                state.scope = limit.scope_sessions.clone();
            })?;
            log_event(&format!("Limit-Pause beginnt: {reason}"));
        }
        let state = self.load_limit_state();
        if !state.active {
            return Ok(());
        }
        if at_limit {
            if let Err(error) = self.compact_pass(limit, &state) {
                log_once(
                    spoken,
                    "limit-compact",
                    &format!("Kompaktieren fehlgeschlagen: {error:#}"),
                );
            } else {
                spoken.remove("limit-compact");
            }
        } else if released {
            if let Err(error) = self.resume_pass(limit, &state) {
                log_once(
                    spoken,
                    "limit-resume",
                    &format!("Fortsetzen fehlgeschlagen: {error:#}"),
                );
            } else {
                spoken.remove("limit-resume");
            }
        }
        Ok(())
    }

    fn map_sessions(
        &self,
        rpc: &mut t3::Rpc,
        threads: &[ThreadSummary],
        state: &LimitState,
    ) -> Result<BTreeMap<String, String>> {
        let unmapped: Vec<(&String, &Parked)> = state
            .sessions
            .iter()
            .filter(|(_, parked)| parked.thread_id.is_none())
            .collect();
        let Some(earliest) = unmapped.iter().map(|(_, parked)| parked.told_at).min() else {
            return Ok(BTreeMap::new());
        };
        let Some(earliest) = to_utc(earliest) else {
            return Ok(BTreeMap::new());
        };
        let mut recent: Vec<&ThreadSummary> = threads
            .iter()
            .filter(|thread| {
                thread.updated_at.is_some_and(|updated| {
                    updated.signed_duration_since(earliest).num_seconds() >= -MAPPING_SLACK_SECONDS
                })
            })
            .collect();
        recent.sort_by_key(|thread| std::cmp::Reverse(thread.updated_at));
        let mut found = BTreeMap::new();
        for thread in recent.into_iter().take(MAX_PROJECTIONS_PER_TICK) {
            let detail = rpc.thread_detail(&thread.id)?;
            for (session, _) in &unmapped {
                if detail.native_session_ids.iter().any(|id| id == *session) {
                    found.insert((*session).clone(), thread.id.clone());
                }
            }
            if found.len() == unmapped.len() {
                break;
            }
        }
        if !found.is_empty() {
            self.update_limit_state(|state| {
                for (session, thread) in &found {
                    if let Some(parked) = state.sessions.get_mut(session) {
                        parked.thread_id = Some(thread.clone());
                    }
                }
            })?;
        }
        Ok(found)
    }

    fn compact_pass(&self, limit: &LimitCompact, state: &LimitState) -> Result<()> {
        let client = self.t3_client(limit);
        let threads = client.claude_threads()?;
        let now = Utc::now();
        let mut candidates: Vec<&ThreadSummary> = threads
            .iter()
            .filter(|thread| !state.compacted.contains_key(&thread.id))
            .filter(|thread| warm_candidate(thread, limit, now))
            .filter(|thread| {
                let stamp = thread.last_run_completed_at.map(|time| time.to_rfc3339());
                state.inspected.get(&thread.id) != stamp.as_ref()
            })
            .collect();
        let unmapped = state
            .sessions
            .values()
            .any(|parked| parked.thread_id.is_none());
        if candidates.is_empty() && !unmapped {
            return Ok(());
        }
        candidates.sort_by_key(|thread| std::cmp::Reverse(thread.last_run_completed_at));
        let mut rpc = client.connect()?;
        self.map_sessions(&mut rpc, &threads, state)?;
        let budget = limit.max_compactions.saturating_sub(state.compacted.len());
        let mut sent = 0;
        for thread in candidates {
            let detail = rpc.thread_detail(&thread.id)?;
            let stamp = thread
                .last_run_completed_at
                .map(|time| time.to_rfc3339())
                .unwrap_or_default();
            let in_scope = limit.scope_sessions.is_empty()
                || detail
                    .native_session_ids
                    .iter()
                    .any(|id| limit.scope_sessions.contains(id));
            if sent >= budget || !in_scope || !worth_compacting(&detail, limit) {
                self.update_limit_state(|state| {
                    state.inspected.insert(thread.id.clone(), stamp.clone());
                })?;
                continue;
            }
            let at = unix_timestamp()?;
            self.update_limit_state(|state| {
                for session in &detail.native_session_ids {
                    state.compacting_sessions.insert(session.clone(), at);
                }
                state.compacted.insert(thread.id.clone(), at);
            })?;
            rpc.send_message(&thread.id, COMPACT_COMMAND)?;
            sent += 1;
            log_event(&format!(
                "Limit-Pause: Thread {} kompaktiert ({} Tokens im Kontext)",
                thread.id,
                detail.used_tokens.unwrap_or(0)
            ));
        }
        Ok(())
    }

    fn resume_pass(&self, limit: &LimitCompact, state: &LimitState) -> Result<()> {
        let outcomes: Vec<(String, Outcome)> = if state.sessions.is_empty() {
            Vec::new()
        } else {
            let client = self.t3_client(limit);
            let threads = client.claude_threads()?;
            let mut rpc = client.connect()?;
            let found = self.map_sessions(&mut rpc, &threads, state)?;
            let mut outcomes = Vec::new();
            for (session, parked) in &state.sessions {
                let thread_id = parked
                    .thread_id
                    .clone()
                    .or_else(|| found.get(session).cloned());
                let outcome = match thread_id {
                    Some(thread_id) => {
                        self.resume_thread(&mut rpc, &threads, &thread_id, parked, state)?
                    }
                    None => self.resume_without_thread(session, parked)?,
                };
                outcomes.push((session.clone(), outcome));
            }
            outcomes
        };
        let mut done = Vec::new();
        for (session, outcome) in outcomes {
            if let Outcome::Done(what) = outcome {
                log_event(&format!("Limit-Pause: Sitzung {session} {what}"));
                done.push(session);
            }
        }
        let finished = self.update_limit_state(|state| {
            for session in &done {
                state.sessions.remove(session);
            }
            if state.sessions.is_empty() {
                *state = LimitState::default();
                true
            } else {
                false
            }
        })?;
        if finished {
            log_event("Limit-Pause beendet: alle angehaltenen Sitzungen sind versorgt");
        }
        Ok(())
    }

    fn resume_thread(
        &self,
        rpc: &mut t3::Rpc,
        threads: &[ThreadSummary],
        thread_id: &str,
        parked: &Parked,
        state: &LimitState,
    ) -> Result<Outcome> {
        let Some(thread) = threads.iter().find(|thread| thread.id == thread_id) else {
            return Ok(Outcome::Done(format!(
                "nicht fortgesetzt: Thread {thread_id} ist archiviert oder geloescht"
            )));
        };
        let ours = state
            .compacted
            .get(thread_id)
            .copied()
            .unwrap_or(parked.told_at)
            .max(parked.told_at);
        if user_continued(thread, ours) {
            return Ok(Outcome::Done(format!(
                "wurde in Thread {thread_id} schon von Hand fortgesetzt"
            )));
        }
        if thread.busy {
            return Ok(Outcome::Pending);
        }
        rpc.send_message(thread_id, &resume_message(&with_todo(parked)))?;
        Ok(Outcome::Done(format!("in Thread {thread_id} fortgesetzt")))
    }

    fn resume_without_thread(&self, session: &str, parked: &Parked) -> Result<Outcome> {
        let (Some(stopped_at), Some(cwd)) = (parked.stopped_at, parked.cwd.clone()) else {
            return Ok(Outcome::Done(
                "nicht fortgesetzt: kein T3-Thread und kein Turn-Ende gesehen".to_owned(),
            ));
        };
        let modified = parked
            .transcript
            .as_deref()
            .and_then(|path| fs::metadata(path).ok())
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_secs());
        if modified.is_some_and(|modified| modified > stopped_at + TRANSCRIPT_SLACK_SECONDS) {
            return Ok(Outcome::Done(
                "lief nach dem Stopp schon weiter, kein Fortsetzen noetig".to_owned(),
            ));
        }
        let job = self.add_job(
            JobKind::Resume {
                session_id: session.to_owned(),
                text: resume_message(&with_todo(parked)),
            },
            cwd,
            JobOptions::default(),
        )?;
        Ok(Outcome::Done(format!(
            "als Aufgabe [{}] eingeplant",
            job.id
        )))
    }

    pub fn show_limit_state(&self) -> Result<()> {
        let config = self.load_config();
        let limit = &config.limit_compact;
        println!(
            "Limit-Pause: {}{}",
            if limit.enabled { "an" } else { "aus" },
            if limit.force { " (erzwungen)" } else { "" }
        );
        println!(
            "Schwelle {:.0}%, fortsetzen unter {:.0}%, kompaktieren ab {} Tokens, \
             Cache-Fenster {} min, Ruhezeit {} min, hoechstens {} Threads",
            limit.threshold,
            limit.resume_below,
            limit.min_context_tokens,
            limit.cache_ttl_minutes,
            limit.idle_minutes,
            limit.max_compactions
        );
        let state = self.load_limit_state();
        if !state.active {
            println!("Zustand: keine Pause aktiv");
            return Ok(());
        }
        println!("Zustand: Pause aktiv ({})", state.reason);
        for (session, parked) in &state.sessions {
            println!(
                "  Sitzung {session}: Thread {}, {}, TODO {}",
                parked.thread_id.as_deref().unwrap_or("-"),
                if parked.stopped_at.is_some() {
                    "angehalten"
                } else {
                    "angewiesen"
                },
                parked
                    .todo
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "-".to_owned())
            );
        }
        for thread in state.compacted.keys() {
            println!("  kompaktiert: Thread {thread}");
        }
        Ok(())
    }
}

pub fn log_hook_failure(error: &anyhow::Error) {
    log_problem(&format!("Limit-Hook fehlgeschlagen: {error:#}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(event: &str, session: &str) -> HookInput {
        HookInput {
            hook_event_name: event.to_owned(),
            session_id: session.to_owned(),
            cwd: Some(PathBuf::from("/repo")),
            ..HookInput::default()
        }
    }

    fn active() -> LimitState {
        LimitState {
            active: true,
            reason: "test".to_owned(),
            ..LimitState::default()
        }
    }

    #[test]
    fn inactive_state_never_answers() {
        let mut state = LimitState::default();
        assert!(hook_reply(&input("PostToolUse", "s"), &mut state, 1).is_none());
        assert!(hook_reply(&input("Stop", "s"), &mut state, 1).is_none());
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn post_tool_use_tells_once_and_stop_then_records() {
        let mut state = active();
        let first = hook_reply(&input("PostToolUse", "s"), &mut state, 10);
        assert!(first.is_some_and(|value| {
            value
                .pointer("/hookSpecificOutput/additionalContext")
                .is_some()
        }));
        assert!(hook_reply(&input("PostToolUse", "s"), &mut state, 11).is_none());
        assert!(hook_reply(&input("Stop", "s"), &mut state, 12).is_none());
        assert_eq!(state.sessions["s"].stopped_at, Some(12));
    }

    #[test]
    fn untold_stop_is_blocked_once() {
        let mut state = active();
        let reply = hook_reply(&input("Stop", "s"), &mut state, 10);
        assert_eq!(
            reply.and_then(|value| value.get("decision").cloned()),
            Some(json!("block"))
        );
        assert!(hook_reply(&input("Stop", "s"), &mut state, 20).is_none());
        assert_eq!(state.sessions["s"].stopped_at, Some(20));
    }

    #[test]
    fn scoped_pause_only_touches_listed_sessions() {
        let mut state = active();
        state.scope = vec!["s".to_owned()];
        assert!(hook_reply(&input("PostToolUse", "other"), &mut state, 10).is_none());
        assert!(hook_reply(&input("PostToolUse", "s"), &mut state, 10).is_some());
    }

    #[test]
    fn compaction_turns_and_subagents_are_left_alone() {
        let mut state = active();
        state.compacting_sessions.insert("s".to_owned(), 1);
        assert!(hook_reply(&input("Stop", "s"), &mut state, 10).is_none());
        let mut sub = input("PostToolUse", "t");
        sub.agent_id = Some("a".to_owned());
        assert!(hook_reply(&sub, &mut state, 10).is_none());
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn todo_path_comes_from_the_last_mention() {
        let text = "Saved.\nTODO: `/home/x/repo/.tasks/2026-10-27-a/TODO.md`.";
        assert_eq!(
            find_todo_path(text),
            Some(PathBuf::from("/home/x/repo/.tasks/2026-10-27-a/TODO.md"))
        );
        assert_eq!(find_todo_path("see .tasks/x/TODO.md"), None);
    }

    #[test]
    fn only_warm_idle_threads_are_candidates() {
        let limit = LimitCompact::default();
        let now = Utc::now();
        let thread = |minutes_ago: i64, busy: bool| ThreadSummary {
            id: "t".to_owned(),
            busy,
            last_run_completed_at: Some(now - chrono::Duration::minutes(minutes_ago)),
            last_user_message_at: Some(now - chrono::Duration::minutes(minutes_ago + 1)),
            updated_at: None,
        };
        assert!(warm_candidate(&thread(10, false), &limit, now));
        assert!(!warm_candidate(&thread(10, true), &limit, now));
        assert!(!warm_candidate(&thread(0, false), &limit, now));
        assert!(!warm_candidate(&thread(70, false), &limit, now));
    }
}
