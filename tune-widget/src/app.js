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

// ── Recherche (#7595) ──────────────────────────────────────────────────────
//
// La bibliothèque d'abord, affichée dès qu'elle répond ; Qobuz et TIDAL en
// même temps, chacun par sa propre requête, rangés DESSOUS dans l'ordre fixe
// de `SERVICES_DE_RECHERCHE` quel que soit celui qui répond le premier.
// Un délai entre les frappes, et toute requête d'une saisie dépassée est
// annulée (AbortController) : sa réponse ne peut plus repeindre la liste.
//
// Toutes les formes du serveur passent par `contrat.js`.

const DELAI_DE_FRAPPE_MS = 250;
let recherche = null; // { q, ctrl, sources: [{ cle, nom, limite, etat, cats, visibles }], actif }

const SOURCES_DE_RECHERCHE = [{ cle: 'local', nom: 'Bibliothèque' }, ...C.SERVICES_DE_RECHERCHE];

function cleDElement(i) { return `${i.source}:${i.type}:${i.id}`; }

function ouvrirPanneauDeRecherche(ouvert) {
  $('app').classList.toggle('en-recherche', ouvert);
  $('search-results').className = ouvert ? 'visible' : '';
}

function fermerRecherche() {
  clearTimeout(searchTimeout);
  if (recherche && recherche.ctrl) recherche.ctrl.abort();
  recherche = null;
  $('search-results').replaceChildren();
  ouvrirPanneauDeRecherche(false);
}

async function chargerSource(r, src) {
  const url = src.cle === 'local'
    ? C.urlDeRecherche(SERVER, r.q, src.limite)
    : C.urlDeRechercheDeService(SERVER, r.q, src.cle, src.limite);
  try {
    const rep = await fetch(url, { signal: r.ctrl.signal });
    if (!rep.ok) throw new Error(rep.status);
    const json = await rep.json();
    if (recherche !== r) return; // saisie dépassée
    src.cats = src.cle === 'local' ? C.resultatsLocaux(json) : C.resultatsDeService(json, src.cle, src.limite);
    src.etat = 'pret';
  } catch (e) {
    if (recherche !== r || (e && e.name === 'AbortError')) return;
    src.etat = 'echec';
  }
  dessinerRecherche();
}

function lancerRecherche(q) {
  if (recherche && recherche.ctrl) recherche.ctrl.abort();
  const r = {
    q,
    ctrl: new AbortController(),
    actif: recherche ? recherche.actif : null,
    sources: SOURCES_DE_RECHERCHE.map(s => ({
      cle: s.cle, nom: s.nom, limite: C.LIMITE_INITIALE, etat: 'attente', cats: null,
      visibles: { ...C.PAR_CATEGORIE },
    })),
  };
  recherche = r;
  ouvrirPanneauDeRecherche(true);
  dessinerRecherche();
  // La bibliothèque part la première ; les services aussitôt derrière, en
  // parallèle, sans attendre sa réponse.
  r.sources.forEach(src => { chargerSource(r, src); });
}

function vignette(item) {
  const url = C.urlDeVignette(SERVER, item.cover);
  const repli = () => {
    const s = document.createElement('span');
    s.className = 'vignette vignette-repli' + (item.type === 'artist' ? ' ronde' : '');
    s.textContent = item.type === 'artist' ? ((item.title || '?').trim().charAt(0).toUpperCase() || '?') : '♪';
    return s;
  };
  if (!url) return repli();
  const img = document.createElement('img');
  img.className = 'vignette' + (item.type === 'artist' ? ' ronde' : '');
  img.alt = '';
  img.loading = 'lazy';
  img.decoding = 'async';
  img.onerror = () => img.replaceWith(repli());
  img.src = url;
  return img;
}

function ligneDeResultat(item) {
  const row = document.createElement('div');
  row.className = 'search-item nav';
  row.setAttribute('role', 'option');
  row.dataset.cle = cleDElement(item);
  const info = document.createElement('div');
  info.className = 'search-item-info';
  // Construit nœud par nœud : un titre ou un nom d'artiste est du texte,
  // jamais du HTML.
  const t = document.createElement('div');
  t.className = 'search-item-title';
  t.textContent = item.title || '?';
  info.append(t);
  if (item.subtitle) {
    const a = document.createElement('div');
    a.className = 'search-item-artist';
    a.textContent = item.subtitle;
    info.append(a);
  }
  const play = document.createElement('span');
  play.className = 'search-item-play';
  play.textContent = '▶';
  row.append(vignette(item), info, play);
  row.onclick = () => lireResultat(item);
  row.onmouseenter = () => activer(row.dataset.cle, false);
  return row;
}

