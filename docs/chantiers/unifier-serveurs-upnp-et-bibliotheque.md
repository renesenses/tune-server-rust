# Unifier les serveurs multimédia UPnP et la bibliothèque

**Reconnaissance en lecture seule.** Relevé sur `origin/batch/refonte-coeur-2`
(`33af7382`), 13 septembre 2026. Aucun fichier de production n'est modifié par
ce document.

Le degré retenu par Bertrand est le plus fort des trois : **indexer le contenu
des serveurs distants dans la bibliothèque locale**, pour qu'il devienne
triable, filtrable et cherchable comme le contenu local. Les deux degrés plus
légers (navigation unifiée, recherche unifiée) sont écartés en connaissance de
cause. Ce document dit ce qui existe, ce que ça coûte, ce qui reste à décider,
et propose une découpe.

Tout ce qui est chiffré ici a été mesuré. Les mesures réseau ont été faites le
13/09 depuis le Mac (`192.168.1.41`) contre les serveurs réellement présents.
Les mesures de code sont des `grep` sur l'arbre, hors `.claude/worktrees/`,
`target/` et `vendor/`.

---

## Ce que les mesures changent au plan

Quatre constats renversent une partie des hypothèses de départ.

1. **La bibliothèque accepte déjà des pistes sans fichier.** `tracks.source`
   existe depuis l'origine avec `DEFAULT 'local'`, `tracks.file_path` est
   `Option<String>` côté Rust et nullable en base, et le vocabulaire vivant est
   `local | qobuz | tidal | radio | podcast | bandcamp`. Il n'y a **pas de
   nouveau schéma à inventer** pour porter une piste distante.
2. **`source = "upnp"` est déjà routé dans l'orchestrateur** — mais pour Tune
   *renderer*, pas pour Tune *client de serveur média*. La branche de lecture
   d'une URL distante existe et fonctionne ; c'est le reste de la chaîne (DSP,
   seek, OAAT) qui n'y est pas branché.
3. **L'identifiant d'une piste sur un serveur Tune est son `tracks.id`**, et un
   rescan complet réattribue les `id` dans l'ordre de parcours. L'ObjectID **ne
   survit donc pas** à une réindexation du serveur distant. C'est le point dur
   du chantier, et il n'a pas de solution propre côté protocole.
4. **`track_source_links` est morte en écriture** : zéro écrivain hors tests
   unitaires dans tout le dépôt. Ce n'est pas une structure éprouvée qu'on
   réutilise, c'est une structure jamais mise en service qu'on ressusciterait.

---

## Le terrain mesuré

### Les trois mondes, et lequel est vraiment un précédent

| Monde | Où | Persistance | Ce qu'il unifie |
|---|---|---|---|
| Bibliothèque | `tune-server/src/routes/library/`, `tune-core/src/db/` | SQLite + PostgreSQL, indexée | tout le local |
| Serveurs UPnP distants | `tune-server/src/routes/network.rs:921` (`browse`), `:1062` (`search`) | **aucune** — DIDL parsé et jeté | rien |
| Multi-serveur Tune | `tune-server/src/routes/multi_server.rs` (485 l.) | une clé de réglages `multi_server_list` | les zones, en éventail |

**Le multi-serveur est à écarter comme modèle, pas à imiter.** Trois raisons,
lisibles dans les 485 lignes du fichier :

- il ne passe **pas par le réseau local** : `proxy_to_remote`
  (`multi_server.rs:66`) tape `https://bridge.mozaiklabs.fr/api/relay/{server_id}/…`
  avec un `BridgeToken`. C'est un relais cloud, sous licence
  (`Feature::MultiServer`, sept appels à `require_premium`), pour des serveurs
  **Tune** enregistrés à la main. Un serveur UPnP tiers n'a ni `server_id`
  Mozaik, ni jeton de pont ;
- `unified/zones` (`multi_server.rs:412`) est une **agrégation à la volée** :
  elle interroge chaque serveur en parallèle, colle `server_name` / `server_id`
  / `remote` sur chaque zone, et rend le tout. Rien n'est écrit. Le motif
  s'appelle *fan-out*, pas *indexation* — c'est exactement le degré 1 que
  Bertrand a écarté ;
- il stocke sa liste de serveurs dans une **valeur JSON de la table
  `settings`**, pas dans une table. Aucune contrainte, aucun index, aucune
  jointure possible. Une bibliothèque de 22 000 pistes distantes n'y tient pas.

Ce qui, en revanche, **est** le bon précédent, et il est déjà écrit noir sur
blanc dans le dépôt : `streaming_item_tags`
(`tune-core/src/db/migrations.rs:1693-1720`). La doctrine y est posée en toutes
lettres :

> « un INSTANTANE d'affichage (`title`, `artist`, `album`, `cover_url`) pose a
> l'etiquetage. […] un album de streaming peut DISPARAITRE du catalogue, et la
> liste par etiquette doit continuer de s'afficher. Comme elle se rend depuis
> l'instantane, elle n'interroge jamais le service : **un `source_id` mort
> degrade sa pochette, il ne bloque pas l'ecran.** »

C'est mot pour mot la réponse au cycle de vie d'une piste UPnP. Le même patron
existe en trois autres exemplaires : `streaming_favorites`
(`migrations.rs:735-754`), `offline_cache` (`migrations.rs:264-287`),
`queue_items` (`tune-core/src/db/sqlite.rs:551-570`).

### Ce que le réseau porte réellement (13/09, M-SEARCH depuis le Mac)

| Adresse | Rôle | Détail |
|---|---|---|
| `192.168.1.18` | MediaServer Tune 0.9.147 | `uuid:2b0a41f8-…`, port 8888 |
| `192.168.1.42` | MediaServer Tune 0.9.146 | `uuid:2c35bec3-…`, port 8888 |
| `192.168.1.41` | MediaServer Tune (ce Mac) | `uuid:5c29ccbe-…`, port 8888 |
| `192.168.1.19` | Sonos ZPS1 86.8-78270 | `uuid:RINCON_B8E937B44D2201400_MS`, port 1400 |
| `192.168.1.20` | Sonos ZPS1 86.8-78270 | `uuid:RINCON_B8E937B44D0801400_MS`, port 1400 |
| `192.168.1.17` | renderer (AVTransport seul) | pas de MediaServer |
| `192.168.1.1` | Livebox / SoftAtHome | InternetGatewayDevice, pas de MediaServer |

