use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::config::LimitCompact;
use crate::t3::{self, ThreadSummary};
use crate::{App, log_event, log_once};

const STATE_FILE: &str = "idle-compact.json";
const LOCK_FILE: &str = ".idle-compact.lock";
const TICK_LOCK_FILE: &str = ".idle-compact-tick.lock";
const COMPACT_COMMAND: &str = "/compact";
const SPOKEN_KEY: &str = "idle-compact";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Checked {
    pub threads: BTreeMap<String, i64>,
}

fn idle_candidate(thread: &ThreadSummary, limit: &LimitCompact, now: DateTime<Utc>) -> bool {
    let Some(completed) = thread.last_run_completed_at else {
        return false;
    };
    let idle = now.signed_duration_since(completed).num_seconds();
    limit.idle_compact_minutes > 0
        && !thread.busy
        && !thread.settled
        && idle >= seconds(limit.idle_compact_minutes)
        && idle < seconds(limit.cache_ttl_minutes)
        && thread
            .last_user_message_at
            .is_none_or(|message| message <= completed)
}

fn seconds(minutes: u64) -> i64 {
    i64::try_from(minutes.saturating_mul(60)).unwrap_or(i64::MAX)
}

fn completed_ms(thread: &ThreadSummary) -> i64 {
    thread
        .last_run_completed_at
        .map_or(0, |completed| completed.timestamp_millis())
}

fn in_scope(sessions: &[String], limit: &LimitCompact) -> bool {
    limit.scope_sessions.is_empty()
        || sessions
            .iter()
            .any(|session| limit.scope_sessions.contains(session))
}

fn unchecked<'a>(candidates: &[&'a ThreadSummary], checked: &Checked) -> Vec<&'a ThreadSummary> {
    candidates
        .iter()
        .copied()
        .filter(|thread| checked.threads.get(&thread.id) != Some(&completed_ms(thread)))
        .collect()
}

impl App {
    fn checked_path(&self) -> PathBuf {
        self.paths.store.join(STATE_FILE)
    }

    fn load_checked(&self) -> Result<Checked> {
        match fs::read(self.checked_path()) {
            Ok(data) => serde_json::from_slice(&data)
                .context("Zustand des Ruhe-Kompaktierens ist kein gueltiges JSON"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Checked::default()),
            Err(error) => Err(error).context("Zustand des Ruhe-Kompaktierens nicht lesbar"),
        }
    }

    fn update_checked(&self, change: impl FnOnce(&mut Checked)) -> Result<()> {
        let lock = self.open_lock(LOCK_FILE)?;
        FileExt::lock_exclusive(&lock).context("Ruhe-Lock nicht zu setzen")?;
        let mut checked = self.load_checked()?;
        let before = checked.clone();
        change(&mut checked);
        if checked != before {
            let payload = serde_json::to_vec_pretty(&checked)?;
            crate::atomic_write(&self.checked_path(), &payload, 0o600)?;
        }
        Ok(())
    }

    pub(crate) fn idle_compact_tick(&self, spoken: &mut HashMap<String, String>) {
        match self.idle_compact_pass(spoken) {
            Ok(()) => {
                spoken.remove(SPOKEN_KEY);
            }
            Err(error) => log_once(
                spoken,
                SPOKEN_KEY,
                &format!("Ruhe-Kompaktieren fehlgeschlagen: {error:#}"),
            ),
        }
    }

