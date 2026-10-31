// Config
const C = window.TuneContrat;
const invoke = (cmd, args) => window.__TAURI__?.core?.invoke(cmd, args);

// L'adresse du serveur : celle que l'utilisateur a tapée ici (localStorage),
// sinon celle du côté natif (config.json), sinon la machine
// elle-même. L'ancien défaut était une adresse du réseau de développement.
let SERVER = C.normaliserServeur(localStorage.getItem('tune-server')) || C.SERVEUR_PAR_DEFAUT;
let state = { playing: false, zone_id: 0, duration_ms: 0, position_ms: 0 };
// Loi du curseur de chaque zone (`volume_curve` de GET /zones, #6787).
const courbes = new Map();
const courbe = () => courbes.get(state.zone_id) || 'linear';
let searchTimeout = null;

const $ = (id) => document.getElementById(id);

function formatTime(ms) {
  const s = Math.floor(ms / 1000);
  const m = Math.floor(s / 60);
  return `${m}:${String(s % 60).padStart(2, '0')}`;
}

function coverUrl(path) {
  return C.urlDePochette(SERVER, path);
}

function updateNowPlaying(data) {
  const np = data.now_playing || data;
  $('track-title').textContent = np.title || '—';
  $('track-artist').textContent = [np.artist_name, np.album_title].filter(Boolean).join(' — ') || '—';
  state.duration_ms = np.duration_ms || 0;
  $('time-total').textContent = formatTime(state.duration_ms);

  const url = coverUrl(np.cover_path);
  const img = $('cover-art');
  if (url) {
    img.src = url;
    img.classList.add('loaded');
  } else {
    img.classList.remove('loaded');
  }
}

function updateState(playing) {
  state.playing = playing;
  $('icon-play').style.display = playing ? 'none' : 'block';
  $('icon-pause').style.display = playing ? 'block' : 'none';
}

function updatePosition(ms) {
  state.position_ms = ms;
  $('time-current').textContent = formatTime(ms);
  if (state.duration_ms > 0) $('progress-bar').value = (ms / state.duration_ms) * 100;
}

let volumeLocalUntil = 0;

// `vol` est le gain linéaire 0..1 que publie le serveur ; le curseur affiche
// sa position selon la loi de la zone.
function updateVolume(vol) {
  if (Date.now() < volumeLocalUntil) return;
  const v = C.positionDuCurseur(vol, courbe());
  $('volume-slider').value = v;
  $('volume-value').textContent = v;
}

function setConnected(ok) {
  $('server-status').className = ok ? '' : 'offline';
  $('server-label').textContent = ok ? 'Connecté' : 'Déconnecté';
}

async function apiGet(path) {
  const r = await fetch(`${SERVER}${path}`);
  if (!r.ok) throw new Error(r.status);
  return r.json();
}

async function apiPost(path) {
  await fetch(`${SERVER}${path}`, { method: 'POST' });
}

