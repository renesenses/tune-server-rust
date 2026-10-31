// Témoins du contrat widget ↔ serveur. `node --test tune-widget/tests`.
//
// Les charges utiles ci-dessous recopient la forme que rend
// `origin/batch/bugs-rc10-20261030` : `routes/search.rs` (bloc `local`),
// `routes/zones/lecture.rs` (`volume`, `volume_curve`), `routes/ws.rs`
// (`{type, data}` avec `data.zone_id`), `tune_core::audio::volume_scale`.
'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const C = require(path.join(__dirname, '..', 'src', 'contrat.js'));

const S = 'http://tune.local:8888';

test('la recherche lit les pistes et albums sous `local`, comme le serveur les range (#3189)', () => {
  const reponse = {
    local: {
      artists: [],
      albums: [{ id: 7, title: 'Kind of Blue', artist_name: 'Miles Davis' }],
      tracks: [{ id: 42, title: 'So What', artist_name: 'Miles Davis' }],
      totals: { artists: 0, albums: 1, tracks: 1 },
    },
    radios: [],
    services: { qobuz: { tracks: [{ id: 'q1', title: 'So What' }] } },
  };
  assert.deepEqual(C.resultatsDeRecherche(reponse), [
    { type: 'track', id: 42, title: 'So What', artist: 'Miles Davis' },
    { type: 'album', id: 7, title: 'Kind of Blue', artist: 'Miles Davis' },
  ]);
});

test('la recherche ne demande que la bibliothèque locale', () => {
  const u = new URL(C.urlDeRecherche(S, 'so what & co'));
  assert.equal(u.pathname, '/api/v1/search');
  assert.equal(u.searchParams.get('q'), 'so what & co');
  assert.equal(u.searchParams.get('sources'), 'local');
});

test('un résultat sans identifiant numérique n’est pas proposé', () => {
  const r = C.resultatsDeRecherche({ local: { tracks: [{ id: 'abc', title: 'x' }, { title: 'y' }], albums: [] } });
  assert.deepEqual(r, []);
});

test('le corps de lecture nomme la piste ou l’album', () => {
  assert.deepEqual(C.corpsDeLecture({ type: 'track', id: 3 }), { track_id: 3 });
  assert.deepEqual(C.corpsDeLecture({ type: 'album', id: 9 }), { album_id: 9 });
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
