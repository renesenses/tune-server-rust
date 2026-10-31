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

  // ── Recherche (#7595) ────────────────────────────────────────────────────
  //
  // `GET /api/v1/search` (`tune-server/src/routes/search.rs`,
  // `federated_search`) rend, quel que soit `sources` :
  //
  //   { local: { artists: [Artist], albums: [Album], tracks: [Track],
  //              totals: {artists, albums, tracks, tracks_via_metadata},
  //              totals_capped: {…}, has_more: {artists, albums, tracks},
  //              limit, offset },
  //     radios: [...], radios_annuaire: {...},
  //     services: { <nom>: { tracks, albums, artists, playlists } } }
  //
  // * `local.*` sont les modèles de `tune_core::db::models` : `id` entier,
  //   `artists[].name` + `image_path`, `albums[].title` + `artist_name` +
  //   `cover_path`, `tracks[].title` + `artist_name` + `album_title` +
  //   `cover_path` (celle de la piste, sinon celle de l'album).
  // * `services.<nom>` est un `SearchResults` (`tune_core::streaming::traits`) :
  //   pistes et albums sont identifiés par `source_id` (CHAÎNE), jamais par
  //   `id` ; `cover_path` y est une URL distante (Qobuz, TIDAL). Le nom du
  //   service n'est que la CLÉ du bloc : rien dans l'élément ne le répète.
  // * `sources` est une liste blanche (#3226) : `local` → la bibliothèque et
  //   aucun service ; `qobuz` → ce service seul, `local` présent mais vide.
  //   Un service non connecté (ou désactivé, #5103) n'a simplement pas de bloc.
  // * `limit` vaut pour CHACUNE des trois listes locales et pour chaque
  //   catégorie d'un service (plafonné à 50 côté serveur).

  /** Ce que la fenêtre montre d'abord, et ce qu'ajoute chaque « Voir plus ». */
  const PAR_CATEGORIE = { artists: 4, albums: 6, tracks: 6 };
  /** Ordre et intitulés des catégories. */
  const CATEGORIES = ['artists', 'albums', 'tracks'];
  const NOMS_DE_CATEGORIE = { artists: 'Artistes', albums: 'Albums', tracks: 'Titres' };
  /** Première demande : deux pages de chaque catégorie, pour un « Voir plus » immédiat. */
  const LIMITE_INITIALE = 12;
  /** Le plafond par catégorie que la route applique aux services. */
  const PLAFOND_DE_LIMITE = 50;
  /**
   * Les services interrogés, dans l'ordre d'affichage, après la bibliothèque.
   * Une requête PAR service : chacun s'affiche dès qu'il répond, sans attendre
   * le plus lent.
   */
  const SERVICES_DE_RECHERCHE = [
    { cle: 'qobuz', nom: 'Qobuz' },
    { cle: 'tidal', nom: 'TIDAL' },
  ];

  function urlAvecSources(serveur, requete, sources, limite) {
    return `${serveur}/api/v1/search?q=${encodeURIComponent(requete)}&limit=${limite}&sources=${encodeURIComponent(sources)}`;
  }

  /** La bibliothèque seule : la réponse la plus rapide, affichée d'abord. */
  function urlDeRecherche(serveur, requete, limite = LIMITE_INITIALE) {
    return urlAvecSources(serveur, requete, 'local', limite);
  }

  /** Un service seul (`qobuz`, `tidal`) : le bloc `local` revient vide. */
  function urlDeRechercheDeService(serveur, requete, service, limite = LIMITE_INITIALE) {
    return urlAvecSources(serveur, requete, service, limite);
  }

  const tableau = (x) => (Array.isArray(x) ? x : []);
  const texte = (x) => (typeof x === 'string' ? x : '');
  const entierOuNull = (x) => (Number.isInteger(x) && x >= 0 ? x : null);

  function categorie(items, total, aSuite) {
    return { items, total, aSuite: !!aSuite };
  }

  /**
   * Le bloc `local` d'une réponse, rangé par catégorie. Seuls les éléments à
   * `id` entier sont gardés : c'est lui que `POST /zones/{id}/play` attend.
   * La racine est encore lue pour un serveur d'avant #3189.
   */
  function resultatsLocaux(reponse) {
    const r = reponse && typeof reponse === 'object' ? reponse : {};
    const bloc = r.local && typeof r.local === 'object' ? r.local : r;
    const totaux = bloc.totals && typeof bloc.totals === 'object' ? bloc.totals : {};
    const suites = bloc.has_more && typeof bloc.has_more === 'object' ? bloc.has_more : {};
    const avecId = (x) => x && Number.isInteger(x.id);
    const artists = tableau(bloc.artists).filter(avecId).map((a) => ({
      source: 'local', type: 'artist', id: a.id,
      title: texte(a.name), subtitle: '', cover: a.image_path || null,
    }));
    const albums = tableau(bloc.albums).filter(avecId).map((a) => ({
      source: 'local', type: 'album', id: a.id,
      title: texte(a.title), subtitle: texte(a.artist_name), cover: a.cover_path || null,
    }));
    const tracks = tableau(bloc.tracks).filter(avecId).map((t) => ({
      source: 'local', type: 'track', id: t.id,
      title: texte(t.title),
      subtitle: [texte(t.artist_name), texte(t.album_title)].filter(Boolean).join(' — '),
      cover: t.cover_path || null,
    }));
    return {
      artists: categorie(artists, entierOuNull(totaux.artists), suites.artists),
      albums: categorie(albums, entierOuNull(totaux.albums), suites.albums),
      tracks: categorie(tracks, entierOuNull(totaux.tracks), suites.tracks),
    };
  }

  /**
   * Le bloc `services.<service>` d'une réponse, rangé par catégorie, ou `null`
   * quand le service n'a pas répondu (non connecté, désactivé, en échec).
   *
   * Les artistes d'un service ne sont PAS proposés : `POST /zones/{id}/play`
   * n'a pas de champ pour en lire un (seulement `artist_id`, de la
   * bibliothèque). Une piste marquée `disponible: false` non plus : le
   * service a dit qu'il ne la servirait pas.
   *
   * Sans pagination demandée, le serveur ne dit pas s'il reste des résultats :
   * une catégorie PLEINE (autant d'éléments que `limite`) peut en avoir.
   */
  function resultatsDeService(reponse, service, limite = LIMITE_INITIALE) {
    const services = reponse && typeof reponse === 'object' && reponse.services;
    const bloc = services && typeof services === 'object' ? services[service] : null;
    if (!bloc || typeof bloc !== 'object') return null;
    const avecSourceId = (x) => x && (typeof x.source_id === 'string' || Number.isFinite(x.source_id)) && String(x.source_id) !== '';
    const pistesBrutes = tableau(bloc.tracks);
    const albumsBruts = tableau(bloc.albums);
    const tracks = pistesBrutes
      .filter((t) => avecSourceId(t) && t.disponible !== false)
      .map((t) => ({
        source: service, type: 'track', id: String(t.source_id),
        title: texte(t.title),
        subtitle: [texte(t.artist_name), texte(t.album_title)].filter(Boolean).join(' — '),
        cover: t.cover_path || null,
        // Ce que la lecture d'une piste distante transmet pour se décrire
        // (même corps que `lirePiste` du client web, SearchV2.svelte).
        artist_name: t.artist_name ?? null,
        album_title: t.album_title ?? null,
        duration_ms: Number.isFinite(t.duration_ms) ? t.duration_ms : undefined,
      }));
    const albums = albumsBruts.filter(avecSourceId).map((a) => ({
      source: service, type: 'album', id: String(a.source_id),
      title: texte(a.title), subtitle: texte(a.artist_name), cover: a.cover_path || null,
    }));
    return {
      artists: categorie([], 0, false),
      albums: categorie(albums, null, albumsBruts.length >= limite),
      tracks: categorie(tracks, null, pistesBrutes.length >= limite),
    };
  }

  /**
   * « Voir plus » sur une catégorie qui montre `visibles` éléments.
   *
   * Rend le nouveau nombre à montrer et, s'il dépasse ce qui est déjà reçu
   * alors que le serveur a une suite, la limite à redemander pour cette
   * source (`null` sinon : tout est déjà là).
   */
  function voirPlus(cat, visibles, pas, limiteActuelle) {
    const recus = cat && Array.isArray(cat.items) ? cat.items.length : 0;
    const voulus = visibles + pas;
    let limite = null;
    if (voulus > recus && cat && cat.aSuite && limiteActuelle < PLAFOND_DE_LIMITE) {
      limite = Math.min(PLAFOND_DE_LIMITE, Math.max(voulus, limiteActuelle * 2));
    }
    return { visibles: voulus, limite };
  }

  /** Reste-t-il quelque chose à montrer derrière un « Voir plus » ? */
  function aEncore(cat, visibles, limiteActuelle) {
    if (!cat || !Array.isArray(cat.items)) return false;
    if (cat.items.length > visibles) return true;
    return !!cat.aSuite && limiteActuelle < PLAFOND_DE_LIMITE;
  }

  /**
   * Corps de `POST /zones/{id}/play` pour un résultat, mêmes corps que le
   * client web (`SearchV2.svelte` : `playTrack`, `lirePiste`, `lireDistant` ;
   * `lib/lireArtiste`) :
   *
   * * piste locale → `{ track_id }` ; album local → `{ album_id }` ;
   * * artiste local → `{ artist_id }` : le serveur tire jusqu'à 50 de ses
   *   titres au hasard (#6467, `lire_artiste_6467`), contexte `artist` ;
   * * piste de service → `{ source, source_id, title, artist_name,
   *   album_title, cover_path, duration_ms }` : sans ligne en bibliothèque,
   *   c'est par ces champs que la piste dit qui elle est ;
   * * album de service → `{ source, streaming_album_id }` — la PAIRE : un
   *   identifiant seul fait retomber le serveur sur « reprendre la lecture ».
   */
  function corpsDeLecture(item) {
    const local = !item.source || item.source === 'local';
    if (local) {
      if (item.type === 'album') return { album_id: item.id };
      if (item.type === 'artist') return { artist_id: item.id };
      return { track_id: item.id };
    }
    if (item.type === 'album') return { source: item.source, streaming_album_id: String(item.id) };
    const corps = {
      source: item.source,
      source_id: String(item.id),
      title: item.title || null,
      artist_name: item.artist_name ?? null,
      album_title: item.album_title ?? null,
      cover_path: item.cover || null,
    };
    if (Number.isFinite(item.duration_ms)) corps.duration_ms = item.duration_ms;
    return corps;
  }

  /**
   * Vignette d'un résultat (≈ 36 px affichés, 80 demandés pour l'écran Retina),
   * par les routes de pochette existantes :
   *
   * * une pochette du cache local → `/api/v1/library/artwork/{condensat}?size=80`
   *   (cases `THUMB_SIZES` de `tune_core::library::artwork`) ;
   * * une pochette distante (Qobuz, TIDAL) → `/api/v1/library/artwork/proxy?url=`,
   *   comme `artworkUrl` du client web — le relais du serveur admet ces hôtes
   *   (`artwork_proxy::HOTES_AUTORISES`) ;
   * * une adresse déjà sous `/api/` est servie telle quelle.
   */
  function urlDeVignette(serveur, chemin, taille = 80) {
    if (!chemin) return null;
    const c = String(chemin);
    if (/^https?:\/\//i.test(c)) {
      if (serveur && c.startsWith(`${serveur}/`)) return c;
      return `${serveur}/api/v1/library/artwork/proxy?url=${encodeURIComponent(c)}`;
    }
    if (c.startsWith('/api/')) {
      return /^\/api\/v1\/library\/artwork\/(?!proxy(?:[/?]|$))[^/?#]+$/.test(c) ? `${serveur}${c}?size=${taille}` : `${serveur}${c}`;
    }
    const fichier = c.split('/').pop();
    if (!fichier) return null;
    return `${serveur}/api/v1/library/artwork/${encodeURIComponent(fichier)}?size=${taille}`;
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
    PAR_CATEGORIE,
    CATEGORIES,
    NOMS_DE_CATEGORIE,
    LIMITE_INITIALE,
    PLAFOND_DE_LIMITE,
    SERVICES_DE_RECHERCHE,
    urlDeRecherche,
    urlDeRechercheDeService,
    resultatsLocaux,
    resultatsDeService,
    voirPlus,
    aEncore,
    corpsDeLecture,
    urlDeVignette,
    listeDeZones,
    gainPerceptif,
    positionPerceptive,
    positionDuCurseur,
    gainDuCurseur,
    corpsDeVolume,
    lireEvenement,
  };
});
