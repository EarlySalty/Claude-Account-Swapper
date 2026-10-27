const $ = (selector, root = document) => root.querySelector(selector);
const notice = $("#notice");
let dashboard = null;
let refreshing = false;

function showNotice(message, isError = false) {
  notice.textContent = message || "";
  notice.classList.toggle("error", isError);
  notice.classList.toggle("visible", Boolean(message));
}

function validName(name) {
  return name.length > 0
    && name.trim() === name
    && name !== "."
    && name !== ".."
    && new TextEncoder().encode(name).length <= 64
    && !name.includes("/")
    && !name.includes("\\")
    && !/\p{Cc}/u.test(name);
}

function shellQuote(value) {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function actionButton(text, action, className = "button button-secondary", extra = {}) {
  const button = el("button", className, text);
  button.type = "button";
  button.dataset.action = action;
  for (const [key, value] of Object.entries(extra)) button.dataset[key] = value;
  return button;
}

function percent(value) {
  return Number.isFinite(Number(value)) ? `${Math.round(Number(value))}%` : "–";
}

function durationLabel(milliseconds) {
  const minutes = Math.max(0, Math.round(milliseconds / 60000));
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor((minutes % 1440) / 60);
  const rest = minutes % 60;
  if (days) return `${days} T. ${hours} Std.`;
  if (hours) return `${hours} Std. ${rest} Min.`;
  return `${rest} Min.`;
}

function resetLabel(value) {
  if (!value) return "Fenster ungenutzt oder zurückgesetzt";
  const date = new Date(value);
  if (Number.isNaN(date.valueOf())) return "Reset-Zeit unbekannt";
  const at = date.toLocaleString("de-DE", { weekday: "short", hour: "2-digit", minute: "2-digit" });
  const remaining = date.valueOf() - Date.now();
  return remaining > 0 ? `Reset in ${durationLabel(remaining)} · ${at}` : `Zurückgesetzt seit ${at}`;
}

function weeklyPace(profile, threshold) {
  const week = profile.usage?.windows?.seven_day;
  if (!week) return null;
  const stop = profile.limits?.seven_day ?? threshold;
  const headroom = Math.max(0, stop - (Number(week.utilization) || 0));
  const reset = week.resets_at ? new Date(week.resets_at).valueOf() : NaN;
  const hours = Number.isNaN(reset) ? 168 : Math.max(1, (reset - Date.now()) / 3600000);
  return { headroom, hours, pace: headroom / hours };
}

function paceLabel(profile) {
  const pace = weeklyPace(profile, Number(dashboard?.config?.threshold) || 98);
  if (!pace) return "";
  return `Wochenrest ${Math.round(pace.headroom)}% · braucht ${pace.pace.toLocaleString("de-DE", { maximumFractionDigits: 2 })}%/Std. bis zum Reset`;
}

function renderFlow() {
  const config = dashboard?.config || {};
  const service = dashboard?.service || {};
  const profiles = dashboard?.profiles || [];
  const running = Boolean(service.active);
  const auto = Boolean(config.auto_switch);
  const threshold = Math.round(Number(config.threshold) || 0);
  const others = profiles.filter((profile) => !profile.active).map((profile) => profile.name);
  let title;
  let note;
  if (!service.installed) {
    title = "Hintergrunddienst fehlt";
    note = "Ohne den Dienst bleibt die Auslastung stehen und es wird nicht automatisch gewechselt.";
  } else if (!running) {
    title = "Hintergrunddienst ist gestoppt";
    note = "Auslastung und Reset-Zeiten werden nicht aktualisiert, Logins werden nicht vorbereitet und es gibt keinen Auto-Wechsel.";
  } else if (!auto) {
    title = "Auslastung wird jede Minute geladen";
    note = "Auto-Wechsel ist aus. Bei vollem Limit wird nicht von selbst gewechselt.";
  } else {
    title = `Auto-Wechsel aktiv ab ${threshold}%`;
    const pacing = config.weekly_pacing !== false;
    const urgent = [...profiles].map((profile) => ({ profile, pace: weeklyPace(profile, threshold) })).filter((entry) => entry.pace).sort((left, right) => right.pace.pace - left.pace.pace)[0];
    note = !others.length
      ? "Speichere einen zweiten Account, damit gewechselt werden kann."
      : pacing
        ? `Bei ${threshold}% im 5h- oder Wochenfenster wird sofort gewechselt. Dazwischen wird das Wochenkontingent gleichmäßig verbraucht${urgent ? `, gerade bevorzugt: ${urgent.profile.name}` : ""}.`
        : `Erreicht der aktive Account ${threshold}% oder seine eigene Grenze, wechselt der Dienst zum freiesten Account (${others.join(", ")}).`;
  }
  $("#flow-title").textContent = title;
  $("#flow-note").textContent = note;
  $("#flow-strip").classList.toggle("is-good", running && auto);
  $("#flow-strip").classList.toggle("is-warn", !running);
  $("#flow-enable").hidden = running && auto;
  $("#flow-enable").disabled = !service.installed;
  $("#flow-disable").hidden = !auto;
}

async function enableAutomation() {
  try {
    const config = { ...(dashboard?.config || {}), auto_switch: true };
    await api("api/config", config);
    const service = dashboard?.service || {};
    if (!service.enabled) await api("api/service", { action: "enable" });
    await api("api/service", { action: service.active ? "restart" : "start" });
    showNotice("Automatik ist eingeschaltet. Die Auslastung wird ab jetzt jede Minute geladen.");
  } catch (error) {
    showNotice(error.message, true);
  }
  await loadDashboard();
}

async function disableAutoSwitch() {
  try {
    await api("api/config", { ...(dashboard?.config || {}), auto_switch: false });
    showNotice("Auto-Wechsel ist aus. Die Auslastung wird weiter geladen.");
  } catch (error) {
    showNotice(error.message, true);
  }
  await loadDashboard();
}

function timeLabel(unixSeconds) {
  if (!unixSeconds) return "–";
  return new Date(unixSeconds * 1000).toLocaleString("de-DE", { dateStyle: "medium", timeStyle: "short" });
}

async function api(path, body) {
  const options = { cache: "no-store" };
  if (body !== undefined) {
    options.method = "POST";
    options.headers = { "Content-Type": "application/json" };
    options.body = JSON.stringify(body);
  }
  const response = await fetch(path, options);
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(data.error || `Anfrage fehlgeschlagen (${response.status}).`);
  return data;
}

async function run(path, body, successText) {
  try {
    const data = await api(path, body);
    showNotice(data.task ? `${data.task.action} läuft. Der Status erscheint unten im Aktionsverlauf.` : (data.message || successText || "Aktion abgeschlossen."));
    await loadDashboard();
  } catch (error) {
    showNotice(error.message, true);
  }
}

function switchProfile(name, skipCheck = false) {
  if (skipCheck && !window.confirm(`Ohne Prüfung zu „${name}“ wechseln? Ein ungültiger Login kann offene Claude-Sitzungen auf diesem Rechner unterbrechen.`)) return;
  run("api/accounts/switch", { name, skipCheck });
}

function createUsageBucket(label, bucket, ageSeconds) {
  const wrap = el("div", "usage-block");
  const heading = el("div", "usage-heading");
  heading.append(el("span", "", label), el("strong", "", percent(bucket?.utilization)));
  const track = el("div", "meter");
  const fill = el("span", "meter-fill");
  fill.style.width = `${Math.min(100, Math.max(0, Number(bucket?.utilization) || 0))}%`;
  if (Number(bucket?.utilization) >= 90) fill.classList.add("hot");
  track.append(fill);
  wrap.append(heading, track, el("small", "muted", resetLabel(bucket?.resets_at)));
  if (Number.isFinite(ageSeconds) && ageSeconds > 45) wrap.append(el("small", "stale", `Stand vor ${Math.max(1, Math.round(ageSeconds / 60))} Min.`));
  return wrap;
}

function profileCard(profile) {
  const card = el("article", `account-card${profile.active ? " is-active" : ""}`);
  const top = el("div", "account-card-top");
  const identity = el("div", "account-identity");
  identity.append(el("h3", "", profile.name));
  identity.append(el("p", "muted", [profile.email, profile.subscriptionType, profile.organization].filter(Boolean).join(" · ")));
  const badge = el("span", `status-pill ${profile.active ? "good" : ""}`, profile.active ? "Aktiv" : "Gespeichert");
  top.append(identity, badge);
  card.append(top);

  const usage = profile.usage?.windows;
  const usageGrid = el("div", "usage-grid");
  if (usage) {
    usageGrid.append(createUsageBucket("5-Stunden-Fenster", usage.five_hour, profile.usage.ageSeconds));
    usageGrid.append(createUsageBucket("Wochenfenster", usage.seven_day, profile.usage.ageSeconds));
  } else {
    usageGrid.append(el("p", "usage-error", profile.usageError || "Auslastung noch nicht verfügbar. Aktualisieren lädt den Stand neu."));
  }
  card.append(usageGrid);

  const pace = paceLabel(profile);
  if (pace) card.append(el("p", "muted pace-line", pace));
  const metadata = el("div", "account-meta");
  metadata.append(el("span", "muted", `Zuletzt gesichert: ${timeLabel(profile.syncedAt || profile.savedAt)}`));
  if (profile.loginFailedAt) metadata.append(el("span", "warning-text", `Login zuletzt fehlgeschlagen: ${timeLabel(profile.loginFailedAt)}`));
  card.append(metadata);

  const actions = el("div", "account-actions");
  if (!profile.active) actions.append(actionButton("Wechseln und prüfen", "switch", "button", { name: profile.name }));
  if (!profile.active) actions.append(actionButton("Ohne Prüfung wechseln", "switch-raw", "button button-quiet", { name: profile.name }));
  const limits = profile.limits || {};
  const limitText = [limits.five_hour != null ? `5h ${percent(limits.five_hour)}` : "", limits.seven_day != null ? `7d ${percent(limits.seven_day)}` : "", limits.hard ? "hart" : ""].filter(Boolean).join(" · ");
  if (limitText) actions.append(el("span", "limit-summary", limitText));
  card.append(actions);
  return card;
}

function renderAccounts() {
  const host = $("#accounts");
  const profiles = dashboard?.profiles || [];
  host.replaceChildren();
  $("#account-count").textContent = `${profiles.length} ${profiles.length === 1 ? "Account" : "Accounts"}`;
  if (!profiles.length) {
    const empty = el("div", "empty-state");
    empty.append(el("h3", "", "Noch keine Accounts gespeichert"), el("p", "muted", "Speichere den aktuell angemeldeten Account oder melde einen weiteren an."));
    host.append(empty);
  } else {
    profiles.forEach((profile) => host.append(profileCard(profile)));
  }
  renderQuickSwitch(profiles);
  renderLimits(profiles);
}

function renderQuickSwitch(profiles) {
  const select = $("#quick-switch-target");
  const button = $("#quick-switch-button");
  const previous = select.value;
  select.replaceChildren(new Option("Account auswählen …", ""));
  for (const profile of profiles) {
    const option = new Option(`${profile.name}${profile.active ? " · aktiv" : ""}`, profile.name);
    option.dataset.active = String(profile.active);
    select.append(option);
  }
  const firstOther = profiles.find((profile) => !profile.active);
  if (profiles.some((profile) => profile.name === previous)) select.value = previous;
  else if (firstOther) select.value = firstOther.name;
  else if (profiles.length === 1) select.value = profiles[0].name;
  updateQuickSwitchButton();
}

function updateQuickSwitchButton() {
  const profiles = dashboard?.profiles || [];
  const select = $("#quick-switch-target");
  const button = $("#quick-switch-button");
  const selected = profiles.find((profile) => profile.name === select.value);
  button.disabled = !selected || selected.active;
  button.textContent = selected ? selected.active ? "Bereits aktiv" : "Jetzt wechseln" : profiles.length ? "Account auswählen" : "Kein Account gespeichert";
}

function renderLimits(profiles) {
  const host = $("#limits-list");
  host.replaceChildren();
  if (!profiles.length) {
    host.append(el("p", "muted", "Speichere zuerst einen Account."));
    return;
  }
  for (const profile of profiles) {
    const row = el("form", "limit-row");
    row.dataset.name = profile.name;
    const name = el("div", "limit-account");
    name.append(el("strong", "", profile.name), el("small", "muted", profile.email));
    const fiveWrap = el("div", "limit-input");
    fiveWrap.append(el("label", "", "5h-Grenze %"));
    const five = el("input"); five.type = "number"; five.min = "0"; five.max = "100"; five.step = "1"; five.name = "fiveHour"; five.value = profile.limits?.five_hour ?? ""; five.placeholder = "Global";
    fiveWrap.append(five);
    const sevenWrap = el("div", "limit-input");
    sevenWrap.append(el("label", "", "7d-Grenze %"));
    const seven = el("input"); seven.type = "number"; seven.min = "0"; seven.max = "100"; seven.step = "1"; seven.name = "sevenDay"; seven.value = profile.limits?.seven_day ?? ""; seven.placeholder = "Global";
    sevenWrap.append(seven);
    const hardWrap = el("label", "hard-limit");
    const hard = el("input"); hard.type = "checkbox"; hard.name = "hard"; hard.checked = Boolean(profile.limits?.hard);
    hardWrap.append(hard, el("span", "", "Hart"));
    const save = el("button", "button button-secondary", "Speichern"); save.type = "submit";
    const clear = actionButton("Grenzen löschen", "clear-limits", "text-button", { name: profile.name });
    row.append(name, fiveWrap, sevenWrap, hardWrap, save, clear);
    host.append(row);
  }
}

function fillConfig() {
  const config = dashboard?.config;
  if (!config) return;
  $("#auto-switch").checked = Boolean(config.auto_switch);
  $("#weekly-pacing").checked = config.weekly_pacing !== false;
  $("#threshold").value = config.threshold;
  $("#threshold-value").textContent = `${Math.round(config.threshold)}%`;
  $("#ping-enabled").checked = Boolean(config.ping?.enabled);
  $("#ping-prompt").value = config.ping?.prompt || "";
  $("#ping-model").value = config.ping?.model || "haiku";
  $("#ping-timeout").value = config.ping?.timeout_minutes || 5;
}

function renderService() {
  const service = dashboard?.service || {};
  const badge = $("#service-badge");
  badge.textContent = !service.installed ? "Nicht installiert" : service.active ? "Läuft" : "Gestoppt";
  badge.className = `status-pill ${service.active ? "good" : service.installed ? "" : "bad"}`;
  $("#service-note").textContent = service.installed
    ? `${service.enabled ? "Autostart ist eingeschaltet." : "Autostart ist ausgeschaltet."} Der Dienst nutzt die aktuellen Einstellungen.`
    : "Der Hintergrunddienst wurde auf diesem Rechner noch nicht eingerichtet.";
  for (const id of ["service-start", "service-stop", "service-restart", "service-enable", "service-disable"]) $("#" + id).disabled = !service.installed;
  $("#service-start").disabled = !service.installed || service.active;
  $("#service-stop").disabled = !service.installed || !service.active;
  $("#service-enable").disabled = !service.installed || service.enabled;
  $("#service-disable").disabled = !service.installed || !service.enabled;
}

function renderSessions() {
  const sessions = dashboard?.sessions || [];
  const host = $("#sessions");
  const select = $("#session-id");
  const previous = select.value;
  select.replaceChildren(new Option("Sitzung wählen …", ""));
  host.replaceChildren();
  if (!sessions.length) {
    host.append(el("p", "muted", "Keine Claude-Sitzungen gefunden."));
    return;
  }
  for (const session of sessions) {
    const option = new Option(`${session.title || "Sitzung"} · ${session.id.slice(0, 8)}`, session.id);
    option.dataset.cwd = session.cwd;
    select.append(option);
    const card = el("article", "session-card");
    card.append(el("strong", "", session.title || "Sitzung ohne Titel"));
    card.append(el("small", "muted", `${session.cwd} · ${timeLabel(session.modified)}`));
    const choose = actionButton("Fortsetzen auswählen", "choose-session", "text-button", { session: session.id });
    card.append(choose);
    host.append(card);
  }
  if ([...select.options].some((option) => option.value === previous)) select.value = previous;
}

function renderJobs() {
  const jobs = dashboard?.jobs || [];
  const host = $("#jobs");
  host.replaceChildren();
  $("#job-count").textContent = `${jobs.filter((job) => job.enabled).length} aktiv · ${jobs.length} gesamt`;
  if (!jobs.length) {
    host.append(el("p", "muted", "Die Warteschlange ist leer."));
    return;
  }
  for (const job of jobs) {
    const card = el("article", "job-card");
    const head = el("div", "job-head");
    const identity = el("div", "job-title");
    identity.append(el("h4", "", `[${job.id}] ${job.title}`));
    const kind = job.art === "resume" ? `Sitzung ${job.session_id}` : "Neuer Auftrag";
    identity.append(el("small", "muted", `${kind} · ${job.repeat ? "wiederholend" : "einmalig"} · ${job.enabled ? "aktiv" : "pausiert"}`));
    head.append(identity, el("span", `status-pill ${job.enabled ? "good" : ""}`, job.last_status || (job.enabled ? "In Warteschlange" : "Pausiert")));
    card.append(head);
    card.append(el("p", "job-prompt", job.text || ""));
    card.append(el("small", "muted", `${job.cwd}${job.model ? ` · ${job.model}` : ""} · ${job.timeout_minutes} Min.${job.skip_permissions ? " · ohne Berechtigungsfragen" : " · Berechtigungsfragen zulassen"}`));
    const actions = el("div", "job-actions");
    actions.append(actionButton("Jetzt ausführen", "run-job", "button button-secondary", { id: job.id }));
    actions.append(actionButton(job.enabled ? "Pausieren" : "Aktivieren", job.enabled ? "disable-job" : "enable-job", "button button-quiet", { id: job.id }));
    actions.append(actionButton("Protokoll", "job-log", "text-button", { id: job.id }));
    actions.append(actionButton("Löschen", "remove-job", "text-button danger-text", { id: job.id }));
    card.append(actions);
    const log = el("pre", "job-log"); log.hidden = true; log.dataset.id = job.id;
    card.append(log);
    host.append(card);
  }
}

function renderTasks() {
  const tasks = dashboard?.tasks || [];
  const host = $("#task-list");
  host.replaceChildren();
  if (!tasks.length) host.append(el("p", "muted", "Noch keine Aktionen ausgeführt."));
  for (const task of [...tasks].reverse()) {
    const row = el("div", "task-row");
    const info = el("div", ""); info.append(el("strong", "", task.action));
    if (task.message) info.append(el("small", task.status === "fehler" ? "error-text" : "muted", task.message));
    row.append(info, el("span", `status-pill ${task.status === "fertig" ? "good" : task.status === "fehler" ? "bad" : ""}`, task.status));
    host.append(row);
  }
}

function renderDeviceLogin() {
  const host = $("#login-device");
  const task = [...(dashboard?.tasks || [])].reverse().find((item) => item.action === "Login mit Device Code");
  if (!task) {
    host.hidden = true;
    return;
  }
  host.hidden = false;
  host.dataset.taskId = task.id;
  const active = task.status === "läuft";
  $("#login-device-status").textContent = task.message || "Claude-Code-Login wird gestartet …";
  const urlRow = $("#login-url-row");
  const link = $("#login-url");
  let safeUrl = "";
  try {
    const parsed = new URL(task.authUrl || "");
    if (parsed.protocol === "https:" && ["claude.com", "claude.ai"].includes(parsed.hostname) && parsed.pathname.includes("oauth/authorize")) safeUrl = parsed.href;
  } catch {}
  urlRow.hidden = !safeUrl;
  link.href = safeUrl || "#";
  link.textContent = safeUrl;
  $("#login-code-form").hidden = !active || !safeUrl;
  $("#login-code-submit").disabled = !active;
  $("#login-cancel").hidden = !active;
}

function renderStatus() {
  const status = $("#system-status");
  status.replaceChildren();
  const rows = [
    ["Angemeldet", dashboard?.loggedIn ? "Ja" : "Nein"],
    ["Aktives Profil", dashboard?.activeProfile || "Nicht zugeordnet"],
    ["Account-E-Mail", dashboard?.activeEmail || "–"],
    ["Automatischer Wechsel", dashboard?.config?.auto_switch ? "Ein" : "Aus"],
    ["Wechselgrenze", percent(dashboard?.config?.threshold)],
    ["Fenster-Ping", dashboard?.config?.ping?.enabled ? "Ein" : "Aus"],
    ["Hintergrunddienst", dashboard?.service?.active ? "Läuft" : "Gestoppt"],
  ];
  for (const [term, definition] of rows) { status.append(el("dt", "muted", term), el("dd", "", definition)); }
}

function render() {
  const active = dashboard?.activeProfile || (dashboard?.loggedIn ? "Profil nicht zugeordnet" : "Nicht eingeloggt");
  $("#active-heading").textContent = active;
  $("#active-email").textContent = dashboard?.activeEmail || "";
  $("#updated-at").textContent = `Stand ${new Date().toLocaleTimeString("de-DE", { hour: "2-digit", minute: "2-digit" })}`;
  renderAccounts(); if (!$("#config-form").contains(document.activeElement)) fillConfig(); renderService(); renderFlow(); renderSessions(); renderJobs(); renderTasks(); renderDeviceLogin(); renderStatus();
}

async function loadDashboard() {
  if (refreshing) return;
  refreshing = true;
  try {
    dashboard = await api("api/dashboard");
    render();
  } catch (error) {
    showNotice(error.message, true);
    $("#active-heading").textContent = "Status nicht verfügbar";
  } finally { refreshing = false; }
}

async function loadAutomationLogs() {
  try {
    $("#automation-logs").textContent = (await api("api/automation/logs")).logs || "Noch keine Protokolleinträge.";
  } catch (error) { $("#automation-logs").textContent = error.message; }
}

function selectTab(name) {
  document.querySelectorAll(".tab").forEach((tab) => tab.classList.toggle("active", tab.dataset.tab === name));
  document.querySelectorAll(".view").forEach((view) => {
    const isActive = view.id === `view-${name}`;
    view.classList.toggle("active", isActive);
    view.hidden = !isActive;
  });
  if (name === "system") loadAutomationLogs();
}

document.querySelectorAll(".tab").forEach((tab) => tab.addEventListener("click", () => selectTab(tab.dataset.tab)));
$("#refresh").addEventListener("click", () => { showNotice(""); loadDashboard(); });
$("#refresh-usage").addEventListener("click", () => run("api/actions/usage", {}));
$("#flow-enable").addEventListener("click", enableAutomation);
$("#flow-disable").addEventListener("click", disableAutoSwitch);
$("#threshold").addEventListener("input", (event) => { $("#threshold-value").textContent = `${event.target.value}%`; });

$("#save-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const name = $("#save-name").value.trim();
  if (!validName(name)) return showNotice("Profilnamen dürfen normale Buchstaben und Zeichen enthalten. Schrägstriche, Steuerzeichen und Rand-Leerzeichen sind nicht erlaubt, maximal 64 UTF-8-Bytes.", true);
  await run("api/accounts/save", { name });
  event.currentTarget.reset();
});