**Avertissement de portée.** Aucun serveur multimédia tiers *riche*
(MinimServer, Asset, Twonky, Synology, LMS) n'est présent sur ce réseau. Les
deux Sonos annoncent bien un MediaServer, mais leur bibliothèque est **vide**
(`A:TRACKS` → `TotalMatches = 0`). Les observations d'ObjectID ci-dessous sont
donc **certaines pour un serveur Tune, partiellement extrapolées pour le
reste**. La phase 0 proposée plus bas commence par lever cette incertitude
contre trois serveurs tiers réels.

---

## Les sept questions

### 1. L'identité d'une piste distante

**Ce que le serveur rend.** Un `Browse` sur `.42`, réponse brute :

```xml
<item id="track/21825" parentID="tracks" restricted="1">
  <dc:title>Lachrimae Antiquae</dc:title>
  <dc:creator>0 Divers</dc:creator>
  <upnp:artist>0 Divers</upnp:artist>
  <upnp:class>object.item.audioItem.musicTrack</upnp:class>
  <upnp:album>MIRRORS OF TIME  - Tribute Reflections</upnp:album>
  <upnp:albumArtURI>http://192.168.1.42:8888/api/v1/library/artwork/21bb782e…</upnp:albumArtURI>
  <upnp:originalTrackNumber>1</upnp:originalTrackNumber>
  <res protocolInfo="http-get:*:application/x-dsd:*" duration="0:04:42.773"
       sampleFrequency="2822400" bitsPerSample="1" nrAudioChannels="2"
       size="199578696">http://192.168.1.42:8888/api/v1/library/tracks/21825/audio</res>
</item>
```

L'identité complète d'une piste distante est donc le couple
**(UDN du serveur, ObjectID)** — ici
`uuid:2c35bec3-15c0-4583-a6bf-6eaf8e4333e6` + `track/21825`. L'UDN est déjà ce
que Tune utilise comme clé de registre :
`MediaServerInfo.id = device_id_from_usn(usn)`
(`tune-core/src/discovery/ssdp.rs:1012`, construction `:1417`).

**Survit-il à une réindexation ? Non, pour un serveur Tune.** L'ObjectID est
`format!("track/{track_id}")` (`tune-core/src/upnp_server.rs:2678`), résolu par
`strip_prefix("track/")` (`:1753`). C'est le `tracks.id` de la base distante,
un `INTEGER PRIMARY KEY AUTOINCREMENT`. Et le dépôt documente lui-même le
problème, dans le commentaire qui justifie la table `file_first_seen`
(`tune-core/src/db/sqlite.rs:464-467`) :

> « a full rescan does DELETE FROM tracks/albums (**ids reassigned in walk
> order**), which would reset any timestamp there. »

Un scan **incrémental** préserve les `id` (upsert par `file_path`, qui est
`UNIQUE`). Un **rescan complet** les redistribue. Conséquence : après un rescan
complet du `.42`, chaque `track/NNNNN` mémorisé pointe une **autre piste**, pas
une piste absente. C'est le pire des deux mondes : pas d'erreur, une confusion
silencieuse. Le `res_url` a exactement le même défaut, puisqu'il porte le même
`id`.

**Les serveurs tiers font mieux.** Sonos utilise des ObjectID **dérivés du
contenu**, à préfixe : `A:ARTIST`, `A:ALBUMARTIST`, `A:ALBUM`, `A:GENRE`,
`A:COMPOSER`, `A:TRACKS`, `A:PLAYLISTS`, et sous eux des chemins du type
`A:ALBUM/<titre>`. Ceux-là survivent à une réindexation tant que le contenu ne
change pas de nom. MinimServer et Asset font de même. **Tune est le mauvais
élève du réseau sur ce point précis** — ce qui est ironique pour le chantier :
le serveur le plus difficile à indexer de façon stable est le nôtre.