function boutonVoirPlus(r, src, cat) {
  const b = document.createElement('div');
  b.className = 'search-more nav';
  b.setAttribute('role', 'option');
  b.dataset.cle = `plus:${src.cle}:${cat}`;
  b.textContent = 'Voir plus';
  b.onclick = async () => {
    const { visibles, limite } = C.voirPlus(src.cats[cat], src.visibles[cat], C.PAR_CATEGORIE[cat], src.limite);
    src.visibles[cat] = visibles;
    if (limite) {
      src.limite = limite;
      src.etat = 'attente';
      dessinerRecherche();
      await chargerSource(r, src);
    } else {
      dessinerRecherche();
    }
  };
  b.onmouseenter = () => activer(b.dataset.cle, false);
  return b;
}

function dessinerRecherche() {
  const r = recherche;
  const box = $('search-results');
  if (!r) return;
  const noeuds = [];
  let toutPret = true;
  let rien = true;
  r.sources.forEach(src => {
    if (src.etat === 'attente') toutPret = false;
    const cats = src.cats;
    const nonVides = cats ? C.CATEGORIES.filter(c => cats[c].items.length) : [];
    if (!nonVides.length) {
      // La bibliothèque annonce qu'elle cherche ; un service muet ne prend pas de place.
      if (src.cle === 'local' && src.etat === 'attente') {
        noeuds.push(enTete('search-source', src.nom), etatDeRecherche('Recherche…'));
      }
      return;
    }
    rien = false;
    noeuds.push(enTete('search-source', src.nom + (src.etat === 'attente' ? ' …' : '')));
    nonVides.forEach(c => {
      const cat = cats[c];
      const total = Number.isInteger(cat.total) && cat.total > 0 ? ` · ${cat.total}` : '';
      noeuds.push(enTete('search-category', C.NOMS_DE_CATEGORIE[c] + total));
      cat.items.slice(0, src.visibles[c]).forEach(i => noeuds.push(ligneDeResultat(i)));
      if (C.aEncore(cat, src.visibles[c], src.limite)) noeuds.push(boutonVoirPlus(r, src, c));
    });
  });
  if (rien && toutPret) noeuds.push(etatDeRecherche('Aucun résultat'));
  box.replaceChildren(...noeuds);
  activer(r.actif, false);
}

function enTete(classe, texte) {
  const h = document.createElement('div');
  h.className = classe;
  h.textContent = texte;
  return h;
}

function etatDeRecherche(texte) {
  const d = document.createElement('div');
  d.className = 'search-empty';
  d.textContent = texte;
  return d;
}

function navigables() { return Array.from($('search-results').querySelectorAll('.nav')); }

function activer(cle, defiler) {
  if (!recherche) return;
  recherche.actif = cle;
  let choisi = null;
  navigables().forEach(el => {
    const oui = cle != null && el.dataset.cle === cle;
    el.classList.toggle('actif', oui);
    el.setAttribute('aria-selected', oui ? 'true' : 'false');
    if (oui) choisi = el;
  });
  if (choisi && defiler) choisi.scrollIntoView({ block: 'nearest' });
}

function deplacer(delta) {
  const els = navigables();
  if (!els.length || !recherche) return;
  const i = els.findIndex(el => el.dataset.cle === recherche.actif);
  const j = i < 0 ? (delta > 0 ? 0 : els.length - 1) : Math.max(0, Math.min(els.length - 1, i + delta));
  activer(els[j].dataset.cle, true);
}

async function lireResultat(item) {
  fermerRecherche();
  $('search-input').value = '';
  try {
    const r = await fetch(`${SERVER}/api/v1/zones/${state.zone_id}/play`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(C.corpsDeLecture(item)),
    });
    if (!r.ok) console.error('play', r.status, await r.text().catch(() => ''));
  } catch (e) {
    console.error('play', e);
  }
}

$('search-input').oninput = () => {
  clearTimeout(searchTimeout);
  const q = $('search-input').value.trim();
  if (!q) { fermerRecherche(); return; }
  searchTimeout = setTimeout(() => lancerRecherche(q), DELAI_DE_FRAPPE_MS);
};

$('search-input').addEventListener('keydown', (e) => {
  if (e.key === 'Escape') {
    e.preventDefault();
    $('search-input').value = '';
    fermerRecherche();
  } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    deplacer(e.key === 'ArrowDown' ? 1 : -1);
  } else if (e.key === 'Enter') {
    e.preventDefault();
    const els = navigables();
    const el = els.find(x => recherche && x.dataset.cle === recherche.actif)
      || els.find(x => x.classList.contains('search-item'));
    if (el) el.click();
  }
});

// Settings
const mainEls = ['cover-container', 'track-info', 'progress-container', 'controls', 'volume-container', 'search-container'];
$('btn-settings').onclick = () => { $('search-input').value = ''; fermerRecherche(); mainEls.forEach(id => $(id).style.display = 'none'); $('settings-panel').style.display = 'block'; $('server-url').value = SERVER.replace(/^https?:\/\//, ''); };
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