function selectedLoginMode() {
  return document.querySelector('input[name="login-mode"]:checked')?.value || "local";
}

function updateLoginCommand() {
  const name = $("#login-name").value.trim();
  const mode = selectedLoginMode();
  $("#login-command").textContent = validName(name) ? `claude-account login -- ${shellQuote(name)}` : "Profilname eingeben, um den Login-Befehl zu sehen";
  $("#login-command-row").hidden = mode !== "local";
  $("#login-start").textContent = mode === "device" ? "Device-Code-Login starten" : "Login starten";
  $("#login-mode-help").textContent = mode === "device"
    ? "Claude Code erzeugt eine Login-URL, ohne auf diesem Rechner einen Browser zu öffnen. Öffne die URL, melde dich an und füge anschließend den Code oder die Rückgabe-URL hier ein."
    : "Der offizielle Claude-Login wird auf diesem Rechner gestartet. Der Browser öffnet sich dort automatisch.";
}
$("#login-name").addEventListener("input", updateLoginCommand);
document.querySelectorAll('input[name="login-mode"]').forEach((input) => input.addEventListener("change", updateLoginCommand));
$("#login-start").addEventListener("click", async () => {
  const name = $("#login-name").value.trim();
  if (!validName(name)) return showNotice("Bitte einen gültigen Profilnamen eingeben. Schrägstriche, Steuerzeichen und Rand-Leerzeichen sind nicht erlaubt, maximal 64 UTF-8-Bytes.", true);
  const mode = selectedLoginMode();
  if (mode === "device") {
    try {
      const data = await api("api/accounts/login", { name, mode });
      showNotice("Device-Code-Login gestartet. Die URL erscheint hier.");
      await loadDashboard();
      return data;
    } catch (error) { showNotice(error.message, true); }
    return;
  }
  run("api/accounts/login", { name, mode });
});
$("#copy-login").addEventListener("click", async () => {
  const command = $("#login-command").textContent;
  if (!command.startsWith("claude-account login ")) return showNotice("Bitte zuerst einen gültigen Profilnamen eingeben.", true);
  try { await navigator.clipboard.writeText(command); showNotice("Login-Befehl kopiert. Auf diesem Rechner im Terminal ausführen."); }
  catch { showNotice("Kopieren nicht möglich. Den angezeigten Befehl kannst du markieren und kopieren.", true); }
});
$("#copy-login-url").addEventListener("click", async () => {
  const url = $("#login-url").href;
  if (!url || url.endsWith("#")) return;
  try { await navigator.clipboard.writeText(url); showNotice("Login-URL kopiert."); }
  catch { showNotice("Kopieren nicht möglich. Die angezeigte URL kannst du markieren und kopieren.", true); }
});
$("#login-code-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const code = $("#login-code").value.trim();
  const taskId = Number($("#login-device").dataset.taskId);
  if (!code) return showNotice("Bitte den Code oder die Rückgabe-URL einfügen.", true);
  $("#login-code-submit").disabled = true;
  try {
    const data = await api("api/accounts/login/complete", { taskId, code });
    $("#login-code").value = "";
    showNotice(data.message);
    await loadDashboard();
  } catch (error) { showNotice(error.message, true); }
  finally { $("#login-code-submit").disabled = false; }
});
$("#login-cancel").addEventListener("click", async () => {
  const taskId = Number($("#login-device").dataset.taskId);
  try {
    const data = await api("api/accounts/login/cancel", { taskId });
    showNotice(data.message);
    await loadDashboard();
  } catch (error) { showNotice(error.message, true); }
});

