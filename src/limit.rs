use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
use crate::forecast;
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
const RESET_NEAR_SECONDS: i64 = 60 * 60;
const COMPACTING_MARKER_SECONDS: u64 = 30 * 60;
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
    pub told_actors: BTreeSet<String>,
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
    model: Option<String>,
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
         had quota left. {pointer} and continue the task from there. Compacted sub-agent \
         threads may be resumed: their context is only the short summary, so that send is a \
         few uncached tokens and the cold-resume gate allows it. Do not resume a sub-agent \
         thread that was not compacted. If nothing is left to do, say so in one line and stop."
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

#[derive(Debug, Clone, Default)]
struct ClaudeModels {
    ids: HashSet<String>,
    aliases: HashSet<String>,
    injected: HashSet<String>,
    loaded: bool,
    exclusions_loaded: bool,
}

#[derive(Debug, Default, Deserialize)]
struct CatalogFile {
    #[serde(default)]
    catalog: Option<CatalogBody>,
}

#[derive(Debug, Default, Deserialize)]
struct CatalogBody {
    #[serde(default)]
    config: Option<CatalogConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct CatalogConfig {
    #[serde(default)]
    models: Vec<Value>,
    #[serde(default)]
    provider_alias_targets: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
struct InjectedModels {
    #[serde(default)]
    available_models: Vec<Value>,
    #[serde(default)]
    model: Value,
    #[serde(default)]
    small_model: Value,
}

fn normalized_model(model: &str) -> String {
    let model = model.trim().to_ascii_lowercase();
    model
        .strip_suffix("[1m]")
        .unwrap_or(&model)
        .trim()
        .to_owned()
}

fn model_family(model: &str) -> &str {
    model
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or("")
}

fn remember_name(names: &mut HashSet<String>, name: &str) {
    let name = normalized_model(name);
    if !name.is_empty() {
        names.insert(name);
    }
}

fn remember_value(names: &mut HashSet<String>, value: &Value) {
    if let Some(name) = value.as_str() {
        remember_name(names, name);
    }
}

fn claude_model_sources(home: &Path) -> (PathBuf, PathBuf) {
    let catalog = home.join(".claude").join("cache").join("model-catalog");
    let injected = home
        .join(".config")
        .join(["claude", "gpt6"].join("-"))
        .join("config.json");
    (catalog, injected)
}

fn load_claude_models() -> ClaudeModels {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return ClaudeModels::default();
    };
    let (catalog, injected) = claude_model_sources(&home);
    load_claude_models_from(&catalog, &injected)
}

fn load_claude_models_from(catalog_dir: &Path, injected_path: &Path) -> ClaudeModels {
    let mut models = ClaudeModels::default();
    if let Ok(bytes) = fs::read(injected_path)
        && let Ok(config) = serde_json::from_slice::<InjectedModels>(&bytes)
    {
        models.exclusions_loaded = true;
        for name in &config.available_models {
            remember_value(&mut models.injected, name);
        }
        remember_value(&mut models.injected, &config.model);
        remember_value(&mut models.injected, &config.small_model);
    }
    if let Ok(entries) = fs::read_dir(catalog_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_json = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
            if !is_json {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            if absorb_catalog(&mut models, &bytes) {
                models.loaded = true;
            }
        }
    }
    models
}

fn absorb_catalog(models: &mut ClaudeModels, bytes: &[u8]) -> bool {
    let Ok(file) = serde_json::from_slice::<CatalogFile>(bytes) else {
        return false;
    };
    let Some(config) = file.catalog.and_then(|body| body.config) else {
        return false;
    };
    let mut found = false;
    for model in &config.models {
        if let Some(id) = model.get("id").and_then(Value::as_str) {
            let before = models.ids.len();
            remember_name(&mut models.ids, id);
            found |= models.ids.len() > before;
        }
        if let Some(family) = model.pointer("/runtime/family").and_then(Value::as_str) {
            let before = models.aliases.len();
            remember_name(&mut models.aliases, family);
            found |= models.aliases.len() > before;
        }
    }
    for alias in config.provider_alias_targets.keys() {
        let before = models.aliases.len();
        remember_name(&mut models.aliases, alias);
        found |= models.aliases.len() > before;
    }
    found
}

fn kept_claude_name(model: &str) -> bool {
    model.starts_with("claude") || matches!(model_family(model), "haiku" | "sonnet" | "opus")
}

fn is_claude_model(model: &str, catalog: &ClaudeModels) -> bool {
    let model = normalized_model(model);
    if model.is_empty() || catalog.injected.contains(&model) {
        return false;
    }
    if kept_claude_name(&model) {
        return true;
    }
    catalog.loaded
        && catalog.exclusions_loaded
        && (catalog.ids.contains(&model) || catalog.aliases.contains(model_family(&model)))
}

fn model_flag(args: &[String]) -> Option<String> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--model" || arg == "-m" {
            return args.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--model=") {
            return Some(value.to_owned());
        }
    }
    None
}

fn is_claude_cli(exe: &str) -> bool {
    exe == "claude"
        || exe
            .strip_prefix("claude")
            .is_some_and(|rest| rest == "-code")
}

fn model_verdict(args: &[String], catalog: &ClaudeModels) -> Option<bool> {
    let exe = args
        .first()
        .map(|path| path.rsplit('/').next().unwrap_or(path))
        .unwrap_or("");
    if let Some(model) = model_flag(args) {
        return Some(is_claude_model(&model, catalog));
    }
    if exe.starts_with("grok") {
        return Some(false);
    }
    if is_claude_cli(exe) {
        return Some(true);
    }
    None
}

fn process_args(pid: u32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    if args.is_empty() { None } else { Some(args) }
}

fn parent_pid(pid: u32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("PPid:") else {
            continue;
        };
        return rest.trim().parse().ok().filter(|parent| *parent > 1);
    }
    None
}

fn ancestry_uses_claude_model(catalog: &ClaudeModels) -> bool {
    let mut pid = std::process::id();
    for _ in 0..12 {
        let Some(parent) = parent_pid(pid) else {
            return false;
        };
        if let Some(args) = process_args(parent)
            && let Some(verdict) = model_verdict(&args, catalog)
        {
            return verdict;
        }
        pid = parent;
    }
    false
}

fn session_uses_claude_model(input: &HookInput, catalog: &ClaudeModels) -> bool {
    if let Some(model) = input
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
    {
        return is_claude_model(model, catalog);
    }
    ancestry_uses_claude_model(catalog)
}

fn hook_should_forget(input: &HookInput, catalog: &ClaudeModels) -> bool {
    input.agent_id.is_none()
        && !input.session_id.is_empty()
        && !session_uses_claude_model(input, catalog)
}

