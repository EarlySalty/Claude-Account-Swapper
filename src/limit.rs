use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
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
use crate::usage::Usage;
use crate::{App, JobOptions, log_event, log_once, log_problem, unix_timestamp};

const STATE_FILE: &str = "limit-compact.json";
const LOCK_FILE: &str = ".limit-compact.lock";
const TICK_LOCK_FILE: &str = ".limit-tick.lock";
const TOKEN_FILE: &str = "t3-bearer";
const COMPACT_COMMAND: &str = "/compact";
const MAPPING_SLACK_SECONDS: i64 = 10 * 60;
const USER_MESSAGE_SLACK_SECONDS: i64 = 5;
const TRANSCRIPT_SLACK_SECONDS: u64 = 120;
const TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;
const TRANSCRIPT_HEAD_BYTES: u64 = 64 * 1024;
const RESUME_GRACE_SECONDS: u64 = 30 * 60;
const TODO_FILE: &str = "TODO.md";
const T3_ENTRYPOINT: &str = "sdk-ts";
const ENTRYPOINT_ENV: &str = "CLAUDE_CODE_ENTRYPOINT";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitState {
    pub active: bool,
    pub since: Option<u64>,
    pub released_at: Option<u64>,
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
    pub thread_id: Option<String>,
    pub entrypoint: Option<String>,
    pub continued_by_user: bool,
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
    #[serde(skip)]
    entrypoint: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Pending,
    Done(String),
}