$("#accounts").addEventListener("click", (event) => {
  const button = event.target.closest("[data-action]"); if (!button) return;
  if (button.dataset.action === "switch") switchProfile(button.dataset.name);
  if (button.dataset.action === "switch-raw") switchProfile(button.dataset.name, true);
});
$("#quick-switch-button").addEventListener("click", () => {
  const select = $("#quick-switch-target");
  if (!select.value) return;
  const option = select.selectedOptions[0];
  if (option?.dataset.active !== "true") switchProfile(select.value);
});
$("#quick-switch-target").addEventListener("change", updateQuickSwitchButton);

$("#limits-list").addEventListener("submit", async (event) => {
  if (!event.target.matches("form.limit-row")) return;
  event.preventDefault();
  const form = event.target;
  const value = (name) => form.elements[name].value === "" ? null : Number(form.elements[name].value);
  const body = { name: form.dataset.name, fiveHour: value("fiveHour"), sevenDay: value("sevenDay"), hard: form.elements.hard.checked };
  if ([body.fiveHour, body.sevenDay].some((number) => number !== null && (number < 0 || number > 100))) return showNotice("Grenzen müssen zwischen 0 und 100 Prozent liegen.", true);
  await run("api/accounts/limits", body);
});
$("#limits-list").addEventListener("click", (event) => {
  const button = event.target.closest('[data-action="clear-limits"]');
  if (button && window.confirm(`Alle eigenen Grenzen für „${button.dataset.name}“ entfernen?`)) run("api/accounts/limits", { name: button.dataset.name, clear: true });
});

