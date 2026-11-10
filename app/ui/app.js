// Forever Launcher UI. Everything privileged happens in Rust (src-tauri); this file only calls its commands.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);

let state = null;      // snapshot from get_state
let draft = null;      // settings being edited in the drawer
let toastTimer = 0;
let clientStatus = null;
let updating = false;
let launcherUpdate = null;
let launcherUpdating = false;
let setupReady = false;
let clientReady = true;

function toast(text) {
  const t = $('toast');
  t.textContent = text;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 4500);
}

function log(line) {
  const pre = $('log');
  const time = new Date().toLocaleTimeString(lang);
  pre.textContent += `[${time}] ${line}\n`;
  pre.scrollTop = pre.scrollHeight;
}

function setStep(text, kind = '') {
  const s = $('step');
  s.textContent = text;
  s.className = 'step' + (kind ? ' ' + kind : '');
}

function formatBytes(bytes) {
  const units = ['Б', 'КиБ', 'МиБ', 'ГиБ', 'ТиБ'];
  let value = Number(bytes) || 0;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(index === 0 || value >= 10 ? 0 : 1)} ${units[index]}`;
}

async function refreshClientStatus() {
  if (!state || !state.gameFound) {
    clientReady = false;
    $('client-state').textContent = t('clientNotInstalled');
    $('client-notice').hidden = true;
    updatePlayButton();
    return;
  }
  try {
    clientStatus = await invoke('client_status');
    clientReady = clientStatus.upToDate;
    updatePlayButton();
    if (updating) return;
    if (clientStatus.upToDate) {
      $('client-state').textContent = t('clientUpToDate');
      $('client-notice').hidden = true;
    } else {
      $('client-state').textContent = t('clientUpdateAvailable', {
        files: clientStatus.changedFiles,
        size: formatBytes(clientStatus.changedBytes),
      });
      $('client-notice').textContent = t('clientNotice');
      $('client-notice').hidden = false;
    }
  } catch (e) {
    // Не блокируем игру только из-за недоступного манифеста: сервер мог
    // временно не ответить, а клиент при этом рабочий.
    $('client-state').textContent = t('updateError', { e });
    $('client-notice').hidden = true;
    updatePlayButton();
  }
}

function handleUpdateEvent(payload) {
  const progressKey = 'clientProgress';
  const startKey = 'clientStarting';
  const doneKey = 'clientDone';
  if (payload.kind === 'start') {
    $('update-progress').hidden = false;
    $('update-bar').style.width = '0%';
    $('update-detail').textContent = t(startKey);
    $('client-notice').textContent = t(startKey);
    $('client-notice').hidden = false;
    return;
  }
  if (payload.kind === 'progress') {
    const percent = Math.max(0, Math.min(100, Number(payload.percent) || 0));
    $('update-progress').hidden = false;
    $('update-bar').style.width = `${percent}%`;
    const file = payload.file ? ` · ${String(payload.file).split('/').pop()}` : '';
    const detail = t(progressKey, { current: payload.current, total: payload.total, percent: percent.toFixed(1) }) + file;
    $('update-detail').textContent = detail;
    $('client-notice').textContent = detail;
    $('client-notice').hidden = false;
    return;
  }
  if (payload.kind === 'done') {
    $('update-bar').style.width = '100%';
    $('update-detail').textContent = t(doneKey);
    $('client-notice').textContent = t(doneKey);
    $('client-notice').hidden = false;
    toast(t(doneKey));
    updating = false;
    updatePlayButton();
    refreshClientStatus();
    return;
  }
  if (payload.kind === 'error') {
    const message = t('updateError', { e: payload.message });
    $('update-detail').textContent = message;
    $('client-notice').textContent = message;
    $('client-notice').hidden = false;
    toast(message);
    updating = false;
    updatePlayButton();
  }
}

async function installClient() {
  if (updating) return;
  const selected = $('s-game-dir').value.trim() || state.settings.gameDir;
  if (!selected) {
    toast(t('chooseGameDir'));
    return;
  }
  updating = true;
  updatePlayButton();
  $('update-progress').hidden = false;
  $('update-bar').style.width = '0%';
  $('update-detail').textContent = t('clientStarting');
  try {
    await invoke('install_client', { path: selected });
    await refresh();
    $('s-game-dir').value = state.settings.gameDir;
    gameDirState();
    refreshChecks();
    refreshClientStatus();
    toast(t('clientDone'));
  } catch (e) {
    $('update-detail').textContent = t('updateError', { e });
    toast(t('updateError', { e }));
  } finally {
    updating = false;
    updatePlayButton();
  }
}

async function runUpdate(prefix, command, startKey) {
  if (updating) return;
  updating = true;
  updatePlayButton();
  $('update-progress').hidden = false;
  $('update-bar').style.width = '0%';
  $('update-detail').textContent = t(startKey);
  try {
    await invoke(command);
  } catch (e) {
    $('update-detail').textContent = t('updateError', { e });
    toast(t('updateError', { e }));
  } finally {
    updating = false;
    updatePlayButton();
    refreshClientStatus();
  }
}

// ------------------------------------------------------------------------------------------------ state
// '' follows the system language; Rust gets the same language for its messages
function setLanguage(setting) {
  const code = resolveLanguage(setting);
  if (code === lang && document.documentElement.lang === code) return;
  applyLanguage(code);
  if (state) updatePlayButton();
  invoke('set_language', { language: code });
}

async function refresh() {
  state = await invoke('get_state');
  setLanguage(state.settings.language);
  $('realm-field').textContent = t('realm', { name: state.settings.server.name || 'Дримворлд Навсегда' });
  $('version').textContent = `v${state.version} · ${{ windows: 'Windows', macos: 'macOS', linux: 'Linux' }[state.platform]}`;
  $('signup').hidden = !state.settings.server.website;
  const logged = !!state.email;
  $('view-login').hidden = logged;
  $('view-play').hidden = !logged;
  $('account').hidden = !logged;
  $('game-field').textContent = `WoW Forever ${state.gameVersion || '1.60.1.70862'}`;
  if (logged) {
    $('account-email').textContent = state.email;
    $('avatar').textContent = state.email[0].toUpperCase();
  }
  updatePlayButton();
  refreshClientStatus();
}

function updatePlayButton() {
  const play = $('play');
  const hasClient = !!state.gameFound;
  play.textContent = state.playing
    ? t('inGame')
    : updating || launcherUpdating
      ? t('working')
      : !hasClient
        ? t('playInstall')
        : !clientReady
          ? t('playUpdate')
          : t('play');
  play.disabled =
    state.playing ||
    updating ||
    launcherUpdating ||
    (hasClient && clientReady && !setupReady);
  play.classList.toggle('running', state.playing);
  if (!hasClient && !state.playing && !updating) setStep(t('clientNotInstalled'), '');
  else if (hasClient && !clientReady && !updating) setStep(t('clientUpdateShort'), '');
  else if (hasClient && clientReady && !state.playing && !updating) setStep('', '');
}

async function refreshStatus() {
  const pill = $('status');
  try {
    const s = await invoke('server_status');
    pill.dataset.state = s.online && s.login ? 'online' : s.online || s.login ? 'partial' : 'offline';
    $('status-text').textContent = t(s.online && s.login ? 'online' : s.online || s.login ? 'starting' : 'offline');
  } catch {
    pill.dataset.state = 'offline';
    $('status-text').textContent = t('unreachable');
  }
}

async function refreshChecks() {
  try {
    const c = await invoke('checks');
    showCheck('check-hosts', c.hosts, c.hostsDetail);
    showCheck('check-cert', c.certificate, c.certificateDetail);
    // on Linux the prefix certificate is installed automatically at launch: only the hosts entry needs the user
    const needsCert = state.platform !== 'linux' && !c.certificate;
    const blocking = !c.hosts || needsCert;
    setupReady = !blocking;
    updatePlayButton();
    $('setup-notice').hidden = !blocking;
    $('setup-detail').textContent = [!c.hosts && c.hostsDetail, needsCert && c.certificateDetail]
      .filter(Boolean).join(' · ');
    return c;
  } catch (e) {
    log(t('checks', { e }));
  }
}

function showCheck(id, ok, detail) {
  const el = $(id);
  el.dataset.ok = String(ok);
  el.querySelector('p').textContent = detail;
}

async function repair(btn) {
  btn.disabled = true;
  const label = btn.textContent;
  btn.textContent = t('working');
  try {
    const c = await invoke('repair');
    showCheck('check-hosts', c.hosts, c.hostsDetail);
    showCheck('check-cert', c.certificate, c.certificateDetail);
    toast(c.hosts ? t('pcReady') : t('partialRepair'));
  } catch (e) {
    toast(t('failed', { e }));
  } finally {
    btn.disabled = false;
    btn.textContent = label;
    refreshChecks();
  }
}

// ------------------------------------------------------------------------------------------------ login
$('login-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const btn = $('login-btn');
  const err = $('login-error');
  err.hidden = true;
  btn.disabled = true;
  btn.textContent = t('loggingIn');
  try {
    await invoke('login', { email: $('email').value, password: $('password').value });
    $('password').value = '';
    await refresh();
    refreshChecks();
  } catch (e) {
    err.textContent = String(e);
    err.hidden = false;
  } finally {
    btn.disabled = false;
    btn.textContent = t('loginButton');
  }
});

$('logout').addEventListener('click', async () => {
  await invoke('logout');
  await refresh();
});

$('signup-link').addEventListener('click', (ev) => {
  ev.preventDefault();
  window.__TAURI__.opener.openUrl(state.settings.server.website);
});

// ------------------------------------------------------------------------------------------------ play
$('play').addEventListener('click', async () => {
  if (updating || launcherUpdating) return;
  if (!state.gameFound) {
    await installClient();
    return;
  }
  if (!clientReady) {
    await runUpdate('client', 'update_client', 'clientStarting');
    return;
  }
  $('log').textContent = '';
  state.playing = true;
  updatePlayButton();
  setStep(t('preparing'));
  try {
    await invoke('play');
  } catch (e) {
    state.playing = false;
    updatePlayButton();
    setStep(String(e), 'err');
    log(t('error', { e }));
    refresh();   // an expired session sends back to the login view
  }
});

$('setup-fix').addEventListener('click', (ev) => repair(ev.currentTarget));
$('toggle-log').addEventListener('click', (ev) => {
  ev.preventDefault();
  $('log-box').hidden = !$('log-box').hidden;
});
$('close-log').addEventListener('click', () => ($('log-box').hidden = true));
$('repair').addEventListener('click', (ev) => repair(ev.currentTarget));

listen('game', ({ payload }) => {
  const { kind, message } = payload;
  if (kind === 'log') return log(message);
  log(message);
  if (kind === 'step' || kind === 'running') setStep(message);
  if (kind === 'ready') setStep(message, 'ok');
  if (kind === 'error') setStep(message, 'err');
  if (kind === 'closed' || kind === 'error') {
    state.playing = false;
    updatePlayButton();
    if (kind === 'closed') setStep('');
  }
});

// ------------------------------------------------------------------------------------------------ settings
function option(select, value, label) {
  const o = document.createElement('option');
  o.value = value;
  o.textContent = label;
  select.append(o);
}

function fillSettings() {
  draft = structuredClone(state.settings);
  $('s-game-dir').value = draft.gameDir;
  const loc = $('s-locale');
  loc.replaceChildren();
  state.locales.forEach((l) => option(loc, l, l));
  loc.value = draft.locale;
  $('linux-settings').hidden = state.platform !== 'linux';
  if (state.platform === 'linux') {
    const r = $('s-runner');
    r.replaceChildren();
    state.runners.forEach(([name, path]) => option(r, path, name));
    if (![...r.options].some((o) => o.value === draft.linux.runner) && draft.linux.runner) option(r, draft.linux.runner, draft.linux.runner);
    r.value = draft.linux.runner;
    $('s-prefix').value = draft.linux.prefix;
    $('s-gamescope').checked = draft.linux.gamescope;
    $('s-gamescope-args').value = draft.linux.gamescopeArgs;
    $('s-keyboard').value = draft.linux.keyboard;
  }
  const language = $('s-language');
  language.replaceChildren();
  option(language, '', t('systemLanguage'));
  Object.entries(LANGUAGES).forEach(([code, name]) => option(language, code, name));
  language.value = draft.language || '';
  gameDirState();
}

function gameDirState() {
  const el = $('game-dir-state');
  const dir = $('s-game-dir').value.trim();
  const same = dir === state.settings.gameDir;
  const game = state.platform === 'macos' ? t('gameApp') : 'Wow.exe';
  el.textContent = !dir ? t('noFolder') : same && state.gameFound ? t('gameFound', { game }) : same ? t('gameMissing', { game }) : '';
  el.className = 'small ' + (same && state.gameFound ? 'ok' : dir && same ? 'ko' : '');
}

function openSettings() {
  fillSettings();
  refreshClientStatus();
  $('settings').hidden = false;
  $('scrim').hidden = false;
  refreshChecks();
}

function closeSettings() {
  $('settings').hidden = true;
  $('scrim').hidden = true;
}

async function pick(input, title) {
  const dir = await window.__TAURI__.dialog.open({ directory: true, title, defaultPath: input.value || undefined });
  if (dir) {
    input.value = dir;
    if (input.id === 's-game-dir') {
      const loc = await invoke('detect_locale', { gameDir: dir });
      if (loc) $('s-locale').value = loc;
    }
  }
}

$('open-settings').addEventListener('click', openSettings);
$('client-notice').addEventListener('click', openSettings);
$('play-settings').addEventListener('click', openSettings);
$('close-settings').addEventListener('click', closeSettings);
$('cancel-settings').addEventListener('click', closeSettings);
$('scrim').addEventListener('click', closeSettings);
$('pick-game').addEventListener('click', () => pick($('s-game-dir'), t('pickGameDir')));
$('pick-prefix').addEventListener('click', () => pick($('s-prefix'), t('pickPrefix')));
$('s-game-dir').addEventListener('input', gameDirState);
document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && !$('settings').hidden) closeSettings(); });

$('check-client').addEventListener('click', async () => {
  await refreshClientStatus();
  if (clientStatus && clientStatus.upToDate) toast(t('clientUpToDate'));
});
$('update-client').addEventListener('click', () => runUpdate('client', 'update_client', 'clientStarting'));
$('launcher-update-btn').addEventListener('click', installLauncherUpdate);
listen('client', ({ payload }) => handleUpdateEvent(payload));
listen('launcher-update', ({ payload }) => handleLauncherUpdateEvent(payload));

$('save-settings').addEventListener('click', async () => {
  draft.gameDir = $('s-game-dir').value.trim();
  draft.locale = $('s-locale').value;
  draft.language = $('s-language').value;
  if (state.platform === 'linux') {
    draft.linux.runner = $('s-runner').value;
    draft.linux.prefix = $('s-prefix').value.trim();
    draft.linux.gamescope = $('s-gamescope').checked;
    draft.linux.gamescopeArgs = $('s-gamescope-args').value.trim();
    draft.linux.keyboard = $('s-keyboard').value.trim();
  }
  try {
    await invoke('save_settings', { settings: draft });
    closeSettings();
    await refresh();
    refreshStatus();
    refreshChecks();
    refreshNews();
    refreshLauncherUpdate();
    toast(t('saved'));
  } catch (e) {
    toast(t('saveFailed', { e }));
  }
});

// ------------------------------------------------------------------------------------------------ news
// shown until the website has a blog (<website>/api/news)
const defaultNews = () => [1, 2, 3].map((i) => ({ tag: t(`news${i}Tag`), title: t(`news${i}`), image: '', url: 'https://t.me/amatol_blog' }));

function openLink(url) {
  if (url) window.__TAURI__.opener.openUrl(url);
}

async function refreshNews() {
  let items = [];
  try {
    items = await invoke('news');
  } catch (e) {
    console.debug('news:', e);
  }
  const fromSite = items.length > 0;
  if (!fromSite) items = defaultNews();
  $('news-more').hidden = !fromSite;
  $('news-more').onclick = (ev) => { ev.preventDefault(); openLink('https://t.me/amatol_blog'); };
  $('news').replaceChildren(...items.slice(0, 3).map((n) => {
    const card = document.createElement('button');
    card.className = 'news-card';
    card.type = 'button';
    // a drawn placeholder when the post has no image (or it does not load)
    const thumb = () => Object.assign(document.createElement('div'), { className: 'thumb', textContent: 'F' });
    let img = thumb();
    if (n.image) {
      img = document.createElement('img');
      img.src = n.image;
      img.alt = '';
      img.onerror = () => img.replaceWith(thumb());
    }
    const body = document.createElement('div');
    body.className = 'body';
    const tag = document.createElement('span');
    tag.className = 'tag';
    tag.textContent = n.tag || t('newsTag');
    const title = document.createElement('strong');
    title.textContent = n.title;
    body.append(tag, title);
    if (n.date) {
      const date = document.createElement('span');
      date.className = 'date';
      date.textContent = n.date;
      body.append(date);
    }
    card.append(img, body);
    card.addEventListener('click', () => openLink(n.url || 'https://t.me/amatol_blog'));
    return card;
  }));
}

// ------------------------------------------------------------------------------------------------ background
function rotateBackground() {
  const slides = [...document.querySelectorAll('.backdrop .bg')];
  let current = Math.floor(Math.random() * slides.length);
  slides[current].classList.add('on');
  setInterval(() => {
    slides[current].classList.remove('on');
    current = (current + 1) % slides.length;
    slides[current].classList.add('on');
  }, 20000);
}

// ------------------------------------------------------------------------------------------------ start
rotateBackground();
(async () => {
  await refresh();
  refreshStatus();
  refreshNews();
  refreshLauncherUpdate();
  if (state.email) refreshChecks();
  setInterval(refreshStatus, 20000);
})();
