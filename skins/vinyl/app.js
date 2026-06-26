(() => {
  const BASE = `${location.protocol}//${location.host}/api/v1`;
  let ws = null;
  let currentZoneId = null;
  let pollInterval = null;
  let seekDragging = false;
  let volumeDragging = false;
  let lastQueueHash = '';

  // DOM refs
  const $ = (sel) => document.querySelector(sel);
  const disc = $('#vinyl-disc');
  const tonearm = $('#tonearm');
  const coverArt = $('#cover-art');
  const trackTitle = $('#track-title');
  const trackArtist = $('#track-artist');
  const trackAlbum = $('#track-album');
  const badgeFormat = $('#badge-format');
  const badgeQuality = $('#badge-quality');
  const timeCurrent = $('#time-current');
  const timeTotal = $('#time-total');
  const seekBar = $('#seek-bar');
  const btnPlay = $('#btn-play');
  const iconPlay = $('#icon-play');
  const iconPause = $('#icon-pause');
  const btnShuffle = $('#btn-shuffle');
  const btnRepeat = $('#btn-repeat');
  const volumeSlider = $('#volume-slider');
  const volumeValue = $('#volume-value');
  const zoneSelect = $('#zone-select');
  const queueList = $('#queue-list');
  const queueCount = $('#queue-count');
  const connDot = $('#connection-dot');
  const connText = $('#connection-text');

  // Helpers
  function fmt(ms) {
    if (!ms || ms <= 0) return '0:00';
    const s = Math.floor(ms / 1000);
    const m = Math.floor(s / 60);
    return `${m}:${String(s % 60).padStart(2, '0')}`;
  }

  async function api(path, opts) {
    const res = await fetch(`${BASE}${path}`, opts);
    if (!res.ok) throw new Error(`${res.status}`);
    return res.json();
  }

  function artworkUrl(path) {
    if (!path) return null;
    if (path.startsWith('http')) return path;
    if (path.match(/^[a-f0-9]{32}$/)) return `${BASE}/library/artwork/${path}`;
    return `${location.protocol}//${location.host}${path}`;
  }

  function setCover(coverPath) {
    const src = artworkUrl(coverPath);
    if (src) {
      if (coverArt.src !== src) coverArt.src = src;
      coverArt.classList.add('visible');
    } else {
      coverArt.classList.remove('visible');
    }
  }

  let lastCoverTrackId = null;
  async function fetchTrackCover(trackId) {
    if (trackId === lastCoverTrackId) return;
    lastCoverTrackId = trackId;
    try {
      const track = await api(`/library/tracks/${trackId}`);
      const cover = track.cover_path || track.album_cover;
      if (cover) {
        setCover(cover);
      } else if (track.album_id) {
        const album = await api(`/library/albums/${track.album_id}`);
        if (album.cover_path) setCover(album.cover_path);
        else coverArt.classList.remove('visible');
      } else {
        coverArt.classList.remove('visible');
      }
    } catch (e) {
      coverArt.classList.remove('visible');
    }
  }

  // Load zones
  async function loadZones() {
    try {
      const zones = await api('/zones/');
      zoneSelect.innerHTML = '';
      zones.forEach(z => {
        const opt = document.createElement('option');
        opt.value = z.id;
        opt.textContent = z.name || `Zone ${z.id}`;
        if (z.state === 'playing') opt.textContent += ' ▶';
        zoneSelect.appendChild(opt);
      });
      if (zones.length > 0) {
        if (!currentZoneId) {
          const playing = zones.find(z => z.state === 'playing');
          currentZoneId = playing ? playing.id : zones[0].id;
        }
        zoneSelect.value = currentZoneId;
        updateFromZone(zones.find(z => z.id == currentZoneId) || zones[0]);
      }
    } catch (e) { console.error('loadZones', e); }
  }

  // Update UI from zone state
  function updateFromZone(zone) {
    if (!zone) return;
    currentZoneId = zone.id;

    const np = zone.now_playing || zone.current_track;
    const state = zone.state || 'stopped';
    const isPlaying = state === 'playing';

    // Vinyl animation
    if (isPlaying) {
      disc.classList.add('spinning');
      tonearm.classList.add('playing');
    } else {
      disc.classList.remove('spinning');
      tonearm.classList.remove('playing');
    }

    // Play/Pause icon
    iconPlay.style.display = isPlaying ? 'none' : 'block';
    iconPause.style.display = isPlaying ? 'block' : 'none';

    // Track info
    if (np) {
      trackTitle.textContent = np.title || 'Unknown';
      trackArtist.textContent = np.artist_name || np.artist || '—';
      trackAlbum.textContent = np.album_title || np.album || '';

      // Cover — resolve from cover_path, or fetch from track/album API
      const coverUrl = np.cover_path || np.cover_url;
      if (coverUrl) {
        setCover(coverUrl);
      } else if (np.track_id || np.id) {
        fetchTrackCover(np.track_id || np.id);
      } else {
        coverArt.classList.remove('visible');
      }

      // Audio badge
      const format = (np.format || '').toUpperCase().replace('AUDIO/', '');
      badgeFormat.textContent = format || '—';
      const sr = np.sample_rate ? `${np.sample_rate >= 1000 ? (np.sample_rate/1000).toFixed(1) : np.sample_rate} kHz` : '';
      const bd = np.bit_depth ? `${np.bit_depth} bit` : '';
      badgeQuality.textContent = [sr, bd].filter(Boolean).join(' / ') || '';
      badgeQuality.style.display = badgeQuality.textContent ? '' : 'none';

      // Time
      const pos = zone.position_ms || 0;
      const dur = np.duration_ms || 0;
      timeCurrent.textContent = fmt(pos);
      timeTotal.textContent = fmt(dur);
      if (!seekDragging && dur > 0) {
        seekBar.max = dur;
        seekBar.value = pos;
      }
    } else {
      trackTitle.textContent = 'No track';
      trackArtist.textContent = '—';
      trackAlbum.textContent = '';
      coverArt.classList.remove('visible');
      badgeFormat.textContent = '—';
      badgeQuality.textContent = '';
      badgeQuality.style.display = 'none';
      timeCurrent.textContent = '0:00';
      timeTotal.textContent = '0:00';
      seekBar.value = 0;
    }

    // Volume — only update if user is not dragging
    if (!volumeDragging) {
      const vol = Math.round((zone.volume ?? 0.5) * 100);
      volumeSlider.value = vol;
      volumeValue.textContent = vol;
    }

    // Shuffle / Repeat
    btnShuffle.classList.toggle('active', !!zone.shuffle);
    btnRepeat.classList.toggle('active', zone.repeat && zone.repeat !== 'off');

    // Queue position highlight (without full rebuild)
    const queuePos = zone.queue_position ?? 0;
    const items = queueList.querySelectorAll('.queue-item');
    items.forEach((item, i) => {
      item.classList.toggle('active', i === queuePos);
    });
    queueCount.textContent = `${items.length > 0 ? queuePos + 1 : 0} / ${items.length}`;

    // Scroll active into view
    const activeItem = queueList.querySelector('.active');
    if (activeItem) activeItem.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
  }

  // Load queue — only rebuild DOM when queue content changes
  async function loadQueue() {
    if (!currentZoneId) return;
    try {
      const q = await api(`/zones/${currentZoneId}/queue`);
      const tracks = q.tracks || [];
      const pos = q.position ?? 0;

      // Build a hash to detect changes
      const hash = tracks.map(t => `${t.id || t.source_id || t.title}`).join(',');
      if (hash === lastQueueHash) {
        // Just update position highlight
        const items = queueList.querySelectorAll('.queue-item');
        items.forEach((item, i) => item.classList.toggle('active', i === pos));
        queueCount.textContent = `${tracks.length > 0 ? pos + 1 : 0} / ${tracks.length}`;
        const activeItem = queueList.querySelector('.active');
        if (activeItem) activeItem.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
        return;
      }
      lastQueueHash = hash;

      queueCount.textContent = `${tracks.length > 0 ? pos + 1 : 0} / ${tracks.length}`;
      queueList.innerHTML = '';
      tracks.forEach((t, i) => {
        const div = document.createElement('div');
        div.className = 'queue-item' + (i === pos ? ' active' : '');

        const coverSrc = artworkUrl(t.cover_path) || '';

        div.innerHTML = `
          <span class="queue-num">${i + 1}</span>
          ${coverSrc ? `<img class="queue-thumb" src="${coverSrc}" loading="lazy">` : '<div class="queue-thumb"></div>'}
          <div class="queue-info">
            <div class="queue-title">${t.title || 'Unknown'}</div>
            <div class="queue-artist">${t.artist_name || t.artist || ''}</div>
          </div>
          <span class="queue-duration">${fmt(t.duration_ms)}</span>
        `;
        div.addEventListener('click', () => playQueuePosition(i));
        queueList.appendChild(div);
      });

      const activeItem = queueList.querySelector('.active');
      if (activeItem) activeItem.scrollIntoView({ block: 'nearest' });
    } catch (e) { console.error('loadQueue', e); }
  }

  async function playQueuePosition(pos) {
    if (!currentZoneId) return;
    try {
      const result = await api(`/zones/${currentZoneId}/queue/jump`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ position: pos }),
      });
      updateFromZone(result);
    } catch (e) { console.error('jump', e); }
  }

  // Poll zone state
  async function pollState() {
    if (!currentZoneId) return;
    try {
      const zone = await api(`/zones/${currentZoneId}`);
      updateFromZone(zone);
    } catch (e) { /* ignore */ }
  }

  // WebSocket
  function connectWS() {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    ws = new WebSocket(`${proto}//${location.host}/ws`);

    ws.onopen = () => {
      connDot.classList.add('connected');
      connText.textContent = 'Connected';
      setTimeout(() => {
        $('#connection-bar').classList.add('hidden');
      }, 2000);
    };

    ws.onmessage = (e) => {
      try {
        const msg = JSON.parse(e.data);
        const t = msg.type || msg.event || '';
        if (t.includes('zone') || t.includes('playback') || t.includes('queue') || t.includes('track')) {
          pollState();
        }
      } catch (_) {}
    };

    ws.onclose = () => {
      connDot.classList.remove('connected');
      connText.textContent = 'Disconnected';
      $('#connection-bar').classList.remove('hidden');
      setTimeout(connectWS, 3000);
    };

    ws.onerror = () => ws.close();
  }

  // Controls
  btnPlay.onclick = async () => {
    if (!currentZoneId) return;
    try {
      const zone = await api(`/zones/${currentZoneId}`);
      const action = zone.state === 'playing' ? 'pause' : 'play';
      await api(`/zones/${currentZoneId}/${action}`, { method: 'POST' });
      pollState();
    } catch (e) { console.error('play/pause', e); }
  };

  $('#btn-prev').onclick = async () => {
    if (!currentZoneId) return;
    await api(`/zones/${currentZoneId}/previous`, { method: 'POST' });
    pollState();
  };

  $('#btn-next').onclick = async () => {
    if (!currentZoneId) return;
    await api(`/zones/${currentZoneId}/next`, { method: 'POST' });
    pollState();
  };

  btnShuffle.onclick = async () => {
    if (!currentZoneId) return;
    const zone = await api(`/zones/${currentZoneId}`);
    await api(`/zones/${currentZoneId}/shuffle?enabled=${!zone.shuffle}`, { method: 'POST' });
    pollState();
  };

  btnRepeat.onclick = async () => {
    if (!currentZoneId) return;
    const zone = await api(`/zones/${currentZoneId}`);
    const modes = ['off', 'all', 'one'];
    const next = modes[(modes.indexOf(zone.repeat || 'off') + 1) % modes.length];
    await api(`/zones/${currentZoneId}/repeat?mode=${next}`, { method: 'POST' });
    pollState();
  };

  // Seek
  seekBar.addEventListener('input', () => {
    seekDragging = true;
    timeCurrent.textContent = fmt(parseInt(seekBar.value));
  });
  seekBar.addEventListener('change', async () => {
    seekDragging = false;
    if (!currentZoneId) return;
    await api(`/zones/${currentZoneId}/seek`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ position_ms: parseInt(seekBar.value) }),
    });
    pollState();
  });

  // Volume
  volumeSlider.addEventListener('mousedown', () => { volumeDragging = true; });
  volumeSlider.addEventListener('touchstart', () => { volumeDragging = true; });

  let volumeTimeout = null;
  volumeSlider.addEventListener('input', () => {
    volumeValue.textContent = volumeSlider.value;
    clearTimeout(volumeTimeout);
    volumeTimeout = setTimeout(async () => {
      if (!currentZoneId) return;
      await api(`/zones/${currentZoneId}/volume`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ volume: parseInt(volumeSlider.value) / 100 }),
      });
    }, 150);
  });

  volumeSlider.addEventListener('change', () => { volumeDragging = false; });
  volumeSlider.addEventListener('mouseup', () => { volumeDragging = false; });
  volumeSlider.addEventListener('touchend', () => { volumeDragging = false; });

  // Zone selector
  zoneSelect.addEventListener('change', () => {
    currentZoneId = parseInt(zoneSelect.value);
    lastQueueHash = '';
    loadQueue();
    pollState();
  });

  // Init
  loadZones();
  loadQueue();
  connectWS();
  pollInterval = setInterval(pollState, 2000);

  // Reload queue less frequently
  setInterval(loadQueue, 5000);
})();