**Le signal standard de changement est inutilisable contre Tune.** UPnP prévoit
`SystemUpdateID` (et l'`UpdateID` par conteneur) pour dire « mon catalogue a
bougé ». Mesuré sur `.42` :

```
GetSystemUpdateID → <Id>1</Id>
Browse tracks     → <UpdateID>1</UpdateID>
```

C'est une **constante littérale** dans le code : `"<Id>1</Id>"`
(`tune-core/src/upnp_server.rs:541`), et `<UpdateID>1</UpdateID>` en dur aux
lignes 642 et 1387. Le Sonos, lui, rend un vrai compteur (root `UpdateID = 6`,
`X-RINCON-BOOTSEQ: 433`). Détecter une réindexation distante par le protocole
marchera contre les tiers et **jamais contre un autre Tune**, tant que ce `1`
n'est pas remplacé par un vrai compteur.

**Ce que le parser jette.** `parse_res_elements`
(`tune-server/src/routes/network.rs:1403`) lit `protocolInfo`, `duration`,
`sampleFrequency`, `bitsPerSample`, `nrAudioChannels` — mais **pas `size`**
(`struct DidlRes`, `:1393-1400`), alors que le DIDL le fournit
(`size="199578696"` ci-dessus). Or `res@size` + `duration` + `sampleFrequency`
est le seul triplet vaguement discriminant dont on disposera pour rapprocher
une piste distante d'une piste locale sans la décoder. Le récupérer coûte trois
lignes.

### 2. Le schéma

**Rien n'est obligatoire qui n'ait de sens.** Le `CREATE TABLE tracks` n'est pas
dans `migrations.rs` mais dans `tune-core/src/db/sqlite.rs:409-448` (const
`CORE_SCHEMA`) ; `migrations.rs` ne fait qu'ajouter des colonnes ensuite.

Les seules contraintes dures sont `title NOT NULL` (que le DIDL satisfait
toujours) et `file_path TEXT UNIQUE` — **qui tolère les NULL multiples**, en
SQLite comme en PostgreSQL. Le mécanisme est déjà exploité en production par
les pistes CUE (commentaire `sqlite.rs:441-444`), dont le `file_path` est NUL.

**Colonnes de `tracks` sans objet pour une piste distante** — 13 sur ~35 :
`file_path`, `file_mtime`, `file_size`, `audio_hash`, `audio_fingerprint`,
`cover_path`, `cue_media_path`, `cue_start_ms`, `cue_end_ms`, `waveform_json`,
`acoustid_fingerprint`, `acoustid_confidence`, `trailing_silence_ms`, `bpm`.
Quatre douteuses : `format`, `sample_rate`, `bit_depth`, `channels`.

**Le ReplayGain ne coûte rien** : il n'est pas dans `tracks` mais dans la table
clé/valeur `track_metadata` (`migrations.rs:505-511`, clés `rg_track_gain`,
`rg_track_peak`, `rg_album_gain`, `rg_album_peak`, `rg_analyzed`). Pour une
piste distante, on n'écrit simplement aucune ligne.

**Un seul champ Rust ment.** `Track::file_path` est déjà `Option<String>`
(`tune-core/src/db/models.rs:145`), comme `file_mtime`, `file_size`,
`audio_hash`, `format`, `sample_rate`, `bit_depth`. Le champ gênant est
`channels: i32` avec défaut `2` (`models.rs:149`) : `to_json()` (`models.rs:200`)
en dérive un `channel_badge` **affiché à l'écran**. Une piste UPnP sans
`nrAudioChannels` annoncerait « stéréo » sans preuve.

**Le crochet existe déjà.** `tracks.source TEXT DEFAULT 'local'` +
`tracks.source_id TEXT` (`sqlite.rs:428-429`), même paire sur `albums`
(`:371-372`), index `idx_tracks_source_path ON tracks(source, file_path)`
(`migrations.rs:337`). Le test `tune-server/tests/comptes_par_source_2147.rs:55-57`
documente que le produit assume déjà des pistes sans fichier :

> « Une piste Qobuz, Tidal, radio, podcast ou Bandcamp vit dans `tracks` sans
> avoir le moindre fichier à trouver. […] 142 pistes non locales suffisent à
> produire l'écart ENTIER. »

**Le vrai coût est dans le code, pas dans le schéma.** Chiffres bruts,
`grep -rn --include="*.rs"` hors worktrees/target/vendor :

| Motif | Occurrences |
|---|---:|
| `file_path`, toutes formes | **1 248** |
| `.file_path`, accès de champ Rust | **375** |
| `Path::new(` / `PathBuf::from(` appliqué à un chemin de **piste** | **29** |
| SQL `file_path IS NOT NULL` / `!= ''` écrit à la main | **48** |
| SQL `source = 'local'` | **41** |
| `canonicalize` appliqué à un `tracks.file_path` | **0** |
| `A_UN_FICHIER`, le prédicat factorisé, réellement employé | **5** |

**Total des sites qui supposent un chemin local, au sens strict :
375 + 29 + 48 = 452.**

Top 5 des fichiers par occurrences de `file_path` :
`tune-core/src/audio/decode.rs` (117), `tune-core/src/db/track_repo.rs` (105),
`tune-core/src/orchestrator/resolve_local.rs` (54),
`tune-server/src/routes/library/tracks.rs` (35),
`tune-core/src/db/play_queue_repo.rs` (35).

La bonne nouvelle structurelle : le travail CUE a déjà créé le concept SQL
correct (`tune-core/src/db/track_repo.rs:553-566`) —

```rust
macro_rules! chemin_ouvrable {
    () => { "COALESCE(NULLIF(t.file_path, ''), NULLIF(t.cue_media_path, ''))" };
}
pub const A_UN_FICHIER: &str = concat!(chemin_ouvrable!(), " IS NOT NULL");
```

— mais il n'est utilisé qu'à **5** endroits pendant que **48** requêtes
réécrivent le prédicat à la main. **43 requêtes à convertir, 41 filtres
`source = 'local'` à auditer** : c'est le chiffre de reste-à-faire le plus
honnête du dossier. Chacun de ces 41 filtres exclut aujourd'hui silencieusement
une piste UPnP de la déduplication, des playlists de dossier, de
l'enrichissement, des statistiques, de l'export.

**Piège documenté à respecter** (#2939, `tune-server/src/routes/system/scan.rs:578-600`) :
`file_path TEXT UNIQUE` **ne connaît pas `source`**. Une carte chargée avec
`WHERE source='local'` rendait invisible une ligne d'importateur, et
l'insertion était refusée en silence — l'album entier d'Alain Bonnel, fil 1313.

### 3. La lecture

**Une piste distante se joue déjà, et le code le sait.** L'aiguillage est dans
`tune-core/src/orchestrator/commun.rs:162-193` :

| `source` | résolveur | ligne |
|---|---|---|
| `upload` | `resolve_uploaded_file` | `:172` |
| `podcast`, `radio`, **`upnp`**, `bandcamp` | **`resolve_direct_url`** | `:185-188` |
| `qobuz`, `tidal`, `deezer`… | `resolve_streaming_url` | `:189` |
| `local` / absent | `resolve_local_track` | `:192` |

Le bras terminal de `resolve_direct_url`
(`tune-core/src/orchestrator/resolve_direct.rs:421-434`) est commenté
« Media-server / podcast direct URL » et rend l'URL amont **verbatim** :

```rust
(audio_url.to_string(), None, mime_type.to_string(),
 req.sample_rate, req.bit_depth.map(|b| b as u32), None)
```

Ses deux appelants actuels sont `tune-server/src/routes/upnp_media_renderer.rs:330`
et `:505` — c'est-à-dire **Tune en tant que renderer**, qui reçoit un
`SetAVTransportURI` d'un point de contrôle tiers. La branche est donc éprouvée,
mais dans l'autre sens que celui du chantier.

**Qui résout l'URL pour les serveurs multimédia aujourd'hui ? Personne.** Les
deux routes prévues sont des **coquilles** :

```rust
// tune-server/src/routes/network.rs:1578
async fn media_server_stream_url(…) -> Json<Value> {
    Json(json!({ …, "stream_url": null,
        "message": "UPnP stream URL resolution not yet implemented" }))
}
// :1587
async fn play_media_server_item(…) -> Json<Value> {
    Json(json!({ …, "status": "not_implemented",
        "message": "UPnP media server playback not yet implemented" }))
}
```

Elles sont routées (`network.rs:57-64`) et le commentaire de
`discovery_setup.rs:139-140` s'appuie explicitement dessus pour justifier qu'un
retrait de serveur ne casse aucune lecture : « aucune lecture en cours n'en
dépend — `play_media_server_item` (`routes/network.rs`) répond
`not_implemented` ; seuls `browse` et `search` fonctionnent ».

**Le DSP ne s'applique pas.** `StreamingDsp` (ReplayGain → EQ → convolveur →
crossfeed, `tune-core/src/orchestrator.rs:986-1049`) n'a que **quatre** sites
d'injection : `resolve_local.rs:1346`, `resolve_local.rs:2300`,
`resolve_stream.rs:1680`, `resolve_stream.rs:1813`. **Aucun dans
`resolve_direct.rs`** (`grep -c load_streaming_dsp` = 0). Par sortie :

| Sortie | DSP | Transcodage |
|---|---|---|
| réseau (DLNA/OpenHome) | **aucun** — pas un octet ne traverse Tune | aucun |
| navigateur | **aucun** — `create_proxy_session(…, false)`, relais octet pour octet (`resolve_direct.rs:709-712`) | aucun |
| locale | **oui**, posé sur l'objet de sortie (`transport.rs:1459-1510`) — mais ReplayGain = 1.0, il dépend de `track_id` (`:1469-1475`) | décodage par `LocalOutput` (`outputs/local.rs:4074`) |
| OAAT | **cassé** : les bras OAAT sont fermés sur `is_radio` (`:371`) et `is_bandcamp` (`:373`) ; un item `upnp` reçoit l'URL compressée brute alors qu'OAAT « ne consomme que du PCM en conteneur WAV » (`:613-618`). Silence attendu. | aucun |

C'est exactement la panne #2863, déjà corrigée pour Qobuz/Tidal
(`resolve_stream.rs:1806-1814` force le pré-transcodage quand un DSP est actif)
et **jamais portée** sur ce chemin.

**Le relais existe pourtant.** `tune-stream-http/src/lib.rs:1645+`
(`proxy_stream`) sait déjà relayer un flux HTTP tiers arbitraire : GET amont,
recopie de `Content-Type` / `Content-Length` / `Content-Range`, ré-résolution
d'URL expirée (`send_with_reresolve`, `:1719`), reprise 206
(`resumable_proxy_body`, `:1793`). Il est branché sur Bandcamp
(`resolve_direct.rs:765`) et sur le cas navigateur (`:709`). Le chaînon manquant
est de l'utiliser aussi pour les sorties réseau et OAAT.

**Le seek est cassé sur sortie locale.** `resolve_direct.rs` ne contient **pas
une seule occurrence de `seek`** (`grep -c seek` = 0), là où `resolve_local.rs`
en a 3 et `resolve_stream.rs` en propage un depuis `:600`. Un seek sur une
source `upnp` en sortie locale ou OAAT passe par
`replay_zone_at_position` (`transport.rs:2516`) et **relance la piste à 0:00**.
En sortie réseau, `output.checked_seek()` (`:2531`) envoie un SOAP `Seek` et
c'est le renderer qui refait son `Range` sur le serveur amont — ça marche si
l'amont honore `Range`, et rien dans Tune ne le vérifie.

**Un blocage transversal à connaître** : issue **#3933**. Tout ce que le serveur
média de Tune publie dans sa DIDL vit sous `/api/v1`, donc **répond 401 quand
`auth_enabled = true`**. Un renderer n'a aucun moyen de s'authentifier. Le même
défaut frappera l'indexation croisée entre deux Tune protégés.

### 4. Le cycle de vie

Le motif existe, il est mûr, et il est en **quatre étages** que le chantier peut
reprendre tels quels.

**Étage 1 — le registre en mémoire ne retire presque jamais.** Deux politiques
délibérément distinctes dans `tune-core/src/discovery/ssdp.rs` :

| | Renderers | Serveurs multimédia |
|---|---|---|
| critère | compteur de cycles manqués | horloge |
| seuil | `MISS_GRACE_CYCLES = 3` (`:38`) | `max-age` annoncé, plancher `MEDIA_SERVER_MIN_MAX_AGE = 1800 s` (`:32`) |
| marquage | — | `MEDIA_SERVER_STALE_AFTER = 900 s` → `is_reachable() = false` (`:174`) |
| retrait | après sonde unicast échouée (`:1575-1622`) | après sonde unicast échouée (`:1644-1660`) |

La doctrine est écrite (`ssdp.rs:33-41`, forum 1425) : « **marquer ceux qui ne
répondent plus plutôt que de les retirer** ». Un `ssdp:byebye` n'est jamais cru
sur parole : debounce par `byebye_pending` puis sonde de vie (`:493-545`), parce
qu'une TV émet un byebye par service embarqué. Et `redecouverte.rs:1-45` traite
le cas « l'adresse a changé, l'entité est la même » par un M-SEARCH **unicast**
`ST: uuid:<udn>` — l'UDN est l'identité stable, le port change.

**Étage 2 — la présence des zones qualifie sans agir.**
`tune-server/src/routes/zones/presence.rs` (113 l.) : seuil
`RECENTE_SECS = 24 h` (`:19`), quatre états `en_ligne` / `eteinte_recemment` /
`absente_depuis` (avec `jours_absente`) / `jamais_vue`. **Rien n'est supprimé ni
masqué** ; c'est un champ purement descriptif. `zones.last_seen_at` n'est écrit
que par le passage **en ligne** (`zone_repo.rs:273-292`), jamais par le passage
hors ligne, avec la doctrine explicite (`migrations.rs:1659-1673`) : « poser la
date de la mise a jour sur une zone morte depuis trois semaines la ferait passer
pour recente ».

**Étage 3 — la purge de bibliothèque a quatre gardes empilées**, toutes dans
`tune-server/src/routes/system/scan.rs`, toutes payées par l'incident #1943
(21 277 pistes de Yacine supprimées) :

- **A** `verdict_purge` (`:454-481`) : `HorsPerimetre` si la liste de racines est
  vide — « une liste vide ne veut pas dire *tout est hors périmètre*, elle veut
  dire qu'on ne sait rien » ;
- **B** racine vidée : `roots_gone_empty` (`:390-408`) ;
- **C** sous-arbre vidé d'un coup : `SEUIL_SOUS_ARBRE_VIDE = 100` (`:313`) ;
- **D** plafond volumétrique : `PART_MAX_PURGE = 0.20` (`:490`), refus publié
  avec le nombre à remettre dans `?confirm_purge=N` (`:531-536`).

Le miroir le plus lisible est `tune-core/src/cloud/library_reconcile.rs:152` :
« Si `SELECT id FROM artists` rend zéro ligne, ce n'est pas *tout a disparu*,
c'est une base non montée. »

**Étage 4 — la suppression n'a lieu qu'après une observation complète et
saine.** `favorites_reconcile.rs:22-24` : « Un favori vraiment introuvable
**n'est supprimé qu'après un scan COMPLET et sain** (`delete_unresolved`) —
jamais au démarrage ni sur un scan partiel. » Et `LOCAL_ITEM_TYPES`
(`:48`) : « Tout autre `item_type` (**streaming**…) est ignoré — et surtout
**jamais supprimé**. »

**Deux manques mesurés.**

- Les serveurs multimédia **ne sont pas persistés du tout** : `state.media_servers`
  est un `Arc<Mutex<HashMap<String, MediaServerInfo>>>`
  (`tune-server/src/state.rs:86`), et `last_seen` est un `Instant`
  `#[serde(skip)]` (`ssdp.rs:146`). Rien ne survit à un redémarrage. Le
  précédent à copier est la clé de réglages `known_renderers`
  (`discovery_setup.rs:193-300`) — avec son piège documenté : `#[serde(default)]`
  obligatoire sur tout champ ajouté, « sinon `from_str` échoue et TOUTES les
  zones disparaissent au démarrage suivant ».
- Le registre est **rance en pratique**. Mesuré sur `.18` le 13/09 :

  ```
  GET /api/v1/network/media-servers →
    3 serveurs, tous "reachable": false, "last_seen_secs": 84 194  (23 h 23)
  ```

  Les deux Sonos, pourtant vivants et répondant au M-SEARCH depuis le Mac dans
  la même minute, **n'y figurent pas**. Le rafraîchissement du registre des
  serveurs média ne se fait manifestement pas au rythme annoncé. C'est un
  défaut à établir avant tout chantier d'indexation : indexer depuis un registre
  qui ne se met pas à jour n'a pas de sens.

**Le bon modèle de « source de bibliothèque » existe** : `network_mounts`
(`migrations.rs:178-197`), qui sépare l'**intention** (`active`) du **constat**
(`mount_state`, `last_mount_error`), distinction documentée par #1916. C'est le
point de greffe naturel pour une source UPnP indexée.

### 5. La taille et le temps

Mesuré le 13/09 par les routes existantes, via `.18` contre `.42`.

| Opération | Volume | Temps | Sortie |
|---|---:|---:|---:|
| `browse?object_id=0` (racine) | 7 conteneurs | **0,05 s** | 0,6 Ko |
| `browse?object_id=albums` | 1 488 albums | **0,26 s** | 311 Ko |
| `browse?object_id=tracks` | **22 331 pistes** | **11,61 s** | **8,3 Mo** |
| `search?q=Lachrimae` | 1 item | **0,08 s** | 0,4 Ko |

Complétude vérifiée sur le parcours complet : `total_matches = 22 331`,
`number_returned = 22 331`, 22 331 `"id":"track/`. Aucun `res_url` nul, 0
artiste nul, 3 albums nuls, 23 durées nulles, 1 fréquence nulle. **Le parcours
est intégral et propre** — la pagination de `browse_media_server`
(`network.rs:944-1025`, `PAGE_SIZE = 200`, `MAX_PAGES = 500`) fait son travail.

Contexte : `.18` porte **46 877 pistes / 4 255 albums / 1 913 Go** en local
(`GET /api/v1/library/stats`). Indexer le seul `.42` ajouterait **+ 48 %** de
pistes. `.41` (ce Mac) en porte 233.

**Extrapolation.** ≈ 1 900 pistes/s en parcours brut, 8,3 Mo pour 22 331 pistes,
soit ≈ 390 octets de JSON par piste. Pour un serveur à 100 000 pistes : **≈ 52 s
et 37 Mo** rien que pour le transport, plafond `MAX_PAGES` atteint à 100 000
enfants exactement. À quoi il faut ajouter l'écriture en base, non mesurée ici,
et qui dominera largement — le scan local d'une bibliothèque de cette taille se
compte en dizaines de minutes.

**Ce que ces chiffres imposent.** Un parcours complet est **trop rapide pour
justifier un cache incrémental compliqué, et trop lourd pour être fait à chaud
sur requête utilisateur**. 11,6 s bloquerait un écran ; 11,6 s en tâche de fond
toutes les heures est indolore. La conception naturelle est donc : parcours
complet périodique en tâche de fond, réconciliation par différence, jamais de
parcours synchrone.

**Ce que ces chiffres n'établissent pas.** Le `.42` est un Tune, et son
`Browse` sur 22 331 pistes est servi par un `SELECT` sur sa propre base. Un
MinimServer sur Raspberry Pi ou un NAS Synology lisant des tags peut être un à
deux ordres de grandeur plus lent. La mesure contre trois serveurs tiers est le
premier livrable de la phase 0.

### 6. Les doublons

**Le rapprochement existant est intégralement local.** Trois règles seulement
décident aujourd'hui que deux pistes sont la même :

1. **Hash d'octets** — `tracks.audio_hash` + confirmation octet-à-octet
   `files_are_byte_identical` (`tune-core/src/library/duplicate_detector.rs:34-66`).
   Seul critère marqué `suppression_sure: true`.
2. **Étiquettes normalisées** — `duplicates.rs:272-284` :
   `LOWER(t1.title) = LOWER(t2.title) AND t1.duration_ms = t2.duration_ms` +
   `LOWER(ar1.name) = LOWER(ar2.name)`. La normalisation se limite à `LOWER()` :
   pas de pliage d'accents, pas de retrait de ponctuation, pas de tolérance de
   durée sauf dans `/smart` (`± 3 000 ms`, `:571-580`).
3. **Empreinte de contenu** — BIB-B2, ci-dessous.

**BIB-B2 est réel et bien fait, et inutilisable tel quel.**
`tune-core/src/audio/empreinte.rs` (527 l.) : format `env100ms-v1:<hex>`,
décodage mono 11 025 Hz, 60 s en trames de 100 ms, 2 octets par trame (RMS
relatif dB + taux de passages par zéro), comparaison par alignement ± 3 trames
avec `SEUIL_MEME_CONTENU = 0.05` calibré sur 556 fichiers réels. Stockée dans
`tracks.audio_fingerprint`, calculée par la passe ReplayGain
(`audio/replaygain.rs:874`).

Le blocage : `empreinte_du_fichier(chemin)` (`:124`) **ouvre le fichier**, et le
rattrapage exige `t.file_path IS NOT NULL AND != ''`. **Mais la porte d'entrée
existe** : `empreinte_des_echantillons(&[i32], bit_depth)` (`:146`) ne prend que
des échantillons. Streamer 90 s depuis le serveur UPnP et les lui passer est un
chemin direct, sans toucher à l'algorithme. Coût : 90 s de réseau par piste
distante — inenvisageable sur 22 331 pistes en une passe, envisageable en
rattrapage borné, exactement comme le fait déjà `CANDIDATS_EMPREINTE_WHERE`.

Chromaprint/AcoustID (`metadata/fingerprint.rs`) est à écarter : shell-out vers
le binaire externe `fpcalc`, absent de Tune OS et du Raspberry Pi, et la
comparaison est une **égalité de chaîne** (`duplicate_detector.rs:203-238`), pas
une distance.

**L'absorption est mûre — et elle refuse explicitement le distant.**
`AlbumRepo::absorber` (`tune-core/src/db/album_repo.rs:611`) déplace, dans
l'ordre : 8 champs vides seulement si vides côté cible, `tracks.album_id`,
`listen_history.album_id`, `metadata_suggestions`, puis à clé unique
`album_ratings`, `album_metadata`, `favorites`, `hidden_items`, `item_tags`,
`metadata_reports`, `metadata_proposals`, réécriture des collections, purge de
`album_distinct_pairs`, `DELETE`, recalcul de `track_count` et `folder_path`.
`ArtistRepo::absorber` (`artist_repo.rs:568`) fait l'équivalent, sauf
`listen_history` qui ne connaît que le nom.

Ce qu'elles ne font pas : aucune fusion **piste à piste** (deux pistes n° 3
restent deux pistes sous la cible), aucun journal, aucun retour arrière.

