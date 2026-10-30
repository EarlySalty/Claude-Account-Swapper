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
use crate::jobs::{ContinuationGuard, JobKind};
use crate::t3::{self, ThreadSummary};
use crate::usage::Candidate;
use crate::{App, JobOptions, log_event, log_once, log_problem, unix_millis, unix_timestamp};

const STATE_FILE: &str = "limit-compact.json";
const LOCK_FILE: &str = ".limit-compact.lock";
const TICK_LOCK_FILE: &str = ".limit-tick.lock";
const TOKEN_FILE: &str = "t3-bearer";
const COMPACT_COMMAND: &str = "/compact";
const RESUME_JOB_TITLE: &str = "Nach der Limit-Pause fortsetzen";
const MAPPING_SLACK_SECONDS: i64 = 10 * 60;
const TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;
const TRANSCRIPT_HEAD_BYTES: u64 = 64 * 1024;
const RESUME_GRACE_SECONDS: u64 = 30 * 60;
const T3_RESUME_SETTLE_MS: u64 = 2 * 60 * 1000;
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
    pub told_at_ms: u64,
    pub cwd: Option<PathBuf>,
    pub transcript: Option<PathBuf>,
    pub stopped_at_ms: Option<u64>,
    pub thread_id: Option<String>,
    pub entrypoint: Option<String>,
    pub continued_by_user: bool,
    pub retell: bool,
    pub resume_message_id: Option<String>,
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
         an account has quota again."
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
        "[claude-account] An account has quota again. You stopped earlier because no account \
         had quota left. {pointer} and continue the task from there. If nothing is \
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
    u64::try_from(time.timestamp_millis()).ok()
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