fn quota_hook_owns(input: &HookInput, catalog: &ClaudeModels) -> bool {
    !input.session_id.is_empty() && session_uses_claude_model(input, catalog)
}

fn actor_key(input: &HookInput) -> &str {
    input.agent_id.as_deref().unwrap_or("")
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
    catalog: &ClaudeModels,
) -> Option<Value> {
    if input.session_id.is_empty() {
        return None;
    }
    if input.agent_id.is_some()
        && input
            .model
            .as_ref()
            .is_some_and(|model| !is_claude_model(model, catalog))
    {
        return None;
    }
    let parent = input.agent_id.is_none();
    if parent
        && input.hook_event_name == "Stop"
        && state
            .compacting_sessions
            .remove(&input.session_id)
            .is_some()
    {
        return None;
    }
    if !state.active || !still_blocked {
        if parent
            && input.hook_event_name == "Stop"
            && let Some(parked) = state.sessions.get_mut(&input.session_id)
        {
            write_parent_baseline(parked, input, now, parked.told_at_ms == 0);
            parked.stopped_at_ms.get_or_insert(now);
        }
        return None;
    }
    if !state.scope.is_empty() && !state.scope.contains(&input.session_id) {
        return None;
    }
    let message = wrap_up_message(&state.reason);
    let actor = actor_key(input).to_owned();
    match input.hook_event_name.as_str() {
        "PostToolUse" => {
            let parked = state
                .sessions
                .entry(input.session_id.clone())
                .or_insert_with(|| {
                    if parent {
                        parked_from(input, now)
                    } else {
                        Parked {
                            told_at_ms: now,
                            ..Parked::default()
                        }
                    }
                });
            if parent && (parked.stopped_at_ms.is_some() || parked.retell) {
                parked.stopped_at_ms = None;
                parked.continued_by_user = false;
                parked.retell = false;
                parked.resume_message_id = None;
                parked.told_actors.clear();
                write_parent_baseline(parked, input, now, true);
            } else if parent {
                write_parent_baseline(parked, input, now, parked.told_at_ms == 0);
            }
            if !parked.told_actors.insert(actor) {
                return None;
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
                let first = parked.told_actors.insert(actor);
                if parent {
                    write_parent_baseline(parked, input, now, parked.told_at_ms == 0);
                    if !first {
                        parked.stopped_at_ms.get_or_insert(now);
                    }
                }
                first.then(|| json!({ "decision": "block", "reason": message }))
            }
            None if parent && input.stop_hook_active => {
                let mut parked = parked_from(input, now);
                parked.stopped_at_ms = Some(now);
                parked.told_actors.insert(actor);
                state.sessions.insert(input.session_id.clone(), parked);
                None
            }
            None => {
                let mut parked = if parent {
                    parked_from(input, now)
                } else {
                    Parked {
                        told_at_ms: now,
                        ..Parked::default()
                    }
                };
                parked.told_actors.insert(actor);
                state.sessions.insert(input.session_id.clone(), parked);
                Some(json!({ "decision": "block", "reason": message }))
            }
        },
        _ => None,
    }
}

fn parked_from(input: &HookInput, now: u64) -> Parked {
    let mut parked = Parked {
        told_at_ms: now,
        ..Parked::default()
    };
    write_parent_baseline(&mut parked, input, now, true);
    parked
}

fn write_parent_baseline(parked: &mut Parked, input: &HookInput, now: u64, reset_clock: bool) {
    if input.cwd.is_some() {
        parked.cwd.clone_from(&input.cwd);
    }
    if input.transcript_path.is_some() {
        parked.transcript.clone_from(&input.transcript_path);
    }
    if input.entrypoint.is_some() {
        parked.entrypoint.clone_from(&input.entrypoint);
    }
    if reset_clock {
        parked.told_at_ms = now;
    }
}

fn reset_is_soon(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    resets_at.is_some_and(|resets_at| {
        let seconds = resets_at.signed_duration_since(now).num_seconds();
        seconds > 0 && seconds <= RESET_NEAR_SECONDS
    })
}

fn window_blocks(utilization: f64, limits: &[f64]) -> bool {
    limits.iter().any(|limit| utilization >= *limit)
}

fn account_frees_within_an_hour(
    candidate: &Candidate,
    config: &Config,
    now: DateTime<Utc>,
) -> bool {
    if !blocked(candidate, config) {
        return false;
    }
    let five_hard = candidate
        .stops
        .hard
        .then_some(candidate.stops.five_hour.unwrap_or(config.threshold));
    let seven_hard = candidate
        .stops
        .hard
        .then_some(candidate.stops.seven_day.unwrap_or(config.threshold));
    let five_blocks = window_blocks(
        candidate.usage.five_hour.utilization,
        &[
            config.limit_compact.threshold,
            config.threshold,
            five_hard.unwrap_or(f64::MAX),
        ],
    );
    let seven_blocks = window_blocks(
        candidate.usage.seven_day.utilization,
        &[config.threshold, seven_hard.unwrap_or(f64::MAX)],
    );
    (five_blocks || seven_blocks)
        && (!five_blocks || reset_is_soon(candidate.usage.five_hour.resets_at, now))
        && (!seven_blocks || reset_is_soon(candidate.usage.seven_day.resets_at, now))
}

fn any_account_frees_within_an_hour(config: &Config, levels: &[Level], now: DateTime<Utc>) -> bool {
    levels
        .iter()
        .filter_map(|(_, candidate)| candidate.as_ref())
        .any(|candidate| account_frees_within_an_hour(candidate, config, now))
}

fn cheap_to_resume(detail: &t3::ThreadDetail, min_context_tokens: u64) -> bool {
    detail.last_item_is_compaction
        || detail
            .used_tokens
            .is_some_and(|tokens| tokens < min_context_tokens)
}

enum ResumeWhenReady {
    Send,
    KeepUntilCompact,
}

fn resume_when_ready(detail: &t3::ThreadDetail, min_context_tokens: u64) -> ResumeWhenReady {
    if cheap_to_resume(detail, min_context_tokens) {
        ResumeWhenReady::Send
    } else {
        ResumeWhenReady::KeepUntilCompact
    }
}

enum ResumeDelivery {
    Send,
    Wait,
}

fn resume_delivery(ready: ResumeWhenReady, grace_elapsed: bool) -> ResumeDelivery {
    match (ready, grace_elapsed) {
        (ResumeWhenReady::Send, _) => ResumeDelivery::Send,
        (ResumeWhenReady::KeepUntilCompact, false) => ResumeDelivery::Wait,
        (ResumeWhenReady::KeepUntilCompact, true) => ResumeDelivery::Send,
    }
}

