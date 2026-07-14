// Config
let SERVER = localStorage.getItem('tune-server') || 'http://192.168.1.15:8888';
let state = { playing: false, zone_id: 0, duration_ms: 0, position_ms: 0 };
let searchTimeout = null;

const $ = (id) => document.getElementById(id);

function formatTime(ms) {
  const s = Math.floor(ms / 1000);
  const m = Math.floor(s / 60);
  return `${m}:${String(s % 60).padStart(2, '0')}`;
}

function coverUrl(path) {
  if (!path) return null;
  if (path.startsWith('http')) return path;
  if (/^[0-9a-f]{32,64}$/i.test(path)) return `${SERVER}/api/v1/library/artwork/${path}`;
  return null;
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

function updateVolume(vol) {
  if (Date.now() < volumeLocalUntil) return;
  const v = vol > 1 ? Math.round(vol) : Math.round(vol * 100);
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
    updateVolume(zone.volume ?? 0.5);
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

async function init() {
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
    if (playing) {
      state.zone_id = playing.zone_id;
    } else {
      // Pick first paused zone, or first zone
      const paused = (data.zones || []).find(z => z.state === 'paused');
      state.zone_id = paused ? paused.zone_id : ((data.zones && data.zones[0]) ? data.zones[0].zone_id : 1);
    }
    applyZoneData(data);
  } catch (e) {
    console.error('init:', e);
    setConnected(false);
  }

  try {
    const zones = await apiGet('/api/v1/zones');
    const sel = $('zone-select');
    sel.innerHTML = '';
    (Array.isArray(zones) ? zones : []).forEach(z => {
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

$('volume-slider').oninput = () => {
  const v = parseInt($('volume-slider').value);
  $('volume-value').textContent = v;
  volumeLocalUntil = Date.now() + 3000;
  apiPut(`/api/v1/zones/${state.zone_id}/volume`, { volume: v });
};

$('zone-select').onchange = () => {
  state.zone_id = parseInt($('zone-select').value);
  // Apply the picked zone directly. Do NOT call init() — it re-picks the
  // "best" playing/paused zone and would immediately clobber the user's
  // selection. refresh() just re-renders now-playing for the chosen zone.
  refresh();
};

// Search
$('search-input').oninput = () => {
  clearTimeout(searchTimeout);
  const q = $('search-input').value.trim();
  if (!q) { $('search-results').className = ''; return; }
  searchTimeout = setTimeout(async () => {
    try {
      const data = await apiGet(`/api/v1/search?q=${encodeURIComponent(q)}&limit=8`);
      // The search endpoint nests results under `local` ({local:{tracks,albums,artists}}).
      // Reading data.tracks/data.albums (top-level) always came back empty → no results.
      const local = data.local || data;
      const items = [];
      (local.tracks || []).slice(0, 4).forEach(t => items.push({ title: t.title, artist: t.artist_name, type: 'track', id: t.id }));
      (local.albums || []).slice(0, 4).forEach(a => items.push({ title: a.title, artist: a.artist_name, type: 'album', id: a.id }));
      if (!items.length) { $('search-results').className = ''; return; }
      $('search-results').innerHTML = items.map(i => `
        <div class="search-item" data-type="${i.type}" data-id="${i.id}">
          <div class="search-item-info">
            <div class="search-item-title">${i.title || '?'}</div>
            <div class="search-item-artist">${i.artist || ''}</div>
          </div>
          <span class="search-item-play">▶</span>
        </div>`).join('');
      $('search-results').className = 'visible';
      $('search-results').querySelectorAll('.search-item').forEach(el => {
        el.onclick = () => {
          const body = el.dataset.type === 'album' ? { album_id: parseInt(el.dataset.id) } : { track_id: parseInt(el.dataset.id) };
          fetch(`${SERVER}/api/v1/zones/${state.zone_id}/play`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
          $('search-input').value = '';
          $('search-results').className = '';
        };
      });
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
  SERVER = v.startsWith('http') ? v : `http://${v}`;
  localStorage.setItem('tune-server', SERVER);
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
    try {
      const msg = JSON.parse(e.data);
      const type = msg.type || '';
      const data = msg.data || msg;
      if (data.zone_id && data.zone_id !== state.zone_id) return;

      if (type.includes('track_changed') || type.includes('started')) {
        updateNowPlaying(data);
        updateState(true);
      } else if (type.includes('paused')) {
        updateState(false);
      } else if (type.includes('resumed')) {
        updateState(true);
      } else if (type.includes('stopped')) {
        updateState(false);
      } else if (type.includes('position')) {
        updatePosition(data.position_ms || 0);
      } else if (type.includes('volume')) {
        updateVolume(data.volume ?? 0);
      }
    } catch {}
  };

  ws.onclose = () => {
    setTimeout(connectWS, wsBackoff);
    wsBackoff = Math.min(wsBackoff * 2, 30000);
  };
  ws.onerror = () => ws.close();
}

// Fallback polling every 10s (in case WS fails)
setInterval(refresh, 10000);

// Start
init();
connectWS();