Les primitives sont factorisées dans `tune-core/src/db/absorption.rs` :
`repointer` (`:22`), `repointer_a_cle_unique` (`:51`),
`reprendre_les_champs_vides` (`:77`), `recaler_le_texte` (`:105`),
`table_absente` (`:10`). **C'est le point d'extension naturel.**

Mais la route `POST /library/albums/{cible}/absorber/{doublon}`
(`routes/library/albums.rs:1647-1770`) oppose quatre refus, dont deux sont
rédhibitoires ici :

- `409 source_non_locale` : les deux albums doivent avoir
  `COALESCE(source,'local') = 'local'` ;
- `409 dossiers_differents` : intersection des dossiers de pistes, donc
  `file_path`, absent d'un album UPnP.

**Ce qui se réutilise tel quel** : `db/absorption.rs` (les 4 primitives),
`find_album_by_identity` (`favorites_reconcile.rs:115`, la règle canonique
titre+artiste, l'album le plus peuplé gagne, aucun repli titre-seul si l'artiste
est connu), `cle_titre_sans_tranche` (`albums.rs:1223`), `cle_artiste`
(`artist_repo.rs:364`), et surtout **`contenu_commun`** (`albums.rs:1912`) qui
apparie deux albums par `(disc_number, track_number)` et rend `same_content` si
≥ 80 % des pistes correspondent — c'est la brique la plus proche du besoin.