fn warm_candidate(thread: &ThreadSummary, limit: &LimitCompact, now: DateTime<Utc>) -> bool {
    let Some(completed) = thread.last_run_completed_at else {
        return false;
    };
    let idle = now.signed_duration_since(completed).num_seconds();
    !thread.busy
        && !thread.settled
        && idle >= minutes(limit.idle_minutes)
        && idle < minutes(limit.cache_ttl_minutes)
        && thread
            .last_user_message_at
            .is_none_or(|message| message <= completed)
}

fn minutes(value: u64) -> i64 {
    i64::try_from(value.saturating_mul(60)).unwrap_or(i64::MAX)
}

pub(crate) fn worth_compacting(detail: &t3::ThreadDetail, limit: &LimitCompact) -> bool {
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
        at_limit: limit.enabled
            && (limit.force
                || (all_at_limit && !any_account_frees_within_an_hour(config, levels, Utc::now()))),
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

fn nearing_by_percent(config: &Config, levels: &[Level]) -> bool {
    let limit = &config.limit_compact;
    levels.iter().all(|(_, candidate)| {
        candidate.as_ref().is_some_and(|candidate| {
            candidate.usage.five_hour.utilization >= limit.compact_from.min(limit.threshold)
                || blocked(candidate, config)
        })
    }) && !any_account_frees_within_an_hour(config, levels, Utc::now())
}

fn headroom(candidate: &Candidate, config: &Config) -> forecast::Headroom {
    let limit = config.limit_compact.threshold;
    let weekly_open = candidate.usage.seven_day.utilization < config.threshold
        && !(candidate.stops.hard && !candidate.within_stops(config.threshold));
    let left = match blocked(candidate, config) {
        true => 0.0,
        false => (limit - candidate.usage.five_hour.utilization).max(0.0),
    };
    forecast::Headroom {
        left,
        refill: if weekly_open { limit - left } else { 0.0 },
        resets_at: candidate.usage.five_hour.resets_at,
    }
}

fn minutes_to_pause(
    config: &Config,
    levels: &[Level],
    burn: Option<f64>,
    now: DateTime<Utc>,
) -> Option<f64> {
    let accounts = levels
        .iter()
        .map(|(_, candidate)| {
            candidate
                .as_ref()
                .map(|candidate| headroom(candidate, config))
        })
        .collect::<Option<Vec<_>>>()?;
    forecast::minutes_until_exhausted(&accounts, burn?, now)
}

fn nearing_limit(config: &Config, levels: &[Level], burn: Option<f64>) -> bool {
    let limit = &config.limit_compact;
    limit.enabled
        && !levels.is_empty()
        && (nearing_by_percent(config, levels)
            || minutes_to_pause(config, levels, burn, Utc::now())
                .is_some_and(|minutes| minutes <= limit.compact_lead_minutes as f64))
}

fn describe_forecast(config: &Config, levels: &[Level], burn: Option<f64>) -> String {
    let window = forecast::BURN_WINDOW_SECONDS / 60;
    match (burn, minutes_to_pause(config, levels, burn, Utc::now())) {
        (Some(burn), Some(minutes)) => format!(
            "Verbrauch {burn:.2}%/min, Limit-Pause in etwa {:.0} min",
            minutes
        ),
        (Some(burn), None) => format!(
            "Verbrauch {burn:.2}%/min, Zeitpunkt offen: nicht fuer jeden Account liegt eine \
             Auslastung vor"
        ),
        (None, _) => format!("kein messbarer Verbrauch in den letzten {window} min"),
    }
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

fn still_blocked(
    config: &Config,
    levels: &[Level],
    active_profile: impl FnOnce() -> Option<String>,
) -> bool {
    let limit = &config.limit_compact;
    if !limit.enabled {
        return false;
    }
    if limit.force {
        return true;
    }
    if config.auto_switch {
        return !levels
            .iter()
            .filter_map(|(_, candidate)| candidate.as_ref())
            .any(|candidate| fit(candidate, config) != Fit::No);
    }
    !verdict(config, levels, active_profile().as_deref()).released
}

fn reenter(state: &mut LimitState, now: u64) -> Vec<String> {
    state.compacted.clear();
    state.inspected.clear();
    state
        .compacting_sessions
        .retain(|_, at| now.saturating_sub(*at) < COMPACTING_MARKER_SECONDS);
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

    pub(crate) fn open_lock(&self, name: &str) -> Result<File> {
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
        let catalog = load_claude_models();
        if hook_should_forget(&input, &catalog) {
            if state.sessions.contains_key(&input.session_id)
                || state.compacting_sessions.contains_key(&input.session_id)
            {
                self.update_limit_state(|state| {
                    state.sessions.remove(&input.session_id);
                    state.compacting_sessions.remove(&input.session_id);
                })?;
            }
            return Ok(None);
        }
        if !quota_hook_owns(&input, &catalog) {
            return Ok(None);
        }
        let now = unix_millis()?;
        let relevant =
            state.active && (state.scope.is_empty() || state.scope.contains(&input.session_id));
        let blocked = !relevant || self.blocked_now().unwrap_or(true);
        let reply = self
            .update_limit_state(|state| hook_reply_with(&input, state, now, blocked, &catalog))?;
        Ok(reply.map(|value| value.to_string()))
    }

    fn blocked_now(&self) -> Result<bool> {
        self.check_config_file()?;
        let config = self.load_config();
        let levels = self.cached_levels()?;
        Ok(still_blocked(&config, &levels, || {
            self.active_profile_name_memoized(None)
        }))
    }

    pub(crate) fn t3_client(&self, limit: &LimitCompact) -> t3::Client {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        t3::Client::new(
            &limit.t3_url,
            self.paths.store.join(TOKEN_FILE),
            home.join(".local/bin/t3"),
        )
    }

    fn pool_burn(&self, levels: &[Level], now: u64) -> Option<f64> {
        let histories: Vec<Vec<forecast::Sample>> = levels
            .iter()
            .map(|(name, _)| self.usage_history(name))
            .collect();
        forecast::burn_per_minute(histories.iter().map(Vec::as_slice), now)
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
        if limit.enabled && !limit.force && before.active {
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
        let burn = self.pool_burn(&levels, now);
        let nearing = nearing_limit(config, &levels, burn);
        if nearing {
            if !spoken.contains_key("limit-forecast") {
                log_event(&format!(
                    "Limit-Pause naht ({}); Kompaktieren beginnt",
                    describe_forecast(config, &levels, burn)
                ));
                spoken.insert("limit-forecast".to_owned(), String::new());
            }
        } else {
            spoken.remove("limit-forecast");
        }
        if nearing && may_spend(config, &levels, active_profile.as_deref()) {
            let result = self.compact_pass(limit, &state);
            report(
                spoken,
                "limit-compact",
                "Kompaktieren fehlgeschlagen",
                result,
            );
        }
        if state.sessions.is_empty() {
            if !nearing && state != LimitState::default() {
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
                let dropped = reenter(state, now);
                state.active = true;
                state.since = Some(now);
                state.released_at = None;
                state.reason = reason.clone();
                state.scope = limit.scope_sessions.clone();
                (Some(format!("Limit-Pause beginnt: {reason}")), dropped)
            } else if verdict.released && state.active {
                state.active = false;
                state.released_at = Some(now);
                state.compacted.clear();
                state.inspected.clear();
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
                "Limit-Kompaktieren: Thread {} kompaktiert ({} Tokens im Kontext)",
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

    fn request_compact_if_needed(
        &self,
        rpc: &mut t3::Rpc,
        thread_id: &str,
        detail: &t3::ThreadDetail,
    ) -> Result<()> {
        let limit = self.load_config().limit_compact;
        let state = self.load_limit_state()?;
        if state.compacted.contains_key(thread_id)
            || state.compacted.len() >= limit.max_compactions
            || !worth_compacting(detail, &limit)
        {
            return Ok(());
        }
        let at = unix_timestamp()?;
        let message_id = uuid::Uuid::new_v4().to_string();
        self.update_limit_state(|state| {
            for session in &detail.native_session_ids {
                state.compacting_sessions.insert(session.clone(), at);
            }
            state.compacted.insert(thread_id.to_owned(), at);
        })?;
        if let Err(error) = rpc.send_message(thread_id, COMPACT_COMMAND, &message_id) {
            if t3::is_rejection(&error) {
                self.update_limit_state(|state| {
                    for session in &detail.native_session_ids {
                        state.compacting_sessions.remove(session);
                    }
                    state.compacted.remove(thread_id);
                })?;
            }
            return Err(error);
        }
        log_event(&format!(
            "Limit-Pause: Thread {thread_id} wird nach der Freigabe kompaktiert ({} Tokens)",
            detail.used_tokens.unwrap_or(0)
        ));
        Ok(())
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
        let min_context_tokens = self.load_config().limit_compact.min_context_tokens;
        if matches!(
            resume_delivery(
                resume_when_ready(&detail, min_context_tokens),
                grace_over(state, now)
            ),
            ResumeDelivery::Wait
        ) {
            self.request_compact_if_needed(rpc, thread_id, &detail)?;
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
        if !cwd.is_dir() {
            return Ok(Outcome::Done(format!(
                "nicht fortgesetzt: Arbeitsverzeichnis gibt es nicht mehr: {}",
                cwd.display()
            )));
        }
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
            "Schwelle {:.0}%, fortsetzen unter {:.0}%, kompaktieren ab {:.0}% oder {} min vor \
             der Pause und ab {} Tokens, Cache-Fenster {} min, Ruhezeit {} min, hoechstens {} Threads",
            limit.threshold,
            limit.resume_below,
            limit.compact_from.min(limit.threshold),
            limit.compact_lead_minutes,
            limit.min_context_tokens,
            limit.cache_ttl_minutes,
            limit.idle_minutes,
            limit.max_compactions
        );
        if let Ok(levels) = self.cached_levels() {
            let burn = self.pool_burn(&levels, unix_timestamp()?);
            println!("Prognose: {}", describe_forecast(&config, &levels, burn));
        }
        let state = self.load_limit_state()?;
        match (state.active, state.sessions.is_empty()) {
            (true, _) => println!("Zustand: Pause aktiv ({})", state.reason),
            (false, false) => println!("Zustand: Pause aufgehoben, Fortsetzen laeuft"),
            (false, true) => println!("Zustand: keine Pause aktiv"),
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
        hook_reply_with(input, state, now, true, &ClaudeModels::default())
    }

    fn input(event: &str, session: &str) -> HookInput {
        HookInput {
            hook_event_name: event.to_owned(),
            session_id: session.to_owned(),
            cwd: Some(PathBuf::from("/repo")),
            ..HookInput::default()
        }
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn listed_catalog() -> ClaudeModels {
        let mut catalog = ClaudeModels {
            loaded: true,
            exclusions_loaded: true,
            ..ClaudeModels::default()
        };
        for id in [
            "claude-opus-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "gpt-6.1-sol",
        ] {
            catalog.ids.insert(id.to_owned());
        }
        for alias in ["fable", "haiku", "sonnet", "opus"] {
            catalog.aliases.insert(alias.to_owned());
        }
        for name in ["gpt-6.1-sol", "gpt-6-astra", "gpt-6-luna"] {
            catalog.injected.insert(name.to_owned());
        }
        catalog
    }

    #[test]
    fn only_a_claude_model_name_counts() {
        let catalog = listed_catalog();
        assert!(is_claude_model("claude-opus-5-5", &catalog));
        assert!(is_claude_model(" Claude-Haiku-5-5 ", &catalog));
        assert!(is_claude_model("haiku", &catalog));
        assert!(is_claude_model("Sonnet", &catalog));
        assert!(is_claude_model("opus", &catalog));
        assert!(is_claude_model("sonnet-4.6", &catalog));
        assert!(is_claude_model("fable", &catalog));
        assert!(is_claude_model("Fable[1m]", &catalog));
        assert!(is_claude_model("claude-fable-5-1", &catalog));
        assert!(!is_claude_model("grok-4.7", &catalog));
        assert!(!is_claude_model("gpt-6.1-sol", &catalog));
        assert!(!is_claude_model("glm-5.3", &catalog));
        assert!(!is_claude_model("", &catalog));
        assert!(!is_claude_model("sonnetextra", &catalog));
    }

    #[test]
    fn missing_catalog_keeps_the_previous_claude_names() {
        let catalog = ClaudeModels::default();
        assert!(is_claude_model("claude-fable-5-1", &catalog));
        assert!(is_claude_model(" Claude-Haiku-5-5[1m] ", &catalog));
        assert!(is_claude_model("haiku", &catalog));
        assert!(is_claude_model("sonnet-4.6", &catalog));
        assert!(is_claude_model("opus", &catalog));
        assert!(!is_claude_model("fable", &catalog));
        assert!(!is_claude_model("gpt-6.1-sol", &catalog));
        assert!(!is_claude_model("sonnetextra", &catalog));
    }

    #[test]
    fn partial_catalog_adds_fable_without_dropping_known_names() {
        let mut catalog = ClaudeModels {
            loaded: true,
            exclusions_loaded: true,
            ..ClaudeModels::default()
        };
        catalog.ids.insert("claude-fable-5-1".to_owned());
        catalog.aliases.insert("fable".to_owned());
        assert!(is_claude_model("fable", &catalog));
        assert!(is_claude_model("claude-opus-5-5", &catalog));
        assert!(is_claude_model("sonnet-4.6", &catalog));
        assert!(is_claude_model("haiku", &catalog));
        assert!(!is_claude_model("gpt-6.1-sol", &catalog));
        catalog.exclusions_loaded = false;
        catalog.ids.insert("gpt-6.1-sol".to_owned());
        assert!(!is_claude_model("fable", &catalog));
        assert!(!is_claude_model("gpt-6.1-sol", &catalog));
        assert!(is_claude_model("claude-opus-5-5", &catalog));
        assert!(is_claude_model("opus", &catalog));
    }

    #[test]
    fn injected_name_is_not_claude_even_when_listed() {
        let mut catalog = listed_catalog();
        let routed = ["claude", "routed"].join("-");
        catalog.ids.insert(routed.clone());
        catalog.injected.insert(routed.clone());
        assert!(catalog.loaded);
        assert!(!is_claude_model(&routed, &catalog));
        assert!(!is_claude_model("gpt-6.1-sol", &catalog));
        catalog.loaded = false;
        assert!(!is_claude_model(&routed, &catalog));
        assert!(is_claude_model("claude-fable-5-1", &catalog));
    }

    #[test]
    fn catalog_files_define_claude_and_injected_names() {
        let dir = std::env::temp_dir().join(format!("cas-hook-catalog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp catalog");
        fs::write(dir.join("bad.json"), b"{").expect("bad catalog");
        fs::write(dir.join("note.txt"), b"ignore").expect("note");
        let routed = ["claude", "routed"].join("-");
        fs::write(
            dir.join("models.json"),
            format!(
                r#"{{"catalog":{{"config":{{"models":[{{"id":"claude-fable-5-1","runtime":{{"family":"fable"}}}},{{"id":"gpt-6.1-sol"}},{{"id":"  "}},{{"id":"{routed}"}}],"provider_alias_targets":{{"Fable":{{}},"haiku":{{}}}}}}}}}}"#
            ),
        )
        .expect("models");
        fs::write(
            dir.join("more.json"),
            r#"{"catalog":{"config":{"models":[],"provider_alias_targets":{"opus":{}}}}}"#,
        )
        .expect("more");
        let injected = dir.join("injected.json");
        fs::write(
            &injected,
            format!(
                r#"{{"available_models":["gpt-6.1-sol","gpt-6.1-sol-fast","{routed}"],"model":"gpt-6-astra","small_model":"gpt-6-luna","port":1}}"#
            ),
        )
        .expect("injected");
        let catalog = load_claude_models_from(&dir, &injected);
        assert!(catalog.loaded);
        assert!(catalog.exclusions_loaded);
        assert!(is_claude_model("fable", &catalog));
        assert!(is_claude_model("claude-fable-5-1", &catalog));
        assert!(is_claude_model("haiku", &catalog));
        assert!(is_claude_model("opus", &catalog));
        assert!(!is_claude_model("gpt-6.1-sol", &catalog));
        assert!(!is_claude_model("gpt-6-astra", &catalog));
        assert!(!is_claude_model("gpt-6-luna", &catalog));
        assert!(is_claude_model("sonnet", &catalog));
        assert!(is_claude_model("sonnet-4.6", &catalog));
        assert!(is_claude_model("claude-opus-5-5", &catalog));
        assert!(!is_claude_model(&routed, &catalog));
        let unseen = load_claude_models_from(&dir, &dir.join("absent.json"));
        assert!(unseen.loaded);
        assert!(!unseen.exclusions_loaded);
        assert!(!is_claude_model("gpt-6.1-sol", &unseen));
        assert!(!is_claude_model("fable", &unseen));
        assert!(is_claude_model("claude-fable-5-1", &unseen));
        assert!(is_claude_model("haiku", &unseen));
        assert!(is_claude_model("sonnet-4.6", &unseen));
        fs::write(dir.join("broken.json"), b"{").expect("broken exclusions");
        let broken = load_claude_models_from(&dir, &dir.join("broken.json"));
        assert!(broken.loaded);
        assert!(!broken.exclusions_loaded);
        assert!(!is_claude_model("gpt-6.1-sol", &broken));
        assert!(!is_claude_model("fable", &broken));
        assert!(is_claude_model("opus", &broken));
        fs::write(dir.join("empty.json"), b"{}").expect("empty exclusions");
        let empty = load_claude_models_from(&dir, &dir.join("empty.json"));
        assert!(empty.loaded);
        assert!(empty.exclusions_loaded);
        assert!(is_claude_model("fable", &empty));
        assert!(is_claude_model("haiku", &empty));
        assert!(is_claude_model("claude-opus-5-5", &empty));
        let fallback = load_claude_models_from(&dir.join("absent"), &injected);
        assert!(!fallback.loaded);
        assert!(fallback.exclusions_loaded);
        assert!(is_claude_model("claude-fable-5-1", &fallback));
        assert!(is_claude_model("haiku", &fallback));
        assert!(is_claude_model("sonnet-4.6", &fallback));
        assert!(!is_claude_model("fable", &fallback));
        assert!(!is_claude_model("gpt-6.1-sol", &fallback));
        assert!(!is_claude_model(&routed, &fallback));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn subagent_hooks_do_not_forget_the_parent_session() {
        let catalog = listed_catalog();
        let mut parent = input("PostToolUse", "parent");
        parent.model = Some("grok-4.7".to_owned());
        assert!(hook_should_forget(&parent, &catalog));
        parent.model = Some("haiku".to_owned());
        assert!(!hook_should_forget(&parent, &catalog));
        parent.model = Some("fable".to_owned());
        assert!(!hook_should_forget(&parent, &catalog));
        parent.model = Some("sonnet-4.6".to_owned());
        assert!(!hook_should_forget(&parent, &ClaudeModels::default()));
        parent.model = Some("haiku".to_owned());
        assert!(!hook_should_forget(&parent, &ClaudeModels::default()));
        let mut subagent = input("PostToolUse", "parent");
        subagent.model = Some("grok-4.7".to_owned());
        subagent.agent_id = Some("agent-1".to_owned());
        assert!(!hook_should_forget(&subagent, &catalog));
        assert!(!quota_hook_owns(&subagent, &catalog));
        subagent.hook_event_name = "Stop".to_owned();
        assert!(!quota_hook_owns(&subagent, &catalog));
        subagent.hook_event_name = "PostToolUse".to_owned();
        subagent.model = Some("haiku".to_owned());
        assert!(quota_hook_owns(&subagent, &catalog));
        subagent.model = Some("fable".to_owned());
        assert!(quota_hook_owns(&subagent, &catalog));
        assert!(!quota_hook_owns(&subagent, &ClaudeModels::default()));
        let mut nameless = input("PostToolUse", "");
        nameless.model = Some("grok-4.7".to_owned());
        assert!(!hook_should_forget(&nameless, &catalog));
    }

    #[test]
    fn process_cmdline_decides_claude_model() {
        let catalog = listed_catalog();
        assert_eq!(
            model_verdict(&args(&["/usr/bin/grok", "agent"]), &catalog),
            Some(false)
        );
        assert_eq!(
            model_verdict(
                &args(&[
                    "/home/nathanael/.local/bin/claude",
                    "--model",
                    "claude-haiku-5-5"
                ]),
                &catalog
            ),
            Some(true)
        );
        assert_eq!(
            model_verdict(&args(&["claude", "--model=gpt-6.1-sol"]), &catalog),
            Some(false)
        );
        assert_eq!(
            model_verdict(&args(&["claude", "--model", "haiku"]), &catalog),
            Some(true)
        );
        assert_eq!(
            model_verdict(&args(&["claude", "--model", "fable"]), &catalog),
            Some(true)
        );
        assert_eq!(
            model_verdict(
                &args(&["claude", "--model", "fable"]),
                &ClaudeModels::default()
            ),
            Some(false)
        );
        assert_eq!(
            model_verdict(
                &args(&["claude", "--model", "sonnet-4.6"]),
                &ClaudeModels::default()
            ),
            Some(true)
        );
        assert_eq!(model_verdict(&args(&["claude"]), &catalog), Some(true));
        assert_eq!(
            model_verdict(&args(&["/usr/bin/claude-code"]), &catalog),
            Some(true)
        );
        assert_eq!(
            model_verdict(&args(&["claude-account", "hook"]), &catalog),
            None
        );
        assert_eq!(model_verdict(&args(&["grok-4"]), &catalog), Some(false));
        assert_eq!(
            model_verdict(&args(&["/usr/bin/t3", "serve"]), &catalog),
            None
        );
    }

    #[test]
    fn explicit_hook_model_overrides_process_ancestry() {
        let mut hook_input = input("PostToolUse", "s");
        hook_input.model = Some("grok-4.7".to_owned());
        assert!(!session_uses_claude_model(&hook_input, &listed_catalog()));
        hook_input.model = Some("claude-fable-5-1".to_owned());
        assert!(session_uses_claude_model(
            &hook_input,
            &ClaudeModels::default()
        ));
        hook_input.model = Some("fable".to_owned());
        assert!(session_uses_claude_model(&hook_input, &listed_catalog()));
        assert!(!session_uses_claude_model(
            &hook_input,
            &ClaudeModels::default()
        ));
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
    fn compaction_turns_are_left_alone_and_subagents_stop_with_the_parent() {
        let mut state = active();
        state.compacting_sessions.insert("s".to_owned(), 1);
        let mut child_stop = input("Stop", "s");
        child_stop.agent_id = Some("a".to_owned());
        child_stop.model = Some("haiku".to_owned());
        assert!(hook_reply(&child_stop, &mut state, 10).is_some());
        assert!(state.compacting_sessions.contains_key("s"));
        assert!(state.sessions["s"].stopped_at_ms.is_none());
        assert!(hook_reply(&input("Stop", "s"), &mut state, 11).is_none());
        assert!(state.compacting_sessions.is_empty());

        let mut state = active();
        let mut first_child = input("PostToolUse", "t");
        first_child.agent_id = Some("a".to_owned());
        first_child.model = Some("opus".to_owned());
        assert!(hook_reply(&first_child, &mut state, 10).is_some());
        assert!(hook_reply(&input("PostToolUse", "t"), &mut state, 11).is_some());
        let mut second_child = input("PostToolUse", "t");
        second_child.agent_id = Some("b".to_owned());
        second_child.model = Some("sonnet".to_owned());
        assert!(hook_reply(&second_child, &mut state, 12).is_some());
        assert!(hook_reply(&first_child, &mut state, 13).is_none());
        assert_eq!(state.sessions.len(), 1);
        assert!(state.sessions["t"].stopped_at_ms.is_none());
        assert_eq!(
            state.sessions["t"].told_actors,
            ["", "a", "b"].into_iter().map(str::to_owned).collect()
        );
        let mut stop = input("Stop", "t");
        stop.agent_id = Some("a".to_owned());
        stop.model = Some("opus".to_owned());
        assert!(hook_reply(&stop, &mut state, 20).is_none());
        assert!(state.sessions["t"].stopped_at_ms.is_none());
        assert!(hook_reply(&input("Stop", "t"), &mut state, 21).is_none());
        assert_eq!(state.sessions["t"].stopped_at_ms, Some(21));

        let mut grok = input("PostToolUse", "g");
        grok.agent_id = Some("agent-1".to_owned());
        grok.model = Some("grok-4.7".to_owned());
        assert!(hook_reply(&grok, &mut state, 30).is_none());
        grok.hook_event_name = "Stop".to_owned();
        assert!(hook_reply(&grok, &mut state, 31).is_none());
        assert!(!state.sessions.contains_key("g"));

        let mut state = active();
        let mut parent_hook = input("PostToolUse", "p");
        parent_hook.cwd = Some(PathBuf::from("/parent"));
        parent_hook.transcript_path = Some(PathBuf::from("/parent/session.jsonl"));
        parent_hook.entrypoint = Some("sdk-ts".to_owned());
        assert!(hook_reply(&parent_hook, &mut state, 10).is_some());
        let mut child = input("PostToolUse", "p");
        child.agent_id = Some("a".to_owned());
        child.model = Some("haiku".to_owned());
        child.cwd = Some(PathBuf::from("/child"));
        child.transcript_path = Some(PathBuf::from("/child/session.jsonl"));
        child.entrypoint = Some("child".to_owned());
        assert!(hook_reply(&child, &mut state, 40).is_some());
        assert_eq!(state.sessions["p"].told_at_ms, 10);
        assert_eq!(state.sessions["p"].cwd, Some(PathBuf::from("/parent")));
        assert_eq!(
            state.sessions["p"].transcript,
            Some(PathBuf::from("/parent/session.jsonl"))
        );
        assert_eq!(state.sessions["p"].entrypoint.as_deref(), Some("sdk-ts"));

        let mut state = active();
        let mut child_first = input("PostToolUse", "c");
        child_first.agent_id = Some("a".to_owned());
        child_first.model = Some("opus".to_owned());
        child_first.cwd = Some(PathBuf::from("/child"));
        child_first.transcript_path = Some(PathBuf::from("/child/session.jsonl"));
        assert!(hook_reply(&child_first, &mut state, 5).is_some());
        assert_eq!(state.sessions["c"].told_at_ms, 5);
        assert!(state.sessions["c"].cwd.is_none());
        assert!(state.sessions["c"].transcript.is_none());
        let mut parent_stop = input("Stop", "c");
        parent_stop.cwd = Some(PathBuf::from("/parent"));
        parent_stop.transcript_path = Some(PathBuf::from("/parent/session.jsonl"));
        parent_stop.entrypoint = Some("sdk-ts".to_owned());
        assert!(hook_reply(&parent_stop, &mut state, 6).is_some());
        assert_eq!(state.sessions["c"].told_at_ms, 5);
        assert_eq!(state.sessions["c"].cwd, Some(PathBuf::from("/parent")));
        assert_eq!(
            state.sessions["c"].transcript,
            Some(PathBuf::from("/parent/session.jsonl"))
        );
        assert_eq!(state.sessions["c"].entrypoint.as_deref(), Some("sdk-ts"));
        assert!(state.sessions["c"].stopped_at_ms.is_none());
        assert!(hook_reply(&parent_stop, &mut state, 7).is_none());
        assert_eq!(state.sessions["c"].stopped_at_ms, Some(7));
        assert_eq!(state.sessions["c"].told_at_ms, 5);
    }

    #[test]
    fn released_parent_stop_keeps_the_child_clock_and_takes_parent_paths() {
        let mut state = active();
        let mut child = input("PostToolUse", "c");
        child.agent_id = Some("a".to_owned());
        child.model = Some("haiku".to_owned());
        child.cwd = Some(PathBuf::from("/child"));
        child.transcript_path = Some(PathBuf::from("/child/session.jsonl"));
        assert!(hook_reply(&child, &mut state, 5).is_some());
        state.active = false;
        let mut parent_stop = input("Stop", "c");
        parent_stop.cwd = Some(PathBuf::from("/parent"));
        parent_stop.transcript_path = Some(PathBuf::from("/parent/session.jsonl"));
        parent_stop.entrypoint = Some("sdk-ts".to_owned());
        assert!(hook_reply(&parent_stop, &mut state, 20).is_none());
        assert_eq!(state.sessions["c"].told_at_ms, 5);
        assert_eq!(state.sessions["c"].stopped_at_ms, Some(20));
        assert_eq!(state.sessions["c"].cwd, Some(PathBuf::from("/parent")));
        assert_eq!(
            state.sessions["c"].transcript,
            Some(PathBuf::from("/parent/session.jsonl"))
        );
        assert_eq!(state.sessions["c"].entrypoint.as_deref(), Some("sdk-ts"));
    }

    #[test]
    fn near_reset_does_not_open_the_pause_and_a_far_one_does() {
        let soon = Utc::now() + chrono::Duration::minutes(30);
        let later = Utc::now() + chrono::Duration::minutes(90);
        let levels = |at| vec![("a".to_owned(), usage_at(100.0, 10.0, Some(at)))];
        assert!(!verdict(&config(true), &levels(soon), Some("a")).at_limit);
        assert!(verdict(&config(true), &levels(later), Some("a")).at_limit);
        let mut forced = config(true);
        forced.limit_compact.force = true;
        assert!(verdict(&forced, &levels(soon), Some("a")).at_limit);
        let mut weekly = usage_at(100.0, 100.0, Some(soon)).unwrap();
        weekly.usage.seven_day.resets_at = Some(Utc::now() + chrono::Duration::days(5));
        let weekly_levels = vec![("a".to_owned(), Some(weekly))];
        assert!(verdict(&config(true), &weekly_levels, Some("a")).at_limit);
        let mut hard_week = usage_at(10.0, 80.0, Some(soon)).unwrap();
        hard_week.usage.seven_day.resets_at = Some(Utc::now() + chrono::Duration::days(4));
        hard_week.stops = crate::usage::Stops {
            five_hour: None,
            seven_day: Some(50.0),
            hard: true,
        };
        let hard_levels = vec![("a".to_owned(), Some(hard_week))];
        assert!(verdict(&config(true), &hard_levels, Some("a")).at_limit);
        let mut both_soon = usage_at(100.0, 100.0, Some(soon)).unwrap();
        both_soon.usage.seven_day.resets_at = Some(soon);
        let both_levels = vec![("a".to_owned(), Some(both_soon))];
        assert!(!verdict(&config(true), &both_levels, Some("a")).at_limit);
        let undated = usage_at(10.0, 100.0, Some(soon)).unwrap();
        let undated_levels = vec![("a".to_owned(), Some(undated))];
        assert!(verdict(&config(true), &undated_levels, Some("a")).at_limit);
    }

    #[test]
    fn compaction_starts_before_the_last_account_hits_the_pause() {
        let later = Utc::now() + chrono::Duration::minutes(90);
        let soon = Utc::now() + chrono::Duration::minutes(30);
        let at = |five, reset| ("a".to_owned(), usage_at(five, 10.0, Some(reset)));
        let full = ("b".to_owned(), usage_at(100.0, 10.0, Some(later)));
        let config = config(true);
        assert!(nearing_limit(
            &config,
            &[at(86.0, later), full.clone()],
            None
        ));
        assert!(may_spend(
            &config,
            &[at(86.0, later), full.clone()],
            Some("a")
        ));
        assert!(!nearing_limit(
            &config,
            &[at(70.0, later), full.clone()],
            None
        ));
        let mut above = config.clone();
        above.limit_compact.compact_from = 99.0;
        assert!(nearing_limit(
            &above,
            &[at(96.0, later), full.clone()],
            None
        ));
        assert!(above.validate().is_ok());
        let freeing = ("b".to_owned(), usage_at(100.0, 10.0, Some(soon)));
        assert!(!nearing_limit(&config, &[at(86.0, later), freeing], None));
        assert!(!nearing_limit(
            &config,
            &[
                at(86.0, later),
                ("b".to_owned(), usage_at(40.0, 10.0, Some(later)))
            ],
            None
        ));
        let calm = [at(70.0, later), full.clone()];
        assert!(nearing_limit(&config, &calm, Some(1.0)));
        assert!(!nearing_limit(&config, &calm, Some(0.5)));
        let mut no_lead = config.clone();
        no_lead.limit_compact.compact_lead_minutes = 0;
        assert!(!nearing_limit(&no_lead, &calm, Some(1.0)));
        let refilled = [
            at(70.0, later),
            ("b".to_owned(), usage_at(100.0, 10.0, Some(soon))),
        ];
        assert!(!nearing_limit(&config, &refilled, Some(0.5)));
    }

    #[test]
    fn only_a_compacted_or_small_thread_is_cheap_to_resume() {
        let detail = |compaction, tokens| t3::ThreadDetail {
            native_session_ids: Vec::new(),
            used_tokens: tokens,
            last_item_is_compaction: compaction,
            last_compaction_at: None,
            last_foreign_message_at: None,
            message_ids: Vec::new(),
        };
        assert!(cheap_to_resume(&detail(true, Some(200_000)), 60_000));
        assert!(cheap_to_resume(&detail(false, Some(10_000)), 60_000));
        assert!(!cheap_to_resume(&detail(false, Some(200_000)), 60_000));
        assert!(!cheap_to_resume(&detail(false, None), 60_000));
        assert!(matches!(
            resume_when_ready(&detail(true, Some(200_000)), 60_000),
            ResumeWhenReady::Send
        ));
        assert!(matches!(
            resume_when_ready(&detail(false, Some(10_000)), 60_000),
            ResumeWhenReady::Send
        ));
        assert!(matches!(
            resume_when_ready(&detail(false, Some(200_000)), 60_000),
            ResumeWhenReady::KeepUntilCompact
        ));
        assert!(matches!(
            resume_when_ready(&detail(false, None), 60_000),
            ResumeWhenReady::KeepUntilCompact
        ));
        let fat = detail(false, Some(200_000));
        let unknown = detail(false, None);
        assert!(matches!(
            resume_delivery(resume_when_ready(&fat, 60_000), false),
            ResumeDelivery::Wait
        ));
        assert!(matches!(
            resume_delivery(resume_when_ready(&unknown, 60_000), false),
            ResumeDelivery::Wait
        ));
        assert!(matches!(
            resume_delivery(resume_when_ready(&fat, 60_000), true),
            ResumeDelivery::Send
        ));
        assert!(matches!(
            resume_delivery(resume_when_ready(&unknown, 60_000), true),
            ResumeDelivery::Send
        ));
        assert!(matches!(
            resume_delivery(
                resume_when_ready(&detail(true, Some(200_000)), 60_000),
                false
            ),
            ResumeDelivery::Send
        ));
    }

    #[test]
    fn resume_message_allows_compacted_subagents() {
        let text = resume_message(&Parked::default(), None);
        assert!(text.contains("Compacted sub-agent"));
        assert!(text.contains("was not compacted"));
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
        usage_at(five_hour, seven_day, None)
    }

    fn usage_at(
        five_hour: f64,
        seven_day: f64,
        resets_at: Option<DateTime<Utc>>,
    ) -> Option<Candidate> {
        let bucket = |utilization, resets_at| crate::usage::Bucket {
            utilization,
            resets_at,
        };
        Some(Candidate {
            name: String::new(),
            usage: crate::usage::Usage {
                five_hour: bucket(five_hour, resets_at),
                seven_day: bucket(seven_day, None),
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
        assert!(
            hook_reply_with(
                &input("PostToolUse", "s"),
                &mut state,
                10,
                false,
                &ClaudeModels::default()
            )
            .is_none()
        );
        assert!(state.sessions.is_empty());
        assert!(
            hook_reply_with(
                &input("PostToolUse", "s"),
                &mut state,
                10,
                true,
                &ClaudeModels::default()
            )
            .is_some()
        );
    }

    #[test]
    fn only_a_proven_free_account_lifts_the_block() {
        let active = |name: &'static str| move || Some(name.to_owned());
        let levels = vec![
            ("a".to_owned(), usage(100.0, 10.0)),
            ("b".to_owned(), usage(10.0, 10.0)),
        ];
        assert!(!still_blocked(&config(true), &levels, active("a")));
        assert!(still_blocked(&config(false), &levels, active("a")));
        assert!(!still_blocked(&config(false), &levels, active("b")));
        let full = vec![("a".to_owned(), usage(100.0, 10.0))];
        assert!(still_blocked(&config(true), &full, active("a")));
        let unknown = vec![("a".to_owned(), usage(100.0, 10.0)), ("b".to_owned(), None)];
        assert!(still_blocked(&config(true), &unknown, active("a")));
        let mut forced = config(true);
        forced.limit_compact.force = true;
        assert!(still_blocked(&forced, &levels, active("a")));
    }

    #[test]
    fn t3_gets_two_minutes_to_resume_first() {
        assert!(settling(1_000, 1));
        assert!(settling(1_000, 120));
        assert!(!settling(1_000, 121));
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
    fn entry_after_pre_pause_compaction_keeps_its_markers() {
        let mut state = LimitState::default();
        state.compacted.insert("t".to_owned(), 1);
        state.compacting_sessions.insert("s".to_owned(), 4000);
        state.compacting_sessions.insert("old".to_owned(), 1);
        reenter(&mut state, 5000);
        assert!(!state.compacting_sessions.contains_key("old"));
        assert!(state.compacted.is_empty());
        state.active = true;
        assert!(hook_reply(&input("Stop", "s"), &mut state, 50).is_none());
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn reentry_keeps_only_stopped_sessions_and_fresh_compaction_markers_and_resets_the_budget() {
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
        let dropped = reenter(&mut state, 60);
        assert_eq!(dropped, vec!["manual".to_owned()]);
        assert!(!state.sessions["stopped"].retell);
        assert!(state.sessions["running"].retell);
        let mut active_state = state.clone();
        active_state.active = true;
        assert!(hook_reply(&input("PostToolUse", "running"), &mut active_state, 50).is_some());
        assert!(!active_state.sessions["running"].retell);
        assert_eq!(active_state.sessions["running"].told_at_ms, 50);
        assert!(state.compacted.is_empty() && state.compacting_sessions.contains_key("s"));
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
            settled: false,
            last_run_completed_at: Some(now - chrono::Duration::minutes(minutes_ago)),
            last_user_message_at: Some(now - chrono::Duration::minutes(minutes_ago + 1)),
            updated_at: None,
        };
        assert!(warm_candidate(&thread(10, false), &limit, now));
        assert!(!warm_candidate(&thread(10, true), &limit, now));
        assert!(!warm_candidate(&thread(0, false), &limit, now));
        assert!(!warm_candidate(&thread(70, false), &limit, now));
        let settled = ThreadSummary {
            settled: true,
            ..thread(10, false)
        };
        assert!(!warm_candidate(&settled, &limit, now));
    }
}