    fn idle_compact_pass(&self, spoken: &mut HashMap<String, String>) -> Result<()> {
        let tick_lock = self.open_lock(TICK_LOCK_FILE)?;
        if FileExt::try_lock_exclusive(&tick_lock).is_err() {
            return Ok(());
        }
        self.check_config_file()?;
        let limit = self.load_config().limit_compact;
        if limit.idle_compact_minutes == 0 || self.limit_pause_holds() {
            return Ok(());
        }
        let client = self.t3_client(&limit);
        let threads = client.claude_threads()?;
        let now = Utc::now();
        let candidates: Vec<&ThreadSummary> = threads
            .iter()
            .filter(|thread| idle_candidate(thread, &limit, now))
            .collect();
        let checked = self.load_checked()?;
        let pending = unchecked(&candidates, &checked);
        let mut rpc: Option<t3::Rpc> = None;
        let mut failed = Vec::new();
        for thread in pending {
            let key = format!("{SPOKEN_KEY}:{}", thread.id);
            match self.compact_idle_thread(&client, &mut rpc, thread, &limit) {
                Ok(()) => {
                    spoken.remove(&key);
                }
                Err(error) => {
                    rpc = None;
                    failed.push(thread.id.clone());
                    log_once(
                        spoken,
                        &key,
                        &format!(
                            "Ruhe-Kompaktieren fuer Thread {} fehlgeschlagen: {error:#}",
                            thread.id
                        ),
                    );
                }
            }
        }
        self.update_checked(|checked| {
            checked.threads.retain(|id, _| {
                candidates.iter().any(|thread| &thread.id == id) && !failed.contains(id)
            });
        })?;
        spoken.retain(|key, _| {
            key.strip_prefix(SPOKEN_KEY)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_none_or(|id| failed.iter().any(|failed| failed == id))
        });
        Ok(())
    }

    fn compact_idle_thread(
        &self,
        client: &t3::Client,
        rpc: &mut Option<t3::Rpc>,
        thread: &ThreadSummary,
        limit: &LimitCompact,
    ) -> Result<()> {
        let rpc = connected(client, rpc)?;
        let detail = rpc.thread_detail(&thread.id)?;
        let skip = !in_scope(&detail.native_session_ids, limit)
            || !crate::limit::worth_compacting(&detail, limit)
            || self.limit_compacted(&thread.id)?;
        if skip {
            let id = thread.id.clone();
            let done = completed_ms(thread);
            return self.update_checked(|checked| {
                checked.threads.insert(id, done);
            });
        }
        self.note_compacting(&detail.native_session_ids)?;
        let message_id = uuid::Uuid::new_v4().to_string();
        if let Err(error) = rpc.send_message(&thread.id, COMPACT_COMMAND, &message_id) {
            if t3::is_rejection(&error) {
                let _ = self.clear_compacting(&detail.native_session_ids);
            }
            return Err(error);
        }
        let id = thread.id.clone();
        let done = completed_ms(thread);
        self.update_checked(|checked| {
            checked.threads.insert(id, done);
        })?;
        log_event(&format!(
            "Ruhe-Kompaktieren: Thread {} kompaktiert ({} Tokens im Kontext)",
            thread.id,
            detail.used_tokens.unwrap_or(0)
        ));
        Ok(())
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

    fn thread(idle_minutes: i64, now: DateTime<Utc>) -> ThreadSummary {
        ThreadSummary {
            id: "t".to_owned(),
            busy: false,
            settled: false,
            last_run_completed_at: Some(now - chrono::Duration::minutes(idle_minutes)),
            last_user_message_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn compacts_unsettled_threads_idle_inside_the_cache_window() {
        let now = Utc::now();
        let mut limit = LimitCompact {
            idle_compact_minutes: 30,
            ..LimitCompact::default()
        };
        assert!(!idle_candidate(&thread(29, now), &limit, now));
        assert!(idle_candidate(&thread(30, now), &limit, now));
        assert!(idle_candidate(&thread(54, now), &limit, now));
        assert!(!idle_candidate(&thread(55, now), &limit, now));
        let mut settled = thread(40, now);
        settled.settled = true;
        assert!(!idle_candidate(&settled, &limit, now));
        let mut busy = thread(40, now);
        busy.busy = true;
        assert!(!idle_candidate(&busy, &limit, now));
        let mut queued = thread(40, now);
        queued.last_user_message_at = Some(now);
        assert!(!idle_candidate(&queued, &limit, now));
        let mut never = thread(40, now);
        never.last_run_completed_at = None;
        assert!(!idle_candidate(&never, &limit, now));
        limit.idle_compact_minutes = 0;
        assert!(!idle_candidate(&thread(40, now), &limit, now));
    }

    #[test]
    fn a_checked_thread_is_skipped_until_it_runs_again() {
        let now = Utc::now();
        let first = thread(40, now);
        let mut checked = Checked::default();
        checked.threads.insert("t".to_owned(), completed_ms(&first));
        assert!(unchecked(&[&first], &checked).is_empty());
        let rerun = thread(35, now);
        assert_eq!(unchecked(&[&rerun], &checked).len(), 1);
    }
}