**Attention à une hypothèse fausse du brief** : `DUP-1`, `BIB-B3` et
`remplacee_probable` **n'existent pas comme identifiants dans le dépôt** (zéro
occurrence, tous fichiers confondus). « BIB-B3 » et « porte unique » sont des
formules de doc-comment (`routes/library/duplicates.rs:22-34`) désignant la
route `GET /library/duplicates` unifiée. « Remplacée probable » correspond au
rapport `zones_doublons` (`routes/system/diagnostics.rs:614-617`) et à la fusion
explicite `POST /zones/{doublon}/fusionner-dans/{cible}`
(`routes/zones/ecriture.rs:1168-1240`). Et **aucun document de `docs/` ne
mentionne BIB-B2, BIB-B3 ni DUP-1** : toute la spécification vit dans les
doc-comments Rust.

### 7. Ce qui existe déjà

**Échafaudages morts, tous nommés.**

| Ce qui est là | Où | État |
|---|---|---|
| `media_server_stream_url` | `network.rs:1578` | rend `"stream_url": null` |
| `play_media_server_item` | `network.rs:1587` | rend `"status": "not_implemented"` |
| `track_source_links` | `migrations.rs:383-399`, `db/source_link_repo.rs` | **zéro écrivain hors tests** |
| `GET /library/tracks/{id}/source-links` | `routes/library/tracks.rs:884-891` | rend toujours `{"links": []}` |
| `res@size` | DIDL du `.42` | présent, jeté par le parser |
| `SystemUpdateID` | `upnp_server.rs:541` | constante `1` |

