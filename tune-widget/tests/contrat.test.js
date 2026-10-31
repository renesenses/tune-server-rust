// Témoins du contrat widget ↔ serveur. `node --test tune-widget/tests`.
//
// Les charges utiles ci-dessous recopient la forme que rend
// `origin/batch/bugs-rc10-20261030` : `routes/search.rs` (bloc `local`, blocs
// `services.<nom>` en `SearchResults`), `routes/playback.rs` (`PlayRequest`),
// `routes/zones/lecture.rs` (`volume`, `volume_curve`), `routes/ws.rs`
// (`{type, data}` avec `data.zone_id`), `tune_core::audio::volume_scale`.
'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const C = require(path.join(__dirname, '..', 'src', 'contrat.js'));

const S = 'http://tune.local:8888';

// ── Recherche (#7595) ──────────────────────────────────────────────────────
// Réponse de `federated_search` (routes/search.rs) avec `sources=local` : les
// modèles `Artist` / `Album` / `Track::to_json`, les totaux et `has_more` de
// #3189, `services` vide. Avec `sources=qobuz` : `local` présent mais vide, et
// le bloc du service est un `SearchResults` (`source_id`, `artist_name`,
// `album_title`, `cover_path` distant — sérialisation de `StreamTrack`).
const REPONSE_LOCALE = {
  local: {
    artists: [
      { id: 3, name: 'Miles Davis', sort_name: 'Davis, Miles', musicbrainz_id: null, discogs_id: null, bio: null, image_path: 'abcdef0123456789abcdef0123456789.jpg', image_source: null },
      { id: null, name: 'Sans id' },
    ],
    albums: Array.from({ length: 8 }, (_, i) => ({ id: 100 + i, title: `Album ${i}`, artist_id: 3, artist_name: 'Miles Davis', cover_path: i === 0 ? '0123456789abcdef0123456789abcdef' : null, source: 'local', source_id: null })),
    tracks: [
      { id: 42, title: 'So What', album_id: 100, album_title: 'Kind of Blue', artist_id: 3, artist_name: 'Miles Davis', cover_path: '/api/v1/library/artwork/0123456789abcdef0123456789abcdef', source: 'local', channel_badge: null },
      { id: 'abc', title: 'id non entier' },
    ],
    totals: { artists: 1, albums: 37, tracks: 1, tracks_via_metadata: 0 },
    totals_capped: { artists: false, albums: false, tracks: false },
    has_more: { artists: false, albums: true, tracks: false },
    limit: 12,
    offset: 0,
  },
  radios: [],
  radios_annuaire: {},
  services: {},
};

const REPONSE_QOBUZ = {
  local: { artists: [], albums: [], tracks: [], totals: { artists: 0, albums: 0, tracks: 0, tracks_via_metadata: 0 }, has_more: { artists: false, albums: false, tracks: false }, limit: 12, offset: 0 },
  radios: [],
  services: {
    qobuz: {
      tracks: [
        { source_id: '5966783', title: 'So What', artist_name: 'Miles Davis', album_title: 'Kind of Blue', album_id: '0886443927087', duration_ms: 562000, cover_path: 'https://static.qobuz.com/images/covers/87/70/0886443927087_600.jpg', track_number: 1, disc_number: 1, explicit: false, quality: null, isrc: null, composer: null, artist_id: '7616' },
        { source_id: '1', title: 'Indisponible', artist_name: 'X', disponible: false, duration_ms: 1 },
        { title: 'Sans source_id', artist_name: 'X' },
      ],
      albums: [{ source_id: '0886443927087', title: 'Kind of Blue', artist_name: 'Miles Davis', artist_id: '7616', cover_path: 'https://static.qobuz.com/images/covers/87/70/0886443927087_600.jpg', year: 1959, track_count: 5, quality: null }],
      artists: [{ id: '7616', name: 'Miles Davis', image_path: null }],
      playlists: [],
    },
  },
};