$("#config-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const config = {
    auto_switch: $("#auto-switch").checked,
    weekly_pacing: $("#weekly-pacing").checked,
    threshold: Number($("#threshold").value),
    ping: { enabled: $("#ping-enabled").checked, prompt: $("#ping-prompt").value, model: $("#ping-model").value.trim(), timeout_minutes: Number($("#ping-timeout").value) },
  };
  if (!config.ping.prompt.trim() || !config.ping.model || config.ping.timeout_minutes < 1) return showNotice("Ping-Text, Modell und ein Zeitlimit ab einer Minute werden benötigt.", true);
  run("api/config", config);
});

$("#auto-preview").addEventListener("click", () => run("api/actions/auto", { threshold: Number($("#threshold").value), dryRun: true }));
$("#auto-run").addEventListener("click", () => {
  if (window.confirm("Automatikentscheidung jetzt ausführen? Der aktive Claude-Account kann gewechselt werden.")) run("api/actions/auto", { threshold: Number($("#threshold").value), dryRun: false });
});
$("#sync-now").addEventListener("click", () => run("api/actions/sync", {}));
$("#keepalive-now").addEventListener("click", () => {
  const maxAgeDays = Number($("#keepalive-days").value);
  if (maxAgeDays < 1 || maxAgeDays > 365) return showNotice("Bitte zwischen 1 und 365 Tagen wählen.", true);
  run("api/actions/keepalive", { maxAgeDays });
});
$("#ping-now").addEventListener("click", () => {
  if (window.confirm("Fenster-Ping jetzt an Claude senden? Das kann das aktuelle Fünf-Stunden-Fenster öffnen.")) run("api/actions/ping", {});
});