**Sur `track_source_links` précisément**, puisque le brief la donne comme
candidate. Structure : `(track_id, service, service_track_id, confidence,
match_method, linked_at)`, `UNIQUE(track_id, service)`, cascade sur suppression
de piste, deux index. Miroirs PostgreSQL en place (`005_additional_tables.sql:156`,
`pg_migrate.rs:782`).

Verdict : **elle ne convient pas telle quelle, pour deux raisons de forme.**

1. `UNIQUE(track_id, service)` autorise **un seul lien par service**. Si
   `service = "upnp"`, une piste locale présente sur trois serveurs UPnP du
   réseau ne peut en référencer qu'un. Il faudrait soit
   `service = "upnp:<udn>"` (ce qui fait exploser le vocabulaire et casse le
   `match` codé en dur de `cloud/playlist_hub.rs:88-94`), soit relâcher la
   contrainte en `UNIQUE(track_id, service, service_track_id)` — une migration.
2. Elle **lie une piste locale à un id de service**. Elle ne décrit pas une
   piste qui n'existe QUE sur le serveur distant, c'est-à-dire le gros du
   volume : 22 331 pistes sur le `.42` dont la plupart n'ont pas de jumelle
   locale.

Elle reste **le bon outil pour le deuxième problème** — dire « cette piste
locale est aussi celle-là chez le voisin », avec `confidence` et `match_method`
déjà dimensionnés pour ça. Elle n'est pas l'outil du premier.