test('la bibliothèque rend artistes, albums et pistes, avec totaux et suite (#3189)', () => {
  const r = C.resultatsLocaux(REPONSE_LOCALE);
  assert.deepEqual(r.artists.items, [
    { source: 'local', type: 'artist', id: 3, title: 'Miles Davis', subtitle: '', cover: 'abcdef0123456789abcdef0123456789.jpg' },
  ]);
  assert.equal(r.albums.items.length, 8);
  assert.equal(r.albums.total, 37);
  assert.equal(r.albums.aSuite, true);
  assert.deepEqual(r.tracks.items, [
    { source: 'local', type: 'track', id: 42, title: 'So What', subtitle: 'Miles Davis — Kind of Blue', cover: '/api/v1/library/artwork/0123456789abcdef0123456789abcdef' },
  ]);
  assert.equal(r.tracks.aSuite, false);
});

test('contre-épreuve : la réponse d’un service ne fournit RIEN à la bibliothèque, et inversement', () => {
  const l = C.resultatsLocaux(REPONSE_QOBUZ);
  assert.equal(l.tracks.items.length + l.albums.items.length + l.artists.items.length, 0);
  assert.equal(C.resultatsDeService(REPONSE_LOCALE, 'qobuz'), null);
  // Un service absent (non connecté) n'a pas de bloc : null, pas une section vide.
  assert.equal(C.resultatsDeService(REPONSE_QOBUZ, 'tidal'), null);
});

test('un service : pistes et albums par `source_id`, sans artistes ni titres indisponibles', () => {
  const r = C.resultatsDeService(REPONSE_QOBUZ, 'qobuz');
  assert.equal(r.artists.items.length, 0, 'POST /play ne sait pas lire un artiste de service');
  assert.deepEqual(r.tracks.items.map((t) => [t.source, t.type, t.id]), [['qobuz', 'track', '5966783']]);
  assert.equal(r.tracks.items[0].subtitle, 'Miles Davis — Kind of Blue');
  assert.deepEqual(r.albums.items.map((a) => [a.source, a.type, a.id, a.title]), [['qobuz', 'album', '0886443927087', 'Kind of Blue']]);
  // Trois pistes reçues pour une limite de 3 : il peut en rester.
  assert.equal(C.resultatsDeService(REPONSE_QOBUZ, 'qobuz', 3).tracks.aSuite, true);
  assert.equal(C.resultatsDeService(REPONSE_QOBUZ, 'qobuz', 12).tracks.aSuite, false);
});

test('les URL : la bibliothèque seule, puis un service seul, jamais le tout-en-un', () => {
  const u = new URL(C.urlDeRecherche(S, 'so what & co'));
  assert.equal(u.pathname, '/api/v1/search');
  assert.equal(u.searchParams.get('q'), 'so what & co');
  assert.equal(u.searchParams.get('sources'), 'local');
  assert.equal(u.searchParams.get('limit'), String(C.LIMITE_INITIALE));
  const q = new URL(C.urlDeRechercheDeService(S, 'so what', 'qobuz', 24));
  assert.equal(q.searchParams.get('sources'), 'qobuz');
  assert.equal(q.searchParams.get('limit'), '24');
  // Contre-épreuve : sans `sources`, le serveur rendrait bibliothèque ET tous
  // les services en une réponse, au rythme du plus lent.
  for (const url of [C.urlDeRecherche(S, 'x'), ...C.SERVICES_DE_RECHERCHE.map((s) => C.urlDeRechercheDeService(S, 'x', s.cle))]) {
    const v = new URL(url).searchParams.get('sources');
    assert.ok(v && v !== 'all', url);
  }
  assert.deepEqual(C.SERVICES_DE_RECHERCHE.map((s) => s.cle), ['qobuz', 'tidal']);
});