#[derive(Debug, Default, PartialEq)]
struct Verdict {
    at_limit: bool,
    released: bool,
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

pub fn resume_message(parked: &Parked, todo: Option<&Path>) -> String {
    let pointer = match (todo, &parked.cwd) {
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

fn read_range(path: &Path, from_end: bool, bytes: u64) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = if from_end {
        length.saturating_sub(bytes)
    } else {
        0
    };
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buffer = Vec::new();
    file.take(bytes).read_to_end(&mut buffer).ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

fn entry_time(value: &Value) -> Option<u64> {
    let stamp = value.get("timestamp").and_then(Value::as_str)?;
    let time = DateTime::parse_from_rfc3339(stamp).ok()?;
    u64::try_from(time.timestamp()).ok()
}

fn assistant_text_since(tail: &str, since: u64) -> Option<String> {
    tail.lines().rev().find_map(|line| {
        let value: Value = serde_json::from_str(line).ok()?;
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        if entry_time(&value).is_none_or(|time| time < since) {
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

fn todo_after(parked: &Parked) -> Option<PathBuf> {
    let tail = read_range(parked.transcript.as_deref()?, true, TRANSCRIPT_TAIL_BYTES)?;
    find_todo_path(&assistant_text_since(&tail, parked.told_at)?)
}

fn entrypoint_from_transcript(transcript: &Path) -> Option<String> {
    let head = read_range(transcript, false, TRANSCRIPT_HEAD_BYTES)?;
    head.lines().find_map(|line| {
        let value: Value = serde_json::from_str(line).ok()?;
        value
            .get("entrypoint")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

fn runs_in_t3(parked: &Parked) -> bool {
    let entrypoint = parked.entrypoint.clone().or_else(|| {
        parked
            .transcript
            .as_deref()
            .and_then(entrypoint_from_transcript)
    });
    entrypoint.as_deref() == Some(T3_ENTRYPOINT)
}

fn hook_reply(input: &HookInput, state: &mut LimitState, now: u64) -> Option<Value> {
    if input.session_id.is_empty() || input.agent_id.is_some() {
        return None;
    }
    if input.hook_event_name == "Stop"
        && state
            .compacting_sessions
            .remove(&input.session_id)
            .is_some()
    {
        return None;
    }
    if !state.active {
        if input.hook_event_name == "Stop"
            && let Some(parked) = state.sessions.get_mut(&input.session_id)
        {
            parked.stopped_at.get_or_insert(now);
        }
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
        "Stop" => match state.sessions.get_mut(&input.session_id) {
            Some(parked) => {
                parked.stopped_at.get_or_insert(now);
                None
            }
            None if input.stop_hook_active => None,
            None => {
                state
                    .sessions
                    .insert(input.session_id.clone(), parked_from(input, now));
                Some(json!({ "decision": "block", "reason": message }))
            }
        },
        _ => None,
    }
}

fn parked_from(input: &HookInput, now: u64) -> Parked {
    Parked {
        told_at: now,
        cwd: input.cwd.clone(),
        transcript: input.transcript_path.clone(),
        entrypoint: input.entrypoint.clone(),
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

fn usable(usage: &Usage, limit: &LimitCompact, switch_threshold: f64) -> bool {
    usage.five_hour.utilization < limit.resume_below
        && usage.seven_day.utilization < switch_threshold
}

fn verdict(
    config: &Config,
    levels: &[(String, Option<Usage>)],
    active_profile: Option<&str>,
) -> Verdict {
    let limit = &config.limit_compact;
    let all_at_limit = !levels.is_empty()
        && levels.iter().all(|(_, usage)| {
            usage
                .as_ref()
                .is_some_and(|usage| usage.five_hour.utilization >= limit.threshold)
        });
    let active_usable = levels.iter().any(|(name, usage)| {
        active_profile == Some(name.as_str())
            && usage
                .as_ref()
                .is_some_and(|usage| usable(usage, limit, config.threshold))
    });
    Verdict {
        at_limit: limit.enabled && (limit.force || all_at_limit),
        released: !limit.enabled || (!limit.force && active_usable),
    }
}

fn mtime_seconds(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
}

fn grace_over(state: &LimitState, now: u64) -> bool {
    state
        .released_at
        .is_none_or(|released| now.saturating_sub(released) >= RESUME_GRACE_SECONDS)
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

    fn open_lock(&self, name: &str) -> Result<File> {
        self.ensure_store()?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.paths.store.join(name))
            .with_context(|| format!("{name} nicht zu oeffnen"))
    }

    fn update_limit_state<T>(&self, change: impl FnOnce(&mut LimitState) -> T) -> Result<T> {
        let lock = self.open_lock(LOCK_FILE)?;
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
        let state = self.load_limit_state();
        if !state.active && state.sessions.is_empty() && state.compacting_sessions.is_empty() {
            return Ok(None);
        }
        let mut input: HookInput =
            serde_json::from_str(input).context("Hook-Eingabe ist kein JSON")?;
        input.entrypoint = std::env::var(ENTRYPOINT_ENV).ok();
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

    fn cached_levels(&self) -> Result<Vec<(String, Option<Usage>)>> {
        let now = unix_timestamp()?;
        Ok(self
            .load_profiles()?
            .into_iter()
            .map(|profile| {
                let usage = self
                    .read_cached_usage(&profile.name)
                    .filter(|cached| {
                        now.saturating_sub(cached.fetched_at) <= crate::USAGE_CACHE_MAX_AGE_SECONDS
                    })
                    .map(|cached| cached.usage);
                (profile.name, usage)
            })
            .collect())
    }

    pub(crate) fn limit_tick(
        &self,
        config: &Config,
        spoken: &mut HashMap<String, String>,
    ) -> Result<()> {
        let tick_lock = self.open_lock(TICK_LOCK_FILE)?;
        if FileExt::try_lock_exclusive(&tick_lock).is_err() {
            return Ok(());
        }
        let limit = &config.limit_compact;
        let levels = self.cached_levels()?;
        let active_profile = self.active_profile_name_memoized(None);
        let verdict = verdict(config, &levels, active_profile.as_deref());
        let now = unix_timestamp()?;
        self.enter_or_release(limit, &verdict, now)?;

        let state = self.load_limit_state();
        if state.active {
            if verdict.at_limit {
                let result = self.compact_pass(limit, &state);
                report(
                    spoken,
                    "limit-compact",
                    "Kompaktieren fehlgeschlagen",
                    result,
                );
            }
            return Ok(());
        }
        if state.sessions.is_empty() {
            if state != LimitState::default() {
                self.update_limit_state(|state| *state = LimitState::default())?;
            }
            return Ok(());
        }
        if !limit.enabled {
            self.drop_parked("nicht fortgesetzt: Limit-Pause wurde abgeschaltet")?;
        } else if verdict.released {
            let result = self.resume_pass(limit, spoken, now);
            report(spoken, "limit-resume", "Fortsetzen fehlgeschlagen", result);
        }
        Ok(())
    }

    fn enter_or_release(&self, limit: &LimitCompact, verdict: &Verdict, now: u64) -> Result<()> {
        let reason = if limit.force {
            "forced test mode of the account switcher".to_owned()
        } else {
            format!(
                "every saved Claude account is at or above {:.0}% of its 5-hour window",
                limit.threshold
            )
        };
        let change = self.update_limit_state(|state| {
            if verdict.at_limit && !state.active {
                state.active = true;
                state.since = Some(now);
                state.released_at = None;
                state.reason = reason.clone();
                state.scope = limit.scope_sessions.clone();
                Some(format!("Limit-Pause beginnt: {reason}"))
            } else if verdict.released && state.active {
                state.active = false;
                state.released_at = Some(now);
                Some(format!(
                    "Limit-Pause aufgehoben, {} angehaltene Sitzung(en) werden fortgesetzt",
                    state.sessions.len()
                ))
            } else {
                None
            }
        })?;
        if let Some(line) = change {
            log_event(&line);
        }
        Ok(())
    }

    fn drop_parked(&self, why: &str) -> Result<()> {
        let dropped = self.update_limit_state(|state| {
            let sessions: Vec<String> = state.sessions.keys().cloned().collect();
            *state = LimitState::default();
            sessions
        })?;
        for session in dropped {
            log_event(&format!("Limit-Pause: Sitzung {session} {why}"));
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
            .filter(|(_, parked)| parked.thread_id.is_none() && runs_in_t3(parked))
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
        for thread in recent {
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
            .any(|parked| parked.thread_id.is_none() && runs_in_t3(parked));
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
            self.note_manual_continuation(thread, &detail)?;
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

    fn note_manual_continuation(
        &self,
        thread: &ThreadSummary,
        detail: &t3::ThreadDetail,
    ) -> Result<()> {
        self.update_limit_state(|state| {
            for session in &detail.native_session_ids {
                if let Some(parked) = state.sessions.get_mut(session) {
                    parked.thread_id.get_or_insert_with(|| thread.id.clone());
                    if user_continued(thread, parked.told_at) {
                        parked.continued_by_user = true;
                    }
                }
            }
        })
    }

    fn resume_pass(
        &self,
        limit: &LimitCompact,
        spoken: &mut HashMap<String, String>,
        now: u64,
    ) -> Result<()> {
        let client = self.t3_client(limit);
        let mut t3 = match client.claude_threads() {
            Ok(threads) => client.connect().map(|rpc| (threads, rpc)),
            Err(error) => Err(error),
        };
        let problem = match t3.as_mut() {
            Ok((threads, rpc)) => {
                let state = self.load_limit_state();
                self.map_sessions(rpc, threads, &state)
                    .err()
                    .map(|error| format!("Zuordnung zu T3-Threads fehlgeschlagen: {error:#}"))
            }
            Err(error) => Some(format!("T3 nicht erreichbar: {error:#}")),
        };
        let mapped = problem.is_none();
        let state = self.load_limit_state();
        for (session, parked) in &state.sessions {
            let outcome = match (&parked.thread_id, t3.as_mut()) {
                _ if parked.continued_by_user => {
                    Ok(Outcome::Done("wurde schon von Hand fortgesetzt".to_owned()))
                }
                (Some(thread_id), Ok((threads, rpc))) => {
                    self.resume_thread(rpc, threads, thread_id, parked, &state)
                }
                (Some(_), Err(_)) => Ok(Outcome::Pending),
                (None, _) if runs_in_t3(parked) => Ok(match mapped && grace_over(&state, now) {
                    true => Outcome::Done(
                        "nicht fortgesetzt: kein passender T3-Thread gefunden".to_owned(),
                    ),
                    false => Outcome::Pending,
                }),
                (None, _) => self.resume_without_thread(session, parked, &state, now),
            };
            let key = format!("limit-resume-{session}");
            match outcome {
                Ok(Outcome::Done(what)) => {
                    self.update_limit_state(|state| state.sessions.remove(session))?;
                    spoken.remove(&key);
                    log_event(&format!("Limit-Pause: Sitzung {session} {what}"));
                }
                Ok(Outcome::Pending) => {}
                Err(error) => log_once(
                    spoken,
                    &key,
                    &format!("Limit-Pause: Sitzung {session} nicht fortgesetzt: {error:#}"),
                ),
            }
        }
        let finished = self.update_limit_state(|state| {
            if !state.active && state.sessions.is_empty() {
                *state = LimitState::default();
                true
            } else {
                false
            }
        })?;
        if finished {
            log_event("Limit-Pause beendet: alle angehaltenen Sitzungen sind versorgt");
        }
        match problem {
            Some(problem) => anyhow::bail!("{problem}"),
            None => Ok(()),
        }
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
        rpc.send_message(
            thread_id,
            &resume_message(parked, todo_after(parked).as_deref()),
        )?;
        Ok(Outcome::Done(format!("in Thread {thread_id} fortgesetzt")))
    }

    fn resume_without_thread(
        &self,
        session: &str,
        parked: &Parked,
        state: &LimitState,
        now: u64,
    ) -> Result<Outcome> {
        let Some(stopped_at) = parked.stopped_at else {
            return Ok(match grace_over(state, now) {
                true => Outcome::Done("nicht fortgesetzt: Turn-Ende nie gesehen".to_owned()),
                false => Outcome::Pending,
            });
        };
        let Some(cwd) = parked.cwd.clone() else {
            return Ok(Outcome::Done(
                "nicht fortgesetzt: Arbeitsverzeichnis unbekannt".to_owned(),
            ));
        };
        let modified = parked.transcript.as_deref().and_then(mtime_seconds);
        if modified.is_some_and(|modified| modified > stopped_at + TRANSCRIPT_SLACK_SECONDS) {
            return Ok(Outcome::Done(
                "lief nach dem Stopp schon weiter, kein Fortsetzen noetig".to_owned(),
            ));
        }
        let mut job = self.add_job(
            JobKind::Resume {
                session_id: session.to_owned(),
                text: resume_message(parked, todo_after(parked).as_deref()),
            },
            cwd,
            JobOptions {
                title: Some("Nach der Limit-Pause fortsetzen".to_owned()),
                skip_permissions: false,
                ..JobOptions::default()
            },
        )?;
        job.last_window = None;
        self.write_job(&job)?;
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
        match (state.active, state.sessions.is_empty()) {
            (true, _) => println!("Zustand: Pause aktiv ({})", state.reason),
            (false, false) => println!("Zustand: Pause aufgehoben, Fortsetzen laeuft"),
            (false, true) => {
                println!("Zustand: keine Pause aktiv");
                return Ok(());
            }
        }
        for (session, parked) in &state.sessions {
            println!(
                "  Sitzung {session}: Thread {}, {}, TODO {}",
                parked.thread_id.as_deref().unwrap_or("-"),
                if parked.stopped_at.is_some() {
                    "angehalten"
                } else {
                    "angewiesen"
                },
                todo_after(parked)
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

fn report(spoken: &mut HashMap<String, String>, key: &str, what: &str, result: Result<()>) {
    match result {
        Ok(()) => {
            spoken.remove(key);
        }
        Err(error) => log_once(spoken, key, &format!("{what}: {error:#}")),
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
    fn released_pause_still_records_stops_and_keeps_the_first() {
        let mut state = active();
        hook_reply(&input("PostToolUse", "s"), &mut state, 10);
        state.active = false;
        assert!(hook_reply(&input("PostToolUse", "s"), &mut state, 15).is_none());
        assert!(hook_reply(&input("Stop", "s"), &mut state, 20).is_none());
        assert!(hook_reply(&input("Stop", "s"), &mut state, 30).is_none());
        assert_eq!(state.sessions["s"].stopped_at, Some(20));
        assert!(hook_reply(&input("Stop", "new"), &mut state, 30).is_none());
        assert!(!state.sessions.contains_key("new"));
    }

    #[test]
    fn compaction_exemption_ends_with_its_stop() {
        let mut state = active();
        state.compacting_sessions.insert("s".to_owned(), 1);
        assert!(hook_reply(&input("Stop", "s"), &mut state, 10).is_none());
        assert!(state.compacting_sessions.is_empty());
        assert!(hook_reply(&input("Stop", "s"), &mut state, 11).is_some());
    }

    fn usage(five_hour: f64, seven_day: f64) -> Usage {
        let bucket = |utilization| crate::usage::Bucket {
            utilization,
            resets_at: None,
        };
        Usage {
            five_hour: bucket(five_hour),
            seven_day: bucket(seven_day),
        }
    }

    fn config(auto_switch: bool) -> Config {
        Config {
            auto_switch,
            threshold: 98.0,
            limit_compact: LimitCompact {
                enabled: true,
                ..LimitCompact::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn verdict_needs_usable_quota_to_release() {
        let levels = vec![
            ("a".to_owned(), Some(usage(99.0, 10.0))),
            ("b".to_owned(), None),
        ];
        let partial = verdict(&config(true), &levels, Some("a"));
        assert!(!partial.at_limit && !partial.released);
        let levels = vec![
            ("a".to_owned(), Some(usage(99.0, 10.0))),
            ("b".to_owned(), Some(usage(10.0, 10.0))),
        ];
        assert!(!verdict(&config(true), &levels, Some("a")).released);
        assert!(!verdict(&config(false), &levels, Some("a")).released);
        assert!(!verdict(&config(true), &levels, None).released);
        assert!(verdict(&config(false), &levels, Some("b")).released);
        let weekly_full = vec![("a".to_owned(), Some(usage(0.0, 100.0)))];
        assert!(!verdict(&config(true), &weekly_full, Some("a")).released);
        let mut off = config(true);
        off.limit_compact.enabled = false;
        assert!(verdict(&off, &weekly_full, Some("a")).released);
    }

    #[test]
    fn todo_comes_only_from_replies_after_the_instruction() {
        let line = |time: &str, text: &str| {
            json!({
                "type": "assistant",
                "timestamp": time,
                "message": {"content": [{"type": "text", "text": text}]}
            })
            .to_string()
        };
        let tail = [
            line("2026-10-27T10:00:00Z", "TODO: /old/.tasks/a/TODO.md"),
            line("2026-10-27T10:10:00Z", "Done reading."),
        ]
        .join("\n");
        let told = 1_793_095_500;
        assert_eq!(
            assistant_text_since(&tail, told).and_then(|text| find_todo_path(&text)),
            None
        );
        assert!(assistant_text_since(&tail, told - 1000).is_some());
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