**PR ouvertes et issues.** Aucune PR ouverte ne porte sur l'unification ; les
trois PR UPnP ouvertes (#3974, #3976, #3977) concernent le **renderer**, pas le
serveur média. Côté issues :

| # | Sujet | Rapport au chantier |
|---|---|---|
| **#3933** | tout ce que la DIDL publie vit sous `/api/v1` et répond 401 | **bloquant** dès que `auth_enabled` |
| **#1800** | le dossier Radio publie l'URL du diffuseur, pas celle de Tune | même défaut de conception : publier une adresse tierce |
| **#3786** | Tune n'est plus dans sa propre liste Serveur Multimédia depuis 0.9.144 | le registre est déjà suspect |
| **#3688** | Tune redécouvre son propre MediaRenderer, jusqu'à 7 lignes | l'auto-exclusion par UDN est fragile |
| **#2219** | epic — découper le pipeline et unifier les backends | le chantier de fond en cours |

---

## Ce qui revient à Bertrand

Six décisions. Aucune n'est technique au point qu'un développeur puisse la
prendre seul.

**D1 — Que faire des doublons local ↔ distant ?**
Trois postures possibles :
- *(a) rien* : la piste distante s'ajoute, l'album apparaît deux fois. Coût
  zéro, écran sale ;
- *(b) marquer* : un badge « aussi sur Salon » sur l'album local, sans fusion,
  par `find_album_by_identity` (titre+artiste). C'est ce que le dépôt sait
  déjà faire, sans nouveau code de fusion ;
- *(c) fusionner* : étendre `absorber` au distant, ce qui veut dire lever
  `409 source_non_locale` et `409 dossiers_differents`, donc réécrire les deux
  gardes de la route.

Recommandation : **(b)**, en phase 3, et (c) jamais automatiquement.

**D1bis — Le badge d'une piste distante dit « UPNP ». ✅ TRANCHÉ**

**Arbitrage de Bertrand, 14/09/2026** : une piste venue d'un serveur UPnP porte le badge
**`UPNP`**, comme `QOBUZ` ou `BANDCAMP` portent le leur. Pas « local », et pas un badge
par serveur.

C'est une entrée de plus dans la table de `ServiceBadge.svelte`, rien d'autre :

```ts
upnp: { name: 'UPNP', bg: '…', color: '#ffffff' },
```

État mesuré du code au moment de la décision (v0.9.149) :

* `ServiceBadge.svelte` est une table fixe de huit entrées. Une source absente de la table
  rend `null`, donc **aucun badge** — une piste `source = 'upnp'` n'afficherait rien
  aujourd'hui, ce qui est un défaut discret ;
* le « LOCAL » visible vient des appelants qui forcent le défaut :
  `source={(t as any).source ?? 'local'}` dans `FavoritesView.svelte:886` et `:930`.
  **Ces replis doivent cesser** — sinon une piste distante réapparaîtra en « LOCAL »,
  c'est-à-dire en mensonge.

Le **nom du serveur d'origine** reste utile pour distinguer deux pistes identiques venues de
deux machines, mais il n'a pas sa place dans le badge : il ira dans l'infobulle ou dans le
détail de la piste, à trancher au moment de l'écran.

**D2 — Que faire quand un serveur disparaît ?** ✅ Tranché le 14/09/2026 : **24 h**,
comme les zones (`SERVEUR_ABSENT_APRES`). La première implémentation avait posé
5 400 s (trois `max-age`) ; ramenée à 24 h le 16/09 sur rappel de Bertrand.
Le dépôt a déjà tranché quatre fois dans le même sens (marquer, jamais
retirer ; ne pas croire un byebye ; ne supprimer qu'après une observation
complète et saine ; plafonner toute purge à 20 %). La question posée à
Bertrand est seulement : **combien de temps avant qu'un serveur absent cesse
d'être proposé à la lecture** — tout en restant visible et cherchable ? Le
précédent des zones dit 24 h.

**D3 — Jusqu'où indexer ?**
Trois plafonds à fixer :
- **par serveur** : tout, ou seulement les conteneurs choisis par
  l'utilisateur (« Albums » mais pas « All Tracks ») ;
- **en volume** : un plafond dur au-delà duquel on refuse et on le dit. 22 331
  pistes passent en 11,6 s ; 500 000 non ;
- **en profondeur** : les pistes seules, ou aussi les playlists, radios et
  genres du serveur distant. Le `.42` expose 51 radios et 21 playlists.

**D4 — Une piste distante est-elle jouable, et par quelles sorties ? ✅ TRANCHÉ**

**Arbitrage de Bertrand, 14/09/2026 : jouable PARTOUT, défauts assumés et DITS.**

Chaque zone joue ce qu'elle peut. Là où c'est dégradé, **l'écran le dit** au lieu de faire
semblant. Là où c'est impossible, la lecture **refuse avec un motif** au lieu de rendre du
silence.

Ce que cela impose, sortie par sortie, à partir de l'état mesuré :

| sortie | état | ce que la décision exige |
|---|---|---|
| réseau | joue, **sans DSP** | dire que le DSP ne s'applique pas |
| navigateur | joue, **sans DSP** | idem |
| locale | joue, **avec DSP**, sans ReplayGain, **seek cassé** | dire les deux manques ; ne pas prétendre que le seek marche |
| OAAT | **silence** | **refuser explicitement**, avec un motif — un silence sans message est le pire des deux |

Le principe qui tranche les cas non listés : **ne jamais faire semblant**. Une piste qui ne
peut pas jouer sur une zone doit le dire avant d'être lancée, pas après.

L'ancien état, pour mémoire :

**D4 — Une piste distante est-elle jouable, et par quelles sorties ?**
Aujourd'hui : sortie réseau oui (sans DSP), navigateur oui (sans DSP), locale
oui (avec DSP, sans ReplayGain, seek cassé), OAAT non (silence). Livrer
l'indexation sans trancher ceci produirait une bibliothèque où la moitié des
pistes ne joue pas selon la zone choisie — le pire résultat possible.

**D5 — Corrige-t-on le `SystemUpdateID` de Tune ?**
Sans cela, deux Tune ne peuvent pas détecter mutuellement une réindexation
autrement qu'en re-parcourant tout. C'est une correction de trois lignes dans
`upnp_server.rs`, mais elle n'aide que les versions à venir : les serveurs
0.9.146/0.9.147 déjà installés resteront à `1`.

**D6 — Change-t-on la forme des ObjectID de Tune ?**
`track/<rowid>` est instable par construction. Le remplacer par une clé dérivée
du contenu (condensat de `album_folder` + `disc` + `track`, par exemple) rendrait
la bibliothèque Tune indexable de façon stable — et **casserait tous les points
de contrôle tiers qui ont mémorisé les anciens ObjectID**. C'est un changement
de contrat public.

---

## Découpe proposée

Chaque phase livre quelque chose d'observable, avec son témoin (le vert qui
prouve que ça marche) et sa contre-épreuve (le rouge qui prouve que le témoin
garde vraiment quelque chose).

### Phase 0 — Nommer et mesurer, sans rien changer

Ce document, plus ce qu'il ne pouvait pas mesurer :

- parcourir **trois serveurs tiers réels** (MinimServer, Asset ou Twonky,
  Synology DS Audio, LMS) et relever pour chacun : forme de l'ObjectID, présence
  de `res@size`, `SystemUpdateID` réel, `SearchCapabilities`, temps de parcours
  complet, comportement à la réindexation ;
- établir **pourquoi le registre `media_servers` du `.18` est à 84 194 s** et
  n'a pas les deux Sonos. Sans ce point, aucune phase ultérieure ne repose sur
  rien ;
- compter précisément les **41 filtres `source = 'local'`** un par un, et dire
  pour chacun s'il doit rester exclusif ou s'ouvrir.

**Témoin** : un tableau de mesures par serveur tiers, dans `docs/mesures/`.
**Contre-épreuve** : une réindexation forcée d'un serveur tiers, et la preuve
que ses ObjectID ont ou n'ont pas bougé. Si tous s'avèrent stables, la phase 2
change de forme.

### Phase 1 — Le registre des serveurs devient durable

Persister `media_servers` comme `network_mounts` : identité (UDN), adresse,
nom, `active` (intention) et `last_seen_at` / `last_state` (constat). Rejouer au
démarrage, comme `known_renderers`, avec `#[serde(default)]` sur tout champ
ajouté.