test('les corps de lecture : ceux du client web (SearchV2, lireArtiste)', () => {
  const l = C.resultatsLocaux(REPONSE_LOCALE);
  assert.deepEqual(C.corpsDeLecture(l.tracks.items[0]), { track_id: 42 });
  assert.deepEqual(C.corpsDeLecture(l.albums.items[0]), { album_id: 100 });
  assert.deepEqual(C.corpsDeLecture(l.artists.items[0]), { artist_id: 3 });
  const q = C.resultatsDeService(REPONSE_QOBUZ, 'qobuz');
  assert.deepEqual(C.corpsDeLecture(q.tracks.items[0]), {
    source: 'qobuz', source_id: '5966783', title: 'So What', artist_name: 'Miles Davis',
    album_title: 'Kind of Blue', cover_path: 'https://static.qobuz.com/images/covers/87/70/0886443927087_600.jpg',
    duration_ms: 562000,
  });
  // La PAIRE source + streaming_album_id : un identifiant seul fait
  // « reprendre la lecture » côté serveur.
  assert.deepEqual(C.corpsDeLecture(q.albums.items[0]), { source: 'qobuz', streaming_album_id: '0886443927087' });
  // Contre-épreuve : un identifiant de service n'est jamais envoyé comme
  // `track_id` / `album_id` (entiers de la bibliothèque), et un élément local
  // ne porte jamais `source`.
  for (const i of [...q.tracks.items, ...q.albums.items]) {
    const c = C.corpsDeLecture(i);
    assert.ok(!('track_id' in c) && !('album_id' in c), JSON.stringify(c));
  }
  for (const i of [...l.tracks.items, ...l.albums.items, ...l.artists.items]) {
    assert.ok(!('source' in C.corpsDeLecture(i)));
  }
});

test('les vignettes passent par les routes de pochette du serveur', () => {
  const h = '0123456789abcdef0123456789abcdef';
  assert.equal(C.urlDeVignette(S, h), `${S}/api/v1/library/artwork/${h}?size=80`);
  assert.equal(C.urlDeVignette(S, `/api/v1/library/artwork/${h}`), `${S}/api/v1/library/artwork/${h}?size=80`);
  const qobuz = 'https://static.qobuz.com/images/covers/87/70/0886443927087_600.jpg';
  assert.equal(C.urlDeVignette(S, qobuz), `${S}/api/v1/library/artwork/proxy?url=${encodeURIComponent(qobuz)}`);
  // Un relais déjà fait n'est pas re-dimensionné (seul le condensat lit ?size=).
  assert.equal(C.urlDeVignette(S, '/api/v1/library/artwork/proxy?url=a'), `${S}/api/v1/library/artwork/proxy?url=a`);
  // Une pochette de CE serveur n'est pas relayée par lui-même.
  assert.equal(C.urlDeVignette(S, `${S}/api/v1/library/artwork/${h}`), `${S}/api/v1/library/artwork/${h}`);
  assert.equal(C.urlDeVignette(S, null), null);
});

test('« Voir plus » révèle ce qui est reçu, puis redemande au serveur', () => {
  const albums = C.resultatsLocaux(REPONSE_LOCALE).albums; // 8 reçus, suite annoncée
  assert.equal(C.PAR_CATEGORIE.tracks, 6);
  assert.equal(C.PAR_CATEGORIE.albums, 6);
  assert.equal(C.PAR_CATEGORIE.artists, 4);
  assert.equal(C.aEncore(albums, 6, 12), true);
  // 6 → 12 : 8 reçus seulement, le serveur a une suite → redemander.
  assert.deepEqual(C.voirPlus(albums, 6, 6, 12), { visibles: 12, limite: 24 });
  // Tout reçu et pas de suite : on montre, on ne redemande rien.
  const pistes = C.resultatsLocaux(REPONSE_LOCALE).tracks;
  assert.equal(C.aEncore(pistes, 6, 12), false);
  assert.deepEqual(C.voirPlus(pistes, 6, 6, 12), { visibles: 12, limite: null });
  // Au plafond du serveur (50), plus de redemande.
  assert.equal(C.voirPlus(albums, 48, 6, 50).limite, null);
  assert.equal(C.aEncore({ items: albums.items, aSuite: true }, 8, 50), false);
});

test('le volume envoyé est TOUJOURS une fraction : 1 % ne devient pas 100 %', () => {
  // Le serveur lit `volume > 1.0` comme un pourcentage et tout le reste comme
  // une fraction : l'ancien `{volume: 1}` valait le plein volume.
  assert.ok(C.corpsDeVolume(1, 'linear').volume < 0.02);
  assert.equal(C.corpsDeVolume(100, 'linear').volume, 1);
  assert.equal(C.corpsDeVolume(0, 'linear').volume, 0);
  for (let p = 0; p <= 100; p++) {
    for (const courbe of ['linear', 'db60', undefined]) {
      const v = C.corpsDeVolume(p, courbe).volume;
      assert.ok(v >= 0 && v <= 1, `position ${p} (${courbe}) → ${v}`);
    }
  }
});

