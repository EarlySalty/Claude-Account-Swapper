use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use wait_timeout::ChildExt;

use crate::routing::Selection;
use crate::runtime::{Lease, ManagedSession};
use crate::{App, unix_timestamp};

pub(crate) fn parent_death_signal(command: &mut Command) {
    let parent = std::process::id();
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent as libc::pid_t {
                return Err(std::io::Error::other("Launcher wurde beendet"));
            }
            Ok(())
        });
    }
}

enum Event {
    Input(Vec<u8>),
    InputClosed,
    Output(u64, Vec<u8>),
    OutputClosed(u64),
    Error(String),
}

struct Process {
    child: Child,
    input: Option<ChildStdin>,
    lease: Lease,
    generation: u64,
}

impl Process {
    fn send(&mut self, line: &[u8]) -> Result<()> {
        let input = self
            .input
            .as_mut()
            .context("Claude-Eingang ist geschlossen")?;
        input.write_all(line)?;
        input.flush()?;
        Ok(())
    }

    fn stop(mut self, seconds: u64) -> Result<()> {
        self.input.take();
        if self
            .child
            .wait_timeout(Duration::from_secs(seconds))?
            .is_none()
        {
            self.child.kill()?;
            self.child.wait()?;
        }
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.input.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn forwarded_args(original: &[String], id: &str, resume: bool) -> Vec<String> {
    let mut result = Vec::new();
    let mut iterator = original.iter();
    while let Some(argument) = iterator.next() {
        if matches!(argument.as_str(), "--resume" | "-r" | "--session-id") {
            iterator.next();
            continue;
        }
        if argument.starts_with("--resume=") || argument.starts_with("--session-id=") {
            continue;
        }
        result.push(argument.clone());
    }
    result.push(if resume { "--resume" } else { "--session-id" }.into());
    result.push(id.into());
    result
}

fn session_argument(arguments: &[String]) -> Option<String> {
    arguments.iter().enumerate().find_map(|(index, argument)| {
        if matches!(argument.as_str(), "--resume" | "-r" | "--session-id") {
            arguments.get(index + 1).cloned()
        } else {
            argument
                .strip_prefix("--resume=")
                .or_else(|| argument.strip_prefix("--session-id="))
                .map(str::to_owned)
        }
    })
}

fn is_stream(arguments: &[String]) -> bool {
    arguments.iter().enumerate().any(|(index, value)| {
        value == "--input-format=stream-json"
            || (value == "--input-format"
                && arguments
                    .get(index + 1)
                    .is_some_and(|value| value == "stream-json"))
    })
}

fn spawn_process(
    app: &App,
    lease: Lease,
    session: &ManagedSession,
    arguments: &[String],
    resume: bool,
    generation: u64,
    sender: mpsc::Sender<Event>,
) -> Result<Process> {
    let mut command = app.native_command(&lease)?;
    command
        .args(forwarded_args(arguments, &session.id, resume))
        .current_dir(&session.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    parent_death_signal(&mut command);
    let mut child = command
        .spawn()
        .context("Claude-Prozess konnte nicht gestartet werden")?;
    let input = child.stdin.take();
    let output = child.stdout.take().context("Claude-Ausgang fehlt")?;
    std::thread::spawn(move || {
        let mut reader = BufReader::new(output);
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if sender.send(Event::Output(generation, line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Event::Error(format!("Claude-Ausgang: {error}")));
                    break;
                }
            }
        }
        let _ = sender.send(Event::OutputClosed(generation));
    });
    Ok(Process {
        child,
        input,
        lease,
        generation,
    })
}

fn replayable_control(kind: &str) -> bool {
    matches!(
        kind,
        "initialize"
            | "set_permission_mode"
            | "set_model"
            | "set_max_thinking_tokens"
            | "mcp_set_servers"
    )
}

fn replay_controls(
    child: &mut Process,
    replay: &BTreeMap<String, Value>,
    internal_ids: &mut BTreeSet<String>,
) -> Result<()> {
    for message in replay.values() {
        let mut message = message.clone();
        let id = format!("account-replay-{}", uuid::Uuid::new_v4());
        message["request_id"] = Value::String(id.clone());
        internal_ids.insert(id);
        child.send(format!("{}\n", serde_json::to_string(&message)?).as_bytes())?;
    }
    Ok(())
}

pub fn run(app: &App, arguments: Vec<String>) -> Result<i32> {
    if !is_stream(&arguments) {
        return Ok(Command::new(&app.paths.claude_bin)
            .args(arguments)
            .status()?
            .code()
            .unwrap_or(1));
    }
    app.start_native_monitor();
    ensure!(
        !arguments
            .iter()
            .any(|value| value == "--continue" || value == "-c"),
        "T3-Routing braucht eine explizite Session-ID statt --continue"
    );
    let id = session_argument(&arguments).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut session = ManagedSession::new(id, "t3", std::env::current_dir()?.canonicalize()?)?;
    let mut resume = arguments
        .iter()
        .any(|value| value == "--resume" || value == "-r" || value.starts_with("--resume="));
    let _session_lock = app
        .try_lock_path(&format!("native-thread-{}.lock", session.id))?
        .context("Diese Claude-Session wird bereits verwaltet")?;
    let previous = app
        .managed_sessions()?
        .into_iter()
        .find(|previous| previous.id == session.id);
    if let Some(previous) = previous {
        ensure!(
            !previous.alive(),
            "Diese Claude-Session läuft bereits in einem anderen Launcher"
        );
        session.account = previous.account;
        session.last_switch = previous.last_switch;
    }
    let (sender, receiver) = mpsc::channel();
    let input_sender = sender.clone();
    std::thread::spawn(move || {
        let mut reader = std::io::stdin().lock();
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => {
                    let _ = input_sender.send(Event::InputClosed);
                    break;
                }
                Ok(_) => {
                    if input_sender.send(Event::Input(line)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = input_sender.send(Event::Error(format!("T3-Eingang: {error}")));
                    break;
                }
            }
        }
    });
    let mut output = std::io::stdout().lock();
    let mut process: Option<Process> = None;
    let mut generation = 0u64;
    let mut inflight = 0usize;
    let mut background = BTreeSet::<String>::new();
    let mut queue = VecDeque::<Vec<u8>>::new();
    let mut replay = BTreeMap::<String, Value>::new();
    let mut pending_controls = BTreeMap::<String, (String, Value)>::new();
    let mut internal_ids = BTreeSet::<String>::new();
    let mut closed_input = false;
    let mut last_activity = Instant::now();
    let mut previous_directory: Option<std::path::PathBuf> = None;
    app.write_managed_session(&mut session)?;
    loop {
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(Event::Input(line)) => {
                let message: Value =
                    serde_json::from_slice(&line).context("T3 lieferte ungültiges Stream-JSON")?;
                let control =
                    message.get("type").and_then(Value::as_str) == Some("control_request");
                if control
                    && message.pointer("/request/subtype").and_then(Value::as_str)
                        == Some("interrupt")
                    && inflight == 0
                    && background.is_empty()
                    && !queue.is_empty()
                {
                    let request_id = message
                        .get("request_id")
                        .context("Interrupt ohne Anfragekennung")?;
                    let acknowledgement = serde_json::json!({"type":"control_response","response":{"subtype":"success","request_id":request_id,"response":{}}});
                    writeln!(output, "{acknowledgement}")?;
                    output.flush()?;
                    session.state = "stopped".into();
                    session.reason = "Wartender Auftrag wurde vor dem Start abgebrochen".into();
                    app.write_managed_session(&mut session)?;
                    return Ok(0);
                }
                if control && !queue.is_empty() {
                    queue.push_front(line);
                } else {
                    queue.push_back(line);
                }
                last_activity = Instant::now();
            }
            Ok(Event::InputClosed) => {
                closed_input = true;
            }
            Ok(Event::Error(error)) => anyhow::bail!("{error}"),
            Ok(Event::Output(source, line)) => {
                if source == generation {
                    let value: Value = serde_json::from_slice(&line)
                        .context("Claude lieferte ungültiges Stream-JSON")?;
                    let request_id = value
                        .pointer("/response/request_id")
                        .and_then(Value::as_str);
                    if let Some(id) = request_id {
                        if internal_ids.remove(id) {
                            ensure!(
                                value.pointer("/response/subtype").and_then(Value::as_str)
                                    != Some("error"),
                                "Claude konnte den T3-Kontrollzustand nach dem Wechsel nicht wiederherstellen"
                            );
                            continue;
                        }
                        if let Some((kind, message)) = pending_controls.remove(id)
                            && replayable_control(&kind)
                            && value.pointer("/response/subtype").and_then(Value::as_str)
                                == Some("success")
                        {
                            replay.insert(kind, message);
                        }
                    }
                    output.write_all(&line)?;
                    output.flush()?;
                    if value.get("type").and_then(Value::as_str) == Some("result") {
                        inflight = inflight.saturating_sub(1);
                        resume = true;
                        session.state = if background.is_empty() {
                            "idle"
                        } else {
                            "background"
                        }
                        .into();
                        app.write_managed_session(&mut session)?;
                    }
                    if value.get("type").and_then(Value::as_str) == Some("system") {
                        if let Some(id) = value.get("task_id").and_then(Value::as_str) {
                            match value.get("subtype").and_then(Value::as_str) {
                                Some("task_started") => {
                                    background.insert(id.into());
                                }
                                Some("task_notification") => {
                                    background.remove(id);
                                }
                                _ => {}
                            }
                        }
                        if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                            ensure!(
                                id == session.id,
                                "Claude hat eine andere Session-ID geöffnet; kein automatischer Kontextwechsel"
                            );
                        }
                    }
                    last_activity = Instant::now();
                }
            }
            Ok(Event::OutputClosed(source)) => {
                if process
                    .as_ref()
                    .is_some_and(|process| process.generation == source)
                {
                    let mut ended = process.take().context("Claude-Prozess fehlt")?;
                    let code = ended.child.wait()?.code().unwrap_or(1);
                    session.state = if inflight == 0 && code == 0 {
                        "stopped"
                    } else {
                        "error"
                    }
                    .into();
                    app.write_managed_session(&mut session)?;
                    return Ok(code);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let config = app.routing_config()?;
        let idle = inflight == 0
            && background.is_empty()
            && pending_controls.is_empty()
            && internal_ids.is_empty();
        if idle
            && !closed_input
            && queue.is_empty()
            && last_activity.elapsed().as_secs() >= config.routing.idle_release_seconds
            && let Some(old) = process.take()
        {
            previous_directory = Some(old.lease.directory.clone());
            old.stop(config.routing.shutdown_seconds)?;
            session.state = "idle".into();
            session.reason =
                "Account im Leerlauf für andere Sessions und Auffrischung freigegeben".into();
            app.write_managed_session(&mut session)?;
        }
        if closed_input
            && queue.is_empty()
            && let Some(process) = &mut process
        {
            process.input.take();
        }
        if closed_input && queue.is_empty() && process.is_none() {
            session.state = "stopped".into();
            app.write_managed_session(&mut session)?;
            break;
        }
        let Some(line) = queue.front() else {
            continue;
        };
        let value: Value =
            serde_json::from_slice(line).context("T3 lieferte ungültiges Stream-JSON")?;
        let user = value.get("type").and_then(Value::as_str) == Some("user");
        if idle && user {
            let own = process
                .as_ref()
                .map(|process| process.lease.account.as_str());
            let mut allocation = session.clone();
            if !resume {
                allocation.account = None;
            }
            match app.choose(&allocation, None, own)? {
                Selection::Wait { reason } => {
                    if let Some(old) = process.take() {
                        previous_directory = Some(old.lease.directory.clone());
                        old.stop(config.routing.shutdown_seconds)?;
                    }
                    if session.state != "waiting" || session.reason != reason {
                        session.state = "waiting".into();
                        session.reason = reason;
                        app.write_managed_session(&mut session)?;
                    }
                    continue;
                }
                Selection::Use { account, reason } => {
                    if process
                        .as_ref()
                        .is_some_and(|process| process.lease.account != account)
                    {
                        let old = process.take().context("Claude-Prozess fehlt")?;
                        previous_directory = Some(old.lease.directory.clone());
                        old.stop(config.routing.shutdown_seconds)?;
                    }
                    if process.is_none() {
                        let Some(lease) = app.acquire_account(&account)? else {
                            continue;
                        };
                        if resume && let Some(previous) = &previous_directory {
                            ensure!(
                                previous
                                    .join("projects")
                                    .canonicalize()
                                    .ok()
                                    .zip(lease.directory.join("projects").canonicalize().ok())
                                    .is_some_and(|(a, b)| a == b),
                                "Zum Account-Wechsel mit bestehendem Kontext müssen beide Accounts den Verlauf teilen: `claude-account share-setup <account>`"
                            );
                        }
                        generation += 1;
                        let mut child = spawn_process(
                            app,
                            lease,
                            &session,
                            &arguments,
                            resume,
                            generation,
                            sender.clone(),
                        )?;
                        replay_controls(&mut child, &replay, &mut internal_ids)?;
                        if session.account.as_deref() != Some(&account) {
                            session.last_switch = unix_timestamp()?;
                        }
                        session.account = Some(account);
                        process = Some(child);
                    }
                    session.reason = reason;
                }
            }
        }
        if process.is_none() && !user {
            let rule = config.routing.rule(&session.cwd, Some(&session.id));
            for name in config.routing.homes.keys().filter(|name| {
                !config.routing.disabled.contains(*name)
                    && rule.account.as_ref().is_none_or(|fixed| fixed == *name)
                    && (rule.pool.is_empty() || rule.pool.contains(*name))
            }) {
                if let Some(lease) = app.acquire_account(name)? {
                    generation += 1;
                    let mut child = spawn_process(
                        app,
                        lease,
                        &session,
                        &arguments,
                        resume,
                        generation,
                        sender.clone(),
                    )?;
                    if value.pointer("/request/subtype").and_then(Value::as_str)
                        != Some("initialize")
                    {
                        replay_controls(&mut child, &replay, &mut internal_ids)?;
                    }
                    process = Some(child);
                    session.account = Some(name.clone());
                    break;
                }
            }
        }
        if user && (!pending_controls.is_empty() || !internal_ids.is_empty()) {
            continue;
        }
        let Some(child) = &mut process else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("control_request")
            && let Some(subtype) = value.pointer("/request/subtype").and_then(Value::as_str)
            && let Some(id) = value.get("request_id").and_then(Value::as_str)
        {
            pending_controls.insert(id.into(), (subtype.into(), value.clone()));
        }
        child.send(&queue.pop_front().context("T3-Nachricht fehlt")?)?;
        if user {
            inflight += 1;
            session.state = "running".into();
        }
        app.write_managed_session(&mut session)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resume_replaces_only_session_flags() {
        let arguments = [
            "--input-format",
            "stream-json",
            "--session-id",
            "old",
            "--model",
            "custom",
        ]
        .map(String::from);
        let forwarded = forwarded_args(&arguments, "new", true);
        assert_eq!(
            forwarded,
            [
                "--input-format",
                "stream-json",
                "--model",
                "custom",
                "--resume",
                "new"
            ]
        );
    }
    #[test]
    fn supports_both_stream_argument_spellings() {
        assert!(is_stream(&["--input-format=stream-json".into()]));
        assert!(!is_stream(&["--version".into()]));
    }
}
