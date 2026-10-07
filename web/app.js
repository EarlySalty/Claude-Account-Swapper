'use strict';
const $ = (selector) => document.querySelector(selector);
const csrf = $('meta[name="account-csrf"]').content;
let initialized = false;
let latest;
let loading = false;
const labels = {starting:'Startet',running:'Arbeitet',idle:'Bereit',background:'Hintergrundaufgaben',waiting:'Wartet',stopped:'Beendet',error:'Fehler'};
function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (className) node.className = className;
  return node;
}
function resetAt(bucket) {
  return bucket?.resets_at ? 'Reset: ' + new Date(bucket.resets_at).toLocaleString('de-DE',{weekday:'short',hour:'2-digit',minute:'2-digit'}) : 'Noch kein laufendes Fenster';
}
function accountSelect(accounts, selected = '') {
  const select = element('select');
  const auto = element('option','Automatisch / Projektregel'); auto.value = ''; select.append(auto);
  for (const account of accounts) {
    const option = element('option', account.name); option.value = account.name; select.append(option);
  }
  select.value = selected;
  return select;
}
async function action(payload) {
  $('#error').textContent = '';
  const response = await fetch('./api/action',{method:'POST',headers:{'Content-Type':'application/json','X-Account-CSRF':csrf},body:JSON.stringify(payload)});
  const data = await response.json();
  if (!response.ok) throw new Error(data.error || 'Änderung konnte nicht gespeichert werden');
  await load();
}
function fail(error) { $('#error').textContent = error.message || String(error); }
function meter(title, bucket, threshold) {
  const container = element('div');
  const head = element('div',undefined,'meter-head');
  head.append(element('span',title),element('span',bucket ? bucket.utilization.toFixed(1) + ' %' : 'Unbekannt'));
  container.append(head);
  if (bucket) {
    const progress = element('progress'); progress.max=100; progress.value=Math.min(100,bucket.utilization);
    progress.setAttribute('aria-label',title);
    if (bucket.utilization >= threshold) progress.classList.add('full');
    container.append(progress,element('p',resetAt(bucket)));
  }
  return container;
}
function render(data) {
  latest = data;
  $('#connection').textContent = 'Verbunden · ' + new Date().toLocaleTimeString('de-DE');
  $('#threshold').textContent = 'Wechsel bei ' + data.threshold + ' % · 5h';
  const cards = $('#accounts'); cards.replaceChildren();
  for (const account of data.accounts) {
    const disabled = data.routing.disabled.includes(account.name);
    const card = element('article',undefined,'account' + (disabled ? ' disabled' : ''));
    const head = element('div',undefined,'account-head');
    const toggle = element('button',disabled ? 'Aktivieren' : 'Pausieren'); toggle.type='button';
    toggle.onclick = async () => { toggle.disabled=true; try { await action({action:'account',name:account.name,enabled:disabled}); } catch(error) { fail(error); toggle.disabled=false; } };
    head.append(element('h3',account.name),toggle); card.append(head);
    card.append(element('p', account.busy ? 'Von einer Session belegt' : data.routing.homes[account.name] ? 'Native Session-Konfiguration verbunden' : 'Native Anmeldung fehlt'));
    card.append(meter('5-Stunden-Fenster',account.usage?.five_hour,data.threshold),meter('Wochenfenster',account.usage?.seven_day,data.routing.weekly_threshold));
    for (const [name,bucket] of Object.entries(account.models || {})) card.append(meter(name,bucket,data.routing.weekly_threshold));
    if (account.fetched_at) card.append(element('p','Messung vor ' + Math.max(0,Math.floor(data.now-account.fetched_at)) + ' Sekunden · ' + account.sessions + ' Sessions'));
    if (account.problem) card.append(element('p',account.problem,'problem'));
    cards.append(card);
  }
  if (!data.accounts.length) cards.append(element('p','Noch keine Accounts gespeichert. Im Terminal: claude-account save <name>','empty'));
  if (!initialized) {
    const form = $('#settings');
    for (const name of ['strategy','weekly_threshold','cooldown_seconds','hysteresis']) form.elements[name].value=data.routing[name];
    form.elements.threshold.value=data.threshold; form.elements.auto_switch.checked=data.routing.auto_switch;
    initialized=true;
  }
  const oldValue=$('#project-account').value;
  const select=accountSelect(data.accounts,oldValue); select.id='project-account'; select.name='account';
  $('#project-account').replaceWith(select);
  const projects=$('#projects'); projects.replaceChildren();
  for (const [path,rule] of Object.entries(data.routing.projects)) {
    const row=element('div',undefined,'row'); const main=element('div',undefined,'row-main');
    main.append(element('strong',path),element('p',rule.account || (rule.pool.length ? 'Pool: '+rule.pool.join(', ') : 'Automatisch') ));
    const remove=element('button','Zuordnung entfernen'); remove.type='button';
    remove.onclick=()=>action({action:'project',path,rule:null}).catch(fail); row.append(main,remove); projects.append(row);
  }
  const sessions=$('#sessions'); sessions.replaceChildren();
  $('#session-count').textContent=data.sessions.filter(session=>session.state!=='stopped').length+' aktiv';
  for (const session of data.sessions) {
    const row=element('div',undefined,'row'); const main=element('div',undefined,'row-main');
    main.append(element('strong',session.cwd),element('p',(session.kind==='t3'?'T3-Launcher':'Terminal')+' · '+session.id),element('p',session.reason));
    const state=element('span',labels[session.state] || session.state,'state '+session.state);
    const controls=element('div',undefined,'row-controls');
    const rule=data.routing.sessions[session.id]; const choice=accountSelect(data.accounts,rule?.account || '');
    choice.setAttribute('aria-label','Account für Session '+session.id);
    choice.disabled = session.kind !== 't3' || session.state === 'stopped';
    choice.onchange=()=>action({action:'session',id:session.id,rule:choice.value ? {account:choice.value} : null}).catch(fail);
    controls.append(choice,element('p','Aktuell: '+(session.account || 'noch keiner'),'muted'));
    if (session.kind !== 't3') controls.append(element('p','Terminal: Account bis zum Ende fest','muted'));
    if (rule?.account && rule.account!==session.account && session.state!=='stopped') controls.append(element('p','Wechsel beim nächsten sicheren Start','muted'));
    row.append(main,state,controls); sessions.append(row);
  }
  if (!data.sessions.length) sessions.append(element('p','Noch keine verwalteten Sessions. Einen Account mit claude-account run starten oder den T3-Launcher einrichten.','empty'));
}
async function load() {
  if (loading) return;
  loading=true;
  try {
    const response=await fetch('./api/state',{cache:'no-store'});
    const data=await response.json();
    if (!response.ok) throw new Error(data.error || 'Status nicht verfügbar');
    render(data);
  } catch(error) { $('#connection').textContent='Verbindung unterbrochen'; fail(error); }
  finally { loading=false; }
}
$('#settings').onsubmit=async event=>{
  event.preventDefault(); const form=event.currentTarget; const button=form.querySelector('button'); button.disabled=true;
  try {
    await action({action:'settings',strategy:form.elements.strategy.value,auto_switch:form.elements.auto_switch.checked,threshold:Number(form.elements.threshold.value),weekly_threshold:Number(form.elements.weekly_threshold.value),cooldown_seconds:Number(form.elements.cooldown_seconds.value),hysteresis:Number(form.elements.hysteresis.value)});
  } catch(error) { fail(error); } finally { button.disabled=false; }
};
$('#project').onsubmit=async event=>{
  event.preventDefault(); const form=event.currentTarget;
  const account=form.elements.account.value; const pool=form.elements.pool.value.split(',').map(value=>value.trim()).filter(Boolean);
  if (account && pool.length) { fail(new Error('Entweder einen festen Account oder einen Pool wählen.')); return; }
  try { await action({action:'project',path:form.elements.path.value,rule:{account:account || null,pool,model:form.elements.model.value.trim() || null}}); }
  catch(error) { fail(error); }
};
$('#refresh').onclick=load;
document.addEventListener('visibilitychange',()=>{if(!document.hidden) load();});
setInterval(()=>{if(!document.hidden) load();},5000);
load();
