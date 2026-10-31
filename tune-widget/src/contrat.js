// Contrat du mini-lecteur avec le serveur Tune : tout ce qui lit une réponse
// du serveur ou fabrique une requête, sans toucher au DOM.
//
// Séparé de `app.js` pour être éprouvé hors de la fenêtre (`node --test
// tune-widget/tests`), parce que c'est exactement là que le widget avait
// dérivé sans que rien ne le voie : la recherche lisait `tracks` à la racine
// alors que le serveur les range sous `local` depuis #3189, le curseur de
// volume envoyait des pourcentages qu'une valeur de 1 transformait en plein
// volume, et un événement `zone.volume_externe` remettait le curseur à zéro.
//
// Chargé deux fois : par la page (`<script src="contrat.js">`, il pose alors
// `window.TuneContrat`) et par les tests (`require`).
(function (racine, fabrique) {
  const contrat = fabrique();
  if (typeof module === 'object' && module.exports) module.exports = contrat;
  else racine.TuneContrat = contrat;
})(typeof self !== 'undefined' ? self : this, function () {
  'use strict';

  /** Adresse par défaut : le serveur de la machine elle-même. */
  const SERVEUR_PAR_DEFAUT = 'http://localhost:8888';

  /** `192.168.1.20:8888` → `http://192.168.1.20:8888`, sans `/` final. */
  function normaliserServeur(saisie) {
    const brut = String(saisie || '').trim().replace(/\/+$/, '');
    if (!brut) return '';
    return /^https?:\/\//i.test(brut) ? brut : `http://${brut}`;
  }

  /**
   * URL d'une pochette à partir du `cover_path` publié par le serveur.
   *
   * Les quatre formes que le serveur publie (même lecture que `artworkUrl` du
   * client web et `upnp_server::artwork_url`) : une URL distante, un chemin
   * déjà relatif à l'API, un condensat du cache, un nom de fichier du cache.
   */
  function urlDePochette(serveur, chemin) {
    if (!chemin) return null;
    const c = String(chemin);
    if (/^https?:\/\//i.test(c)) return c;
    if (c.startsWith('/api/')) return `${serveur}${c}`;
    const fichier = c.split('/').pop();
    if (!fichier) return null;
    return `${serveur}/api/v1/library/artwork/${encodeURIComponent(fichier)}`;
  }

  /**
   * Recherche : la bibliothèque LOCALE seulement. Les résultats des services
   * de streaming n'ont pas d'identifiant numérique, le widget ne saurait pas
   * les lire, et les attendre ralentissait chaque frappe.
   */
  function urlDeRecherche(serveur, requete, limite = 8) {
    return `${serveur}/api/v1/search?q=${encodeURIComponent(requete)}&limit=${limite}&sources=local`;
  }

  /**
   * Les pistes et albums d'une réponse de `GET /search`. Le serveur les range
   * sous `local` (#3189) ; la racine reste lue pour un serveur très ancien.
   */
  function resultatsDeRecherche(reponse, parType = 4) {
    const r = reponse && typeof reponse === 'object' ? reponse : {};
    const bloc = r.local && typeof r.local === 'object' ? r.local : r;
    const pistes = Array.isArray(bloc.tracks) ? bloc.tracks : [];
    const albums = Array.isArray(bloc.albums) ? bloc.albums : [];
    const items = [];
    pistes
      .filter((t) => Number.isInteger(t && t.id))
      .slice(0, parType)
      .forEach((t) => items.push({ type: 'track', id: t.id, title: t.title || '', artist: t.artist_name || '' }));
    albums
      .filter((a) => Number.isInteger(a && a.id))
      .slice(0, parType)
      .forEach((a) => items.push({ type: 'album', id: a.id, title: a.title || '', artist: a.artist_name || '' }));
    return items;
  }

  /** Corps de `POST /zones/{id}/play` pour un résultat de recherche. */
  function corpsDeLecture(item) {
    return item.type === 'album' ? { album_id: item.id } : { track_id: item.id };
  }

  /** `GET /zones` rend un tableau ; on accepte aussi `{ zones: [...] }`. */
  function listeDeZones(reponse) {
    if (Array.isArray(reponse)) return reponse;
    if (reponse && Array.isArray(reponse.zones)) return reponse.zones;
    return [];
  }

  // ── Volume (#1274, #6787) ────────────────────────────────────────────────
  //
  // `volume` est TOUJOURS un gain linéaire 0..1, en lecture comme en écriture.
  // Sur une sortie logicielle, la zone publie `volume_curve: "db60"` et le
  // curseur est en dB : 60 dB de course, une droite sous le genou de 10 %.
  // Mêmes formules que `tune_core::audio::volume_scale`.

  const PLAGE_PERCEPTIVE_DB = 60;
  const GENOU_PERCEPTIF = 0.1;
  const GAIN_AU_GENOU = Math.pow(10, (-PLAGE_PERCEPTIVE_DB * (1 - GENOU_PERCEPTIF)) / 20);

  function gainPerceptif(position) {
    if (!(position > 0)) return 0;
    if (position >= 1) return 1;
    if (position < GENOU_PERCEPTIF) return (GAIN_AU_GENOU * position) / GENOU_PERCEPTIF;
    return Math.pow(10, (-PLAGE_PERCEPTIVE_DB * (1 - position)) / 20);
  }

  function positionPerceptive(gain) {
    if (!(gain > 0)) return 0;
    if (gain >= 1) return 1;
    if (gain < GAIN_AU_GENOU) return Math.min((gain / GAIN_AU_GENOU) * GENOU_PERCEPTIF, GENOU_PERCEPTIF);
    return Math.min(Math.max(1 + (20 * Math.log10(gain)) / PLAGE_PERCEPTIVE_DB, GENOU_PERCEPTIF), 1);
  }

  /**
   * Position du curseur (entier 0..100) pour le gain `volume` de la zone.
   * Arrondie vers le BAS : renvoyer la position lue ne monte jamais le son.
   */
  function positionDuCurseur(volume, courbe) {
    let g = Number(volume);
    if (!Number.isFinite(g) || g < 0) g = 0;
    // Un serveur d'avant #1274 pouvait publier 0..100.
    if (g > 1) g = g / 100;
    const p = courbe === 'db60' ? positionPerceptive(g) : Math.min(g, 1);
    let points = Math.floor(p * 100 + 1e-9);
    while (points > 0 && gainDuCurseur(points, courbe) > g + 1e-12) points -= 1;
    return Math.max(0, Math.min(100, points));
  }

  /** Gain linéaire 0..1 à envoyer pour la position `points` (0..100). */
  function gainDuCurseur(points, courbe) {
    let p = Number(points);
    if (!Number.isFinite(p)) p = 0;
    p = Math.max(0, Math.min(100, p)) / 100;
    return courbe === 'db60' ? gainPerceptif(p) : p;
  }

  /** Corps de `PUT /zones/{id}/volume` : toujours une fraction. */
  function corpsDeVolume(points, courbe) {
    return { volume: gainDuCurseur(points, courbe) };
  }

  // ── WebSocket ────────────────────────────────────────────────────────────

  const TYPES_DE_LECTURE = {
    'playback.started': 'piste',
    'playback.track_changed': 'piste',
    'playback.paused': 'pause',
    'playback.resumed': 'reprise',
    'playback.stopped': 'arret',
    'playback.position': 'position',
    'playback.seek': 'position',
    'playback.volume': 'volume',
  };

  /**
   * Ce qu'un message du WebSocket demande au widget, ou `null`.
   *
   * Lecture par type EXACT : un test « contient volume » prenait aussi
   * `zone.volume_externe` (#5662), qui n'a pas de champ `volume`, et remettait
   * le curseur à zéro. Les messages d'une autre zone sont ignorés ; ceux qui
   * n'en nomment aucune aussi, faute de savoir à qui ils s'adressent.
   */
  function lireEvenement(message, zoneId) {
    if (!message || typeof message !== 'object') return null;
    const genre = TYPES_DE_LECTURE[message.type];
    if (!genre) return null;
    const data = message.data && typeof message.data === 'object' ? message.data : {};
    if (data.zone_id !== zoneId) return null;
    if (genre === 'volume' && typeof data.volume !== 'number') return null;
    return { genre, data };
  }

  return {
    SERVEUR_PAR_DEFAUT,
    normaliserServeur,
    urlDePochette,
    urlDeRecherche,
    resultatsDeRecherche,
    corpsDeLecture,
    listeDeZones,
    gainPerceptif,
    positionPerceptive,
    positionDuCurseur,
    gainDuCurseur,
    corpsDeVolume,
    lireEvenement,
  };
});