fn prompt_text(value: &Value) -> Option<String> {
    let flag = |name: &str| value.get(name).and_then(Value::as_bool) == Some(true);
    if value.get("type").and_then(Value::as_str) != Some("user")
        || flag("isMeta")
        || flag("isCompactSummary")
    {
        return None;
    }
    let text = match value.pointer("/message/content")? {
        Value::String(text) => text.trim().to_owned(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

fn timed_entries(text: &str) -> impl Iterator<Item = (u64, Value)> + '_ {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| Some((entry_time(&value)?, value)))
}

fn prompt_after(text: &str, since: u64, ignore: Option<&str>) -> bool {
    timed_entries(text).any(|(time, value)| {
        time > since
            && prompt_text(&value)
                .is_some_and(|prompt| Some(prompt.as_str()) != ignore.map(str::trim))
    })
}

fn reaches_back_to(text: &str, since: u64) -> bool {
    timed_entries(text)
        .next()
        .is_some_and(|(time, _)| time <= since)
}

pub(crate) fn user_prompt_after(transcript: &Path, since: u64, ignore: Option<&str>) -> bool {
    let Some(tail) = read_range(transcript, true, TRANSCRIPT_TAIL_BYTES) else {
        return false;
    };
    if prompt_after(&tail, since, ignore) {
        return true;
    }
    let truncated = fs::metadata(transcript).is_ok_and(|meta| meta.len() > TRANSCRIPT_TAIL_BYTES);
    truncated
        && !reaches_back_to(&tail, since)
        && fs::read_to_string(transcript).is_ok_and(|full| prompt_after(&full, since, ignore))
}

fn foreign_message_after(detail: &t3::ThreadDetail, since: u64) -> bool {
    let Some(since) = from_millis(since) else {
        return false;
    };
    detail
        .last_foreign_message_at
        .is_some_and(|message| message > since)
}

fn todo_after(parked: &Parked) -> Option<PathBuf> {
    let tail = read_range(parked.transcript.as_deref()?, true, TRANSCRIPT_TAIL_BYTES)?;
    find_todo_path(&assistant_text_since(&tail, parked.told_at_ms)?)
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

fn hook_reply_with(
    input: &HookInput,
    state: &mut LimitState,
    now: u64,
    still_blocked: bool,
) -> Option<Value> {
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
    if !state.active || !still_blocked {
        if input.hook_event_name == "Stop"
            && let Some(parked) = state.sessions.get_mut(&input.session_id)
        {
            parked.stopped_at_ms.get_or_insert(now);
        }
        return None;
    }
    if !state.scope.is_empty() && !state.scope.contains(&input.session_id) {
        return None;
    }
    let message = wrap_up_message(&state.reason);
    match input.hook_event_name.as_str() {
        "PostToolUse" => {
            match state.sessions.get_mut(&input.session_id) {
                Some(parked) if parked.stopped_at_ms.is_some() || parked.retell => {
                    parked.told_at_ms = now;
                    parked.stopped_at_ms = None;
                    parked.continued_by_user = false;
                    parked.retell = false;
                    parked.resume_message_id = None;
                }
                Some(_) => return None,
                None => {
                    state
                        .sessions
                        .insert(input.session_id.clone(), parked_from(input, now));
                }
            }
            Some(json!({
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": message,
                }
            }))
        }
        "Stop" => match state.sessions.get_mut(&input.session_id) {
            Some(parked) => {
                parked.stopped_at_ms.get_or_insert(now);
                None
            }
            None if input.stop_hook_active => {
                let mut parked = parked_from(input, now);
                parked.stopped_at_ms = Some(now);
                state.sessions.insert(input.session_id.clone(), parked);
                None
            }
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
        told_at_ms: now,
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

fn from_millis(millis: u64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(i64::try_from(millis).ok()?)
}

type Level = (String, Option<Candidate>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fit {
    No,
    Reserve,
    Strict,
}

fn hard_blocked(candidate: &Candidate, config: &Config) -> bool {
    candidate.stops.hard && !candidate.within_stops(config.threshold)
}

fn fit(candidate: &Candidate, config: &Config) -> Fit {
    if !candidate.usage.is_free(config.threshold)
        || candidate.usage.five_hour.utilization >= config.limit_compact.resume_below
        || hard_blocked(candidate, config)
    {
        Fit::No
    } else if candidate.within_stops(config.threshold) {
        Fit::Strict
    } else {
        Fit::Reserve
    }
}

fn blocked(candidate: &Candidate, config: &Config) -> bool {
    candidate.usage.five_hour.utilization >= config.limit_compact.threshold
        || !candidate.usage.is_free(config.threshold)
        || hard_blocked(candidate, config)
}

fn active_fit(config: &Config, levels: &[Level], active_profile: Option<&str>) -> Fit {
    levels
        .iter()
        .filter(|(name, _)| active_profile == Some(name.as_str()))
        .filter_map(|(_, candidate)| candidate.as_ref())
        .map(|candidate| fit(candidate, config))
        .max()
        .unwrap_or(Fit::No)
}

fn best_other(
    config: &Config,
    levels: &[Level],
    active_profile: Option<&str>,
    wanted: Fit,
) -> Option<String> {
    levels
        .iter()
        .filter(|(name, _)| active_profile != Some(name.as_str()))
        .filter_map(|(name, candidate)| Some((name, candidate.as_ref()?)))
        .filter(|(_, candidate)| fit(candidate, config) == wanted)
        .min_by(|(_, left), (_, right)| {
            left.usage
                .five_hour
                .utilization
                .total_cmp(&right.usage.five_hour.utilization)
        })
        .map(|(name, _)| name.clone())
}

fn verdict(config: &Config, levels: &[Level], active_profile: Option<&str>) -> Verdict {
    let limit = &config.limit_compact;
    let all_at_limit = !levels.is_empty()
        && levels.iter().all(|(_, candidate)| {
            candidate
                .as_ref()
                .is_some_and(|candidate| blocked(candidate, config))
        });
    let active_usable = match active_fit(config, levels, active_profile) {
        Fit::Strict => true,
        Fit::Reserve => best_other(config, levels, active_profile, Fit::Strict).is_none(),
        Fit::No => false,
    };
    Verdict {
        at_limit: limit.enabled && (limit.force || all_at_limit),
        released: !limit.enabled || (!limit.force && active_usable),
    }
}

fn switch_target(
    config: &Config,
    levels: &[Level],
    active_profile: Option<&str>,
) -> Option<String> {
    let active = active_fit(config, levels, active_profile);
    if active == Fit::Strict {
        return None;
    }
    best_other(config, levels, active_profile, Fit::Strict).or_else(|| match active {
        Fit::No => best_other(config, levels, active_profile, Fit::Reserve),
        _ => None,
    })
}

fn may_spend(config: &Config, levels: &[Level], active_profile: Option<&str>) -> bool {
    levels
        .iter()
        .filter(|(name, _)| active_profile == Some(name.as_str()))
        .filter_map(|(_, candidate)| candidate.as_ref())
        .any(|candidate| {
            candidate.usage.is_free(config.threshold) && !hard_blocked(candidate, config)
        })
}

fn limit_reason(levels: &[Level]) -> String {
    let figures = levels
        .iter()
        .filter_map(|(_, candidate)| candidate.as_ref())
        .map(|candidate| {
            format!(
                "{}: 5h {:.0}%, 7d {:.0}%",
                candidate.name,
                candidate.usage.five_hour.utilization,
                candidate.usage.seven_day.utilization
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!("no saved Claude account has quota left ({figures})")
}

fn still_blocked(config: &Config, levels: &[Level], active_profile: Option<&str>) -> bool {
    let verdict = verdict(config, levels, active_profile);
    !verdict.released && (verdict.at_limit || !config.auto_switch)
}

fn reenter(state: &mut LimitState) -> Vec<String> {
    state.compacted.clear();
    state.compacting_sessions.clear();
    state.inspected.clear();
    let dropped: Vec<String> = state
        .sessions
        .iter()
        .filter(|(_, parked)| parked.continued_by_user)
        .map(|(session, _)| session.clone())
        .collect();
    for session in &dropped {
        state.sessions.remove(session);
    }
    for parked in state.sessions.values_mut() {
        if parked.stopped_at_ms.is_none() {
            parked.retell = true;
        }
    }
    dropped
}

fn is_t3_session(parked: &Parked) -> bool {
    parked.thread_id.is_some() || runs_in_t3(parked)
}

fn settling(since_ms: u64, now: u64) -> bool {
    now.saturating_mul(1000) < since_ms.saturating_add(T3_RESUME_SETTLE_MS)
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

    pub fn load_limit_state(&self) -> Result<LimitState> {
        match fs::read(self.limit_state_path()) {
            Ok(data) => serde_json::from_slice(&data)
                .context("Zustand der Limit-Pause ist kein gueltiges JSON"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LimitState::default()),
            Err(error) => Err(error).context("Zustand der Limit-Pause nicht lesbar"),
        }
    }

    pub(crate) fn limit_pause_holds(&self) -> bool {
        self.load_limit_state()
            .ok()
            .is_none_or(|state| state.active)
    }

    fn check_config_file(&self) -> Result<()> {
        match fs::read_to_string(self.config_path()) {
            Ok(text) => Config::parse(&text).map(|_| ()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("Einstellungen nicht lesbar"),
        }
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
        let mut state = self.load_limit_state()?;
        let before = state.clone();
        let result = change(&mut state);
        if state != before {
            let payload = serde_json::to_vec_pretty(&state)?;
            crate::atomic_write(&self.limit_state_path(), &payload, 0o600)?;
        }
        Ok(result)
    }

    pub fn hook(&self, input: &str) -> Result<Option<String>> {
        let state = self.load_limit_state()?;
        if !state.active && state.sessions.is_empty() && state.compacting_sessions.is_empty() {
            return Ok(None);
        }
        let mut input: HookInput =
            serde_json::from_str(input).context("Hook-Eingabe ist kein JSON")?;
        input.entrypoint = std::env::var(ENTRYPOINT_ENV).ok();
        let now = unix_millis()?;
        let blocked = !state.active || self.blocked_now()?;
        let reply =
            self.update_limit_state(|state| hook_reply_with(&input, state, now, blocked))?;
        Ok(reply.map(|value| value.to_string()))
    }

    fn blocked_now(&self) -> Result<bool> {
        let config = self.load_config();
        let levels = self.cached_levels()?;
        let active = self.active_profile_name_memoized(None);
        Ok(still_blocked(&config, &levels, active.as_deref()))
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

    fn cached_levels(&self) -> Result<Vec<Level>> {
        let now = unix_timestamp()?;
        Ok(self
            .load_profiles()?
            .into_iter()
            .map(|profile| {
                let candidate = self
                    .read_cached_usage(&profile.name)
                    .filter(|cached| {
                        now.saturating_sub(cached.fetched_at) <= crate::USAGE_CACHE_MAX_AGE_SECONDS
                    })
                    .map(|cached| Candidate {
                        name: profile.name.clone(),
                        usage: cached.usage,
                        stops: profile.limits,
                    });
                (profile.name, candidate)
            })
            .collect())
    }

    fn switch_for_resume(
        &self,
        config: &Config,
        levels: &[Level],
        spoken: &mut HashMap<String, String>,
    ) {
        let active = self.active_profile_name_memoized(None);
        let Some(target) = switch_target(config, levels, active.as_deref()) else {
            spoken.remove("limit-switch-off");
            return;
        };
        if !config.auto_switch {
            log_once(
                spoken,
                "limit-switch-off",
                &format!(
                    "Limit-Pause wartet auf den aktiven Account; {target} waere frei, \
                     der Auto-Wechsel ist aber aus"
                ),
            );
            return;
        }
        log_event(&format!(
            "Limit-Pause: wechsle auf {target}, damit angehaltene Sitzungen fortgesetzt werden"
        ));
        let result = self.switch_checked(&target, true);
        report(
            spoken,
            "limit-switch",
            "Wechsel fuer die Limit-Pause fehlgeschlagen",
            result,
        );
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
        if let Err(error) = self.check_config_file() {
            log_once(
                spoken,
                "limit-config",
                &format!("Limit-Pause ausgesetzt, Einstellungen fehlerhaft: {error:#}"),
            );
            return Ok(());
        }
        spoken.remove("limit-config");
        let limit = &config.limit_compact;
        let levels = self.cached_levels()?;
        let before = self.load_limit_state()?;
        if limit.enabled && !limit.force && (before.active || !before.sessions.is_empty()) {
            self.switch_for_resume(config, &levels, spoken);
        }
        let active_profile = self.active_profile_name_memoized(None);
        let verdict = verdict(config, &levels, active_profile.as_deref());
        let now = unix_timestamp()?;
        self.enter_or_release(config, &levels, &verdict, now)?;

        let state = self.load_limit_state()?;
        if state.active {
            if verdict.at_limit && may_spend(config, &levels, active_profile.as_deref()) {
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

    fn enter_or_release(
        &self,
        config: &Config,
        levels: &[Level],
        verdict: &Verdict,
        now: u64,
    ) -> Result<()> {
        let limit = &config.limit_compact;
        let reason = if limit.force {
            "forced test mode of the account switcher".to_owned()
        } else {
            limit_reason(levels)
        };
        let (change, dropped) = self.update_limit_state(|state| {
            if state.active {
                state.reason = reason.clone();
            }
            if verdict.at_limit && !state.active {
                let dropped = reenter(state);
                state.active = true;
                state.since = Some(now);
                state.released_at = None;
                state.reason = reason.clone();
                state.scope = limit.scope_sessions.clone();
                (Some(format!("Limit-Pause beginnt: {reason}")), dropped)
            } else if verdict.released && state.active {
                state.active = false;
                state.released_at = Some(now);
                (
                    Some(format!(
                        "Limit-Pause aufgehoben, {} angehaltene Sitzung(en) werden fortgesetzt",
                        state.sessions.len()
                    )),
                    Vec::new(),
                )
            } else {
                (None, Vec::new())
            }
        })?;
        if let Some(line) = change {
            log_event(&line);
        }
        for session in dropped {
            log_event(&format!(
                "Limit-Pause: Sitzung {session} aus der vorigen Pause verworfen, sie lief von Hand weiter"
            ));
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
        let Some(earliest) = unmapped.iter().map(|(_, parked)| parked.told_at_ms).min() else {
            return Ok(BTreeMap::new());
        };
        let Some(earliest) = from_millis(earliest) else {
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
            let message_id = uuid::Uuid::new_v4().to_string();
            if let Err(error) = rpc.send_message(&thread.id, COMPACT_COMMAND, &message_id) {
                if !t3::is_rejection(&error) {
                    return Err(error);
                }
                self.update_limit_state(|state| {
                    for session in &detail.native_session_ids {
                        state.compacting_sessions.remove(session);
                    }
                    state.compacted.remove(&thread.id);
                })?;
                return Err(error);
            }
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
                    if foreign_message_after(detail, parked.told_at_ms) {
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
        let before = self.load_limit_state()?;
        let wants_t3 = before
            .sessions
            .values()
            .any(|parked| !parked.continued_by_user && is_t3_session(parked));
        let mut t3 = wants_t3.then(|| self.open_t3(limit, &before));
        let state = self.load_limit_state()?;
        for (session, parked) in &state.sessions {
            let outcome = if parked.continued_by_user {
                Ok(Outcome::Done("wurde schon von Hand fortgesetzt".to_owned()))
            } else if is_t3_session(parked) {
                match t3.as_mut() {
                    Some(Ok((threads, rpc))) => {
                        self.resume_t3_session(rpc, threads, session, parked, &state, now)
                    }
                    _ => Ok(Outcome::Pending),
                }
            } else {
                self.resume_without_thread(session, parked, &state, now)
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
        match t3 {
            Some(Err(error)) => Err(error),
            _ => Ok(()),
        }
    }

    fn open_t3(
        &self,
        limit: &LimitCompact,
        state: &LimitState,
    ) -> Result<(Vec<ThreadSummary>, t3::Rpc)> {
        let client = self.t3_client(limit);
        let threads = client.claude_threads()?;
        let mut rpc = client.connect()?;
        self.map_sessions(&mut rpc, &threads, state)?;
        Ok((threads, rpc))
    }

    fn resume_t3_session(
        &self,
        rpc: &mut t3::Rpc,
        threads: &[ThreadSummary],
        session: &str,
        parked: &Parked,
        state: &LimitState,
        now: u64,
    ) -> Result<Outcome> {
        let waiting_or = |done: &str| match grace_over(state, now) {
            true => Outcome::Done(done.to_owned()),
            false => Outcome::Pending,
        };
        let Some(stopped_at_ms) = parked.stopped_at_ms else {
            return Ok(waiting_or("nicht fortgesetzt: Turn-Ende nie gesehen"));
        };
        let Some(thread_id) = parked.thread_id.as_deref() else {
            return Ok(waiting_or(
                "nicht fortgesetzt: kein passender T3-Thread gefunden",
            ));
        };
        let Some(thread) = threads.iter().find(|thread| thread.id == thread_id) else {
            return Ok(Outcome::Done(format!(
                "nicht fortgesetzt: Thread {thread_id} ist archiviert oder geloescht"
            )));
        };
        let detail = rpc.thread_detail(thread_id)?;
        if let Some(id) = &parked.resume_message_id
            && detail.message_ids.contains(id)
        {
            return Ok(Outcome::Done(format!("in Thread {thread_id} fortgesetzt")));
        }
        if foreign_message_after(&detail, parked.told_at_ms) {
            return Ok(Outcome::Done(format!(
                "wurde in Thread {thread_id} schon fortgesetzt"
            )));
        }
        let released_ms = state.released_at.unwrap_or(0).saturating_mul(1000);
        if settling(stopped_at_ms.max(released_ms), now) || thread.busy {
            return Ok(Outcome::Pending);
        }
        let message_id = match &parked.resume_message_id {
            Some(id) => id.clone(),
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                self.update_limit_state(|state| {
                    if let Some(parked) = state.sessions.get_mut(session) {
                        parked.resume_message_id = Some(id.clone());
                    }
                })?;
                id
            }
        };
        let sent = rpc.send_message(
            thread_id,
            &resume_message(parked, todo_after(parked).as_deref()),
            &message_id,
        );
        if let Err(error) = sent {
            if t3::is_rejection(&error) {
                self.update_limit_state(|state| {
                    if let Some(parked) = state.sessions.get_mut(session) {
                        parked.resume_message_id = None;
                    }
                })?;
            }
            return Err(error);
        }
        Ok(Outcome::Done(format!("in Thread {thread_id} fortgesetzt")))
    }

    fn resume_without_thread(
        &self,
        session: &str,
        parked: &Parked,
        state: &LimitState,
        now: u64,
    ) -> Result<Outcome> {
        let Some(stopped_at_ms) = parked.stopped_at_ms else {
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
        if parked
            .transcript
            .as_deref()
            .is_some_and(|transcript| user_prompt_after(transcript, stopped_at_ms, None))
        {
            return Ok(Outcome::Done(
                "lief nach dem Stopp schon weiter, kein Fortsetzen noetig".to_owned(),
            ));
        }
        let existing = self.load_jobs()?.into_iter().find(|job| {
            job.title == RESUME_JOB_TITLE
                && job.created_at >= stopped_at_ms / 1000
                && matches!(&job.kind, JobKind::Resume { session_id, .. } if session_id == session)
        });
        if let Some(job) = existing {
            return Ok(Outcome::Done(format!(
                "ist schon als Aufgabe [{}] eingeplant",
                job.id
            )));
        }
        let job = self.add_guarded_job(
            JobKind::Resume {
                session_id: session.to_owned(),
                text: resume_message(parked, todo_after(parked).as_deref()),
            },
            cwd,
            JobOptions {
                title: Some(RESUME_JOB_TITLE.to_owned()),
                skip_permissions: false,
                ..JobOptions::default()
            },
            parked
                .transcript
                .clone()
                .map(|transcript| ContinuationGuard {
                    transcript,
                    since_ms: stopped_at_ms,
                }),
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
        let state = self.load_limit_state()?;
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
                if parked.stopped_at_ms.is_some() {
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

    fn hook_reply(input: &HookInput, state: &mut LimitState, now: u64) -> Option<Value> {
        hook_reply_with(input, state, now, true)
    }

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
        assert_eq!(state.sessions["s"].stopped_at_ms, Some(12));
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
        assert_eq!(state.sessions["s"].stopped_at_ms, Some(20));
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
        assert_eq!(state.sessions["s"].stopped_at_ms, Some(20));
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

    fn usage(five_hour: f64, seven_day: f64) -> Option<Candidate> {
        let bucket = |utilization| crate::usage::Bucket {
            utilization,
            resets_at: None,
        };
        Some(Candidate {
            name: String::new(),
            usage: crate::usage::Usage {
                five_hour: bucket(five_hour),
                seven_day: bucket(seven_day),
            },
            stops: crate::usage::Stops::default(),
        })
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
    fn hook_stays_silent_when_fresh_usage_is_no_longer_blocked() {
        let mut state = LimitState {
            active: true,
            reason: "r".to_owned(),
            ..LimitState::default()
        };
        assert!(hook_reply_with(&input("PostToolUse", "s"), &mut state, 10, false).is_none());
        assert!(state.sessions.is_empty());
        assert!(hook_reply_with(&input("PostToolUse", "s"), &mut state, 10, true).is_some());
    }

    #[test]
    fn freed_account_lifts_the_block_only_when_it_can_be_used() {
        let levels = vec![
            ("a".to_owned(), usage(100.0, 10.0)),
            ("b".to_owned(), usage(10.0, 10.0)),
        ];
        assert!(!still_blocked(&config(true), &levels, Some("a")));
        assert!(still_blocked(&config(false), &levels, Some("a")));
        assert!(!still_blocked(&config(false), &levels, Some("b")));
        let full = vec![("a".to_owned(), usage(100.0, 10.0))];
        assert!(still_blocked(&config(true), &full, Some("a")));
    }

    #[test]
    fn reason_names_the_window_that_really_blocks() {
        let weekly_full = |name: &str, five: f64, seven: f64| {
            let mut candidate = usage(five, seven);
            if let Some(candidate) = candidate.as_mut() {
                candidate.name = name.to_owned();
            }
            (name.to_owned(), candidate)
        };
        let reason = limit_reason(&[weekly_full("a", 100.0, 71.0), weekly_full("b", 72.0, 100.0)]);
        assert_eq!(
            reason,
            "no saved Claude account has quota left (a: 5h 100%, 7d 71%; b: 5h 72%, 7d 100%)"
        );
    }

    #[test]
    fn verdict_needs_usable_quota_to_release() {
        let levels = vec![("a".to_owned(), usage(99.0, 10.0)), ("b".to_owned(), None)];
        let partial = verdict(&config(true), &levels, Some("a"));
        assert!(!partial.at_limit && !partial.released);
        let levels = vec![
            ("a".to_owned(), usage(99.0, 10.0)),
            ("b".to_owned(), usage(10.0, 10.0)),
        ];
        assert!(!verdict(&config(true), &levels, Some("a")).released);
        assert!(!verdict(&config(false), &levels, Some("a")).released);
        assert!(!verdict(&config(true), &levels, None).released);
        assert!(verdict(&config(false), &levels, Some("b")).released);
        let weekly_full = vec![("a".to_owned(), usage(0.0, 100.0))];
        assert!(!verdict(&config(true), &weekly_full, Some("a")).released);
        assert!(verdict(&config(true), &weekly_full, Some("a")).at_limit);
        let mut off = config(true);
        off.limit_compact.enabled = false;
        assert!(verdict(&off, &weekly_full, Some("a")).released);
    }

    #[test]
    fn hard_stops_block_release_and_count_as_full() {
        let mut capped = usage(50.0, 10.0);
        if let Some(candidate) = capped.as_mut() {
            candidate.stops = crate::usage::Stops {
                five_hour: Some(40.0),
                seven_day: None,
                hard: true,
            };
        }
        let levels = vec![
            ("a".to_owned(), capped),
            ("b".to_owned(), usage(97.0, 10.0)),
        ];
        let result = verdict(&config(true), &levels, Some("a"));
        assert!(result.at_limit && !result.released);
        let mut low_switch = config(true);
        low_switch.threshold = 20.0;
        let levels = vec![("a".to_owned(), usage(30.0, 10.0))];
        assert!(!verdict(&low_switch, &levels, Some("a")).released);
    }

    fn soft(five_hour: f64, seven_day: f64, weekly_stop: f64) -> Option<Candidate> {
        let mut candidate = usage(five_hour, seven_day);
        if let Some(candidate) = candidate.as_mut() {
            candidate.stops = crate::usage::Stops {
                five_hour: None,
                seven_day: Some(weekly_stop),
                hard: false,
            };
        }
        candidate
    }

    #[test]
    fn soft_stops_are_a_reserve_not_a_barrier() {
        let levels = vec![
            ("a".to_owned(), soft(10.0, 60.0, 50.0)),
            ("b".to_owned(), usage(97.0, 10.0)),
        ];
        let result = verdict(&config(true), &levels, Some("a"));
        assert!(!result.at_limit && result.released);
        assert_eq!(switch_target(&config(true), &levels, Some("a")), None);
        let levels = vec![
            ("a".to_owned(), soft(10.0, 60.0, 50.0)),
            ("b".to_owned(), usage(10.0, 10.0)),
        ];
        assert!(!verdict(&config(true), &levels, Some("a")).released);
        assert_eq!(
            switch_target(&config(true), &levels, Some("a")),
            Some("b".to_owned())
        );
        let levels = vec![
            ("a".to_owned(), usage(96.0, 10.0)),
            ("b".to_owned(), soft(10.0, 60.0, 50.0)),
        ];
        assert_eq!(
            switch_target(&config(true), &levels, Some("a")),
            Some("b".to_owned())
        );
    }

    #[test]
    fn compaction_never_spends_past_a_hard_stop() {
        let mut capped = usage(96.0, 60.0);
        if let Some(candidate) = capped.as_mut() {
            candidate.stops = crate::usage::Stops {
                five_hour: None,
                seven_day: Some(50.0),
                hard: true,
            };
        }
        let levels = vec![
            ("a".to_owned(), capped),
            ("b".to_owned(), usage(96.0, 10.0)),
        ];
        assert!(!may_spend(&config(true), &levels, Some("a")));
        assert!(may_spend(&config(true), &levels, Some("b")));
        assert!(!may_spend(&config(true), &levels, None));
    }

    #[test]
    fn untold_stop_after_another_block_is_still_recorded() {
        let mut state = active();
        let mut stop = input("Stop", "s");
        stop.stop_hook_active = true;
        assert!(hook_reply(&stop, &mut state, 40).is_none());
        assert_eq!(state.sessions["s"].stopped_at_ms, Some(40));
    }

    #[test]
    fn switch_target_only_when_active_is_unusable() {
        let levels = vec![
            ("a".to_owned(), usage(90.0, 10.0)),
            ("b".to_owned(), usage(30.0, 10.0)),
            ("c".to_owned(), usage(5.0, 10.0)),
            ("d".to_owned(), None),
        ];
        assert_eq!(
            switch_target(&config(true), &levels, Some("a")),
            Some("c".to_owned())
        );
        assert_eq!(switch_target(&config(true), &levels, Some("b")), None);
        let full = vec![("a".to_owned(), usage(96.0, 10.0))];
        assert_eq!(switch_target(&config(true), &full, Some("a")), None);
    }

    #[test]
    fn reentry_keeps_only_stopped_sessions_and_forgets_compactions() {
        let mut state = LimitState::default();
        let parked = |stopped_at_ms: Option<u64>, continued_by_user: bool| Parked {
            stopped_at_ms,
            continued_by_user,
            ..Parked::default()
        };
        state
            .sessions
            .insert("stopped".to_owned(), parked(Some(5), false));
        state
            .sessions
            .insert("running".to_owned(), parked(None, false));
        state
            .sessions
            .insert("manual".to_owned(), parked(Some(5), true));
        state.compacted.insert("t".to_owned(), 1);
        state.compacting_sessions.insert("s".to_owned(), 1);
        state.inspected.insert("t".to_owned(), "x".to_owned());
        let dropped = reenter(&mut state);
        assert_eq!(dropped, vec!["manual".to_owned()]);
        assert!(!state.sessions["stopped"].retell);
        assert!(state.sessions["running"].retell);
        let mut active_state = state.clone();
        active_state.active = true;
        assert!(hook_reply(&input("PostToolUse", "running"), &mut active_state, 50).is_some());
        assert!(!active_state.sessions["running"].retell);
        assert_eq!(active_state.sessions["running"].told_at_ms, 50);
        assert!(state.compacted.is_empty() && state.compacting_sessions.is_empty());
        assert!(state.inspected.is_empty());
    }

    #[test]
    fn stopped_session_that_runs_again_is_told_again() {
        let mut state = active();
        hook_reply(&input("PostToolUse", "s"), &mut state, 10);
        hook_reply(&input("Stop", "s"), &mut state, 20);
        let again = hook_reply(&input("PostToolUse", "s"), &mut state, 30);
        assert!(again.is_some());
        assert_eq!(state.sessions["s"].told_at_ms, 30);
        assert_eq!(state.sessions["s"].stopped_at_ms, None);
    }

    #[test]
    fn only_real_prompts_after_the_stop_count_as_continuation() {
        let line = |time: &str, extra: Value| {
            let mut value = json!({ "type": "user", "timestamp": time });
            if let (Some(target), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
                target.extend(extra.clone());
            }
            value.to_string()
        };
        let stop = 1_793_095_500_300;
        let before = line(
            "2026-10-27T10:00:00Z",
            json!({ "message": { "content": "go on" } }),
        );
        let tool = line(
            "2026-10-27T10:20:00Z",
            json!({ "message": { "content": [{ "type": "tool_result", "content": "x" }] } }),
        );
        let meta = line(
            "2026-10-27T10:21:00Z",
            json!({ "isMeta": true, "message": { "content": "caveat" } }),
        );
        let summary = line(
            "2026-10-27T10:22:00Z",
            json!({ "isCompactSummary": true, "message": { "content": "summary" } }),
        );
        let noise = [before.clone(), tool, meta, summary].join("\n");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        let check = |body: &str, ignore: Option<&str>| {
            fs::write(&path, body).expect("write");
            user_prompt_after(&path, stop, ignore)
        };
        assert!(!check(&noise, None));
        let prompt = line(
            "2026-10-27T10:30:00Z",
            json!({ "message": { "content": [{ "type": "text", "text": "next" }] } }),
        );
        let with_prompt = [noise.clone(), prompt.clone()].join("\n");
        assert!(check(&with_prompt, None));
        assert!(!check(&with_prompt, Some(" next\n")));
        assert!(check(&with_prompt, Some("other")));
        let late = line(
            "2026-10-27T10:40:00Z",
            json!({ "message": { "content": [{ "type": "tool_result", "content": "x" }] } }),
        );
        let filler = vec![late; 4_000].join("\n");
        assert!(!check(&[before.clone(), filler.clone()].join("\n"), None));
        assert!(check(
            &[before.clone(), prompt.clone(), filler.clone()].join("\n"),
            None
        ));
        assert!(!check(&before, None));
        let same_second = |time: &str| line(time, json!({ "message": { "content": "typed" } }));
        assert!(check(&same_second("2026-10-27T10:05:00.400Z"), None));
        assert!(!check(&same_second("2026-10-27T10:05:00.200Z"), None));
        let huge = line(
            "2026-10-27T10:50:00Z",
            json!({ "message": { "content": [{ "type": "tool_result", "content": "x".repeat(300_000) }] } }),
        );
        assert!(check(
            &[before.clone(), prompt.clone(), huge.clone()].join("\n"),
            None
        ));
        assert!(!check(&[before, huge].join("\n"), None));
    }

    #[test]
    fn only_foreign_messages_after_the_instruction_count() {
        let at = |text: &str| {
            DateTime::parse_from_rfc3339(text)
                .ok()
                .map(|time| time.with_timezone(&Utc))
        };
        let detail = |foreign: Option<&str>| t3::ThreadDetail {
            native_session_ids: Vec::new(),
            used_tokens: None,
            last_item_is_compaction: false,
            last_compaction_at: None,
            last_foreign_message_at: foreign.and_then(at),
            message_ids: Vec::new(),
        };
        let told = 1_793_095_500_300;
        assert!(!foreign_message_after(&detail(None), told));
        assert!(!foreign_message_after(
            &detail(Some("2026-10-27T10:00:00Z")),
            told
        ));
        assert!(foreign_message_after(
            &detail(Some("2026-10-27T10:05:01Z")),
            told
        ));
        assert!(!foreign_message_after(
            &detail(Some("2026-10-27T10:05:00.200Z")),
            told
        ));
        assert!(foreign_message_after(
            &detail(Some("2026-10-27T10:05:00.400Z")),
            told
        ));
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
        let told = 1_793_095_500_000;
        assert_eq!(
            assistant_text_since(&tail, told).and_then(|text| find_todo_path(&text)),
            None
        );
        assert!(assistant_text_since(&tail, told - 1_000_000).is_some());
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