async function serviceAction(action) {
  if (["stop", "restart", "disable"].includes(action) && !window.confirm(`Hintergrunddienst ${action === "disable" ? "Autostart ausschalten" : action === "restart" ? "neu starten" : "stoppen"}?`)) return;
  await run("api/service", { action });
}
for (const action of ["start", "stop", "restart", "enable", "disable"]) $("#service-" + (action === "enable" ? "enable" : action === "disable" ? "disable" : action)).addEventListener("click", () => serviceAction(action));

$("#job-kind").addEventListener("change", (event) => {
  const resume = event.target.value === "resume";
  $("#session-picker").hidden = !resume;
  $("#job-text").placeholder = resume ? "Anweisung für Claude, oder leer lassen für die Standardfortsetzung." : "Was soll Claude erledigen?";
});
$("#session-id").addEventListener("change", (event) => {
  const option = event.target.selectedOptions[0];
  if (option?.dataset.cwd) $("#job-cwd").value = option.dataset.cwd;
  if ($("#job-kind").value === "resume" && !$("#job-text").value.trim()) $("#job-text").value = "Mach da weiter, wo du aufgehört hast.";
});
$("#sessions").addEventListener("click", (event) => {
  const button = event.target.closest('[data-action="choose-session"]'); if (!button) return;
  $("#job-kind").value = "resume"; $("#job-kind").dispatchEvent(new Event("change"));
  $("#session-id").value = button.dataset.session; $("#session-id").dispatchEvent(new Event("change"));
  selectTab("jobs"); $("#job-text").focus();
});
$("#job-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const kind = $("#job-kind").value;
  const sessionId = $("#session-id").value;
  if (kind === "resume" && !sessionId) return showNotice("Bitte die Sitzung auswählen, die fortgesetzt werden soll.", true);
  const body = {
    kind, sessionId: kind === "resume" ? sessionId : null,
    text: $("#job-text").value.trim() || (kind === "resume" ? "Mach da weiter, wo du aufgehört hast." : ""),
    cwd: $("#job-cwd").value.trim(), title: $("#job-title").value.trim() || null,
    settings: $("#job-settings").value.trim() || null, model: $("#job-model").value.trim() || null,
    timeoutMinutes: $("#job-timeout").value ? Number($("#job-timeout").value) : null,
    allowPermissions: $("#job-permissions").checked, repeat: $("#job-repeat").checked,
  };
  await run("api/jobs", body);
  event.currentTarget.reset(); $("#session-picker").hidden = true;
});
$("#jobs").addEventListener("click", async (event) => {
  const button = event.target.closest("[data-action]"); if (!button) return;
  const { action, id } = button.dataset;
  if (action === "remove-job" && !window.confirm(`Aufgabe [${id}] löschen?`)) return;
  if (action === "run-job" && !window.confirm(`Aufgabe [${id}] sofort ausführen? Das nutzt das aktive Claude-Konto.`)) return;
  if (action === "job-log") {
    const pre = button.closest(".job-card").querySelector(".job-log");
    if (!pre.hidden) { pre.hidden = true; return; }
    try { pre.textContent = (await api(`api/jobs/${encodeURIComponent(id)}/log`)).log || "Noch kein Protokoll vorhanden."; pre.hidden = false; }
    catch (error) { pre.textContent = error.message; pre.hidden = false; }
    return;
  }
  const endpoint = { "run-job": "run", "enable-job": "enable", "disable-job": "disable", "remove-job": "remove" }[action];
  if (endpoint) run(`api/jobs/${encodeURIComponent(id)}/${endpoint}`, {});
});

$("#refresh-logs").addEventListener("click", loadAutomationLogs);
updateLoginCommand();
loadDashboard();
function scheduleDashboardRefresh() {
  const taskRunning = dashboard?.tasks?.some((task) => task.status === "läuft");
  window.setTimeout(async () => {
    if (!document.hidden) await loadDashboard();
    scheduleDashboardRefresh();
  }, taskRunning ? 1000 : 10000);
}
scheduleDashboardRefresh();