async function apiPut(path, body) {
  await fetch(`${SERVER}${path}`, { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
}

function applyZoneData(data) {
  const zone = (data.zones || []).find(z => z.zone_id === state.zone_id);
  if (zone) {
    const np = zone.now_playing;
    if (np && np.title && np.title !== 'Recovering...') {
      updateNowPlaying(zone);
    }
    updateState(zone.state === 'playing');
    updatePosition(zone.position_ms || 0);
    if (typeof zone.volume === 'number') updateVolume(zone.volume);
  }
  setConnected(true);
}

async function refresh() {
  try {
    const data = await apiGet('/api/v1/widget/data');
    applyZoneData(data);
  } catch {
    setConnected(false);
  }
}

// `zoneChoisie` : la zone que l'utilisateur vient de choisir dans le menu.
// Sans elle, init() reprenait la zone qui joue et le menu revenait aussitôt
// en arrière.
async function init(zoneChoisie) {
  try {
    const data = await apiGet('/api/v1/widget/data');
    // First load: pick the best zone
    const playing = (data.zones || []).find(z => {
      if (z.state !== 'playing') return false;
      const np = z.now_playing;
      if (!np) return false;
      if (!np.track_id && (!np.title || np.title === 'Recovering...')) return false;
      return true;
    });
    if (Number.isInteger(zoneChoisie)) {
      state.zone_id = zoneChoisie;
    } else if (playing) {
      state.zone_id = playing.zone_id;
    } else {
      // Pick first paused zone, or first zone
      const paused = (data.zones || []).find(z => z.state === 'paused');
      state.zone_id = paused ? paused.zone_id : ((data.zones && data.zones[0]) ? data.zones[0].zone_id : 1);
    }
    // Les raccourcis globaux (côté natif) visent la zone affichée ici.
    invoke('select_zone', { zoneId: state.zone_id })?.catch?.(() => {});
    applyZoneData(data);
  } catch (e) {
    console.error('init:', e);
    setConnected(false);
  }

  try {
    const zones = C.listeDeZones(await apiGet('/api/v1/zones'));
    const sel = $('zone-select');
    sel.innerHTML = '';
    zones.forEach(z => {
      courbes.set(z.id, z.volume_curve === 'db60' ? 'db60' : 'linear');
      if (z.id === state.zone_id && typeof z.volume === 'number') updateVolume(z.volume);
      const o = document.createElement('option');
      o.value = z.id;
      o.textContent = z.name;
      if (z.id === state.zone_id) o.selected = true;
      sel.appendChild(o);
    });
  } catch {}
}

// Controls
$('btn-play').onclick = async () => {
  await apiPost(`/api/v1/zones/${state.zone_id}/${state.playing ? 'pause' : 'resume'}`);
  updateState(!state.playing);
};
$('btn-next').onclick = () => apiPost(`/api/v1/zones/${state.zone_id}/next`);
$('btn-prev').onclick = () => apiPost(`/api/v1/zones/${state.zone_id}/previous`);

$('btn-close').onclick = () => {
  try { window.__TAURI__?.window?.getCurrentWindow()?.hide(); } catch {}
};

// Bascule vers la fenêtre complète. Le menu de la barre système la propose
// aussi, mais encore faut-il savoir qu'il existe : le seul endroit où
// l'utilisateur regarde, c'est la fenêtre qu'il a sous les yeux (Sandro).
$('btn-expand').onclick = async () => {
  try {
    await window.__TAURI__?.core?.invoke('set_display_mode', { full: true });
  } catch (e) {
    console.error('set_display_mode', e);
  }
};

$('volume-slider').oninput = () => {
  const v = parseInt($('volume-slider').value);
  $('volume-value').textContent = v;
  volumeLocalUntil = Date.now() + 3000;
  // Une fraction 0..1, jamais un pourcentage : le serveur lit tout nombre
  // <= 1 comme une fraction, et `{volume: 1}` valait donc le plein volume.
  apiPut(`/api/v1/zones/${state.zone_id}/volume`, C.corpsDeVolume(v, courbe()));
};

$('zone-select').onchange = () => {
  init(parseInt($('zone-select').value, 10));
};

// Search
$('search-input').oninput = () => {
  clearTimeout(searchTimeout);
  const q = $('search-input').value.trim();
  if (!q) { $('search-results').className = ''; return; }
  searchTimeout = setTimeout(async () => {
    try {
      const r = await fetch(C.urlDeRecherche(SERVER, q));
      if (!r.ok) throw new Error(r.status);
      const items = C.resultatsDeRecherche(await r.json());
      const box = $('search-results');
      if (!items.length) { box.className = ''; return; }
      // Construit nœud par nœud : un titre ou un nom d'artiste est du texte,
      // jamais du HTML.
      box.replaceChildren(...items.map(i => {
        const row = document.createElement('div');
        row.className = 'search-item';
        const info = document.createElement('div');
        info.className = 'search-item-info';
        const t = document.createElement('div');
        t.className = 'search-item-title';
        t.textContent = i.title || '?';
        const a = document.createElement('div');
        a.className = 'search-item-artist';
        a.textContent = i.artist || '';
        info.append(t, a);
        const play = document.createElement('span');
        play.className = 'search-item-play';
        play.textContent = '▶';
        row.append(info, play);
        row.onclick = () => {
          fetch(`${SERVER}/api/v1/zones/${state.zone_id}/play`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(C.corpsDeLecture(i)) });
          $('search-input').value = '';
          box.className = '';
        };
        return row;
      }));
      box.className = 'visible';
    } catch {}
  }, 300);
};

// Settings
const mainEls = ['cover-container', 'track-info', 'progress-container', 'controls', 'volume-container', 'search-container'];
$('btn-settings').onclick = () => { mainEls.forEach(id => $(id).style.display = 'none'); $('settings-panel').style.display = 'block'; $('server-url').value = SERVER.replace(/^https?:\/\//, ''); };
$('btn-settings-back').onclick = () => { $('settings-panel').style.display = 'none'; mainEls.forEach(id => $(id).style.display = ''); };
$('btn-connect').onclick = () => {
  const v = $('server-url').value.trim();
  if (!v) return;
  SERVER = C.normaliserServeur(v);
  localStorage.setItem('tune-server', SERVER);
  // Le côté natif (raccourcis, fenêtre complète) suit la même adresse.
  invoke('set_server_url', { url: SERVER })?.catch?.(() => {});
  if (ws) ws.close();
  $('settings-panel').style.display = 'none';
  mainEls.forEach(id => $(id).style.display = '');
  init();
};
$('server-url').addEventListener('keydown', e => { if (e.key === 'Enter') $('btn-connect').click(); });

// Position interpolation
setInterval(() => {
  if (state.playing && state.duration_ms > 0) {
    state.position_ms += 200;
    $('time-current').textContent = formatTime(state.position_ms);
    $('progress-bar').value = (state.position_ms / state.duration_ms) * 100;
  }
}, 200);

// WebSocket real-time updates
let ws = null;
let wsBackoff = 1000;

function connectWS() {
  const wsUrl = SERVER.replace('http://', 'ws://').replace('https://', 'wss://') + '/ws';
  try { ws = new WebSocket(wsUrl); } catch { return; }

  ws.onopen = () => {
    wsBackoff = 1000;
    ws.send(JSON.stringify({ subscribe: ['playback.*', 'zone.*'] }));
  };

  ws.onmessage = (e) => {
    let msg;
    try { msg = JSON.parse(e.data); } catch { return; }
    const ev = C.lireEvenement(msg, state.zone_id);
    if (!ev) return;
    const data = ev.data;
    switch (ev.genre) {
      case 'piste': updateNowPlaying(data); updateState(true); break;
      case 'pause': updateState(false); break;
      case 'reprise': updateState(true); break;
      case 'arret': updateState(false); break;
      case 'position': updatePosition(data.position_ms || 0); break;
      case 'volume': updateVolume(data.volume); break;
    }
  };

  ws.onclose = () => {
    setTimeout(connectWS, wsBackoff);
    wsBackoff = Math.min(wsBackoff * 2, 30000);
  };
  ws.onerror = () => ws.close();
}

// Fallback polling every 10s (in case WS fails)
setInterval(refresh, 10000);

// Start : l'adresse retenue par le côté natif l'emporte sur le défaut, pas
// sur celle que l'utilisateur a tapée dans ce panneau.
(async () => {
  if (!localStorage.getItem('tune-server')) {
    try {
      const natif = C.normaliserServeur(await invoke('get_server_url'));
      if (natif) SERVER = natif;
    } catch {}
  }
  init();
  connectWS();
})();