test('sur une sortie logicielle (`db60`), le curseur suit la loi en dB du serveur (#6787)', () => {
  // Repères de `volume_scale` : 50 % → −30 dB, 25 % → −45 dB, 10 % → −54 dB.
  const db = (g) => 20 * Math.log10(g);
  assert.ok(Math.abs(db(C.gainDuCurseur(50, 'db60')) + 30) < 1e-9);
  assert.ok(Math.abs(db(C.gainDuCurseur(25, 'db60')) + 45) < 1e-9);
  assert.ok(Math.abs(db(C.gainDuCurseur(10, 'db60')) + 54) < 1e-9);
  // Une zone à 5 % de gain (−26 dB) se lit à 56 ou 57 sur le curseur, pas à 5.
  const p = C.positionDuCurseur(0.05, 'db60');
  assert.ok(p >= 56 && p <= 57, `position ${p}`);
});

test('relire puis renvoyer la position ne monte jamais le son', () => {
  for (const courbe of ['linear', 'db60']) {
    for (let i = 0; i <= 1000; i++) {
      const g = i / 1000;
      const p = C.positionDuCurseur(g, courbe);
      assert.ok(C.gainDuCurseur(p, courbe) <= g + 1e-12, `${courbe} gain ${g} → ${p}`);
    }
  }
});

test('un serveur ancien qui publiait 0..100 est encore lu', () => {
  assert.equal(C.positionDuCurseur(40, 'linear'), 40);
});

test('les événements se lisent par type EXACT et par zone', () => {
  const z = 3;
  assert.deepEqual(C.lireEvenement({ type: 'playback.volume', data: { volume: 0.3, zone_id: 3 } }, z), {
    genre: 'volume',
    data: { volume: 0.3, zone_id: 3 },
  });
  // #5662 : le bandeau « volume changé ailleurs » n'est pas une consigne.
  assert.equal(
    C.lireEvenement({ type: 'zone.volume_externe', data: { zone_id: 3, renderer_volume: 0.8, actif: true } }, z),
    null,
  );
  assert.equal(C.lireEvenement({ type: 'playback.paused', data: { zone_id: 4 } }, z), null);
  assert.equal(C.lireEvenement({ type: 'snapshot', data: { zones: [] } }, z), null);
  assert.equal(C.lireEvenement({ type: 'playback.track_changed', data: { zone_id: 3, title: 'T' } }, z).genre, 'piste');
  assert.equal(C.lireEvenement({ type: 'playback.position', data: { zone_id: 3, position_ms: 10 } }, z).genre, 'position');
});

test('les quatre formes de pochette publiées par le serveur', () => {
  assert.equal(C.urlDePochette(S, 'https://cdn.example/x.jpg'), 'https://cdn.example/x.jpg');
  assert.equal(C.urlDePochette(S, '/api/v1/library/artwork/proxy?url=a'), `${S}/api/v1/library/artwork/proxy?url=a`);
  const h = '0123456789abcdef0123456789abcdef';
  assert.equal(C.urlDePochette(S, h), `${S}/api/v1/library/artwork/${h}`);
  assert.equal(C.urlDePochette(S, '/cache/artwork/abc.jpg'), `${S}/api/v1/library/artwork/abc.jpg`);
  assert.equal(C.urlDePochette(S, null), null);
});

test('l’adresse saisie est normalisée, et le défaut est la machine elle-même', () => {
  assert.equal(C.normaliserServeur(' 192.168.1.20:8888/ '), 'http://192.168.1.20:8888');
  assert.equal(C.normaliserServeur('https://tune.example'), 'https://tune.example');
  assert.equal(C.normaliserServeur(''), '');
  assert.equal(C.SERVEUR_PAR_DEFAUT, 'http://localhost:8888');
});

test('la liste des zones accepte le tableau que rend GET /zones', () => {
  assert.equal(C.listeDeZones([{ id: 1 }]).length, 1);
  assert.equal(C.listeDeZones({ zones: [{ id: 1 }, { id: 2 }] }).length, 2);
  assert.deepEqual(C.listeDeZones(null), []);
});