Rien n'est encore indexé. La liste des serveurs survit simplement au
redémarrage, et un serveur absent depuis trois semaines le dit.

**Témoin** : redémarrer le serveur, `GET /network/media-servers` rend la même
liste avant tout M-SEARCH, avec des `last_seen_at` absolus (plus des
`Instant` jetés).
**Contre-épreuve** : éteindre un serveur, redémarrer Tune — il doit apparaître
`absent_depuis`, **pas** disparaître, et **pas** apparaître comme vu à
l'instant.

### Phase 2 — Une source UPnP indexée, en lecture seule de la base

Une seule source, choisie à la main. Écriture dans `tracks` / `albums` /
`artists` avec `source = 'upnp'` et `source_id = '<udn>|<objectid>'`, plus un
**instantané d'affichage** (titre, artiste, album, URL de pochette, URL de
lecture) selon la doctrine de `streaming_item_tags:1693-1720`. Aucune
suppression : première passe purement additive.

Prérequis mécaniques : récupérer `res@size` (3 lignes), rendre `channels`
optionnel ou ne pas dériver de `channel_badge` sans preuve, et convertir les
**43 requêtes** `file_path IS NOT NULL` vers `A_UN_FICHIER`.

**Témoin** : après indexation du `.42`, `GET /library/stats` sur le `.18` rend
`tracks_by_source: {local: 46877, upnp: 22331}`, et l'écran Albums trie et
filtre les 1 488 albums distants comme les autres.
**Contre-épreuve** : couper le `.42` et recharger l'écran — les 1 488 albums
doivent **rester affichés** avec leurs pochettes en cache et un marquage
d'indisponibilité, sans une seule requête au serveur éteint. Si l'écran se vide
ou rame, l'instantané n'en est pas un.

### Phase 3 — La lecture

Brancher `resolve_direct_url` sur les items de serveur média : remplacer les
deux coquilles `network.rs:1578` / `:1587` par une résolution réelle, et faire
passer les sorties réseau et OAAT par `proxy_stream`
(`tune-stream-http/src/lib.rs:1645`) quand un DSP est actif — le portage de
#2863 qui n'a jamais eu lieu sur ce chemin. Propager `seek_ms` dans
`resolve_direct.rs`, qui n'en contient aujourd'hui aucune occurrence.

**Témoin** : une piste distante joue sur les quatre familles de sortie, EQ actif
audible, et un seek à 2:00 reprend à 2:00.
**Contre-épreuve** : la même piste avec DSP actif sur sortie OAAT — aujourd'hui
elle produit un silence ; le témoin ne vaut que si ce silence a été constaté
avant.

### Phase 4 — La réconciliation

Parcours complet périodique en tâche de fond (11,6 s pour 22 331 pistes, donc
horaire sans douleur), différence avec l'indexé, et application des quatre
gardes de purge existantes : jamais sur un serveur injoignable, jamais si le
parcours a échoué en cours de route, plafond à 20 % avec confirmation chiffrée,
suppression seulement après un parcours **complet et sain**.

**Témoin** : ajouter 10 pistes sur le `.42`, attendre un cycle, les voir
apparaître ; en retirer 10, les voir disparaître.
**Contre-épreuve** : couper le `.42` en plein parcours et vérifier que **zéro**
piste est supprimée, avec le refus journalisé. Puis rescanner complètement le
`.42` (ce qui réattribue ses `tracks.id`) et vérifier ce qui arrive : c'est le
scénario qui dira si la clé retenue tient.

### Phase 5 — Les doublons

Seulement après la phase 4, et seulement le marquage (D1-b) : un album local et
un album distant rapprochés par `find_album_by_identity` +
`cle_titre_sans_tranche` portent une mention réciproque. Pas de fusion.

**Témoin** : un album présent sur `.18` et `.42` affiche « aussi sur … ».
**Contre-épreuve** : deux albums homonymes d'artistes différents ne doivent
**pas** être rapprochés — et deux éditions du même album (remaster, coffret)
non plus.

---

## Ce que je déconseille

**Ne pas fusionner automatiquement local et distant.** `absorber` est
irréversible (aucun journal, aucun retour arrière) et ses deux gardes
protectrices reposent sur `file_path`, absent en distant. Une fusion
automatique sur une clé titre+artiste détruirait des albums locaux sur la foi
d'une graphie. Le marquage donne 90 % du bénéfice pour 0 % du risque.

**Ne pas réutiliser `track_source_links` pour porter les pistes distantes.**
`UNIQUE(track_id, service)` interdit deux serveurs UPnP pour une même piste, et
la table ne sait pas décrire une piste sans jumelle locale — ce qui est le cas
de la quasi-totalité du volume. Elle reste bonne pour dire « cette piste locale
est aussi celle-là chez le voisin », en phase 5.

**Ne pas indexer avant d'avoir corrigé le registre.** Le `.18` affiche
aujourd'hui trois serveurs vus il y a 23 heures et ignore deux Sonos vivants.
Bâtir une indexation périodique sur ce registre, c'est bâtir sur un capteur qui
ne mesure pas. La phase 1 n'est pas un préliminaire poli, c'est une condition.

**Ne pas livrer l'indexation avant la lecture.** Une bibliothèque où l'on peut
trier, filtrer et chercher 22 331 pistes qui ne jouent pas — ou qui jouent sans
EQ sur trois sorties et pas du tout sur la quatrième — sera perçue comme une
régression, pas comme une fonctionnalité. Si l'ordre doit être inversé, autant
livrer la phase 3 avant la phase 2 : une navigation qui joue vaut mieux qu'un
index qui ne joue pas.

**Ne pas compter sur `SystemUpdateID` contre un autre Tune.** Il vaut `1` en
dur et vaudra `1` sur toutes les versions déjà déployées, quoi qu'on corrige
aujourd'hui. La réconciliation doit être conçue **sans** ce signal, et
l'utiliser seulement comme raccourci opportuniste quand un serveur tiers le
fournit.

**Ne pas espérer une clé stable côté Tune sans casser un contrat public.**
`track/<rowid>` ne survit pas à un rescan complet, et le changer casse les
points de contrôle tiers qui ont mémorisé les anciens ObjectID. La
réconciliation devra donc, pour les serveurs Tune, se rabattre sur une clé
**dérivée du contenu** reconstruite côté client — titre + artiste + album +
durée arrondie, complété par `res@size` une fois celui-ci récupéré. C'est moins
sûr qu'un identifiant, et c'est le prix à payer. Il faut le dire avant de
commencer, pas après.
